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

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nd_audio::AudioSession;
use nd_capture::{create_capturer, enumerate_monitors, Rect};
use nd_codec::{
    create_decoder, create_encoder, create_hardware_encoder, CodecKind, ContentProfile,
    DecodedFrame, EncodedChunk, VideoDecoder, VideoEncoder,
};
use nd_crypto::{generate_static_keypair, HandshakeRole};
use nd_features::{
    AnnotationLayer, Capability, HostAction, Hotkey, HotkeyMap, KeyEvent, KeyState,
    PermissionBroker, PermissionSet, ReconnectController, ReconnectPolicy,
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
    decoder_audio, decoder_controle, decoder_infos_pair, decoder_moniteurs, decoder_permissions,
    decoder_qualite, decoder_region, encoder_audio, encoder_controle, encoder_infos_pair,
    encoder_moniteurs, encoder_permissions, encoder_qualite, encoder_region, Categorie,
    ChatMessage, CommandeMedia, SousTypeControle, MONITEURS_MAX,
};
use crate::p2p::{self, AttenteRendezvous};
use crate::tunnel::{EtatTunnels, TunnelHandle};
use crate::{
    apply_input, establish, EnregistrementPartage, EtatEnregistrement, EtatQualite, HostPipeline,
    HostStreamOptions, HostStreamTick, PeerInfo, RegionPartagee, RemoteMonitor, SessionConfig,
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
    /// **Raccourcis clavier hôte** (côté contrôlé) : chaque événement clavier
    /// autorisé par les permissions passe par [`HotkeyMap::action_for`] **avant**
    /// injection — un appui correspondant déclenche l'action ([`HostAction`],
    /// ex. [`HostAction::ReleaseMouse`]) au lieu d'être injecté comme frappe, et
    /// il est compté dans [`SessionStats::hotkeys_applied`]. `None` = table par
    /// défaut ([`raccourcis_hote_defaut`]) ; `Some(HotkeyMap::new())` (table
    /// vide) coupe la fonction.
    pub hotkeys: Option<HotkeyMap<HostAction>>,
}

impl Default for SessionOptions {
    /// Permissions dérivées de la configuration, pas d'enregistrement, ABR en
    /// profil bureautique, delta **actif**, reconnexion par défaut, canaux
    /// annexes **coupés** (session vidéo + entrées historique), raccourcis
    /// clavier hôte par défaut ([`raccourcis_hote_defaut`]).
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
            hotkeys: None,
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
    media: &Arc<EtatMedia>,
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
        // Bascule moniteur, confidentialité (cadre noir), cadre d'écran, préréglage
        // de qualité et enregistrement à chaud ne sont pilotables que dans la boucle
        // **étendue** (le plan de contrôle fiable hôte → contrôleur y est branché).
        monitor_switch: etendu.then(|| Arc::clone(&media.moniteur)),
        privacy: etendu.then(|| Arc::clone(&media.privacy)),
        region_switch: etendu.then(|| Arc::clone(&media.region)),
        quality: etendu.then(|| Arc::clone(&media.qualite)),
        recording_switch: etendu.then(|| Arc::clone(&media.enregistrement)),
    }
}

// ---------------------------------------------------------------------------
// Raccourcis clavier hôte : résolution AVANT injection (plan 13)
// ---------------------------------------------------------------------------

// Scancodes (jeu 1, préfixe `0xE0` pour les touches étendues) suivis par le
// filtre de raccourcis — la convention du protocole d'entrées
// (`nd_proto::InputEvent::Key`), celle que l'injection Windows rejoue telle
// quelle (`SendInput` + `KEYEVENTF_SCANCODE`).
/// Ctrl gauche.
const SCAN_CTRL_GAUCHE: u32 = 0x1D;
/// Ctrl droit (étendu).
const SCAN_CTRL_DROIT: u32 = 0xE01D;
/// Maj gauche.
const SCAN_MAJ_GAUCHE: u32 = 0x2A;
/// Maj droite.
const SCAN_MAJ_DROITE: u32 = 0x36;
/// Alt gauche.
const SCAN_ALT_GAUCHE: u32 = 0x38;
/// Alt droit / AltGr (étendu).
const SCAN_ALT_DROIT: u32 = 0xE038;
/// Win gauche (étendu).
const SCAN_WIN_GAUCHE: u32 = 0xE05B;
/// Win droit (étendu).
const SCAN_WIN_DROIT: u32 = 0xE05C;
/// Touche `M` — `Ctrl+Alt+M` libère la souris ([`HostAction::ReleaseMouse`]).
const SCAN_M: u32 = 0x32;
/// Touche `Fin` (étendue) — `Ctrl+Alt+Fin` envoie Ctrl+Alt+Suppr, comme le
/// client Bureau à distance de Windows.
const SCAN_FIN: u32 = 0xE04F;

/// Bit de modificateur ([`Hotkey`]) porté par un scancode, si c'en est un.
fn bit_modificateur(scancode: u32) -> Option<u8> {
    match scancode {
        SCAN_CTRL_GAUCHE | SCAN_CTRL_DROIT => Some(Hotkey::CTRL),
        SCAN_MAJ_GAUCHE | SCAN_MAJ_DROITE => Some(Hotkey::SHIFT),
        SCAN_ALT_GAUCHE | SCAN_ALT_DROIT => Some(Hotkey::ALT),
        SCAN_WIN_GAUCHE | SCAN_WIN_DROIT => Some(Hotkey::WIN),
        _ => None,
    }
}

/// Table de raccourcis clavier **hôte** par défaut, appliquée quand
/// [`SessionOptions::hotkeys`] vaut `None` (et par [`crate::UnattendedHost`]) :
///
/// * `Ctrl+Alt+M` → [`HostAction::ReleaseMouse`] : geste hôte « tout
///   relâcher » (touches et boutons injectés), l'anti « souris capturée » ;
/// * `Ctrl+Alt+Fin` → [`HostAction::SendCtrlAltDel`] : la convention du client
///   Bureau à distance de Windows (voie SAS côté hôte, best-effort).
///
/// Volontairement minimale : chaque lien par défaut masque la combinaison
/// correspondante pour les applications du poste distant. Les autres actions
/// ([`HostAction::ToggleViewOnly`], [`HostAction::Disconnect`], …) se câblent
/// via une table personnalisée ([`SessionOptions::hotkeys`]).
#[must_use]
pub fn raccourcis_hote_defaut() -> HotkeyMap<HostAction> {
    let mut carte = HotkeyMap::new();
    carte.bind(
        Hotkey::new(Hotkey::CTRL | Hotkey::ALT, SCAN_M),
        HostAction::ReleaseMouse,
    );
    carte.bind(
        Hotkey::new(Hotkey::CTRL | Hotkey::ALT, SCAN_FIN),
        HostAction::SendCtrlAltDel,
    );
    carte
}

/// Table de raccourcis hôte effective : celle des options si fournie, sinon la
/// table par défaut ([`raccourcis_hote_defaut`]).
fn resoudre_raccourcis(options: &SessionOptions) -> HotkeyMap<HostAction> {
    options
        .hotkeys
        .clone()
        .unwrap_or_else(raccourcis_hote_defaut)
}

/// Issue du filtre de raccourcis pour un événement d'entrée **autorisé**.
enum FiltrageEntree {
    /// L'événement suit le chemin normal d'injection vers l'OS.
    Injecter,
    /// Événement avalé sans action : répétition ou relâchement d'une touche
    /// dont l'appui a déclenché un raccourci (jamais de frappe orpheline).
    Avale,
    /// Un raccourci a déclenché cette action hôte : l'appliquer, ne pas injecter.
    Action(HostAction),
}

/// Filtre de raccourcis hôte : suit les **modificateurs** au fil du flux
/// d'entrées (le protocole ne porte que des scancodes), résout chaque appui via
/// [`HotkeyMap::action_for`] **avant** injection, et avale la frappe d'un
/// raccourci déclenché (appui, répétitions, relâchement).
struct FiltreRaccourcis {
    carte: HotkeyMap<HostAction>,
    /// Scancodes de modificateurs actuellement enfoncés — gauche et droite
    /// suivis séparément : relâcher l'un ne masque pas l'autre.
    modificateurs_tenus: HashSet<u32>,
    /// Touches dont l'appui a déclenché une action : leurs répétitions et leur
    /// relâchement sont avalés (l'hôte ne voit jamais la frappe du raccourci).
    avalees: HashSet<u32>,
}

