//! Implémentation Linux **Wayland** de [`InputInjector`] via **`/dev/uinput`**
//! (niveau noyau/evdev, `libc` — pas de bibliothèque C à lier).
//!
//! En session Wayland pure, les compositeurs refusent l'injection globale par
//! XTEST (voir `linux.rs`). La voie robuste et universelle est de créer un
//! **périphérique d'entrée virtuel** via le sous-système uinput du noyau : les
//! événements (clavier, boutons, molette, déplacement) sont émis sous les
//! couches d'affichage, donc honorés aussi bien sous Wayland que sous X11.
//!
//! # Séquence
//!
//! `open("/dev/uinput")` → déclaration des capacités (`UI_SET_EVBIT`/`KEYBIT`/
//! `RELBIT`/`ABSBIT`) → écriture d'un [`libc::uinput_user_dev`] (nom, plages des
//! axes absolus) → `UI_DEV_CREATE`. Chaque geste est une écriture d'un ou
//! plusieurs [`libc::input_event`] suivie d'un `SYN_REPORT`. Le `Drop` détruit le
//! périphérique (`UI_DEV_DESTROY`) et ferme le descripteur.
//!
//! # Droits requis (à valider sur la vraie plateforme)
//!
//! `/dev/uinput` n'est accessible qu'à `root` ou aux membres du groupe autorisé
//! par une règle udev. Le déploiement fournira cette règle (ou un service
//! privilégié) — voir plan 07 §Wayland. Sans droits, [`UinputInjector::new`]
//! renvoie une erreur claire (jamais de panique) et l'appelant peut retomber sur
//! XTEST si un serveur X est joignable (voir `create_injector`).
//!
//! # Limites assumées
//!
//! * **Ciblage par moniteur** : un périphérique absolu uinform expose une plage
//!   `0..=65535` que le compositeur étale sur **tout le bureau virtuel**. Le
//!   ciblage *par écran* exige la géométrie des sorties (`wl_output` / portail),
//!   inconnue d'un périphérique uinput isolé ; `mouse_move_abs` projette donc
//!   `(x, y)` sur l'ensemble du bureau (correct en mono-écran, à affiner en
//!   multi-écran via le portail).
//! * **Unicode** : uinput est au niveau *keycode* evdev, sans notion de
//!   caractère ; [`UinputInjector::unicode`] renvoie une erreur explicite.
//!
//! # Voie « intégrée bureau » à venir : portail RemoteDesktop + libei
//!
//! L'alternative sans privilège spécial est le portail
//! `org.freedesktop.portal.RemoteDesktop` (xdg-desktop-portal) couplé à **libei**
//! (protocole d'émulation d'entrées) : `CreateSession` → `SelectDevices` →
//! `Start` (consentement utilisateur) → `ConnectToEIS` (descripteur EI) → poignée
//! de main libei puis émission clavier/souris. Cette voie, asynchrone et
//! dépendante d'un portail en cours d'exécution, est le prolongement prévu ;
//! uinput reste le repli fonctionnel et testable retenu ici (voir plan 07
//! §Wayland). Ce module compile en cible `x86_64-unknown-linux-gnu` (libc = Rust
//! pur) ; l'injection réelle est à valider sur une session Wayland avec droits
//! uinput (non reproductible sur le poste Windows de développement).
//!
//! Tout le `unsafe` FFI (`open`/`ioctl`/`write`/`close`) est concentré ici,
//! derrière le trait.
#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use libc::{c_char, c_int, c_ulong};
use nd_proto::{MonitorId, NdError, Result};

use crate::{InputInjector, MouseButton};

// --- Encodage des numéros d'ioctl (asm-generic : x86_64/aarch64/arm) ---------

const IOC_NRBITS: c_ulong = 8;
const IOC_TYPEBITS: c_ulong = 8;
const IOC_SIZEBITS: c_ulong = 14;
const IOC_NRSHIFT: c_ulong = 0;
const IOC_TYPESHIFT: c_ulong = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: c_ulong = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: c_ulong = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_NONE: c_ulong = 0;
const IOC_WRITE: c_ulong = 1;

