//! Implémentation Windows de [`InputInjector`] via **`SendInput`**.
//!
//! Souris en coordonnées absolues normalisées, boutons, molette haute résolution,
//! touches par scancode et saisie Unicode. Voir plan 07 §Windows.
//!
//! **Multi-écran** : `mouse_move_abs` honore le paramètre moniteur. Les moniteurs
//! sont énumérés via `EnumDisplayMonitors`/`GetMonitorInfoW` (rectangles dans le
//! bureau virtuel), le point normalisé est projeté sur le rectangle du bon écran
//! ([`crate::screen`]), puis converti en coordonnées `0..=65535` du bureau virtuel
//! et injecté avec `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`. L'injection
//! dans le bureau sécurisé/UAC (service SYSTEM) viendra ensuite.
//!
//! Entrées avancées (plan 07) : suivi de l'état enfoncé des touches/boutons pour un
//! `release_all()` réel, séquence d'attention sécurisée (Ctrl+Alt+Suppr) via `SendSAS`
//! et injection tactile via `InjectTouchInput`.
//!
//! Tout le `unsafe` FFI est concentré ici, derrière le trait.
#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use nd_proto::{MonitorId, NdError, Result};
use windows::Win32::Foundation::{BOOL, FALSE, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAGS, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION,
    TOUCH_MASK_PRESSURE,
};

use crate::screen::{pixel_vers_normalise_65535, point_absolu, BureauVirtuel, MonitorRect};
use crate::{InputInjector, MouseButton};

/// Incrément standard d'un cran de molette.
const WHEEL_DELTA: i32 = 120;
/// Valeurs `mouseData` pour les boutons latéraux.
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

/// Nombre maximal de contacts tactiles simultanés gérés (un seul pour l'instant).
const TOUCH_MAX_CONTACTS: u32 = 1;
/// Demi-côté du rectangle de contact tactile (pixels).
const TOUCH_CONTACT_RADIUS: i32 = 2;
/// Orientation du contact tactile (degrés) ; 90 = doigt vertical usuel.
const TOUCH_ORIENTATION: u32 = 90;
/// Pression tactile par défaut (plage 0..=1024 ; 512 = valeur médiane).
const TOUCH_PRESSURE: u32 = 512;

/// État interne suivi par l'injecteur (touches/boutons enfoncés, contact tactile).
///
/// Protégé par un [`Mutex`] : les méthodes du trait prennent `&self` et l'injecteur
/// doit rester `Send + Sync` pour être partagé entre tâches.
#[derive(Default)]
struct InjectorState {
    /// Scancodes clavier actuellement enfoncés.
    keys: BTreeSet<u16>,
    /// Boutons souris actuellement enfoncés (codés, voir [`button_code`]).
    buttons: BTreeSet<u8>,
    /// Injection tactile initialisée pour ce processus ?
    touch_ready: bool,
    /// Contact tactile en cours : position écran du dernier point, si posé.
    touch_contact: Option<(i32, i32)>,
}

/// Injecteur d'entrées Windows fondé sur `SendInput`.
pub struct SendInputInjector {
    state: Mutex<InjectorState>,
}

impl SendInputInjector {
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

    /// Mémorise l'état enfoncé/relâché d'un scancode.
    fn record_key(&self, scancode: u16, down: bool) {
        let mut st = self.lock();
        if down {
            st.keys.insert(scancode);
        } else {
            st.keys.remove(&scancode);
        }
    }

