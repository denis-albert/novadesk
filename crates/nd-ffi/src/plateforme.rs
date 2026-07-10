//! Intégration plateforme **Windows sans privilèges administrateur** : chiffrement
//! des secrets au repos (DPAPI) et démarrage automatique avec la session
//! (clé de registre `Run` de l'utilisateur courant).
//!
//! Les deux mécanismes n'utilisent que des API à **portée utilisateur** — aucun
//! droit administrateur requis :
//!
//! * [`proteger`] / [`deproteger`] enveloppent `CryptProtectData` /
//!   `CryptUnprotectData` (DPAPI, portée utilisateur courant, sans interface).
//!   [`crate::etat`] s'en sert pour chiffrer au repos **la clé privée d'identité**
//!   (`identite.cle`) et **le haché du mot de passe d'accès non surveillé**.
//! * [`appliquer_demarrage_auto`] ajoute ou retire la valeur `NovaDesk` de
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (chemin de l'exécutable
//!   courant) — le « démarrer avec le système » réellement effectif.
//!
//! # Repli hors Windows (`#[cfg]`)
//!
//! Sur les autres plateformes, [`proteger`] / [`deproteger`] sont l'**identité**
//! (les octets sont stockés **en clair, comme aujourd'hui** — le coffre-fort de
//! l'OS, Keychain/Secret Service, viendra plus tard) et [`appliquer_demarrage_auto`]
//! est **sans effet** (`Ok(())` : le réglage reste persisté mais inopérant). Ce
//! repli est documenté et volontaire — NovaDesk cible d'abord Windows.

// ---------------------------------------------------------------------------
// Implémentation Windows : DPAPI + clé de registre `Run`
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    // Bloc plateforme : FFI Win32 non sûre, cantonnée à ce module et documentée
    // ligne à ligne (`// SAFETY:`), conformément à la barre de qualité du workspace.
    #![allow(unsafe_code)]

    use std::iter;
    use std::ptr;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ,
    };
    // Lecture/suppression réservées aux tests d'auto-démarrage (voir plus bas).
    #[cfg(test)]
    use windows::Win32::System::Registry::{
        RegDeleteKeyW, RegOpenKeyExW, RegQueryValueExW, KEY_READ,
    };

    /// Sous-clé (relative à `HKEY_CURRENT_USER`) du démarrage automatique utilisateur.
    const SOUS_CLE_RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// Nom de la valeur d'auto-démarrage de NovaDesk dans la clé `Run`.
    const VALEUR_DEMARRAGE: &str = "NovaDesk";

    /// Encode une chaîne en tampon UTF-16 terminé par un zéro (API `*W`).
    fn utf16z(texte: &str) -> Vec<u16> {
        texte.encode_utf16().chain(iter::once(0)).collect()
    }

    // ----- DPAPI (secrets au repos) ---------------------------------------

    /// Chiffre `clair` au repos via DPAPI (portée utilisateur courant).
    pub(super) fn proteger(clair: &[u8]) -> Result<Vec<u8>, String> {
        let taille = u32::try_from(clair.len())
            .map_err(|_| "secret trop volumineux pour DPAPI".to_owned())?;
        // DPAPI ne modifie pas l'entrée ; le `*mut` de la signature n'est pas honoré
        // en écriture (cast d'un pointeur constant, jamais déréférencé en écriture).
        let entree = CRYPT_INTEGER_BLOB {
            cbData: taille,
            pbData: clair.as_ptr() as *mut u8,
        };
        let mut sortie = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY : `entree` pointe sur `clair`, valide pour toute la durée de l'appel.
        // DPAPI alloue `sortie.pbData` (LocalAlloc) ; il est copié puis libéré ci-dessous.
        unsafe {
            CryptProtectData(
                &entree,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut sortie,
            )
            .map_err(|e| format!("chiffrement DPAPI impossible : {e}"))?;
        }
        let resultat = copie_blob(&sortie);
        liberer_blob(&mut sortie);
        Ok(resultat)
    }

    /// Déchiffre un blob DPAPI produit par [`proteger`].
    pub(super) fn deproteger(chiffre: &[u8]) -> Result<Vec<u8>, String> {
        let taille = u32::try_from(chiffre.len())
            .map_err(|_| "secret chiffré trop volumineux pour DPAPI".to_owned())?;
        let entree = CRYPT_INTEGER_BLOB {
            cbData: taille,
            pbData: chiffre.as_ptr() as *mut u8,
        };
        let mut sortie = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY : idem [`proteger`] — `entree` valide le temps de l'appel, `sortie`
        // alloué par DPAPI, copié puis libéré.
        unsafe {
            CryptUnprotectData(
                &entree,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut sortie,
            )
            .map_err(|e| format!("déchiffrement DPAPI impossible : {e}"))?;
        }
        let resultat = copie_blob(&sortie);
        liberer_blob(&mut sortie);
        Ok(resultat)
    }

    /// Copie le contenu d'un blob DPAPI dans un `Vec` possédé.
    fn copie_blob(blob: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if blob.pbData.is_null() || blob.cbData == 0 {
            return Vec::new();
        }
        // SAFETY : DPAPI garantit `pbData` valide sur `cbData` octets.
        unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec() }
    }

    /// Libère le tampon alloué par DPAPI (LocalAlloc) puis neutralise le pointeur.
    fn liberer_blob(blob: &mut CRYPT_INTEGER_BLOB) {
        if !blob.pbData.is_null() {
            // SAFETY : `pbData` provient de LocalAlloc (DPAPI) ; libéré une seule fois.
            unsafe {
                let _ = LocalFree(HLOCAL(blob.pbData.cast()));
            }
            blob.pbData = ptr::null_mut();
        }
    }

    // ----- Démarrage automatique (clé `Run`) ------------------------------

    /// Ajoute (si `actif`) ou retire la valeur `NovaDesk` de la clé `Run` de
    /// l'utilisateur, pointant sur l'exécutable courant.
    pub(super) fn appliquer_demarrage_auto(actif: bool) -> Result<(), String> {
        if actif {
            let exe = std::env::current_exe()
                .map_err(|e| format!("chemin de l'exécutable introuvable : {e}"))?;
            // Guillemets : le chemin peut contenir des espaces (Program Files…).
            let valeur = format!("\"{}\"", exe.display());
            ecrire_valeur(SOUS_CLE_RUN, VALEUR_DEMARRAGE, &valeur)
        } else {
            supprimer_valeur(SOUS_CLE_RUN, VALEUR_DEMARRAGE)
        }
    }

    /// Ouvre (en création) une sous-clé de `HKEY_CURRENT_USER` avec les droits `sam`.
    fn ouvrir_cle(sous_cle: &str, sam: REG_SAM_FLAGS) -> Result<HKEY, String> {
        let sous_cle_w = utf16z(sous_cle);
        let mut cle = HKEY(ptr::null_mut());
        // SAFETY : `sous_cle_w` vit jusqu'à la fin de l'appel ; `cle` reçoit la poignée.
        let statut = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
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
                "ouverture de la clé de registre « {sous_cle} » impossible (code {})",
                statut.0
            ));
        }
        Ok(cle)
    }

    /// Écrit une valeur chaîne (`REG_SZ`) sous `HKCU\<sous_cle>`.
    pub(super) fn ecrire_valeur(sous_cle: &str, nom: &str, valeur: &str) -> Result<(), String> {
        let cle = ouvrir_cle(sous_cle, KEY_WRITE)?;
        let nom_w = utf16z(nom);
        // `REG_SZ` attend les octets UTF-16LE, terminateur nul inclus.
        let donnees = octets_utf16(&utf16z(valeur));
        // SAFETY : `cle` valide ; `nom_w` et `donnees` vivent jusqu'à la fin de l'appel.
        let statut = unsafe {
            RegSetValueExW(
                cle,
                PCWSTR::from_raw(nom_w.as_ptr()),
                0,
                REG_SZ,
                Some(&donnees),
            )
        };
        fermer_cle(cle);
        if statut != ERROR_SUCCESS {
            return Err(format!(
                "écriture de la valeur « {nom} » impossible (code {})",
                statut.0
            ));
        }
        Ok(())
    }

    /// Supprime une valeur ; l'absence de valeur est un **succès idempotent**.
    pub(super) fn supprimer_valeur(sous_cle: &str, nom: &str) -> Result<(), String> {
        let cle = ouvrir_cle(sous_cle, KEY_WRITE)?;
        let nom_w = utf16z(nom);
        // SAFETY : `cle` valide ; `nom_w` vit jusqu'à la fin de l'appel.
        let statut = unsafe { RegDeleteValueW(cle, PCWSTR::from_raw(nom_w.as_ptr())) };
        fermer_cle(cle);
        if statut != ERROR_SUCCESS && statut != ERROR_FILE_NOT_FOUND {
            return Err(format!(
                "suppression de la valeur « {nom} » impossible (code {})",
                statut.0
            ));
        }
        Ok(())
    }

    /// Lit une valeur chaîne (`REG_SZ`) ; `None` si la clé ou la valeur est absente.
    /// Utilisée par les tests pour vérifier l'écriture/suppression de l'auto-démarrage.
    #[cfg(test)]
    pub(super) fn lire_valeur(sous_cle: &str, nom: &str) -> Result<Option<String>, String> {
        let sous_cle_w = utf16z(sous_cle);
        let mut cle = HKEY(ptr::null_mut());
        // SAFETY : `sous_cle_w` vit jusqu'à la fin de l'appel ; `cle` reçoit la poignée.
        let statut = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(sous_cle_w.as_ptr()),
                0,
                KEY_READ,
                &mut cle,
            )
        };
        if statut == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if statut != ERROR_SUCCESS {
            return Err(format!(
                "ouverture de la clé de registre « {sous_cle} » impossible (code {})",
                statut.0
            ));
        }
        let nom_w = utf16z(nom);
        let mut taille: u32 = 0;
        // 1er appel : dimensionnement (lpData nul, lpcbData reçoit la taille).
        // SAFETY : `cle` et `nom_w` valides ; `taille` reçoit le nombre d'octets.
        let statut = unsafe {
            RegQueryValueExW(
                cle,
                PCWSTR::from_raw(nom_w.as_ptr()),
                None,
                None,
                None,
                Some(&mut taille),
            )
        };
        if statut == ERROR_FILE_NOT_FOUND {
            fermer_cle(cle);
            return Ok(None);
        }
        if statut != ERROR_SUCCESS {
            fermer_cle(cle);
            return Err(format!(
                "lecture de la taille de « {nom} » impossible (code {})",
                statut.0
            ));
        }
        let mut tampon = vec![0u8; taille as usize];
        let mut taille_lue = taille;
        // 2e appel : lecture effective dans `tampon`.
        // SAFETY : `tampon` fait `taille` octets ; `taille_lue` borne l'écriture.
        let statut = unsafe {
            RegQueryValueExW(
                cle,
                PCWSTR::from_raw(nom_w.as_ptr()),
                None,
                None,
                Some(tampon.as_mut_ptr()),
                Some(&mut taille_lue),
            )
        };
        fermer_cle(cle);
        if statut != ERROR_SUCCESS {
            return Err(format!(
                "lecture de la valeur « {nom} » impossible (code {})",
                statut.0
            ));
        }
        tampon.truncate(taille_lue as usize);
        Ok(Some(decoder_reg_sz(&tampon)))
    }

    /// Supprime une sous-clé **vide** de `HKEY_CURRENT_USER` (nettoyage des tests).
    #[cfg(test)]
    pub(super) fn supprimer_cle(sous_cle: &str) -> Result<(), String> {
        let sous_cle_w = utf16z(sous_cle);
        // SAFETY : `sous_cle_w` vit jusqu'à la fin de l'appel.
        let statut =
            unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR::from_raw(sous_cle_w.as_ptr())) };
        if statut != ERROR_SUCCESS && statut != ERROR_FILE_NOT_FOUND {
            return Err(format!(
                "suppression de la clé « {sous_cle} » impossible (code {})",
                statut.0
            ));
        }
        Ok(())
    }

    /// Ferme une poignée de clé (les échecs de fermeture sont sans conséquence ici).
    fn fermer_cle(cle: HKEY) {
        // SAFETY : `cle` a été ouverte par `RegCreateKeyExW`/`RegOpenKeyExW`.
        unsafe {
            let _ = RegCloseKey(cle);
        }
    }

    /// Aplati un tampon UTF-16 en octets little-endian (format `REG_SZ`).
    fn octets_utf16(mots: &[u16]) -> Vec<u8> {
        mots.iter().flat_map(|m| m.to_le_bytes()).collect()
    }

    /// Décode un `REG_SZ` (octets UTF-16LE, terminateur nul éventuel) en `String`.
    #[cfg(test)]
    fn decoder_reg_sz(octets: &[u8]) -> String {
        let mots: Vec<u16> = octets
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        // S'arrête au premier terminateur nul.
        let utile = mots.split(|&m| m == 0).next().unwrap_or(&mots);
        String::from_utf16_lossy(utile)
    }
}

