//! Gestion interne des sessions **live** de la façade FFI (voir [`crate::api`]).
//!
//! `flutter_rust_bridge` ne portant pas bien un objet mutable partagé, la façade
//! travaille **par identifiant opaque** : chaque session démarrée vit dans une table
//! statique (`OnceLock<Mutex<HashMap<u64, EntreeSession>>>`) et l'UI ne manipule que
//! son `u64`. Ce module est **privé** : il n'est pas scanné par le codegen
//! (`rust_input: crate::api` dans `ui/flutter_rust_bridge.yaml`) et ne fait pas
//! partie du contrat.
//!
//! # Threads de drainage
//!
//! Les `Receiver` du [`SessionHandle`] (états, frames) sont extraits à la création
//! (échangés contre des canaux factices : la poignée implémente `Drop`, on ne peut
//! pas la déstructurer) puis consommés **par un seul mécanisme chacun** :
//!
//! * un thread de drainage vers un `StreamSink` Dart (`flux_etats` / `flux_video`),
//!   qui vit jusqu'à la fin de la session (canal déconnecté) ou l'annulation du
//!   `Stream` côté Dart (`add` en échec) ;
//! * ou les lectures synchrones de repli (`attendre_etat` / `collecter_frames`),
//!   qui empruntent le récepteur le temps d'une lecture puis le restituent.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nd_audio::AudioSession;
use nd_codec::DecodedFrame;
use nd_core::{
    ChatMessage, SessionEndpoint, SessionEngine, SessionHandle, SessionMedia, SessionOptions,
    SessionState, UnattendedHost, UnattendedHostHandle,
};
use nd_features::{PermissionSet, Permissions};
use nd_files::{ClipboardSync, TransferEvent};
use nd_proto::{InputEvent, NovaId};
use nd_transport::ServerIdentity;

use crate::api::{
    ChatMessageDto, IncomingRequestDto, ListenInfoDto, PermissionsDto, SessionConfigDto,
    SessionEndpointDto, SessionOptionsDto, SessionStateDto, SessionStatsDto, TransferEventDto,
    VideoFrameDto,
};
use crate::frb_generated::{SseEncode, StreamSink};

/// Plafond des délais d'attente acceptés : les délais démesurés sont écrêtés
/// (protège aussi l'arithmétique d'échéance de tout débordement).
const DELAI_MAX: Duration = Duration::from_secs(3_600);

// ---------------------------------------------------------------------------
// Table des sessions
// ---------------------------------------------------------------------------

/// Une session vivante : la poignée du moteur et ses sorties encore disponibles.
struct EntreeSession {
    /// Poignée du moteur (statistiques, canal d'entrées, arrêt). Ses récepteurs
    /// ont été remplacés par des canaux factices dès l'insertion dans la table.
    poignee: SessionHandle,
    /// Transitions d'état, tant qu'aucun consommateur ne les a prises.
    etats: Option<Receiver<SessionState>>,
    /// Frames décodées (rôle contrôleur), tant qu'aucun consommateur ne les a prises.
    frames: Option<Receiver<DecodedFrame>>,
    /// Messages de chat reçus + échos (mode étendu), tant qu'aucun consommateur
    /// ne les a pris.
    chat: Option<Receiver<ChatMessage>>,
    /// Progression des transferts de fichiers (mode étendu), idem.
    transfert: Option<Receiver<TransferEvent>>,
    /// Adresse/certificat d'écoute (sessions hôtes `Loopback` uniquement).
    ecoute: Option<ListenInfoDto>,
}

/// Table des sessions vivantes, indexée par identifiant opaque.
type TableSessions = Mutex<HashMap<u64, EntreeSession>>;

/// Prochain identifiant de session (compteur monotone : 1, 2, 3…).
static PROCHAIN_ID: AtomicU64 = AtomicU64::new(1);

/// Table statique unique du processus.
static SESSIONS: OnceLock<TableSessions> = OnceLock::new();

/// Verrouille la table. Un empoisonnement (panique d'un autre thread sous verrou)
/// est absorbé : les opérations sous verrou sont triviales et laissent la table
/// cohérente ; mieux vaut une façade qui répond qu'une panique en cascade dans l'UI.
fn verrou() -> MutexGuard<'static, HashMap<u64, EntreeSession>> {
    SESSIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Exécute `action` sur l'entrée `id`, avec une erreur lisible si elle est inconnue.
fn avec_entree<R>(id: u64, action: impl FnOnce(&mut EntreeSession) -> R) -> Result<R, String> {
    let mut table = verrou();
    let entree = table
        .get_mut(&id)
        .ok_or_else(|| format!("session {id} inconnue (jamais démarrée ou déjà arrêtée)"))?;
    Ok(action(entree))
}

