//! Orchestrateur de session réutilisable : [`SessionEngine`] câble les briques réelles
//! (QUIC → chiffrement Noise → capture/codec/entrées) sur des **threads dédiés**,
//! pilote les transitions [`SessionState`] et expose les sorties à un consommateur
//! (future UI, voir plan 10) : frames décodées, statistiques continues, canal
//! d'entrées à transmettre.
//!
//! Architecture (pas de runtime async : threads `std` + `std::sync::mpsc`) :
//!
//! ```text
//! SessionEngine::start(config, endpoint) ──► thread « nd-session-pilote »
//!   états poussés dans state_rx : Resolving → Connecting → Handshaking → Active → Closed
//!
//!   contrôleur : connect → establish(Initiator)
//!     ├─► ViewerPipeline::run_streaming (pilote) ──► frame_rx (+ fps/frames_decoded)
//!     └─► thread « nd-session-entrees » : input_rx → canal Input chiffré
//!
//!   contrôlé   : accept → establish(Responder)
//!     ├─► HostPipeline::run_streaming (pilote) : capture → H.264 → QUIC chiffré
//!     └─► thread « nd-session-injection » : poll_recv → apply_input (+ inputs_applied)
//! ```
//!
//! Les deux threads d'un même rôle partagent le transport chiffré via
//! `TransportPartage` (verrou + comptage octets/RTT) : chaque côté n'a qu'un seul
//! thread émetteur et un seul thread récepteur, ce qui préserve l'ordre des nonces
//! Noise dans chaque direction. L'identité Noise est pour l'instant **éphémère**
//! (une paire de clés par session) ; l'identité persistante (`IdentityStore`) et la
//! traversée NAT complète (STUN/punch/relais) arrivent aux lots suivants (plan 05/06).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nd_capture::create_capturer;
use nd_codec::{create_decoder, create_encoder, CodecKind, DecodedFrame};
use nd_crypto::{generate_static_keypair, HandshakeRole};
use nd_input::create_injector;
use nd_proto::{ChannelKind, InputEvent, NdError, NovaId, Reliability, Result};
use nd_signaling::{PeerRecord, RendezvousClient};
use nd_transport::{bind, connect, ChannelHandle, Listener, PathEstimate, Transport};

use crate::{
    apply_input, establish, HostPipeline, SessionConfig, SessionRole, SessionState, ViewerPipeline,
};

/// Largeur de la fenêtre glissante de mesure du débit d'images (fps).
const FENETRE_FPS: Duration = Duration::from_secs(1);

/// Période d'échantillonnage du RTT depuis [`Transport::path_estimate`].
const PERIODE_RTT: Duration = Duration::from_millis(100);

/// Profondeur de la file de frames livrées au consommateur : au-delà, les frames
/// excédentaires sont sautées (un consommateur lent ne bloque jamais le décodage).
const PROFONDEUR_FILE_FRAMES: usize = 4;

/// Tentatives de résolution d'un ID au rendez-vous (espacées de 25 ms ≈ 5 s au total).
const TENTATIVES_RESOLUTION: usize = 200;

/// Délai maximal accordé aux threads du moteur pour se terminer dans
/// [`SessionHandle::stop`].
const DELAI_ARRET: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Point de contact réseau
// ---------------------------------------------------------------------------

/// Point de contact réseau : décrit **comment joindre le pair**.
///
/// Les variantes couvrent la mise en relation testable dès maintenant
/// (loopback/LAN) ; la traversée NAT complète (STUN, hole punching, relais) est le
/// lot 05 et s'ajoutera derrière [`SessionEndpoint::ByRendezvous`].
pub enum SessionEndpoint {
    /// La session **accepte** la connexion entrante sur un écouteur QUIC déjà lié
    /// (hôte en loopback/LAN ; l'appelant a publié l'adresse et le certificat).
    Loopback {
        /// Écouteur QUIC lié (voir `nd_transport::bind`).
        listener: Listener,
    },
    /// La session **se connecte** directement à une adresse connue, avec le
    /// certificat épinglé du pair (rôle contrôleur typique en loopback/LAN).
    Direct {
        /// Adresse QUIC (UDP) du pair.
        addr: SocketAddr,
        /// Certificat auto-signé (DER) du pair, épinglé à la connexion.
        cert_der: Vec<u8>,
    },
    /// Résolution **par ID** via un serveur de rendez-vous (`nd-signaling`).
    ///
    /// Repli simple actuel : le contrôleur résout `peer_id` (`lookup`) puis se
    /// connecte en direct ; le contrôlé lie un écouteur loopback, publie son ID
    /// (`register`) puis accepte. STUN/hole punching/relais : lot 05.
    ByRendezvous {
        /// Adresse du serveur de rendez-vous.
        server: SocketAddr,
    },
}

