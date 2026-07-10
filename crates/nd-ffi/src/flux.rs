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
use nd_codec::{ContentProfile, DecodedFrame};
use nd_core::{
    ChatMessage, SessionEndpoint, SessionEngine, SessionHandle, SessionMedia, SessionOptions,
    SessionState, TunnelHandle, UnattendedHost, UnattendedHostHandle,
};
use nd_features::decouverte::{
    AnnonceurPresence, EcouteurPresence, OptionsEcoute, PORT_DECOUVERTE_DEFAUT,
};
use nd_features::{AnnotationLayer, Capability, PermissionSet, Permissions};
use nd_files::{ClipboardSync, TransferEvent};
use nd_proto::{InputEvent, NovaId};
use nd_transport::ServerIdentity;

use crate::api::{
    AnnotationDto, ChatMessageDto, DiscoveredPeerDto, EntreeFsDto, IncomingRequestDto,
    ListenInfoDto, MonitorInfoDto, PeerInfoDto, PermissionsDto, SessionConfigDto,
    SessionEndpointDto, SessionOptionsDto, SessionStateDto, SessionStatsDto, TransferEventDto,
    TunnelOuvertDto, VideoFrameDto,
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
    /// Couches d'annotation / tableau blanc reçues du pair (mode étendu), tant
    /// qu'aucun consommateur ne les a prises.
    annotations: Option<Receiver<AnnotationLayer>>,
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
    let annotations = Some(std::mem::replace(
        &mut poignee.annotation_rx,
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
            annotations,
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
    // Ferme d'abord les tunnels TCP de la session (cesse d'accepter, joint les
    // fils d'acceptation), puis arrête le moteur.
    fermer_tunnels_interne(id);
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
// Capacités moteur avancées : confidentialité, cadre d'écran, tunnel, annotation
// ---------------------------------------------------------------------------
//
// Comme les commandes média étendues, ces fonctions délèguent aux méthodes de
// [`SessionHandle`] déjà livrées. Confidentialité / région / annotation postent
// sur un canal interne (inertes hors mode étendu ou permission absente, mais
// toujours `Ok` tant que la session existe) ; le tunnel lie en revanche
// **immédiatement** un écouteur TCP local et peut donc échouer.

/// Active (ou lève) le mode confidentialité de la session.
pub(crate) fn definir_confidentialite(id: u64, actif: bool) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.set_privacy(actif))
}

/// État du mode confidentialité connu localement (indicateur à afficher).
pub(crate) fn confidentialite_active(id: u64) -> Result<bool, String> {
    avec_entree(id, |entree| entree.poignee.privacy_active())
}

/// Restreint la zone d'écran partagée (« cadre d'écran ») ou rétablit le plein
/// écran avec `None`.
pub(crate) fn definir_region(id: u64, region: Option<(u32, u32, u32, u32)>) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.set_region(region))
}

/// Cadre d'écran actuellement demandé (`None` = plein écran).
pub(crate) fn region_demandee(id: u64) -> Result<Option<(u32, u32, u32, u32)>, String> {
    avec_entree(id, |entree| entree.poignee.requested_region())
}

/// Ouvre un tunnel TCP de session : écoute sur `127.0.0.1:port_local` et relaie
/// vers `cible` (« ip:port ») à travers la session. Stocke la poignée du tunnel
/// dans la table dédiée (fermée par [`fermer_tunnels`] ou à l'arrêt de la
/// session) et renvoie l'adresse locale réellement écoutée.
pub(crate) fn ouvrir_tunnel(
    id: u64,
    port_local: u16,
    cible: String,
) -> Result<TunnelOuvertDto, String> {
    // L'analyse de la cible précède la recherche de session : une saisie
    // invalide échoue avec un message français clair, sans toucher à la session.
    let cible_addr = parser_adresse("de la cible du tunnel", &cible)?;
    ouvrir_tunnel_vers(id, port_local, cible_addr)
}