/// Écrête un délai utilisateur (millisecondes) en `Duration` bornée par [`DELAI_MAX`].
fn duree_bornee(timeout_ms: u64) -> Duration {
    DELAI_MAX.min(Duration::from_millis(timeout_ms))
}

// ---------------------------------------------------------------------------
// Cycle de vie
// ---------------------------------------------------------------------------

/// Démarre le moteur de session avec les options par défaut. Renvoie
/// l'identifiant opaque attribué.
pub(crate) fn demarrer_session(
    config: SessionConfigDto,
    endpoint: SessionEndpointDto,
) -> Result<u64, String> {
    demarrer_session_interne(config, endpoint, SessionOptions::default())
}

/// Démarre le moteur de session avec des options avancées (miroir plat traduit
/// en [`SessionOptions`]). Renvoie l'identifiant opaque attribué.
pub(crate) fn demarrer_session_avec_options(
    config: SessionConfigDto,
    endpoint: SessionEndpointDto,
    options: SessionOptionsDto,
) -> Result<u64, String> {
    demarrer_session_interne(config, endpoint, options.into())
}

/// Cœur commun : prépare l'endpoint, démarre le moteur avec `options` (en
/// injectant les briques média du mode étendu) et enregistre la poignée dans la
/// table.
fn demarrer_session_interne(
    config: SessionConfigDto,
    endpoint: SessionEndpointDto,
    options: SessionOptions,
) -> Result<u64, String> {
    let (endpoint_moteur, ecoute) = preparer_endpoint(endpoint)?;
    let media = construire_media(&options);
    // `start_with_media` avec un `SessionMedia::default` équivaut à
    // `start_with_options` (mode classique) ; en mode étendu, `media` porte
    // l'audio et le presse-papiers réels (voir `construire_media`).
    let mut poignee =
        SessionEngine::start_with_media(config.into(), endpoint_moteur, options, media)
            .map_err(|e| format!("démarrage de la session impossible : {e}"))?;

    // Extrait les récepteurs de la poignée en les échangeant contre des canaux
    // factices (dont l'émetteur est aussitôt lâché) : `SessionHandle` implémente
    // `Drop`, la déstructuration est donc interdite, mais l'échange de champs
    // publics reste sûr — la poignée n'utilise pas ses récepteurs en interne.
    let etats = Some(std::mem::replace(&mut poignee.state_rx, mpsc::channel().1));
    let frames = Some(std::mem::replace(&mut poignee.frame_rx, mpsc::channel().1));
    let chat = Some(std::mem::replace(&mut poignee.chat_rx, mpsc::channel().1));
    let transfert = Some(std::mem::replace(
        &mut poignee.transfer_rx,
        mpsc::channel().1,
    ));

    let id = PROCHAIN_ID.fetch_add(1, Ordering::Relaxed);
    verrou().insert(
        id,
        EntreeSession {
            poignee,
            etats,
            frames,
            chat,
            transfert,
            ecoute,
        },
    );
    Ok(id)
}

/// Construit les briques média **injectées** dans la session.
///
/// Hors mode étendu : [`SessionMedia::default`] (aucune brique — session vidéo +
/// entrées, comportement historique). En mode étendu : ouvre l'audio duplex
/// système ([`AudioSession::duplex_systeme`]) et le presse-papiers de la
/// plateforme ([`ClipboardSync::new`]) ; chaque brique indisponible reste
/// `None` — la session démarre sans planter, seule la fonction correspondante
/// reste inerte. Note : le moteur reconstruit l'audio à la volée si la capacité
/// est accordée, mais **pas** le presse-papiers — l'injecter ici est donc ce qui
/// active réellement la synchro presse-papiers.
fn construire_media(options: &SessionOptions) -> SessionMedia {
    if !options.extended_features {
        return SessionMedia::default();
    }
    SessionMedia {
        audio: AudioSession::duplex_systeme().ok(),
        clipboard: ClipboardSync::new().ok(),
    }
}