// ---------------------------------------------------------------------------
// Statistiques
// ---------------------------------------------------------------------------

/// Instantané des statistiques d'une session, rafraîchies en continu par les
/// threads du moteur (voir [`SessionHandle::stats`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStats {
    /// Images décodées par seconde, fenêtre glissante d'une seconde (contrôleur).
    pub fps: f32,
    /// RTT du chemin réseau en microsecondes (échantillonné depuis `path_estimate`).
    pub rtt_us: u64,
    /// Octets utiles reçus (charges après déchiffrement, hors handshake).
    pub bytes_in: u64,
    /// Octets utiles émis (charges avant chiffrement, hors handshake).
    pub bytes_out: u64,
    /// Frames décodées livrées au consommateur (contrôleur).
    pub frames_decoded: u64,
    /// Entrées reçues et appliquées à l'OS (contrôlé).
    pub inputs_applied: u64,
}

/// Compteurs partagés entre les threads du moteur, mis à jour en continu.
#[derive(Default)]
struct CompteursSession {
    rtt_us: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    frames_decoded: AtomicU64,
    inputs_applied: AtomicU64,
    /// Horodatages des frames livrées (fenêtre glissante pour le fps).
    fenetre_fps: Mutex<VecDeque<Instant>>,
    /// Dernière erreur d'exécution du pilote (session close en erreur).
    derniere_erreur: Mutex<Option<String>>,
}

impl CompteursSession {
    /// Enregistre la livraison d'une frame décodée (compteur + fenêtre fps).
    fn frame_livree(&self) {
        self.frames_decoded.fetch_add(1, Ordering::Relaxed);
        let mut fenetre = self.fenetre_fps.lock().expect("verrou de la fenêtre fps");
        let maintenant = Instant::now();
        fenetre.push_back(maintenant);
        // Élagage au fil de l'eau : la fenêtre reste bornée (≈ fps × 1 s entrées).
        elaguer_fenetre(&mut fenetre, maintenant);
    }

    /// Mémorise la dernière erreur d'exécution du moteur.
    fn note_erreur(&self, erreur: &NdError) {
        *self
            .derniere_erreur
            .lock()
            .expect("verrou de la dernière erreur") = Some(erreur.to_string());
    }

    /// Instantané cohérent des statistiques.
    fn instantane(&self) -> SessionStats {
        let fps = {
            let mut fenetre = self.fenetre_fps.lock().expect("verrou de la fenêtre fps");
            fps_fenetre_glissante(&mut fenetre, Instant::now())
        };
        SessionStats {
            fps,
            rtt_us: self.rtt_us.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            frames_decoded: self.frames_decoded.load(Ordering::Relaxed),
            inputs_applied: self.inputs_applied.load(Ordering::Relaxed),
        }
    }
}

/// Retire de la fenêtre les horodatages plus vieux que [`FENETRE_FPS`].
fn elaguer_fenetre(fenetre: &mut VecDeque<Instant>, maintenant: Instant) {
    while fenetre
        .front()
        .is_some_and(|t| maintenant.duration_since(*t) > FENETRE_FPS)
    {
        fenetre.pop_front();
    }
}

/// Élague la fenêtre puis renvoie le débit d'images sur la dernière seconde.
fn fps_fenetre_glissante(fenetre: &mut VecDeque<Instant>, maintenant: Instant) -> f32 {
    elaguer_fenetre(fenetre, maintenant);
    fenetre.len() as f32 / FENETRE_FPS.as_secs_f32()
}

// ---------------------------------------------------------------------------
// Transport partagé instrumenté
// ---------------------------------------------------------------------------

/// Transport partagé entre les threads d'un même rôle : sérialise les accès au
/// transport chiffré derrière un verrou et alimente les compteurs de session
/// (octets entrants/sortants, RTT échantillonné toutes les [`PERIODE_RTT`]).
#[derive(Clone)]
struct TransportPartage {
    interne: Arc<Mutex<Box<dyn Transport>>>,
    compteurs: Arc<CompteursSession>,
    /// Prochain échantillonnage du RTT (propre à chaque clone).
    prochain_rtt: Instant,
}

