//! Façade d'API orientée UI — contrat stable pour l'application Flutter (plan 10).
//!
//! # Intégration Flutter
//!
//! Ce module est le **périmètre scanné** par `flutter_rust_bridge_codegen`
//! (`rust_input: crate::api` dans `ui/flutter_rust_bridge.yaml`) : chaque fonction
//! publique ici devient une fonction Dart, les paramètres `StreamSink<T>` devenant
//! des `Stream<T>`. Outre les helpers purs historiques, la façade pilote désormais
//! le moteur réel : voir « Session live » plus bas ([`start_session`],
//! [`session_video_stream`]…). La commande de régénération du pont est documentée
//! en tête de `lib.rs`.
//!
//! # Principes du contrat
//!
//! * Les DTO (« data transfer objects ») sont **plats** : uniquement des champs
//!   simples (`String`, `u64`, `bool`, `f64`, `Option<_>`) que le pont FFI sait
//!   traduire en Dart sans friction.
//! * Les fonctions faillibles renvoient `Result<_, String>` : le message d'erreur est
//!   en français, lisible, affichable tel quel par l'UI.
//! * Les conversions vers/depuis les types internes (`nd_core`, `nd_proto`,
//!   `nd_features`) vivent ici : l'UI ne manipule jamais les types internes.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use nd_codec::DecodedFrame;
use nd_core::{
    ChatMessage, PeerInfo, RemoteMonitor, SessionConfig, SessionOptions, SessionRole, SessionState,
    SessionStats,
};
use nd_features::{AnnotationLayer, MacAddr, PermissionSet, Permissions, Stroke};
use nd_files::TransferEvent;
use nd_proto::{InputEvent, NdError, NovaId};
use serde::{Deserialize, Serialize};

use crate::frb_generated::StreamSink;

// ---------------------------------------------------------------------------
// Informations générales
// ---------------------------------------------------------------------------

/// Informations générales sur l'application (écran « À propos », journaux…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    /// Version du moteur/protocole, ex. `"0.1"`.
    pub version: String,
}

/// Renvoie les informations générales de l'application.
#[must_use]
pub fn app_info() -> AppInfo {
    AppInfo {
        version: nd_core::engine_version().to_string(),
    }
}

// ---------------------------------------------------------------------------
// ID NovaDesk : affichage et saisie
// ---------------------------------------------------------------------------

/// Formate un ID NovaDesk pour affichage : 9 chiffres groupés par 3, ex. `123 456 789`.
#[must_use]
pub fn format_nova_id(id: u64) -> String {
    NovaId(id).to_string()
}

/// Analyse un ID NovaDesk saisi par l'utilisateur.
///
/// Tolère le format groupé produit par [`format_nova_id`] (`"123 456 789"`) ainsi que
/// tout espacement parasite (espaces, tabulations, espaces insécables d'un
/// copier-coller). Les zéros de tête sont acceptés (`"000 000 001"` → `1`).
pub fn parse_nova_id(texte: &str) -> Result<u64, String> {
    let chiffres: String = texte.chars().filter(|c| !c.is_whitespace()).collect();
    if chiffres.is_empty() {
        return Err("l'ID NovaDesk est vide".to_owned());
    }
    if let Some(c) = chiffres.chars().find(|c| !c.is_ascii_digit()) {
        return Err(format!("caractère invalide dans l'ID NovaDesk : « {c} »"));
    }
    chiffres
        .parse::<u64>()
        .map_err(|_| format!("ID NovaDesk trop long : « {chiffres} »"))
}

// ---------------------------------------------------------------------------
// Rôle et état de session
// ---------------------------------------------------------------------------

/// Rôle du poste local dans la session (miroir plat de [`nd_core::SessionRole`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRoleDto {
    /// Ce poste pilote l'autre.
    Controller,
    /// Ce poste est piloté.
    Controlled,
}

impl From<SessionRole> for SessionRoleDto {
    fn from(role: SessionRole) -> Self {
        match role {
            SessionRole::Controller => SessionRoleDto::Controller,
            SessionRole::Controlled => SessionRoleDto::Controlled,
        }
    }
}

impl From<SessionRoleDto> for SessionRole {
    fn from(dto: SessionRoleDto) -> Self {
        match dto {
            SessionRoleDto::Controller => SessionRole::Controller,
            SessionRoleDto::Controlled => SessionRole::Controlled,
        }
    }
}

/// État de session lisible par l'UI (miroir plat de [`nd_core::SessionState`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStateDto {
    /// Aucune session active.
    Idle,
    /// Résolution de l'ID pair via le rendez-vous.
    Resolving,
    /// Établissement du transport (NAT traversal / relais).
    Connecting,
    /// Handshake cryptographique en cours.
    Handshaking,
    /// Session établie et média en cours.
    Active,
    /// Coupure réseau : tentative de reconnexion rapide.
    Reconnecting,
    /// Session terminée.
    Closed,
}

impl SessionStateDto {
    /// Libellé français court et stable, prêt à afficher tel quel.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SessionStateDto::Idle => "inactive",
            SessionStateDto::Resolving => "résolution du pair",
            SessionStateDto::Connecting => "connexion",
            SessionStateDto::Handshaking => "authentification",
            SessionStateDto::Active => "active",
            SessionStateDto::Reconnecting => "reconnexion",
            SessionStateDto::Closed => "terminée",
        }
    }
}

impl From<SessionState> for SessionStateDto {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Idle => SessionStateDto::Idle,
            SessionState::Resolving => SessionStateDto::Resolving,
            SessionState::Connecting => SessionStateDto::Connecting,
            SessionState::Handshaking => SessionStateDto::Handshaking,
            SessionState::Active => SessionStateDto::Active,
            SessionState::Reconnecting => SessionStateDto::Reconnecting,
            SessionState::Closed => SessionStateDto::Closed,
        }
    }
}

impl From<SessionStateDto> for SessionState {
    fn from(dto: SessionStateDto) -> Self {
        match dto {
            SessionStateDto::Idle => SessionState::Idle,
            SessionStateDto::Resolving => SessionState::Resolving,
            SessionStateDto::Connecting => SessionState::Connecting,
            SessionStateDto::Handshaking => SessionState::Handshaking,
            SessionStateDto::Active => SessionState::Active,
            SessionStateDto::Reconnecting => SessionState::Reconnecting,
            SessionStateDto::Closed => SessionState::Closed,
        }
    }
}

/// Photographie de l'état d'une session, prête à afficher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatusDto {
    /// Libellé de l'état courant (voir [`SessionStateDto::label`]).
    pub state: String,
    /// ID du pair au format groupé (`"123 456 789"`), si connu.
    pub peer: Option<String>,
}

/// Construit un statut de session affichable à partir d'un état et d'un pair éventuel.
#[must_use]
pub fn session_status(state: SessionStateDto, peer_id: Option<u64>) -> SessionStatusDto {
    SessionStatusDto {
        state: state.label().to_owned(),
        peer: peer_id.map(format_nova_id),
    }
}

// ---------------------------------------------------------------------------
// Permissions et configuration de session
// ---------------------------------------------------------------------------

/// Permissions de session sous forme plate (miroir de [`nd_features::Permissions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionsDto {
    pub keyboard: bool,
    pub mouse: bool,
    pub clipboard: bool,
    pub files: bool,
    pub audio: bool,
    /// Si vrai, la session est en lecture seule (aucune entrée injectée).
    pub view_only: bool,
}

impl PermissionsDto {
    /// Contrôle complet (clavier, souris, presse-papiers, fichiers, audio).
    ///
    /// `frb(ignore)` : non exposé au binding — l'UI construit le DTO par champs,
    /// et un constructeur `full()` bridgé n'apporte rien.
    #[flutter_rust_bridge::frb(ignore)]
    #[must_use]
    pub fn full() -> Self {
        Permissions::full().into()
    }

    /// Observation seule : rien n'est injecté ni transféré.
    ///
    /// `frb(ignore)` **nécessaire** : sans cela FRB génère une méthode statique
    /// `viewOnly()` qui entre en conflit avec le champ `view_only` (→ `viewOnly`).
    #[flutter_rust_bridge::frb(ignore)]
    #[must_use]
    pub fn view_only() -> Self {
        Permissions::view_only().into()
    }
}

impl Default for PermissionsDto {
    /// Défaut prudent, aligné sur [`nd_features::Permissions::default`] : observation seule.
    fn default() -> Self {
        PermissionsDto::view_only()
    }
}

impl From<Permissions> for PermissionsDto {
    fn from(p: Permissions) -> Self {
        PermissionsDto {
            keyboard: p.keyboard,
            mouse: p.mouse,
            clipboard: p.clipboard,
            files: p.files,
            audio: p.audio,
            view_only: p.view_only,
        }
    }
}

impl From<PermissionsDto> for Permissions {
    fn from(dto: PermissionsDto) -> Self {
        Permissions {
            keyboard: dto.keyboard,
            mouse: dto.mouse,
            clipboard: dto.clipboard,
            files: dto.files,
            audio: dto.audio,
            view_only: dto.view_only,
        }
    }
}

/// Paramètres de démarrage d'une session, sous forme plate
/// (miroir de [`nd_core::SessionConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfigDto {
    pub role: SessionRoleDto,
    /// ID NovaDesk du poste local.
    pub local_id: u64,
    /// ID du pair à joindre (requis pour le rôle contrôleur).
    pub peer_id: Option<u64>,
    /// Permissions initiales (le poste contrôlé fait foi).
    pub permissions: PermissionsDto,
}

/// Construit et valide une configuration de session côté UI.
///
/// Vérifie dès la saisie ce que le moteur refuserait plus tard, afin que l'UI
/// puisse afficher une erreur immédiate et compréhensible :
/// * le rôle contrôleur exige l'ID du pair ;
/// * on ne se connecte pas à soi-même.
pub fn new_session_config(
    role: SessionRoleDto,
    local_id: u64,
    peer_id: Option<u64>,
    permissions: PermissionsDto,
) -> Result<SessionConfigDto, String> {
    if role == SessionRoleDto::Controller && peer_id.is_none() {
        return Err("le rôle contrôleur nécessite l'ID du pair à joindre".to_owned());
    }
    if peer_id == Some(local_id) {
        return Err(format!(
            "l'ID du pair ({}) est identique à l'ID local : impossible de se connecter à soi-même",
            format_nova_id(local_id)
        ));
    }
    Ok(SessionConfigDto {
        role,
        local_id,
        peer_id,
        permissions,
    })
}

impl From<SessionConfigDto> for SessionConfig {
    fn from(dto: SessionConfigDto) -> Self {
        SessionConfig {
            role: dto.role.into(),
            local_id: NovaId(dto.local_id),
            peer_id: dto.peer_id.map(NovaId),
            permissions: dto.permissions.into(),
        }
    }
}

impl From<SessionConfig> for SessionConfigDto {
    fn from(config: SessionConfig) -> Self {
        SessionConfigDto {
            role: config.role.into(),
            local_id: config.local_id.as_u64(),
            peer_id: config.peer_id.map(NovaId::as_u64),
            permissions: config.permissions.into(),
        }
    }
}

