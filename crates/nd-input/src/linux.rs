//! Implémentation Linux (X11) de [`InputInjector`] via l'extension **XTEST**.
//!
//! Les événements sont synthétisés côté serveur X par `XTestFakeInput`
//! (`xtest_fake_input` dans `x11rb`) : mouvements de pointeur (absolus/relatifs),
//! boutons (molette incluse : boutons 4–7) et touches par keycode. La saisie Unicode
//! remappe temporairement un keycode libre vers le keysym voulu (technique xdotool).
//!
//! **Multi-écran** : `mouse_move_abs` honore le paramètre moniteur. Les
//! rectangles par écran proviennent de RandR (`randr_get_monitors`) ; le point
//! normalisé est projeté sur le rectangle du bon écran via la logique partagée
//! et testée [`crate::screen`], puis converti en coordonnées de la fenêtre
//! racine. Repli sur la racine entière (moniteur 0) sans RandR.
//!
//! **Wayland** : cette implémentation exige un serveur X (ou XWayland avec accès au
//! pointeur/clavier globaux, ce que les compositeurs refusent en général). Une
//! session Wayland pure passe désormais par `uinput` (voir [`crate::uinput`]) ;
//! le portail `RemoteDesktop` (xdg-desktop-portal) + libei reste la voie
//! « intégrée bureau » à venir — voir plan 07 §Wayland et `uinput.rs`.
//!
//! Aucun bloc `unsafe` ici : `x11rb` est du Rust pur (pas de bibliothèque C).

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use nd_proto::{MonitorId, NdError, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ConnectionExt as _, Keysym, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::screen::{point_absolu, MonitorRect};
use crate::{InputInjector, MouseButton};

/// `detail` de `xtest_fake_input` pour un MotionNotify : coordonnées absolues.
const MOTION_ABSOLU: u8 = 0;
/// `detail` de `xtest_fake_input` pour un MotionNotify : coordonnées relatives.
const MOTION_RELATIF: u8 = 1;

/// Numéros de boutons du protocole X11.
const BOUTON_GAUCHE: u8 = 1;
const BOUTON_MILIEU: u8 = 2;
const BOUTON_DROIT: u8 = 3;
/// Molette verticale : un « clic » bouton 4 (haut) ou 5 (bas) par cran.
const MOLETTE_HAUT: u8 = 4;
const MOLETTE_BAS: u8 = 5;
/// Molette horizontale : boutons 6 (gauche) et 7 (droite).
const MOLETTE_GAUCHE: u8 = 6;
const MOLETTE_DROITE: u8 = 7;
/// Boutons latéraux (précédent/suivant) dans la convention X11 usuelle.
const BOUTON_X1: u8 = 8;
const BOUTON_X2: u8 = 9;

/// Décalage keycode : sous Xorg (pilote evdev/libinput), keycode X = keycode evdev + 8.
const DECALAGE_EVDEV: u32 = 8;

/// Nombre de colonnes de keysyms écrites lors du remappage Unicode (normal + Maj).
const COLONNES_KEYSYMS: u8 = 2;

/// État interne suivi par l'injecteur (touches/boutons enfoncés) pour `release_all`.
#[derive(Default)]
struct InjectorState {
    /// Keycodes X11 actuellement enfoncés.
    keys: BTreeSet<u8>,
    /// Numéros de boutons X11 actuellement enfoncés.
    buttons: BTreeSet<u8>,
}

/// Injecteur d'entrées X11 fondé sur l'extension XTEST.
pub struct XtestInjector {
    conn: RustConnection,
    /// Fenêtre racine de l'écran par défaut.
    root: Window,
    /// Dimensions de la racine (bureau virtuel complet), en pixels.
    width: u16,
    height: u16,
    /// Keycode sans mapping, réservé à la saisie Unicode par remappage temporaire.
    spare_keycode: Option<u8>,
    state: Mutex<InjectorState>,
}

impl XtestInjector {
    /// Se connecte au serveur X (`$DISPLAY`) et vérifie la présence de XTEST.
    ///
    /// Échoue proprement sous session Wayland pure (pas de serveur X joignable) ou si
    /// l'extension XTEST est absente.
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| NdError::Input(format!("connexion au serveur X impossible : {e}")))?;

        // Poignée de main XTEST : détecte l'absence de l'extension dès la création.
        conn.xtest_get_version(2, 2)
            .map_err(|e| NdError::Input(format!("requête XTEST GetVersion : {e}")))?
            .reply()
            .map_err(|e| NdError::Input(format!("extension XTEST indisponible : {e}")))?;

        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| NdError::Input("écran X11 par défaut introuvable".into()))?;
        let (root, width, height) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);
        let spare_keycode = trouve_keycode_libre(&conn)?;

        Ok(Self {
            conn,
            root,
            width,
            height,
            spare_keycode,
            state: Mutex::new(InjectorState::default()),
        })
    }

    /// Verrouille l'état, en récupérant un `Mutex` empoisonné plutôt que de paniquer.
    fn lock(&self) -> MutexGuard<'_, InjectorState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mémorise l'état enfoncé/relâché d'un keycode.
    fn record_key(&self, keycode: u8, down: bool) {
        let mut st = self.lock();
        if down {
            st.keys.insert(keycode);
        } else {
            st.keys.remove(&keycode);
        }
    }

    /// Mémorise l'état enfoncé/relâché d'un bouton (numéro X11).
    fn record_button(&self, button: u8, down: bool) {
        let mut st = self.lock();
        if down {
            st.buttons.insert(button);
        } else {
            st.buttons.remove(&button);
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

    /// Envoie un événement XTEST vérifié côté serveur (touches, boutons).
    ///
    /// Le `check()` fait un aller-retour : acceptable pour ces événements peu
    /// fréquents, et il remonte les erreurs du serveur (BadValue, etc.).
    fn fake_checked(&self, type_: u8, detail: u8) -> Result<()> {
        self.conn
            .xtest_fake_input(type_, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| NdError::Input(format!("xtest_fake_input a échoué : {e}")))?
            .check()
            .map_err(|e| NdError::Input(format!("xtest_fake_input refusé par le serveur : {e}")))
    }

    /// Envoie un mouvement de pointeur en flux tendu (pas d'aller-retour par
    /// événement : les mouvements sont très fréquents), avec un simple `flush`.
    fn fake_motion(&self, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(
                MOTION_NOTIFY_EVENT,
                detail,
                x11rb::CURRENT_TIME,
                self.root,
                x,
                y,
                0,
            )
            .map_err(|e| NdError::Input(format!("xtest_fake_input (motion) a échoué : {e}")))?
            .ignore_error();
        self.conn
            .flush()
            .map_err(|e| NdError::Input(format!("flush X11 a échoué : {e}")))
    }

    /// Un « clic » complet (press + release) d'un bouton, utilisé par la molette.
    fn clic_bouton(&self, button: u8) -> Result<()> {
        self.fake_checked(BUTTON_PRESS_EVENT, button)?;
        self.fake_checked(BUTTON_RELEASE_EVENT, button)
    }

    /// Rectangles des moniteurs dans l'espace de la fenêtre racine (RandR
    /// `GetMonitors`, moniteurs actifs). `MonitorId(i)` = `i`-ième moniteur de la
    /// réponse RandR — même correspondance que l'énumération de `nd-capture`
    /// (§Linux). Repli sur la racine entière (moniteur 0 couvrant tout le bureau
    /// virtuel) si RandR est absent/trop ancien : jamais de liste vide.
    fn moniteurs(&self) -> Vec<MonitorRect> {
        let mons = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|r| r.monitors)
            .filter(|m| !m.is_empty());
        match mons {
            Some(mons) => mons
                .iter()
                .enumerate()
                .map(|(i, m)| MonitorRect {
                    id: i as u32,
                    x: i32::from(m.x),
                    y: i32::from(m.y),
                    width: u32::from(m.width),
                    height: u32::from(m.height),
                })
                .collect(),
            None => vec![MonitorRect {
                id: 0,
                x: 0,
                y: 0,
                width: u32::from(self.width),
                height: u32::from(self.height),
            }],
        }
    }
}