/// Encode un numéro d'ioctl (équivalent de la macro noyau `_IOC`).
const fn ioc(dir: c_ulong, typ: c_ulong, nr: c_ulong, size: c_ulong) -> c_ulong {
    (dir << IOC_DIRSHIFT) | (typ << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

/// Base d'ioctl uinput (lettre magique « U »).
const UINPUT_IOCTL_BASE: c_ulong = b'U' as c_ulong;

/// `_IOW(UINPUT_IOCTL_BASE, nr, int)` : ioctl d'écriture d'un `int`.
const fn iow_int(nr: c_ulong) -> c_ulong {
    ioc(
        IOC_WRITE,
        UINPUT_IOCTL_BASE,
        nr,
        std::mem::size_of::<c_int>() as c_ulong,
    )
}

/// `_IO(UINPUT_IOCTL_BASE, nr)` : ioctl sans argument.
const fn io_uinput(nr: c_ulong) -> c_ulong {
    ioc(IOC_NONE, UINPUT_IOCTL_BASE, nr, 0)
}

const UI_DEV_CREATE: c_ulong = io_uinput(1);
const UI_DEV_DESTROY: c_ulong = io_uinput(2);
const UI_SET_EVBIT: c_ulong = iow_int(100);
const UI_SET_KEYBIT: c_ulong = iow_int(101);
const UI_SET_RELBIT: c_ulong = iow_int(102);
const UI_SET_ABSBIT: c_ulong = iow_int(103);

// --- Codes d'événements evdev (linux/input-event-codes.h) --------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
/// Dernier code EV_KEY valide (inclut les `BTN_*` souris) ; borne des capacités
/// clavier déclarées et de la validation des keycodes reçus.
const KEY_MAX: u16 = 0x2ff;
/// Bus « USB » : périphérique virtuel généralement accepté par libinput.
const BUS_USB: u16 = 0x03;
/// Pleine échelle des axes absolus (plage logique du pointeur virtuel).
const ABS_PLEINE_ECHELLE: i32 = 65_535;

/// Chemin du nœud uinput du noyau.
const CHEMIN_UINPUT: &[u8] = b"/dev/uinput\0";
/// Nom présenté du périphérique virtuel (visible dans `libinput list-devices`).
const NOM_PERIPHERIQUE: &[u8] = b"NovaDesk Virtual Input";

/// État suivi pour un `release_all` réel (touches/boutons enfoncés).
#[derive(Default)]
struct EtatUinput {
    /// Codes EV_KEY clavier actuellement enfoncés.
    keys: BTreeSet<u16>,
    /// Codes `BTN_*` souris actuellement enfoncés.
    buttons: BTreeSet<u16>,
}

/// Injecteur d'entrées Wayland fondé sur un périphérique virtuel uinput.
pub struct UinputInjector {
    /// Descripteur ouvert sur `/dev/uinput` (fermé au `Drop`).
    fd: c_int,
    state: Mutex<EtatUinput>,
}

impl UinputInjector {
    /// Ouvre `/dev/uinput`, déclare les capacités et crée le périphérique
    /// virtuel. Échoue proprement (jamais de panique) sans droits d'accès ou si
    /// le module `uinput` n'est pas chargé.
    pub fn new() -> Result<Self> {
        // SAFETY : `CHEMIN_UINPUT` est terminé par NUL ; open ne conserve pas le
        // pointeur au-delà de l'appel.
        let fd = unsafe {
            libc::open(
                CHEMIN_UINPUT.as_ptr().cast::<c_char>(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(NdError::Input(format!(
                "uinput : ouverture de /dev/uinput impossible (droits/règle udev ? module uinput chargé ?) : {}",
                std::io::Error::last_os_error()
            )));
        }
        let injecteur = UinputInjector {
            fd,
            state: Mutex::new(EtatUinput::default()),
        };
        // En cas d'échec de configuration, le `Drop` d'`injecteur` ferme le fd.
        injecteur.configurer()?;
        Ok(injecteur)
    }

    /// Verrouille l'état, en récupérant un `Mutex` empoisonné plutôt que de paniquer.
    fn lock(&self) -> MutexGuard<'_, EtatUinput> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Enveloppe d'`ioctl` uinput ; `arg` est ignoré par le noyau pour les
    /// ioctls sans argument (`_IO`).
    ///
    /// SAFETY : `self.fd` est un descripteur valide ouvert sur `/dev/uinput` ;
    /// `req` est un numéro d'ioctl uinput bien formé.
    unsafe fn ui_ioctl(&self, req: c_ulong, arg: c_int) -> c_int {
        libc::ioctl(self.fd, req as _, arg)
    }

    /// Active un bit de capacité (`UI_SET_*BIT`).
    fn set_bit(&self, req: c_ulong, bit: u16) -> Result<()> {
        // SAFETY : ioctl d'écriture d'un `int` sur un fd uinput valide.
        if unsafe { self.ui_ioctl(req, c_int::from(bit)) } < 0 {
            return Err(NdError::Input(format!(
                "uinput : ioctl {req:#x} (bit {bit}) a échoué : {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Déclare les capacités du périphérique puis le crée.
    fn configurer(&self) -> Result<()> {
        // Types d'événements pris en charge.
        for ev in [EV_SYN, EV_KEY, EV_REL, EV_ABS] {
            self.set_bit(UI_SET_EVBIT, ev)?;
        }
        // Toute la plage EV_KEY (touches clavier + boutons souris `BTN_*`).
        for code in 1..=KEY_MAX {
            self.set_bit(UI_SET_KEYBIT, code)?;
        }
        // Axes relatifs : déplacement et molette (verticale + horizontale).
        for code in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
            self.set_bit(UI_SET_RELBIT, code)?;
        }
        // Axes absolus : positionnement du pointeur.
        for code in [ABS_X, ABS_Y] {
            self.set_bit(UI_SET_ABSBIT, code)?;
        }
        self.creer_peripherique()
    }

    /// Écrit la description du périphérique (nom, plages des axes) puis
    /// `UI_DEV_CREATE`.
    fn creer_peripherique(&self) -> Result<()> {
        // SAFETY : `uinput_user_dev` est un POD C ; le zéro est un état valide
        // (tableaux absmin/absmax/... nuls hormis les axes renseignés).
        let mut dev: libc::uinput_user_dev = unsafe { std::mem::zeroed() };
        for (dst, &src) in dev.name.iter_mut().zip(NOM_PERIPHERIQUE.iter()) {
            *dst = src as c_char;
        }
        dev.id = libc::input_id {
            bustype: BUS_USB,
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
        };
        dev.absmin[ABS_X as usize] = 0;
        dev.absmax[ABS_X as usize] = ABS_PLEINE_ECHELLE;
        dev.absmin[ABS_Y as usize] = 0;
        dev.absmax[ABS_Y as usize] = ABS_PLEINE_ECHELLE;

        let taille = std::mem::size_of::<libc::uinput_user_dev>();
        // SAFETY : on écrit `taille` octets depuis une structure valide et vivante.
        let n = unsafe { libc::write(self.fd, std::ptr::addr_of!(dev).cast(), taille) };
        if n != taille as isize {
            return Err(NdError::Input(format!(
                "uinput : écriture de la description du périphérique a échoué : {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY : ioctl de création sur un fd uinput configuré.
        if unsafe { self.ui_ioctl(UI_DEV_CREATE, 0) } < 0 {
            return Err(NdError::Input(format!(
                "uinput : UI_DEV_CREATE a échoué : {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Émet un unique `input_event`.
    fn emit(&self, type_: u16, code: u16, value: i32) -> Result<()> {
        // SAFETY : `input_event` est un POD C ; le zéro (horodatage inclus, le
        // noyau le renseigne) est valide.
        let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
        ev.type_ = type_;
        ev.code = code;
        ev.value = value;
        let taille = std::mem::size_of::<libc::input_event>();
        // SAFETY : on écrit `taille` octets depuis une structure valide et vivante.
        let n = unsafe { libc::write(self.fd, std::ptr::addr_of!(ev).cast(), taille) };
        if n != taille as isize {
            return Err(NdError::Input(format!(
                "uinput : écriture d'un événement (type {type_}, code {code}) a échoué : {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Publie les événements accumulés (fin de rapport).
    fn syn(&self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
}

/// Quantifie une coordonnée normalisée `[0, 1]` vers la plage absolue uinput
/// `0..=65535` (bornée aux extrêmes).
fn normalise_abs(f: f64) -> i32 {
    (f.clamp(0.0, 1.0) * f64::from(ABS_PLEINE_ECHELLE)).round() as i32
}

/// Code `BTN_*` evdev d'un [`MouseButton`].
fn code_bouton(btn: MouseButton) -> u16 {
    match btn {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::X1 => BTN_SIDE,
        MouseButton::X2 => BTN_EXTRA,
    }
}

impl InputInjector for UinputInjector {
    fn mouse_move_abs(&self, x: f64, y: f64, _monitor: MonitorId) -> Result<()> {
        // Périphérique de pointage absolu unique (plage 0..=65535) étalé par le
        // compositeur sur tout le bureau virtuel. Le ciblage *par moniteur* exige
        // la géométrie des sorties (wl_output/portail), inconnue ici — `_monitor`
        // est donc inutilisé, `(x, y)` couvre l'ensemble du bureau (voir la doc
        // du module). Le verrou sérialise la séquence ABS_X/ABS_Y/SYN.
        let _garde = self.lock();
        self.emit(EV_ABS, ABS_X, normalise_abs(x))?;
        self.emit(EV_ABS, ABS_Y, normalise_abs(y))?;
        self.syn()
    }

    fn mouse_move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        let _garde = self.lock();
        self.emit(EV_REL, REL_X, dx.round() as i32)?;
        self.emit(EV_REL, REL_Y, dy.round() as i32)?;
        self.syn()
    }

    fn mouse_button(&self, btn: MouseButton, down: bool) -> Result<()> {
        let code = code_bouton(btn);
        let mut st = self.lock();
        self.emit(EV_KEY, code, i32::from(down))?;
        self.syn()?;
        if down {
            st.buttons.insert(code);
        } else {
            st.buttons.remove(&code);
        }
        Ok(())
    }

    fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        // Molette en crans (parité avec les autres backends) : REL_WHEEL vertical
        // (positif = haut), REL_HWHEEL horizontal (positif = droite).
        let v = dy.round() as i32;
        let h = dx.round() as i32;
        if v == 0 && h == 0 {
            return Ok(());
        }
        let _garde = self.lock();
        if v != 0 {
            self.emit(EV_REL, REL_WHEEL, v)?;
        }
        if h != 0 {
            self.emit(EV_REL, REL_HWHEEL, h)?;
        }
        self.syn()
    }

    fn key(&self, scancode: u32, down: bool) -> Result<()> {
        // Le paramètre est interprété comme un keycode evdev (Linux input) : émis
        // tel quel (contrairement à XTEST qui ajoute 8 pour l'espace X11).
        let code = u16::try_from(scancode)
            .ok()
            .filter(|&c| c <= KEY_MAX)
            .ok_or_else(|| {
                NdError::Input(format!("uinput : keycode evdev hors plage : {scancode}"))
            })?;
        let mut st = self.lock();
        self.emit(EV_KEY, code, i32::from(down))?;
        self.syn()?;
        if down {
            st.keys.insert(code);
        } else {
            st.keys.remove(&code);
        }
        Ok(())
    }

    fn unicode(&self, _ch: char) -> Result<()> {
        // uinput opère au niveau *keycode* evdev, sans notion de caractère : pas
        // d'équivalent direct à KEYEVENTF_UNICODE sans disposition clavier. La voie
        // propre est le portail RemoteDesktop + libei (voir la doc du module).
        Err(NdError::Input(
            "uinput : saisie Unicode non prise en charge au niveau evdev \
             (voir portail RemoteDesktop + libei)"
                .into(),
        ))
    }

    fn release_all(&self) {
        // Récupère l'état sous verrou puis relâche hors section critique d'état,
        // en best-effort (on tente tous les « up » sans s'arrêter au premier échec).
        let (keys, buttons) = {
            let mut st = self.lock();
            let keys: Vec<u16> = st.keys.iter().copied().collect();
            let buttons: Vec<u16> = st.buttons.iter().copied().collect();
            st.keys.clear();
            st.buttons.clear();
            (keys, buttons)
        };
        for code in keys {
            let _ = self.emit(EV_KEY, code, 0);
        }
        for code in buttons {
            let _ = self.emit(EV_KEY, code, 0);
        }
        let _ = self.syn();
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        // Détruit le périphérique virtuel puis ferme le descripteur (best-effort ;
        // un échec en fin de vie est sans conséquence).
        // SAFETY : `self.fd` est valide jusqu'à ce `close` ; les deux appels FFI
        // sont sans effet de bord au-delà du périphérique/descripteur.
        unsafe {
            let _ = self.ui_ioctl(UI_DEV_DESTROY, 0);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les numéros d'ioctl encodés correspondent aux valeurs connues du noyau
    /// (asm-generic, x86_64/aarch64). Verrou anti-régression de l'arithmétique
    /// `_IOC`.
    #[test]
    fn constantes_ioctl_connues() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
    }

    /// La quantification absolue couvre exactement `[0, 65535]` et sature hors bornes.
    #[test]
    fn normalise_abs_bornes_et_saturation() {
        assert_eq!(normalise_abs(0.0), 0);
        assert_eq!(normalise_abs(1.0), 65_535);
        assert_eq!(normalise_abs(-1.0), 0);
        assert_eq!(normalise_abs(2.0), 65_535);
        let centre = normalise_abs(0.5);
        assert!((32_760..=32_775).contains(&centre), "centre : {centre}");
    }

    /// Les codes de boutons sont distincts et alignés sur la convention evdev.
    #[test]
    fn codes_boutons_distincts() {
        let codes = [
            code_bouton(MouseButton::Left),
            code_bouton(MouseButton::Right),
            code_bouton(MouseButton::Middle),
            code_bouton(MouseButton::X1),
            code_bouton(MouseButton::X2),
        ];
        assert_eq!(codes[0], 0x110);
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// Sans `/dev/uinput` accessible (CI sans droits), la création échoue
    /// proprement — jamais de panique.
    #[test]
    fn creation_sans_panique() {
        match UinputInjector::new() {
            Ok(_injecteur) => { /* périphérique créé (droits présents) */ }
            Err(e) => {
                let _ = e.to_string();
            }
        }
    }
}