/// Options avancées de démarrage d'une session, sous forme plate (miroir
/// simplifié de [`nd_core::SessionOptions`]).
///
/// Complète [`SessionConfigDto`] : ce dernier porte le rôle, les ID et les
/// permissions historiques ; celui-ci affine le comportement côté **contrôlé**
/// (filtre de permissions granulaire, enregistrement local, encodage delta) et
/// active les **canaux média annexes** (chat, fichiers, audio, presse-papiers,
/// bascule moniteur) via [`extended_features`](SessionOptionsDto::extended_features).
/// Les axes non exposés ici (profil ABR, politique de reconnexion) prennent les
/// valeurs par défaut du moteur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOptionsDto {
    /// Permissions granulaires appliquées avant chaque injection d'entrée
    /// (contrôlé). Fait autorité sur les permissions de [`SessionConfigDto`].
    pub permissions: PermissionsDto,
    /// Chemin du MP4 à écrire pour l'enregistrement local (hôte) ; `None` =
    /// pas d'enregistrement.
    pub recording_path: Option<String>,
    /// Encodage delta **opt-in** : à n'activer que si la capture renseigne
    /// fidèlement les régions modifiées (voir [`nd_core::SessionOptions::delta_mode`]).
    pub delta_mode: bool,
    /// Active les **fonctions étendues** de la session (canaux annexes : chat,
    /// transfert de fichiers, audio, presse-papiers, bascule moniteur), chacune
    /// gardée par sa permission. `false` (défaut) = session vidéo + entrées
    /// historique, comportement strictement inchangé. Quand ce drapeau est vrai,
    /// la façade démarre le moteur via `SessionEngine::start_with_media` en
    /// injectant l'audio duplex système et le presse-papiers de la plateforme
    /// (voir [`start_session_with_options`]).
    pub extended_features: bool,
    /// Répertoire de réception des fichiers transférés (canal `Files`). `None` =
    /// dossier temporaire du système. Ignoré hors mode étendu.
    pub transfer_dir: Option<String>,
    /// Reconnexion transparente **au niveau transport** pour un point de contact
    /// [`SessionEndpointDto::Direct`] côté contrôleur (voir
    /// [`nd_core::SessionOptions::transport_reconnect`]). `false` par défaut.
    pub transport_reconnect: bool,
}

impl From<SessionOptionsDto> for SessionOptions {
    fn from(dto: SessionOptionsDto) -> Self {
        SessionOptions {
            permissions: Some(PermissionSet::from(Permissions::from(dto.permissions))),
            recording: dto.recording_path.map(PathBuf::from),
            delta_mode: dto.delta_mode,
            extended_features: dto.extended_features,
            transfer_dir: dto.transfer_dir.map(PathBuf::from),
            transport_reconnect: dto.transport_reconnect,
            // Profil ABR et politique de reconnexion : défauts du moteur.
            ..SessionOptions::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Événements d'entrée
// ---------------------------------------------------------------------------

/// Événement d'entrée sous forme plate (miroir de [`nd_proto::InputEvent`]).
///
/// La sérialisation binaire reste celle de `nd-proto`
/// ([`encode_input_event`] / [`decode_input_event`]) : ce type n'existe que pour que
/// l'UI n'importe pas les types du protocole.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEventDto {
    /// Déplacement absolu, coordonnées normalisées 0.0–1.0 sur le moniteur.
    MouseMoveAbs { x: f64, y: f64, monitor: u32 },
    /// Déplacement relatif en pixels.
    MouseMoveRel { dx: f64, dy: f64 },
    /// Bouton souris (0=gauche, 1=droit, 2=milieu, 3=X1, 4=X2).
    MouseButton { button: u8, down: bool },
    /// Molette (crans ; positif = haut/droite).
    Scroll { dx: f64, dy: f64 },
    /// Touche par scancode physique.
    Key { scancode: u32, down: bool },
    /// Caractère Unicode (point de code).
    Unicode { codepoint: u32 },
}

impl From<InputEvent> for InputEventDto {
    fn from(event: InputEvent) -> Self {
        match event {
            InputEvent::MouseMoveAbs { x, y, monitor } => {
                InputEventDto::MouseMoveAbs { x, y, monitor }
            }
            InputEvent::MouseMoveRel { dx, dy } => InputEventDto::MouseMoveRel { dx, dy },
            InputEvent::MouseButton { button, down } => InputEventDto::MouseButton { button, down },
            InputEvent::Scroll { dx, dy } => InputEventDto::Scroll { dx, dy },
            InputEvent::Key { scancode, down } => InputEventDto::Key { scancode, down },
            InputEvent::Unicode { codepoint } => InputEventDto::Unicode { codepoint },
        }
    }
}

impl From<InputEventDto> for InputEvent {
    fn from(dto: InputEventDto) -> Self {
        match dto {
            InputEventDto::MouseMoveAbs { x, y, monitor } => {
                InputEvent::MouseMoveAbs { x, y, monitor }
            }
            InputEventDto::MouseMoveRel { dx, dy } => InputEvent::MouseMoveRel { dx, dy },
            InputEventDto::MouseButton { button, down } => InputEvent::MouseButton { button, down },
            InputEventDto::Scroll { dx, dy } => InputEvent::Scroll { dx, dy },
            InputEventDto::Key { scancode, down } => InputEvent::Key { scancode, down },
            InputEventDto::Unicode { codepoint } => InputEvent::Unicode { codepoint },
        }
    }
}

/// Sérialise un événement d'entrée au format binaire du canal `Input`
/// (via [`nd_proto::InputEvent::to_bytes`]).
#[must_use]
pub fn encode_input_event(event: InputEventDto) -> Vec<u8> {
    InputEvent::from(event).to_bytes()
}

/// Désérialise un événement d'entrée depuis le format de [`encode_input_event`]
/// (via [`nd_proto::InputEvent::from_bytes`]).
pub fn decode_input_event(data: &[u8]) -> Result<InputEventDto, String> {
    InputEvent::from_bytes(data)
        .map(InputEventDto::from)
        .ok_or_else(|| {
            format!(
                "événement d'entrée illisible ({} octet(s) reçus)",
                data.len()
            )
        })
}

// ---------------------------------------------------------------------------
// Session live : DTO
// ---------------------------------------------------------------------------

/// Image décodée prête à afficher, poussée par [`session_video_stream`]
/// (miroir plat de `nd_codec::DecodedFrame`).
///
/// L'ordre des champs est aussi l'ordre d'encodage sur le pont : ne pas le changer
/// sans régénérer le binding Dart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameDto {
    /// Largeur en pixels.
    pub width: u32,
    /// Hauteur en pixels.
    pub height: u32,
    /// Pixels RGBA (largeur × hauteur × 4 octets), ordre R, G, B, A.
    pub rgba: Vec<u8>,
}

impl From<DecodedFrame> for VideoFrameDto {
    fn from(frame: DecodedFrame) -> Self {
        VideoFrameDto {
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba,
        }
    }
}

/// Instantané des statistiques d'une session (miroir plat de
/// [`nd_core::SessionStats`], rafraîchies en continu par le moteur).
///
/// Les cinq premiers champs sont historiques (lot 03) ; les suivants exposent
/// les statistiques enrichies du moteur (lot §2 : permissions, ABR,
/// enregistrement, reconnexion) et le backend d'encodage réellement à l'œuvre.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatsDto {
    /// Images décodées par seconde, fenêtre glissante d'une seconde (contrôleur).
    pub fps: f64,
    /// RTT du chemin réseau en microsecondes.
    pub rtt_us: u64,
    /// Octets utiles reçus (après déchiffrement, hors handshake).
    pub bytes_in: u64,
    /// Octets utiles émis (avant chiffrement, hors handshake).
    pub bytes_out: u64,
    /// Frames décodées livrées depuis le début de la session (contrôleur).
    pub frames: u64,
    /// Entrées reçues mais **refusées par les permissions** (contrôlé).
    pub inputs_denied: u64,
    /// Débit cible actuellement appliqué à l'encodeur par l'ABR (hôte), kbit/s.
    pub target_bitrate_kbps: u32,
    /// Palier ABR courant (hôte) : 0 = plein régime, croît en dégradant.
    pub abr_level: u32,
    /// Images écrites dans l'enregistrement local (hôte), toutes époques confondues.
    pub frames_recorded: u64,
    /// Reconnexions **réussies** depuis le début de la session.
    pub reconnects: u32,
    /// Nom du backend d'encodage réellement à l'œuvre côté hôte (NVENC, repli
    /// logiciel…) ; `None` tant que l'encodeur n'est pas créé ou côté contrôleur.
    pub encoder_backend: Option<String>,
}

impl From<SessionStats> for SessionStatsDto {
    fn from(stats: SessionStats) -> Self {
        SessionStatsDto {
            // Le cœur mesure en `f32` ; le pont Dart ne connaît que le `f64`.
            fps: f64::from(stats.fps),
            rtt_us: stats.rtt_us,
            bytes_in: stats.bytes_in,
            bytes_out: stats.bytes_out,
            frames: stats.frames_decoded,
            inputs_denied: stats.inputs_denied,
            target_bitrate_kbps: stats.target_bitrate_kbps,
            abr_level: stats.abr_level,
            frames_recorded: stats.frames_recorded,
            reconnects: stats.reconnects,
            // Renseigné à part par la façade (voir `flux`), la poignée exposant
            // le backend hors de `SessionStats`.
            encoder_backend: None,
        }
    }
}

/// Point de contact réseau d'une session (miroir plat de
/// [`nd_core::SessionEndpoint`]).
///
/// Couvre la mise en relation directe testable dès maintenant (loopback/LAN) **et**
/// la connexion par ID via le serveur de rendez-vous ([`SessionEndpointDto::ByRendezvous`] :
/// STUN, hole punching, relais optionnel). Les adresses y sont fournies en texte
/// (« ip:port ») et analysées par la façade, avec un message d'erreur français clair
/// en cas de saisie invalide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndpointDto {
    /// La session lie un écouteur QUIC local (`127.0.0.1`, port éphémère) et
    /// **accepte** la connexion entrante (rôle hôte typique). L'adresse et le
    /// certificat à transmettre au pair se relisent via [`session_listen_info`].
    Loopback,
    /// La session **se connecte** directement à `addr` (format « ip:port ») avec le
    /// certificat auto-signé (DER) épinglé du pair (rôle contrôleur typique).
    Direct {
        /// Adresse QUIC (UDP) du pair, ex. « 127.0.0.1:53211 ».
        addr: String,
        /// Certificat DER du pair, épinglé à la connexion.
        cert_der: Vec<u8>,
    },
    /// La session se met en relation **par ID** via un serveur de rendez-vous :
    /// STUN → hole punching → QUIC sur la socket percée, avec repli relais
    /// optionnel. C'est le seul point de contact **reconnectable**. Toutes les
    /// adresses sont en texte (« ip:port »).
    ByRendezvous {
        /// Adresse du serveur de rendez-vous (`nd-signaling`), ex. « 203.0.113.7:9000 ».
        server: String,
        /// Serveurs STUN interrogés pour le candidat réflexif. Liste vide =
        /// candidats locaux seulement (LAN/boucle locale).
        stun_servers: Vec<String>,
        /// Relais de repli (`nd-relay`) quand le punch échoue ; `None` = pas de repli.
        relay: Option<String>,
    },
}

/// Coordonnées d'écoute d'une session hôte démarrée en
/// [`SessionEndpointDto::Loopback`] : à transmettre au pair pour qu'il se connecte
/// en [`SessionEndpointDto::Direct`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenInfoDto {
    /// Adresse d'écoute effective (« 127.0.0.1:port », port éphémère résolu).
    pub addr: String,
    /// Certificat auto-signé (DER) à épingler côté pair.
    pub cert_der: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Session média étendue : DTO (chat, transfert de fichiers)
// ---------------------------------------------------------------------------

/// Message de chat poussé par [`session_chat_stream`] (miroir plat de
/// `nd_core::ChatMessage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageDto {
    /// `true` si le message vient du pair distant, `false` pour l'écho local
    /// d'un message que ce poste vient d'émettre via [`send_chat`].
    pub from_remote: bool,
    /// Texte du message (UTF-8).
    pub text: String,
}

impl From<ChatMessage> for ChatMessageDto {
    fn from(message: ChatMessage) -> Self {
        ChatMessageDto {
            from_remote: message.from_remote,
            text: message.text,
        }
    }
}

