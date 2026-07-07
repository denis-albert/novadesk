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

use std::path::PathBuf;

use nd_codec::DecodedFrame;
use nd_core::{SessionConfig, SessionOptions, SessionRole, SessionState, SessionStats};
use nd_features::{PermissionSet, Permissions};
use nd_proto::{InputEvent, NovaId};

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
/// (filtre de permissions granulaire, enregistrement local, encodage delta). Les
/// axes non exposés ici (profil ABR, politique de reconnexion) prennent les
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
}

impl From<SessionOptionsDto> for SessionOptions {
    fn from(dto: SessionOptionsDto) -> Self {
        SessionOptions {
            permissions: Some(PermissionSet::from(Permissions::from(dto.permissions))),
            recording: dto.recording_path.map(PathBuf::from),
            delta_mode: dto.delta_mode,
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
/// encodage delta).
///
/// [`start_session`] équivaut à cet appel avec les options par défaut du moteur.
/// L'identifiant renvoyé s'utilise avec les mêmes fonctions `session_*`.
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
