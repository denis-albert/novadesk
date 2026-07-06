//! Persistance de l'identité statique d'un appareil (plan 06 — accès non surveillé).
//!
//! Chaque machine possède une paire de clés statiques X25519 stable : c'est son
//! identité cryptographique pour l'accès non surveillé et l'épinglage TOFU (voir
//! [`crate::pinning`]). Ce module la charge depuis le disque ou la crée au premier
//! lancement.
//!
//! Format du fichier (texte, trois lignes) :
//!
//! ```text
//! novadesk-identite v1
//! <clé privée, 64 caractères hexadécimaux>
//! <clé publique, 64 caractères hexadécimaux>
//! ```
//!
//! NOTE (plan 06 §gestion des clés) : le stockage sécurisé par l'OS — DPAPI sous
//! Windows, Keychain sous macOS, Secret Service sous Linux — viendra plus tard.
//! En attendant, la clé privée est écrite en clair sur disque : le fichier doit
//! être placé dans le profil de l'utilisateur et protégé par les permissions du
//! système de fichiers.

use std::fs;
use std::path::Path;

use nd_proto::{NdError, Result};

use crate::{derive_public_key, generate_static_keypair, StaticKeypair};

/// En-tête du fichier d'identité, versionné pour permettre une migration future
/// (p. ex. passage au stockage OS sécurisé ou chiffrement au repos).
const ENTETE_IDENTITE: &str = "novadesk-identite v1";

/// Taille attendue (en octets) d'une clé X25519, privée comme publique.
const CLE_LEN: usize = 32;

/// Encode des octets en hexadécimal minuscule (implémentation std, sans dépendance).
pub(crate) fn encode_hex(octets: &[u8]) -> String {
    octets.iter().map(|b| format!("{b:02x}")).collect()
}

/// Décode une chaîne hexadécimale en octets ; refuse toute chaîne mal formée
/// (longueur impaire, caractère non hexadécimal, non-ASCII).
pub(crate) fn decode_hex(texte: &str) -> Result<Vec<u8>> {
    if !texte.is_ascii() || !texte.len().is_multiple_of(2) {
        return Err(NdError::Crypto("chaîne hexadécimale mal formée".into()));
    }
    (0..texte.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&texte[i..i + 2], 16)
                .map_err(|_| NdError::Crypto("chaîne hexadécimale mal formée".into()))
        })
        .collect()
}

/// Erreur type pour un fichier d'identité illisible ou altéré.
fn corrompu(detail: &str) -> NdError {
    NdError::Crypto(format!("fichier d'identité corrompu : {detail}"))
}

/// Magasin de l'identité persistante d'un appareil.
///
/// L'identité (paire de clés statiques X25519) est générée une seule fois puis
/// rechargée à chaque démarrage : les pairs distants peuvent ainsi épingler notre
/// empreinte (TOFU) et détecter une substitution de clé.
pub struct IdentityStore;

impl IdentityStore {
    /// Charge la paire de clés statique depuis `path` ; si le fichier n'existe pas
    /// encore, en génère une nouvelle via [`generate_static_keypair`] et l'enregistre.
    ///
    /// Un fichier existant mais illisible ou incohérent (en-tête inconnu, hex
    /// invalide, clé publique ne correspondant pas à la clé privée) est refusé
    /// avec [`NdError::Crypto`] : on ne régénère jamais silencieusement l'identité,
    /// car cela invaliderait l'épinglage des pairs distants.
    pub fn load_or_create(path: &Path) -> Result<StaticKeypair> {
        if path.exists() {
            Self::charge(path)
        } else {
            let paire = generate_static_keypair()?;
            Self::enregistre(path, &paire)?;
            Ok(paire)
        }
    }

    /// Charge et valide le fichier d'identité.
    fn charge(path: &Path) -> Result<StaticKeypair> {
        let octets = fs::read(path)?;
        let texte = String::from_utf8(octets).map_err(|_| corrompu("contenu non UTF-8"))?;

        // Trois lignes exactement (tolère les fins de ligne \r\n et les blancs
        // en fin de fichier, rien de plus).
        let lignes: Vec<&str> = texte.trim_end().lines().map(str::trim).collect();
        let &[entete, prive_hex, publique_hex] = lignes.as_slice() else {
            return Err(corrompu("nombre de lignes inattendu"));
        };
        if entete != ENTETE_IDENTITE {
            return Err(corrompu("en-tête inconnu"));
        }

        let prive = decode_hex(prive_hex).map_err(|_| corrompu("clé privée non hexadécimale"))?;
        let publique =
            decode_hex(publique_hex).map_err(|_| corrompu("clé publique non hexadécimale"))?;
        if prive.len() != CLE_LEN || publique.len() != CLE_LEN {
            return Err(corrompu("taille de clé inattendue"));
        }

        // Contrôle d'intégrité : la clé publique stockée doit être celle dérivée de
        // la clé privée (X25519), sinon le fichier a été altéré ou recomposé.
        if derive_public_key(&prive)? != publique {
            return Err(corrompu("clés privée et publique incohérentes"));
        }

        Ok(StaticKeypair {
            private: prive,
            public: publique,
        })
    }