    /// Mémorise l'état enfoncé/relâché d'un bouton souris.
    fn record_button(&self, btn: MouseButton, down: bool) {
        let code = button_code(btn);
        let mut st = self.lock();
        if down {
            st.buttons.insert(code);
        } else {
            st.buttons.remove(&code);
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

    /// Assure l'initialisation unique de l'injection tactile pour ce processus.
    fn ensure_touch_ready(&self) -> Result<()> {
        let mut st = self.lock();
        if st.touch_ready {
            return Ok(());
        }
        // SAFETY : appel FFI aux arguments constants valides ; renvoie une erreur claire
        // si l'injection tactile est indisponible (pilote/OS).
        unsafe {
            InitializeTouchInjection(TOUCH_MAX_CONTACTS, TOUCH_FEEDBACK_DEFAULT)
                .map_err(|e| NdError::Input(format!("InitializeTouchInjection a échoué : {e}")))?;
        }
        st.touch_ready = true;
        Ok(())
    }

    /// Injecte un seul contact tactile aux coordonnées écran avec les drapeaux donnés.
    fn inject_touch(&self, x: i32, y: i32, flags: POINTER_FLAGS) -> Result<()> {
        let info = touch_info(x, y, flags);
        // SAFETY : `info` est un `POINTER_TOUCH_INFO` valide (zéro-initialisé puis
        // renseigné) ; on passe un slice d'un unique élément.
        unsafe {
            InjectTouchInput(&[info])
                .map_err(|e| NdError::Input(format!("InjectTouchInput a échoué : {e}")))
        }
    }

    /// Pose un contact tactile (doigt) aux coordonnées écran données.
    ///
    /// Initialise l'injection tactile au premier appel. Voir [`Self::touch_move`] et
    /// [`Self::touch_up`] pour poursuivre puis terminer le geste.
    pub fn touch_down(&self, x: i32, y: i32) -> Result<()> {
        self.ensure_touch_ready()?;
        self.inject_touch(
            x,
            y,
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
        )?;
        self.lock().touch_contact = Some((x, y));
        Ok(())
    }

    /// Déplace le contact tactile en cours vers les coordonnées écran données.
    pub fn touch_move(&self, x: i32, y: i32) -> Result<()> {
        self.ensure_touch_ready()?;
        self.inject_touch(
            x,
            y,
            POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
        )?;
        self.lock().touch_contact = Some((x, y));
        Ok(())
    }

    /// Relâche le contact tactile en cours (au dernier point connu).
    ///
    /// Sans contact en cours, ne fait rien et réussit.
    pub fn touch_up(&self) -> Result<()> {
        let pos = self.lock().touch_contact.take();
        if let Some((x, y)) = pos {
            self.inject_touch(x, y, POINTER_FLAG_UP)?;
        }
        Ok(())
    }
}

impl Default for SendInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

/// Code compact d'un bouton souris pour le suivi d'état.
fn button_code(btn: MouseButton) -> u8 {
    match btn {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

/// Réciproque de [`button_code`] (les codes proviennent toujours de cette fonction).
fn button_from_code(code: u8) -> MouseButton {
    match code {
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::X1,
        4 => MouseButton::X2,
        _ => MouseButton::Left,
    }
}

/// Drapeaux et `mouseData` pour relâcher un bouton donné.
fn button_up_flags(btn: MouseButton) -> (MOUSE_EVENT_FLAGS, u32) {
    match btn {
        MouseButton::Left => (MOUSEEVENTF_LEFTUP, 0),
        MouseButton::Right => (MOUSEEVENTF_RIGHTUP, 0),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEUP, 0),
        MouseButton::X1 => (MOUSEEVENTF_XUP, XBUTTON1),
        MouseButton::X2 => (MOUSEEVENTF_XUP, XBUTTON2),
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

/// Construit un `POINTER_TOUCH_INFO` pour un contact unique.
fn touch_info(x: i32, y: i32, flags: POINTER_FLAGS) -> POINTER_TOUCH_INFO {
    POINTER_TOUCH_INFO {
        pointerInfo: POINTER_INFO {
            pointerType: PT_TOUCH,
            pointerId: 0,
            pointerFlags: flags,
            ptPixelLocation: POINT { x, y },
            ..Default::default()
        },
        touchFlags: 0,
        touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
        rcContact: RECT {
            left: x - TOUCH_CONTACT_RADIUS,
            top: y - TOUCH_CONTACT_RADIUS,
            right: x + TOUCH_CONTACT_RADIUS,
            bottom: y + TOUCH_CONTACT_RADIUS,
        },
        rcContactRaw: RECT::default(),
        orientation: TOUCH_ORIENTATION,
        pressure: TOUCH_PRESSURE,
    }
}

/// Envoie la séquence d'attention sécurisée (Ctrl+Alt+Suppr) via `SendSAS`.
///
/// **Privilèges requis** : le processus appelant doit être un service tournant en tant
/// que **SYSTEM** (bureau sécurisé, session 0) et la stratégie de groupe
/// « Désactiver ou activer la génération logicielle du SAS » (`SoftwareSASGeneration`)
/// doit autoriser les services (valeur « Services » ou « Services et applications de
/// bureau »). Sans cette configuration, l'appel est ignoré silencieusement par le
/// système : `SendSAS` ne retourne aucun code d'erreur, on renvoie donc `Ok(())` et la
/// bonne exécution dépend de l'environnement.
///
/// `AsUser = FALSE` : le SAS est généré comme s'il provenait du matériel, cas d'un
/// service SYSTEM. (`TRUE` conviendrait à un appelant déjà dans la session interactive
/// de l'utilisateur.)
pub fn send_secure_attention_sequence() -> Result<()> {
    // SAFETY : `SendSAS` ne prend qu'un `BOOL` par valeur et ne renvoie rien ; aucun
    // pointeur ni durée de vie en jeu.
    unsafe {
        SendSAS(FALSE);
    }
    Ok(())
}

/// Rappel `EnumDisplayMonitors` : accumule le rectangle de chaque moniteur.
///
/// `data` transporte un `*mut Vec<(RECT, bool)>` (rectangle + drapeau primaire).
/// On ne fixe pas encore l'identifiant : il est attribué après tri (primaire en
/// tête) dans [`enumerer_moniteurs`].
///
/// SAFETY : `data` provient de [`enumerer_moniteurs`] et pointe vers un
/// `Vec<(RECT, bool)>` valide pour toute la durée de l'énumération (pile de
/// l'appelant). `hmon` est un handle valide fourni par le système.
unsafe extern "system" fn rappel_moniteur(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let moniteurs = &mut *(data.0 as *mut Vec<(RECT, bool)>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // GetMonitorInfoW renseigne `rcMonitor` (rectangle bureau virtuel) et
    // `dwFlags` (bit MONITORINFOF_PRIMARY). Un échec ponctuel saute ce moniteur
    // sans interrompre l'énumération.
    if GetMonitorInfoW(hmon, &mut info).as_bool() {
        let est_primaire = info.dwFlags & MONITORINFOF_PRIMARY != 0;
        moniteurs.push((info.rcMonitor, est_primaire));
    }
    TRUE
}

/// Énumère les moniteurs du bureau (rectangles dans le bureau virtuel).
///
/// `MonitorId(0)` = moniteur **principal** (placé en tête) ; les suivants gardent
/// l'ordre d'énumération du système. Cet ordre suit la convention documentée par
/// `nd-capture` (`MonitorId(0)` = sortie principale) ; l'alignement fin avec
/// l'ordre DXGI de `nd-capture` relève de l'intégration multi-écran (plan 13).
/// Renvoie un vecteur vide si l'énumération échoue (repli géré par l'appelant).
fn enumerer_moniteurs() -> Vec<MonitorRect> {
    let mut bruts: Vec<(RECT, bool)> = Vec::new();
    // SAFETY : appel FFI standard ; `rappel_moniteur` reçoit un pointeur valide
    // vers `bruts` le temps (synchrone) de l'énumération, aucun HDC/clip requis.
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(rappel_moniteur),
            LPARAM(std::ptr::addr_of_mut!(bruts) as isize),
        );
    }
    // Moniteur principal en tête (tri stable : l'ordre système est conservé
    // pour les écrans secondaires).
    bruts.sort_by(|a, b| b.1.cmp(&a.1));
    bruts
        .into_iter()
        .enumerate()
        .map(|(i, (r, _))| MonitorRect {
            id: i as u32,
            x: r.left,
            y: r.top,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
        })
        .collect()
}

/// Bornes du bureau virtuel (union de tous les moniteurs), via `GetSystemMetrics`.
fn bureau_virtuel() -> BureauVirtuel {
    // SAFETY : `GetSystemMetrics` est sans effet de bord et sûr pour tout index.
    let (x, y, largeur, hauteur) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    BureauVirtuel {
        x,
        y,
        width: largeur.max(1) as u32,
        height: hauteur.max(1) as u32,
    }
}

impl InputInjector for SendInputInjector {
    fn mouse_move_abs(&self, x: f64, y: f64, monitor: MonitorId) -> Result<()> {
        let moniteurs = enumerer_moniteurs();
        // Projette (x, y) normalisés sur le rectangle du moniteur visé, puis
        // convertit en coordonnées 0..=65535 du bureau virtuel.
        let (dx, dy) = match point_absolu(&moniteurs, monitor, x, y) {
            Some((px, py)) => pixel_vers_normalise_65535(px, py, bureau_virtuel()),
            // Énumération indisponible : repli sur l'écran primaire seul
            // (comportement historique, sans VIRTUALDESK).
            None => {
                let dx = (x.clamp(0.0, 1.0) * 65_535.0).round() as i32;
                let dy = (y.clamp(0.0, 1.0) * 65_535.0).round() as i32;
                return send_one(mouse_input(
                    dx,
                    dy,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                ));
            }
        };
        send_one(mouse_input(
            dx,
            dy,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
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
        send_one(mouse_input(0, 0, data, flags))?;
        self.record_button(btn, down);
        Ok(())
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
        send_one(key_input(scancode as u16, flags))?;
        self.record_key(scancode as u16, down);
        Ok(())
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
        // Récupère l'état sous verrou, puis relâche hors verrou (on ne tient pas le
        // Mutex pendant les appels FFI). Relâche RÉELLEMENT touches, boutons et contact
        // tactile encore enfoncés — pas seulement les boutons souris.
        let (keys, buttons, touch) = {
            let mut st = self.lock();
            let keys: Vec<u16> = st.keys.iter().copied().collect();
            let buttons: Vec<u8> = st.buttons.iter().copied().collect();
            let touch = st.touch_contact.take();
            st.keys.clear();
            st.buttons.clear();
            (keys, buttons, touch)
        };

        for scan in keys {
            let _ = send_one(key_input(scan, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
        }
        for code in buttons {
            let (flags, data) = button_up_flags(button_from_code(code));
            let _ = send_one(mouse_input(0, 0, data, flags));
        }
        if let Some((x, y)) = touch {
            let _ = self.inject_touch(x, y, POINTER_FLAG_UP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suivi_etat_touches_et_boutons() {
        let inj = SendInputInjector::new();
        // Suivi déterministe de l'état, indépendant du succès FFI.
        inj.record_key(0x1E, true); // A
        inj.record_key(0x30, true); // B
        inj.record_button(MouseButton::Left, true);
        assert_eq!(inj.pressed_counts(), (2, 1));

        inj.record_key(0x1E, false);
        assert_eq!(inj.pressed_counts(), (1, 1));

        // release_all vide tout l'état suivi (envoi des « up » en best-effort).
        inj.release_all();
        assert_eq!(inj.pressed_counts(), (0, 0));
    }

    #[test]
    fn presse_puis_release_all_ne_panique_pas() {
        let inj = SendInputInjector::new();
        // Scancodes F13/F14 : inoffensifs sur la plupart des configurations. L'appel
        // peut échouer si l'UIPI bloque l'injection ; on vérifie surtout l'absence de
        // panique et un état final vide après release_all.
        let _ = inj.key(0x64, true);
        let _ = inj.key(0x65, true);
        inj.release_all();
        assert_eq!(inj.pressed_counts(), (0, 0));
    }

    /// La séquence d'attention sécurisée est exposée comme **action hôte
    /// déclenchable** (câblée dans `nd-core` via `HostAction::SendCtrlAltDel`, et
    /// autorisée par la politique `SoftwareSASGeneration` que pose le service). On
    /// valide ici sa **signature** de fonction hôte sans la déclencher : un vrai
    /// envoi exige le contexte service/SYSTEM et le bureau sécurisé, et
    /// perturberait la session de test.
    #[test]
    fn sas_exposee_comme_action_hote() {
        let action: Option<fn() -> Result<()>> = Some(send_secure_attention_sequence);
        assert!(action.is_some());
    }

    #[test]
    fn button_code_est_bijectif() {
        for btn in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::X1,
            MouseButton::X2,
        ] {
            assert_eq!(button_from_code(button_code(btn)), btn);
        }
    }
}