/// Évènement de progression d'un transfert de fichiers, poussé par
/// [`session_transfer_stream`] — **aplatissement** des variantes de
/// `nd_files::TransferEvent` en une structure plate que le pont FFI traduit sans
/// friction. L'UI branche sur [`kind`](TransferEventDto::kind) ; les champs
/// non pertinents pour un `kind` donné valent `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferEventDto {
    /// Nature de l'évènement : `"started"` (début d'un fichier), `"progress"`
    /// (avancement), `"completed"` (fichier terminé et vérifié), `"finished"`
    /// (toute la file transférée) ou `"cancelled"` (annulation).
    pub kind: String,
    /// Index (0-basé) du fichier concerné dans la file (`started`/`progress`/`completed`).
    pub file_index: Option<u64>,
    /// Nom du fichier concerné (`started`/`progress`/`completed`).
    pub file_name: Option<String>,
    /// Octets du **fichier courant** déjà présents (offset de reprise pour
    /// `started`, octets faits pour `progress`, taille pour `completed`).
    pub bytes_done: Option<u64>,
    /// Taille totale du **fichier courant** (`started`/`progress`/`completed`).
    pub bytes_total: Option<u64>,
    /// Octets déjà présents pour l'ensemble de la file (`progress`).
    pub session_bytes_done: Option<u64>,
    /// Taille totale connue de la file (`progress`).
    pub session_bytes_total: Option<u64>,
    /// Pourcentage accompli de la **session** dans `[0, 100]` (`progress`).
    pub percent: Option<f64>,
    /// Débit instantané moyen de la session en octets/seconde (`progress`).
    pub bytes_per_sec: Option<f64>,
    /// Temps estimé avant la fin de la session en secondes, si un débit existe
    /// (`progress`).
    pub eta_secs: Option<f64>,
}

impl From<TransferEvent> for TransferEventDto {
    fn from(event: TransferEvent) -> Self {
        // Gabarit « tout absent » : chaque variante ne renseigne que ses champs.
        let vide = |kind: &str| TransferEventDto {
            kind: kind.to_owned(),
            file_index: None,
            file_name: None,
            bytes_done: None,
            bytes_total: None,
            session_bytes_done: None,
            session_bytes_total: None,
            percent: None,
            bytes_per_sec: None,
            eta_secs: None,
        };
        match event {
            TransferEvent::FileStarted {
                index,
                name,
                size,
                resume_offset,
            } => TransferEventDto {
                file_index: Some(index),
                file_name: Some(name),
                bytes_done: Some(resume_offset),
                bytes_total: Some(size),
                ..vide("started")
            },
            TransferEvent::Progress(info) => {
                // `percent` emprunte `info` : calculé avant de déplacer `file_name`.
                let percent = info.percent();
                TransferEventDto {
                    file_index: Some(info.file_index),
                    file_name: Some(info.file_name),
                    bytes_done: Some(info.file_bytes_done),
                    bytes_total: Some(info.file_bytes_total),
                    session_bytes_done: Some(info.session_bytes_done),
                    session_bytes_total: Some(info.session_bytes_total),
                    percent: Some(percent),
                    bytes_per_sec: Some(info.bytes_per_sec),
                    eta_secs: info.eta_secs,
                    ..vide("progress")
                }
            }
            TransferEvent::FileCompleted { index, name, size } => TransferEventDto {
                file_index: Some(index),
                file_name: Some(name),
                bytes_done: Some(size),
                bytes_total: Some(size),
                ..vide("completed")
            },
            TransferEvent::Finished => vide("finished"),
            TransferEvent::Cancelled => vide("cancelled"),
        }
    }
}

// ---------------------------------------------------------------------------
// Session live : cycle de vie et flux
// ---------------------------------------------------------------------------

/// Démarre une session réelle ([`nd_core::SessionEngine`] : QUIC → Noise →
/// capture/codec/entrées) et renvoie son **identifiant opaque**.
///
/// L'identifiant indexe une table statique interne : toutes les autres fonctions de
/// session (`session_*`, [`send_input`], [`stop_session`]) le prennent en premier
/// argument. La session vit jusqu'à [`stop_session`], même si elle se clôt
/// d'elle-même entre-temps (les statistiques et [`session_last_error`] restent
/// consultables).
pub fn start_session(
    config: SessionConfigDto,
    endpoint: SessionEndpointDto,
) -> Result<u64, String> {
    crate::flux::demarrer_session(config, endpoint)
}

/// Démarre une session comme [`start_session`], mais avec des options avancées
/// ([`SessionOptionsDto`] : permissions granulaires, enregistrement local,
/// encodage delta, **fonctions média étendues**).
///
/// [`start_session`] équivaut à cet appel avec les options par défaut du moteur.
/// L'identifiant renvoyé s'utilise avec les mêmes fonctions `session_*`.
///
/// # Mode étendu
///
/// Si [`SessionOptionsDto::extended_features`] est vrai, la façade démarre le
/// moteur via `nd_core::SessionEngine::start_with_media` en **injectant des
/// briques média réelles** : l'audio duplex système
/// (`nd_audio::AudioSession::duplex_systeme`) et le presse-papiers de la
/// plateforme (`nd_files::ClipboardSync::new`). Chaque brique indisponible
/// (pas de périphérique audio, OS sans presse-papiers) est **silencieusement
/// omise** (`None`) : la session démarre quand même, seule la fonction
/// correspondante reste inerte — jamais d'échec de démarrage de ce fait. Hors
/// mode étendu, aucune brique n'est injectée (comportement historique).
///
/// Une fois la session active, les canaux annexes se pilotent par ID via
/// [`session_chat_stream`]/[`send_chat`],
/// [`session_transfer_stream`]/[`send_files`], [`set_audio_enabled`] et
/// [`switch_monitor`].
pub fn start_session_with_options(
    config: SessionConfigDto,
    endpoint: SessionEndpointDto,
    options: SessionOptionsDto,
) -> Result<u64, String> {
    crate::flux::demarrer_session_avec_options(config, endpoint, options)
}

/// Adresse et certificat d'écoute d'une session démarrée en
/// [`SessionEndpointDto::Loopback`] (erreur pour les autres endpoints).
pub fn session_listen_info(id: u64) -> Result<ListenInfoDto, String> {
    crate::flux::info_ecoute(id)
}

/// Pousse chaque transition d'état de la session dans `sink`
/// (`Resolving` → `Connecting` → `Handshaking` → `Active` → … → `Closed`).
///
/// Les transitions déjà émises avant l'abonnement sont conservées et livrées
/// d'emblée (canal tamponné). Un seul consommateur d'états par session : ce flux
/// **ou** [`wait_session_state`]. Le drain s'arrête à la fin de la session ou à
/// l'annulation du `Stream` côté Dart.
pub fn session_state_stream(id: u64, sink: StreamSink<SessionStateDto>) -> Result<(), String> {
    crate::flux::flux_etats(id, sink)
}

/// Pousse chaque frame vidéo décodée de la session (rôle contrôleur) dans `sink`.
///
/// **C'est la fonction clé du rendu UI** : l'interface peint chaque
/// [`VideoFrameDto`] reçue. Le moteur saute les frames en retard (file bornée) :
/// un consommateur lent ne bloque jamais le décodage. Un seul consommateur vidéo
/// par session : ce flux **ou** [`collect_video_frames`].
pub fn session_video_stream(id: u64, sink: StreamSink<VideoFrameDto>) -> Result<(), String> {
    crate::flux::flux_video(id, sink)
}

/// Attend (au plus `timeout_ms`, écrêté à une heure) la prochaine transition d'état.
///
/// Renvoie `Ok(None)` si aucune transition n'arrive dans le délai ou si la session
/// est terminée (l'état final `Closed` aura été livré auparavant). Lecture
/// synchrone de repli : mutuellement exclusive avec [`session_state_stream`].
pub fn wait_session_state(id: u64, timeout_ms: u64) -> Result<Option<SessionStateDto>, String> {
    crate::flux::attendre_etat(id, timeout_ms)
}

/// Collecte jusqu'à `max_frames` frames décodées (au plus `timeout_ms`, écrêté à
/// une heure) et les renvoie d'un bloc.
///
/// Lecture synchrone de repli (tests, sondes) : mutuellement exclusive avec
/// [`session_video_stream`]. Renvoie ce qui a été reçu (possiblement moins que
/// `max_frames` si le délai expire ou si la session se termine).
pub fn collect_video_frames(
    id: u64,
    max_frames: u32,
    timeout_ms: u64,
) -> Result<Vec<VideoFrameDto>, String> {
    crate::flux::collecter_frames(id, max_frames, timeout_ms)
}

/// Instantané des statistiques de la session (fps, RTT, octets, frames).
pub fn session_stats(id: u64) -> Result<SessionStatsDto, String> {
    crate::flux::statistiques(id)
}

/// Dernière erreur d'exécution du moteur (`None` tant que la session vit ou si
/// elle s'est close proprement). À afficher quand l'état passe à `Closed`.
pub fn session_last_error(id: u64) -> Result<Option<String>, String> {
    crate::flux::derniere_erreur(id)
}

/// Pousse un événement d'entrée vers le pair (rôle contrôleur) : l'événement part
/// sur le canal `Input` chiffré du moteur.
pub fn send_input(id: u64, event: InputEventDto) -> Result<(), String> {
    crate::flux::envoyer_entree(id, event.into())
}

/// Arrête la session et la retire de la table : lève le signal d'arrêt du moteur
/// puis attend la fin de ses threads (au plus ~5 s). L'identifiant devient invalide.
pub fn stop_session(id: u64) -> Result<(), String> {
    crate::flux::arreter_session(id)
}

// ---------------------------------------------------------------------------
// Session média étendue : chat, transfert de fichiers, audio, moniteur
// ---------------------------------------------------------------------------
//
// Toutes ces fonctions n'ont d'effet que sur une session démarrée en mode
// étendu ([`SessionOptionsDto::extended_features`]) et dans la limite des
// permissions accordées ; sur une session classique elles restent inertes
// (aucune erreur, mais rien n'est émis/reçu). Elles prennent l'identifiant de
// session en premier argument, comme les autres fonctions `session_*`.

/// Pousse chaque message de chat de la session dans `sink` : messages **reçus**
/// du pair ([`ChatMessageDto::from_remote`] vrai) et **échos locaux** des
/// messages émis via [`send_chat`] (faux).
///
/// Un seul consommateur de chat par session (le drain prend définitivement le
/// récepteur). Le drain s'arrête à la fin de la session (canal déconnecté) ou à
/// l'annulation du `Stream` côté Dart.
pub fn session_chat_stream(id: u64, sink: StreamSink<ChatMessageDto>) -> Result<(), String> {
    crate::flux::flux_chat(id, sink)
}

/// Envoie un message de chat au pair (canal `Control` chiffré). L'écho local est
/// livré sur [`session_chat_stream`] une fois le message effectivement émis.
pub fn send_chat(id: u64, texte: String) -> Result<(), String> {
    crate::flux::envoyer_chat(id, texte)
}

/// Pousse chaque évènement de progression du transfert de fichiers dans `sink`
/// (début, avancement, fin par fichier, fin de file, annulation), tant côté
/// **émetteur** que **récepteur** (voir [`TransferEventDto`]).
///
/// Un seul consommateur de transfert par session. Le drain s'arrête à la fin de
/// la session ou à l'annulation du `Stream` côté Dart.
pub fn session_transfer_stream(id: u64, sink: StreamSink<TransferEventDto>) -> Result<(), String> {
    crate::flux::flux_transfert(id, sink)
}

/// Démarre l'**envoi** d'une file de fichiers vers le pair (canal `Files`) : les
/// `chemins` locaux sont émis séquentiellement, la progression est observable
/// sur [`session_transfer_stream`]. Gardé par la permission « fichiers » côté
/// émetteur.
pub fn send_files(id: u64, chemins: Vec<String>) -> Result<(), String> {
    crate::flux::envoyer_fichiers(id, chemins)
}