/// Traduit le DTO d'endpoint en [`SessionEndpoint`] du moteur. Pour `Loopback`,
/// lie l'écouteur QUIC ici même afin de connaître immédiatement l'adresse et le
/// certificat à publier ([`ListenInfoDto`]).
fn preparer_endpoint(
    dto: SessionEndpointDto,
) -> Result<(SessionEndpoint, Option<ListenInfoDto>), String> {
    match dto {
        SessionEndpointDto::Loopback => {
            let ecouteur =
                nd_transport::bind("127.0.0.1:0".parse().expect("adresse loopback valide"))
                    .map_err(|e| format!("ouverture de l'écouteur loopback impossible : {e}"))?;
            let info = ListenInfoDto {
                addr: ecouteur.local_addr().to_string(),
                cert_der: ecouteur.server_cert_der(),
            };
            Ok((SessionEndpoint::Loopback { listener: ecouteur }, Some(info)))
        }
        SessionEndpointDto::Direct { addr, cert_der } => {
            let adresse = parser_adresse("du pair", &addr)?;
            Ok((
                SessionEndpoint::Direct {
                    addr: adresse,
                    cert_der,
                },
                None,
            ))
        }
        SessionEndpointDto::ByRendezvous {
            server,
            stun_servers,
            relay,
        } => {
            let serveur = parser_adresse("du serveur de rendez-vous", &server)?;
            let stun = parser_adresses_stun(&stun_servers)?;
            let relais = match relay {
                Some(r) => Some(parser_adresse("du relais", &r)?),
                None => None,
            };
            Ok((
                SessionEndpoint::ByRendezvous {
                    server: serveur,
                    stun_servers: stun,
                    relay: relais,
                },
                None,
            ))
        }
    }
}

/// Analyse une adresse « ip:port » avec un message d'erreur français explicite.
/// `quoi` qualifie l'adresse dans le message (ex. « du serveur de rendez-vous »).
fn parser_adresse(quoi: &str, texte: &str) -> Result<SocketAddr, String> {
    texte
        .trim()
        .parse()
        .map_err(|e| format!("adresse {quoi} « {texte} » invalide (attendu « ip:port ») : {e}"))
}

/// Analyse une liste de serveurs STUN (« ip:port ») ; le message d'erreur situe
/// l'entrée fautive par son rang.
fn parser_adresses_stun(entrees: &[String]) -> Result<Vec<SocketAddr>, String> {
    entrees
        .iter()
        .enumerate()
        .map(|(i, s)| {
            s.trim().parse::<SocketAddr>().map_err(|e| {
                format!(
                    "serveur STUN n°{} « {s} » invalide (attendu « ip:port ») : {e}",
                    i + 1
                )
            })
        })
        .collect()
}

/// Coordonnées d'écoute d'une session hôte `Loopback` (erreur sinon).
pub(crate) fn info_ecoute(id: u64) -> Result<ListenInfoDto, String> {
    avec_entree(id, |entree| entree.ecoute.clone())?
        .ok_or_else(|| format!("la session {id} n'écoute pas (endpoint non loopback)"))
}

/// Arrête la session et la retire de la table. `stop()` est appelé **hors verrou** :
/// il attend la fin des threads du moteur (au plus ~5 s).
pub(crate) fn arreter_session(id: u64) -> Result<(), String> {
    let entree = verrou()
        .remove(&id)
        .ok_or_else(|| format!("session {id} inconnue (jamais démarrée ou déjà arrêtée)"))?;
    entree.poignee.stop();
    Ok(())
}

// ---------------------------------------------------------------------------
// Statistiques et entrées
// ---------------------------------------------------------------------------

/// Instantané des statistiques du moteur, converti en DTO plat. Le backend
/// d'encodage, exposé hors de [`nd_core::SessionStats`], est renseigné ici depuis
/// la poignée.
pub(crate) fn statistiques(id: u64) -> Result<SessionStatsDto, String> {
    avec_entree(id, |entree| {
        let mut dto = SessionStatsDto::from(entree.poignee.stats());
        dto.encoder_backend = entree.poignee.encoder_backend();
        dto
    })
}

/// Dernière erreur d'exécution du moteur (voir [`SessionHandle::last_error`]).
pub(crate) fn derniere_erreur(id: u64) -> Result<Option<String>, String> {
    avec_entree(id, |entree| entree.poignee.last_error())
}

/// Pousse un événement d'entrée dans le canal du moteur (rôle contrôleur).
pub(crate) fn envoyer_entree(id: u64, evenement: InputEvent) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.input_tx.send(evenement))?
        .map_err(|_| format!("la session {id} n'accepte plus d'entrées (moteur arrêté)"))
}

// ---------------------------------------------------------------------------
// Commandes des fonctions média étendues (chat, fichiers, audio, moniteur)
// ---------------------------------------------------------------------------
//
// Chaque commande délègue à la méthode correspondante de [`SessionHandle`], qui
// poste sur un canal interne non bloquant : inerte hors mode étendu ou si la
// permission n'est pas accordée, mais toujours `Ok` tant que la session existe.

/// Envoie un message de chat au pair (canal `Control` chiffré).
pub(crate) fn envoyer_chat(id: u64, texte: String) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.send_chat(texte))
}

