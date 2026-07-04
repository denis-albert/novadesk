//! Implémentation Windows de [`InputInjector`] via **`SendInput`**.
//!
//! Souris en coordonnées absolues normalisées, boutons, molette haute résolution,
//! touches par scancode et saisie Unicode. Voir plan 07 §Windows. Le multi-écran
//! (drapeau `VIRTUALDESK` + rectangle du moniteur) et l'injection dans le bureau
//! sécurisé/UAC (service SYSTEM) viendront ensuite.
//!
//! Tout le `unsafe` FFI est concentré ici, derrière le trait.
#![allow(unsafe_code)]

use nd_proto::{MonitorId, NdError, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY,
};

use crate::{InputInjector, MouseButton};

/// Incrément standard d'un cran de molette.
const WHEEL_DELTA: i32 = 120;
/// Valeurs `mouseData` pour les boutons latéraux.
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

/// Injecteur d'entrées Windows fondé sur `SendInput`.
pub struct SendInputInjector;

impl SendInputInjector {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for SendInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

/// Envoie un unique événement d'entrée.
fn send_one(input: INPUT) -> Result<()> {
    // SAFETY : `input` est un `INPUT` correctement initialisé ; `cbsize` correspond.
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(NdError::Input(
            "SendInput a échoué (entrée bloquée par l'UIPI ?)".into(),
        ))
    }
}

/// Construit un événement souris.
fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Construit un événement clavier (par scancode ou Unicode).
fn key_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl InputInjector for SendInputInjector {
    fn mouse_move_abs(&self, x: f64, y: f64, _monitor: MonitorId) -> Result<()> {
        // Écran primaire pour l'instant (moniteur 0). Le multi-écran ajoutera le
        // drapeau VIRTUALDESK et le rectangle du moniteur (plan 07).
        let dx = (x.clamp(0.0, 1.0) * 65535.0).round() as i32;
        let dy = (y.clamp(0.0, 1.0) * 65535.0).round() as i32;
        send_one(mouse_input(
            dx,
            dy,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
        ))
    }

    fn mouse_move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        send_one(mouse_input(
            dx.round() as i32,
            dy.round() as i32,
            0,
            MOUSEEVENTF_MOVE,
        ))
    }

    fn mouse_button(&self, btn: MouseButton, down: bool) -> Result<()> {
        let (flags, data) = match (btn, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        send_one(mouse_input(0, 0, data, flags))
    }

    fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        let v = (dy * f64::from(WHEEL_DELTA)).round() as i32;
        if v != 0 {
            send_one(mouse_input(0, 0, v as u32, MOUSEEVENTF_WHEEL))?;
        }
        let h = (dx * f64::from(WHEEL_DELTA)).round() as i32;
        if h != 0 {
            send_one(mouse_input(0, 0, h as u32, MOUSEEVENTF_HWHEEL))?;
        }
        Ok(())
    }

    fn key(&self, scancode: u32, down: bool) -> Result<()> {
        let mut flags = KEYEVENTF_SCANCODE;
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send_one(key_input(scancode as u16, flags))
    }

    fn unicode(&self, ch: char) -> Result<()> {
        let mut buf = [0u16; 2];
        for &unit in ch.encode_utf16(&mut buf).iter() {
            send_one(key_input(unit, KEYEVENTF_UNICODE))?;
            send_one(key_input(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP))?;
        }
        Ok(())
    }

    fn release_all(&self) {
        // Relâche les boutons souris (garde-fou anti-blocage). La libération des touches
        // nécessitera un suivi de l'état pressé (plan 07).
        for (flags, data) in [
            (MOUSEEVENTF_LEFTUP, 0),
            (MOUSEEVENTF_RIGHTUP, 0),
            (MOUSEEVENTF_MIDDLEUP, 0),
            (MOUSEEVENTF_XUP, XBUTTON1),
            (MOUSEEVENTF_XUP, XBUTTON2),
        ] {
            let _ = send_one(mouse_input(0, 0, data, flags));
        }
    }
}