// ---------------------------------------------------------------------------
// Repli hors Windows : identité (clair) + auto-démarrage inerte
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    /// Repli documenté : renvoie les octets **inchangés** (stockage en clair,
    /// comme historiquement — pas de coffre-fort OS ici).
    pub(super) fn proteger(clair: &[u8]) -> Result<Vec<u8>, String> {
        Ok(clair.to_vec())
    }

    /// Repli documenté : identité (les octets « chiffrés » sont en fait le clair).
    pub(super) fn deproteger(chiffre: &[u8]) -> Result<Vec<u8>, String> {
        Ok(chiffre.to_vec())
    }

    /// Repli documenté : sans effet (le réglage reste persisté mais inopérant).
    pub(super) fn appliquer_demarrage_auto(_actif: bool) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Surface interne (indépendante de la plateforme)
// ---------------------------------------------------------------------------

/// Chiffre `clair` au repos (DPAPI sous Windows ; **clair** sinon, cf. module).
pub(crate) fn proteger(clair: &[u8]) -> Result<Vec<u8>, String> {
    imp::proteger(clair)
}

/// Déchiffre un blob produit par [`proteger`] (DPAPI sous Windows ; **clair** sinon).
pub(crate) fn deproteger(chiffre: &[u8]) -> Result<Vec<u8>, String> {
    imp::deproteger(chiffre)
}