/// Démarre l'envoi d'une file de fichiers vers le pair (canal `Files`).
pub(crate) fn envoyer_fichiers(id: u64, chemins: Vec<String>) -> Result<(), String> {
    let fichiers: Vec<PathBuf> = chemins.into_iter().map(PathBuf::from).collect();
    avec_entree(id, |entree| entree.poignee.send_files(fichiers))
}

/// Active/désactive l'audio de la session.
pub(crate) fn definir_audio_actif(id: u64, actif: bool) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.set_audio_enabled(actif))
}

/// Demande la bascule vers le moniteur d'index donné (hôte).
pub(crate) fn basculer_moniteur(id: u64, moniteur: u32) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.switch_monitor(moniteur))
}

// ---------------------------------------------------------------------------
// Flux vers Dart (StreamSink) et lectures synchrones de repli
// ---------------------------------------------------------------------------

/// Branche le flux d'états : prend définitivement le récepteur d'états et lance
/// son thread de drainage vers `sink`.
pub(crate) fn flux_etats(id: u64, sink: StreamSink<SessionStateDto>) -> Result<(), String> {
    let etats = avec_entree(id, |entree| entree.etats.take())?.ok_or_else(|| {
        format!("les états de la session {id} sont déjà consommés (flux ou lecture en cours)")
    })?;
    demarrer_drain(
        format!("nd-ffi-etats-{id}"),
        etats,
        sink,
        SessionStateDto::from,
    )
}

/// Branche le flux vidéo (fonction clé du rendu UI) : prend définitivement le
/// récepteur de frames et lance son thread de drainage vers `sink`.
pub(crate) fn flux_video(id: u64, sink: StreamSink<VideoFrameDto>) -> Result<(), String> {
    let frames = avec_entree(id, |entree| entree.frames.take())?.ok_or_else(|| {
        format!("le flux vidéo de la session {id} est déjà consommé (flux ou lecture en cours)")
    })?;
    demarrer_drain(
        format!("nd-ffi-video-{id}"),
        frames,
        sink,
        VideoFrameDto::from,
    )
}

/// Branche le flux de chat (mode étendu) : prend définitivement le récepteur de
/// chat et lance son thread de drainage vers `sink`.
pub(crate) fn flux_chat(id: u64, sink: StreamSink<ChatMessageDto>) -> Result<(), String> {
    let chat = avec_entree(id, |entree| entree.chat.take())?
        .ok_or_else(|| format!("le chat de la session {id} est déjà consommé (flux en cours)"))?;
    demarrer_drain(
        format!("nd-ffi-chat-{id}"),
        chat,
        sink,
        ChatMessageDto::from,
    )
}

/// Branche le flux de progression des transferts de fichiers (mode étendu) :
/// prend définitivement le récepteur de transfert et lance son drainage.
pub(crate) fn flux_transfert(id: u64, sink: StreamSink<TransferEventDto>) -> Result<(), String> {
    let transfert = avec_entree(id, |entree| entree.transfert.take())?.ok_or_else(|| {
        format!("le flux de transfert de la session {id} est déjà consommé (flux en cours)")
    })?;
    demarrer_drain(
        format!("nd-ffi-transfert-{id}"),
        transfert,
        sink,
        TransferEventDto::from,
    )
}

/// Lance le thread dédié qui draine `rx` vers `sink` (conversion à la volée).
///
/// Le thread vit jusqu'à la fin de la session (canal déconnecté quand le moteur
/// lâche ses émetteurs) ou l'annulation du flux côté Dart (`add` en échec). Lâcher
/// le sink en sortie clôt le `Stream` Dart correspondant. Le thread est détaché :
/// son cycle de vie est exactement celui du canal ou du sink.
fn demarrer_drain<I, D, F>(
    nom: String,
    rx: Receiver<I>,
    sink: StreamSink<D>,
    convertir: F,
) -> Result<(), String>
where
    I: Send + 'static,
    D: SseEncode + Send + 'static,
    F: Fn(I) -> D + Send + 'static,
{
    thread::Builder::new()
        .name(nom.clone())
        .spawn(move || {
            while let Ok(valeur) = rx.recv() {
                if sink.add(convertir(valeur)).is_err() {
                    // Flux annulé côté Dart : on cesse de drainer, le moteur
                    // continue (les frames excédentaires sont sautées chez lui).
                    break;
                }
            }
        })
        .map(|_poignee| ())
        .map_err(|e| format!("création du thread « {nom} » impossible : {e}"))
}