/// Active ou désactive l'audio de la session (émission côté hôte, lecture côté
/// contrôleur). Sans effet si la permission audio n'est pas accordée.
pub fn set_audio_enabled(id: u64, actif: bool) -> Result<(), String> {
    crate::flux::definir_audio_actif(id, actif)
}

/// Demande à l'hôte de diffuser le **moniteur** d'index donné (bascule
/// multi-écran). L'hôte applique au mieux (un index hors bornes est ignoré).
pub fn switch_monitor(id: u64, moniteur: u32) -> Result<(), String> {
    crate::flux::basculer_moniteur(id, moniteur)
}

// ---------------------------------------------------------------------------
// Hôte « accès non surveillé » : DTO
// ---------------------------------------------------------------------------

/// Demande d'accès entrante vers un hôte « accès non surveillé », poussée par
/// [`unattended_incoming_stream`] pour chaque appelant à approuver.
///
/// L'UI présente la demande (dialogue d'acceptation) puis tranche via
/// [`approve_incoming`] avec le même `peer_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingRequestDto {
    /// ID NovaDesk brut de l'appelant (à repasser à [`approve_incoming`]).
    pub peer_id: u64,
    /// ID de l'appelant au format groupé (« 123 456 789 »), prêt à afficher.
    pub peer_id_formate: String,
}

// ---------------------------------------------------------------------------
// Hôte « accès non surveillé » : cycle de vie et approbation
// ---------------------------------------------------------------------------

/// Démarre un hôte « accès non surveillé » ([`nd_core::UnattendedHost`]) : publie
/// `local_id` au serveur de `rendezvous` (adresse « ip:port »), génère une
/// identité TLS et attend les appelants en continu. Renvoie un **identifiant
/// opaque** d'hôte (distinct des identifiants de session).
///
/// Chaque appelant est soumis à **approbation pilotée par le Dart** : l'`accept`
/// du moteur bloque jusqu'à ce que l'UI réponde via [`approve_incoming`], borné
/// par un délai au-delà duquel l'appelant est **refusé par défaut** (jamais de
/// blocage indéfini). Abonnez-vous à [`unattended_incoming_stream`] pour recevoir
/// les demandes.
///
/// `stun_servers` (adresses « ip:port », liste éventuellement vide) alimente les
/// candidats de hole punching. `permissions` s'applique aux entrées reçues
/// (filtre côté contrôlé).
pub fn start_unattended_host(
    local_id: u64,
    rendezvous: String,
    stun_servers: Vec<String>,
    permissions: PermissionsDto,
) -> Result<u64, String> {
    crate::flux::demarrer_hote_non_surveille(local_id, rendezvous, stun_servers, permissions)
}

/// Pousse chaque demande d'accès entrante de l'hôte `host_id` dans `sink`
/// ([`IncomingRequestDto`]).
///
/// À brancher juste après [`start_unattended_host`] : une demande arrivée sans
/// abonné n'est pas livrée et expirera (refus par défaut). Un seul abonnement à
/// la fois (le dernier `sink` remplace le précédent).
pub fn unattended_incoming_stream(
    host_id: u64,
    sink: StreamSink<IncomingRequestDto>,
) -> Result<(), String> {
    crate::flux::flux_demandes_entrantes(host_id, sink)
}

/// Tranche une demande d'accès entrante de l'hôte `host_id` : `accepter = true`
/// débloque et sert la session, `false` la refuse.
///
/// `peer_id` est celui de la [`IncomingRequestDto`] reçue. Erreur si aucune
/// demande n'attend pour ce pair (déjà tranchée, expirée, ou jamais reçue).
pub fn approve_incoming(host_id: u64, peer_id: u64, accepter: bool) -> Result<(), String> {
    crate::flux::approuver_entrant(host_id, peer_id, accepter)
}

/// Instantané des statistiques cumulées des sessions servies par l'hôte `host_id`
/// (entrées appliquées/refusées, débit ABR, octets…). `encoder_backend` reste
/// `None` : la poignée de l'hôte non surveillé ne l'expose pas.
pub fn unattended_stats(host_id: u64) -> Result<SessionStatsDto, String> {
    crate::flux::statistiques_hote(host_id)
}

/// Arrête l'hôte `host_id` et le retire de la table : réveille toute approbation
/// en attente (refus), lève le signal d'arrêt puis attend la fin du thread de
/// service (au plus ~5 s). L'identifiant devient invalide.
pub fn stop_unattended_host(host_id: u64) -> Result<(), String> {
    crate::flux::arreter_hote_non_surveille(host_id)
}

// ===========================================================================
// État applicatif persistant (identité, carnet, réglages, historique,
// enregistrements, accès non surveillé)
// ===========================================================================
//
// Ces fonctions remplacent les données fictives de l'UI par un état **réel et
// durable** (voir [`crate::etat`] pour le stockage : JSON atomique sous le
// répertoire de données de l'application, `%APPDATA%\NovaDesk` sous Windows).
// Toutes sont **synchrones** et faillibles (`Result<_, String>`, message
// français affichable) : aucune ne pousse de flux, donc aucun nouvel encodeur
// de pont (`SseEncode`) n'est requis.

// ---------------------------------------------------------------------------
// 1. Identité locale
// ---------------------------------------------------------------------------

/// Identité locale de l'appareil, prête à afficher (écran d'accueil « votre ID »).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentityDto {
    /// `NovaId` brut à 9 chiffres, stable et persistant.
    pub id: u64,
    /// ID au format groupé (« 123 456 789 »), prêt à afficher.
    pub id_formate: String,
    /// Empreinte hexadécimale (BLAKE2s, 64 caractères) de la clé publique
    /// statique — sert à la vérification d'identité (TOFU).
    pub empreinte: String,
}

/// Renvoie l'identité locale, en la **créant et persistant** au premier appel
/// (paire de clés statiques via `nd_crypto::IdentityStore` + `NovaId` dérivé et
/// stocké). Les appels suivants rechargent exactement les mêmes valeurs.
pub fn local_identity() -> Result<LocalIdentityDto, String> {
    crate::etat::magasin().identite_locale()
}

/// Génère un mot de passe éphémère **lisible** (session ponctuelle) : 10
/// caractères d'un alphabet sans symboles ambigus. Non persisté.
#[must_use]
pub fn generate_ephemeral_password() -> String {
    crate::etat::generer_mot_de_passe_ephemere()
}

// ---------------------------------------------------------------------------
// 2. Carnet d'adresses persistant
// ---------------------------------------------------------------------------

/// Entrée du carnet d'adresses (contact enregistré).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressBookEntryDto {
    /// `NovaId` du contact.
    pub id: u64,
    /// Nom lisible donné au contact.
    pub alias: String,
    /// Groupe de rangement (chaîne vide = non groupé).
    #[serde(default)]
    pub groupe: String,
    /// Étiquettes libres associées au contact.
    #[serde(default)]
    pub etiquettes: Vec<String>,
    /// Contact marqué comme favori.
    #[serde(default)]
    pub favori: bool,
    /// Horodatage Unix (secondes) de la dernière connexion, si connue.
    #[serde(default)]
    pub derniere_connexion: Option<i64>,
}

/// Liste tous les contacts du carnet.
pub fn list_contacts() -> Result<Vec<AddressBookEntryDto>, String> {
    crate::etat::magasin().lister_contacts()
}

/// Ajoute un contact et renvoie l'entrée créée. Erreur si l'`id` existe déjà.
/// Un `groupe` non vide est automatiquement ajouté à la liste des groupes.
pub fn add_contact(
    alias: String,
    id: u64,
    groupe: String,
    etiquettes: Vec<String>,
) -> Result<AddressBookEntryDto, String> {
    crate::etat::magasin().ajouter_contact(alias, id, groupe, etiquettes)
}

/// Met à jour l'alias, le groupe et les étiquettes d'un contact existant.
/// Le favori et la dernière connexion ne sont pas touchés (voir
/// [`set_favorite`] et [`record_session`]). Erreur si l'`id` est inconnu.
pub fn update_contact(
    id: u64,
    alias: String,
    groupe: String,
    etiquettes: Vec<String>,
) -> Result<(), String> {
    crate::etat::magasin().modifier_contact(id, alias, groupe, etiquettes)
}

/// Retire un contact du carnet. Erreur si l'`id` est inconnu.
pub fn remove_contact(id: u64) -> Result<(), String> {
    crate::etat::magasin().supprimer_contact(id)
}

/// Marque (ou démarque) un contact comme favori. Erreur si l'`id` est inconnu.
pub fn set_favorite(id: u64, favori: bool) -> Result<(), String> {
    crate::etat::magasin().definir_favori(id, favori)
}

/// Liste les groupes déclarés du carnet.
pub fn list_groups() -> Result<Vec<String>, String> {
    crate::etat::magasin().lister_groupes()
}

/// Ajoute un groupe (éventuellement vide de contacts). Erreur si le nom est
/// vide ou déjà présent.
pub fn add_group(nom: String) -> Result<(), String> {
    crate::etat::magasin().ajouter_groupe(nom)
}

// ---------------------------------------------------------------------------
// 3. Réglages persistants
// ---------------------------------------------------------------------------

/// Réglage clé/valeur (les deux en texte : l'UI interprète selon la clé).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingDto {
    /// Clé du réglage (ex. `theme`, `langue`, `dossier_enregistrement`,
    /// `serveur_rendezvous`, `serveur_relais`, `serveurs_stun`,
    /// `prereglage_qualite`, `demarrer_avec_systeme`).
    pub cle: String,
    /// Valeur textuelle courante (surcharge persistée ou défaut).
    pub valeur: String,
}

/// Renvoie tous les réglages effectifs (défauts raisonnables fusionnés avec les
/// surcharges persistées), triés par clé.
pub fn get_settings() -> Result<Vec<SettingDto>, String> {
    crate::etat::magasin().get_reglages()
}

/// Valeur effective d'un réglage (`None` si la clé est inconnue). Pratique pour
/// lire une clé isolée sans parcourir [`get_settings`].
pub fn get_setting(cle: String) -> Result<Option<String>, String> {
    crate::etat::magasin().reglage(&cle)
}

/// Définit (persiste) la valeur d'un réglage. Erreur si la clé est vide.
pub fn set_setting(cle: String, valeur: String) -> Result<(), String> {
    crate::etat::magasin().definir_reglage(cle, valeur)
}

// ---------------------------------------------------------------------------
// 4. Historique de sessions
// ---------------------------------------------------------------------------

/// Une session récente (historique borné, le plus récent en tête).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentSessionDto {
    /// `NovaId` du pair joint.
    pub id: u64,
    /// Alias affiché au moment de la session.
    pub alias: String,
    /// Horodatage Unix (secondes) du démarrage de la session.
    pub timestamp: i64,
}

/// Journalise le démarrage d'une session (à appeler au moment de se connecter) :
/// ajoute/rafraîchit l'entrée en tête de l'historique (dédupliquée par `id`,
/// bornée) et met à jour la dernière connexion du contact correspondant.
pub fn record_session(id: u64, alias: String) -> Result<(), String> {
    crate::etat::magasin().enregistrer_session(id, alias)
}

/// Renvoie les sessions récentes, de la plus récente à la plus ancienne.
pub fn recent_sessions() -> Result<Vec<RecentSessionDto>, String> {
    crate::etat::magasin().sessions_recentes()
}

// ---------------------------------------------------------------------------
// 5. Enregistrements
// ---------------------------------------------------------------------------

/// Description d'un fichier d'enregistrement présent sur le disque.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingDto {
    /// Chemin absolu du fichier.
    pub chemin: String,
    /// Nom de fichier seul.
    pub nom: String,
    /// Date de modification du fichier (horodatage Unix, secondes).
    pub date: i64,
    /// Durée en secondes (lue via `nd_features::Mp4Reader` pour un `.mp4` ;
    /// `0.0` si inconnue).
    pub duree_s: f64,
    /// Taille du fichier en octets.
    pub taille_octets: u64,
}