/// Cœur de l'ouverture de tunnel, la cible étant déjà résolue en [`SocketAddr`]
/// (voir [`ouvrir_tunnel`] pour la variante à cible texte « ip:port », et
/// [`crate::api::session_open_tunnel`] pour la variante à hôte et port séparés).
pub(crate) fn ouvrir_tunnel_vers(
    id: u64,
    port_local: u16,
    cible: SocketAddr,
) -> Result<TunnelOuvertDto, String> {
    let tunnel = avec_entree(id, |entree| entree.poignee.open_tunnel(port_local, cible))?
        .map_err(|e| format!("ouverture du tunnel impossible : {e}"))?;
    let adresse_locale = tunnel.local_addr();
    let dto = TunnelOuvertDto {
        adresse_locale: adresse_locale.to_string(),
        port_local: adresse_locale.port(),
    };
    verrou_tunnels().entry(id).or_default().push(tunnel);
    Ok(dto)
}

/// Ferme tous les tunnels TCP ouverts pour la session `id` (idempotent : aucune
/// erreur si la session n'a aucun tunnel).
pub(crate) fn fermer_tunnels(id: u64) -> Result<(), String> {
    fermer_tunnels_interne(id);
    Ok(())
}

/// Envoie une couche d'annotation au pair (un seul trait, bâti depuis le DTO
/// plat). Sans effet hors mode étendu.
pub(crate) fn envoyer_annotation(id: u64, annotation: AnnotationDto) -> Result<(), String> {
    // La conversion (validation du genre / des points) peut échouer avant tout
    // accès à la session.
    let couche = crate::api::couche_depuis_annotation(&annotation)?;
    avec_entree(id, |entree| entree.poignee.send_annotation(couche))
}

// ---------------------------------------------------------------------------
// Plan de contrôle de session : permissions à chaud, qualité, enregistrement,
// moniteurs, infos du pair
// ---------------------------------------------------------------------------

/// Traduit une **clé de capacité** (contrat UI stable) en [`Capability`]. Une
/// clé inconnue renvoie une erreur française listant les valeurs acceptées
/// (l'analyse précède tout accès à la session).
fn capacite_depuis_cle(cle: &str) -> Result<Capability, String> {
    let capacite = match cle {
        "voir_ecran" => Capability::ViewScreen,
        "souris" => Capability::ControlMouse,
        "clavier" => Capability::ControlKeyboard,
        "presse_papiers_lecture" => Capability::ClipboardRead,
        "presse_papiers_ecriture" => Capability::ClipboardWrite,
        "fichiers_envoi" => Capability::FileUpload,
        "fichiers_reception" => Capability::FileDownload,
        "audio" => Capability::Audio,
        "redemarrage" => Capability::RestartRemote,
        "enregistrement" => Capability::SessionRecording,
        "confidentialite" => Capability::PrivacyMode,
        "tunnel" => Capability::TcpTunnel,
        autre => {
            return Err(format!(
                "capacité inconnue : « {autre} » (attendu : voir_ecran, souris, clavier, \
                 presse_papiers_lecture, presse_papiers_ecriture, fichiers_envoi, \
                 fichiers_reception, audio, redemarrage, enregistrement, confidentialite, tunnel)"
            ))
        }
    };
    Ok(capacite)
}

/// Traduit un **préréglage de qualité** (contrat UI) en `(profil ABR, plafond
/// kbit/s)`. `0` = aucun plafond. Un préréglage inconnu renvoie une erreur.
fn qualite_depuis_preset(preset: &str) -> Result<(ContentProfile, u32), String> {
    let cible = match preset {
        "auto" => (ContentProfile::Text, 0),
        "fluide" => (ContentProfile::Video, 0),
        "equilibre" => (ContentProfile::Video, 5_000),
        // Tolère la saisie avec ou sans accent.
        "nettete" | "netteté" => (ContentProfile::Text, 0),
        autre => {
            return Err(format!(
                "préréglage de qualité inconnu : « {autre} » \
                 (attendu : auto, fluide, equilibre, netteté)"
            ))
        }
    };
    Ok(cible)
}

/// Renégocie une permission à chaud : lit l'ensemble vivant de la session,
/// accorde/retire la capacité, puis pousse le nouvel ensemble à l'hôte.
pub(crate) fn definir_permission(id: u64, capacite: &str, autorise: bool) -> Result<(), String> {
    // L'analyse de la clé précède l'accès à la session (erreur claire, sans
    // toucher à la session pour une clé fautive).
    let cap = capacite_depuis_cle(capacite)?;
    avec_entree(id, |entree| {
        let mut permissions = entree.poignee.current_permissions();
        if autorise {
            permissions.grant(cap);
        } else {
            permissions.revoke(cap);
        }
        entree.poignee.set_permissions(permissions);
    })
}

