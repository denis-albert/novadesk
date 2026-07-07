//! Implémentation macOS de [`InputInjector`] via **Quartz Event Services** (`CGEvent`).
//!
//! Les événements sont créés par `CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent`
//! / `CGEventCreateScrollWheelEvent` puis postés au niveau HID (`CGEventPost`, tap
//! [`CGEventTapLocation::HID`]), comme s'ils provenaient du matériel.
//!
//! **Permission Accessibilité (TCC) requise** : le processus doit être approuvé dans
//! Réglages Système → Confidentialité et sécurité → Accessibilité, sinon le système
//! ignore *silencieusement* les événements postés (aucune erreur retournée). La
//! détection (`AXIsProcessTrusted`) et le guidage de l'utilisateur vers le panneau TCC
//! relèvent de l'application hôte — voir plan 07 §macOS.
//!
//! Aucun bloc `unsafe` ici : la crate `core-graphics` encapsule les appels FFI Quartz
//! derrière des fonctions sûres.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use nd_proto::{MonitorId, NdError, Result};

use crate::screen::{point_absolu, MonitorRect};
use crate::{InputInjector, MouseButton};

/// Numéros de boutons dans la convention CoreGraphics (champ `MOUSE_EVENT_BUTTON_NUMBER`).
const NUM_GAUCHE: u8 = 0;
const NUM_DROIT: u8 = 1;
const NUM_MILIEU: u8 = 2;
/// Boutons latéraux : au-delà des trois boutons de `CGMouseButton`, on force le numéro.
const NUM_X1: u8 = 3;
const NUM_X2: u8 = 4;

/// État interne suivi par l'injecteur (touches/boutons enfoncés) pour `release_all`
/// et pour poster des événements *Dragged* pendant qu'un bouton est tenu.
#[derive(Default)]
struct InjectorState {
    /// Keycodes virtuels macOS actuellement enfoncés.
    keys: BTreeSet<CGKeyCode>,
    /// Numéros de boutons (convention CG) actuellement enfoncés.
    buttons: BTreeSet<u8>,
}

/// Injecteur d'entrées macOS fondé sur Quartz Event Services.
pub struct QuartzInjector {
    state: Mutex<InjectorState>,
}

impl QuartzInjector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InjectorState::default()),
        }
    }

    /// Verrouille l'état, en récupérant un `Mutex` empoisonné plutôt que de paniquer.
    fn lock(&self) -> MutexGuard<'_, InjectorState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mémorise l'état enfoncé/relâché d'un keycode virtuel.
    fn record_key(&self, keycode: CGKeyCode, down: bool) {
        let mut st = self.lock();
        if down {
            st.keys.insert(keycode);
        } else {
            st.keys.remove(&keycode);
        }
    }

    /// Mémorise l'état enfoncé/relâché d'un bouton (numéro CG).
    fn record_button(&self, numero: u8, down: bool) {
        let mut st = self.lock();
        if down {
            st.buttons.insert(numero);
        } else {
            st.buttons.remove(&numero);
        }
    }

    /// Nombre de touches puis de boutons actuellement suivis comme enfoncés.
    ///
    /// Utile pour les tests et le diagnostic anti « stuck key ».
    #[must_use]
    pub fn pressed_counts(&self) -> (usize, usize) {
        let st = self.lock();
        (st.keys.len(), st.buttons.len())
    }

    /// Type d'événement de déplacement selon les boutons tenus : macOS attend des
    /// événements *Dragged* (et non *Moved*) pendant qu'un bouton est enfoncé, sans
    /// quoi le glisser-déposer ne fonctionne pas.
    fn type_deplacement(&self) -> (CGEventType, CGMouseButton) {
        let st = self.lock();
        if st.buttons.contains(&NUM_GAUCHE) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if st.buttons.contains(&NUM_DROIT) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if st.buttons.is_empty() {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        } else {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        }
    }
}

impl Default for QuartzInjector {
    fn default() -> Self {
        Self::new()
    }
}

/// Rectangles des écrans actifs (bornes CoreGraphics, en **points** de l'espace
/// global ; origine possiblement négative pour un écran à gauche/au-dessus du
/// principal). `MonitorId(i)` = `i`-ième écran de `CGGetActiveDisplayList`,
/// cohérent avec l'énumération de `nd-capture` (§macOS). Renvoie un vecteur vide
/// si l'énumération échoue (repli géré par l'appelant).
fn moniteurs_actifs() -> Vec<MonitorRect> {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    ids.iter()
        .enumerate()
        .map(|(i, &id)| {
            let b = CGDisplay::new(id).bounds();
            MonitorRect {
                id: i as u32,
                x: b.origin.x as i32,
                y: b.origin.y as i32,
                width: b.size.width.max(0.0) as u32,
                height: b.size.height.max(0.0) as u32,
            }
        })
        .collect()
}