/// Liste les enregistrements (`.mp4`/`.ndr`) d'un dossier — `dir` s'il est
/// fourni, sinon le réglage `dossier_enregistrement`, sinon
/// `<répertoire de données>/enregistrements`. Un dossier absent renvoie une
/// liste vide. Les fichiers sont triés du plus récent au plus ancien.
pub fn list_recordings(dir: Option<String>) -> Result<Vec<RecordingDto>, String> {
    crate::etat::magasin().lister_enregistrements(dir)
}

// ---------------------------------------------------------------------------
// 6. Accès non surveillé persistant
// ---------------------------------------------------------------------------

/// Configuration d'accès non surveillé, sans jamais exposer le secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedConfigDto {
    /// Un mot de passe permanent est configuré (seul un hachage salé est stocké).
    pub a_mot_de_passe: bool,
    /// `NovaId` des appareils de confiance.
    pub appareils_de_confiance: Vec<u64>,
}

/// Une entrée du journal des accès non surveillés.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessLogEntryDto {
    /// `NovaId` brut de l'appelant.
    pub peer_id: u64,
    /// ID de l'appelant au format groupé, prêt à afficher.
    pub peer_id_formate: String,
    /// Horodatage Unix (secondes) de l'accès.
    pub timestamp: i64,
    /// Vrai si l'accès a été accepté, faux s'il a été refusé.
    pub accepte: bool,
}

/// Renvoie la configuration d'accès non surveillé.
pub fn unattended_config() -> Result<UnattendedConfigDto, String> {
    crate::etat::magasin().config_non_surveille()
}

/// Définit le mot de passe permanent d'accès non surveillé (stocké **haché et
/// salé**, jamais en clair). Un mot de passe vide efface la configuration.
pub fn set_unattended_password(pwd: String) -> Result<(), String> {
    crate::etat::magasin().definir_mot_de_passe_non_surveille(pwd)
}

/// Vérifie un mot de passe candidat contre le hachage stocké (`false` si aucun
/// mot de passe n'est configuré).
pub fn verify_unattended_password(pwd: String) -> Result<bool, String> {
    crate::etat::magasin().verifier_mot_de_passe_non_surveille(pwd)
}

/// Ajoute un appareil à la liste de confiance (sans effet s'il y figure déjà).
pub fn add_trusted_device(id: u64) -> Result<(), String> {
    crate::etat::magasin().ajouter_appareil_confiance(id)
}

/// Retire un appareil de la liste de confiance. Erreur s'il n'y figure pas.
pub fn remove_trusted_device(id: u64) -> Result<(), String> {
    crate::etat::magasin().retirer_appareil_confiance(id)
}

/// Ajoute une entrée au journal des accès (append) : à appeler quand une
/// demande d'accès non surveillé est tranchée.
pub fn record_access(peer_id: u64, accepte: bool) -> Result<(), String> {
    crate::etat::magasin().enregistrer_acces(peer_id, accepte)
}

/// Renvoie le journal des accès, du plus récent au plus ancien.
pub fn access_log() -> Result<Vec<AccessLogEntryDto>, String> {
    crate::etat::magasin().journal_acces()
}

// ===========================================================================
// Wake-on-LAN
// ===========================================================================

/// Réveille un appareil par **Wake-on-LAN** : construit le « paquet magique »
/// pour l'adresse MAC `mac` et l'émet en UDP (voir `nd_features::send_wol`).
///
/// * `mac` : adresse MAC de la carte réseau cible, écrite avec `:` ou `-`
///   (« 01:23:45:67:89:AB » ou « 01-23-45-67-89-ab », casse indifférente).
/// * `broadcast` : cible « ip:port » du paquet ; `None` (ou chaîne vide) diffuse
///   vers `255.255.255.255:9` — diffusion limitée au sous-réseau local, port
///   « discard » (9) habituel du Wake-on-LAN.
///
/// Renvoie un message d'erreur **français clair** si la MAC est mal formée, si
/// l'adresse de diffusion est invalide, ou si l'émission UDP échoue. Fonction
/// **synchrone** à DTO plats (`String`, `Option<String>`).
pub fn send_wol(mac: String, broadcast: Option<String>) -> Result<(), String> {
    // Les messages de `MacAddr` sont déjà clairs et en français ; on retire le
    // préfixe technique « protocole : » qu'ajoute l'affichage de `NdError`.
    let adresse_mac: MacAddr = mac.parse().map_err(|e| match e {
        NdError::Protocol(message) => message,
        autre => autre.to_string(),
    })?;

    // Cible : « ip:port » fournie, sinon diffusion limitée sur le port discard.
    let cible = match broadcast {
        Some(texte) if !texte.trim().is_empty() => {
            texte.trim().parse::<SocketAddr>().map_err(|e| {
                format!("adresse de diffusion « {texte} » invalide (attendu « ip:port ») : {e}")
            })?
        }
        _ => nd_features::limited_broadcast(nd_features::WOL_PORT_DISCARD),
    };

    nd_features::send_wol(adresse_mac, cible)
        .map_err(|e| format!("envoi du Wake-on-LAN impossible : {e}"))
}

#[cfg(test)]
mod tests_wol {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::time::Duration;

    #[test]
    fn send_wol_refuse_une_mac_invalide() {
        let erreur = send_wol("pas-une-mac".to_owned(), None).expect_err("MAC invalide refusée");
        assert!(
            erreur.contains("MAC"),
            "message peu clair pour une MAC invalide : {erreur}"
        );
        // Le préfixe technique « protocole : » de `NdError` ne fuite pas à l'UI.
        assert!(
            !erreur.starts_with("protocole :"),
            "préfixe technique non retiré : {erreur}"
        );
    }

    #[test]
    fn send_wol_refuse_une_adresse_de_diffusion_invalide() {
        let erreur = send_wol(
            "01:23:45:67:89:AB".to_owned(),
            Some("pas-ip-port".to_owned()),
        )
        .expect_err("diffusion invalide refusée");
        assert!(
            erreur.contains("diffusion"),
            "message peu clair pour une diffusion invalide : {erreur}"
        );
    }

    #[test]
    fn send_wol_emet_le_paquet_magique_vers_la_cible() {
        // `wake_on_lan` ouvre le socket en SO_BROADCAST mais émet aussi bien vers
        // une adresse unicast (loopback) : pas de diffusion réelle nécessaire.
        let recepteur = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("socket récepteur");
        recepteur
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("délai de lecture");
        let cible = recepteur.local_addr().expect("adresse locale");

        send_wol("de:ad:be:ef:00:42".to_owned(), Some(cible.to_string())).expect("émission WoL");

        let mut tampon = [0u8; 128];
        let (recus, _) = recepteur.recv_from(&mut tampon).expect("réception");
        let attendu = nd_features::magic_packet([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x42]);
        assert_eq!(&tampon[..recus], &attendu[..]);
    }

    #[test]
    fn send_wol_accepte_le_format_a_tirets() {
        // Format à tirets accepté par `MacAddr` ; émission vers un récepteur local.
        let recepteur = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("socket récepteur");
        recepteur
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("délai de lecture");
        let cible = recepteur.local_addr().expect("adresse locale");

        send_wol("01-23-45-67-89-ab".to_owned(), Some(cible.to_string()))
            .expect("MAC à tirets acceptée");
        let mut tampon = [0u8; 128];
        let (recus, _) = recepteur.recv_from(&mut tampon).expect("réception");
        let attendu = nd_features::magic_packet([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB]);
        assert_eq!(&tampon[..recus], &attendu[..]);
    }
}

// ===========================================================================
// Capacités moteur avancées : confidentialité, cadre d'écran, tunnel TCP,
// annotations / tableau blanc, relecture d'enregistrement
// ===========================================================================
//
// Ces fonctions mettent à portée du Dart des capacités **déjà implémentées** —
// dans le moteur ([`nd_core::SessionHandle`]) pour les quatre premières, dans
// [`nd_features`]/[`nd_codec`] pour la relecture — jusqu'ici inatteignables
// depuis l'UI. Comme les autres fonctions `session_*`, celles qui pilotent une
// session prennent son identifiant opaque en premier argument ; elles restent
// inertes hors mode étendu ou permission absente (mais renvoient `Ok` tant que
// la session existe), à l'exception du tunnel qui lie un écouteur réel.

// ---------------------------------------------------------------------------
// 1. Mode confidentialité
// ---------------------------------------------------------------------------

/// Active (ou lève) le **mode confidentialité** de la session : côté contrôleur,
/// une demande est transmise à l'hôte qui — s'il détient la capacité — cesse de
/// diffuser son écran réel (cadre noir) ; côté hôte, le rideau est appliqué
/// directement. L'état effectif se lit via [`privacy_active`]. Sans effet hors
/// mode étendu ([`SessionOptionsDto::extended_features`]).
pub fn set_privacy(session_id: u64, actif: bool) -> Result<(), String> {
    crate::flux::definir_confidentialite(session_id, actif)
}

/// État du mode confidentialité **connu localement** : côté contrôleur, le
/// dernier drapeau annoncé par l'hôte (l'indicateur « rideau actif » à
/// afficher) ; côté hôte, le rideau qu'il applique.
pub fn privacy_active(session_id: u64) -> Result<bool, String> {
    crate::flux::confidentialite_active(session_id)
}

// ---------------------------------------------------------------------------
// 2. Cadre d'écran (région)
// ---------------------------------------------------------------------------

/// Zone rectangulaire d'écran à partager (« cadre d'écran »), en **pixels du
/// moniteur** de l'hôte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionDto {
    /// Abscisse du coin supérieur gauche.
    pub x: u32,
    /// Ordonnée du coin supérieur gauche.
    pub y: u32,
    /// Largeur de la zone, en pixels.
    pub largeur: u32,
    /// Hauteur de la zone, en pixels.
    pub hauteur: u32,
}

/// Restreint la zone d'écran partagée au `RegionDto` fourni, ou **rétablit le
/// plein écran** avec `None`. Côté contrôleur, la demande est transmise à
/// l'hôte, qui l'applique au mieux ; côté hôte, elle est appliquée directement.
/// Sans effet hors mode étendu.
pub fn set_session_region(session_id: u64, region: Option<RegionDto>) -> Result<(), String> {
    crate::flux::definir_region(session_id, region.map(|r| (r.x, r.y, r.largeur, r.hauteur)))
}

/// Cadre d'écran actuellement demandé (`None` = plein écran). Côté hôte, reflète
/// la demande reçue du contrôleur — utile pour prouver qu'une commande de région
/// a bien traversé la session, ou pour refléter l'état dans l'UI.
pub fn session_requested_region(session_id: u64) -> Result<Option<RegionDto>, String> {
    Ok(
        crate::flux::region_demandee(session_id)?.map(|(x, y, largeur, hauteur)| RegionDto {
            x,
            y,
            largeur,
            hauteur,
        }),
    )
}

// ---------------------------------------------------------------------------
// 3. Tunnel TCP de session
// ---------------------------------------------------------------------------

/// Tunnel TCP de session ouvert : coordonnées de l'écouteur local à utiliser
/// côté contrôleur (renvoyé par [`open_tunnel`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelOuvertDto {
    /// Adresse locale réellement écoutée (« 127.0.0.1:port », port résolu si le
    /// port demandé était `0`).
    pub adresse_locale: String,
    /// Port local réellement écouté (pratique pour l'UI sans reparser l'adresse).
    pub port_local: u16,
}