impl FiltreRaccourcis {
    fn new(carte: HotkeyMap<HostAction>) -> Self {
        FiltreRaccourcis {
            carte,
            modificateurs_tenus: HashSet::new(),
            avalees: HashSet::new(),
        }
    }

    /// Bits de modificateurs actuellement actifs (convention [`Hotkey`]).
    fn modificateurs(&self) -> u8 {
        self.modificateurs_tenus
            .iter()
            .filter_map(|&scan| bit_modificateur(scan))
            .fold(0, |bits, bit| bits | bit)
    }

    /// Note l'appui/relâchement d'un éventuel modificateur.
    fn noter_modificateur(&mut self, scancode: u32, down: bool) {
        if bit_modificateur(scancode).is_some() {
            if down {
                self.modificateurs_tenus.insert(scancode);
            } else {
                self.modificateurs_tenus.remove(&scancode);
            }
        }
    }

    /// Filtre un événement d'entrée : résout les raccourcis **avant** injection.
    ///
    /// Les modificateurs sont ceux tenus *avant* l'événement : `Ctrl+Alt+M` se
    /// déclenche à l'appui de `M` avec Ctrl et Alt déjà enfoncés (leurs appuis
    /// ont suivi le chemin normal — leurs relâchements aussi, rien ne reste
    /// coincé). Seuls les événements clavier sont concernés : souris, molette et
    /// Unicode (aucun scancode) suivent toujours l'injection normale.
    fn filtrer(&mut self, evenement: &InputEvent) -> FiltrageEntree {
        let InputEvent::Key { scancode, down } = *evenement else {
            return FiltrageEntree::Injecter;
        };
        // Répétition (appui maintenu) ou relâchement d'une touche consommée :
        // avalé sans re-déclencher (un raccourci ne tire qu'une fois par appui).
        if self.avalees.contains(&scancode) {
            if !down {
                self.avalees.remove(&scancode);
            }
            self.noter_modificateur(scancode, down);
            return FiltrageEntree::Avale;
        }
        let etat = if down {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let action = self.carte.action_for(KeyEvent {
            modifiers: self.modificateurs(),
            key: scancode,
            state: etat,
        });
        self.noter_modificateur(scancode, down);
        match action {
            // `action_for` ne se déclenche que sur l'appui : la touche est
            // marquée consommée jusqu'à son relâchement.
            Some(action) => {
                self.avalees.insert(scancode);
                FiltrageEntree::Action(action)
            }
            None => FiltrageEntree::Injecter,
        }
    }
}

/// Guichet du chemin chaud d'injection côté **contrôlé** : permissions →
/// raccourcis → (lecture seule) → injection. Partagé par la boucle historique
/// ([`executer_hote`]) et le démux étendu ([`recepteur_hote`]) — et donc par le
/// service [`crate::UnattendedHost`].
struct GuichetEntrees {
    /// Guichet de permissions (chemin chaud sans verrou, journal au premier refus).
    broker: PermissionBroker,
    /// Capacités dont un refus a déjà été journalisé.
    refus_journalises: PermissionSet,
    /// Filtre de raccourcis hôte (résolution avant injection).
    filtre: FiltreRaccourcis,
    /// Mode **lecture seule** basculé par [`HostAction::ToggleViewOnly`] : les
    /// entrées (hors raccourcis) sont refusées — comptées `inputs_denied` —
    /// tant qu'il est actif. Les raccourcis restent résolus, sinon la bascule
    /// serait sans retour.
    lecture_seule: bool,
    /// ID du pair contrôleur (acteur du journal d'audit).
    acteur: String,
    /// Arrêt de l'époque courante, toujours levé par [`HostAction::Disconnect`].
    arret_epoque: Arc<AtomicBool>,
    /// Arrêt **global** de la session, levé en plus par
    /// [`HostAction::Disconnect`] quand le propriétaire veut qu'un raccourci
    /// clôture toute la session (moteur de session) ; `None` pour l'hôte non
    /// surveillé, qui survit à ses sessions et retourne à l'attente.
    stop_session: Option<Arc<AtomicBool>>,
    /// Permissions **vivantes** partagées (renégociation à chaud) : lues sans
    /// verrou avant chaque entrée pour réaligner le guichet. `None` = permissions
    /// figées à l'ensemble initial (mode non étendu, ou hôte non surveillé).
    permissions_live: Option<Arc<AtomicU16>>,
}

impl GuichetEntrees {
    /// Construit le guichet d'une époque hôte. `arret_epoque` est le drapeau
    /// d'arrêt de l'époque (celui des boucles média). Les permissions sont figées
    /// à l'ensemble initial ; le mode étendu y branche ensuite l'ensemble vivant
    /// via [`GuichetEntrees::brancher_permissions_vivantes`].
    fn new(params: &ParamsEpoqueHote<'_>, arret_epoque: Arc<AtomicBool>) -> Self {
        GuichetEntrees {
            broker: PermissionBroker::with_permissions(params.permissions),
            refus_journalises: PermissionSet::none(),
            filtre: FiltreRaccourcis::new(params.raccourcis.clone()),
            lecture_seule: false,
            acteur: params.pair.to_string(),
            arret_epoque,
            stop_session: params.deconnexion_globale.then(|| Arc::clone(params.stop)),
            permissions_live: None,
        }
    }

    /// Branche l'ensemble de permissions **vivant** partagé (renégociation à
    /// chaud) : à chaque entrée, le guichet s'y réaligne. Réservé au mode étendu,
    /// où le canal `Control` porte [`SousTypeControle::MajPermissions`].
    fn brancher_permissions_vivantes(&mut self, permissions: Arc<AtomicU16>) {
        self.permissions_live = Some(permissions);
    }

