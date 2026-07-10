//! **Bascule sur le bureau d'entrée** (dont le *bureau sécurisé* Winlogon).
//!
//! Une station fenêtrée (`WinSta0`) porte plusieurs **bureaux** : `Default` (le
//! bureau interactif habituel), `Winlogon` (l'écran d'ouverture de session, l'UAC
//! et Ctrl+Alt+Suppr), `Screen-saver`… À un instant donné, **un seul** reçoit les
//! entrées : le *bureau d'entrée*. Quand une invite UAC surgit ou que la session
//! se verrouille, l'entrée bascule vers `Winlogon`.
//!
//! La capture (DXGI Desktop Duplication) et l'injection (`SendInput`) portent sur
//! le bureau **auquel le thread appelant est associé**. Pour continuer à filmer et
//! piloter pendant une invite UAC, l'assistant doit donc, sur son thread de
//! capture :
//!
//! 1. [`ouvrir_bureau_entree`] — `OpenInputDesktop` : poignée du bureau qui reçoit
//!    l'entrée **maintenant** (`Winlogon` pendant l'UAC) ;
//! 2. [`BureauEntree::associer_thread`] — `SetThreadDesktop` : lie le thread à ce
//!    bureau **avant** de (re)créer le capteur.
//!
//! # Exigence de privilèges (honnêteté)
//!
//! `OpenInputDesktop` sur `Winlogon` **échoue avec « accès refusé »** pour un
//! processus lancé sous le jeton de l'utilisateur : seul **SYSTEM** a accès au
//! bureau sécurisé. Concrètement, pour couvrir l'UAC/le verrouillage, le service
//! doit lancer l'assistant **en SYSTEM dans la session active** (jeton SYSTEM du
//! service dupliqué + `TokenSessionId` fixé à la console active), et **non** sous
//! le jeton utilisateur de [`crate::session0::lancer_dans_session_active`]. Sur le
//! bureau `Default` (cas courant), la bascule fonctionne aussi sous l'utilisateur.
//! Voir le module [`crate::pont`] pour le choix du jeton de lancement.
//!
//! `SetThreadDesktop` **échoue** si le thread possède déjà des fenêtres ou des
//! hooks : on l'appelle sur le thread de capture **avant** toute création de
//! ressource graphique, et l'on recrée le capteur après chaque bascule.
#![allow(unsafe_code)]

use windows::Win32::Foundation::{FALSE, HANDLE};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW,
    DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD,
    DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, HDESK, UOI_NAME,
};

/// Accès demandé sur le bureau d'entrée : de quoi créer fenêtres/hooks, lire et
/// écrire des objets (capture + injection), et suivre les bascules.
///
/// `DESKTOP_ACCESS_FLAGS` n'implémente pas `BitOr` : on combine les bits bruts.
fn acces_bureau() -> DESKTOP_ACCESS_FLAGS {
    DESKTOP_ACCESS_FLAGS(
        DESKTOP_CREATEWINDOW.0
            | DESKTOP_CREATEMENU.0
            | DESKTOP_HOOKCONTROL.0
            | DESKTOP_JOURNALRECORD.0
            | DESKTOP_JOURNALPLAYBACK.0
            | DESKTOP_READOBJECTS.0
            | DESKTOP_WRITEOBJECTS.0
            | DESKTOP_ENUMERATE.0
            | DESKTOP_SWITCHDESKTOP.0,
    )
}

/// Poignée possédée d'un **bureau d'entrée**, refermée à la libération (RAII).
pub struct BureauEntree {
    hdesk: HDESK,
    /// Nom du bureau (`Default`, `Winlogon`, `Screen-saver`…) : sert à **détecter
    /// une bascule** en comparant au bureau précédemment associé.
    pub nom: String,
}

impl Drop for BureauEntree {
    fn drop(&mut self) {
        // SAFETY : poignée valide obtenue d'`OpenInputDesktop`, fermée une fois.
        unsafe {
            let _ = CloseDesktop(self.hdesk);
        }
    }
}

impl BureauEntree {
    /// Associe le **thread appelant** à ce bureau (`SetThreadDesktop`). À appeler
    /// sur le thread de capture, **avant** de (re)créer le capteur/injecteur.
    ///
    /// # Errors
    /// Échoue si le thread possède déjà des fenêtres/hooks, ou si l'accès manque.
    pub fn associer_thread(&self) -> Result<(), String> {
        // SAFETY : `hdesk` est une poignée de bureau valide.
        unsafe { SetThreadDesktop(self.hdesk) }
            .map_err(|e| format!("SetThreadDesktop({}) impossible : {e}", self.nom))
    }
}

/// Ouvre le **bureau qui reçoit l'entrée maintenant** (`Winlogon` pendant l'UAC ou
/// le verrouillage, `Default` sinon) et lit son nom.
///
/// # Errors
/// Échoue si `OpenInputDesktop` est refusé — typiquement `Winlogon` sous un jeton
/// utilisateur (accès réservé à SYSTEM, voir la doc du module).
pub fn ouvrir_bureau_entree() -> Result<BureauEntree, String> {
    // SAFETY : appel FFI ; `finherit = FALSE` (poignée non héritable) ; renvoie une
    // poignée valide ou une erreur.
    let hdesk = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), FALSE, acces_bureau()) }
        .map_err(|e| {
            format!("OpenInputDesktop impossible (bureau sécurisé sans SYSTEM ?) : {e}")
        })?;
    let nom = nom_bureau(hdesk).unwrap_or_else(|| "?".to_owned());
    Ok(BureauEntree { hdesk, nom })
}

/// Lit le nom d'un objet bureau via `GetUserObjectInformationW(UOI_NAME)`.
fn nom_bureau(hdesk: HDESK) -> Option<String> {
    let handle = HANDLE(hdesk.0);
    let mut requis: u32 = 0;
    // Premier appel : dimensionne le tampon (échoue avec la taille requise).
    // SAFETY : `pvinfo = None` (mesure), `requis` reçoit la taille en octets.
    unsafe {
        let _ = GetUserObjectInformationW(handle, UOI_NAME, None, 0, Some(&mut requis));
    }
    if requis == 0 {
        return None;
    }
    let nb_u16 = (requis as usize).div_ceil(2).max(1);
    let mut tampon = vec![0u16; nb_u16];
    // SAFETY : `tampon` fait `requis` octets ; `pvinfo` pointe son début.
    let ok = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(tampon.as_mut_ptr().cast()),
            requis,
            None,
        )
    }
    .is_ok();
    if !ok {
        return None;
    }
    let fin = tampon.iter().position(|&c| c == 0).unwrap_or(tampon.len());
    Some(String::from_utf16_lossy(&tampon[..fin]))
}