impl TransportPartage {
    fn new(interne: Box<dyn Transport>, compteurs: Arc<CompteursSession>) -> Self {
        Self {
            interne: Arc::new(Mutex::new(interne)),
            compteurs,
            prochain_rtt: Instant::now(),
        }
    }
}

impl Transport for TransportPartage {
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
        self.interne
            .lock()
            .expect("verrou du transport partagé")
            .open_channel(kind)
    }

    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, reliability: Reliability) -> Result<()> {
        let octets = data.len() as u64;
        self.interne
            .lock()
            .expect("verrou du transport partagé")
            .send(ch, data, reliability)?;
        self.compteurs
            .bytes_out
            .fetch_add(octets, Ordering::Relaxed);
        Ok(())
    }

    fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
        let maintenant = Instant::now();
        let echantillonner = maintenant >= self.prochain_rtt;
        if echantillonner {
            self.prochain_rtt = maintenant + PERIODE_RTT;
        }
        let recu = {
            let mut transport = self.interne.lock().expect("verrou du transport partagé");
            if echantillonner {
                self.compteurs
                    .rtt_us
                    .store(transport.path_estimate().rtt_us, Ordering::Relaxed);
            }
            transport.poll_recv()?
        };
        if let Some((_canal, donnees)) = &recu {
            self.compteurs
                .bytes_in
                .fetch_add(donnees.len() as u64, Ordering::Relaxed);
        }
        Ok(recu)
    }

    fn path_estimate(&self) -> PathEstimate {
        self.interne
            .lock()
            .expect("verrou du transport partagé")
            .path_estimate()
    }
}

// ---------------------------------------------------------------------------
// Poignée de session
// ---------------------------------------------------------------------------

/// Poignée d'une session démarrée par [`SessionEngine::start`] : canaux de sortie,
/// statistiques continues et arrêt.
///
/// Côté **contrôlé** (hôte), `frame_rx` ne produit rien et `input_tx` n'est pas
/// consommé : les entrées viennent du pair, pas de la poignée locale.
pub struct SessionHandle {
    /// Transitions d'état poussées par le moteur (`Resolving` → … → `Closed`).
    pub state_rx: Receiver<SessionState>,
    /// Frames décodées (contrôleur). File bornée : quand le consommateur prend du
    /// retard, les frames excédentaires sont sautées plutôt que mises en attente.
    pub frame_rx: Receiver<DecodedFrame>,
    /// Entrées à transmettre au pair (contrôleur), sérialisées sur le canal `Input`.
    pub input_tx: Sender<InputEvent>,
    compteurs: Arc<CompteursSession>,
    stop: Arc<AtomicBool>,
    pilote: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Instantané des statistiques, mises à jour en continu par les threads du moteur.
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        self.compteurs.instantane()
    }

    /// Dernière erreur d'exécution rencontrée par le moteur (`None` tant que la
    /// session vit ou si elle s'est close proprement).
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.compteurs
            .derniere_erreur
            .lock()
            .expect("verrou de la dernière erreur")
            .clone()
    }

    /// Arrête la session : lève le signal d'arrêt puis attend la fin des threads
    /// (au plus ~5 s). Un pilote bloqué dans un `accept()` sans pair entrant est
    /// détaché : il se terminera de lui-même à la première connexion ou à la fin
    /// du processus.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(pilote) = self.pilote.take() {
            let echeance = Instant::now() + DELAI_ARRET;
            while !pilote.is_finished() && Instant::now() < echeance {
                thread::sleep(Duration::from_millis(5));
            }
            if pilote.is_finished() {
                let _ = pilote.join();
            }
        }
    }
}

