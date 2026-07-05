//! Implémentation Windows de [`Clipboard`](crate::Clipboard) via l'API Win32
//! (`OpenClipboard`/`GetClipboardData`/`SetClipboardData`, format `CF_UNICODETEXT`).
//!
//! Ce module concentre tout le `unsafe` FFI du presse-papiers Windows ; il est
//! isolé derrière le trait pour que le reste du crate reste 100 % sûr.
#![allow(unsafe_code)]

use std::time::Duration;

use nd_proto::{NdError, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::Clipboard;

/// Format presse-papiers « texte UTF-16 terminé par NUL » (valeur Win32 `CF_UNICODETEXT`).
/// Déclarée localement pour ne pas activer la feature `Win32_System_Ole` qui l'héberge.
const CF_UNICODETEXT: u32 = 13;

/// Convertit une erreur `windows` en [`NdError::Io`] contextualisée
/// (pas de variante `NdError` dédiée au presse-papiers).
fn clip_err(ctx: &str, e: windows::core::Error) -> NdError {
    NdError::Io(std::io::Error::other(format!(
        "presse-papiers : {ctx} : {e}"
    )))
}

/// Garde RAII : presse-papiers ouvert à la construction, refermé au `drop`
/// quel que soit le chemin de sortie (succès, erreur, panique).
struct OpenedClipboard;

impl OpenedClipboard {
    /// Ouvre le presse-papiers, avec quelques tentatives espacées : il peut
    /// être tenu brièvement par un autre processus (c'est un verrou global).
    fn open() -> Result<Self> {
        const ESSAIS: u32 = 5;
        let mut derniere = None;
        for essai in 0..ESSAIS {
            if essai > 0 {
                std::thread::sleep(Duration::from_millis(15));
            }
            // SAFETY : appel FFI simple ; HWND nul = presse-papiers associé au thread courant.
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(e) => derniere = Some(e),
            }
        }
        Err(clip_err(
            "OpenClipboard",
            derniere.expect("au moins une tentative a été faite"),
        ))
    }
}

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        // SAFETY : le presse-papiers a été ouvert par `open` (invariant du type).
        let _ = unsafe { CloseClipboard() };
    }
}

/// Presse-papiers Windows (texte Unicode uniquement à ce stade ; les autres
/// formats — images, listes de fichiers — viendront avec le plan 09 complet).
#[derive(Debug, Default)]
pub struct WindowsClipboard;

impl WindowsClipboard {
    /// Crée un accès au presse-papiers Windows.
    pub fn new() -> Self {
        Self
    }
}

impl Clipboard for WindowsClipboard {
    fn get_text(&self) -> Result<Option<String>> {
        let _ouvert = OpenedClipboard::open()?;

        // SAFETY : simple interrogation de disponibilité d'un format.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
            // Pas de texte dans le presse-papiers (vide ou autre format).
            return Ok(None);
        }

        // SAFETY : presse-papiers ouvert par le guard ; le handle renvoyé
        // appartient au système et reste valide tant qu'il est ouvert.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }
            .map_err(|e| clip_err("GetClipboardData", e))?;
        let hmem = HGLOBAL(handle.0);

        // SAFETY : `hmem` est un HGLOBAL valide fourni par `GetClipboardData`.
        let ptr = unsafe { GlobalLock(hmem) }.cast::<u16>();
        if ptr.is_null() {
            return Err(clip_err("GlobalLock", windows::core::Error::from_win32()));
        }

        // SAFETY : bloc verrouillé de `GlobalSize` octets ; CF_UNICODETEXT
        // garantit une chaîne UTF-16 terminée par NUL dans le bloc. La recherche
        // du NUL est bornée par la taille du bloc, par défense en profondeur.
        let texte = unsafe {
            let max_u16 = GlobalSize(hmem) / std::mem::size_of::<u16>();
            let mut len = 0usize;
            while len < max_u16 && *ptr.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
        };

        // SAFETY : `hmem` a été verrouillé avec succès ci-dessus.
        let _ = unsafe { GlobalUnlock(hmem) };
        Ok(Some(texte))
    }

    fn set_text(&self, text: &str) -> Result<()> {
        // CF_UNICODETEXT exige de l'UTF-16 terminé par NUL.
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let octets = utf16.len() * std::mem::size_of::<u16>();

        let _ouvert = OpenedClipboard::open()?;

        // SAFETY : presse-papiers ouvert par le guard ci-dessus.
        unsafe { EmptyClipboard() }.map_err(|e| clip_err("EmptyClipboard", e))?;

        // SAFETY : allocation d'un bloc global déplaçable de `octets` octets,
        // comme l'exige `SetClipboardData`.
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, octets) }
            .map_err(|e| clip_err("GlobalAlloc", e))?;

        // SAFETY : `hmem` vient d'être alloué ; le verrou rend un pointeur
        // vers au moins `octets` octets accessibles en écriture.
        let ptr = unsafe { GlobalLock(hmem) }.cast::<u16>();
        if ptr.is_null() {
            // SAFETY : le bloc n'a pas été transmis au système : à nous de le libérer.
            let _ = unsafe { GlobalFree(hmem) };
            return Err(clip_err("GlobalLock", windows::core::Error::from_win32()));
        }
        // SAFETY : source et destination font `utf16.len()` u16 et ne se recouvrent pas.
        unsafe { std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len()) };
        // SAFETY : `hmem` a été verrouillé avec succès ci-dessus.
        let _ = unsafe { GlobalUnlock(hmem) };

        // SAFETY : bloc valide et déverrouillé ; en cas de succès, le système
        // devient propriétaire du bloc (il ne faut alors PAS le libérer).
        match unsafe { SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0)) } {
            Ok(_) => Ok(()),
            Err(e) => {
                // SAFETY : le système a refusé le bloc : il nous appartient encore.
                let _ = unsafe { GlobalFree(hmem) };
                Err(clip_err("SetClipboardData", e))
            }
        }
    }
}