/// Applique un préréglage de qualité (profil ABR + plafond de débit).
pub(crate) fn definir_qualite(id: u64, preset: &str) -> Result<(), String> {
    let (profil, plafond) = qualite_depuis_preset(preset)?;
    avec_entree(id, |entree| entree.poignee.set_quality(profil, plafond))
}

/// Démarre (chemin) ou arrête (`None`) l'enregistrement local de l'hôte à chaud.
pub(crate) fn definir_enregistrement(id: u64, chemin: Option<PathBuf>) -> Result<(), String> {
    avec_entree(id, |entree| entree.poignee.set_recording(chemin))
}

/// Liste des moniteurs publiée par l'hôte (vide tant qu'elle n'est pas arrivée).
pub(crate) fn moniteurs(id: u64) -> Result<Vec<MonitorInfoDto>, String> {
    let liste = avec_entree(id, |entree| entree.poignee.monitors())?;
    Ok(liste
        .unwrap_or_default()
        .into_iter()
        .map(MonitorInfoDto::from)
        .collect())
}

/// Infos système du pair (erreur tant que l'annonce n'est pas arrivée).
pub(crate) fn infos_pair(id: u64) -> Result<PeerInfoDto, String> {
    let infos = avec_entree(id, |entree| entree.poignee.peer_info())?;
    infos.map(PeerInfoDto::from).ok_or_else(|| {
        format!("infos système du pair non encore reçues pour la session {id} (annonce en attente)")
    })
}

// ---------------------------------------------------------------------------
// Listing de répertoire distant (brique `nd_files` routée DANS la session)
// ---------------------------------------------------------------------------

/// Liste le répertoire distant `chemin` via la session `id` (rôle contrôleur,
/// mode étendu) et aplatit les entrées en DTO. L'erreur applicative de l'hôte
/// (accès refusé sans la permission fichiers/réception, dossier inexistant…)
/// est propagée en `Err(String)` — jamais de liste partielle trompeuse.
pub(crate) fn lister_repertoire_distant(
    id: u64,
    chemin: String,
) -> Result<Vec<EntreeFsDto>, String> {
    // 1. Sous verrou : détache le client de listing de la poignée. La table
    //    des sessions n'est PAS retenue pendant l'attente de la réponse —
    //    `send_input`, les statistiques… restent fluides pendant ce temps.
    let client = avec_entree(id, |entree| entree.poignee.remote_fs())?;
    // 2. Hors verrou : requête sur le canal `Control` chiffré + attente de la
    //    réponse corrélée (délai borné par le moteur, ~10 s).
    let reponse = client
        .lister(chemin)
        .map_err(|e| format!("listing distant impossible : {e}"))?;
    // 3. Le refus ou l'échec côté hôte se propage tel quel (message lisible).
    if let Some(erreur) = reponse.erreur {
        return Err(erreur);
    }
    Ok(reponse.entrees.into_iter().map(EntreeFsDto::from).collect())
}

// --- Table des tunnels TCP par session --------------------------------------

/// Tunnels TCP ouverts par session : la [`TunnelHandle`] doit vivre aussi
/// longtemps que le tunnel (son `Drop`/`close` cesse d'accepter les connexions
/// locales). Table distincte de [`SESSIONS`] pour qu'un `close` bloquant (join
/// du fil d'acceptation) ne retienne pas le verrou des sessions.
type TableTunnels = Mutex<HashMap<u64, Vec<TunnelHandle>>>;

/// Table statique des tunnels, indexée par identifiant de session.
static TUNNELS: OnceLock<TableTunnels> = OnceLock::new();

/// Verrouille la table des tunnels (empoisonnement absorbé, cf. [`verrou`]).
fn verrou_tunnels() -> MutexGuard<'static, HashMap<u64, Vec<TunnelHandle>>> {
    TUNNELS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Retire et ferme les tunnels de la session `id`. Le verrou est relâché avant