/// Ouvre un **tunnel TCP de session** : écoute sur `127.0.0.1:port_local`
/// (`port_local = 0` → port éphémère) et relaie chaque connexion locale vers
/// `cible` (« ip:port ») **à travers le canal fiable de la session** (l'hôte
/// compose la connexion réelle vers la cible). Renvoie l'adresse locale écoutée.
///
/// La durée de vie du tunnel est gérée par la façade : la poignée est conservée
/// jusqu'à [`close_tunnels`] ou l'arrêt de la session ([`stop_session`]).
/// Best-effort : exige le mode étendu et la capacité côté hôte. Erreur française
/// claire si `cible` n'est pas « ip:port » ou si l'écouteur local ne peut être
/// lié (port pris, droits…).
pub fn open_tunnel(
    session_id: u64,
    port_local: u16,
    cible: String,
) -> Result<TunnelOuvertDto, String> {
    crate::flux::ouvrir_tunnel(session_id, port_local, cible)
}

/// Ferme **tous** les tunnels TCP ouverts pour la session (cesse d'accepter de
/// nouvelles connexions locales, joint les fils d'acceptation). Idempotent :
/// aucune erreur si la session n'a aucun tunnel. Les tunnels sont aussi fermés
/// automatiquement à l'arrêt de la session.
pub fn close_tunnels(session_id: u64) -> Result<(), String> {
    crate::flux::fermer_tunnels(session_id)
}

// ---------------------------------------------------------------------------
// 4. Annotations / tableau blanc
// ---------------------------------------------------------------------------

/// Un **trait d'annotation** (« tableau blanc ») dessiné par-dessus l'image,
/// sous forme plate — miroir d'un [`nd_features::Stroke`].
///
/// # Conventions
///
/// * `genre` sélectionne la forme : `0` = trait libre / polyligne, `1` =
///   rectangle, `2` = ellipse, `3` = flèche, `4` = texte.
/// * `points` est une liste plate de coordonnées `[x0, y0, x1, y1, …]`,
///   **normalisées** dans `0.0..=1.0` (repère partagé émetteur/récepteur). Le
///   nombre de points attendu dépend du `genre` : trait libre = 1 point ou plus
///   (la polyligne) ; rectangle = 2 points (coins opposés) ; ellipse = 2 points
///   (centre puis demi-axes `rx`,`ry`) ; flèche = 2 points (origine, pointe) ;
///   texte = 1 point (position). Un nombre incorrect est refusé.
/// * `couleur_argb` est empaquetée **ARGB** (`0xAARRGGBB`, convention `Color` de
///   Flutter). La façade la convertit vers le RGBA interne et inversement.
/// * `epaisseur` est l'épaisseur du tracé ; pour le **texte**, c'est la hauteur
///   de police (`size`).
/// * `texte` ne concerne que le genre texte (`4`) ; il y est **requis** et
///   ignoré (laissé `None`) pour les autres genres.
///
/// **Champs non représentables** : une couche reçue peut porter plusieurs traits
/// et chacun a un identifiant stable interne (gomme ciblée) ; le DTO plat, lui,
/// décrit **un seul** trait sans identifiant. À la réception
/// ([`session_annotation_stream`]), une couche de `n` traits est donc livrée
/// comme `n` `AnnotationDto` successifs.
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationDto {
    /// Forme du trait (voir la doc du type).
    pub genre: i32,
    /// Coordonnées plates `[x0, y0, x1, y1, …]`, normalisées `0.0..=1.0`.
    pub points: Vec<f32>,
    /// Couleur ARGB empaquetée (`0xAARRGGBB`).
    pub couleur_argb: u32,
    /// Épaisseur du tracé (ou hauteur de police pour le texte).
    pub epaisseur: f32,
    /// Contenu textuel (genre texte uniquement ; requis pour lui, sinon `None`).
    pub texte: Option<String>,
}

// Genres d'annotation : miroir plat des variantes de [`nd_features::Stroke`].
const ANNOTATION_TRAIT_LIBRE: i32 = 0;
const ANNOTATION_RECTANGLE: i32 = 1;
const ANNOTATION_ELLIPSE: i32 = 2;
const ANNOTATION_FLECHE: i32 = 3;
const ANNOTATION_TEXTE: i32 = 4;

/// Couleur ARGB empaquetée (`0xAARRGGBB`, convention `Color` de Flutter) →
/// RGBA (`0xRRGGBBAA`, convention interne [`nd_features::Stroke`]). Les deux
/// formats ne diffèrent que d'une rotation d'octet : l'alpha de tête passe en
/// queue.
fn argb_vers_rgba(argb: u32) -> u32 {
    argb.rotate_left(8)
}

/// Couleur RGBA (`0xRRGGBBAA`) → ARGB (`0xAARRGGBB`) : rotation inverse.
fn rgba_vers_argb(rgba: u32) -> u32 {
    rgba.rotate_right(8)
}

/// Regroupe une liste plate `[x0, y0, x1, y1, …]` en points `(x, y)`.
fn en_points(coords: &[f32]) -> Vec<(f32, f32)> {
    coords.chunks_exact(2).map(|p| (p[0], p[1])).collect()
}

/// Aplatit des points `(x, y)` en `[x0, y0, x1, y1, …]`.
fn aplatir(points: &[(f32, f32)]) -> Vec<f32> {
    points.iter().flat_map(|(x, y)| [*x, *y]).collect()
}

/// Exige exactement 2 points (message d'erreur situant la forme fautive).
fn deux_points(points: &[(f32, f32)], quoi: &str) -> Result<[(f32, f32); 2], String> {
    match points {
        [a, b] => Ok([*a, *b]),
        _ => Err(format!(
            "annotation « {quoi} » : exactement 2 points (4 coordonnées) attendus, {} reçu(s)",
            points.len()
        )),
    }
}

/// Exige exactement 1 point.
fn un_point(points: &[(f32, f32)], quoi: &str) -> Result<[(f32, f32); 1], String> {
    match points {
        [a] => Ok([*a]),
        _ => Err(format!(
            "annotation « {quoi} » : exactement 1 point (2 coordonnées) attendu, {} reçu(s)",
            points.len()
        )),
    }
}

/// Construit le [`nd_features::Stroke`] correspondant au DTO plat, en validant
/// le genre et le nombre de points.
fn stroke_depuis_dto(dto: &AnnotationDto) -> Result<Stroke, String> {
    if !dto.points.len().is_multiple_of(2) {
        return Err(format!(
            "annotation : nombre impair de coordonnées ({}) — attendu des paires (x, y)",
            dto.points.len()
        ));
    }
    let couleur = argb_vers_rgba(dto.couleur_argb);
    let points = en_points(&dto.points);
    match dto.genre {
        ANNOTATION_TRAIT_LIBRE => {
            if points.is_empty() {
                return Err(
                    "annotation « trait libre » : au moins un point (2 coordonnées) requis"
                        .to_owned(),
                );
            }
            Ok(Stroke::Line {
                points,
                color: couleur,
                width: dto.epaisseur,
            })
        }
        ANNOTATION_RECTANGLE => {
            let [min, max] = deux_points(&points, "rectangle")?;
            Ok(Stroke::Rect {
                min,
                max,
                color: couleur,
                width: dto.epaisseur,
            })
        }
        ANNOTATION_ELLIPSE => {
            let [centre, rayons] = deux_points(&points, "ellipse")?;
            Ok(Stroke::Ellipse {
                center: centre,
                radii: rayons,
                color: couleur,
                width: dto.epaisseur,
            })
        }
        ANNOTATION_FLECHE => {
            let [de, vers] = deux_points(&points, "flèche")?;
            Ok(Stroke::Arrow {
                from: de,
                to: vers,
                color: couleur,
                width: dto.epaisseur,
            })
        }
        ANNOTATION_TEXTE => {
            let [position] = un_point(&points, "texte")?;
            let contenu = dto
                .texte
                .clone()
                .ok_or_else(|| "annotation « texte » : le champ `texte` est requis".to_owned())?;
            Ok(Stroke::Text {
                position,
                contenu,
                color: couleur,
                size: dto.epaisseur,
            })
        }
        autre => Err(format!(
            "genre d'annotation inconnu : {autre} (attendu 0=trait libre, 1=rectangle, \
             2=ellipse, 3=flèche, 4=texte)"
        )),
    }
}

/// Aplatit un [`nd_features::Stroke`] en DTO plat (l'identifiant de couche est
/// perdu — voir la doc d'[`AnnotationDto`]).
fn dto_depuis_stroke(stroke: &Stroke) -> AnnotationDto {
    match stroke {
        Stroke::Line {
            points,
            color,
            width,
        } => AnnotationDto {
            genre: ANNOTATION_TRAIT_LIBRE,
            points: aplatir(points),
            couleur_argb: rgba_vers_argb(*color),
            epaisseur: *width,
            texte: None,
        },
        Stroke::Rect {
            min,
            max,
            color,
            width,
        } => AnnotationDto {
            genre: ANNOTATION_RECTANGLE,
            points: vec![min.0, min.1, max.0, max.1],
            couleur_argb: rgba_vers_argb(*color),
            epaisseur: *width,
            texte: None,
        },
        Stroke::Ellipse {
            center,
            radii,
            color,
            width,
        } => AnnotationDto {
            genre: ANNOTATION_ELLIPSE,
            points: vec![center.0, center.1, radii.0, radii.1],
            couleur_argb: rgba_vers_argb(*color),
            epaisseur: *width,
            texte: None,
        },
        Stroke::Arrow {
            from,
            to,
            color,
            width,
        } => AnnotationDto {
            genre: ANNOTATION_FLECHE,
            points: vec![from.0, from.1, to.0, to.1],
            couleur_argb: rgba_vers_argb(*color),
            epaisseur: *width,
            texte: None,
        },
        Stroke::Text {
            position,
            contenu,
            color,
            size,
        } => AnnotationDto {
            genre: ANNOTATION_TEXTE,
            points: vec![position.0, position.1],
            couleur_argb: rgba_vers_argb(*color),
            epaisseur: *size,
            texte: Some(contenu.clone()),
        },
    }
}

/// Construit une couche d'annotation (**un seul trait**) depuis le DTO plat.
/// Utilisée par [`send_annotation`] ; exposée à la crate pour le drain du flux.
pub(crate) fn couche_depuis_annotation(dto: &AnnotationDto) -> Result<AnnotationLayer, String> {
    let mut couche = AnnotationLayer::new();
    couche.add(stroke_depuis_dto(dto)?);
    Ok(couche)
}

/// Aplatit une couche reçue en **un [`AnnotationDto`] par trait** (ordre de
/// dessin). Utilisée par le drain de [`session_annotation_stream`].
pub(crate) fn annotations_depuis_couche(couche: &AnnotationLayer) -> Vec<AnnotationDto> {
    couche
        .strokes()
        .iter()
        .map(|(_id, stroke)| dto_depuis_stroke(stroke))
        .collect()
}

/// Envoie une annotation au pair (canal `Control` chiffré) : le trait décrit par
/// `annotation` est émis comme une couche à un seul trait. Les annotations
/// reçues du pair arrivent sur [`session_annotation_stream`]. Sans effet hors
/// mode étendu. Erreur si le DTO est mal formé (genre inconnu, points
/// incohérents, texte manquant).
pub fn send_annotation(session_id: u64, annotation: AnnotationDto) -> Result<(), String> {
    crate::flux::envoyer_annotation(session_id, annotation)
}

/// Pousse chaque annotation **reçue** du pair dans `sink`, à raison d'un
/// [`AnnotationDto`] par trait de la couche reçue (voir [`AnnotationDto`]).
///
/// Un seul consommateur d'annotations par session (le drain prend définitivement
/// le récepteur). Le drain s'arrête à la fin de la session (canal déconnecté) ou
/// à l'annulation du `Stream` côté Dart.
pub fn session_annotation_stream(
    session_id: u64,
    sink: StreamSink<AnnotationDto>,
) -> Result<(), String> {
    crate::flux::flux_annotations(session_id, sink)
}

