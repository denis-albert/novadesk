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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nd_codec::DecodedFrame;
use nd_core::{SessionEndpoint, SessionEngine, SessionHandle, SessionState};
use nd_proto::InputEvent;

use crate::api::{
    ListenInfoDto, SessionConfigDto, SessionEndpointDto, SessionStateDto, SessionStatsDto,
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

/// Démarre le moteur de session et enregistre la poignée dans la table.
/// Renvoie l'identifiant opaque attribué.
pub(crate) fn demarrer_session(
    config: SessionConfigDto,
    endpoint: SessionEndpointDto,
) -> Result<u64, String> {
    let (endpoint_moteur, ecoute) = preparer_endpoint(endpoint)?;
    let mut poignee = SessionEngine::start(config.into(), endpoint_moteur)
        .map_err(|e| format!("démarrage de la session impossible : {e}"))?;

    // Extrait les récepteurs de la poignée en les échangeant contre des canaux
    // factices (dont l'émetteur est aussitôt lâché) : `SessionHandle` implémente
    // `Drop`, la déstructuration est donc interdite, mais l'échange de champs
    // publics reste sûr — la poignée n'utilise pas ses récepteurs en interne.
    let etats = Some(std::mem::replace(&mut poignee.state_rx, mpsc::channel().1));
    let frames = Some(std::mem::replace(&mut poignee.frame_rx, mpsc::channel().1));

    let id = PROCHAIN_ID.fetch_add(1, Ordering::Relaxed);
    verrou().insert(
        id,
        EntreeSession {
            poignee,
            etats,
            frames,
            ecoute,
        },
    );
    Ok(id)
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
            let adresse: SocketAddr = addr
                .parse()
                .map_err(|e| format!("adresse « {addr} » invalide : {e}"))?;
            Ok((
                SessionEndpoint::Direct {
                    addr: adresse,
                    cert_der,
                },
                None,
            ))
        }
    }
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

/// Instantané des statistiques du moteur, converti en DTO plat.
pub(crate) fn statistiques(id: u64) -> Result<SessionStatsDto, String> {
    avec_entree(id, |entree| entree.poignee.stats().into())
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
