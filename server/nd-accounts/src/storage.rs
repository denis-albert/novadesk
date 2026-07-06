//! Persistance fichier des comptes NovaDesk — JSON + écriture atomique (plan 11).
//!
//! Format : un document JSON unique ([`DonneesPersistees`], versionné) qui
//! tient l'intégralité de l'état **durable** du service : hachages de mots de
//! passe, secrets TOTP, liens de fédération OIDC. Les **sessions ne sont pas
//! persistées** : un jeton de session est volatil par conception (un
//! redémarrage du service invalide les sessions ouvertes).
//!
//! Durabilité : chaque sauvegarde écrit d'abord un fichier temporaire dans le
//! *même répertoire* que le fichier final (même volume : condition d'un
//! `rename` atomique), le synchronise sur disque (`sync_all`), puis le renomme
//! par-dessus le fichier final (`std::fs::rename` remplace l'existant, sous
//! POSIX comme sous Windows). Un arrêt brutal laisse donc soit l'ancien
//! fichier intact, soit le nouveau complet — jamais un fichier tronqué.
//!
//! Sécurité :
//! - seuls les **hachages PHC Argon2id** sont écrits, jamais un mot de passe
//!   en clair ;
//! - les secrets TOTP sont stockés en hexadécimal, **en clair pour l'instant** :
//!   le chiffrement au repos (enveloppe AEAD sous une clé serveur) est prévu
//!   et viendra avec la gestion de clés du plan 11 ;
//! - le fichier doit être protégé par les permissions du système (répertoire
//!   du service, lecture réservée au compte de service).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Version du format de fichier. Incrémentée à chaque évolution incompatible ;
/// un fichier d'une version **plus récente** que le service est refusé au
/// chargement (jamais réécrit à l'aveugle).
pub const VERSION_FORMAT: u32 = 1;

/// État durable du service de comptes, tel qu'écrit sur disque.
///
/// Les champs optionnels portent `#[serde(default)]` : un fichier d'une
/// version antérieure qui ne les connaît pas reste lisible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DonneesPersistees {
    /// Version du format (voir [`VERSION_FORMAT`]).
    pub version: u32,
    /// E-mail → hachage **PHC Argon2id** (jamais le mot de passe en clair).
    pub comptes: HashMap<String, String>,
    /// E-mail → secret TOTP encodé en hexadécimal (voir [`octets_vers_hex`]).
    /// En clair pour l'instant ; chiffrement au repos à venir (doc du module).
    #[serde(default)]
    pub secrets_2fa: HashMap<String, String>,
    /// Sujet OIDC (identifiant stable du fournisseur, p. ex. `iss|sub`) →
    /// e-mail du compte local lié.
    #[serde(default)]
    pub liens_oidc: HashMap<String, String>,
}

/// Compteur d'unicité des fichiers temporaires (plusieurs magasins peuvent
/// écrire dans le même répertoire au sein du même processus).
static COMPTEUR_TEMP: AtomicU64 = AtomicU64::new(0);

/// Stockage fichier d'un [`DonneesPersistees`] : chargement au démarrage,
/// sauvegarde atomique à chaque mutation durable. Clonable (les clones
/// pointent le même chemin) ; la sérialisation des écritures concurrentes est
/// assurée par l'appelant (le verrou d'état d'`AccountStore`).
#[derive(Debug, Clone)]
pub struct StockageFichier {
    chemin: PathBuf,
}

impl StockageFichier {
    /// Stockage pointant le fichier donné (rien n'est lu ni créé ici).
    #[must_use]
    pub fn new(chemin: impl Into<PathBuf>) -> Self {
        Self {
            chemin: chemin.into(),
        }
    }

    /// Chemin du fichier de comptes.
    #[must_use]
    pub fn chemin(&self) -> &Path {
        &self.chemin
    }

