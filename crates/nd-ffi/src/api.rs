//! Façade d'API orientée UI — contrat stable pour l'application Flutter (plan 10).
//!
//! # Intégration Flutter à venir
//!
//! Lors du câblage `flutter_rust_bridge` (plan 10 — interface client), les types et
//! fonctions publics de ce module seront annotés `#[flutter_rust_bridge::frb]` et les
//! flux d'événements passeront par des `StreamSink`. En attendant, tout ici est du
//! **Rust pur**, synchrone et testable, sans aucune dépendance à Flutter.
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

use nd_core::{SessionConfig, SessionRole, SessionState};
use nd_features::Permissions;
use nd_proto::{InputEvent, NovaId};

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
