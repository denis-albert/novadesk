//! `nd-features` — permissions de session et fonctionnalités avancées (accès
//! non-surveillé, multi-moniteur, enregistrement, tunnel, Wake-on-LAN…).
//! Voir `../../plan-technique/13-fonctionnalites-avancees.md`.
//!
//! Règle transverse : les permissions sont **toujours appliquées côté machine
//! contrôlée** (défense en profondeur), jamais seulement dans l'UI du contrôleur.
//!
//! # Points d'intégration pour l'orchestrateur (`nd-core`)
//!
//! - **Permissions** : gardes à poser avant chaque action de session —
//!   [`PermissionBroker::is_allowed`] (chemin chaud, sans journal),
//!   [`PermissionBroker::authorize`]/[`PermissionBroker::authorize_input`]
//!   (journalisés), table [`Capability::required_for_input`]. Contrat complet
//!   en tête de [`permissions`].
//! - **Enregistrement** : [`Mp4Muxer::record_video_chunk`] consomme les
//!   `EncodedChunk` H.264 de `nd-codec` et produit un `.mp4` rejouable
//!   (validé par [`Mp4Reader`]) ; l'archive interne `.ndr`
//!   ([`IndexedRecorder`]) se convertit via [`ndr_to_mp4`]. Contrat en tête
//!   de [`recording`].
//! - **Reconnexion** : [`ReconnectController`] (`on_disconnect` /
//!   `next_delay` / `reset`) pilote le backoff sans dormir. Contrat en tête
//!   de [`reconnect`].
//!
//! Restent des modèles purs sans effet système, à brancher côté plateforme :
//! [`privacy`] (exécution des `PrivacyAction`) et [`hotkeys`] (dispatch dans
//! la boucle d'événements de l'UI) — voir leurs docs de module respectives.

pub mod annotation;
pub mod hotkeys;
pub mod invite;
pub mod permissions;
pub mod privacy;
pub mod reconnect;
pub mod recording;
pub mod settings;
pub mod tunnel;
pub mod wol;

pub use annotation::{AnnotationLayer, RgbaCanvas, Stroke};
pub use hotkeys::{ActionCodec, HostAction, Hotkey, HotkeyMap};
pub use invite::{generate_invite, InviteStore, RedeemResult, SessionInvite};
pub use permissions::{
    AuditEntry, AuditEvent, Capability, PermissionBroker, PermissionDecision, PermissionRequest,
    PermissionSet,
};
pub use privacy::{PrivacyAction, PrivacyState};
pub use reconnect::{ReconnectController, ReconnectPolicy, ReconnectState};
pub use recording::mp4::{ndr_to_mp4, Mp4Muxer, Mp4Reader, Mp4Sample, Mp4ValidationReport};
pub use recording::{
    IndexedRecorder, KeyframeEntry, RecordedFrame, RecordingMetadata, SessionReader,
    SessionRecorder, ValidationReport,
};
pub use settings::{QualityParams, QualityPreset, SessionSettings};
pub use tunnel::{pipe_bidirectional, LocalForwarder};
pub use wol::{magic_packet, wake_on_lan};

/// Permissions accordées à une session par le poste contrôlé.
///
/// Modèle historique à six booléens, conservé pour compatibilité. Le modèle
/// granulaire ([`PermissionSet`], [`Capability`], journal d'audit) vit dans
/// [`permissions`] ; les conversions `From` dans les deux sens sont
/// conservatrices (elles n'élargissent jamais les droits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub keyboard: bool,
    pub mouse: bool,
    pub clipboard: bool,
    pub files: bool,
    pub audio: bool,
    /// Si vrai, la session est en lecture seule (aucune entrée injectée).
    pub view_only: bool,
}

impl Permissions {
    /// Contrôle complet (clavier, souris, presse-papiers, fichiers, audio).
    #[must_use]
    pub fn full() -> Self {
        Permissions {
            keyboard: true,
            mouse: true,
            clipboard: true,
            files: true,
            audio: true,
            view_only: false,
        }
    }

    /// Observation seule : rien n'est injecté ni transféré.
    #[must_use]
    pub fn view_only() -> Self {
        Permissions {
            keyboard: false,
            mouse: false,
            clipboard: false,
            files: false,
            audio: false,
            view_only: true,
        }
    }

    /// L'injection d'entrées est-elle autorisée pour cette session ?
    #[must_use]
    pub fn allows_input(self) -> bool {
        !self.view_only && (self.keyboard || self.mouse)
    }
}

impl Default for Permissions {
    fn default() -> Self {
        // Défaut prudent : observation seule tant que l'utilisateur n'accorde rien.
        Permissions::view_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_only_interdit_input() {
        assert!(!Permissions::view_only().allows_input());
        assert!(Permissions::full().allows_input());
    }

    #[test]
    fn defaut_est_prudent() {
        assert_eq!(Permissions::default(), Permissions::view_only());
    }
}