    /// Charge le fichier : `Ok(None)` s'il n'existe pas encore (premier
    /// démarrage), `Ok(Some(_))` sinon.
    ///
    /// # Errors
    /// `InvalidData` si le JSON est illisible ou si la version du format est
    /// plus récente que celle du service ; toute autre erreur d'E/S est
    /// propagée telle quelle.
    pub fn charger(&self) -> io::Result<Option<DonneesPersistees>> {
        let contenu = match std::fs::read_to_string(&self.chemin) {
            Ok(contenu) => contenu,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let donnees: DonneesPersistees = serde_json::from_str(&contenu).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fichier de comptes illisible : {e}"),
            )
        })?;
        if donnees.version > VERSION_FORMAT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "fichier de comptes au format v{} plus récent que le service (v{VERSION_FORMAT})",
                    donnees.version
                ),
            ));
        }
        Ok(Some(donnees))
    }

    /// Sauvegarde atomique : fichier temporaire dans le même répertoire,
    /// `sync_all`, puis `rename` par-dessus le fichier final. Les répertoires
    /// parents sont créés au besoin.
    ///
    /// # Errors
    /// Toute erreur d'E/S (création, écriture, synchronisation, renommage) ;
    /// le fichier temporaire est alors supprimé et l'ancien fichier final,
    /// s'il existait, reste intact.
    pub fn sauvegarder(&self, donnees: &DonneesPersistees) -> io::Result<()> {
        let json = serde_json::to_string_pretty(donnees).map_err(io::Error::other)?;
        if let Some(parent) = self.chemin.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let temporaire = self.chemin_temporaire();
        let resultat = Self::ecrire_puis_renommer(&temporaire, &self.chemin, json.as_bytes());
        if resultat.is_err() {
            // Nettoyage best-effort : ne pas masquer l'erreur d'origine.
            let _ = std::fs::remove_file(&temporaire);
        }
        resultat
    }

    /// Écrit `contenu` dans `temporaire`, le synchronise sur disque, puis le
    /// renomme en `definitif` (remplacement atomique du fichier existant).
    fn ecrire_puis_renommer(temporaire: &Path, definitif: &Path, contenu: &[u8]) -> io::Result<()> {
        {
            let mut fichier = std::fs::File::create(temporaire)?;
            fichier.write_all(contenu)?;
            // Durable sur disque *avant* de devenir visible sous le nom final.
            fichier.sync_all()?;
        }
        std::fs::rename(temporaire, definitif)
    }

    /// Nom de fichier temporaire unique, dans le même répertoire que le
    /// fichier final (pid + compteur : deux processus ou deux magasins du même
    /// processus ne se marchent pas dessus).
    fn chemin_temporaire(&self) -> PathBuf {
        let n = COMPTEUR_TEMP.fetch_add(1, Ordering::Relaxed);
        let mut nom = self.chemin.as_os_str().to_owned();
        nom.push(format!(".tmp-{}-{n}", std::process::id()));
        PathBuf::from(nom)
    }
}

// ---------------------------------------------------------------------------
// Encodage hexadécimal (secrets TOTP : octets bruts → texte JSON)
// ---------------------------------------------------------------------------

/// Encode des octets en hexadécimal minuscule (`[1, 255]` → `"01ff"`).
#[must_use]
pub fn octets_vers_hex(octets: &[u8]) -> String {
    let mut s = String::with_capacity(octets.len() * 2);
    for o in octets {
        let _ = write!(s, "{o:02x}");
    }
    s
}

/// Décode une chaîne hexadécimale (casse indifférente) en octets ;
/// `None` si la longueur est impaire ou un caractère n'est pas hexadécimal.
#[must_use]
pub fn hex_vers_octets(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut octets = Vec::with_capacity(hex.len() / 2);
    for paire in hex.as_bytes().chunks(2) {
        let haut = (paire[0] as char).to_digit(16)?;
        let bas = (paire[1] as char).to_digit(16)?;
        octets.push(((haut << 4) | bas) as u8);
    }
    Some(octets)
}