impl InputInjector for XtestInjector {
    fn mouse_move_abs(&self, x: f64, y: f64, monitor: MonitorId) -> Result<()> {
        // Multi-écran : projette (x, y) sur le rectangle du moniteur visé (RandR),
        // via la logique partagée et testée [`crate::screen`], puis convertit en
        // coordonnées de la fenêtre racine. `moniteurs()` ne renvoie jamais une
        // liste vide (repli racine), donc `point_absolu` réussit toujours ; le
        // `unwrap_or_else` reste défensif.
        let (px, py) = point_absolu(&self.moniteurs(), monitor, x, y).unwrap_or_else(|| {
            (
                (x.clamp(0.0, 1.0) * f64::from(self.width.saturating_sub(1))).round() as i32,
                (y.clamp(0.0, 1.0) * f64::from(self.height.saturating_sub(1))).round() as i32,
            )
        });
        self.fake_motion(MOTION_ABSOLU, en_i16(f64::from(px)), en_i16(f64::from(py)))
    }

    fn mouse_move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        self.fake_motion(MOTION_RELATIF, en_i16(dx), en_i16(dy))
    }

    fn mouse_button(&self, btn: MouseButton, down: bool) -> Result<()> {
        let button = bouton_x11(btn);
        let type_ = if down {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake_checked(type_, button)?;
        self.record_button(button, down);
        Ok(())
    }

    fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        // Molette exprimée en crans (parité avec WHEEL_DELTA côté Windows) : un clic
        // de bouton par cran, fractions arrondies. dy > 0 = vers le haut (bouton 4),
        // dx > 0 = vers la droite (bouton 7).
        let v = dy.round() as i32;
        let (bouton_v, crans_v) = if v >= 0 {
            (MOLETTE_HAUT, v)
        } else {
            (MOLETTE_BAS, -v)
        };
        for _ in 0..crans_v {
            self.clic_bouton(bouton_v)?;
        }

        let h = dx.round() as i32;
        let (bouton_h, crans_h) = if h >= 0 {
            (MOLETTE_DROITE, h)
        } else {
            (MOLETTE_GAUCHE, -h)
        };
        for _ in 0..crans_h {
            self.clic_bouton(bouton_h)?;
        }
        Ok(())
    }

    fn key(&self, scancode: u32, down: bool) -> Result<()> {
        // Le paramètre est interprété comme un keycode evdev (Linux input) ; sous Xorg
        // (pilotes evdev/libinput), keycode X = keycode evdev + 8. La conversion depuis
        // le format du protocole réseau relève de la couche de mapping (plan 07).
        let keycode = scancode
            .checked_add(DECALAGE_EVDEV)
            .filter(|&k| k <= u32::from(u8::MAX))
            .ok_or_else(|| NdError::Input(format!("keycode evdev hors plage X11 : {scancode}")))?
            as u8;
        let type_ = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake_checked(type_, keycode)?;
        self.record_key(keycode, down);
        Ok(())
    }

    fn unicode(&self, ch: char) -> Result<()> {
        // Technique xdotool : remappe temporairement un keycode libre vers le keysym du
        // caractère, frappe la touche, puis restaure le mapping. Les requêtes X11 d'une
        // même connexion sont traitées dans l'ordre : le serveur applique le mapping
        // avant la frappe, et les clients reçoivent le MappingNotify avant le KeyPress.
        let keycode = self.spare_keycode.ok_or_else(|| {
            NdError::Input("saisie Unicode impossible : aucun keycode libre à remapper".into())
        })?;
        let sym = keysym_pour_char(ch);

        change_mapping(&self.conn, keycode, sym)?;
        self.fake_checked(KEY_PRESS_EVENT, keycode)?;
        self.fake_checked(KEY_RELEASE_EVENT, keycode)?;
        // Restaure le keycode à « aucun symbole » pour ne pas polluer la disposition.
        change_mapping(&self.conn, keycode, x11rb::NO_SYMBOL)
    }

    fn release_all(&self) {
        // Récupère l'état sous verrou puis relâche hors verrou, en best-effort : en fin
        // de session on préfère tenter tous les « up » plutôt que s'arrêter au premier
        // échec.
        let (keys, buttons) = {
            let mut st = self.lock();
            let keys: Vec<u8> = st.keys.iter().copied().collect();
            let buttons: Vec<u8> = st.buttons.iter().copied().collect();
            st.keys.clear();
            st.buttons.clear();
            (keys, buttons)
        };

        for keycode in keys {
            let _ = self.fake_checked(KEY_RELEASE_EVENT, keycode);
        }
        for button in buttons {
            let _ = self.fake_checked(BUTTON_RELEASE_EVENT, button);
        }
    }
}

