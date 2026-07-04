//! `nd-input` — abstraction de l'injection d'entrées côté machine contrôlée.
//!
//! Le trait [`InputInjector`] est implémenté par OS (SendInput sur Windows, CGEvent
//! sur macOS, XTEST/uinput sur Linux). Les entrées empruntent un canal QUIC fiable de
//! priorité maximale (voir plan 04). Mapping clavier, écran sécurisé/UAC et Wayland :
//! `../../plan-technique/07-injection-entrees.md`.

use nd_proto::{MonitorId, Result};

/// Bouton de souris.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// Bouton latéral 1 (précédent).
    X1,
    /// Bouton latéral 2 (suivant).
    X2,
}

/// Injecteur d'entrées, implémenté par plateforme.
///
/// Les méthodes prennent `&self` : l'implémentation gère sa propre synchronisation
/// interne, ce qui simplifie le partage entre tâches.
pub trait InputInjector: Send + Sync {
    /// Déplace le curseur en coordonnées absolues (0.0–1.0) sur le moniteur donné.
    fn mouse_move_abs(&self, x: f64, y: f64, monitor: MonitorId) -> Result<()>;
    /// Déplacement relatif (mode jeu/FPS, voir plan 07 §souris).
    fn mouse_move_rel(&self, dx: f64, dy: f64) -> Result<()>;
    /// Presse (`down = true`) ou relâche un bouton de souris.
    fn mouse_button(&self, btn: MouseButton, down: bool) -> Result<()>;
    /// Molette : défilement horizontal/vertical (unités haute résolution).
    fn scroll(&self, dx: f64, dy: f64) -> Result<()>;
    /// Touche clavier par scancode physique (`down` = pressée).
    fn key(&self, scancode: u32, down: bool) -> Result<()>;
    /// Saisie d'un caractère Unicode (chemin `KEYEVENTF_UNICODE` / équivalents).
    fn unicode(&self, ch: char) -> Result<()>;
    /// Relâche toutes les touches/boutons (anti « stuck key » en fin de session).
    fn release_all(&self);
}

#[cfg(windows)]
mod win;

/// Crée l'injecteur adapté à la plateforme courante.
///
/// Windows : `SendInput`. Autres OS : à venir (Phase 4+, voir plan 07/16).
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win::SendInputInjector::new()))
    }
    #[cfg(not(windows))]
    {
        Err(nd_proto::NdError::NotImplemented(
            "nd-input::create_injector (impl macOS/Linux à venir, voir plan 07/16)",
        ))
    }
}