/// Crée une source d'événements au niveau HID (état « matériel » de la session).
fn source() -> Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| NdError::Input("CGEventSourceCreate a échoué".into()))
}

/// Position actuelle du curseur en coordonnées globales (points, origine en haut à
/// gauche de l'écran principal) : un `CGEvent` vierge porte la position du curseur.
fn position_courante() -> Result<CGPoint> {
    let ev =
        CGEvent::new(source()?).map_err(|()| NdError::Input("CGEventCreate a échoué".into()))?;
    Ok(ev.location())
}

/// Construit puis poste un événement souris ; `numero` force le numéro de bouton pour
/// les boutons latéraux (au-delà des trois boutons de `CGMouseButton`).
fn poste_souris(
    event_type: CGEventType,
    pos: CGPoint,
    bouton: CGMouseButton,
    numero: Option<u8>,
) -> Result<()> {
    let ev = CGEvent::new_mouse_event(source()?, event_type, pos, bouton)
        .map_err(|()| NdError::Input("CGEventCreateMouseEvent a échoué".into()))?;
    if let Some(n) = numero {
        ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, i64::from(n));
    }
    ev.post(CGEventTapLocation::HID);
    Ok(())
}

/// Paramètres CoreGraphics d'un événement bouton : type, bouton CG, numéro forcé.
fn parametres_bouton(btn: MouseButton, down: bool) -> (CGEventType, CGMouseButton, Option<u8>) {
    match (btn, down) {
        (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, None),
        (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, None),
        (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, None),
        (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, None),
        (MouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, None),
        (MouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, None),
        (MouseButton::X1, true) => (
            CGEventType::OtherMouseDown,
            CGMouseButton::Center,
            Some(NUM_X1),
        ),
        (MouseButton::X1, false) => (
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
            Some(NUM_X1),
        ),
        (MouseButton::X2, true) => (
            CGEventType::OtherMouseDown,
            CGMouseButton::Center,
            Some(NUM_X2),
        ),
        (MouseButton::X2, false) => (
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
            Some(NUM_X2),
        ),
    }
}

/// Numéro CG compact d'un bouton pour le suivi d'état.
fn numero_bouton(btn: MouseButton) -> u8 {
    match btn {
        MouseButton::Left => NUM_GAUCHE,
        MouseButton::Right => NUM_DROIT,
        MouseButton::Middle => NUM_MILIEU,
        MouseButton::X1 => NUM_X1,
        MouseButton::X2 => NUM_X2,
    }
}

/// Réciproque de [`numero_bouton`] (les numéros proviennent toujours de cette fonction).
fn bouton_depuis_numero(numero: u8) -> MouseButton {
    match numero {
        NUM_DROIT => MouseButton::Right,
        NUM_MILIEU => MouseButton::Middle,
        NUM_X1 => MouseButton::X1,
        NUM_X2 => MouseButton::X2,
        _ => MouseButton::Left,
    }
}

impl InputInjector for QuartzInjector {
    fn mouse_move_abs(&self, x: f64, y: f64, monitor: MonitorId) -> Result<()> {
        // Multi-écran : projette (x, y) sur le rectangle du moniteur visé (bornes
        // CoreGraphics en points de l'espace global) via la logique partagée et
        // testée [`crate::screen`]. Repli sur l'écran principal si l'énumération
        // échoue. Coordonnées globales en points, origine en haut à gauche.
        let (px, py) = match point_absolu(&moniteurs_actifs(), monitor, x, y) {
            Some((px, py)) => (f64::from(px), f64::from(py)),
            None => {
                let bornes = CGDisplay::main().bounds();
                (
                    bornes.origin.x + x.clamp(0.0, 1.0) * bornes.size.width,
                    bornes.origin.y + y.clamp(0.0, 1.0) * bornes.size.height,
                )
            }
        };
        let (event_type, bouton) = self.type_deplacement();
        poste_souris(event_type, CGPoint::new(px, py), bouton, None)
    }

