//! `nd-features` — permissions de session et fonctionnalités avancées (accès
//! non-surveillé, multi-moniteur, enregistrement, tunnel, Wake-on-LAN…).
//! Voir `../../plan-technique/13-fonctionnalites-avancees.md`.
//!
//! Règle transverse : les permissions sont **toujours appliquées côté machine
//! contrôlée** (défense en profondeur), jamais seulement dans l'UI du contrôleur.

pub mod annotation;
pub mod invite;
pub mod privacy;
pub mod recording;
pub mod tunnel;
pub mod wol;

pub use annotation::{AnnotationLayer, Stroke};
pub use invite::{generate_invite, InviteStore, RedeemResult, SessionInvite};
pub use privacy::{PrivacyAction, PrivacyState};
pub use recording::{RecordedFrame, SessionReader, SessionRecorder};
pub use tunnel::{pipe_bidirectional, LocalForwarder};
pub use wol::{magic_packet, wake_on_lan};

/// Permissions accordées à une session par le poste contrôlé.
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