    /// Traite une trame du canal `Input` : décodage, **filtre de permissions**,
    /// **raccourcis hôte** (avant injection), verrou lecture seule, injection.
    /// Alimente les compteurs (`inputs_applied`, `inputs_denied`,
    /// `hotkeys_applied`).
    fn traiter(
        &mut self,
        injecteur: &dyn InputInjector,
        compteurs: &CompteursSession,
        data: &[u8],
    ) {
        // Renégociation à chaud : réaligne le guichet sur les permissions
        // vivantes (lecture atomique lock-free ; le broker n'est réécrit qu'en
        // cas de changement, pour ne pas gonfler le journal d'audit). Un nouvel
        // ensemble ré-arme la journalisation des refus (traçe le prochain blocage).
        if let Some(live) = &self.permissions_live {
            let vivantes = PermissionSet::from_bits(live.load(Ordering::Relaxed));
            if vivantes != self.broker.permissions() {
                self.broker.set_permissions(vivantes);
                self.refus_journalises = PermissionSet::none();
            }
        }
        let Some(evenement) = InputEvent::from_bytes(data) else {
            return;
        };
        // Permissions d'abord : une entrée refusée ne peut pas non plus
        // déclencher d'action hôte (jetée, comptée, journalisée au premier refus).
        let capacite = Capability::required_for_input(&evenement);
        if !self.broker.is_allowed(capacite) {
            compteurs.inputs_denied.fetch_add(1, Ordering::Relaxed);
            if self.refus_journalises.grant(capacite) {
                let _ = self.broker.authorize_input(&self.acteur, &evenement);
            }
            return;
        }
        match self.filtre.filtrer(&evenement) {
            FiltrageEntree::Action(action) => {
                self.appliquer_action(action, injecteur);
                compteurs
                    .raccourcis_appliques
                    .fetch_add(1, Ordering::Relaxed);
            }
            FiltrageEntree::Avale => {}
            FiltrageEntree::Injecter => {
                if self.lecture_seule {
                    // Lecture seule : refus **doux** (réversible par raccourci),
                    // compté avec les refus de permissions.
                    compteurs.inputs_denied.fetch_add(1, Ordering::Relaxed);
                } else if apply_input(injecteur, &evenement).is_ok() {
                    compteurs.inputs_applied.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Applique l'**effet moteur** d'une action de raccourci côté hôte. La
    /// frappe correspondante n'est jamais injectée (consommée par le filtre).
    fn appliquer_action(&mut self, action: HostAction, injecteur: &dyn InputInjector) {
        match action {
            // Geste hôte « libérer la souris » : tout relâcher (boutons et
            // touches injectés) — le curseur n'est plus tenu par la session et
            // les modificateurs du raccourci ne restent pas coincés.
            HostAction::ReleaseMouse => injecteur.release_all(),
            // Bascule lecture seule : voir [`GuichetEntrees::lecture_seule`].
            // En entrant en lecture seule, tout est relâché (aucune touche ne
            // reste tenue pendant le gel des entrées).
            HostAction::ToggleViewOnly => {
                self.lecture_seule = !self.lecture_seule;
                if self.lecture_seule {
                    injecteur.release_all();
                }
            }
            // Fin de session : l'époque s'arrête toujours ; la session entière
            // se clôt quand le propriétaire l'a demandé (moteur de session).
            // L'hôte non surveillé, lui, survit et retourne à l'attente.
            HostAction::Disconnect => {
                if let Some(stop) = &self.stop_session {
                    stop.store(true, Ordering::Relaxed);
                }
                self.arret_epoque.store(true, Ordering::Relaxed);
            }
            // Ctrl+Alt+Suppr : voie SAS de Windows (`SendSAS`), best-effort —
            // sans service SYSTEM ni stratégie `SoftwareSASGeneration`, l'OS
            // ignore l'appel (voir `nd_input::send_secure_attention_sequence`).
            // Ailleurs : compté mais sans voie d'injection (le SAS est un
            // mécanisme Windows).
            HostAction::SendCtrlAltDel => {
                #[cfg(windows)]
                let _ = nd_input::send_secure_attention_sequence();
            }
            // Actions d'IHM du contrôleur (plein écran, capture d'écran,
            // enregistrement — l'UI tient déjà les frames décodées et la
            // commande d'enregistrement) ou exigeant des crochets OS non
            // disponibles sans droits (blocage des entrées locales) : la frappe
            // est consommée et comptée, l'effet visuel appartient à l'UI (plan 10).
            HostAction::ToggleFullscreen
            | HostAction::TakeScreenshot
            | HostAction::ToggleRecording
            | HostAction::ToggleInputBlock => {}
        }
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
    /// Entrées reçues mais **refusées** avant injection (contrôlé) : permissions
    /// insuffisantes, ou mode lecture seule basculé par le raccourci
    /// [`HostAction::ToggleViewOnly`] — jetées silencieusement, voir plan 13.
    pub inputs_denied: u64,
    /// Raccourcis clavier hôte **déclenchés et appliqués** (contrôlé) : la
    /// frappe correspondante est consommée — jamais injectée — et l'action
    /// ([`HostAction`]) exécutée. Voir [`SessionOptions::hotkeys`].
    pub hotkeys_applied: u64,
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
    raccourcis_appliques: AtomicU64,
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
            hotkeys_applied: self.raccourcis_appliques.load(Ordering::Relaxed),
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
    /// Couches d'**annotation / tableau blanc** *à émettre* vers le pair (canal
    /// `Control`). Raccourci : [`SessionHandle::send_annotation`]. Sans effet
    /// hors mode étendu ([`SessionOptions::extended_features`]).
    pub annotation_tx: Sender<AnnotationLayer>,
    /// Couches d'**annotation / tableau blanc** *reçues* du pair (canal
    /// `Control`), à superposer à l'image (voir [`AnnotationLayer::render`]).
    pub annotation_rx: Receiver<AnnotationLayer>,
    /// Commandes vers les threads média (envoi de fichiers, bascule moniteur,
    /// activation audio, confidentialité, région) — voir
    /// [`SessionHandle::send_files`], [`SessionHandle::switch_monitor`],
    /// [`SessionHandle::set_audio_enabled`], [`SessionHandle::set_privacy`],
    /// [`SessionHandle::set_region`].
    commandes_tx: Sender<CommandeMedia>,
    /// État du mode confidentialité **connu localement** : côté hôte, le rideau
    /// qu'il applique ; côté contrôleur, le dernier drapeau annoncé par l'hôte
    /// (l'indicateur à afficher). Lu par [`SessionHandle::privacy_active`].
    privacy: Arc<AtomicBool>,
    /// Cadre d'écran demandé (partagé avec la boucle de diffusion hôte). Lu par
    /// [`SessionHandle::requested_region`].
    region: RegionPartagee,
    /// État des tunnels TCP de session (voir [`SessionHandle::open_tunnel`]).
    tunnels: Arc<EtatTunnels>,
    /// Permissions **vivantes** de la session (bits partagés) : côté hôte,
    /// l'ensemble appliqué par le filtre d'injection (renégocié à chaud) ; côté
    /// contrôleur, l'ensemble initial. Lu par [`SessionHandle::current_permissions`].
    permissions: Arc<AtomicU16>,
    /// Préréglage de qualité appliqué (partagé avec la boucle de diffusion hôte).
    /// Lu par [`SessionHandle::quality`], piloté par [`SessionHandle::set_quality`].
    qualite: Arc<EtatQualite>,
    /// Liste des moniteurs publiée par l'hôte (contrôleur). Lu par
    /// [`SessionHandle::monitors`].
    moniteurs_recus: Arc<Mutex<Option<Vec<RemoteMonitor>>>>,
    /// Infos système du pair (contrôleur). Lu par [`SessionHandle::peer_info`].
    infos_pair_recues: Arc<Mutex<Option<PeerInfo>>>,
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

    /// Envoie une couche d'**annotation / tableau blanc** au pair (canal
    /// `Control`). Raccourci autour de [`SessionHandle::annotation_tx`] ; les
    /// couches reçues arrivent sur [`SessionHandle::annotation_rx`]. Sans effet
    /// hors mode étendu.
    pub fn send_annotation(&self, couche: AnnotationLayer) {
        let _ = self.annotation_tx.send(couche);
    }

    /// Demande l'activation (ou la levée) du **mode confidentialité**. Côté
    /// contrôleur, une demande est transmise à l'hôte, qui — s'il détient
    /// [`Capability::PrivacyMode`] — cesse de diffuser son écran réel (cadre
    /// noir) et renvoie son état ; côté hôte, le rideau est appliqué directement.
    /// L'état effectif se lit via [`SessionHandle::privacy_active`]. Sans effet
    /// hors mode étendu.
    pub fn set_privacy(&self, actif: bool) {
        let _ = self
            .commandes_tx
            .send(CommandeMedia::Confidentialite(actif));
    }

    /// État du mode confidentialité connu localement : côté contrôleur, le
    /// dernier drapeau annoncé par l'hôte (l'indicateur à afficher) ; côté hôte,
    /// le rideau qu'il applique.
    #[must_use]
    pub fn privacy_active(&self) -> bool {
        self.privacy.load(Ordering::Relaxed)
    }

    /// Restreint la zone d'écran partagée à `Some((x, y, largeur, hauteur))` (en
    /// pixels du moniteur) — le « cadre d'écran » — ou rétablit le plein écran
    /// avec `None`. Côté contrôleur, la demande est transmise à l'hôte, qui
    /// l'applique au mieux ([`crate::HostPipeline`] → `set_region`) ; côté hôte,
    /// elle est appliquée directement. Sans effet hors mode étendu.
    pub fn set_region(&self, region: Option<(u32, u32, u32, u32)>) {
        let rect = region.map(|(x, y, w, h)| Rect { x, y, w, h });
        let _ = self.commandes_tx.send(CommandeMedia::DefinirRegion(rect));
    }

    /// Cadre d'écran actuellement demandé (`None` = plein écran). Côté hôte,
    /// reflète la demande reçue du contrôleur ; utile pour prouver qu'une
    /// commande de région a bien traversé la session.
    #[must_use]
    pub fn requested_region(&self) -> Option<(u32, u32, u32, u32)> {
        self.region
            .lock()
            .expect("verrou du cadre d'écran")
            .map(|r| (r.x, r.y, r.w, r.h))
    }

    /// Renégocie les **permissions à chaud** : côté contrôleur, une demande de
    /// remplacement de l'ensemble accordé est transmise à l'hôte, qui l'applique
    /// au vol — le filtre d'injection lit le nouvel ensemble à l'entrée suivante ;
    /// côté hôte, l'ensemble vivant est remplacé directement. L'ensemble effectif
    /// se relit via [`SessionHandle::current_permissions`]. Sans effet hors mode
    /// étendu ([`SessionOptions::extended_features`]).
    pub fn set_permissions(&self, permissions: PermissionSet) {
        let _ = self
            .commandes_tx
            .send(CommandeMedia::MajPermissions(permissions));
    }

    /// Permissions **vivantes** connues localement : côté hôte, l'ensemble
    /// effectivement appliqué par le filtre d'injection (après renégociation à
    /// chaud) ; côté contrôleur, l'ensemble de la **dernière** renégociation
    /// émise (écho optimiste local — les bascules incrémentales se composent).
    #[must_use]
    pub fn current_permissions(&self) -> PermissionSet {
        PermissionSet::from_bits(self.permissions.load(Ordering::Relaxed))
    }

    /// Applique un **préréglage de qualité** : `profil` ABR (netteté vs fluidité)
    /// et `plafond_kbps` (plafond de débit ; `0` = aucun). Côté contrôleur, une
    /// demande est transmise à l'hôte, qui reconfigure son encodeur et son échelle
    /// ABR **sous** le plafond (l'ABR continue de dégrader à partir de là) ; côté
    /// hôte, le préréglage est appliqué directement. L'état appliqué se relit via
    /// [`SessionHandle::quality`]. Sans effet hors mode étendu.
    pub fn set_quality(&self, profil: ContentProfile, plafond_kbps: u32) {
        let _ = self
            .commandes_tx
            .send(CommandeMedia::MajQualite(profil, plafond_kbps));
    }

    /// Préréglage de qualité **appliqué** (profil ABR, plafond kbit/s) connu
    /// localement : côté hôte, ce que la boucle de diffusion applique ; côté
    /// contrôleur, le dernier préréglage **demandé** (écho optimiste local).
    #[must_use]
    pub fn quality(&self) -> (ContentProfile, u32) {
        (
            self.qualite.profil(),
            self.qualite.plafond_kbps.load(Ordering::Relaxed),
        )
    }

    /// Démarre (avec un chemin MP4) ou arrête (`None`) l'**enregistrement local
    /// en cours de session** — côté **hôte** (l'hôte encode et muxe son écran).
    /// Démarrer ouvre une nouvelle époque MP4 ; arrêter clôt proprement le
    /// fichier (relisible). Sans effet côté contrôleur ni hors mode étendu.
    pub fn set_recording(&self, chemin: Option<PathBuf>) {
        let _ = self
            .commandes_tx
            .send(CommandeMedia::DefinirEnregistrement(chemin));
    }

    /// Liste des **moniteurs** publiée par l'hôte, lue côté **contrôleur** :
    /// `None` tant qu'aucune liste n'est arrivée, `Some(liste)` ensuite
    /// (éventuellement vide sur un hôte sans écran énumérable). Chaque index est
    /// celui qu'attend [`SessionHandle::switch_monitor`] — remplace tout écran
    /// codé en dur côté UI.
    #[must_use]
    pub fn monitors(&self) -> Option<Vec<RemoteMonitor>> {
        self.moniteurs_recus
            .lock()
            .expect("verrou des moniteurs")
            .clone()
    }

    /// **Infos système du pair** (nom d'hôte + OS) publiées par l'hôte, lues côté
    /// **contrôleur** : `None` tant qu'elles ne sont pas arrivées.
    #[must_use]
    pub fn peer_info(&self) -> Option<PeerInfo> {
        self.infos_pair_recues
            .lock()
            .expect("verrou des infos du pair")
            .clone()
    }

    /// Ouvre un **tunnel TCP de session** : écoute sur `127.0.0.1:port_local`
    /// (port `0` = éphémère) et relaie chaque connexion locale vers
    /// `cible_distante` **à travers le canal fiable de la session** (l'hôte
    /// compose la connexion réelle vers la cible). Rend une [`TunnelHandle`]
    /// (adresse écoutée, statistiques, arrêt).
    ///
    /// Best-effort (voir [`crate::tunnel`]) : exige le mode étendu et
    /// [`Capability::TcpTunnel`] côté hôte ; les octets relayés sont comptés
    /// dans [`TunnelHandle::stats`].
    ///
    /// # Errors
    /// Échec de liaison de l'écouteur local (port déjà pris, droits…).
    pub fn open_tunnel(&self, port_local: u16, cible_distante: SocketAddr) -> Result<TunnelHandle> {
        crate::tunnel::open_tunnel(&self.tunnels, port_local, cible_distante)
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
        let (annotation_in_tx, annotation_in_rx) = mpsc::channel();
        let (annotation_out_tx, annotation_out_rx) = mpsc::channel();
        let (commandes_tx, commandes_rx) = mpsc::channel();
        let compteurs = Arc::new(CompteursSession::default());
        let stop = Arc::new(AtomicBool::new(false));
        // État partagé des fonctions avancées (confidentialité, cadre d'écran,
        // tunnels), lisible/pilotable depuis la poignée.
        let privacy = Arc::new(AtomicBool::new(false));
        let region: RegionPartagee = Arc::new(Mutex::new(None));
        let tunnels = Arc::new(EtatTunnels::new(Arc::clone(&stop)));
        // Plan de contrôle de session : permissions vivantes (bits partagés),
        // préréglage de qualité, enregistrement à chaud, liste des moniteurs et
        // infos du pair — partagés entre les threads média et la poignée.
        let permissions = Arc::new(AtomicU16::new(
            resoudre_permissions(&config, &options).to_bits(),
        ));
        let qualite = Arc::new(EtatQualite::default());
        // La qualité initiale reflète le profil ABR des options (netteté/fluidité).
        qualite.profil_video.store(
            matches!(options.abr_profile, ContentProfile::Video),
            Ordering::Relaxed,
        );
        let enregistrement: EnregistrementPartage = Arc::new(EtatEnregistrement::default());
        let moniteurs_recus = Arc::new(Mutex::new(None));
        let infos_pair_recues = Arc::new(Mutex::new(None));

        let media = Arc::new(EtatMedia {
            role: config.role,
            permissions: Arc::clone(&permissions),
            transfer_dir: options
                .transfer_dir
                .clone()
                .unwrap_or_else(std::env::temp_dir),
            audio: Mutex::new(injectees.audio),
            clipboard: Mutex::new(injectees.clipboard),
            transfer: Mutex::new(None),
            audio_actif: AtomicBool::new(true),
            moniteur: Arc::new(AtomicU32::new(0)),
            privacy: Arc::clone(&privacy),
            region: Arc::clone(&region),
            dernier_presse_papiers: Mutex::new(None),
            chat_out: chat_out_tx,
            transfer_out: transfer_out_tx,
            annotation_out: annotation_out_tx,
            commandes: Mutex::new(commandes_rx),
            chat_in: Mutex::new(chat_in_rx),
            annotation_in: Mutex::new(annotation_in_rx),
            tunnels: Arc::clone(&tunnels),
            qualite: Arc::clone(&qualite),
            enregistrement: Arc::clone(&enregistrement),
            moniteurs_recus: Arc::clone(&moniteurs_recus),
            infos_pair_recues: Arc::clone(&infos_pair_recues),
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
            annotation_tx: annotation_in_tx,
            annotation_rx: annotation_out_rx,
            commandes_tx,
            privacy,
            region,
            tunnels,
            permissions,
            qualite,
            moniteurs_recus,
            infos_pair_recues,
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
    /// Capacités **vivantes** (bits partagés) : gate de chaque fonction étendue,
    /// **renégociable à chaud** (le contrôleur retire/rend un droit, l'hôte
    /// l'applique au vol via [`SousTypeControle::MajPermissions`]). Lue sans
    /// verrou par le filtre d'injection et les gardes média (voir [`Self::perms`]).
    permissions: Arc<AtomicU16>,
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
    /// État du **mode confidentialité** : hôte → rideau appliqué (lu par la
    /// boucle de diffusion) ; contrôleur → dernier état annoncé par l'hôte.
    /// Partagé avec la [`SessionHandle`] (indicateur).
    privacy: Arc<AtomicBool>,
    /// **Cadre d'écran** demandé (partagé avec la boucle de diffusion hôte et la
    /// [`SessionHandle`]). `None` = plein écran.
    region: RegionPartagee,
    /// Dernier presse-papiers **appliqué** (anti-boucle : ne pas ré-émettre ce
    /// que l'on vient de recevoir).
    dernier_presse_papiers: Mutex<Option<Vec<u8>>>,
    /// Chat reçu + échos → [`SessionHandle::chat_rx`].
    chat_out: Sender<ChatMessage>,
    /// Progression des transferts → [`SessionHandle::transfer_rx`].
    transfer_out: Sender<TransferEvent>,
    /// Annotations reçues → [`SessionHandle::annotation_rx`].
    annotation_out: Sender<AnnotationLayer>,
    /// Commandes depuis la poignée ([`SessionHandle::send_files`], …).
    commandes: Mutex<Receiver<CommandeMedia>>,
    /// Chat à émettre depuis la poignée ([`SessionHandle::chat_tx`]).
    chat_in: Mutex<Receiver<String>>,
    /// Annotations à émettre depuis la poignée ([`SessionHandle::annotation_tx`]).
    annotation_in: Mutex<Receiver<AnnotationLayer>>,
    /// État des tunnels TCP de session (partagé avec la [`SessionHandle`]).
    tunnels: Arc<EtatTunnels>,
    /// Préréglage de **qualité** partagé avec la boucle de diffusion hôte
    /// (profil ABR + plafond de débit) — renégociable à chaud.
    qualite: Arc<EtatQualite>,
    /// Demande d'**enregistrement à chaud** partagée avec la boucle de diffusion
    /// hôte (chemin MP4 voulu / arrêt).
    enregistrement: EnregistrementPartage,
    /// Liste des **moniteurs** reçue du pair (contrôleur) — `None` tant que
    /// l'hôte ne l'a pas annoncée. Partagée avec la [`SessionHandle`].
    moniteurs_recus: Arc<Mutex<Option<Vec<RemoteMonitor>>>>,
    /// **Infos système du pair** reçues (contrôleur) — `None` tant qu'inconnues.
    /// Partagées avec la [`SessionHandle`].
    infos_pair_recues: Arc<Mutex<Option<PeerInfo>>>,
}

impl EtatMedia {
    /// Permissions **vivantes** courantes (lecture atomique lock-free), telles
    /// que renégociées à chaud. Gate de toutes les fonctions étendues.
    fn perms(&self) -> PermissionSet {
        PermissionSet::from_bits(self.permissions.load(Ordering::Relaxed))
    }
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
                flux: options_flux_hote(&ctx.options, epoque, &ctx.media),
                compteurs: &ctx.compteurs,
                stop: &ctx.stop,
                etats: Some(&ctx.state_tx),
                pair,
                raccourcis: resoudre_raccourcis(&ctx.options),
                deconnexion_globale: true,
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
    /// Raccourcis clavier hôte, résolus **avant** injection (voir
    /// [`SessionOptions::hotkeys`] et [`raccourcis_hote_defaut`]).
    pub raccourcis: HotkeyMap<HostAction>,
    /// [`HostAction::Disconnect`] lève aussi le signal `stop` global quand vrai
    /// (moteur de session : toute la session se clôt) ; faux pour l'hôte non
    /// surveillé, qui survit à ses sessions (seule l'époque se termine).
    pub deconnexion_globale: bool,
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
    // Le guichet (permissions + raccourcis hôte + lecture seule) vit dans ce
    // thread — chemin chaud sans verrou, journalisation au premier refus par
    // capacité, résolution des raccourcis **avant** injection.
    let mut guichet = GuichetEntrees::new(params, Arc::clone(arret));
    let mut transport_entrees = transport.clone();
    let arret_entrees = Arc::clone(arret);
    let compteurs_entrees = Arc::clone(params.compteurs);
    let entrees = thread::Builder::new()
        .name("nd-session-injection".to_owned())
        .spawn(move || {
            while !arret_entrees.load(Ordering::Relaxed) {
                match transport_entrees.poll_recv() {
                    Ok(Some((_canal, donnees))) => {
                        guichet.traiter(injecteur.as_ref(), &compteurs_entrees, &donnees);
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

/// Construit (paresseusement) la session audio système si [`Capability::Audio`]
/// est accordé, puis règle le sens actif selon le rôle (émission côté hôte,
/// lecture côté contrôleur).
fn assurer_audio(media: &EtatMedia) {
    if !media.perms().allows(Capability::Audio) {
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
    if !media.perms().allows(Capability::FileDownload) {
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
            if !media.perms().allows(Capability::ClipboardWrite) {
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
        SousTypeControle::Confidentialite => {
            // Demande du contrôleur : l'hôte applique le rideau **s'il l'autorise**
            // ([`Capability::PrivacyMode`], défense en profondeur côté contrôlé).
            if media.role == SessionRole::Controlled
                && media.perms().allows(Capability::PrivacyMode)
            {
                if let Some(&octet) = payload.first() {
                    media.privacy.store(octet != 0, Ordering::Relaxed);
                }
            }
        }
        SousTypeControle::ConfidentialiteEtat => {
            // État annoncé par l'hôte : le contrôleur met à jour son indicateur.
            if media.role == SessionRole::Controller {
                if let Some(&octet) = payload.first() {
                    media.privacy.store(octet != 0, Ordering::Relaxed);
                }
            }
        }
        SousTypeControle::Annotation => {
            if let Ok(couche) = AnnotationLayer::from_bytes(payload) {
                let _ = media.annotation_out.send(couche);
            }
        }
        SousTypeControle::Region => {
            // Demande de cadre d'écran : l'hôte la mémorise (la boucle de
            // diffusion applique `set_region` au mieux). Payload vide = plein écran.
            if media.role == SessionRole::Controlled {
                *media.region.lock().expect("verrou du cadre d'écran") = decoder_region(payload);
            }
        }
        SousTypeControle::Tunnel => {
            let autorise = media.perms().allows(Capability::TcpTunnel);
            EtatTunnels::recevoir(&media.tunnels, payload, media.role, autorise);
        }
        SousTypeControle::MajPermissions => {
            // Renégociation à chaud : l'hôte remplace son ensemble vivant, relu
            // par le filtre d'injection et les gardes média à la volée.
            if media.role == SessionRole::Controlled {
                if let Some(nouvelles) = decoder_permissions(payload) {
                    media
                        .permissions
                        .store(nouvelles.to_bits(), Ordering::Relaxed);
                }
            }
        }
        SousTypeControle::MajQualite => {
            // Préréglage de qualité : l'hôte reconfigure encodeur + ABR sous le
            // plafond (via la génération observée par la boucle de diffusion).
            if media.role == SessionRole::Controlled {
                if let Some((profil, plafond)) = decoder_qualite(payload) {
                    appliquer_qualite(media, profil, plafond);
                }
            }
        }
        SousTypeControle::Moniteurs => {
            // Liste des écrans publiée par l'hôte : le contrôleur la mémorise
            // (remplace tout écran codé en dur côté UI).
            if media.role == SessionRole::Controller {
                *media.moniteurs_recus.lock().expect("verrou des moniteurs") =
                    Some(decoder_moniteurs(payload));
            }
        }
        SousTypeControle::InfosPair => {
            // Infos système du pair : le contrôleur les mémorise.
            if media.role == SessionRole::Controller {
                if let Some(infos) = decoder_infos_pair(payload) {
                    *media
                        .infos_pair_recues
                        .lock()
                        .expect("verrou des infos du pair") = Some(infos);
                }
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
                    if media.perms().allows(Capability::Audio)
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
            if media.perms().allows(Capability::Audio) {
                if let Some(audio) = media.audio.lock().expect("verrou audio").as_mut() {
                    let _ = audio.tick_lecture(maintenant);
                }
            }
        }
    }
    Ok(())
}

/// Récepteur démux **hôte** : entrées → guichet (permissions → raccourcis hôte
/// → injection) ; fichiers → transfert ; contrôle → presse-papiers/chat/bascule
/// moniteur.
fn recepteur_hote(
    mut transport: TransportPartage,
    injecteur: Box<dyn InputInjector>,
    mut guichet: GuichetEntrees,
    media: &Arc<EtatMedia>,
    compteurs: &Arc<CompteursSession>,
    arret: &Arc<AtomicBool>,
) -> Result<()> {
    let carte = construire_carte_reception(&mut transport);
    while !arret.load(Ordering::Relaxed) {
        match transport.poll_recv() {
            Ok(Some((handle, data))) => match carte.get(&handle.0).copied() {
                Some(Categorie::Input) => guichet.traiter(injecteur.as_ref(), compteurs, &data),
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
                if media.perms().allows(Capability::FileUpload) {
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
            CommandeMedia::Confidentialite(actif) => match media.role {
                // Le contrôleur demande ; l'hôte s'applique le rideau directement
                // (la boucle de diffusion le lit et diffuse un cadre noir).
                SessionRole::Controller => {
                    let trame =
                        encoder_controle(SousTypeControle::Confidentialite, &[u8::from(actif)]);
                    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
                }
                SessionRole::Controlled => media.privacy.store(actif, Ordering::Relaxed),
            },
            CommandeMedia::DefinirRegion(region) => match media.role {
                SessionRole::Controller => {
                    let trame = encoder_controle(SousTypeControle::Region, &encoder_region(region));
                    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
                }
                SessionRole::Controlled => {
                    *media.region.lock().expect("verrou du cadre d'écran") = region;
                }
            },
            CommandeMedia::MajPermissions(nouvelles) => {
                // Les deux rôles mémorisent l'ensemble vivant : côté hôte c'est
                // l'**application effective** (le filtre d'injection et les gardes
                // média le lisent au vol) ; côté contrôleur, un **écho optimiste**
                // pour que les renégociations incrémentales se composent (chaque
                // bascule repart de l'état courant, pas de l'initial). Le
                // contrôleur le transmet en plus à l'hôte.
                media
                    .permissions
                    .store(nouvelles.to_bits(), Ordering::Relaxed);
                if media.role == SessionRole::Controller {
                    let trame = encoder_controle(
                        SousTypeControle::MajPermissions,
                        &encoder_permissions(nouvelles),
                    );
                    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
                }
            }
            CommandeMedia::MajQualite(profil, plafond) => {
                // Écho optimiste local +, côté hôte, application effective
                // (reconfiguration encodeur/ABR via la génération) ; le contrôleur
                // transmet aussi la demande à l'hôte.
                appliquer_qualite(media, profil, plafond);
                if media.role == SessionRole::Controller {
                    let trame = encoder_controle(
                        SousTypeControle::MajQualite,
                        &encoder_qualite(profil, plafond),
                    );
                    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
                }
            }
            CommandeMedia::DefinirEnregistrement(chemin) => {
                // Enregistrement **local** de l'hôte : sans effet côté contrôleur
                // (seul l'hôte encode et muxe son écran).
                if media.role == SessionRole::Controlled {
                    appliquer_enregistrement(media, chemin);
                }
            }
        }
    }
}

/// Applique un préréglage de qualité à l'état partagé de l'hôte (profil ABR +
/// plafond de débit) et **signale le changement** à la boucle de diffusion en
/// incrémentant la génération (elle reconfigure alors l'encodeur/l'ABR au vol).
fn appliquer_qualite(media: &EtatMedia, profil: ContentProfile, plafond_kbps: u32) {
    media
        .qualite
        .profil_video
        .store(matches!(profil, ContentProfile::Video), Ordering::Relaxed);
    media
        .qualite
        .plafond_kbps
        .store(plafond_kbps, Ordering::Relaxed);
    media.qualite.generation.fetch_add(1, Ordering::Relaxed);
}

/// Mémorise le chemin d'enregistrement voulu (ou `None` pour arrêter) et
/// **signale le changement** à la boucle de diffusion (génération) : elle ouvre
/// une nouvelle époque MP4 ou clôt proprement le muxeur courant.
fn appliquer_enregistrement(media: &EtatMedia, chemin: Option<PathBuf>) {
    *media
        .enregistrement
        .chemin
        .lock()
        .expect("verrou d'enregistrement à chaud") = chemin;
    media
        .enregistrement
        .generation
        .fetch_add(1, Ordering::Relaxed);
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

/// Émet les couches d'annotation en attente (canal `Control`, sous-type
/// [`SousTypeControle::Annotation`]).
fn envoyer_annotations_en_attente(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
) {
    let couches: Vec<AnnotationLayer> = {
        let rx = media.annotation_in.lock().expect("verrou annotations");
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    };
    for couche in couches {
        // Une couche non sérialisable (trop de traits pour le format) est sautée.
        if let Ok(octets) = couche.to_bytes() {
            let trame = encoder_controle(SousTypeControle::Annotation, &octets);
            let _ = transport.send(canal_controle, trame, Reliability::Reliable);
        }
    }
}

/// Émet les trames de tunnel en attente (canal `Control`, sous-type
/// [`SousTypeControle::Tunnel`]) : données relayées et ouvertures/fermetures de
/// flux, produites par les fils de pont ([`crate::tunnel`]).
fn envoyer_tunnels_en_attente(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
) {
    for corps in media.tunnels.drainer_sortie() {
        let trame = encoder_controle(SousTypeControle::Tunnel, &corps);
        if transport
            .send(canal_controle, trame, Reliability::Reliable)
            .is_err()
        {
            break;
        }
    }
}

/// Émet le presse-papiers local s'il a changé (garde [`Capability::ClipboardRead`]).
fn synchroniser_presse_papiers(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
) {
    if !media.perms().allows(Capability::ClipboardRead) {
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
        if !media.perms().allows(Capability::Audio) || !media.audio_actif.load(Ordering::Relaxed) {
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
        envoyer_annotations_en_attente(&mut transport, canal_controle, media);
        envoyer_tunnels_en_attente(&mut transport, canal_controle, media);
        pomper_transfert(&mut transport, canal_files, media);
        if Instant::now() >= prochaine_synchro {
            prochaine_synchro = Instant::now() + PERIODE_PRESSE_PAPIERS;
            synchroniser_presse_papiers(&mut transport, canal_controle, media);
        }
    }
}

/// Annonce l'état de confidentialité au contrôleur **quand il change** (hôte →
/// contrôleur, sous-type [`SousTypeControle::ConfidentialiteEtat`]) : c'est le
/// drapeau que le contrôleur affiche. `dernier` mémorise le dernier état émis.
fn annoncer_confidentialite(
    transport: &mut TransportPartage,
    canal_controle: ChannelHandle,
    media: &EtatMedia,
    dernier: &mut Option<bool>,
) {
    let actuel = media.privacy.load(Ordering::Relaxed);
    if *dernier != Some(actuel) {
        *dernier = Some(actuel);
        let trame = encoder_controle(SousTypeControle::ConfidentialiteEtat, &[u8::from(actuel)]);
        let _ = transport.send(canal_controle, trame, Reliability::Reliable);
    }
}

/// Énumère les **écrans réels** de l'hôte (best-effort) et les publie sur le
/// canal `Control` (sous-type [`SousTypeControle::Moniteurs`]). Une énumération
/// en échec (session sans bureau, Wayland pur…) publie une **liste vide** : le
/// contrôleur reçoit tout de même l'annonce (la liste a bien traversé).
fn annoncer_moniteurs(transport: &mut TransportPartage, canal_controle: ChannelHandle) {
    let moniteurs: Vec<RemoteMonitor> = enumerate_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|m| RemoteMonitor {
            index: m.id.0,
            width: m.width,
            height: m.height,
            primary: m.is_primary,
        })
        .collect();
    let trame = encoder_controle(SousTypeControle::Moniteurs, &encoder_moniteurs(&moniteurs));
    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
}

/// Publie les infos système de l'hôte (nom d'hôte + OS) sur le canal `Control`
/// (sous-type [`SousTypeControle::InfosPair`]).
fn annoncer_infos_pair(transport: &mut TransportPartage, canal_controle: ChannelHandle) {
    let trame = encoder_controle(
        SousTypeControle::InfosPair,
        &encoder_infos_pair(&infos_systeme_locales()),
    );
    let _ = transport.send(canal_controle, trame, Reliability::Reliable);
}

/// Infos système locales (nom d'hôte + OS) publiées au pair. **Sans dépendance
/// native** : le nom d'hôte vient des variables d'environnement usuelles
/// (`COMPUTERNAME` sous Windows, `HOSTNAME` ailleurs, repli « inconnu ») et l'OS
/// des constantes de compilation.
fn infos_systeme_locales() -> PeerInfo {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "inconnu".to_owned());
    let os = format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH);
    PeerInfo { host, os }
}

/// Émetteur de fonctions étendues côté **hôte** : audio (canal `Audio`) + plan
/// de contrôle (fichiers, presse-papiers, chat, annotations, tunnels, état de
/// confidentialité, commandes).
fn emetteur_features_hote(
    mut transport: TransportPartage,
    media: &Arc<EtatMedia>,
    arret: &Arc<AtomicBool>,
) {
    let canal_files = transport.open_channel(ChannelKind::Files);
    let canal_controle = transport.open_channel(ChannelKind::Control);
    // Annonce initiale du plan de contrôle (à l'établissement) : liste réelle des
    // écrans de l'hôte et infos système du pair (nom d'hôte + OS). Le contrôleur
    // les lit via [`SessionHandle::monitors`] / [`SessionHandle::peer_info`].
    annoncer_moniteurs(&mut transport, canal_controle);
    annoncer_infos_pair(&mut transport, canal_controle);
    let mut prochaine_synchro = Instant::now();
    // État de confidentialité déjà annoncé (émis à la première itération, puis
    // à chaque bascule) — l'indicateur du contrôleur suit l'hôte.
    let mut etat_prive_annonce: Option<bool> = None;
    while !arret.load(Ordering::Relaxed) {
        traiter_commandes(&mut transport, canal_controle, media);
        envoyer_chat_en_attente(&mut transport, canal_controle, media);
        envoyer_annotations_en_attente(&mut transport, canal_controle, media);
        envoyer_tunnels_en_attente(&mut transport, canal_controle, media);
        annoncer_confidentialite(
            &mut transport,
            canal_controle,
            media,
            &mut etat_prive_annonce,
        );
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
    // Mode étendu : le filtre d'injection lit les permissions **vivantes**
    // partagées (renégociation à chaud via le canal `Control`).
    let mut guichet = GuichetEntrees::new(params, Arc::clone(arret));
    guichet.brancher_permissions_vivantes(Arc::clone(&media.permissions));
    let recepteur = thread::Builder::new()
        .name("nd-session-recv-hote".to_owned())
        .spawn(move || {
            let _ = recepteur_hote(
                transport_recepteur,
                injecteur,
                guichet,
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

/// Preuve **unitaire** du guichet d'entrées hôte (permissions → raccourcis →
/// injection) avec un injecteur témoin : un raccourci déclenché est appliqué
/// (geste moteur), **compté** (`hotkeys_applied`) et sa frappe n'est **jamais**
/// injectée ; les autres entrées suivent le chemin historique. Le câblage dans
/// la vraie boucle de session est prouvé par `tests/session_raccourcis.rs`.
#[cfg(test)]
mod tests_raccourcis {
    use super::*;
    use nd_input::MouseButton;

    /// Injecteur témoin : consigne les frappes injectées et compte les
    /// `release_all`, sans toucher à l'OS.
    #[derive(Default)]
    struct InjecteurTemoin {
        touches: Mutex<Vec<(u32, bool)>>,
        souris: AtomicU64,
        liberations: AtomicU64,
    }

    impl InjecteurTemoin {
        fn touches(&self) -> Vec<(u32, bool)> {
            self.touches.lock().expect("verrou des touches").clone()
        }

        fn liberations(&self) -> u64 {
            self.liberations.load(Ordering::Relaxed)
        }
    }

    impl InputInjector for InjecteurTemoin {
        fn mouse_move_abs(&self, _x: f64, _y: f64, _monitor: MonitorId) -> Result<()> {
            self.souris.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn mouse_move_rel(&self, _dx: f64, _dy: f64) -> Result<()> {
            self.souris.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn mouse_button(&self, _btn: MouseButton, _down: bool) -> Result<()> {
            self.souris.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn scroll(&self, _dx: f64, _dy: f64) -> Result<()> {
            self.souris.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn key(&self, scancode: u32, down: bool) -> Result<()> {
            self.touches
                .lock()
                .expect("verrou des touches")
                .push((scancode, down));
            Ok(())
        }

        fn unicode(&self, _ch: char) -> Result<()> {
            Ok(())
        }

        fn release_all(&self) {
            self.liberations.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Guichet de test aux permissions données, avec ses signaux d'arrêt
    /// `(guichet, arrêt d'époque, arrêt global de session)`.
    fn guichet(
        carte: HotkeyMap<HostAction>,
        permissions: PermissionSet,
        deconnexion_globale: bool,
    ) -> (GuichetEntrees, Arc<AtomicBool>, Arc<AtomicBool>) {
        let arret_epoque = Arc::new(AtomicBool::new(false));
        let stop_session = Arc::new(AtomicBool::new(false));
        let guichet = GuichetEntrees {
            broker: PermissionBroker::with_permissions(permissions),
            refus_journalises: PermissionSet::none(),
            filtre: FiltreRaccourcis::new(carte),
            lecture_seule: false,
            acteur: "pair-test".to_owned(),
            arret_epoque: Arc::clone(&arret_epoque),
            stop_session: deconnexion_globale.then(|| Arc::clone(&stop_session)),
            permissions_live: None,
        };
        (guichet, arret_epoque, stop_session)
    }

    /// Écran + souris + clavier accordés.
    fn clavier_complet() -> PermissionSet {
        [
            Capability::ViewScreen,
            Capability::ControlMouse,
            Capability::ControlKeyboard,
        ]
        .into_iter()
        .collect()
    }

    /// Pousse un événement clavier (sérialisé comme sur le fil) dans le guichet.
    fn touche(
        guichet: &mut GuichetEntrees,
        temoin: &InjecteurTemoin,
        compteurs: &CompteursSession,
        scancode: u32,
        down: bool,
    ) {
        guichet.traiter(
            temoin,
            compteurs,
            &InputEvent::Key { scancode, down }.to_bytes(),
        );
    }

    #[test]
    fn la_carte_par_defaut_mappe_release_mouse_et_ctrl_alt_suppr() {
        let carte = raccourcis_hote_defaut();
        assert_eq!(
            carte.lookup(Hotkey::new(Hotkey::CTRL | Hotkey::ALT, SCAN_M)),
            Some(&HostAction::ReleaseMouse)
        );
        assert_eq!(
            carte.lookup(Hotkey::new(Hotkey::CTRL | Hotkey::ALT, SCAN_FIN)),
            Some(&HostAction::SendCtrlAltDel)
        );
    }

    /// **Sonde du lot** : `Ctrl+Alt+M` (carte par défaut) déclenche
    /// [`HostAction::ReleaseMouse`] — geste moteur appliqué (`release_all`),
    /// compté dans `hotkeys_applied` — et la touche `M` n'est **jamais**
    /// injectée (ni l'appui, ni la répétition, ni le relâchement), tandis que
    /// les modificateurs suivent le chemin normal d'injection.
    #[test]
    fn ctrl_alt_m_libere_la_souris_compte_et_n_injecte_pas_la_frappe() {
        let compteurs = CompteursSession::default();
        let temoin = InjecteurTemoin::default();
        let (mut guichet, _arret, _stop) =
            guichet(raccourcis_hote_defaut(), clavier_complet(), true);

        touche(&mut guichet, &temoin, &compteurs, SCAN_CTRL_GAUCHE, true);
        touche(&mut guichet, &temoin, &compteurs, SCAN_ALT_GAUCHE, true);
        touche(&mut guichet, &temoin, &compteurs, SCAN_M, true); // déclenche
        touche(&mut guichet, &temoin, &compteurs, SCAN_M, true); // répétition avalée
        touche(&mut guichet, &temoin, &compteurs, SCAN_M, false); // relâchement avalé
        touche(&mut guichet, &temoin, &compteurs, SCAN_ALT_GAUCHE, false);
        touche(&mut guichet, &temoin, &compteurs, SCAN_CTRL_GAUCHE, false);

        let stats = compteurs.instantane();
        assert_eq!(stats.hotkeys_applied, 1, "une seule action par appui");
        assert_eq!(temoin.liberations(), 1, "geste hôte appliqué (release_all)");
        let touches = temoin.touches();
        assert!(
            touches.iter().all(|&(scan, _)| scan != SCAN_M),
            "la frappe du raccourci ne doit jamais être injectée : {touches:?}"
        );
        // Les modificateurs, eux, ont suivi le chemin normal (appui/relâchement).
        assert_eq!(stats.inputs_applied, 4, "Ctrl/Alt aller-retour injectés");
        assert_eq!(stats.inputs_denied, 0);
    }

    /// Sans [`Capability::ControlKeyboard`], la combinaison est refusée par les
    /// permissions **avant** la résolution : aucune action hôte déclenchable
    /// par un pair en observation seule.
    #[test]
    fn un_raccourci_refuse_par_les_permissions_ne_declenche_rien() {
        let compteurs = CompteursSession::default();
        let temoin = InjecteurTemoin::default();
        let (mut guichet, _arret, _stop) =
            guichet(raccourcis_hote_defaut(), PermissionSet::view_only(), true);

        touche(&mut guichet, &temoin, &compteurs, SCAN_CTRL_GAUCHE, true);
        touche(&mut guichet, &temoin, &compteurs, SCAN_ALT_GAUCHE, true);
        touche(&mut guichet, &temoin, &compteurs, SCAN_M, true);

        let stats = compteurs.instantane();
        assert_eq!(stats.hotkeys_applied, 0);
        assert_eq!(stats.inputs_denied, 3);
        assert_eq!(temoin.liberations(), 0);
        assert!(temoin.touches().is_empty());
    }

    /// [`HostAction::ToggleViewOnly`] (carte personnalisée) gèle les entrées —
    /// refus doux compté `inputs_denied` — puis les rétablit à la seconde
    /// bascule : le raccourci reste résolu pendant le gel (bascule réversible).
    #[test]
    fn bascule_lecture_seule_gele_puis_retablit_les_entrees() {
        const SCAN_F1: u32 = 0x3B;
        const SCAN_A: u32 = 0x1E;
        let mut carte = HotkeyMap::new();
        carte.bind(Hotkey::new(0, SCAN_F1), HostAction::ToggleViewOnly);

        let compteurs = CompteursSession::default();
        let temoin = InjecteurTemoin::default();
        let (mut guichet, _arret, _stop) = guichet(carte, clavier_complet(), true);

        touche(&mut guichet, &temoin, &compteurs, SCAN_F1, true); // gel
        touche(&mut guichet, &temoin, &compteurs, SCAN_F1, false);
        touche(&mut guichet, &temoin, &compteurs, SCAN_A, true); // refusée (gel)
        touche(&mut guichet, &temoin, &compteurs, SCAN_A, false);
        touche(&mut guichet, &temoin, &compteurs, SCAN_F1, true); // dégel
        touche(&mut guichet, &temoin, &compteurs, SCAN_F1, false);
        touche(&mut guichet, &temoin, &compteurs, SCAN_A, true); // injectée
        touche(&mut guichet, &temoin, &compteurs, SCAN_A, false);

        let stats = compteurs.instantane();
        assert_eq!(stats.hotkeys_applied, 2, "deux bascules comptées");
        assert_eq!(stats.inputs_denied, 2, "frappe gelée comptée refusée");
        assert_eq!(stats.inputs_applied, 2, "frappe injectée après dégel");
        assert_eq!(temoin.touches(), vec![(SCAN_A, true), (SCAN_A, false)]);
    }

    /// [`HostAction::Disconnect`] termine toujours l'époque, et clôt toute la
    /// session quand le propriétaire l'a demandé (`deconnexion_globale`) — pas
    /// pour l'hôte non surveillé, qui survit à ses sessions.
    #[test]
    fn deconnexion_leve_les_signaux_selon_le_proprietaire() {
        const SCAN_F2: u32 = 0x3C;
        let mut carte = HotkeyMap::new();
        carte.bind(Hotkey::new(0, SCAN_F2), HostAction::Disconnect);

        // Moteur de session : époque **et** session entière.
        let compteurs = CompteursSession::default();
        let temoin = InjecteurTemoin::default();
        let (mut guichet_moteur, arret, stop) = guichet(carte.clone(), clavier_complet(), true);
        touche(&mut guichet_moteur, &temoin, &compteurs, SCAN_F2, true);
        assert!(arret.load(Ordering::Relaxed), "l'époque s'arrête");
        assert!(stop.load(Ordering::Relaxed), "la session se clôt");
        assert_eq!(compteurs.instantane().hotkeys_applied, 1);

        // Hôte non surveillé : seule l'époque se termine.
        let compteurs = CompteursSession::default();
        let (mut guichet_service, arret, stop) = guichet(carte, clavier_complet(), false);
        touche(&mut guichet_service, &temoin, &compteurs, SCAN_F2, true);
        assert!(arret.load(Ordering::Relaxed), "l'époque s'arrête");
        assert!(!stop.load(Ordering::Relaxed), "le service continue");
    }

    /// Le suivi des modificateurs distingue gauche/droite : relâcher le Ctrl
    /// gauche ne masque pas le droit, et les bits reflètent l'état réel.
    #[test]
    fn le_suivi_des_modificateurs_distingue_gauche_et_droite() {
        let mut filtre = FiltreRaccourcis::new(raccourcis_hote_defaut());
        let mut presser = |scan: u32, down: bool| {
            let _ = filtre.filtrer(&InputEvent::Key {
                scancode: scan,
                down,
            });
        };
        presser(SCAN_CTRL_GAUCHE, true);
        presser(SCAN_CTRL_DROIT, true);
        presser(SCAN_MAJ_GAUCHE, true);
        presser(SCAN_CTRL_GAUCHE, false);
        presser(SCAN_MAJ_GAUCHE, false);
        // Le Ctrl droit tient toujours : CTRL reste actif, SHIFT est retombé.
        assert_eq!(filtre.modificateurs(), Hotkey::CTRL);
        let mut presser = |scan: u32, down: bool| {
            let _ = filtre.filtrer(&InputEvent::Key {
                scancode: scan,
                down,
            });
        };
        presser(SCAN_CTRL_DROIT, false);
        assert_eq!(filtre.modificateurs(), 0);
    }

    /// Souris et Unicode traversent le filtre sans y être résolus : le chemin
    /// d'injection historique reste inchangé pour tout ce qui n'est pas clavier.
    #[test]
    fn souris_et_unicode_ne_sont_pas_des_raccourcis() {
        let compteurs = CompteursSession::default();
        let temoin = InjecteurTemoin::default();
        let (mut guichet, _arret, _stop) =
            guichet(raccourcis_hote_defaut(), clavier_complet(), true);
        guichet.traiter(
            &temoin,
            &compteurs,
            &InputEvent::MouseMoveRel { dx: 2.0, dy: 3.0 }.to_bytes(),
        );
        guichet.traiter(
            &temoin,
            &compteurs,
            &InputEvent::Unicode { codepoint: 0x41 }.to_bytes(),
        );
        let stats = compteurs.instantane();
        assert_eq!(stats.hotkeys_applied, 0);
        assert_eq!(stats.inputs_applied, 2);
        assert_eq!(
            temoin.souris.load(Ordering::Relaxed),
            1,
            "un seul geste souris"
        );
    }
}