    /// Écrit la paire de clés au format texte documenté en tête de module.
    fn enregistre(path: &Path, paire: &StaticKeypair) -> Result<()> {
        let contenu = format!(
            "{ENTETE_IDENTITE}\n{}\n{}\n",
            encode_hex(&paire.private),
            encode_hex(&paire.public)
        );
        fs::write(path, contenu)?;
        Ok(())
    }
}

/// Aides partagées par les tests de ce crate (fichiers temporaires).
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Fichier temporaire à nom unique, supprimé à la fin du test (même en cas de
    /// panique, via `Drop`). Repose sur `std::env::temp_dir()`.
    pub(crate) struct FichierTemp(PathBuf);

    impl FichierTemp {
        pub(crate) fn nouveau(prefixe: &str) -> Self {
            static COMPTEUR: AtomicU64 = AtomicU64::new(0);
            let unique = COMPTEUR.fetch_add(1, Ordering::Relaxed);
            let nom = format!("nd-crypto-{prefixe}-{}-{unique}", std::process::id());
            Self(std::env::temp_dir().join(nom))
        }

        pub(crate) fn chemin(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FichierTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FichierTemp;
    use super::*;

    #[test]
    fn encode_decode_hex_aller_retour() {
        let octets = [0x00, 0x0f, 0xa5, 0xff];
        let hex = encode_hex(&octets);
        assert_eq!(hex, "000fa5ff");
        assert_eq!(decode_hex(&hex).expect("décodage"), octets);
    }

    #[test]
    fn decode_hex_refuse_les_chaines_mal_formees() {
        // Longueur impaire, caractère non hexadécimal, non-ASCII.
        for mauvais in ["abc", "zz", "éé"] {
            assert!(matches!(decode_hex(mauvais), Err(NdError::Crypto(_))));
        }
    }

    #[test]
    fn load_or_create_cree_puis_recharge_la_meme_paire() {
        let fichier = FichierTemp::nouveau("identite");

        // Première invocation : le fichier n'existe pas, l'identité est générée.
        let premiere = IdentityStore::load_or_create(fichier.chemin()).expect("création");
        assert!(fichier.chemin().exists(), "le fichier doit être créé");

        // Seconde invocation : rechargement, mêmes clés privée et publique.
        let seconde = IdentityStore::load_or_create(fichier.chemin()).expect("rechargement");
        assert_eq!(premiere.private, seconde.private);
        assert_eq!(premiere.public, seconde.public);
    }

    #[test]
    fn refuse_un_fichier_corrompu() {
        let cas = [
            // Contenu arbitraire.
            "n'importe quoi\n".to_string(),
            // En-tête inconnu.
            format!(
                "autre-entete v9\n{}\n{}\n",
                "00".repeat(32),
                "00".repeat(32)
            ),
            // Hex invalide sur la clé privée.
            format!("{ENTETE_IDENTITE}\nzz\n{}\n", "00".repeat(32)),
            // Clé trop courte.
            format!(
                "{ENTETE_IDENTITE}\n{}\n{}\n",
                "00".repeat(8),
                "00".repeat(8)
            ),
            // Ligne surnuméraire.
            format!(
                "{ENTETE_IDENTITE}\n{}\n{}\nresidu\n",
                "00".repeat(32),
                "00".repeat(32)
            ),
        ];
        for contenu in cas {
            let fichier = FichierTemp::nouveau("identite-corrompue");
            fs::write(fichier.chemin(), &contenu).expect("écriture du fichier de test");
            assert!(
                matches!(
                    IdentityStore::load_or_create(fichier.chemin()),
                    Err(NdError::Crypto(_))
                ),
                "contenu accepté à tort : {contenu:?}"
            );
        }
    }

    #[test]
    fn refuse_des_cles_incoherentes() {
        // Format valide, mais la clé publique appartient à une autre paire : le
        // contrôle d'intégrité (dérivation X25519) doit rejeter le fichier.
        let a = generate_static_keypair().expect("paire A");
        let b = generate_static_keypair().expect("paire B");
        let fichier = FichierTemp::nouveau("identite-incoherente");
        let contenu = format!(
            "{ENTETE_IDENTITE}\n{}\n{}\n",
            encode_hex(&a.private),
            encode_hex(&b.public)
        );
        fs::write(fichier.chemin(), contenu).expect("écriture du fichier de test");
        assert!(matches!(
            IdentityStore::load_or_create(fichier.chemin()),
            Err(NdError::Crypto(_))
        ));
    }
}