impl Drop for SessionHandle {
    /// Lâcher la poignée demande l'arrêt des threads **sans** attendre leur fin
    /// (voir [`SessionHandle::stop`] pour un arrêt bloquant).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Moteur
// ---------------------------------------------------------------------------

/// Moteur de session : câble les briques réelles (transport QUIC, chiffrement Noise,
/// pipelines média, entrées) sur des threads dédiés et pilote la machine à états.
///
/// Façade sans état : tout le vivant appartient aux threads et à la
/// [`SessionHandle`] rendue par [`SessionEngine::start`].
pub struct SessionEngine;

impl SessionEngine {
    /// Démarre une session et rend immédiatement la main.
    ///
    /// Lance le thread pilote qui déroule `Resolving → Connecting → Handshaking →
    /// Active` (transitions poussées dans [`SessionHandle::state_rx`]) puis fait
    /// tourner le média selon le rôle : le **contrôleur** décode le flux vidéo vers
    /// [`SessionHandle::frame_rx`] et transmet les [`InputEvent`] postés dans
    /// [`SessionHandle::input_tx`] ; le **contrôlé** diffuse son écran et applique
    /// les entrées reçues. `Closed` est toujours poussé en fin de vie.
    ///
    /// # Errors
    /// Erreur immédiate si la configuration est invalide (contrôleur par rendez-vous
    /// sans `peer_id`) ou si le thread pilote ne peut pas être créé. Les erreurs
    /// d'exécution ultérieures closent la session (`Closed`) et sont consultables
    /// via [`SessionHandle::last_error`].
    pub fn start(config: SessionConfig, endpoint: SessionEndpoint) -> Result<SessionHandle> {
        if config.role == SessionRole::Controller
            && config.peer_id.is_none()
            && matches!(endpoint, SessionEndpoint::ByRendezvous { .. })
        {
            return Err(NdError::Protocol(
                "le rôle contrôleur par rendez-vous nécessite un peer_id".to_owned(),
            ));
        }

        let (state_tx, state_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::sync_channel(PROFONDEUR_FILE_FRAMES);
        let (input_tx, input_rx) = mpsc::channel();
        let compteurs = Arc::new(CompteursSession::default());
        let stop = Arc::new(AtomicBool::new(false));

        let compteurs_pilote = Arc::clone(&compteurs);
        let stop_pilote = Arc::clone(&stop);
        let pilote = thread::Builder::new()
            .name("nd-session-pilote".to_owned())
            .spawn(move || {
                executer_pilote(
                    &config,
                    endpoint,
                    &state_tx,
                    frame_tx,
                    input_rx,
                    &compteurs_pilote,
                    &stop_pilote,
                );
            })?;

        Ok(SessionHandle {
            state_rx,
            frame_rx,
            input_tx,
            compteurs,
            stop,
            pilote: Some(pilote),
        })
    }
}

/// Corps du thread pilote : déroule la session, mémorise l'erreur éventuelle,
/// lève le signal d'arrêt pour les threads auxiliaires et pousse toujours `Closed`.
fn executer_pilote(
    config: &SessionConfig,
    endpoint: SessionEndpoint,
    state_tx: &Sender<SessionState>,
    frame_tx: SyncSender<DecodedFrame>,
    input_rx: Receiver<InputEvent>,
    compteurs: &Arc<CompteursSession>,
    stop: &Arc<AtomicBool>,
) {
    if let Err(erreur) = derouler_session(
        config, endpoint, state_tx, frame_tx, input_rx, compteurs, stop,
    ) {
        compteurs.note_erreur(&erreur);
    }
    stop.store(true, Ordering::Relaxed);
    let _ = state_tx.send(SessionState::Closed);
}

/// Enchaîne résolution → connexion → handshake → média, en poussant chaque
/// transition d'état au fil de la progression.
fn derouler_session(
    config: &SessionConfig,
    endpoint: SessionEndpoint,
    state_tx: &Sender<SessionState>,
    frame_tx: SyncSender<DecodedFrame>,
    input_rx: Receiver<InputEvent>,
    compteurs: &Arc<CompteursSession>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let _ = state_tx.send(SessionState::Resolving);
    let transport = etablir_transport(config, endpoint, state_tx, stop)?;

    let _ = state_tx.send(SessionState::Handshaking);
    // Identité Noise éphémère (une paire de clés par session) : le branchement de
    // l'identité persistante (`IdentityStore`) et l'épinglage TOFU viennent avec la
    // connectivité complète (plan 05/06).
    let cles = generate_static_keypair()?;
    let role_noise = match config.role {
        SessionRole::Controller => HandshakeRole::Initiator,
        SessionRole::Controlled => HandshakeRole::Responder,
    };
    let securise = establish(transport, role_noise, &cles.private)?;

    let _ = state_tx.send(SessionState::Active);
    let partage = TransportPartage::new(Box::new(securise), Arc::clone(compteurs));
    match config.role {
        SessionRole::Controller => {
            executer_controleur(partage, frame_tx, input_rx, compteurs, stop)
        }
        SessionRole::Controlled => executer_hote(&partage, compteurs, stop),
    }
}

/// Résout le point de contact et rend le transport QUIC brut (avant chiffrement),
/// en poussant `Connecting` au moment du dial/accept.
fn etablir_transport(
    config: &SessionConfig,
    endpoint: SessionEndpoint,
    state_tx: &Sender<SessionState>,
    stop: &Arc<AtomicBool>,
) -> Result<Box<dyn Transport>> {
    match endpoint {
        SessionEndpoint::Loopback { listener } => {
            let _ = state_tx.send(SessionState::Connecting);
            listener.accept()
        }
        SessionEndpoint::Direct { addr, cert_der } => {
            let _ = state_tx.send(SessionState::Connecting);
            connect(addr, &cert_der)
        }
        SessionEndpoint::ByRendezvous { server } => match config.role {
            SessionRole::Controller => {
                let pair = config.peer_id.ok_or_else(|| {
                    NdError::Protocol(
                        "le rôle contrôleur par rendez-vous nécessite un peer_id".to_owned(),
                    )
                })?;
                let fiche = resoudre_par_id(&RendezvousClient::new(server), pair, stop)?;
                let _ = state_tx.send(SessionState::Connecting);
                connect(fiche.addr, &fiche.cert_der)
            }
            SessionRole::Controlled => {
                // Repli simple loopback/local (la publication d'une adresse joignable
                // à travers un NAT — STUN/candidats — est le lot 05).
                let ecouteur = bind("127.0.0.1:0".parse().expect("adresse loopback valide"))?;
                RendezvousClient::new(server).register(
                    config.local_id,
                    ecouteur.local_addr(),
                    &ecouteur.server_cert_der(),
                )?;
                let _ = state_tx.send(SessionState::Connecting);
                ecouteur.accept()
            }
        },
    }
}

/// Résout un ID au rendez-vous avec tentatives espacées : le pair peut être en
/// train de se publier en parallèle.
fn resoudre_par_id(
    client: &RendezvousClient,
    id: NovaId,
    stop: &Arc<AtomicBool>,
) -> Result<PeerRecord> {
    for _ in 0..TENTATIVES_RESOLUTION {
        if stop.load(Ordering::Relaxed) {
            return Err(NdError::Protocol(
                "session arrêtée pendant la résolution".to_owned(),
            ));
        }
        if let Ok(fiche) = client.lookup(id) {
            return Ok(fiche);
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(NdError::Protocol(format!(
        "ID {id} jamais résolu au rendez-vous"
    )))
}

/// Boucle du rôle **contrôleur** : le pilote décode le flux vidéo entrant vers
/// `frame_tx` (via [`ViewerPipeline::run_streaming`]) pendant qu'un thread dédié
/// transmet les entrées de `input_rx` sur le canal `Input` chiffré.
fn executer_controleur(
    transport: TransportPartage,
    frame_tx: SyncSender<DecodedFrame>,
    input_rx: Receiver<InputEvent>,
    compteurs: &Arc<CompteursSession>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let decodeur = create_decoder(CodecKind::H264)?;

    // Thread d'envoi des entrées : seul émetteur de ce côté de la session.
    let mut transport_entrees = transport.clone();
    let stop_entrees = Arc::clone(stop);
    let entrees = thread::Builder::new()
        .name("nd-session-entrees".to_owned())
        .spawn(move || {
            let canal = transport_entrees.open_channel(ChannelKind::Input);
            while !stop_entrees.load(Ordering::Relaxed) {
                match input_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(evenement) => {
                        if transport_entrees
                            .send(canal, evenement.to_bytes(), Reliability::Reliable)
                            .is_err()
                        {
                            // Connexion fermée : le pilote terminera la session.
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;

    // Pilote : décodage en continu, frame la plus récente vers le consommateur.
    let mut viewer = ViewerPipeline::new(Box::new(transport), decodeur);
    let compteurs_frames = Arc::clone(compteurs);
    let resultat = viewer.run_streaming(
        move |frame| {
            compteurs_frames.frame_livree();
            // File pleine = consommateur en retard : la frame est sautée (try_send)
            // pour que le flux ne bloque jamais.
            let _ = frame_tx.try_send(frame);
        },
        Arc::clone(stop),
    );

    stop.store(true, Ordering::Relaxed);
    let _ = entrees.join();
    resultat.map(|_livrees| ())
}

/// Boucle du rôle **contrôlé** : le pilote diffuse l'écran (via
/// [`HostPipeline::run_streaming`]) pendant qu'un thread dédié reçoit les entrées
/// et les applique à l'OS ([`apply_input`]).
fn executer_hote(
    transport: &TransportPartage,
    compteurs: &Arc<CompteursSession>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let injecteur = create_injector()?;
    let capteur = create_capturer()?;
    let encodeur = create_encoder(CodecKind::H264)?;
    // Construit le pipeline avant de lancer le thread auxiliaire : tout échec
    // (capture indisponible…) est ainsi remonté sans laisser de thread derrière.
    let mut hote = HostPipeline::new(capteur, encodeur, Box::new(transport.clone()))?;

    // Thread de réception/application des entrées : seul récepteur de ce côté.
    let mut transport_entrees = transport.clone();
    let stop_entrees = Arc::clone(stop);
    let compteurs_entrees = Arc::clone(compteurs);
    let entrees = thread::Builder::new()
        .name("nd-session-injection".to_owned())
        .spawn(move || {
            while !stop_entrees.load(Ordering::Relaxed) {
                match transport_entrees.poll_recv() {
                    Ok(Some((_canal, donnees))) => {
                        if let Some(evenement) = InputEvent::from_bytes(&donnees) {
                            if apply_input(injecteur.as_ref(), &evenement).is_ok() {
                                compteurs_entrees
                                    .inputs_applied
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
            // Anti « stuck key » : tout relâcher en fin de session.
            injecteur.release_all();
        })?;

    let resultat = hote.run_streaming(Arc::clone(stop));
    stop.store(true, Ordering::Relaxed);
    let _ = entrees.join();
    resultat.map(|_envoyees| ())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
    use nd_codec::EncoderConfig;
    use nd_features::Permissions;
    use nd_proto::{MonitorId, NovaId};

    fn config(role: SessionRole, peer: Option<NovaId>) -> SessionConfig {
        SessionConfig {
            role,
            local_id: NovaId(101_010_101),
            peer_id: peer,
            permissions: Permissions::default(),
        }
    }

    /// Frame BGRA synthétique 64×64 dont le motif dépend de `seq` (deltas non vides),
    /// pour exercer encodeur/décodeur sans capture d'écran réelle.
    fn frame_synthetique(seq: usize) -> CapturedFrame {
        const COTE: u32 = 64;
        let mut data = vec![0u8; (COTE * COTE * 4) as usize];
        for (i, pixel) in data.chunks_exact_mut(4).enumerate() {
            pixel[0] = ((i + seq * 31) % 256) as u8;
            pixel[1] = ((i / 3 + seq * 7) % 256) as u8;
            pixel[2] = ((seq * 11) % 256) as u8;
            pixel[3] = 255;
        }
        CapturedFrame {
            width: COTE,
            height: COTE,
            monitor: MonitorId(0),
            format: PixelFormat::Bgra8,
            dirty: vec![],
            cursor: None,
            timestamp_us: (seq as u64) * 16_000,
            image: Some(FrameImage::Cpu {
                data,
                stride: (COTE * 4) as usize,
            }),
        }
    }

    #[test]
    fn fps_ne_compte_que_la_fenetre_glissante() {
        let maintenant = Instant::now();
        let mut fenetre: VecDeque<Instant> = VecDeque::new();
        fenetre.push_back(maintenant - Duration::from_millis(1_500));
        fenetre.push_back(maintenant - Duration::from_millis(600));
        fenetre.push_back(maintenant - Duration::from_millis(100));
        let fps = fps_fenetre_glissante(&mut fenetre, maintenant);
        assert!((fps - 2.0).abs() < f32::EPSILON, "fps = {fps}");
        assert_eq!(fenetre.len(), 2, "l'horodatage hors fenêtre est élagué");
    }

    #[test]
    fn transport_partage_compte_les_octets() {
        let ecouteur = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = ecouteur.local_addr();
        let cert = ecouteur.server_cert_der();
        let acceptation = thread::spawn(move || ecouteur.accept().expect("accept"));
        let brut_client = connect(addr, &cert).expect("connect");
        let brut_serveur = acceptation.join().expect("thread d'acceptation");

        let compteurs_client = Arc::new(CompteursSession::default());
        let compteurs_serveur = Arc::new(CompteursSession::default());
        let mut client = TransportPartage::new(brut_client, Arc::clone(&compteurs_client));
        let mut serveur = TransportPartage::new(brut_serveur, Arc::clone(&compteurs_serveur));

        let canal = client.open_channel(ChannelKind::Input);
        for _ in 0..3 {
            client
                .send(canal, vec![0xAB; 100], Reliability::Reliable)
                .expect("send");
        }
        assert_eq!(compteurs_client.instantane().bytes_out, 300);

        let echeance = Instant::now() + Duration::from_secs(5);
        while compteurs_serveur.instantane().bytes_in < 300 && Instant::now() < echeance {
            if serveur.poll_recv().expect("poll_recv").is_none() {
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(compteurs_serveur.instantane().bytes_in, 300);
    }

    #[test]
    fn run_streaming_repond_au_signal_d_arret() {
        let ecouteur = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = ecouteur.local_addr();
        let cert = ecouteur.server_cert_der();
        let acceptation = thread::spawn(move || ecouteur.accept().expect("accept"));
        let transport = connect(addr, &cert).expect("connect");
        let _hote = acceptation.join().expect("thread d'acceptation");

        let decodeur = create_decoder(CodecKind::H264).expect("décodeur");
        let mut viewer = ViewerPipeline::new(transport, decodeur);
        // Signal déjà levé : la boucle doit rendre la main immédiatement, sans frame.
        let stop = Arc::new(AtomicBool::new(true));
        let livrees = viewer.run_streaming(|_frame| {}, stop).expect("streaming");
        assert_eq!(livrees, 0);
    }

    #[test]
    fn run_streaming_livre_les_frames_les_plus_recentes() {
        let ecouteur = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = ecouteur.local_addr();
        let cert = ecouteur.server_cert_der();
        let stop = Arc::new(AtomicBool::new(false));

        // Garde-fou : un blocage imprévu lève le signal au lieu de geler le test.
        let stop_garde = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(15));
            stop_garde.store(true, Ordering::Relaxed);
        });

        // Hôte synthétique : encode et envoie des frames 64×64 jusqu'au signal.
        let stop_hote = Arc::clone(&stop);
        let hote = thread::spawn(move || -> Result<()> {
            let mut transport = ecouteur.accept()?;
            let canal = transport.open_channel(ChannelKind::Video(MonitorId(0)));
            let mut encodeur = create_encoder(CodecKind::H264)?;
            encodeur.configure(EncoderConfig {
                kind: CodecKind::H264,
                width: 64,
                height: 64,
                target_bitrate_kbps: 1_000,
                max_fps: 60,
            })?;
            let mut seq = 0usize;
            while !stop_hote.load(Ordering::Relaxed) {
                let chunk = encodeur.encode(&frame_synthetique(seq), seq.is_multiple_of(25))?;
                if transport
                    .send(canal, chunk.data, Reliability::UnreliableFec)
                    .is_err()
                {
                    break;
                }
                seq += 1;
                thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        });

        let transport = connect(addr, &cert).expect("connect");
        let decodeur = create_decoder(CodecKind::H264).expect("décodeur");
        let mut viewer = ViewerPipeline::new(transport, decodeur);

        let stop_callback = Arc::clone(&stop);
        let mut dims: Vec<(u32, u32)> = Vec::new();
        let livrees = viewer
            .run_streaming(
                |frame| {
                    dims.push((frame.width, frame.height));
                    if dims.len() >= 3 {
                        // Le consommateur décide de la fin : le signal coupe les deux côtés.
                        stop_callback.store(true, Ordering::Relaxed);
                    }
                },
                Arc::clone(&stop),
            )
            .expect("streaming");

        assert!(livrees >= 3, "frames livrées = {livrees}");
        assert!(
            dims.iter().all(|&d| d == (64, 64)),
            "dimensions inattendues : {dims:?}"
        );
        hote.join().expect("thread hôte").expect("hôte synthétique");
    }

    #[test]
    fn controleur_par_rendezvous_sans_peer_id_refuse() {
        let resultat = SessionEngine::start(
            config(SessionRole::Controller, None),
            SessionEndpoint::ByRendezvous {
                server: "127.0.0.1:9".parse().expect("adresse"),
            },
        );
        assert!(resultat.is_err(), "peer_id requis pour résoudre par ID");
    }

    /// Tranche complète du rôle contrôleur, sans capture ni injection : un hôte
    /// « manuel » (accept → Noise répondeur → frames synthétiques → compte les
    /// entrées) face au moteur. Vérifie les transitions d'état, l'arrivée des
    /// frames dans `frame_rx`, les statistiques et l'aller des entrées.
    #[test]
    fn moteur_controleur_flux_entrees_et_etats() {
        let ecouteur = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = ecouteur.local_addr();
        let cert = ecouteur.server_cert_der();

        let hote = thread::spawn(move || -> Result<usize> {
            let brut = ecouteur.accept()?;
            let cles = generate_static_keypair()?;
            let mut chiffre = establish(brut, HandshakeRole::Responder, &cles.private)?;
            let canal = chiffre.open_channel(ChannelKind::Video(MonitorId(0)));
            let mut encodeur = create_encoder(CodecKind::H264)?;
            encodeur.configure(EncoderConfig {
                kind: CodecKind::H264,
                width: 64,
                height: 64,
                target_bitrate_kbps: 1_000,
                max_fps: 60,
            })?;
            let mut entrees = 0usize;
            let mut seq = 0usize;
            let echeance = Instant::now() + Duration::from_secs(15);
            while (seq < 60 || entrees < 3) && Instant::now() < echeance {
                if seq < 60 {
                    let chunk = encodeur.encode(&frame_synthetique(seq), seq.is_multiple_of(25))?;
                    if chiffre
                        .send(canal, chunk.data, Reliability::UnreliableFec)
                        .is_err()
                    {
                        break;
                    }
                    seq += 1;
                }
                while let Some((_canal, donnees)) = chiffre.poll_recv()? {
                    if InputEvent::from_bytes(&donnees).is_some() {
                        entrees += 1;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            Ok(entrees)
        });

        let poignee = SessionEngine::start(
            config(SessionRole::Controller, Some(NovaId(202_020_202))),
            SessionEndpoint::Direct {
                addr,
                cert_der: cert,
            },
        )
        .expect("start");

        // 1. Transitions d'état dans l'ordre attendu, jusqu'à Active.
        let mut etats = Vec::new();
        let echeance = Instant::now() + Duration::from_secs(10);
        while etats.last() != Some(&SessionState::Active) && Instant::now() < echeance {
            if let Ok(etat) = poignee.state_rx.recv_timeout(Duration::from_millis(100)) {
                assert_ne!(
                    etat,
                    SessionState::Closed,
                    "session close prématurément : {:?}",
                    poignee.last_error()
                );
                etats.push(etat);
            }
        }
        assert_eq!(
            etats,
            vec![
                SessionState::Resolving,
                SessionState::Connecting,
                SessionState::Handshaking,
                SessionState::Active
            ]
        );

        // 2. Des frames décodées arrivent dans frame_rx.
        let mut frames = 0usize;
        let echeance = Instant::now() + Duration::from_secs(10);
        while frames < 5 && Instant::now() < echeance {
            if let Ok(frame) = poignee.frame_rx.recv_timeout(Duration::from_millis(200)) {
                assert_eq!((frame.width, frame.height), (64, 64));
                assert_eq!(frame.rgba.len(), 64 * 64 * 4);
                frames += 1;
            }
        }
        assert!(
            frames >= 5,
            "frames reçues = {frames} (erreur moteur : {:?})",
            poignee.last_error()
        );

        // 3. Statistiques mises à jour en continu (prises juste après une frame).
        let stats = poignee.stats();
        assert!(stats.fps > 0.0, "stats = {stats:?}");
        assert!(stats.frames_decoded >= 5, "stats = {stats:?}");
        assert!(stats.bytes_in > 0, "stats = {stats:?}");

        // 4. Les entrées postées dans input_tx traversent jusqu'à l'hôte.
        for _ in 0..3 {
            poignee
                .input_tx
                .send(InputEvent::MouseMoveRel { dx: 1.0, dy: 0.0 })
                .expect("input_tx");
        }
        let entrees = hote.join().expect("thread hôte").expect("hôte manuel");
        assert!(entrees >= 3, "entrées reçues côté hôte = {entrees}");
        let stats = poignee.stats();
        assert!(stats.bytes_out > 0, "stats = {stats:?}");

        poignee.stop();
    }
}