/// les `close` (qui joignent les fils d'acceptation), pour ne pas bloquer les
/// autres opérations de tunnel pendant l'attente.
fn fermer_tunnels_interne(id: u64) {
    let tunnels = verrou_tunnels().remove(&id).unwrap_or_default();
    for tunnel in tunnels {
        tunnel.close();
    }
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

/// Branche le flux d'annotations (mode étendu) : prend définitivement le
/// récepteur d'annotations et lance un thread qui, pour **chaque couche reçue**,
/// pousse **un [`AnnotationDto`] par trait** vers `sink` (une couche peut porter
/// plusieurs traits ; le DTO plat en représente un seul). Même motif que les
/// autres drains, mais un-vers-plusieurs : d'où un thread dédié plutôt que
/// [`demarrer_drain`] (conversion un-vers-un). S'arrête à la fin de la session
/// (canal déconnecté) ou à l'annulation du `Stream` côté Dart (`add` en échec).
pub(crate) fn flux_annotations(id: u64, sink: StreamSink<AnnotationDto>) -> Result<(), String> {
    let annotations = avec_entree(id, |entree| entree.annotations.take())?.ok_or_else(|| {
        format!("les annotations de la session {id} sont déjà consommées (flux en cours)")
    })?;
    let nom = format!("nd-ffi-annotations-{id}");
    thread::Builder::new()
        .name(nom.clone())
        .spawn(move || {
            'boucle: while let Ok(couche) = annotations.recv() {
                for dto in crate::api::annotations_depuis_couche(&couche) {
                    if sink.add(dto).is_err() {
                        // Flux annulé côté Dart : on cesse de drainer.
                        break 'boucle;
                    }
                }
            }
        })
        .map(|_poignee| ())
        .map_err(|e| format!("création du thread « {nom} » impossible : {e}"))
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
// est consulté sur le thread de service, une connexion à la fois. Avec
// l'admission automatique ([`UnattendedHost::start_with_admission`]), il n'est
// plus que le **repli** : seuls les appelants sans preuve (ni appareil de
// confiance, ni mot de passe — auquel cas rien n'a encore été honoré dans le
// canal chiffré) y aboutissent. On implémente donc une **file d'approbation
// bloquante** pilotée par le Dart :
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
///
/// L'admission est **automatique** ([`UnattendedHost::start_with_admission`]) :
/// le vérificateur du mot de passe permanent et la confiance de l'appelant
/// viennent de l'état persistant ([`crate::etat`]). La **confiance à l'admission**
/// vaut **liste blanche d'admission ∪ appareils de confiance** (un ID de l'une ou
/// l'autre liste est accepté sans mot de passe). Le clair reçu du canal Noise
/// n'est comparé qu'au **hachage salé** stocké (déchiffré au repos à la volée),
/// jamais conservé ni journalisé, et toute erreur de lecture vaut refus (fermé
/// par défaut). Sans preuve, l'`accept` du moteur se replie sur la file
/// d'approbation pilotée par le Dart (le dialogue manuel existant).
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
    let poignee = UnattendedHost::start_with_admission(
        NovaId(local_id),
        serveur,
        stun,
        identite,
        permissions_moteur,
        // Repli manuel : dialogue de l'UI (refus par défaut à l'expiration).
        move |pair| approbation_accept.attendre_approbation(pair),
        // Vérificateur du mot de passe permanent : recalcul BLAKE3 salé contre
        // le hachage persisté (déchiffré au repos à la volée — `etat`).
        |mdp: &str| {
            crate::etat::magasin()
                .verifier_mot_de_passe_non_surveille(mdp.to_owned())
                .unwrap_or(false)
        },
        // Confiance à l'admission = **liste blanche d'admission ∪ appareils de
        // confiance** : un ID de l'une OU l'autre liste persistée (`etat`) est
        // traité comme appareil de confiance (accepté sans mot de passe). Toute
        // erreur de lecture vaut refus (fermé par défaut).
        |pair: NovaId| {
            let magasin = crate::etat::magasin();
            let id = pair.as_u64();
            magasin.appareil_de_confiance(id).unwrap_or(false)
                || magasin.admission_contient(id).unwrap_or(false)
        },
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
// Découverte LAN (`nd_features::decouverte`) : instance unique du processus
// ---------------------------------------------------------------------------
//
// Comme les sessions et les hôtes non surveillés, la découverte vit dans un
// état statique — mais **au plus une** instance (le beacon annonce l'identité
// du poste, l'écouteur tient la table des voisins) : un `Option` sous mutex
// plutôt qu'une table. `discovery_start` est idempotent tant qu'elle vit ;
// `discovery_stop` la retire puis joint ses fils **hors verrou**.

/// Découverte LAN vivante : le beacon d'annonce et l'écouteur des voisins.
struct EtatDecouverte {
    /// Annonce périodique `(id local, nom)` — arrêtée en la consommant.
    annonceur: AnnonceurPresence,
    /// Collecte des annonces des voisins (dédupliqués, expirés, id local exclu).
    ecouteur: EcouteurPresence,
}

/// Instance de découverte du processus (`None` = arrêtée).
static DECOUVERTE: OnceLock<Mutex<Option<EtatDecouverte>>> = OnceLock::new();

/// Verrouille l'instance de découverte (empoisonnement absorbé, cf. [`verrou`]).
fn verrou_decouverte() -> MutexGuard<'static, Option<EtatDecouverte>> {
    DECOUVERTE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Démarre la découverte LAN : annonceur (identité locale persistante + `nom`)
/// et écouteur (id local exclu) sur `port` (`0` → [`PORT_DECOUVERTE_DEFAUT`]).
/// **Idempotent** : si une instance vit déjà, l'appel est sans effet — les
/// arguments d'un second appel ne la reconfigurent pas ([`arreter_decouverte`]
/// d'abord pour changer de nom ou de port).
pub(crate) fn demarrer_decouverte(nom: &str, port: u16) -> Result<(), String> {
    let port = if port == 0 {
        PORT_DECOUVERTE_DEFAUT
    } else {
        port
    };
    let mut garde = verrou_decouverte();
    if garde.is_some() {
        return Ok(());
    }
    // Identité locale stable (créée et persistée au premier lancement) : l'id
    // annoncé aux voisins est celui que le pair composera pour se connecter.
    let id = NovaId(crate::etat::magasin().identite_locale()?.id);
    // L'écouteur d'abord : c'est lui qui lie le port partagé du parc (l'échec
    // le plus probable — port déjà occupé — survient avant d'annoncer quoi que
    // ce soit). L'annonceur, lui, émet depuis un port éphémère.
    let ecouteur = EcouteurPresence::demarrer_avec(
        port,
        OptionsEcoute {
            exclure: Some(id),
            ..OptionsEcoute::default()
        },
    )
    .map_err(|e| format!("écoute de découverte impossible sur le port {port} : {e}"))?;
    let annonceur = AnnonceurPresence::demarrer(id, nom, port)
        .map_err(|e| format!("annonce de présence impossible : {e}"))?;
    *garde = Some(EtatDecouverte {
        annonceur,
        ecouteur,
    });
    Ok(())
}

/// Instantané des pairs découverts, aplati en DTO : dédupliqués par id, expirés
/// au-delà du TTL, le poste local exclu, triés par id croissant (garanties de
/// [`EcouteurPresence::pairs`]). Liste vide si la découverte n'est pas démarrée.
pub(crate) fn pairs_decouverts() -> Vec<DiscoveredPeerDto> {
    let garde = verrou_decouverte();
    let Some(etat) = garde.as_ref() else {
        return Vec::new();
    };
    etat.ecouteur
        .pairs()
        .into_iter()
        .map(|pair| DiscoveredPeerDto {
            id: pair.id.as_u64(),
            // Format groupé par 3 (« 123 456 789 »), celui de l'affichage.
            id_formate: pair.id.to_string(),
            nom: pair.nom,
            adresse: pair.adresse.to_string(),
        })
        .collect()
}

/// Arrête la découverte LAN (idempotent : sans effet si elle ne vit pas).
/// Les fils sont joints **hors verrou** (annonce ≤ une période, écoute ≤ 200 ms).
pub(crate) fn arreter_decouverte() {
    let etat = verrou_decouverte().take();
    if let Some(EtatDecouverte {
        annonceur,
        ecouteur,
    }) = etat
    {
        annonceur.arreter();
        ecouteur.arreter();
    }
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

    // -- Tunnel : analyse de la cible --------------------------------------

    #[test]
    fn ouvrir_tunnel_refuse_une_cible_invalide() {
        // L'analyse de la cible précède la recherche de session : une adresse
        // illisible échoue proprement, sans session vivante ni écouteur lié.
        let err = ouvrir_tunnel(999_999, 0, "pas-une-adresse".to_owned()).unwrap_err();
        assert!(err.contains("invalide"), "message peu utile : {err}");
        assert!(
            err.contains("tunnel"),
            "l'étiquette de la cible manque : {err}"
        );
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
