//! Politique **SAS logiciel** (Ctrl+Alt+Suppr) : autorise `SendSAS` depuis les
//! services via la valeur de registre `SoftwareSASGeneration`.
//!
//! `SendSAS` (voir `nd_input::send_secure_attention_sequence`, câblé côté hôte par
//! `HostAction::SendCtrlAltDel`) n'est honoré par le système que si la stratégie
//! « Désactiver ou activer la génération logicielle du SAS » l'autorise. On écrit
//! donc, **à l'installation** (droits administrateur requis, écriture `HKLM`) :
//!
//! ```text
//! HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System
//!     SoftwareSASGeneration = 3   (REG_DWORD)
//! ```
//!
//! Valeurs : `0` = aucune, `1` = services, `2` = applications de bureau (« ease of
//! access »), `3` = **services et applications de bureau**. On retient `3` pour
//! couvrir à la fois le service SYSTEM et un éventuel assistant de bureau.
//!
//! **Honnêteté** : activer la politique **autorise** `SendSAS`, mais un vrai envoi
//! du SAS **vers le bureau sécurisé** (là où s'affiche Ctrl+Alt+Suppr) exige que
//! l'appelant tourne en **service SYSTEM** ; depuis une simple session utilisateur,
//! l'appel reste ignoré silencieusement par l'OS. C'est précisément le rôle de ce
//! service (LocalSystem, session 0).
#![allow(unsafe_code)]

use std::iter;
use std::ptr;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
};

/// Sous-clé (relative à `HKEY_LOCAL_MACHINE`) de la stratégie système.
const SOUS_CLE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
/// Valeur de la stratégie de génération logicielle du SAS.
const VALEUR: &str = "SoftwareSASGeneration";
/// « Services et applications de bureau » : autorise `SendSAS` depuis un service.
const SERVICES_ET_APPS: u32 = 3;

/// Active la génération logicielle du SAS (écrit `SoftwareSASGeneration = 3`).
///
/// # Errors
/// Erreur si l'ouverture ou l'écriture de la clé `HKLM` échoue (droits
/// administrateur requis).
pub fn activer_generation_sas() -> Result<(), String> {
    ecrire_dword(SOUS_CLE, VALEUR, SERVICES_ET_APPS)
}

/// Encode une chaîne en tampon UTF-16 terminé par un zéro (API `*W`).
fn utf16z(texte: &str) -> Vec<u16> {
    texte.encode_utf16().chain(iter::once(0)).collect()
}

/// Ouvre (en création) une sous-clé de `HKEY_LOCAL_MACHINE` avec les droits `sam`.
fn ouvrir_cle(sous_cle: &str, sam: REG_SAM_FLAGS) -> Result<HKEY, String> {
    let sous_cle_w = utf16z(sous_cle);
    let mut cle = HKEY(ptr::null_mut());
    // SAFETY : `sous_cle_w` vit jusqu'à la fin de l'appel ; `cle` reçoit la poignée.
    let statut = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(sous_cle_w.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            sam,
            None,
            &mut cle,
            None,
        )
    };
    if statut != ERROR_SUCCESS {
        return Err(format!(
            "ouverture de la clé « HKLM\\{sous_cle} » impossible (code {}) — \
             droits administrateur requis",
            statut.0
        ));
    }
    Ok(cle)
}

/// Écrit une valeur `REG_DWORD` sous `HKLM\<sous_cle>`.
fn ecrire_dword(sous_cle: &str, nom: &str, valeur: u32) -> Result<(), String> {
    let cle = ouvrir_cle(sous_cle, KEY_WRITE)?;
    let nom_w = utf16z(nom);
    let donnees = valeur.to_le_bytes();
    // SAFETY : `cle` valide ; `nom_w` et `donnees` vivent jusqu'à la fin de l'appel.
    let statut = unsafe {
        RegSetValueExW(
            cle,
            PCWSTR::from_raw(nom_w.as_ptr()),
            0,
            REG_DWORD,
            Some(&donnees),
        )
    };
    // SAFETY : `cle` provient de `RegCreateKeyExW`.
    unsafe {
        let _ = RegCloseKey(cle);
    }
    if statut != ERROR_SUCCESS {
        return Err(format!(
            "écriture de « {nom} » sous « HKLM\\{sous_cle} » impossible (code {})",
            statut.0
        ));
    }
    Ok(())
}