/// Attend la prochaine transition d'état (au plus `timeout_ms`, écrêté).
///
/// Emprunte le récepteur d'états le temps de la lecture puis le restitue, pour que
/// les lectures suivantes (ou un flux branché plus tard) voient la suite. `Ok(None)`
/// si rien n'arrive dans le délai ou si la session est terminée.
pub(crate) fn attendre_etat(id: u64, timeout_ms: u64) -> Result<Option<SessionStateDto>, String> {
    let etats = avec_entree(id, |entree| entree.etats.take())?.ok_or_else(|| {
        format!("les états de la session {id} sont déjà consommés (flux ou lecture en cours)")
    })?;
    let recu = etats.recv_timeout(duree_bornee(timeout_ms));
    restituer(id, |entree| entree.etats = Some(etats));
    match recu {
        Ok(etat) => Ok(Some(etat.into())),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Ok(None),
    }
}

/// Collecte jusqu'à `max_frames` frames décodées (au plus `timeout_ms`, écrêté).
///
/// Emprunte le récepteur de frames le temps de la collecte puis le restitue.
/// Renvoie ce qui a été reçu, possiblement moins que `max_frames` si le délai
/// expire ou si la session se termine.
pub(crate) fn collecter_frames(
    id: u64,
    max_frames: u32,
    timeout_ms: u64,
) -> Result<Vec<VideoFrameDto>, String> {
    let frames = avec_entree(id, |entree| entree.frames.take())?.ok_or_else(|| {
        format!("le flux vidéo de la session {id} est déjà consommé (flux ou lecture en cours)")
    })?;
    let echeance = Instant::now() + duree_bornee(timeout_ms);
    let mut recoltees: Vec<VideoFrameDto> = Vec::new();
    while (recoltees.len() as u64) < u64::from(max_frames) {
        let restant = echeance.saturating_duration_since(Instant::now());
        if restant.is_zero() {
            break;
        }
        match frames.recv_timeout(restant) {
            Ok(frame) => recoltees.push(frame.into()),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    restituer(id, |entree| entree.frames = Some(frames));
    Ok(recoltees)
}

/// Restitue une ressource empruntée à l'entrée `id`, si la session vit toujours
/// (sinon la ressource est simplement lâchée : la session a été arrêtée entre-temps).
fn restituer(id: u64, rendre: impl FnOnce(&mut EntreeSession)) {
    if let Some(entree) = verrou().get_mut(&id) {
        rendre(entree);
    }
}

// ---------------------------------------------------------------------------
// Hôtes « accès non surveillé » : table dédiée + file d'approbation
// ---------------------------------------------------------------------------
//
// # Design de l'approbation entrante (bloquant + garde-fous)
//
// Le hook `accept` du moteur ([`nd_core::UnattendedHost`]) est **synchrone** : il
// est consulté sur le thread de service, une connexion à la fois, avant tout octet
// applicatif. On implémente donc une **file d'approbation bloquante** pilotée par
// le Dart :
//
// * `accept(pair)` enregistre l'attente, pousse une [`IncomingRequestDto`] vers le
//   Dart (best-effort via le sink), puis **bloque** sur une [`Condvar`] ;
// * [`approuver_entrant`] (appel du Dart) dépose la décision et réveille l'attente ;
// * garde-fous **anti-deadlock** : l'attente est bornée par [`DELAI_APPROBATION`]
//   (au-delà = **refus par défaut**) et réveillée immédiatement par l'arrêt de
//   l'hôte (`demander_arret`). Sans abonné au flux, la demande n'est pas livrée et
//   expire donc en refus — jamais de blocage indéfini.

/// Délai maximal d'attente d'une décision d'approbation entrante. Au-delà,
/// l'appelant est refusé par défaut : borne l'`accept` bloquant du moteur.
const DELAI_APPROBATION: Duration = Duration::from_secs(30);

/// État partagé de la file d'approbation, protégé par un unique [`Mutex`].
struct EtatApprobation {
    /// Décision par ID de pair : `None` = en attente, `Some(bool)` = tranchée.
    decisions: HashMap<u64, Option<bool>>,
    /// Arrêt demandé : réveille et refuse toute attente en cours.
    arret: bool,
}

/// File d'approbation d'un hôte : le hook `accept` du moteur y bloque, le Dart y
/// répond. Partagée (`Arc`) entre le thread de service et la façade.
struct ApprobationHote {
    etat: Mutex<EtatApprobation>,
    /// Réveille l'`accept` en attente dès qu'une décision ou l'arrêt arrive.
    signal: Condvar,
    /// Sink des demandes entrantes vers le Dart (`None` tant que non abonné).
    sink: Mutex<Option<StreamSink<IncomingRequestDto>>>,
}

impl ApprobationHote {
    fn new() -> Self {
        ApprobationHote {
            etat: Mutex::new(EtatApprobation {
                decisions: HashMap::new(),
                arret: false,
            }),
            signal: Condvar::new(),
            sink: Mutex::new(None),
        }
    }

    /// Hook `accept` du moteur : notifie le Dart puis bloque jusqu'à la décision,
    /// l'arrêt, ou l'expiration ([`DELAI_APPROBATION`], défaut = refus).
    fn attendre_approbation(&self, pair: NovaId) -> bool {
        self.attendre_approbation_avec_delai(pair, DELAI_APPROBATION)
    }

    /// Cœur de l'attente d'approbation, le délai d'expiration étant paramétré
    /// (les tests l'abrègent pour exercer le refus par défaut sans attendre).
    fn attendre_approbation_avec_delai(&self, pair: NovaId, delai: Duration) -> bool {
        let peer = pair.as_u64();
        // 1. Enregistre l'attente AVANT de notifier : évite la course où une
        //    réponse du Dart arriverait avant l'enregistrement de la demande.
        {
            let mut etat = self.etat.lock().unwrap_or_else(PoisonError::into_inner);
            if etat.arret {
                return false;
            }
            etat.decisions.insert(peer, None);
        }
        // 2. Notifie le Dart (best-effort : sans abonné, la demande expirera).
        if let Some(sink) = self
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = sink.add(IncomingRequestDto {
                peer_id: peer,
                peer_id_formate: pair.to_string(),
            });
        }
        // 3. Bloque jusqu'à décision / arrêt / expiration.
        let echeance = Instant::now() + delai;
        let mut etat = self.etat.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if etat.arret {
                etat.decisions.remove(&peer);
                return false;
            }
            if let Some(Some(decision)) = etat.decisions.get(&peer).copied() {
                etat.decisions.remove(&peer);
                return decision;
            }
            let restant = echeance.saturating_duration_since(Instant::now());
            if restant.is_zero() {
                etat.decisions.remove(&peer);
                return false;
            }
            let (garde, _delai) = self
                .signal
                .wait_timeout(etat, restant)
                .unwrap_or_else(PoisonError::into_inner);
            etat = garde;
        }
    }

    /// Tranche une demande en attente (appel du Dart). Erreur si aucune demande
    /// n'attend pour ce pair (déjà tranchée, expirée, ou jamais reçue).
    fn approuver(&self, peer: u64, accepter: bool) -> Result<(), String> {
        {
            let mut etat = self.etat.lock().unwrap_or_else(PoisonError::into_inner);
            match etat.decisions.get_mut(&peer) {
                Some(slot) => *slot = Some(accepter),
                None => {
                    return Err(format!(
                        "aucune demande d'accès en attente pour le pair {} \
                         (déjà tranchée, expirée, ou jamais reçue)",
                        NovaId(peer)
                    ))
                }
            }
        }
        self.signal.notify_all();
        Ok(())
    }

    /// Enregistre (ou remplace) le sink des demandes entrantes.
    fn abonner(&self, sink: StreamSink<IncomingRequestDto>) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

    /// Demande l'arrêt : refuse toute attente en cours et la réveille aussitôt.
    fn demander_arret(&self) {
        self.etat
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .arret = true;
        self.signal.notify_all();
    }
}