/// Numéro de bouton X11 correspondant à un [`MouseButton`].
fn bouton_x11(btn: MouseButton) -> u8 {
    match btn {
        MouseButton::Left => BOUTON_GAUCHE,
        MouseButton::Middle => BOUTON_MILIEU,
        MouseButton::Right => BOUTON_DROIT,
        MouseButton::X1 => BOUTON_X1,
        MouseButton::X2 => BOUTON_X2,
    }
}

/// Convertit une coordonnée en `i16` X11 en saturant aux bornes du protocole.
fn en_i16(v: f64) -> i16 {
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// Keysym X11 d'un caractère Unicode.
///
/// Latin-1 imprimable : keysym = point de code. Sinon, convention « keysym Unicode » :
/// `0x0100_0000 + point de code` (annexe du protocole X11).
fn keysym_pour_char(ch: char) -> Keysym {
    let cp = u32::from(ch);
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// Cherche un keycode sans aucun keysym associé, réutilisable pour la saisie Unicode.
///
/// Parcourt la table du serveur du haut vers le bas (les keycodes hauts sont rarement
/// utilisés par les dispositions). `None` si la table est pleine (rare).
fn trouve_keycode_libre(conn: &RustConnection) -> Result<Option<u8>> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let count = u8::try_from(u16::from(max).saturating_sub(u16::from(min)) + 1).unwrap_or(u8::MAX);

    let reply = conn
        .get_keyboard_mapping(min, count)
        .map_err(|e| NdError::Input(format!("GetKeyboardMapping a échoué : {e}")))?
        .reply()
        .map_err(|e| NdError::Input(format!("GetKeyboardMapping sans réponse : {e}")))?;

    let par_keycode = usize::from(reply.keysyms_per_keycode.max(1));
    for (i, colonnes) in reply.keysyms.chunks(par_keycode).enumerate().rev() {
        if colonnes.iter().all(|&sym| sym == x11rb::NO_SYMBOL) {
            let keycode = u16::from(min) + u16::try_from(i).unwrap_or(u16::MAX);
            if let Ok(keycode) = u8::try_from(keycode) {
                return Ok(Some(keycode));
            }
        }
    }
    Ok(None)
}

/// (Re)mappe un keycode vers un keysym unique (colonnes normal et Maj), avec
/// vérification côté serveur pour garantir l'ordre avant la frappe qui suit.
fn change_mapping(conn: &RustConnection, keycode: u8, sym: Keysym) -> Result<()> {
    conn.change_keyboard_mapping(1, keycode, COLONNES_KEYSYMS, &[sym, sym])
        .map_err(|e| NdError::Input(format!("ChangeKeyboardMapping a échoué : {e}")))?
        .check()
        .map_err(|e| NdError::Input(format!("ChangeKeyboardMapping refusé : {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_latin1_direct_et_unicode_decale() {
        // ASCII et Latin-1 imprimables : identité.
        assert_eq!(keysym_pour_char('a'), 0x61);
        assert_eq!(keysym_pour_char('é'), 0xE9);
        // Hors Latin-1 : convention 0x0100_0000 + point de code.
        assert_eq!(keysym_pour_char('€'), 0x0100_0000 + 0x20AC);
        // Les contrôles (ex. retour à la ligne) passent aussi par la forme Unicode.
        assert_eq!(keysym_pour_char('\n'), 0x0100_000A);
    }

    #[test]
    fn boutons_x11_distincts_et_hors_molette() {
        let codes = [
            bouton_x11(MouseButton::Left),
            bouton_x11(MouseButton::Middle),
            bouton_x11(MouseButton::Right),
            bouton_x11(MouseButton::X1),
            bouton_x11(MouseButton::X2),
        ];
        for (i, a) in codes.iter().enumerate() {
            // Jamais les boutons molette 4..=7.
            assert!(!(MOLETTE_HAUT..=MOLETTE_DROITE).contains(a));
            for b in &codes[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn en_i16_sature_aux_bornes() {
        assert_eq!(en_i16(0.4), 0);
        assert_eq!(en_i16(-3.6), -4);
        assert_eq!(en_i16(1e9), i16::MAX);
        assert_eq!(en_i16(-1e9), i16::MIN);
    }
}
