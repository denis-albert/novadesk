//! Orchestrateur de session réutilisable : [`SessionEngine`] câble les briques réelles
//! (résolution par ID → QUIC → chiffrement Noise → capture/codec/entrées) sur des
//! **threads dédiés**, pilote les transitions [`SessionState`] et expose les sorties
//! à un consommateur (future UI, voir plan 10) : frames décodées, statistiques
//! continues, canal d'entrées à transmettre.
//!
//! Architecture (pas de runtime async : threads `std` + `std::sync::mpsc`) :
//!
//! ```text
//! SessionEngine::start(config, endpoint) ──► thread « nd-session-pilote »
//!   états poussés dans state_rx : Resolving → Connecting → Handshaking → Active
//!                                 (→ Reconnecting → Handshaking → Active)* → Closed
//!
//!   une **époque** = une connexion vécue de bout en bout :
//!     transport QUIC concret ──► on_disconnect ──► drapeau « lien coupé »
//!     thread « nd-session-garde » : stop global OU lien coupé → arrêt de l'époque
//!     handshake Noise, puis selon le rôle :
//!
//!   contrôleur : ViewerPipeline::run_streaming (pilote) ──► frame_rx (+ fps)
//!     └─► thread « nd-session-entrees » : input_rx → canal Input chiffré
//!
//!   contrôlé   : HostPipeline::run_streaming_pilote (pilote) :
//!                capture → encodeur GPU (repli logiciel) → ABR (~1 Hz) →
//!                enregistrement MP4 (opt-in) → QUIC chiffré
//!     └─► thread « nd-session-injection » : poll_recv → permissions → apply_input
//! ```
//!
//! **Reconnexion** ([`SessionEndpoint::ByRendezvous`] uniquement) : à la coupure du
//! lien, l'état passe à `Reconnecting` et un [`ReconnectController`] cadence les
//! tentatives — `establish_p2p` côté contrôleur, nouvelle attente `await_p2p`
//! (filtrée sur le **même pair**) côté contrôlé. Au succès, une nouvelle époque
//! démarre (nouveau handshake Noise, image-clé de resynchronisation) ; à
//! l'épuisement de la politique (`has_given_up`), la session se clôt. Les points
//! de contact `Loopback`/`Direct` restent mono-époque (pas de rendez-vous pour
//! retrouver le pair).
//!
//! Les threads d'un même rôle partagent le transport chiffré via
//! `TransportPartage` (verrou + comptage octets/RTT) : chaque côté n'a qu'un seul
//! thread émetteur et un seul thread récepteur, ce qui préserve l'ordre des nonces
//! Noise dans chaque direction. L'identité Noise est pour l'instant **éphémère**
//! (une paire de clés par session) ; l'identité persistante (`IdentityStore`) et
//! l'épinglage TOFU arrivent au lot 06.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nd_audio::AudioSession;
use nd_capture::create_capturer;
use nd_codec::{
    create_decoder, create_encoder, create_hardware_encoder, CodecKind, ContentProfile,
    DecodedFrame, EncodedChunk, VideoDecoder, VideoEncoder,
};
use nd_crypto::{generate_static_keypair, HandshakeRole};
use nd_features::{
    Capability, PermissionBroker, PermissionSet, ReconnectController, ReconnectPolicy,
};
use nd_files::{ClipboardSync, TransferEvent, TransferSession};
use nd_input::{create_injector, InputInjector};
use nd_proto::{ChannelKind, InputEvent, MonitorId, NdError, NovaId, Reliability, Result};
use nd_signaling::RendezvousClient;
use nd_transport::{
    connect_quic, Backoff, ChannelHandle, Listener, PathEstimate, QuicTransport,
    ReconnectingTransport, ServerIdentity, Transport,
};

use crate::media::{
    decoder_audio, decoder_controle, encoder_audio, encoder_controle, Categorie, ChatMessage,
    CommandeMedia, SousTypeControle, MONITEURS_MAX,
};
use crate::p2p::{self, AttenteRendezvous};
use crate::{
    apply_input, establish, HostPipeline, HostStreamOptions, HostStreamTick, SessionConfig,
    SessionRole, SessionState, ViewerPipeline,
};

/// Largeur de la fenêtre glissante de mesure du débit d'images (fps).
const FENETRE_FPS: Duration = Duration::from_secs(1);

/// Période d'échantillonnage du RTT depuis [`Transport::path_estimate`].
const PERIODE_RTT: Duration = Duration::from_millis(100);

/// Période d'échantillonnage du chemin réseau pour l'ABR côté hôte (~1 Hz).
const PERIODE_ABR: Duration = Duration::from_secs(1);

/// Profondeur de la file de frames livrées au consommateur : au-delà, les frames
/// excédentaires sont sautées (un consommateur lent ne bloque jamais le décodage).
const PROFONDEUR_FILE_FRAMES: usize = 4;

/// Délai maximal accordé aux threads du moteur pour se terminer dans
/// [`SessionHandle::stop`].
const DELAI_ARRET: Duration = Duration::from_secs(5);

/// Fenêtre du premier établissement par rendez-vous (résolution + punch) : le
/// pair peut être en train de s'enregistrer ou de se mettre en attente.
const DELAI_ETABLISSEMENT: Duration = Duration::from_secs(20);

/// Fenêtre d'une tentative de reconnexion (le [`ReconnectController`] cadence
/// les tentatives entre elles).
const DELAI_TENTATIVE_RECONNEXION: Duration = Duration::from_secs(8);

/// Période de scrutation du pont d'arrêt d'époque (« nd-session-garde »).
const PERIODE_GARDE: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// Point de contact réseau
// ---------------------------------------------------------------------------

/// Point de contact réseau : décrit **comment joindre le pair**.
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
    /// Connexion **par ID** via un serveur de rendez-vous (`nd-signaling`) :
    /// STUN → hole punching UDP → QUIC sur la socket percée, avec repli relais
    /// optionnel. C'est le chemin nominal du plan 05 ; c'est aussi le seul point
    /// de contact **reconnectable** (le rendez-vous permet de retrouver le pair).
    ByRendezvous {
        /// Adresse du serveur de rendez-vous.
        server: SocketAddr,
        /// Serveurs STUN interrogés pour le candidat réflexif. Vide = candidats
        /// locaux seulement (suffisant en LAN/boucle locale, sans espoir à
        /// travers un NAT).
        stun_servers: Vec<SocketAddr>,
        /// Relais de repli (`nd-relay`) quand le punch échoue ; `None` = pas de
        /// repli (l'échec du punch fait échouer l'établissement).
        relay: Option<SocketAddr>,
    },
}

// ---------------------------------------------------------------------------
// Options du moteur
// ---------------------------------------------------------------------------

/// Options additionnelles du moteur — **additif** : [`SessionEngine::start`]
/// applique les défauts, [`SessionEngine::start_with_options`] les expose.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Permissions granulaires appliquées côté **contrôlé** avant chaque
    /// injection d'entrée. `None` = dérivées des [`SessionConfig::permissions`]
    /// historiques (conversion conservatrice de `nd-features`).
    pub permissions: Option<PermissionSet>,
    /// Enregistrement local de la session côté **hôte** (opt-in) : chemin du
    /// MP4 à écrire. En cas de reconnexion, chaque époque ouvre son propre
    /// fichier (`session.mp4`, `session-2.mp4`, …) — les horodatages de capture
    /// repartent de zéro à chaque époque.
    pub recording: Option<PathBuf>,
    /// Profil de contenu de l'échelle ABR (axe de dégradation, voir plan 03).
    pub abr_profile: ContentProfile,
    /// Encodage delta (voir [`HostStreamOptions::delta_mode`]). `nd-capture`
    /// renseigne désormais fidèlement `CapturedFrame::dirty` (vide ⇔ rien
    /// changé) : le delta est donc **actif par défaut** (gain mesuré dans
    /// `examples/session_media_demo.rs`).
    pub delta_mode: bool,
    /// Politique de reconnexion automatique ([`SessionEndpoint::ByRendezvous`]).
    pub reconnect: ReconnectPolicy,
    /// Câble les **canaux annexes** dans la boucle de session — audio (canal
    /// `Audio`), transfert de fichiers (canal `Files`), presse-papiers + chat +
    /// bascule moniteur (canal `Control` multiplexé) — chacun gardé par sa
    /// [`Capability`]. Défaut `false` : session **vidéo + entrées** historique
    /// (comportement strictement inchangé). Voir [`crate::media`] pour
    /// l'arbitrage fiable/non-fiable imposé par le nonce Noise.
    pub extended_features: bool,
    /// Répertoire de réception des fichiers transférés (canal `Files`). `None` =
    /// dossier temporaire du système. Ignoré hors mode étendu.
    pub transfer_dir: Option<PathBuf>,
    /// **Reconnexion transparente au niveau transport** ([`ReconnectingTransport`])
    /// pour le point de contact [`SessionEndpoint::Direct`] côté contrôleur :
    /// enveloppe le transport de session d'une fabrique qui, à la coupure,
    /// re-connecte **et re-négocie Noise** vers la même adresse/certificat
    /// (l'hôte doit ré-accepter). Défaut `false`. Pour
    /// [`SessionEndpoint::ByRendezvous`], la reconnexion transparente est portée
    /// par la boucle d'époques (re-punch + re-négociation via le rendez-vous),
    /// mécanisme plus riche et déjà actif — ce drapeau ne s'y applique pas.
    pub transport_reconnect: bool,
}

impl Default for SessionOptions {
    /// Permissions dérivées de la configuration, pas d'enregistrement, ABR en
    /// profil bureautique, delta **actif**, reconnexion par défaut, canaux
    /// annexes **coupés** (session vidéo + entrées historique).
    fn default() -> Self {
        SessionOptions {
            permissions: None,
            recording: None,
            abr_profile: ContentProfile::Text,
            delta_mode: true,
            reconnect: ReconnectPolicy::default(),
            extended_features: false,
            transfer_dir: None,
            transport_reconnect: false,
        }
    }
}

/// Briques média **injectées** dans une session (tests, sondes, ou back-end
/// personnalisé), en marge des [`SessionOptions`] sérialisables.
///
/// Ces objets ne sont ni `Clone` ni `Debug` (ils portent des périphériques et
/// des tampons) : ils sont donc passés à part, via
/// [`SessionEngine::start_with_media`]. Un champ à `None` est **construit à la
/// volée** depuis les fabriques système si la capacité correspondante est
/// accordée (audio duplex système, presse-papiers de la plateforme).
#[derive(Default)]
pub struct SessionMedia {
    /// Session audio à utiliser (émission côté hôte, lecture côté contrôleur).
    /// `None` + [`Capability::Audio`] ⇒ [`AudioSession::duplex_systeme`].
    pub audio: Option<AudioSession>,
    /// Synchro presse-papiers à utiliser. `None` + capacité presse-papiers ⇒
    /// [`ClipboardSync::new`] (presse-papiers réel de la plateforme).
    pub clipboard: Option<ClipboardSync>,
}