    fn mouse_move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        // Quartz n'a pas d'événement purement relatif : on déplace depuis la position
        // courante et on renseigne aussi les deltas bruts, lus par les applications en
        // « pointeur relatif » (jeux). Le système borne la position à l'écran.
        let cur = position_courante()?;
        let cible = CGPoint::new(cur.x + dx, cur.y + dy);
        let (event_type, bouton) = self.type_deplacement();
        let ev = CGEvent::new_mouse_event(source()?, event_type, cible, bouton)
            .map_err(|()| NdError::Input("CGEventCreateMouseEvent a échoué".into()))?;
        ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx.round() as i64);
        ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy.round() as i64);
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn mouse_button(&self, btn: MouseButton, down: bool) -> Result<()> {
        // Les événements bouton portent une position : on clique là où est le curseur.
        let pos = position_courante()?;
        let (event_type, bouton, numero) = parametres_bouton(btn, down);
        poste_souris(event_type, pos, bouton, numero)?;
        self.record_button(numero_bouton(btn), down);
        Ok(())
    }

    fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        // Molette en crans (unité LINE), parité avec WHEEL_DELTA côté Windows :
        // dy > 0 = vers le haut, dx > 0 = vers la droite. `wheel1` = axe vertical,
        // `wheel2` = axe horizontal (CGEventCreateScrollWheelEvent2, macOS >= 10.13).
        let v = dy.round() as i32;
        let h = dx.round() as i32;
        if v == 0 && h == 0 {
            return Ok(());
        }
        let ev = CGEvent::new_scroll_event(source()?, ScrollEventUnit::LINE, 2, v, h, 0)
            .map_err(|()| NdError::Input("CGEventCreateScrollWheelEvent a échoué".into()))?;
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn key(&self, scancode: u32, down: bool) -> Result<()> {
        // Le paramètre est interprété comme un keycode virtuel macOS (constantes kVK_*).
        // La conversion scancode physique → keycode virtuel relève de la couche de
        // mapping clavier (plan 07 §mapping).
        let keycode = CGKeyCode::try_from(scancode).map_err(|_| {
            NdError::Input(format!("keycode virtuel macOS hors plage : {scancode}"))
        })?;
        let ev = CGEvent::new_keyboard_event(source()?, keycode, down)
            .map_err(|()| NdError::Input("CGEventCreateKeyboardEvent a échoué".into()))?;
        ev.post(CGEventTapLocation::HID);
        self.record_key(keycode, down);
        Ok(())
    }

    fn unicode(&self, ch: char) -> Result<()> {
        // Saisie directe : un événement clavier « neutre » (keycode 0) portant la chaîne
        // UTF-16 via CGEventKeyboardSetUnicodeString — équivalent macOS de
        // KEYEVENTF_UNICODE. Seul l'événement « down » porte le texte.
        let mut buf = [0u16; 2];
        let unites = ch.encode_utf16(&mut buf);

        let down = CGEvent::new_keyboard_event(source()?, 0, true)
            .map_err(|()| NdError::Input("CGEventCreateKeyboardEvent a échoué".into()))?;
        down.set_string_from_utf16_unchecked(unites);
        down.post(CGEventTapLocation::HID);

        let up = CGEvent::new_keyboard_event(source()?, 0, false)
            .map_err(|()| NdError::Input("CGEventCreateKeyboardEvent a échoué".into()))?;
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn release_all(&self) {
        // Récupère l'état sous verrou puis relâche hors verrou, en best-effort : en fin
        // de session on tente tous les « up », sans s'arrêter au premier échec.
        let (keys, buttons) = {
            let mut st = self.lock();
            let keys: Vec<CGKeyCode> = st.keys.iter().copied().collect();
            let buttons: Vec<u8> = st.buttons.iter().copied().collect();
            st.keys.clear();
            st.buttons.clear();
            (keys, buttons)
        };

        for keycode in keys {
            if let Ok(src) = source() {
                if let Ok(ev) = CGEvent::new_keyboard_event(src, keycode, false) {
                    ev.post(CGEventTapLocation::HID);
                }
            }
        }
        if buttons.is_empty() {
            return;
        }
        let pos = position_courante().unwrap_or_else(|_| CGPoint::new(0.0, 0.0));
        for numero in buttons {
            let (event_type, bouton, force) =
                parametres_bouton(bouton_depuis_numero(numero), false);
            let _ = poste_souris(event_type, pos, bouton, force);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numero_bouton_est_bijectif() {
        for btn in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::X1,
            MouseButton::X2,
        ] {
            assert_eq!(bouton_depuis_numero(numero_bouton(btn)), btn);
        }
    }

    #[test]
    fn suivi_etat_touches_et_boutons() {
        // Suivi d'état pur (aucun événement posté) : indépendant de la permission TCC.
        let inj = QuartzInjector::new();
        inj.record_key(0x00, true); // kVK_ANSI_A
        inj.record_key(0x0B, true); // kVK_ANSI_B
        inj.record_button(NUM_GAUCHE, true);
        assert_eq!(inj.pressed_counts(), (2, 1));

        inj.record_key(0x00, false);
        inj.record_button(NUM_GAUCHE, false);
        assert_eq!(inj.pressed_counts(), (1, 0));
    }
}
