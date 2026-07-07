//! `entree` — capture d'entrées navigateur → événements `nd-proto`.
//!
//! Logique **pure** (aucune dépendance wasm) : convertit les valeurs brutes issues du
//! DOM (bouton souris, coordonnées, molette, touches) en [`nd_proto::InputEvent`],
//! dont la sérialisation binaire (`to_bytes`) est **identique** à celle utilisée par
//! le client natif sur le canal `Input`. Le module `client` (wasm) se contente
//! d'appeler ces fonctions puis d'émettre `event.to_bytes()` sur le transport.
//! Entièrement testable sur l'hôte.

use nd_proto::InputEvent;

/// Convertit le code bouton du DOM (`MouseEvent.button`) vers le code `nd-proto`.
///
/// DOM : `0`=gauche, `1`=milieu, `2`=droit, `3`=X1 (précédent), `4`=X2 (suivant).
/// `nd-proto` ([`InputEvent::MouseButton`]) : `0`=gauche, `1`=droit, `2`=milieu,
/// `3`=X1, `4`=X2. Tout autre code est rabattu sur X2, comme le fait
/// `nd_core::apply_input` côté machine contrôlée.
#[must_use]
pub fn bouton_dom_vers_nd(bouton_dom: i16) -> u8 {
    match bouton_dom {
        0 => 0, // gauche
        1 => 2, // milieu
        2 => 1, // droit
        3 => 3, // X1
        _ => 4, // X2 (et repli)
    }
}

/// Événement « bouton souris » à partir d'un `MouseEvent.button` du DOM.
#[must_use]
pub fn souris_bouton(bouton_dom: i16, enfonce: bool) -> InputEvent {
    InputEvent::MouseButton {
        button: bouton_dom_vers_nd(bouton_dom),
        down: enfonce,
    }
}

/// Normalise une composante pixel sur `[0.0, 1.0]` (bornée), en se prémunissant d'une
/// dimension nulle.
#[must_use]
fn normaliser(valeur: f64, taille: f64) -> f64 {
    if taille <= 0.0 {
        return 0.0;
    }
    (valeur / taille).clamp(0.0, 1.0)
}

/// Déplacement absolu (coordonnées normalisées 0.0–1.0) à partir d'une position pixel
/// **dans le canvas** de taille `largeur × hauteur`.
///
/// Le résultat est borné à `[0, 1]` : le pointeur qui déborde légèrement du canvas
/// reste projeté sur le bord du moniteur cible.
#[must_use]
pub fn souris_deplacement_abs(
    x_px: f64,
    y_px: f64,
    largeur: f64,
    hauteur: f64,
    moniteur: u32,
) -> InputEvent {
    InputEvent::MouseMoveAbs {
        x: normaliser(x_px, largeur),
        y: normaliser(y_px, hauteur),
        monitor: moniteur,
    }
}

/// Nombre de pixels d'un « cran » de molette (`WheelEvent`, convention historique
/// `WHEEL_DELTA` = 120).
const PIXELS_PAR_CRAN: f64 = 120.0;

/// Molette : convertit un `WheelEvent` (`deltaX`/`deltaY` en pixels, `deltaY` positif
/// vers le bas) en crans `nd-proto` (positif = haut/droite). L'axe Y est **inversé**
/// pour respecter cette convention.
#[must_use]
pub fn souris_molette(delta_x: f64, delta_y: f64) -> InputEvent {
    InputEvent::Scroll {
        dx: delta_x / PIXELS_PAR_CRAN,
        dy: -delta_y / PIXELS_PAR_CRAN,
    }
}

/// Touche par **scancode physique** (déjà résolu depuis `KeyboardEvent.code`, voir
/// [`scancode_depuis_code`]).
#[must_use]
pub fn touche(scancode: u32, enfonce: bool) -> InputEvent {
    InputEvent::Key {
        scancode,
        down: enfonce,
    }
}

/// Caractère Unicode (point de code) — pour la saisie de texte (`keypress`/`input`).
#[must_use]
pub fn unicode(point_de_code: u32) -> InputEvent {
    InputEvent::Unicode {
        codepoint: point_de_code,
    }
}