/// Une entrée de la table des hôtes non surveillés : la poignée du service et la
/// file d'approbation partagée avec son hook `accept`.
struct EntreeHote {
    poignee: UnattendedHostHandle,
    approbation: Arc<ApprobationHote>,
}

/// Table des hôtes non surveillés vivants, indexée par identifiant opaque.
type TableHotes = Mutex<HashMap<u64, EntreeHote>>;

/// Prochain identifiant d'hôte (compteur monotone, distinct des sessions).
static PROCHAIN_ID_HOTE: AtomicU64 = AtomicU64::new(1);

/// Table statique unique des hôtes non surveillés.
static HOTES: OnceLock<TableHotes> = OnceLock::new();

/// Verrouille la table des hôtes (empoisonnement absorbé, cf. [`verrou`]).
fn verrou_hotes() -> MutexGuard<'static, HashMap<u64, EntreeHote>> {
    HOTES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Exécute `action` sur l'hôte `host_id`, avec une erreur lisible s'il est inconnu.
fn avec_hote<R>(host_id: u64, action: impl FnOnce(&mut EntreeHote) -> R) -> Result<R, String> {
    let mut table = verrou_hotes();
    let entree = table.get_mut(&host_id).ok_or_else(|| {
        format!("hôte non surveillé {host_id} inconnu (jamais démarré ou déjà arrêté)")
    })?;
    Ok(action(entree))
}