// ---------------------------------------------------------------------------
// 5. Relecture d'enregistrement
// ---------------------------------------------------------------------------

/// Métadonnées d'un enregistrement ouvert pour relecture, renvoyées par
/// [`open_recording`]. `id` indexe le lecteur pour [`recording_next_frame`],
/// [`recording_seek`] et [`close_recording`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingInfoDto {
    /// Identifiant opaque du lecteur (à repasser aux autres fonctions `recording_*`).
    pub id: u64,
    /// Largeur des images, en pixels.
    pub largeur: u32,
    /// Hauteur des images, en pixels.
    pub hauteur: u32,
    /// Cadence nominale, en images par seconde.
    pub fps: u32,
    /// Durée de l'enregistrement, en microsecondes.
    pub duree_us: u64,
    /// Nombre d'images de l'enregistrement.
    pub nb_images: u64,
}

/// Ouvre un enregistrement (`.mp4` **ou** archive interne `.ndr`, format
/// auto-détecté) pour relecture et renvoie ses métadonnées + un identifiant
/// opaque. L'enregistrement vit jusqu'à [`close_recording`].
pub fn open_recording(chemin: String) -> Result<RecordingInfoDto, String> {
    crate::lecture::ouvrir(chemin)
}

/// Décode et renvoie la **prochaine image** de l'enregistrement `id` (RGBA,
/// même [`VideoFrameDto`] que [`session_video_stream`]), ou `Ok(None)` en fin de
/// flux. Les échantillons qui ne produisent pas d'image sont sautés.
pub fn recording_next_frame(id: u64) -> Result<Option<VideoFrameDto>, String> {
    crate::lecture::image_suivante(id)
}

/// Repositionne la lecture sur l'**image-clé** la plus proche avant (ou à)
/// `timestamp_us` (point de reprise décodable) et réinitialise le décodeur. Le
/// prochain [`recording_next_frame`] repart de cette image-clé.
pub fn recording_seek(id: u64, timestamp_us: u64) -> Result<(), String> {
    crate::lecture::chercher(id, timestamp_us)
}

/// Ferme l'enregistrement `id` et libère ses ressources. L'identifiant devient
/// invalide.
pub fn close_recording(id: u64) -> Result<(), String> {
    crate::lecture::fermer(id)
}

// ===========================================================================
// Extras session & relecture
// ===========================================================================
//
// Contrôles de session sous les noms `session_*` attendus par l'UI (mêmes
// chemins `flux` que le lot « capacités moteur exposées », qui reste intact) et
// relecture d'enregistrement **en flux poussé**. Tout ce lot est **sans nouvel
// encodeur de pont** : les quatre contrôles sont synchrones à DTO plats déjà
// bridgés ([`RegionDto`], [`AnnotationDto`]), [`recording_info`] réutilise
// [`RecordingInfoDto`], et [`recording_frame_stream`] réutilise le
// `StreamSink<VideoFrameDto>` de [`session_video_stream`] (son `SseEncode` est
// déjà généré) — donc **aucun `pont_provisoire` n'est requis** ; la
// régénération ne fera qu'ajouter le câblage des nouvelles fonctions.

/// Active (ou lève) le **mode confidentialité** de la session `session_id` —
/// même effet que [`set_privacy`], sous le nom `session_*` attendu par l'UI.
///
/// Côté contrôleur, la demande est transmise à l'hôte qui — s'il détient la
/// capacité — cesse de diffuser son écran réel (cadre noir) ; côté hôte, le
/// rideau est appliqué directement. Best-effort : sans effet hors mode étendu
/// ([`SessionOptionsDto::extended_features`]) ou sans la capacité, mais `Ok`
/// tant que la session existe. L'état effectif se lit via [`privacy_active`].
pub fn session_set_privacy(session_id: u64, actif: bool) -> Result<(), String> {
    crate::flux::definir_confidentialite(session_id, actif)
}

/// Restreint la zone d'écran partagée de la session `session_id` au
/// [`RegionDto`] fourni (pixels du moniteur de l'hôte), ou **rétablit le plein
/// écran** avec `None` — même effet que [`set_session_region`], sous le nom
/// `session_*` attendu par l'UI.
///
/// Côté contrôleur, la demande est transmise à l'hôte, qui l'applique au mieux ;
/// côté hôte, elle est appliquée directement. Sans effet hors mode étendu. La
/// région effectivement demandée se relit via [`session_requested_region`].
pub fn session_set_region(session_id: u64, region: Option<RegionDto>) -> Result<(), String> {
    crate::flux::definir_region(session_id, region.map(|r| (r.x, r.y, r.largeur, r.hauteur)))
}

/// Envoie une annotation (« tableau blanc ») au pair de la session
/// `session_id` — même effet que [`send_annotation`], sous le nom `session_*`
/// attendu par l'UI.
///
/// Le trait décrit par `annotation` (genre, points normalisés `0.0..=1.0`,
/// couleur ARGB, épaisseur — voir [`AnnotationDto`]) part comme une couche à un
/// seul trait sur le canal `Control` chiffré ; les annotations du pair arrivent
/// sur [`session_annotation_stream`]. Erreur si le DTO est mal formé (genre
/// inconnu, points incohérents, texte manquant) ; sans effet hors mode étendu.
pub fn session_send_annotation(session_id: u64, annotation: AnnotationDto) -> Result<(), String> {
    crate::flux::envoyer_annotation(session_id, annotation)
}

/// Ouvre un **tunnel TCP de session** vers `hote_distant:port_distant` — comme
/// [`open_tunnel`], mais avec l'hôte et le port distants **séparés** et sans
/// valeur de retour. L'UI qui veut connaître l'adresse locale réellement
/// écoutée (notamment avec `port_local = 0`, port éphémère) passera par
/// [`open_tunnel`].
///
/// Écoute sur `127.0.0.1:port_local` et relaie chaque connexion locale vers la
/// cible **à travers le canal fiable de la session** (l'hôte compose la
/// connexion réelle vers la cible). `hote_distant` est une adresse IP (v4 ou
/// v6) ; erreur française claire sinon. Exige le mode étendu et la capacité
/// côté hôte. La poignée du tunnel est conservée par la façade jusqu'à
/// [`close_tunnels`] ou l'arrêt de la session ([`stop_session`]).
pub fn session_open_tunnel(
    session_id: u64,
    port_local: u16,
    hote_distant: String,
    port_distant: u16,
) -> Result<(), String> {
    // L'analyse de l'hôte précède la recherche de session : une saisie invalide
    // échoue avec un message clair, sans toucher à la session.
    let ip: IpAddr = hote_distant.trim().parse().map_err(|e| {
        format!(
            "hôte distant « {hote_distant} » invalide \
             (adresse IP attendue, ex. « 192.168.1.10 ») : {e}"
        )
    })?;
    crate::flux::ouvrir_tunnel_vers(session_id, port_local, SocketAddr::new(ip, port_distant))
        .map(|_tunnel| ())
}

/// Métadonnées de l'enregistrement `chemin` (`.mp4` **ou** `.ndr`, format
/// auto-détecté) **sans lecteur durable** : le fichier est lu puis refermé
/// aussitôt, rien à fermer ensuite — contrairement à [`open_recording`], qui
/// garde un lecteur ouvert pour décoder.
///
/// Le champ [`RecordingInfoDto::id`] du résultat vaut `0`, valeur jamais
/// attribuée à un lecteur réel : il n'est **pas** utilisable avec les fonctions
/// `recording_*` à identifiant.
pub fn recording_info(chemin: String) -> Result<RecordingInfoDto, String> {
    crate::lecture::infos(chemin)
}

/// Relit l'enregistrement `chemin` en **flux poussé** : chaque image décodée
/// (RGBA, même [`VideoFrameDto`] que [`session_video_stream`]) est poussée dans
/// `sink` dans l'ordre de présentation, jusqu'à la fin du fichier.
///
/// L'ouverture du fichier, l'extraction des échantillons H.264 et la création
/// du décodeur sont **synchrones** (erreur immédiate) ; le décodage se fait
/// ensuite sur un thread dédié. Le `Stream` Dart se clôt en fin
/// d'enregistrement ; une erreur de décodage lui est signalée comme erreur de
/// flux ; son annulation côté Dart arrête la relecture. Alternative **tirée**
/// (image par image, recherche par horodatage) : [`open_recording`] /
/// [`recording_next_frame`] / [`recording_seek`].
pub fn recording_frame_stream(
    chemin: String,
    sink: StreamSink<VideoFrameDto>,
) -> Result<(), String> {
    crate::lecture::flux_images(chemin, sink)
}

// ===========================================================================
// Plan de contrôle de session
// ===========================================================================
//
// Cinq capacités que l'UI ne pouvait pas encore piloter, chacune additive sur
// le canal `Control` existant (ou locale à l'hôte pour l'enregistrement) et
// gardée par les permissions le cas échéant. Toutes **synchrones à DTO plats**
// (aucun `StreamSink`) : aucun `pont_provisoire` requis — la régénération ne
// fera qu'ajouter le câblage des nouvelles fonctions et des DTO neufs
// ([`MonitorInfoDto`], [`PeerInfoDto`]). Comme les autres `session_*`, elles
// prennent l'identifiant opaque de session en premier argument et restent
// inertes hors mode étendu ([`SessionOptionsDto::extended_features`]) ou
// permission absente (mais renvoient `Ok` tant que la session existe), à
// l'exception des lectures ([`session_monitors`], [`session_peer_info`]).

// ---------------------------------------------------------------------------
// 1. Permissions à chaud
// ---------------------------------------------------------------------------

/// Renégocie **une** permission de la session **en cours** : accorde
/// (`autorise = true`) ou retire (`false`) la capacité `capacite` de l'ensemble
/// vivant. Côté contrôleur, la demande est transmise à l'hôte, qui l'applique au
/// vol — le filtre d'injection lit le nouvel ensemble à l'entrée suivante ; côté
/// hôte, elle est appliquée directement. Sans effet hors mode étendu.
///
/// `capacite` est une **clé stable** (contrat UI) parmi : `voir_ecran`,
/// `souris`, `clavier`, `presse_papiers_lecture`, `presse_papiers_ecriture`,
/// `fichiers_envoi`, `fichiers_reception`, `audio`, `redemarrage`,
/// `enregistrement`, `confidentialite`, `tunnel`. Toute autre valeur renvoie une
/// erreur française explicite (sans toucher à la session).
pub fn session_set_permission(
    session_id: u64,
    capacite: String,
    autorise: bool,
) -> Result<(), String> {
    crate::flux::definir_permission(session_id, &capacite, autorise)
}

// ---------------------------------------------------------------------------
// 2. Préréglage de qualité
// ---------------------------------------------------------------------------

/// Applique un **préréglage de qualité** à l'encodeur hôte : `preset` parmi
/// `auto`, `fluide`, `equilibre`, `netteté` (mappé vers un profil ABR
/// `ContentProfile` et un plafond de débit). Côté contrôleur, la demande est
/// transmise à l'hôte, qui reconfigure son encodeur et son échelle ABR **sous**
/// le plafond (l'ABR continue de dégrader à partir de là) ; côté hôte, elle est
/// appliquée directement. Un `preset` inconnu renvoie une erreur française. Sans
/// effet hors mode étendu.
///
/// | preset | profil ABR | plafond |
/// |--------|-----------|---------|
/// | `auto` | netteté (texte) | aucun |
/// | `fluide` | fluidité (vidéo) | aucun |
/// | `equilibre` | fluidité (vidéo) | 5 000 kbit/s |
/// | `netteté` | netteté (texte) | aucun |
pub fn session_set_quality(session_id: u64, preset: String) -> Result<(), String> {
    crate::flux::definir_qualite(session_id, &preset)
}