/// Traduit une valeur `KeyboardEvent.code` (identité **physique** de la touche,
/// indépendante de la disposition) en scancode Windows PS/2 set 1 — le format attendu
/// par l'injecteur côté machine contrôlée (`nd-input`).
///
/// Couverture **partielle mais représentative** : lettres, chiffres du rang
/// supérieur, touches d'édition/navigation et modificateurs courants. Une touche non
/// couverte renvoie `None` ; le client se rabat alors sur l'envoi d'un événement
/// [`InputEvent::Unicode`] pour les caractères imprimables. Table de correspondance :
/// codes W3C UI Events ↔ scancodes PS/2 (jeu 1).
#[must_use]
pub fn scancode_depuis_code(code: &str) -> Option<u32> {
    let sc: u32 = match code {
        // Rangée des lettres
        "KeyA" => 0x1E,
        "KeyB" => 0x30,
        "KeyC" => 0x2E,
        "KeyD" => 0x20,
        "KeyE" => 0x12,
        "KeyF" => 0x21,
        "KeyG" => 0x22,
        "KeyH" => 0x23,
        "KeyI" => 0x17,
        "KeyJ" => 0x24,
        "KeyK" => 0x25,
        "KeyL" => 0x26,
        "KeyM" => 0x32,
        "KeyN" => 0x31,
        "KeyO" => 0x18,
        "KeyP" => 0x19,
        "KeyQ" => 0x10,
        "KeyR" => 0x13,
        "KeyS" => 0x1F,
        "KeyT" => 0x14,
        "KeyU" => 0x16,
        "KeyV" => 0x2F,
        "KeyW" => 0x11,
        "KeyX" => 0x2D,
        "KeyY" => 0x15,
        "KeyZ" => 0x2C,
        // Chiffres du rang supérieur
        "Digit1" => 0x02,
        "Digit2" => 0x03,
        "Digit3" => 0x04,
        "Digit4" => 0x05,
        "Digit5" => 0x06,
        "Digit6" => 0x07,
        "Digit7" => 0x08,
        "Digit8" => 0x09,
        "Digit9" => 0x0A,
        "Digit0" => 0x0B,
        // Contrôle / édition
        "Escape" => 0x01,
        "Backspace" => 0x0E,
        "Tab" => 0x0F,
        "Enter" => 0x1C,
        "Space" => 0x39,
        "Minus" => 0x0C,
        "Equal" => 0x0D,
        "BracketLeft" => 0x1A,
        "BracketRight" => 0x1B,
        "Backslash" => 0x2B,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Backquote" => 0x29,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "CapsLock" => 0x3A,
        // Modificateurs (côté gauche ; les scancodes étendus E0 des variantes droites
        // sont hors de ce sous-ensemble minimal)
        "ShiftLeft" => 0x2A,
        "ShiftRight" => 0x36,
        "ControlLeft" => 0x1D,
        "AltLeft" => 0x38,
        // Fonctions
        "F1" => 0x3B,
        "F2" => 0x3C,
        "F3" => 0x3D,
        "F4" => 0x3E,
        "F5" => 0x3F,
        "F6" => 0x40,
        "F7" => 0x41,
        "F8" => 0x42,
        "F9" => 0x43,
        "F10" => 0x44,
        "F11" => 0x57,
        "F12" => 0x58,
        _ => return None,
    };
    Some(sc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_boutons_dom_vers_nd() {
        assert_eq!(bouton_dom_vers_nd(0), 0); // gauche → gauche
        assert_eq!(bouton_dom_vers_nd(1), 2); // milieu → milieu (code 2)
        assert_eq!(bouton_dom_vers_nd(2), 1); // droit → droit (code 1)
        assert_eq!(bouton_dom_vers_nd(3), 3); // X1
        assert_eq!(bouton_dom_vers_nd(4), 4); // X2
        assert_eq!(bouton_dom_vers_nd(9), 4); // inconnu → X2
    }

    #[test]
    fn bouton_serialise_comme_nd_proto() {
        // L'événement produit doit être exactement celui de nd-proto (roundtrip).
        let ev = souris_bouton(2, true);
        assert_eq!(
            ev,
            InputEvent::MouseButton {
                button: 1,
                down: true
            }
        );
        assert_eq!(InputEvent::from_bytes(&ev.to_bytes()), Some(ev));
    }

    #[test]
    fn deplacement_abs_normalise_et_borne() {
        // Milieu du canvas 800×600.
        let ev = souris_deplacement_abs(400.0, 300.0, 800.0, 600.0, 0);
        assert_eq!(
            ev,
            InputEvent::MouseMoveAbs {
                x: 0.5,
                y: 0.5,
                monitor: 0
            }
        );
        // Débordement → borné à 1.0 ; dimension nulle → 0.0.
        match souris_deplacement_abs(2000.0, -50.0, 800.0, 0.0, 1) {
            InputEvent::MouseMoveAbs { x, y, monitor } => {
                assert_eq!(x, 1.0);
                assert_eq!(y, 0.0);
                assert_eq!(monitor, 1);
            }
            autre => panic!("variante inattendue : {autre:?}"),
        }
    }

    #[test]
    fn molette_inverse_axe_y() {
        // deltaY positif (défilement vers le bas) → dy négatif (convention nd-proto).
        match souris_molette(0.0, 120.0) {
            InputEvent::Scroll { dx, dy } => {
                assert_eq!(dx, 0.0);
                assert_eq!(dy, -1.0);
            }
            autre => panic!("variante inattendue : {autre:?}"),
        }
    }

    #[test]
    fn scancodes_connus_et_inconnus() {
        assert_eq!(scancode_depuis_code("KeyA"), Some(0x1E));
        assert_eq!(scancode_depuis_code("Enter"), Some(0x1C));
        assert_eq!(scancode_depuis_code("Digit0"), Some(0x0B));
        assert_eq!(scancode_depuis_code("F12"), Some(0x58));
        assert_eq!(scancode_depuis_code("MediaPlayPause"), None);
        // La touche produit bien un événement Key sérialisable.
        let ev = touche(scancode_depuis_code("KeyA").unwrap(), true);
        assert_eq!(InputEvent::from_bytes(&ev.to_bytes()), Some(ev));
    }

    #[test]
    fn unicode_roundtrip() {
        let ev = unicode(u32::from('é'));
        assert_eq!(InputEvent::from_bytes(&ev.to_bytes()), Some(ev));
    }
}