/// Démarre un hôte « accès non surveillé » et l'enregistre dans la table.
/// L'`accept` du moteur consulte la file d'approbation pilotée par le Dart.
pub(crate) fn demarrer_hote_non_surveille(
    local_id: u64,
    rendezvous: String,
    stun_servers: Vec<String>,
    permissions: PermissionsDto,
) -> Result<u64, String> {
    let serveur = parser_adresse("du serveur de rendez-vous", &rendezvous)?;
    let stun = parser_adresses_stun(&stun_servers)?;
    let identite = ServerIdentity::generate()
        .map_err(|e| format!("génération de l'identité TLS de l'hôte impossible : {e}"))?;
    let permissions_moteur = PermissionSet::from(Permissions::from(permissions));

    let approbation = Arc::new(ApprobationHote::new());
    let approbation_accept = Arc::clone(&approbation);
    let poignee = UnattendedHost::start(
        NovaId(local_id),
        serveur,
        stun,
        identite,
        permissions_moteur,
        move |pair| approbation_accept.attendre_approbation(pair),
    )
    .map_err(|e| format!("démarrage de l'hôte non surveillé impossible : {e}"))?;

    let id = PROCHAIN_ID_HOTE.fetch_add(1, Ordering::Relaxed);
    verrou_hotes().insert(
        id,
        EntreeHote {
            poignee,
            approbation,
        },
    );
    Ok(id)
}

/// Abonne `sink` au flux des demandes d'accès entrantes de l'hôte `host_id`.
pub(crate) fn flux_demandes_entrantes(
    host_id: u64,
    sink: StreamSink<IncomingRequestDto>,
) -> Result<(), String> {
    let approbation = avec_hote(host_id, |entree| Arc::clone(&entree.approbation))?;
    approbation.abonner(sink);
    Ok(())
}

/// Tranche une demande d'accès entrante de l'hôte `host_id` (débloque l'`accept`).
pub(crate) fn approuver_entrant(host_id: u64, peer_id: u64, accepter: bool) -> Result<(), String> {
    let approbation = avec_hote(host_id, |entree| Arc::clone(&entree.approbation))?;
    approbation.approuver(peer_id, accepter)
}

/// Statistiques cumulées de l'hôte `host_id` (backend d'encodage non exposé
/// par la poignée : reste `None`).
pub(crate) fn statistiques_hote(host_id: u64) -> Result<SessionStatsDto, String> {
    avec_hote(host_id, |entree| entree.poignee.stats().into())
}