// ---------------------------------------------------------------------------
// Aides de test partagées (fichiers temporaires uniques, nettoyés au Drop)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_util {
    //! Fichiers et dossiers temporaires **uniques** (pid + compteur), supprimés
    //! au `Drop` — même si le test panique. Partagé par les tests du crate.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COMPTEUR: AtomicU64 = AtomicU64::new(0);

    /// Chemin unique dans le répertoire temporaire du système.
    fn chemin_unique(prefixe: &str, suffixe: &str) -> PathBuf {
        let n = COMPTEUR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nd-accounts-{prefixe}-{}-{n}{suffixe}",
            std::process::id()
        ))
    }

    /// Fichier temporaire unique (non créé) ; supprimé au `Drop`.
    pub struct FichierTemp(PathBuf);

    impl FichierTemp {
        pub fn nouveau(prefixe: &str) -> Self {
            Self(chemin_unique(prefixe, ".json"))
        }

        pub fn chemin(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FichierTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Dossier temporaire unique (non créé) ; supprimé récursivement au `Drop`.
    pub struct DossierTemp(PathBuf);

    impl DossierTemp {
        pub fn nouveau(prefixe: &str) -> Self {
            Self(chemin_unique(prefixe, ""))
        }

        pub fn chemin(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for DossierTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::test_util::{DossierTemp, FichierTemp};
    use super::*;

    /// Jeu de données représentatif : compte, secret 2FA, lien OIDC.
    fn donnees_exemple() -> DonneesPersistees {
        let mut donnees = DonneesPersistees {
            version: VERSION_FORMAT,
            ..DonneesPersistees::default()
        };
        donnees.comptes.insert(
            "a@example.com".into(),
            "$argon2id$v=19$m=8,t=1,p=1$c2VsZGVzZWw$hachage".into(),
        );
        donnees
            .secrets_2fa
            .insert("a@example.com".into(), octets_vers_hex(&[1, 2, 3, 255]));
        donnees
            .liens_oidc
            .insert("https://idp.example|sub-42".into(), "a@example.com".into());
        donnees
    }

    #[test]
    fn aller_retour_sauvegarde_chargement() {
        let tmp = FichierTemp::nouveau("aller-retour");
        let stockage = StockageFichier::new(tmp.chemin());
        let donnees = donnees_exemple();
        stockage.sauvegarder(&donnees).expect("sauvegarde");
        assert_eq!(stockage.charger().expect("chargement"), Some(donnees));

        // Une seconde sauvegarde remplace la première (pas d'append).
        let mut v2 = donnees_exemple();
        v2.comptes
            .insert("b@example.com".into(), "$argon2id$…".into());
        stockage.sauvegarder(&v2).expect("2e sauvegarde");
        let relu = stockage.charger().expect("2e chargement").expect("présent");
        assert_eq!(relu.comptes.len(), 2);
    }

    #[test]
    fn fichier_absent_charge_none() {
        let tmp = FichierTemp::nouveau("absent");
        let stockage = StockageFichier::new(tmp.chemin());
        assert_eq!(stockage.charger().expect("absence = Ok(None)"), None);
    }

    #[test]
    fn json_corrompu_refuse() {
        let tmp = FichierTemp::nouveau("corrompu");
        std::fs::write(tmp.chemin(), b"{ pas du json ]").expect("écriture");
        let erreur = StockageFichier::new(tmp.chemin())
            .charger()
            .expect_err("corruption détectée");
        assert_eq!(erreur.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn version_future_refusee() {
        let tmp = FichierTemp::nouveau("version-future");
        let stockage = StockageFichier::new(tmp.chemin());
        let mut donnees = donnees_exemple();
        donnees.version = VERSION_FORMAT + 1;
        stockage.sauvegarder(&donnees).expect("sauvegarde");
        let erreur = stockage.charger().expect_err("version future refusée");
        assert_eq!(erreur.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn ecriture_atomique_sans_residu_temporaire() {
        let tmp = FichierTemp::nouveau("sans-residu");
        let stockage = StockageFichier::new(tmp.chemin());
        stockage
            .sauvegarder(&donnees_exemple())
            .expect("sauvegarde 1");
        stockage
            .sauvegarder(&donnees_exemple())
            .expect("sauvegarde 2");

        // Aucun fichier `<nom>.tmp-*` ne doit subsister à côté du fichier final.
        let nom = tmp.chemin().file_name().expect("nom").to_string_lossy();
        let parent = tmp.chemin().parent().expect("parent");
        let residus = std::fs::read_dir(parent)
            .expect("lecture du répertoire")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{nom}.tmp-"))
            })
            .count();
        assert_eq!(residus, 0, "aucun fichier temporaire résiduel");
        assert!(tmp.chemin().exists(), "le fichier final existe");
    }

    #[test]
    fn cree_les_dossiers_parents() {
        let dossier = DossierTemp::nouveau("parents");
        let chemin = dossier.chemin().join("etage").join("comptes.json");
        let stockage = StockageFichier::new(&chemin);
        assert_eq!(stockage.chemin(), chemin.as_path());
        stockage
            .sauvegarder(&donnees_exemple())
            .expect("sauvegarde");
        assert_eq!(
            stockage.charger().expect("chargement"),
            Some(donnees_exemple())
        );
    }

    #[test]
    fn hex_aller_retour() {
        let tous: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_vers_octets(&octets_vers_hex(&tous)), Some(tous));
        assert_eq!(octets_vers_hex(&[]), "");
        assert_eq!(hex_vers_octets(""), Some(Vec::new()));
        assert_eq!(octets_vers_hex(&[1, 255]), "01ff");
        // Casse indifférente au décodage.
        assert_eq!(hex_vers_octets("01FF"), Some(vec![1, 255]));
    }

    #[test]
    fn hex_invalide_refuse() {
        assert_eq!(hex_vers_octets("abc"), None, "longueur impaire");
        assert_eq!(hex_vers_octets("zz"), None, "caractère non hexadécimal");
        assert_eq!(hex_vers_octets("0é"), None, "caractère multioctet");
    }
}