/// Permissions effectives du poste contrôlé : celles des options si fournies,
/// sinon la conversion conservatrice des permissions historiques de la
/// configuration.
fn resoudre_permissions(config: &SessionConfig, options: &SessionOptions) -> PermissionSet {
    options
        .permissions
        .unwrap_or_else(|| PermissionSet::from(config.permissions))
}

/// Chemin du fichier d'enregistrement d'une époque : la première écrit au
/// chemin demandé, les reprises suffixent le nom (`session-2.mp4`, …) — un
/// muxeur MP4 clos ne se rouvre pas.
fn chemin_enregistrement(base: &Path, epoque: u32) -> PathBuf {
    if epoque <= 1 {
        return base.to_path_buf();
    }
    let racine = base.file_stem().map_or_else(
        || "session".to_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    let nom = match base.extension() {
        Some(ext) => format!("{racine}-{epoque}.{}", ext.to_string_lossy()),
        None => format!("{racine}-{epoque}"),
    };
    base.with_file_name(nom)
}

/// Options du flux hôte piloté pour une époque donnée.
///
/// En **mode étendu**, la vidéo passe en émission **fiable** (la direction
/// `hôte → contrôleur` porte alors aussi le plan de contrôle fiable : un seul
/// domaine d'ordonnancement, voir [`crate::media`]) et la bascule moniteur est
/// branchée sur `moniteur`. Hors mode étendu : datagrammes+FEC, pas de bascule.
fn options_flux_hote(
    options: &SessionOptions,
    epoque: u32,
    moniteur: Arc<AtomicU32>,
) -> HostStreamOptions {
    let etendu = options.extended_features;
    HostStreamOptions {
        abr_profile: Some(options.abr_profile),
        abr_period: PERIODE_ABR,
        delta_mode: options.delta_mode,
        recording: options
            .recording
            .as_deref()
            .map(|base| chemin_enregistrement(base, epoque)),
        video_reliability: if etendu {
            Reliability::Reliable
        } else {
            Reliability::UnreliableFec
        },
        monitor_switch: etendu.then_some(moniteur),
    }
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
    /// Entrées reçues mais **refusées par les permissions** (contrôlé) : jetées
    /// silencieusement avant injection, voir plan 13.
    pub inputs_denied: u64,
    /// Débit cible actuellement appliqué à l'encodeur par l'ABR (hôte), kbit/s.
    /// `0` tant que l'encodeur n'est pas configuré.
    pub target_bitrate_kbps: u32,
    /// Palier ABR courant (hôte) : 0 = plein régime, croît en dégradant.
    pub abr_level: u32,
    /// Images écrites dans l'enregistrement local (hôte), toutes époques
    /// confondues. `0` si l'enregistrement n'est pas activé.
    pub frames_recorded: u64,
    /// Reconnexions **réussies** depuis le début de la session.
    pub reconnects: u32,
}

/// Compteurs partagés entre les threads du moteur, mis à jour en continu.
#[derive(Default)]
pub(crate) struct CompteursSession {
    rtt_us: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    frames_decoded: AtomicU64,
    inputs_applied: AtomicU64,
    inputs_denied: AtomicU64,
    debit_cible_kbps: AtomicU64,
    palier_abr: AtomicU64,
    frames_enregistrees: AtomicU64,
    reconnexions: AtomicU64,
    /// Horodatages des frames livrées (fenêtre glissante pour le fps).
    fenetre_fps: Mutex<VecDeque<Instant>>,
    /// Dernière erreur d'exécution du pilote (session close en erreur).
    derniere_erreur: Mutex<Option<String>>,
    /// Nom du backend d'encodage réellement à l'œuvre (hôte) — la preuve
    /// NVENC/repli, voir `nd-codec::create_hardware_encoder`.
    backend_encodeur: Mutex<Option<String>>,
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
    pub(crate) fn note_erreur(&self, erreur: &NdError) {
        *self
            .derniere_erreur
            .lock()
            .expect("verrou de la dernière erreur") = Some(erreur.to_string());
    }

    /// Mémorise le backend d'encodage à l'œuvre (observabilité).
    fn note_backend(&self, backend: &str) {
        *self
            .backend_encodeur
            .lock()
            .expect("verrou du backend d'encodage") = Some(backend.to_owned());
    }

    /// Instantané cohérent des statistiques.
    pub(crate) fn instantane(&self) -> SessionStats {
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
            inputs_denied: self.inputs_denied.load(Ordering::Relaxed),
            target_bitrate_kbps: u32::try_from(self.debit_cible_kbps.load(Ordering::Relaxed))
                .unwrap_or(u32::MAX),
            abr_level: u32::try_from(self.palier_abr.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
            frames_recorded: self.frames_enregistrees.load(Ordering::Relaxed),
            reconnects: u32::try_from(self.reconnexions.load(Ordering::Relaxed))
                .unwrap_or(u32::MAX),
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

    fn is_connected(&self) -> bool {
        self.interne
            .lock()
            .expect("verrou du transport partagé")
            .is_connected()
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
    /// Messages de chat **à émettre** vers le pair (canal `Control` multiplexé).
    /// Sans effet hors mode étendu ([`SessionOptions::extended_features`]).
    pub chat_tx: Sender<String>,
    /// Messages de chat **reçus** du pair ([`ChatMessage::from_remote`] vrai) et
    /// échos locaux des messages émis (faux).
    pub chat_rx: Receiver<ChatMessage>,
    /// Flux de **progression des transferts de fichiers** (canal `Files`) :
    /// démarrage, progression, fin par fichier, et fin de file.
    pub transfer_rx: Receiver<TransferEvent>,
    /// Commandes vers les threads média (envoi de fichiers, bascule moniteur,
    /// activation audio) — voir [`SessionHandle::send_files`],
    /// [`SessionHandle::switch_monitor`], [`SessionHandle::set_audio_enabled`].
    commandes_tx: Sender<CommandeMedia>,
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

    /// Nom du backend d'encodage réellement à l'œuvre côté hôte (ex. le nom
    /// exact du MFT NVENC, ou le repli logiciel) ; `None` tant que l'encodeur
    /// n'est pas créé, ou côté contrôleur.
    #[must_use]
    pub fn encoder_backend(&self) -> Option<String> {
        self.compteurs
            .backend_encodeur
            .lock()
            .expect("verrou du backend d'encodage")
            .clone()
    }

    /// Envoie un message de chat au pair (canal `Control` multiplexé). Raccourci
    /// autour de [`SessionHandle::chat_tx`] ; sans effet hors mode étendu.
    pub fn send_chat(&self, texte: impl Into<String>) {
        let _ = self.chat_tx.send(texte.into());
    }

    /// Démarre l'**envoi** d'une file de fichiers vers le pair (canal `Files`).
    /// La progression est observable sur [`SessionHandle::transfer_rx`]. Gardé
    /// par [`Capability::FileUpload`] côté émetteur ; sans effet hors mode
    /// étendu.
    pub fn send_files(&self, fichiers: Vec<PathBuf>) {
        let _ = self
            .commandes_tx
            .send(CommandeMedia::EnvoyerFichiers(fichiers));
    }

    /// Demande à l'hôte de diffuser le **moniteur** d'index donné (bascule
    /// multi-écran, plan 13). L'hôte applique au mieux (voir
    /// [`crate::HostPipeline`]) ; sans effet hors mode étendu.
    pub fn switch_monitor(&self, moniteur: u32) {
        let _ = self
            .commandes_tx
            .send(CommandeMedia::BasculerMoniteur(moniteur));
    }

    /// Active ou désactive l'audio (émission côté hôte, lecture côté contrôleur).
    /// Sans effet hors mode étendu ou si [`Capability::Audio`] n'est pas accordé.
    pub fn set_audio_enabled(&self, actif: bool) {
        let _ = self.commandes_tx.send(CommandeMedia::AudioActif(actif));
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

/// Moteur de session : câble les briques réelles (résolution par ID, transport
/// QUIC, chiffrement Noise, pipelines média, permissions, entrées, reconnexion)
/// sur des threads dédiés et pilote la machine à états.
///
/// Façade sans état : tout le vivant appartient aux threads et à la
/// [`SessionHandle`] rendue par [`SessionEngine::start`].
pub struct SessionEngine;

impl SessionEngine {
    /// Démarre une session avec les options par défaut ([`SessionOptions`]) et
    /// rend immédiatement la main. Voir [`SessionEngine::start_with_options`].
    ///
    /// # Errors
    /// Voir [`SessionEngine::start_with_options`].
    pub fn start(config: SessionConfig, endpoint: SessionEndpoint) -> Result<SessionHandle> {
        Self::start_with_options(config, endpoint, SessionOptions::default())
    }

    /// Démarre une session et rend immédiatement la main.
    ///
    /// Lance le thread pilote qui déroule `Resolving → Connecting → Handshaking →
    /// Active` (transitions poussées dans [`SessionHandle::state_rx`]) puis fait
    /// tourner le média selon le rôle : le **contrôleur** décode le flux vidéo vers
    /// [`SessionHandle::frame_rx`] et transmet les [`InputEvent`] postés dans
    /// [`SessionHandle::input_tx`] ; le **contrôlé** diffuse son écran (encodeur
    /// matériel avec repli logiciel, débit régulé par l'ABR, enregistrement MP4
    /// opt-in) et applique les entrées reçues **après le filtre de permissions**.
    /// Sur coupure de lien d'une session [`SessionEndpoint::ByRendezvous`], l'état
    /// passe à `Reconnecting` et la session se rétablit selon
    /// [`SessionOptions::reconnect`]. `Closed` est toujours poussé en fin de vie.
    ///
    /// # Errors
    /// Erreur immédiate si la configuration est invalide (contrôleur par rendez-vous
    /// sans `peer_id`) ou si le thread pilote ne peut pas être créé. Les erreurs
    /// d'exécution ultérieures closent la session (`Closed`) et sont consultables
    /// via [`SessionHandle::last_error`].
    pub fn start_with_options(
        config: SessionConfig,
        endpoint: SessionEndpoint,
        options: SessionOptions,
    ) -> Result<SessionHandle> {
        Self::start_with_media(config, endpoint, options, SessionMedia::default())
    }

    /// Démarre une session en **injectant** des briques média
    /// ([`SessionMedia`] : session audio, presse-papiers) — utile aux sondes et
    /// aux back-ends personnalisés. Équivalent additif de
    /// [`SessionEngine::start_with_options`], qui délègue ici avec
    /// [`SessionMedia::default`] (briques construites à la volée si les capacités
    /// sont accordées).
    ///
    /// # Errors
    /// Voir [`SessionEngine::start_with_options`].
    pub fn start_with_media(
        config: SessionConfig,
        endpoint: SessionEndpoint,
        options: SessionOptions,
        injectees: SessionMedia,
    ) -> Result<SessionHandle> {
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
        // Canaux vers/depuis la poignée pour les fonctions étendues.
        let (chat_in_tx, chat_in_rx) = mpsc::channel();
        let (chat_out_tx, chat_out_rx) = mpsc::channel();
        let (transfer_out_tx, transfer_out_rx) = mpsc::channel();
        let (commandes_tx, commandes_rx) = mpsc::channel();
        let compteurs = Arc::new(CompteursSession::default());
        let stop = Arc::new(AtomicBool::new(false));

        let media = Arc::new(EtatMedia {
            role: config.role,
            permissions: resoudre_permissions(&config, &options),
            transfer_dir: options
                .transfer_dir
                .clone()
                .unwrap_or_else(std::env::temp_dir),
            audio: Mutex::new(injectees.audio),
            clipboard: Mutex::new(injectees.clipboard),
            transfer: Mutex::new(None),
            audio_actif: AtomicBool::new(true),
            moniteur: Arc::new(AtomicU32::new(0)),
            dernier_presse_papiers: Mutex::new(None),
            chat_out: chat_out_tx,
            transfer_out: transfer_out_tx,
            commandes: Mutex::new(commandes_rx),
            chat_in: Mutex::new(chat_in_rx),
        });

        let ctx = ContextePilote {
            config,
            options,
            state_tx,
            frame_tx,
            input_rx: Arc::new(Mutex::new(input_rx)),
            compteurs: Arc::clone(&compteurs),
            stop: Arc::clone(&stop),
            media,
        };
        let pilote = thread::Builder::new()
            .name("nd-session-pilote".to_owned())
            .spawn(move || executer_pilote(&ctx, endpoint))?;

        Ok(SessionHandle {
            state_rx,
            frame_rx,
            input_tx,
            chat_tx: chat_in_tx,
            chat_rx: chat_out_rx,
            transfer_rx: transfer_out_rx,
            commandes_tx,
            compteurs,
            stop,
            pilote: Some(pilote),
        })
    }
}

// ---------------------------------------------------------------------------
// Pilote
// ---------------------------------------------------------------------------

/// File d'entrées locales partagée entre les époques du contrôleur : le
/// `Receiver` n'est pas clonable, chaque époque le verrouille à son tour.
type EntreesPartagees = Arc<Mutex<Receiver<InputEvent>>>;

/// État des **fonctions étendues** partagé par les threads d'une session (et
/// persistant entre les époques d'une reconnexion) : briques média, canaux
/// vers/depuis la [`SessionHandle`], et bascules d'activation.
///
/// Toute la mutabilité est intérieure (`Mutex`/atomiques) : une seule époque
/// vit à la fois, la contention est donc nulle en pratique.
struct EtatMedia {
    /// Rôle local (sens de l'audio et de la vidéo).
    role: SessionRole,
    /// Capacités effectives : gate de chaque fonction étendue.
    permissions: PermissionSet,
    /// Répertoire de réception des fichiers (canal `Files`).
    transfer_dir: PathBuf,
    /// Session audio (émission hôte / lecture contrôleur), injectée ou système.
    audio: Mutex<Option<AudioSession>>,
    /// Synchro presse-papiers (injectée ou plateforme).
    clipboard: Mutex<Option<ClipboardSync>>,
    /// Transfert de fichiers actif (émetteur **ou** récepteur), partagé entre le
    /// thread récepteur (`handle_incoming`) et l'émetteur (`poll_outgoing`).
    transfer: Mutex<Option<TransferSession>>,
    /// Émission (hôte) / lecture (contrôleur) audio active.
    audio_actif: AtomicBool,
    /// Index du moniteur demandé (bascule multi-écran), lu par la capture hôte.
    moniteur: Arc<AtomicU32>,
    /// Dernier presse-papiers **appliqué** (anti-boucle : ne pas ré-émettre ce
    /// que l'on vient de recevoir).
    dernier_presse_papiers: Mutex<Option<Vec<u8>>>,
    /// Chat reçu + échos → [`SessionHandle::chat_rx`].
    chat_out: Sender<ChatMessage>,
    /// Progression des transferts → [`SessionHandle::transfer_rx`].
    transfer_out: Sender<TransferEvent>,
    /// Commandes depuis la poignée ([`SessionHandle::send_files`], …).
    commandes: Mutex<Receiver<CommandeMedia>>,
    /// Chat à émettre depuis la poignée ([`SessionHandle::chat_tx`]).
    chat_in: Mutex<Receiver<String>>,
}

/// Contexte vivant du pilote de session, partagé par toutes les époques.
struct ContextePilote {
    config: SessionConfig,
    options: SessionOptions,
    state_tx: Sender<SessionState>,
    frame_tx: SyncSender<DecodedFrame>,
    input_rx: EntreesPartagees,
    compteurs: Arc<CompteursSession>,
    stop: Arc<AtomicBool>,
    /// État des fonctions étendues (audio, fichiers, presse-papiers, chat,
    /// moniteur). Inerte hors [`SessionOptions::extended_features`].
    media: Arc<EtatMedia>,
}

/// Corps du thread pilote : déroule la session, mémorise l'erreur éventuelle,
/// lève le signal d'arrêt pour les threads auxiliaires et pousse toujours `Closed`.
fn executer_pilote(ctx: &ContextePilote, endpoint: SessionEndpoint) {
    if let Err(erreur) = derouler_session(ctx, endpoint) {
        ctx.compteurs.note_erreur(&erreur);
    }
    ctx.stop.store(true, Ordering::Relaxed);
    let _ = ctx.state_tx.send(SessionState::Closed);
}

/// Enchaîne résolution → connexion → (époques média, reconnexions) selon le
/// point de contact, en poussant chaque transition d'état au fil de l'eau.
fn derouler_session(ctx: &ContextePilote, endpoint: SessionEndpoint) -> Result<()> {
    let _ = ctx.state_tx.send(SessionState::Resolving);
    match endpoint {
        // Points de contact mono-époque : pas de rendez-vous pour retrouver le
        // pair, la perte du lien clôt la session (comportement historique).
        SessionEndpoint::Loopback { listener } => {
            let _ = ctx.state_tx.send(SessionState::Connecting);
            let transport = listener.accept_quic()?;
            vivre_epoque(ctx, transport, 1).map(|_fin| ())
        }
        SessionEndpoint::Direct { addr, cert_der } => {
            // Reconnexion transparente **au niveau transport** (opt-in, contrôleur) :
            // le transport de session s'auto-rétablit vers la même adresse.
            if ctx.config.role == SessionRole::Controller && ctx.options.transport_reconnect {
                return vivre_direct_reconnectant(ctx, addr, cert_der);
            }
            let _ = ctx.state_tx.send(SessionState::Connecting);
            let transport = connect_quic(addr, &cert_der)?;
            vivre_epoque(ctx, transport, 1).map(|_fin| ())
        }
        SessionEndpoint::ByRendezvous {
            server,
            stun_servers,
            relay,
        } => match ctx.config.role {
            SessionRole::Controller => {
                derouler_controleur_rendezvous(ctx, server, &stun_servers, relay)
            }
            SessionRole::Controlled => derouler_hote_rendezvous(ctx, server, &stun_servers, relay),
        },
    }
}

/// Rôle **contrôleur** par rendez-vous : établissement P2P initial puis boucle
/// d'époques avec reconnexion automatique sur coupure de lien.
fn derouler_controleur_rendezvous(
    ctx: &ContextePilote,
    server: SocketAddr,
    stun_servers: &[SocketAddr],
    relay: Option<SocketAddr>,
) -> Result<()> {
    let pair = ctx.config.peer_id.ok_or_else(|| {
        NdError::Protocol("le rôle contrôleur par rendez-vous nécessite un peer_id".to_owned())
    })?;
    let rv = RendezvousClient::new(server);

    let _ = ctx.state_tx.send(SessionState::Connecting);
    let mut transport = match p2p::connecter_par_rendezvous(
        &rv,
        ctx.config.local_id,
        pair,
        stun_servers,
        relay,
        DELAI_ETABLISSEMENT,
        &ctx.stop,
    ) {
        Ok(transport) => transport,
        // Arrêt demandé pendant l'établissement : fin propre, pas une erreur.
        Err(_) if ctx.stop.load(Ordering::Relaxed) => return Ok(()),
        Err(e) => return Err(e),
    };

    let mut epoque = 1u32;
    loop {
        if matches!(vivre_epoque(ctx, transport, epoque)?, FinEpoque::Arret) {
            return Ok(());
        }
        // Lien perdu : reconnexion cadencée par la politique de backoff.
        let Some(nouveau) = se_reconnecter(ctx, || {
            p2p::connecter_par_rendezvous(
                &rv,
                ctx.config.local_id,
                pair,
                stun_servers,
                relay,
                DELAI_TENTATIVE_RECONNEXION,
                &ctx.stop,
            )
        }) else {
            return Ok(());
        };
        transport = nouveau;
        epoque += 1;
    }
}

/// Rôle **contrôlé** par rendez-vous : publication de l'ID (identité TLS dont le
/// certificat est épinglé par l'appelant), attente d'une connexion entrante,
/// puis boucle d'époques — la reconnexion n'accepte que le **même pair**.
fn derouler_hote_rendezvous(
    ctx: &ContextePilote,
    server: SocketAddr,
    stun_servers: &[SocketAddr],
    relay: Option<SocketAddr>,
) -> Result<()> {
    let rv = RendezvousClient::new(server);
    // Identité TLS de la session : le certificat publié au rendez-vous doit être
    // celui présenté sur la socket percée et le repli relais (épinglage).
    let identite = ServerIdentity::generate()?;

    let _ = ctx.state_tx.send(SessionState::Connecting);
    // Admission initiale : le pair de la configuration s'il est imposé, sinon
    // le premier appelant venu.
    let attendu = ctx.config.peer_id;
    let admission = move |venu: NovaId| attendu.is_none_or(|impose| impose == venu);
    let attente = AttenteRendezvous {
        rv: &rv,
        local_id: ctx.config.local_id,
        identite: &identite,
        stun_servers,
        relay,
        admission: &admission,
    };
    let (mut transport, mut pair) = match p2p::accepter_par_rendezvous(&attente, None, &ctx.stop) {
        Ok(entrant) => entrant,
        Err(_) if ctx.stop.load(Ordering::Relaxed) => return Ok(()),
        Err(e) => return Err(e),
    };

    let mut epoque = 1u32;
    loop {
        if matches!(
            vivre_epoque_avec_pair(ctx, transport, epoque, pair)?,
            FinEpoque::Arret
        ) {
            return Ok(());
        }
        // Reconnexion : seule la même extrémité peut reprendre la session (les
        // permissions accordées lui appartiennent).
        let meme_pair = move |venu: NovaId| venu == pair;
        let reprise = AttenteRendezvous {
            rv: &rv,
            local_id: ctx.config.local_id,
            identite: &identite,
            stun_servers,
            relay,
            admission: &meme_pair,
        };
        let Some((nouveau, revenant)) = se_reconnecter(ctx, || {
            p2p::accepter_par_rendezvous(&reprise, Some(DELAI_TENTATIVE_RECONNEXION), &ctx.stop)
        }) else {
            return Ok(());
        };
        transport = nouveau;
        pair = revenant;
        epoque += 1;
    }
}

/// Boucle de reconnexion : pousse `Reconnecting`, cadence les tentatives selon
/// la politique ([`ReconnectController`]) et rend le fruit de la première
/// tentative réussie. `None` = arrêt demandé ou politique épuisée
/// (`has_given_up`) : la session doit se clore.
fn se_reconnecter<T>(ctx: &ContextePilote, mut tentative: impl FnMut() -> Result<T>) -> Option<T> {
    let _ = ctx.state_tx.send(SessionState::Reconnecting);
    let mut controleur = ReconnectController::new(ctx.options.reconnect);
    controleur.on_disconnect();
    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            return None;
        }
        // `next_delay` rend le délai avant la prochaine tentative, ou None
        // quand la politique a épuisé ses tentatives.
        let delai = controleur.next_delay()?;
        debug_assert!(!controleur.has_given_up());
        attendre_interruptible(delai, &ctx.stop);
        if ctx.stop.load(Ordering::Relaxed) {
            return None;
        }
        if let Ok(succes) = tentative() {
            controleur.reset();
            ctx.compteurs.reconnexions.fetch_add(1, Ordering::Relaxed);
            return Some(succes);
        }
    }
}

/// Attente interruptible : dort par tranches en surveillant le signal d'arrêt.
fn attendre_interruptible(delai: Duration, stop: &Arc<AtomicBool>) {
    let echeance = Instant::now() + delai;
    while Instant::now() < echeance && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Traduit la politique de reconnexion de session en [`Backoff`] transport
/// (délai initial, plafond, nombre de tentatives).
fn backoff_depuis_politique(politique: ReconnectPolicy) -> Backoff {
    Backoff {
        delai_initial: Duration::from_millis(politique.base_delay_ms),
        delai_max: Duration::from_millis(politique.max_delay_ms),
        max_tentatives: politique.max_attempts,
    }
}

/// Rôle **contrôleur** en [`SessionEndpoint::Direct`] avec **reconnexion
/// transparente au niveau transport** : le transport de session est enveloppé
/// dans un [`ReconnectingTransport`] dont la fabrique re-connecte QUIC **et
/// re-négocie Noise** vers la même adresse/certificat à chaque coupure (l'hôte
/// doit ré-accepter). Le consommateur ne voit pas la coupure : la session reste
/// `Active` et le flux reprend sur l'image-clé de la nouvelle négociation.
///
/// À la différence de la boucle d'époques ([`SessionEndpoint::ByRendezvous`]),
/// il n'y a **qu'une** époque logique : la garde de coupure n'est pas armée, le
/// rétablissement est porté par le transport. L'épuisement du backoff clôt la
/// session (erreur remontée au pilote).
fn vivre_direct_reconnectant(
    ctx: &ContextePilote,
    addr: SocketAddr,
    cert_der: Vec<u8>,
) -> Result<()> {
    let _ = ctx.state_tx.send(SessionState::Connecting);
    // Identité Noise éphémère régénérée à chaque (re)négociation.
    let fabrique = move || -> Result<Box<dyn Transport>> {
        let brut = connect_quic(addr, &cert_der)?;
        let cles = generate_static_keypair()?;
        let securise = establish(Box::new(brut), HandshakeRole::Initiator, &cles.private)?;
        Ok(Box::new(securise) as Box<dyn Transport>)
    };
    let _ = ctx.state_tx.send(SessionState::Handshaking);
    let initial = fabrique()?;
    let _ = ctx.state_tx.send(SessionState::Active);

    let reconnectant = ReconnectingTransport::avec_backoff(
        initial,
        fabrique,
        backoff_depuis_politique(ctx.options.reconnect),
    );
    let partage = TransportPartage::new(Box::new(reconnectant), Arc::clone(&ctx.compteurs));
    let params = ParamsEpoqueControleur {
        compteurs: &ctx.compteurs,
        stop: &ctx.stop,
        etats: &ctx.state_tx,
        frame_tx: &ctx.frame_tx,
        entrees: &ctx.input_rx,
        epoque: 1,
    };
    if ctx.options.extended_features {
        executer_controleur_ext(partage, &params, &ctx.media, &ctx.stop)
    } else {
        executer_controleur(partage, &params, &ctx.stop)
    }
}

// ---------------------------------------------------------------------------
// Époques (une connexion vécue de bout en bout)
// ---------------------------------------------------------------------------

/// Issue d'une époque média.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinEpoque {
    /// Arrêt demandé (signal global) : la session se clôt proprement.
    Arret,
    /// Lien perdu ou pair parti : candidate à la reconnexion.
    LienPerdu,
}

/// Garde d'époque : détection de coupure du lien + pont d'arrêt.
///
/// * `lien_coupe` est levé par le rappel [`QuicTransport::on_disconnect`] ;
/// * le thread « nd-session-garde » lève `arret_epoque` dès que le signal
///   global **ou** la coupure du lien survient — c'est ce drapeau que les
///   boucles média de l'époque observent.
struct GardeEpoque {
    lien_coupe: Arc<AtomicBool>,
    arret_epoque: Arc<AtomicBool>,
    pont: Option<JoinHandle<()>>,
}

impl GardeEpoque {
    /// Arme la garde sur un transport concret (rappel de coupure) et lance le
    /// pont d'arrêt.
    fn armer(transport: &QuicTransport, stop: &Arc<AtomicBool>) -> Result<Self> {
        let lien_coupe = Arc::new(AtomicBool::new(false));
        let arret_epoque = Arc::new(AtomicBool::new(false));

        let coupure = Arc::clone(&lien_coupe);
        transport.on_disconnect(move |_raison| coupure.store(true, Ordering::Relaxed));

        let pont_stop = Arc::clone(stop);
        let pont_lien = Arc::clone(&lien_coupe);
        let pont_arret = Arc::clone(&arret_epoque);
        let pont = thread::Builder::new()
            .name("nd-session-garde".to_owned())
            .spawn(move || {
                while !pont_arret.load(Ordering::Relaxed)
                    && !pont_stop.load(Ordering::Relaxed)
                    && !pont_lien.load(Ordering::Relaxed)
                {
                    thread::sleep(PERIODE_GARDE);
                }
                pont_arret.store(true, Ordering::Relaxed);
            })?;

        Ok(Self {
            lien_coupe,
            arret_epoque,
            pont: Some(pont),
        })
    }

    /// Drapeau d'arrêt de l'époque, à donner aux boucles média.
    fn arret(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.arret_epoque)
    }

    /// Termine le pont et classe l'issue de l'époque : arrêt global, lien
    /// perdu, ou erreur réelle. Quand le lien est mort en cours d'époque, une
    /// erreur média (décodage interrompu, écriture refusée…) en est un
    /// **symptôme** : elle est reclassée `LienPerdu` plutôt que fatale.
    fn conclure(mut self, resultat: Result<()>, stop: &Arc<AtomicBool>) -> Result<FinEpoque> {
        self.arret_epoque.store(true, Ordering::Relaxed);
        if let Some(pont) = self.pont.take() {
            let _ = pont.join();
        }
        let arret_global = stop.load(Ordering::Relaxed);
        let coupe = self.lien_coupe.load(Ordering::Relaxed);
        match resultat {
            Ok(()) if arret_global => Ok(FinEpoque::Arret),
            Ok(()) => Ok(FinEpoque::LienPerdu),
            Err(_) if coupe && !arret_global => Ok(FinEpoque::LienPerdu),
            Err(e) => Err(e),
        }
    }
}

/// Vit une époque avec le pair par défaut de la configuration (points de
/// contact sans résolution d'ID : le pair n'est pas toujours connu).
fn vivre_epoque(ctx: &ContextePilote, transport: QuicTransport, epoque: u32) -> Result<FinEpoque> {
    let pair = ctx.config.peer_id.unwrap_or(NovaId(0));
    vivre_epoque_avec_pair(ctx, transport, epoque, pair)
}

/// Vit une époque : handshake Noise puis boucle média selon le rôle, sous la
/// surveillance de la [`GardeEpoque`].
fn vivre_epoque_avec_pair(
    ctx: &ContextePilote,
    transport: QuicTransport,
    epoque: u32,
    pair: NovaId,
) -> Result<FinEpoque> {
    match ctx.config.role {
        SessionRole::Controller => {
            let params = ParamsEpoqueControleur {
                compteurs: &ctx.compteurs,
                stop: &ctx.stop,
                etats: &ctx.state_tx,
                frame_tx: &ctx.frame_tx,
                entrees: &ctx.input_rx,
                epoque,
            };
            if ctx.options.extended_features {
                vivre_epoque_controleur_ext(transport, &params, &ctx.media)
            } else {
                vivre_epoque_controleur(transport, &params)
            }
        }
        SessionRole::Controlled => {
            let params = ParamsEpoqueHote {
                permissions: resoudre_permissions(&ctx.config, &ctx.options),
                flux: options_flux_hote(&ctx.options, epoque, Arc::clone(&ctx.media.moniteur)),
                compteurs: &ctx.compteurs,
                stop: &ctx.stop,
                etats: Some(&ctx.state_tx),
                pair,
            };
            if ctx.options.extended_features {
                vivre_epoque_hote_ext(transport, &params, &ctx.media)
            } else {
                vivre_epoque_hote(transport, &params)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Époque du contrôleur
// ---------------------------------------------------------------------------

/// Paramètres d'une époque du rôle contrôleur.
struct ParamsEpoqueControleur<'a> {
    compteurs: &'a Arc<CompteursSession>,
    stop: &'a Arc<AtomicBool>,
    etats: &'a Sender<SessionState>,
    frame_tx: &'a SyncSender<DecodedFrame>,
    entrees: &'a EntreesPartagees,
    epoque: u32,
}

/// Époque complète du contrôleur : garde + Noise (initiateur) + média.
fn vivre_epoque_controleur(
    transport: QuicTransport,
    params: &ParamsEpoqueControleur<'_>,
) -> Result<FinEpoque> {
    let garde = GardeEpoque::armer(&transport, params.stop)?;
    let arret = garde.arret();
    let resultat = derouler_epoque_controleur(transport, params, &arret);
    garde.conclure(resultat, params.stop)
}

/// Corps faillible de l'époque contrôleur (la garde est conclue par l'appelant).
fn derouler_epoque_controleur(
    transport: QuicTransport,
    params: &ParamsEpoqueControleur<'_>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let _ = params.etats.send(SessionState::Handshaking);
    // Identité Noise éphémère (une paire de clés par session) : le branchement de
    // l'identité persistante (`IdentityStore`) et l'épinglage TOFU viennent avec
    // le lot 06.
    let cles = generate_static_keypair()?;
    let securise = establish(Box::new(transport), HandshakeRole::Initiator, &cles.private)?;
    let _ = params.etats.send(SessionState::Active);

    let partage = TransportPartage::new(Box::new(securise), Arc::clone(params.compteurs));
    executer_controleur(partage, params, arret)
}

/// Boucle du rôle **contrôleur** : le pilote décode le flux vidéo entrant vers
/// `frame_tx` (via [`ViewerPipeline::run_streaming`]) pendant qu'un thread dédié
/// transmet les entrées de la file partagée sur le canal `Input` chiffré.
fn executer_controleur(
    transport: TransportPartage,
    params: &ParamsEpoqueControleur<'_>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let decodeur = create_decoder(CodecKind::H264)?;

    // Reprise : purge des entrées accumulées pendant la coupure (mouvements
    // souris périmés — les rejouer téléporterait le curseur du pair).
    if params.epoque > 1 {
        if let Ok(file) = params.entrees.lock() {
            while file.try_recv().is_ok() {}
        }
    }

    // Thread d'envoi des entrées : seul émetteur de ce côté de la session.
    let mut transport_entrees = transport.clone();
    let arret_entrees = Arc::clone(arret);
    let file_entrees = Arc::clone(params.entrees);
    let entrees = thread::Builder::new()
        .name("nd-session-entrees".to_owned())
        .spawn(move || {
            let canal = transport_entrees.open_channel(ChannelKind::Input);
            while !arret_entrees.load(Ordering::Relaxed) {
                let evenement = {
                    let file = file_entrees.lock().expect("verrou de la file d'entrées");
                    file.recv_timeout(Duration::from_millis(50))
                };
                match evenement {
                    Ok(evenement) => {
                        if transport_entrees
                            .send(canal, evenement.to_bytes(), Reliability::Reliable)
                            .is_err()
                        {
                            // Connexion fermée : le pilote terminera l'époque.
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
    let compteurs_frames = Arc::clone(params.compteurs);
    let frame_tx = params.frame_tx.clone();
    let resultat = viewer.run_streaming(
        move |frame| {
            compteurs_frames.frame_livree();
            // File pleine = consommateur en retard : la frame est sautée (try_send)
            // pour que le flux ne bloque jamais.
            let _ = frame_tx.try_send(frame);
        },
        Arc::clone(arret),
    );

    arret.store(true, Ordering::Relaxed);
    let _ = entrees.join();
    resultat.map(|_livrees| ())
}

// ---------------------------------------------------------------------------
// Époque de l'hôte (partagée avec le service « accès non surveillé »)
// ---------------------------------------------------------------------------

/// Paramètres d'une époque du rôle contrôlé (hôte). Aussi consommée par
/// [`crate::UnattendedHost`] : `etats` y est `None` (pas de machine à états).
pub(crate) struct ParamsEpoqueHote<'a> {
    /// Capacités accordées à la session : vérifiées **avant chaque injection**.
    pub permissions: PermissionSet,
    /// Options du flux hôte piloté (ABR, delta, enregistrement de l'époque).
    pub flux: HostStreamOptions,
    /// Compteurs alimentés en continu (statistiques).
    pub compteurs: &'a Arc<CompteursSession>,
    /// Signal d'arrêt global du propriétaire de la session.
    pub stop: &'a Arc<AtomicBool>,
    /// Canal des transitions d'état, si le propriétaire en tient un.
    pub etats: Option<&'a Sender<SessionState>>,
    /// ID du pair contrôleur (acteur du journal d'audit des permissions).
    pub pair: NovaId,
}

/// Époque complète de l'hôte : garde + Noise (répondeur) + média piloté.
pub(crate) fn vivre_epoque_hote(
    transport: QuicTransport,
    params: &ParamsEpoqueHote<'_>,
) -> Result<FinEpoque> {
    let garde = GardeEpoque::armer(&transport, params.stop)?;
    let arret = garde.arret();
    let resultat = derouler_epoque_hote(transport, params, &arret);
    garde.conclure(resultat, params.stop)
}

/// Corps faillible de l'époque hôte (la garde est conclue par l'appelant).
fn derouler_epoque_hote(
    transport: QuicTransport,
    params: &ParamsEpoqueHote<'_>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    if let Some(etats) = params.etats {
        let _ = etats.send(SessionState::Handshaking);
    }
    let cles = generate_static_keypair()?;
    let securise = establish(Box::new(transport), HandshakeRole::Responder, &cles.private)?;
    if let Some(etats) = params.etats {
        let _ = etats.send(SessionState::Active);
    }

    let partage = TransportPartage::new(Box::new(securise), Arc::clone(params.compteurs));
    executer_hote(&partage, params, arret)
}

/// Encodeur du flux hôte : **matériel d'abord** (NVENC via le MFT asynchrone,
/// repli MFT logiciel documenté par `nd-codec`), puis repli openh264 si la
/// pile plateforme est entièrement indisponible. Ne panique jamais : dégrade.
fn creer_encodeur_hote() -> Result<Box<dyn VideoEncoder>> {
    create_hardware_encoder(CodecKind::H264).or_else(|_indisponible| {
        // Plateforme sans encodeur matériel/MFT : repli logiciel openh264.
        create_encoder(CodecKind::H264)
    })
}

/// Boucle du rôle **contrôlé** : le pilote diffuse l'écran (via
/// [`HostPipeline::run_streaming_pilote`] : encodeur matériel, ABR ~1 Hz,
/// enregistrement opt-in) pendant qu'un thread dédié reçoit les entrées, les
/// passe au **filtre de permissions** puis les applique à l'OS ([`apply_input`]).
fn executer_hote(
    transport: &TransportPartage,
    params: &ParamsEpoqueHote<'_>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let injecteur = create_injector()?;
    let capteur = create_capturer()?;
    let encodeur = creer_encodeur_hote()?;
    params.compteurs.note_backend(encodeur.nom_backend());
    // Construit le pipeline avant de lancer le thread auxiliaire : tout échec
    // (capture indisponible…) est ainsi remonté sans laisser de thread derrière.
    let mut hote = HostPipeline::new(capteur, encodeur, Box::new(transport.clone()))?;

    // Thread de réception/application des entrées : seul récepteur de ce côté.
    // Le guichet de permissions vit dans ce thread (chemin chaud sans verrou) :
    // `is_allowed` par événement, journalisation au premier refus par capacité.
    let mut broker = PermissionBroker::with_permissions(params.permissions);
    let acteur = params.pair.to_string();
    let mut transport_entrees = transport.clone();
    let arret_entrees = Arc::clone(arret);
    let compteurs_entrees = Arc::clone(params.compteurs);
    let entrees = thread::Builder::new()
        .name("nd-session-injection".to_owned())
        .spawn(move || {
            let mut refus_journalises = PermissionSet::none();
            while !arret_entrees.load(Ordering::Relaxed) {
                match transport_entrees.poll_recv() {
                    Ok(Some((_canal, donnees))) => {
                        let Some(evenement) = InputEvent::from_bytes(&donnees) else {
                            continue;
                        };
                        let capacite = Capability::required_for_input(&evenement);
                        if broker.is_allowed(capacite) {
                            if apply_input(injecteur.as_ref(), &evenement).is_ok() {
                                compteurs_entrees
                                    .inputs_applied
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            // Entrée non autorisée : jetée silencieusement
                            // (chemin chaud), comptée, et tracée dans le journal
                            // d'audit au premier refus de chaque capacité.
                            compteurs_entrees
                                .inputs_denied
                                .fetch_add(1, Ordering::Relaxed);
                            if refus_journalises.grant(capacite) {
                                let _ = broker.authorize_input(&acteur, &evenement);
                            }
                        }
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
            // Anti « stuck key » : tout relâcher en fin d'époque.
            injecteur.release_all();
        })?;

    // Cumul inter-époques : l'enregistrement repart de zéro à chaque époque.
    let enregistrees_avant = params.compteurs.frames_enregistrees.load(Ordering::Relaxed);
    let compteurs_flux = Arc::clone(params.compteurs);
    let resultat = hote.run_streaming_pilote(Arc::clone(arret), params.flux.clone(), move |tick| {
        compteurs_flux
            .debit_cible_kbps
            .store(u64::from(tick.target_bitrate_kbps), Ordering::Relaxed);
        compteurs_flux
            .palier_abr
            .store(u64::from(tick.abr_level), Ordering::Relaxed);
        compteurs_flux
            .frames_enregistrees
            .store(enregistrees_avant + tick.frames_recorded, Ordering::Relaxed);
    });

    arret.store(true, Ordering::Relaxed);
    let _ = entrees.join();
    resultat.map(|_rapport| ())
}

// ---------------------------------------------------------------------------
// Époques « étendues » : vidéo + entrées **plus** audio, fichiers,
// presse-papiers, chat et bascule moniteur (voir [`crate::media`]).
//
// Invariant crypto : dans chaque direction, un seul récepteur (démux ordonné
// des nonces) et des envois **tous fiables** (mutex sérialisant, un seul
// domaine d'ordonnancement — pas de mélange fiable/datagrammes).
// ---------------------------------------------------------------------------

/// Horloge monotone en microsecondes depuis `debut` (base commune de
/// `recevoir`/`tick_lecture` audio).
fn maintenant_us(debut: Instant) -> u64 {
    u64::try_from(debut.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Période du pas de lecture audio côté contrôleur (~50 Hz).
const PERIODE_TICK_AUDIO_US: u64 = 20_000;

/// Période de scrutation du presse-papiers local (émission des changements).
const PERIODE_PRESSE_PAPIERS: Duration = Duration::from_millis(250);

/// Cartographie `index de canal → catégorie` pour le démultiplexage : pré-ouvre
/// tous les canaux susceptibles d'arriver (dont la vidéo de chaque moniteur) de
/// sorte que chaque `poll_recv` se range par catégorie.
fn construire_carte_reception(transport: &mut impl Transport) -> HashMap<u32, Categorie> {
    let mut kinds = vec![
        ChannelKind::Control,
        ChannelKind::Input,
        ChannelKind::Audio,
        ChannelKind::Files,
    ];
    for m in 0..MONITEURS_MAX {
        kinds.push(ChannelKind::Video(MonitorId(m)));
    }
    let mut carte = HashMap::new();
    for kind in kinds {
        let handle = transport.open_channel(kind);
        carte.insert(handle.0, Categorie::depuis_kind(kind));
    }
    carte
}

/// Applique une entrée reçue derrière le **filtre de permissions** (chemin chaud
/// partagé par l'injection classique et étendue).
fn appliquer_entree_gardee(
    injecteur: &dyn InputInjector,
    broker: &mut PermissionBroker,
    refus_journalises: &mut PermissionSet,
    compteurs: &CompteursSession,
    acteur: &str,
    data: &[u8],
) {
    let Some(evenement) = InputEvent::from_bytes(data) else {
        return;
    };
    let capacite = Capability::required_for_input(&evenement);
    if broker.is_allowed(capacite) {
        if apply_input(injecteur, &evenement).is_ok() {
            compteurs.inputs_applied.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        compteurs.inputs_denied.fetch_add(1, Ordering::Relaxed);
        if refus_journalises.grant(capacite) {
            let _ = broker.authorize_input(acteur, &evenement);
        }
    }
}

/// Construit (paresseusement) la session audio système si [`Capability::Audio`]
/// est accordé, puis règle le sens actif selon le rôle (émission côté hôte,
/// lecture côté contrôleur).
fn assurer_audio(media: &EtatMedia) {
    if !media.permissions.allows(Capability::Audio) {
        return;
    }
    let mut garde = media.audio.lock().expect("verrou audio");
    if garde.is_none() {
        if let Ok(session) = AudioSession::duplex_systeme() {
            *garde = Some(session);
        }
    }
    if let Some(audio) = garde.as_mut() {
        let actif = media.audio_actif.load(Ordering::Relaxed);
        match media.role {
            SessionRole::Controlled => {
                audio.definir_emission_active(actif);
                audio.definir_lecture_active(false);
            }
            SessionRole::Controller => {
                audio.definir_lecture_active(actif);
                audio.definir_emission_active(false);
            }
        }
    }
}

/// Reçoit une trame du canal `Files` : crée paresseusement le récepteur si
/// besoin, l'alimente, puis draine ses événements vers la poignée.
fn traiter_fichiers(media: &EtatMedia, data: &[u8]) {
    if !media.permissions.allows(Capability::FileDownload) {
        return;
    }
    let mut garde = media.transfer.lock().expect("verrou transfert");
    if garde.is_none() {
        *garde = Some(TransferSession::receive(media.transfer_dir.clone()));
    }
    if let Some(session) = garde.as_mut() {
        let _ = session.handle_incoming(data);
        for evenement in session.take_events() {
            let _ = media.transfer_out.send(evenement);
        }
    }
}

/// Reçoit un message du canal `Control` multiplexé : chat, presse-papiers ou
/// bascule moniteur (chacun gardé par sa capacité).
fn traiter_controle(media: &EtatMedia, data: &[u8]) {
    let Some((sous_type, payload)) = decoder_controle(data) else {
        return;
    };
    match sous_type {
        SousTypeControle::Chat => {
            if let Ok(texte) = String::from_utf8(payload.to_vec()) {
                let _ = media.chat_out.send(ChatMessage {
                    from_remote: true,
                    text: texte,
                });
            }
        }
        SousTypeControle::PressePapiers => {
            if !media.permissions.allows(Capability::ClipboardWrite) {
                return;
            }
            if let Some(clip) = media
                .clipboard
                .lock()
                .expect("verrou presse-papiers")
                .as_ref()
            {
                let _ = clip.apply_bytes(payload);
            }
            // Mémorise pour ne pas ré-émettre ce que l'on vient d'appliquer.
            *media
                .dernier_presse_papiers
                .lock()
                .expect("verrou dernier presse-papiers") = Some(payload.to_vec());
        }
        SousTypeControle::BasculeMoniteur => {
            if let Ok(octets) = <[u8; 4]>::try_from(payload) {
                media
                    .moniteur
                    .store(u32::from_be_bytes(octets), Ordering::Relaxed);
            }
        }
    }
}

/// Récepteur démux **contrôleur** : vidéo → décodage → frames, audio →
/// `recevoir`/`tick_lecture` → lecture, fichiers → transfert, contrôle →
/// presse-papiers/chat.
fn recepteur_controleur(
    mut transport: TransportPartage,
    mut decodeur: Box<dyn VideoDecoder>,
    frame_tx: SyncSender<DecodedFrame>,
    media: &Arc<EtatMedia>,
    compteurs: &Arc<CompteursSession>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let carte = construire_carte_reception(&mut transport);
    let debut = Instant::now();
    let mut prochain_tick = PERIODE_TICK_AUDIO_US;
    while !arret.load(Ordering::Relaxed) {
        match transport.poll_recv() {
            Ok(Some((handle, data))) => match carte.get(&handle.0).copied() {
                Some(Categorie::Video(_)) => {
                    let chunk = EncodedChunk {
                        data,
                        is_keyframe: false,
                        monitor: MonitorId(0),
                        timestamp_us: 0,
                    };
                    if let Some(frame) = decodeur.decode(&chunk)? {
                        compteurs.frame_livree();
                        let _ = frame_tx.try_send(frame);
                    }
                }
                Some(Categorie::Audio) => {
                    if media.permissions.allows(Capability::Audio)
                        && media.audio_actif.load(Ordering::Relaxed)
                    {
                        if let Some(paquet) = decoder_audio(&data) {
                            let arrivee = maintenant_us(debut);
                            if let Some(audio) = media.audio.lock().expect("verrou audio").as_mut()
                            {
                                audio.recevoir(paquet, arrivee);
                            }
                        }
                    }
                }
                Some(Categorie::Fichiers) => traiter_fichiers(media, &data),
                Some(Categorie::Controle) => traiter_controle(media, &data),
                Some(Categorie::Input) | None => {}
            },
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
        // Pas de lecture audio cadencé (~20 ms), indépendant des arrivées.
        let maintenant = maintenant_us(debut);
        if maintenant >= prochain_tick {
            prochain_tick = maintenant + PERIODE_TICK_AUDIO_US;
            if media.permissions.allows(Capability::Audio) {
                if let Some(audio) = media.audio.lock().expect("verrou audio").as_mut() {
                    let _ = audio.tick_lecture(maintenant);
                }
            }
        }
    }
    Ok(())
}

/// Récepteur démux **hôte** : entrées → filtre de permissions → injection ;
/// fichiers → transfert ; contrôle → presse-papiers/chat/bascule moniteur.
fn recepteur_hote(
    mut transport: TransportPartage,
    injecteur: Box<dyn InputInjector>,
    permissions: PermissionSet,
    acteur: String,
    media: &Arc<EtatMedia>,
    compteurs: &Arc<CompteursSession>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let carte = construire_carte_reception(&mut transport);
    let mut broker = PermissionBroker::with_permissions(permissions);
    let mut refus_journalises = PermissionSet::none();
    while !arret.load(Ordering::Relaxed) {
        match transport.poll_recv() {
            Ok(Some((handle, data))) => match carte.get(&handle.0).copied() {
                Some(Categorie::Input) => appliquer_entree_gardee(
                    injecteur.as_ref(),
                    &mut broker,
                    &mut refus_journalises,
                    compteurs,
                    &acteur,
                    &data,
                ),
                Some(Categorie::Fichiers) => traiter_fichiers(media, &data),
                Some(Categorie::Controle) => traiter_controle(media, &data),
                _ => {}
            },
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
    injecteur.release_all();
    Ok(())
}

/// Draine les commandes de la poignée (envoi de fichiers, bascule moniteur,
/// activation audio) et les applique/émet.
fn traiter_commandes(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
) {
    let commandes: Vec<CommandeMedia> = {
        let rx = media.commandes.lock().expect("verrou commandes");
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    };
    for commande in commandes {
        match commande {
            CommandeMedia::EnvoyerFichiers(fichiers) => {
                if media.permissions.allows(Capability::FileUpload) {
                    if let Ok(session) = TransferSession::send(fichiers) {
                        *media.transfer.lock().expect("verrou transfert") = Some(session);
                    }
                }
            }
            CommandeMedia::BasculerMoniteur(index) => match media.role {
                // Le contrôleur demande à l'hôte ; l'hôte se commande lui-même.
                SessionRole::Controller => {
                    let trame =
                        encoder_controle(SousTypeControle::BasculeMoniteur, &index.to_be_bytes());
                    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
                }
                SessionRole::Controlled => media.moniteur.store(index, Ordering::Relaxed),
            },
            CommandeMedia::AudioActif(actif) => {
                media.audio_actif.store(actif, Ordering::Relaxed);
                if let Some(audio) = media.audio.lock().expect("verrou audio").as_mut() {
                    match media.role {
                        SessionRole::Controlled => audio.definir_emission_active(actif),
                        SessionRole::Controller => audio.definir_lecture_active(actif),
                    }
                }
            }
        }
    }
}

/// Émet les messages de chat en attente (canal `Control`) + écho local.
fn envoyer_chat_en_attente(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
) {
    let messages: Vec<String> = {
        let rx = media.chat_in.lock().expect("verrou chat");
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    };
    for texte in messages {
        let trame = encoder_controle(SousTypeControle::Chat, texte.as_bytes());
        if transport
            .send(canal_controle, trame, Reliability::Reliable)
            .is_ok()
        {
            let _ = media.chat_out.send(ChatMessage {
                from_remote: false,
                text: texte,
            });
        }
    }
}

/// Émet le presse-papiers local s'il a changé (garde [`Capability::ClipboardRead`]).
fn synchroniser_presse_papiers(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
) {
    if !media.permissions.allows(Capability::ClipboardRead) {
        return;
    }
    let octets = {
        let garde = media.clipboard.lock().expect("verrou presse-papiers");
        garde
            .as_ref()
            .and_then(|clip| clip.capture_bytes().ok().flatten())
    };
    let Some(octets) = octets else {
        return;
    };
    {
        let mut dernier = media
            .dernier_presse_papiers
            .lock()
            .expect("verrou dernier presse-papiers");
        if dernier.as_ref() == Some(&octets) {
            return;
        }
        *dernier = Some(octets.clone());
    }
    let trame = encoder_controle(SousTypeControle::PressePapiers, &octets);
    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
}

/// Pompe le transfert de fichiers actif : émet les trames sortantes (canal
/// `Files`) et draine les événements vers la poignée.
fn pomper_transfert(
    transport: &mut TransportPartage,
    canal_files: ChannelHandle,
    media: &EtatMedia,
) {
    let mut garde = media.transfer.lock().expect("verrou transfert");
    let Some(session) = garde.as_mut() else {
        return;
    };
    while let Ok(Some(bytes)) = session.poll_outgoing() {
        if transport
            .send(canal_files, bytes, Reliability::Reliable)
            .is_err()
        {
            break;
        }
    }
    for evenement in session.take_events() {
        let _ = media.transfer_out.send(evenement);
    }
}

/// Boucle d'**émission audio** dédiée de l'hôte (canal `Audio`) : produit une
/// trame par tour et l'émet. La cadence est portée par la capture elle-même
/// (`produire` bloque jusqu'à la trame suivante ≈ 20 ms) — un thread à part pour
/// ne pas retarder le plan de contrôle du thread de fonctions.
fn boucle_audio_hote(
    mut transport: TransportPartage,
    media: &Arc<EtatMedia>,
    arret: &Arc<AtomicBool>,
) {
    let canal_audio = transport.open_channel(ChannelKind::Audio);
    while !arret.load(Ordering::Relaxed) {
        if !media.permissions.allows(Capability::Audio)
            || !media.audio_actif.load(Ordering::Relaxed)
        {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let paquet = {
            let mut garde = media.audio.lock().expect("verrou audio");
            garde
                .as_mut()
                .and_then(|audio| audio.produire().ok().flatten())
        };
        match paquet {
            Some(paquet) => {
                if transport
                    .send(canal_audio, encoder_audio(&paquet), Reliability::Reliable)
                    .is_err()
                {
                    break;
                }
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Émetteur de fonctions étendues côté **contrôleur** : entrées (canal `Input`)
/// + plan de contrôle (fichiers, presse-papiers, chat, commandes).
fn emetteur_features_controleur(
    mut transport: TransportPartage,
    media: &Arc<EtatMedia>,
    entrees: &EntreesPartagees,
    arret: &Arc<AtomicBool>,
) {
    let canal_input = transport.open_channel(ChannelKind::Input);
    let canal_files = transport.open_channel(ChannelKind::Files);
    let canal_controle = transport.open_channel(ChannelKind::Control);
    let mut prochaine_synchro = Instant::now();
    while !arret.load(Ordering::Relaxed) {
        // Entrées : basse latence (recv borné), seule voie émettrice restante.
        let evenement = {
            let file = entrees.lock().expect("verrou de la file d'entrées");
            file.recv_timeout(Duration::from_millis(10))
        };
        match evenement {
            Ok(evenement) => {
                if transport
                    .send(canal_input, evenement.to_bytes(), Reliability::Reliable)
                    .is_err()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        traiter_commandes(&mut transport, canal_controle, media);
        envoyer_chat_en_attente(&mut transport, canal_controle, media);
        pomper_transfert(&mut transport, canal_files, media);
        if Instant::now() >= prochaine_synchro {
            prochaine_synchro = Instant::now() + PERIODE_PRESSE_PAPIERS;
            synchroniser_presse_papiers(&mut transport, canal_controle, media);
        }
    }
}

/// Émetteur de fonctions étendues côté **hôte** : audio (canal `Audio`) + plan
/// de contrôle (fichiers, presse-papiers, chat, commandes).
fn emetteur_features_hote(
    mut transport: TransportPartage,
    media: &Arc<EtatMedia>,
    arret: &Arc<AtomicBool>,
) {
    let canal_files = transport.open_channel(ChannelKind::Files);
    let canal_controle = transport.open_channel(ChannelKind::Control);
    let mut prochaine_synchro = Instant::now();
    while !arret.load(Ordering::Relaxed) {
        traiter_commandes(&mut transport, canal_controle, media);
        envoyer_chat_en_attente(&mut transport, canal_controle, media);
        pomper_transfert(&mut transport, canal_files, media);
        if Instant::now() >= prochaine_synchro {
            prochaine_synchro = Instant::now() + PERIODE_PRESSE_PAPIERS;
            synchroniser_presse_papiers(&mut transport, canal_controle, media);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Époque **étendue** du contrôleur : garde + Noise (initiateur) + démux média.
fn vivre_epoque_controleur_ext(
    transport: QuicTransport,
    params: &ParamsEpoqueControleur<'_>,
    media: &Arc<EtatMedia>,
) -> Result<FinEpoque> {
    let garde = GardeEpoque::armer(&transport, params.stop)?;
    let arret = garde.arret();
    let resultat = derouler_epoque_controleur_ext(transport, params, media, &arret);
    garde.conclure(resultat, params.stop)
}

/// Corps faillible de l'époque contrôleur étendue.
fn derouler_epoque_controleur_ext(
    transport: QuicTransport,
    params: &ParamsEpoqueControleur<'_>,
    media: &Arc<EtatMedia>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let _ = params.etats.send(SessionState::Handshaking);
    let cles = generate_static_keypair()?;
    let securise = establish(Box::new(transport), HandshakeRole::Initiator, &cles.private)?;
    let _ = params.etats.send(SessionState::Active);
    let partage = TransportPartage::new(Box::new(securise), Arc::clone(params.compteurs));
    executer_controleur_ext(partage, params, media, arret)
}

/// Boucle média **étendue** du contrôleur sur un transport déjà chiffré :
/// récepteur démux (pilote) + émetteur de fonctions (thread). Partagée par
/// l'époque étendue et la voie à reconnexion transport ([`vivre_direct_reconnectant`]).
fn executer_controleur_ext(
    partage: TransportPartage,
    params: &ParamsEpoqueControleur<'_>,
    media: &Arc<EtatMedia>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let decodeur = create_decoder(CodecKind::H264)?;
    assurer_audio(media);
    // Reprise : purge des entrées périmées accumulées pendant la coupure.
    if params.epoque > 1 {
        if let Ok(file) = params.entrees.lock() {
            while file.try_recv().is_ok() {}
        }
    }

    let transport_emetteur = partage.clone();
    let media_emetteur = Arc::clone(media);
    let entrees = Arc::clone(params.entrees);
    let arret_emetteur = Arc::clone(arret);
    let emetteur = thread::Builder::new()
        .name("nd-session-features-ctl".to_owned())
        .spawn(move || {
            emetteur_features_controleur(
                transport_emetteur,
                &media_emetteur,
                &entrees,
                &arret_emetteur,
            );
        })?;

    let resultat = recepteur_controleur(
        partage,
        decodeur,
        params.frame_tx.clone(),
        media,
        params.compteurs,
        arret,
    );
    arret.store(true, Ordering::Relaxed);
    let _ = emetteur.join();
    resultat
}

/// Époque **étendue** de l'hôte : garde + Noise (répondeur) + vidéo fiable +
/// audio + démux entrées/fichiers/contrôle.
fn vivre_epoque_hote_ext(
    transport: QuicTransport,
    params: &ParamsEpoqueHote<'_>,
    media: &Arc<EtatMedia>,
) -> Result<FinEpoque> {
    let garde = GardeEpoque::armer(&transport, params.stop)?;
    let arret = garde.arret();
    let resultat = derouler_epoque_hote_ext(transport, params, media, &arret);
    garde.conclure(resultat, params.stop)
}

/// Corps faillible de l'époque hôte étendue.
fn derouler_epoque_hote_ext(
    transport: QuicTransport,
    params: &ParamsEpoqueHote<'_>,
    media: &Arc<EtatMedia>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    if let Some(etats) = params.etats {
        let _ = etats.send(SessionState::Handshaking);
    }
    let cles = generate_static_keypair()?;
    let securise = establish(Box::new(transport), HandshakeRole::Responder, &cles.private)?;
    if let Some(etats) = params.etats {
        let _ = etats.send(SessionState::Active);
    }
    let partage = TransportPartage::new(Box::new(securise), Arc::clone(params.compteurs));

    let injecteur = create_injector()?;
    let capteur = create_capturer()?;
    let encodeur = creer_encodeur_hote()?;
    params.compteurs.note_backend(encodeur.nom_backend());
    let mut hote = HostPipeline::new(capteur, encodeur, Box::new(partage.clone()))?;
    assurer_audio(media);

    let transport_recepteur = partage.clone();
    let media_recepteur = Arc::clone(media);
    let compteurs_recepteur = Arc::clone(params.compteurs);
    let arret_recepteur = Arc::clone(arret);
    let permissions = params.permissions;
    let acteur = params.pair.to_string();
    let recepteur = thread::Builder::new()
        .name("nd-session-recv-hote".to_owned())
        .spawn(move || {
            let _ = recepteur_hote(
                transport_recepteur,
                injecteur,
                permissions,
                acteur,
                &media_recepteur,
                &compteurs_recepteur,
                &arret_recepteur,
            );
        })?;

    let transport_emetteur = partage.clone();
    let media_emetteur = Arc::clone(media);
    let arret_emetteur = Arc::clone(arret);
    let emetteur = thread::Builder::new()
        .name("nd-session-features-hote".to_owned())
        .spawn(move || {
            emetteur_features_hote(transport_emetteur, &media_emetteur, &arret_emetteur);
        })?;

    // Émission audio sur son propre thread : la capture cadence (≈ 20 ms/trame).
    let transport_audio = partage.clone();
    let media_audio = Arc::clone(media);
    let arret_audio = Arc::clone(arret);
    let audio = thread::Builder::new()
        .name("nd-session-audio-hote".to_owned())
        .spawn(move || boucle_audio_hote(transport_audio, &media_audio, &arret_audio))?;

    let enregistrees_avant = params.compteurs.frames_enregistrees.load(Ordering::Relaxed);
    let compteurs_flux = Arc::clone(params.compteurs);
    let resultat = hote.run_streaming_pilote(Arc::clone(arret), params.flux.clone(), move |tick| {
        maj_compteurs_flux(&compteurs_flux, enregistrees_avant, &tick);
    });

    arret.store(true, Ordering::Relaxed);
    let _ = recepteur.join();
    let _ = emetteur.join();
    let _ = audio.join();
    resultat.map(|_rapport| ())
}

/// Reporte un instantané [`HostStreamTick`] dans les compteurs de session.
fn maj_compteurs_flux(
    compteurs: &CompteursSession,
    enregistrees_avant: u64,
    tick: &HostStreamTick,
) {
    compteurs
        .debit_cible_kbps
        .store(u64::from(tick.target_bitrate_kbps), Ordering::Relaxed);
    compteurs
        .palier_abr
        .store(u64::from(tick.abr_level), Ordering::Relaxed);
    compteurs
        .frames_enregistrees
        .store(enregistrees_avant + tick.frames_recorded, Ordering::Relaxed);
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
    use nd_signaling::{await_p2p, serve, P2pIncoming, Registry};
    use nd_transport::{accept_quic_over_socket, bind, connect};
    use std::net::TcpListener;

    fn config(role: SessionRole, peer: Option<NovaId>) -> SessionConfig {
        SessionConfig {
            role,
            local_id: NovaId(101_010_101),
            peer_id: peer,
            permissions: Permissions::default(),
        }
    }

    /// Démarre un serveur de rendez-vous éphémère et rend son adresse.
    fn rendezvous_ephemere() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind rendez-vous");
        let addr = listener.local_addr().expect("adresse rendez-vous");
        thread::spawn(move || {
            let _ = serve(listener, Registry::new());
        });
        addr
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

    /// Hôte synthétique joignable **par rendez-vous** : publie son ID, attend le
    /// punch, accepte QUIC sur la socket percée (identité publiée), répond au
    /// handshake Noise, diffuse des frames 64×64 et compte les entrées reçues —
    /// le tout sur `epoques` connexions successives (test de reconnexion).
    fn hote_synthetique_rendezvous(
        rv_addr: SocketAddr,
        id: NovaId,
        epoques: usize,
        frames_par_epoque: usize,
        entrees_attendues: usize,
    ) -> thread::JoinHandle<Result<usize>> {
        thread::spawn(move || -> Result<usize> {
            let rv = RendezvousClient::new(rv_addr);
            let identite = ServerIdentity::generate()?;
            rv.register(
                id,
                "0.0.0.0:0".parse().expect("adresse de punch"),
                identite.cert_der(),
            )?;
            let mut entrees = 0usize;
            for _epoque in 0..epoques {
                let entrant = loop {
                    match await_p2p(&rv, id, &[], Duration::from_secs(20))? {
                        P2pIncoming::Direct(entrant) => break entrant,
                        P2pIncoming::RelayFallback { .. } => continue,
                    }
                };
                let brut = accept_quic_over_socket(entrant.socket, &identite)?;
                let cles = generate_static_keypair()?;
                let mut chiffre =
                    establish(Box::new(brut), HandshakeRole::Responder, &cles.private)?;
                let canal = chiffre.open_channel(ChannelKind::Video(MonitorId(0)));
                let mut encodeur = create_encoder(CodecKind::H264)?;
                encodeur.configure(EncoderConfig {
                    kind: CodecKind::H264,
                    width: 64,
                    height: 64,
                    target_bitrate_kbps: 1_000,
                    max_fps: 60,
                })?;
                let mut seq = 0usize;
                let echeance = Instant::now() + Duration::from_secs(20);
                while (seq < frames_par_epoque || entrees < entrees_attendues)
                    && Instant::now() < echeance
                {
                    if seq < frames_par_epoque {
                        let chunk =
                            encodeur.encode(&frame_synthetique(seq), seq.is_multiple_of(25))?;
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
                // Fin d'époque : la chute du transport ferme la connexion QUIC,
                // le contrôleur voit la coupure.
            }
            Ok(entrees)
        })
    }

    /// Draine `state_rx` jusqu'à l'état attendu (échec au-delà de l'échéance).
    fn attendre_etat(
        poignee: &SessionHandle,
        attendu: SessionState,
        delai: Duration,
    ) -> Vec<SessionState> {
        let mut vus = Vec::new();
        let echeance = Instant::now() + delai;
        while vus.last() != Some(&attendu) && Instant::now() < echeance {
            if let Ok(etat) = poignee.state_rx.recv_timeout(Duration::from_millis(100)) {
                vus.push(etat);
            }
        }
        vus
    }

    /// Reçoit des frames jusqu'au compte demandé (échec au-delà de l'échéance).
    fn attendre_frames(poignee: &SessionHandle, compte: usize, delai: Duration) -> usize {
        let mut recues = 0usize;
        let echeance = Instant::now() + delai;
        while recues < compte && Instant::now() < echeance {
            if let Ok(frame) = poignee.frame_rx.recv_timeout(Duration::from_millis(200)) {
                assert_eq!((frame.width, frame.height), (64, 64));
                recues += 1;
            }
        }
        recues
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
                stun_servers: vec![],
                relay: None,
            },
        );
        assert!(resultat.is_err(), "peer_id requis pour résoudre par ID");
    }

    #[test]
    fn chemin_enregistrement_suffixe_les_reprises() {
        let base = Path::new("captures/session.mp4");
        assert_eq!(chemin_enregistrement(base, 1), base);
        assert_eq!(
            chemin_enregistrement(base, 2),
            Path::new("captures/session-2.mp4")
        );
        assert_eq!(
            chemin_enregistrement(Path::new("session"), 3),
            Path::new("session-3")
        );
    }

    #[test]
    fn permissions_derivees_de_la_configuration() {
        // Sans option explicite : conversion conservatrice des six booléens
        // (défaut = observation seule, toute entrée est refusée).
        let config = config(SessionRole::Controlled, None);
        let derivees = resoudre_permissions(&config, &SessionOptions::default());
        assert!(derivees.allows(Capability::ViewScreen));
        assert!(!derivees.allows(Capability::ControlMouse));
        assert!(!derivees.allows(Capability::ControlKeyboard));

        // Une option explicite prime sur la configuration.
        let explicites: PermissionSet = [Capability::ViewScreen, Capability::ControlMouse]
            .into_iter()
            .collect();
        let options = SessionOptions {
            permissions: Some(explicites),
            ..SessionOptions::default()
        };
        assert_eq!(resoudre_permissions(&config, &options), explicites);
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
        assert_eq!(stats.reconnects, 0, "stats = {stats:?}");

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

    /// Connexion **par ID réelle** : rendez-vous éphémère, hôte synthétique en
    /// attente (`await_p2p` → QUIC sur socket percée → Noise répondeur), moteur
    /// contrôleur en [`SessionEndpoint::ByRendezvous`]. Preuve loopback du
    /// chemin punch → QUIC → Noise → média → entrées.
    #[test]
    fn moteur_controleur_par_rendezvous_bout_en_bout() {
        let rv_addr = rendezvous_ephemere();
        let id_hote = NovaId(303_030_303);
        let hote = hote_synthetique_rendezvous(rv_addr, id_hote, 1, 80, 3);

        // Reconnexion coupée : à la fin de l'hôte, la session doit se clore
        // (politique épuisée immédiatement → has_given_up → Closed).
        let options = SessionOptions {
            reconnect: ReconnectPolicy {
                max_attempts: Some(0),
                ..ReconnectPolicy::default()
            },
            ..SessionOptions::default()
        };
        let poignee = SessionEngine::start_with_options(
            config(SessionRole::Controller, Some(id_hote)),
            SessionEndpoint::ByRendezvous {
                server: rv_addr,
                stun_servers: vec![],
                relay: None,
            },
            options,
        )
        .expect("start");

        let etats = attendre_etat(&poignee, SessionState::Active, Duration::from_secs(15));
        assert_eq!(
            etats,
            vec![
                SessionState::Resolving,
                SessionState::Connecting,
                SessionState::Handshaking,
                SessionState::Active
            ],
            "erreur moteur : {:?}",
            poignee.last_error()
        );

        let frames = attendre_frames(&poignee, 5, Duration::from_secs(15));
        assert!(
            frames >= 5,
            "frames reçues = {frames} (erreur moteur : {:?})",
            poignee.last_error()
        );

        for _ in 0..3 {
            poignee
                .input_tx
                .send(InputEvent::MouseMoveRel { dx: 1.0, dy: 0.0 })
                .expect("input_tx");
        }
        let entrees = hote.join().expect("thread hôte").expect("hôte rendez-vous");
        assert!(entrees >= 3, "entrées reçues côté hôte = {entrees}");

        // L'hôte est parti et la politique interdit toute reconnexion : Closed.
        let etats = attendre_etat(&poignee, SessionState::Closed, Duration::from_secs(15));
        assert_eq!(etats.last(), Some(&SessionState::Closed));
        poignee.stop();
    }

    /// Reconnexion **contrôleur** : l'hôte synthétique sert deux époques (il
    /// coupe le lien entre les deux) ; le moteur doit passer `Reconnecting`,
    /// rétablir via `establish_p2p`, revenir `Active` et livrer de nouvelles
    /// frames. `stats().reconnects` compte la reprise.
    #[test]
    fn moteur_controleur_se_reconnecte_apres_coupure() {
        let rv_addr = rendezvous_ephemere();
        let id_hote = NovaId(404_040_404);
        let hote = hote_synthetique_rendezvous(rv_addr, id_hote, 2, 40, 0);

        let options = SessionOptions {
            reconnect: ReconnectPolicy {
                base_delay_ms: 100,
                max_delay_ms: 500,
                multiplier: 1.0,
                max_attempts: Some(40),
                jitter: false,
            },
            ..SessionOptions::default()
        };
        let poignee = SessionEngine::start_with_options(
            config(SessionRole::Controller, Some(id_hote)),
            SessionEndpoint::ByRendezvous {
                server: rv_addr,
                stun_servers: vec![],
                relay: None,
            },
            options,
        )
        .expect("start");

        // Époque 1 : Active + premières frames.
        let etats = attendre_etat(&poignee, SessionState::Active, Duration::from_secs(15));
        assert_eq!(
            etats.last(),
            Some(&SessionState::Active),
            "erreur moteur : {:?}",
            poignee.last_error()
        );
        let frames_epoque_1 = attendre_frames(&poignee, 5, Duration::from_secs(15));
        assert!(frames_epoque_1 >= 5, "frames époque 1 = {frames_epoque_1}");

        // Coupure : l'hôte clôt sa première époque → Reconnecting → Active.
        let etats = attendre_etat(
            &poignee,
            SessionState::Reconnecting,
            Duration::from_secs(20),
        );
        assert_eq!(
            etats.last(),
            Some(&SessionState::Reconnecting),
            "erreur moteur : {:?}",
            poignee.last_error()
        );
        let etats = attendre_etat(&poignee, SessionState::Active, Duration::from_secs(20));
        assert_eq!(
            etats,
            vec![SessionState::Handshaking, SessionState::Active],
            "reprise attendue après Reconnecting (erreur : {:?})",
            poignee.last_error()
        );

        // Époque 2 : le flux repart (nouvelles frames décodées).
        let frames_epoque_2 = attendre_frames(&poignee, 3, Duration::from_secs(15));
        assert!(frames_epoque_2 >= 3, "frames époque 2 = {frames_epoque_2}");
        assert_eq!(
            poignee.stats().reconnects,
            1,
            "stats = {:?}",
            poignee.stats()
        );

        let _ = hote.join().expect("thread hôte");
        poignee.stop();
    }

    /// Reconnexion **au niveau transport** ([`ReconnectingTransport`]) pour un
    /// contrôleur en [`SessionEndpoint::Direct`] : un hôte manuel sert deux
    /// connexions successives (il coupe entre les deux) ; le transport de session
    /// se rétablit **de façon transparente** (re-connexion + re-négociation
    /// Noise) et le flux reprend. Preuve : plus de frames décodées que la
    /// première connexion n'en a envoyées.
    #[test]
    fn controleur_direct_se_reconnecte_au_niveau_transport() {
        let ecouteur = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = ecouteur.local_addr();
        let cert = ecouteur.server_cert_der();

        // 1re connexion : 10 frames puis coupure ; 2e connexion : 30 frames.
        let hote = thread::spawn(move || -> Result<()> {
            for (epoque, total) in [(0usize, 10usize), (1, 30)] {
                let _ = epoque;
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
                let mut seq = 0usize;
                let echeance = Instant::now() + Duration::from_secs(10);
                while seq < total && Instant::now() < echeance {
                    let chunk = encodeur.encode(&frame_synthetique(seq), seq.is_multiple_of(20))?;
                    if chiffre
                        .send(canal, chunk.data, Reliability::UnreliableFec)
                        .is_err()
                    {
                        break;
                    }
                    seq += 1;
                    thread::sleep(Duration::from_millis(12));
                }
                // Chute du transport → coupure vue par le contrôleur (reconnexion).
                drop(chiffre);
            }
            Ok(())
        });

        let options = SessionOptions {
            transport_reconnect: true,
            reconnect: ReconnectPolicy {
                base_delay_ms: 50,
                max_delay_ms: 200,
                multiplier: 1.0,
                max_attempts: Some(100),
                jitter: false,
            },
            ..SessionOptions::default()
        };
        let poignee = SessionEngine::start_with_options(
            config(SessionRole::Controller, Some(NovaId(202_020_202))),
            SessionEndpoint::Direct {
                addr,
                cert_der: cert,
            },
            options,
        )
        .expect("start");

        let etats = attendre_etat(&poignee, SessionState::Active, Duration::from_secs(15));
        assert_eq!(
            etats.last(),
            Some(&SessionState::Active),
            "erreur moteur : {:?}",
            poignee.last_error()
        );

        // La 1re connexion n'envoie que 10 frames : dépasser ce total prouve que
        // le flux a repris sur la 2e connexion (reconnexion transport réussie).
        let total = attendre_frames(&poignee, 12, Duration::from_secs(25));
        assert!(
            total >= 12,
            "frames sur deux connexions = {total} (erreur : {:?})",
            poignee.last_error()
        );

        poignee.stop();
        let _ = hote.join().expect("thread hôte");
    }
}