// ---------------------------------------------------------------------------
// 3. Enregistrement à chaud
// ---------------------------------------------------------------------------

/// Démarre (avec un `chemin` MP4) ou arrête (`None`) l'**enregistrement local**
/// de l'hôte **en cours de session** (jusqu'ici figé au démarrage via
/// `SessionOptionsDto::recording_path`). Démarrer ouvre une nouvelle époque MP4 ;
/// arrêter clôt proprement le fichier (relisible). L'enregistrement est
/// **côté hôte** (l'hôte encode et muxe son écran) : à appeler sur une session
/// hôte, sans effet côté contrôleur ni hors mode étendu. Erreur si la session
/// est inconnue.
pub fn session_set_recording(session_id: u64, chemin: Option<String>) -> Result<(), String> {
    crate::flux::definir_enregistrement(session_id, chemin.map(PathBuf::from))
}

// ---------------------------------------------------------------------------
// 4. Liste des moniteurs
// ---------------------------------------------------------------------------

/// Un écran de l'hôte publié sur le plan de contrôle (miroir plat de
/// `nd_core::RemoteMonitor`) : **remplace l'« Écran 1/2 » codé en dur** de l'UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorInfoDto {
    /// Index du moniteur — l'argument attendu par [`switch_monitor`].
    pub index: u32,
    /// Largeur en pixels.
    pub largeur: u32,
    /// Hauteur en pixels.
    pub hauteur: u32,
    /// Vrai pour le moniteur principal.
    pub principal: bool,
}

impl From<RemoteMonitor> for MonitorInfoDto {
    fn from(m: RemoteMonitor) -> Self {
        MonitorInfoDto {
            index: m.index,
            largeur: m.width,
            hauteur: m.height,
            principal: m.primary,
        }
    }
}

/// Liste des **moniteurs réels** de l'hôte, publiée par lui sur le canal
/// `Control` à l'établissement de la session (rôle contrôleur). Renvoie une liste
/// **vide** tant que l'annonce n'est pas arrivée **ou** si l'hôte n'a aucun écran
/// énumérable. L'index de chaque entrée est celui qu'attend [`switch_monitor`].
pub fn session_monitors(session_id: u64) -> Result<Vec<MonitorInfoDto>, String> {
    crate::flux::moniteurs(session_id)
}

// ---------------------------------------------------------------------------
// 5. Infos système du pair
// ---------------------------------------------------------------------------

/// Infos système du pair (miroir plat de `nd_core::PeerInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfoDto {
    /// Nom d'hôte de la machine distante.
    pub hote: String,
    /// Système d'exploitation (chaîne libre, ex. « windows (x86_64) »).
    pub os: String,
}

impl From<PeerInfo> for PeerInfoDto {
    fn from(p: PeerInfo) -> Self {
        PeerInfoDto {
            hote: p.host,
            os: p.os,
        }
    }
}

/// **Infos système du pair** (nom d'hôte + OS) publiées par l'hôte sur le canal
/// `Control` à l'établissement (rôle contrôleur). Erreur tant que l'annonce n'est
/// pas encore arrivée (ou si la session est inconnue).
pub fn session_peer_info(session_id: u64) -> Result<PeerInfoDto, String> {
    crate::flux::infos_pair(session_id)
}

#[cfg(test)]
mod tests_extras_session {
    use super::*;

    /// Identifiant jamais attribué : le compteur de sessions démarre à 1 et
    /// croît de un en un — `u64::MAX` reste hors d'atteinte.
    const SESSION_INCONNUE: u64 = u64::MAX;

    #[test]
    fn session_set_privacy_exige_une_session_vivante() {
        let err = session_set_privacy(SESSION_INCONNUE, true).unwrap_err();
        assert!(err.contains("inconnue"), "message peu utile : {err}");
    }

    #[test]
    fn session_set_region_exige_une_session_vivante() {
        let region = RegionDto {
            x: 10,
            y: 20,
            largeur: 640,
            hauteur: 480,
        };
        let err = session_set_region(SESSION_INCONNUE, Some(region)).unwrap_err();
        assert!(err.contains("inconnue"), "message peu utile : {err}");
        // Le rétablissement du plein écran (None) passe par le même chemin.
        let err = session_set_region(SESSION_INCONNUE, None).unwrap_err();
        assert!(err.contains("inconnue"), "message peu utile : {err}");
    }

    #[test]
    fn session_send_annotation_valide_le_dto_avant_la_session() {
        // DTO mal formé : refusé avec un message sur le genre, sans exiger de
        // session vivante (validation avant tout accès à la table).
        let invalide = AnnotationDto {
            genre: 42,
            points: vec![0.0, 0.0],
            couleur_argb: 0,
            epaisseur: 1.0,
            texte: None,
        };
        let err = session_send_annotation(SESSION_INCONNUE, invalide).unwrap_err();
        assert!(err.contains("genre"), "message peu utile : {err}");

        // DTO valide : l'erreur devient l'absence de session.
        let valide = AnnotationDto {
            genre: 0,
            points: vec![0.1, 0.2],
            couleur_argb: 0xFFFF_0000,
            epaisseur: 2.0,
            texte: None,
        };
        let err = session_send_annotation(SESSION_INCONNUE, valide).unwrap_err();
        assert!(err.contains("inconnue"), "message peu utile : {err}");
    }

    #[test]
    fn session_open_tunnel_refuse_un_hote_illisible() {
        // L'analyse de l'hôte précède la recherche de session : aucune session
        // requise, aucun écouteur lié.
        let err =
            session_open_tunnel(SESSION_INCONNUE, 0, "pas-une-ip".to_owned(), 80).unwrap_err();
        assert!(err.contains("hôte distant"), "message peu utile : {err}");
        assert!(err.contains("invalide"), "message peu utile : {err}");
    }

    #[test]
    fn session_open_tunnel_accepte_ipv4_et_ipv6_mais_exige_une_session() {
        // IP v4, v6 et espaces parasites acceptés : l'erreur restante est bien
        // l'absence de session, pas l'analyse de l'hôte.
        for hote in ["127.0.0.1", "::1", "  192.168.1.10  "] {
            let err = session_open_tunnel(SESSION_INCONNUE, 0, hote.to_owned(), 8080).unwrap_err();
            assert!(err.contains("inconnue"), "hôte « {hote} » : {err}");
        }
    }
}

#[cfg(test)]
mod tests_plan_controle {
    use super::*;

    /// Identifiant jamais attribué (le compteur démarre à 1 et croît).
    const SESSION_INCONNUE: u64 = u64::MAX;

    #[test]
    fn set_permission_valide_la_capacite_avant_la_session() {
        // Clé inconnue : refusée avec un message sur la capacité, **sans** exiger
        // de session vivante (analyse avant tout accès à la table).
        let err = session_set_permission(SESSION_INCONNUE, "pas_une_capacite".to_owned(), true)
            .unwrap_err();
        assert!(
            err.contains("capacité inconnue"),
            "message peu utile : {err}"
        );

        // Clé valide : l'erreur devient l'absence de session.
        let err = session_set_permission(SESSION_INCONNUE, "souris".to_owned(), false).unwrap_err();
        assert!(err.contains("inconnue"), "message peu utile : {err}");
    }

    #[test]
    fn set_quality_valide_le_preset_avant_la_session() {
        let err = session_set_quality(SESSION_INCONNUE, "ultra".to_owned()).unwrap_err();
        assert!(
            err.contains("préréglage") && err.contains("inconnu"),
            "message peu utile : {err}"
        );
        // Préréglages acceptés (avec et sans accent) : l'erreur devient l'absence
        // de session.
        for preset in ["auto", "fluide", "equilibre", "netteté", "nettete"] {
            let err = session_set_quality(SESSION_INCONNUE, preset.to_owned()).unwrap_err();
            assert!(err.contains("inconnue"), "préréglage « {preset} » : {err}");
        }
    }

    #[test]
    fn lectures_et_enregistrement_exigent_une_session_vivante() {
        assert!(session_monitors(SESSION_INCONNUE)
            .unwrap_err()
            .contains("inconnue"));
        assert!(session_peer_info(SESSION_INCONNUE)
            .unwrap_err()
            .contains("inconnue"));
        assert!(
            session_set_recording(SESSION_INCONNUE, Some("s.mp4".to_owned()))
                .unwrap_err()
                .contains("inconnue")
        );
        assert!(session_set_recording(SESSION_INCONNUE, None)
            .unwrap_err()
            .contains("inconnue"));
    }
}

#[cfg(test)]
mod tests_annotations {
    use super::*;

    /// DTO → couche (un trait) → DTO : doit rendre exactement l'entrée.
    fn aller_retour(dto: &AnnotationDto) -> AnnotationDto {
        let couche = couche_depuis_annotation(dto).expect("DTO valide → couche");
        let mut dtos = annotations_depuis_couche(&couche);
        assert_eq!(dtos.len(), 1, "une couche à un trait → un seul DTO");
        dtos.pop().expect("un DTO")
    }

    #[test]
    fn argb_rgba_aller_retour() {
        let argb = 0x8899_AABBu32;
        // ARGB 0xAARRGGBB → RGBA 0xRRGGBBAA.
        assert_eq!(argb_vers_rgba(argb), 0x99AA_BB88);
        assert_eq!(rgba_vers_argb(argb_vers_rgba(argb)), argb);
    }

    #[test]
    fn aller_retour_trait_libre() {
        let dto = AnnotationDto {
            genre: 0,
            points: vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6],
            couleur_argb: 0x80FF_0000,
            epaisseur: 2.5,
            texte: None,
        };
        assert_eq!(aller_retour(&dto), dto);
    }

    #[test]
    fn aller_retour_formes_a_deux_points() {
        // Rectangle, ellipse, flèche : mêmes 2 points, genres distincts.
        for genre in [1, 2, 3] {
            let dto = AnnotationDto {
                genre,
                points: vec![0.0, 0.25, 1.0, 0.75],
                couleur_argb: 0xFF00_FF00,
                epaisseur: 3.0,
                texte: None,
            };
            assert_eq!(aller_retour(&dto), dto, "genre {genre}");
        }
    }

    #[test]
    fn aller_retour_texte() {
        let dto = AnnotationDto {
            genre: 4,
            points: vec![0.2, 0.3],
            couleur_argb: 0xFF11_2233,
            epaisseur: 14.0,
            texte: Some("Cliquez ici — été".to_owned()),
        };
        assert_eq!(aller_retour(&dto), dto);
    }

    #[test]
    fn genre_inconnu_refuse() {
        let dto = AnnotationDto {
            genre: 99,
            points: vec![0.0, 0.0],
            couleur_argb: 0,
            epaisseur: 1.0,
            texte: None,
        };
        let err = couche_depuis_annotation(&dto).unwrap_err();
        assert!(err.contains("genre"), "message peu utile : {err}");
    }

    #[test]
    fn rectangle_mauvais_nombre_de_points_refuse() {
        let dto = AnnotationDto {
            genre: 1,
            points: vec![0.0, 0.0],
            couleur_argb: 0,
            epaisseur: 1.0,
            texte: None,
        };
        let err = couche_depuis_annotation(&dto).unwrap_err();
        assert!(err.contains("2 points"), "message peu utile : {err}");
    }

    #[test]
    fn texte_sans_contenu_refuse() {
        let dto = AnnotationDto {
            genre: 4,
            points: vec![0.0, 0.0],
            couleur_argb: 0,
            epaisseur: 12.0,
            texte: None,
        };
        assert!(couche_depuis_annotation(&dto).is_err());
    }

    #[test]
    fn coordonnees_impaires_refusees() {
        let dto = AnnotationDto {
            genre: 0,
            points: vec![0.0, 0.0, 1.0],
            couleur_argb: 0,
            epaisseur: 1.0,
            texte: None,
        };
        assert!(couche_depuis_annotation(&dto).is_err());
    }
}
