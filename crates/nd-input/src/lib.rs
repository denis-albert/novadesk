//! `nd-input` — abstraction de l'injection d'entrées côté machine contrôlée.
//!
//! Le trait [`InputInjector`] est implémenté par OS (SendInput sur Windows, CGEvent
//! sur macOS, XTEST/uinput sur Linux). Les entrées empruntent un canal QUIC fiable de
//! priorité maximale (voir plan 04). Mapping clavier, écran sécurisé/UAC et Wayland :
//! `../../plan-technique/07-injection-entrees.md`.

use nd_proto::{MonitorId, Result};

/// Correspondance caractère Unicode → keysym X11 (agnostique OS, testée
/// partout — voir [`keysym`]). Utilisée par la saisie Unicode du backend Linux.
mod keysym;
/// Cartographie multi-écran des coordonnées absolues (agnostique OS, testée
/// partout — voir [`screen`]). Utilisée par les backends Windows/macOS/Linux.
mod screen;

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

// Expose l'implémentation Windows concrète : injection tactile (`touch_down`/`_move`/
// `_up`) et séquence d'attention sécurisée (Ctrl+Alt+Suppr), non couvertes par le
// trait multiplateforme. Voir plan 07 §entrées avancées.
#[cfg(windows)]
pub use win::{send_secure_attention_sequence, SendInputInjector};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::QuartzInjector;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
mod uinput;

#[cfg(target_os = "linux")]
pub use linux::XtestInjector;

#[cfg(target_os = "linux")]
pub use uinput::UinputInjector;

/// Sélectionne l'injecteur Linux : `uinput` (niveau noyau) en session Wayland
/// pure, XTEST (X11) sinon.
///
/// La détection suit la même convention que `nd-capture` : une session est
/// « Wayland pure » quand `WAYLAND_DISPLAY` est défini **sans** serveur X
/// joignable (`DISPLAY` absent). Sous XWayland (les deux définis), XTEST reste
/// préféré (intégré au serveur d'affichage, sans droits `/dev/uinput`). Si la
/// création uinput échoue mais qu'un serveur X est joignable, on retombe sur
/// XTEST plutôt que d'échouer — voir `uinput.rs` (droits requis).
#[cfg(target_os = "linux")]
fn create_linux_injector() -> Result<Box<dyn InputInjector>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    if wayland && !x11 {
        // Wayland pur : uinput est la seule voie fiable.
        return Ok(Box::new(uinput::UinputInjector::new()?));
    }
    if wayland {
        // Session Wayland avec XWayland disponible : tente uinput (injection
        // globale), repli XTEST si les droits uinput manquent.
        if let Ok(injecteur) = uinput::UinputInjector::new() {
            return Ok(Box::new(injecteur));
        }
    }
    Ok(Box::new(linux::XtestInjector::new()?))
}

/// Crée l'injecteur adapté à la plateforme courante.
///
/// Windows : `SendInput`. macOS : Quartz Event Services (`CGEventPost`, permission
/// Accessibilité/TCC requise). Linux : XTEST sur X11/XWayland, ou `/dev/uinput`
/// (niveau noyau) en session Wayland pure — voir [`create_linux_injector`] et
/// plan 07 §Wayland. Autres OS : `NotImplemented`.
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win::SendInputInjector::new()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::QuartzInjector::new()))
    }
    #[cfg(target_os = "linux")]
    {
        create_linux_injector()
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(nd_proto::NdError::NotImplemented(
            "nd-input::create_injector (OS non pris en charge, voir plan 07/16)",
        ))
    }
}