/// Arrête l'hôte `host_id` et le retire de la table. `demander_arret` réveille
/// d'abord une approbation éventuellement bloquée (refus), puis `stop()` — appelé
/// **hors verrou** — attend la fin du thread de service (au plus ~5 s).
pub(crate) fn arreter_hote_non_surveille(host_id: u64) -> Result<(), String> {
    let entree = verrou_hotes().remove(&host_id).ok_or_else(|| {
        format!("hôte non surveillé {host_id} inconnu (jamais démarré ou déjà arrêté)")
    })?;
    entree.approbation.demander_arret();
    entree.poignee.stop();
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests unitaires : analyse d'adresses, mappage d'endpoint, file d'approbation
// (aucune session réseau réelle).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Analyse d'adresses ------------------------------------------------

    #[test]
    fn parser_adresse_valide_et_invalide() {
        assert_eq!(
            parser_adresse("du test", "127.0.0.1:9000").expect("adresse valide"),
            "127.0.0.1:9000".parse::<SocketAddr>().expect("littéral")
        );
        // Les espaces autour de l'adresse sont tolérés.
        assert!(parser_adresse("du test", "  127.0.0.1:1  ").is_ok());
        let err = parser_adresse("du serveur de rendez-vous", "pas-une-adresse").unwrap_err();
        assert!(err.contains("invalide"), "message peu utile : {err}");
        assert!(err.contains("rendez-vous"), "l'étiquette manque : {err}");
    }

    #[test]
    fn parser_adresses_stun_valide_et_situe_l_erreur() {
        assert!(parser_adresses_stun(&[]).expect("liste vide").is_empty());
        let ok = parser_adresses_stun(&["1.2.3.4:5".to_owned(), "9.9.9.9:53".to_owned()])
            .expect("adresses valides");
        assert_eq!(ok.len(), 2);
        // Le message situe l'entrée fautive par son rang (ici la 2e).
        let err = parser_adresses_stun(&["1.2.3.4:5".to_owned(), "oups".to_owned()]).unwrap_err();
        assert!(err.contains("n°2"), "rang manquant : {err}");
    }

    // -- Mappage de l'endpoint par rendez-vous -----------------------------

    #[test]
    fn preparer_endpoint_rendezvous_valide() {
        let dto = SessionEndpointDto::ByRendezvous {
            server: "127.0.0.1:9000".to_owned(),
            stun_servers: vec!["127.0.0.1:3478".to_owned()],
            relay: Some("127.0.0.1:5000".to_owned()),
        };
        let (endpoint, ecoute) = preparer_endpoint(dto).expect("endpoint valide");
        assert!(
            ecoute.is_none(),
            "le rendez-vous ne publie pas d'écoute locale"
        );
        match endpoint {
            SessionEndpoint::ByRendezvous {
                server,
                stun_servers,
                relay,
            } => {
                assert_eq!(server, "127.0.0.1:9000".parse().expect("serveur"));
                assert_eq!(stun_servers.len(), 1);
                assert_eq!(relay, Some("127.0.0.1:5000".parse().expect("relais")));
            }
            _ => panic!("variante d'endpoint inattendue"),
        }
    }

    #[test]
    fn preparer_endpoint_rendezvous_invalide() {
        let dto = SessionEndpointDto::ByRendezvous {
            server: "xxx".to_owned(),
            stun_servers: vec![],
            relay: None,
        };
        // `SessionEndpoint` n'implémente pas `Debug` (il porte un `Listener`) :
        // on filtre l'erreur par motif plutôt que via `unwrap_err`.
        let Err(err) = preparer_endpoint(dto) else {
            panic!("une adresse de rendez-vous illisible doit être refusée");
        };
        assert!(
            err.contains("invalide") && err.contains("rendez-vous"),
            "message peu utile : {err}"
        );
    }

    // -- File d'approbation entrante (approve / deny / timeout / arrêt) -----

    /// Attend que l'`accept` ait enregistré sa demande (évite la course avec la
    /// réponse), puis rend la main.
    fn attendre_enregistrement(appro: &ApprobationHote, peer: u64) {
        let echeance = Instant::now() + Duration::from_secs(2);
        while Instant::now() < echeance {
            if appro
                .etat
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .decisions
                .contains_key(&peer)
            {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("la demande d'approbation n'a jamais été enregistrée");
    }

    /// Lance une attente d'approbation dans un thread et rend sa poignée.
    fn lancer_attente(
        appro: &Arc<ApprobationHote>,
        pair: NovaId,
        delai: Duration,
    ) -> thread::JoinHandle<bool> {
        let a = Arc::clone(appro);
        thread::spawn(move || a.attendre_approbation_avec_delai(pair, delai))
    }

    #[test]
    fn approbation_acceptee_par_approve() {
        let appro = Arc::new(ApprobationHote::new());
        let pair = NovaId(123_456_789);
        let attente = lancer_attente(&appro, pair, Duration::from_secs(5));
        attendre_enregistrement(&appro, pair.as_u64());
        appro.approuver(pair.as_u64(), true).expect("approbation");
        assert!(
            attente.join().expect("thread"),
            "l'appelant doit être accepté"
        );
    }

    #[test]
    fn approbation_refusee_par_deny() {
        let appro = Arc::new(ApprobationHote::new());
        let pair = NovaId(42);
        let attente = lancer_attente(&appro, pair, Duration::from_secs(5));
        attendre_enregistrement(&appro, pair.as_u64());
        appro
            .approuver(pair.as_u64(), false)
            .expect("refus explicite");
        assert!(
            !attente.join().expect("thread"),
            "l'appelant doit être refusé"
        );
    }

    #[test]
    fn approbation_expire_en_refus() {
        let appro = ApprobationHote::new();
        let debut = Instant::now();
        // Délai bref, aucune réponse : refus par défaut, sans blocage.
        let accepte = appro.attendre_approbation_avec_delai(NovaId(7), Duration::from_millis(80));
        assert!(!accepte, "l'expiration doit refuser par défaut");
        assert!(
            debut.elapsed() < Duration::from_secs(5),
            "l'attente ne doit pas se bloquer"
        );
    }

    #[test]
    fn approbation_arret_refuse_immediatement() {
        let appro = Arc::new(ApprobationHote::new());
        let pair = NovaId(99);
        // Délai long : seul l'arrêt doit débloquer (pas l'expiration).
        let attente = lancer_attente(&appro, pair, Duration::from_secs(30));
        attendre_enregistrement(&appro, pair.as_u64());
        let debut = Instant::now();
        appro.demander_arret();
        assert!(!attente.join().expect("thread"), "l'arrêt doit refuser");
        assert!(
            debut.elapsed() < Duration::from_secs(5),
            "arrêt non immédiat"
        );
    }

    #[test]
    fn approuver_sans_demande_echoue() {
        let appro = ApprobationHote::new();
        let err = appro.approuver(555, true).unwrap_err();
        assert!(err.contains("aucune demande"), "message peu utile : {err}");
    }
}