/// Applique le réglage « démarrer avec le système » **sans droits administrateur** :
/// ajoute (si `actif`) ou retire la valeur `NovaDesk` de
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointant sur l'exécutable
/// courant. Hors Windows : sans effet (`Ok(())`, cf. module).
///
/// Appelée par la façade quand le réglage `demarrer_avec_systeme` change (voir
/// [`crate::api::set_setting`]) et exposée à l'UI via [`crate::api::apply_autostart`].
pub(crate) fn appliquer_demarrage_auto(actif: bool) -> Result<(), String> {
    imp::appliquer_demarrage_auto(actif)
}

// ---------------------------------------------------------------------------
// Tests : round-trip DPAPI + logique d'auto-démarrage (sous-clé de test isolée)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip_protege_puis_dechiffre() {
        let secret = b"cle-privee-identite + hache mot de passe (donnees sensibles)";
        let protege = proteger(secret).expect("protection");
        // Sous Windows, le blob DPAPI diffère du clair ; hors Windows c'est l'identité.
        #[cfg(windows)]
        assert_ne!(protege.as_slice(), &secret[..], "DPAPI doit chiffrer");
        let dechiffre = deproteger(&protege).expect("déchiffrement");
        assert_eq!(dechiffre.as_slice(), &secret[..], "round-trip DPAPI");
    }

    #[test]
    fn dpapi_round_trip_vide() {
        let protege = proteger(b"").expect("protection du vide");
        assert!(deproteger(&protege)
            .expect("déchiffrement du vide")
            .is_empty());
    }

    /// Un blob arbitraire n'est pas un secret DPAPI valide : le déchiffrement échoue
    /// (garanti par DPAPI ; hors Windows le repli identité l'accepterait, d'où le `cfg`).
    #[cfg(windows)]
    #[test]
    fn dpapi_rejette_un_blob_invalide() {
        assert!(deproteger(b"ceci n'est pas un blob DPAPI valide").is_err());
    }

    /// Écriture / lecture / suppression d'une valeur `Run`, sous une **sous-clé de
    /// test isolée** (jamais la vraie clé `Run`), nettoyée en fin de test.
    #[cfg(windows)]
    #[test]
    fn demarrage_auto_ecrire_lire_supprimer() {
        let sous_cle = format!(
            r"Software\NovaDesk\test-autostart-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let valeur = r#""C:\Program Files\NovaDesk\novadesk.exe""#;

        imp::ecrire_valeur(&sous_cle, "NovaDesk", valeur).expect("écriture");
        assert_eq!(
            imp::lire_valeur(&sous_cle, "NovaDesk").expect("lecture"),
            Some(valeur.to_owned())
        );

        imp::supprimer_valeur(&sous_cle, "NovaDesk").expect("suppression");
        assert_eq!(
            imp::lire_valeur(&sous_cle, "NovaDesk").expect("relecture"),
            None,
            "la valeur doit avoir disparu"
        );
        // Re-suppression : idempotente (valeur déjà absente).
        imp::supprimer_valeur(&sous_cle, "NovaDesk").expect("suppression idempotente");

        // Nettoyage : retire la sous-clé de test (désormais vide) et son parent créé.
        let _ = imp::supprimer_cle(&sous_cle);
        let _ = imp::supprimer_cle(r"Software\NovaDesk");
    }
}
