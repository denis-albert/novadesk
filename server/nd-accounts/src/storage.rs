//! Persistance des comptes NovaDesk — base **redb** transactionnelle (plan 11).
//!
//! Le stockage « document JSON réécrit en entier » des débuts est remplacé par
//! une vraie base embarquée : **redb**, clé/valeur transactionnelle **pur
//! Rust** (fichier unique, ACID, commits durables — `fsync` avant visibilité).
//! Chaque mutation durable (`inserer_compte`, `definir_secret_2fa`,
//! `inserer_lien_oidc`, `definir_licence`) est **une transaction** : un arrêt
//! brutal laisse la base dans l'état d'avant ou d'après, jamais entre les deux.
//!
//! ## Schéma (version [`VERSION_SCHEMA`])
//! | table         | clé                  | valeur                                  |
//! |---------------|----------------------|-----------------------------------------|
//! | `meta`        | `"version_schema"`   | version du schéma (u64)                 |
//! | `comptes`     | e-mail               | hachage **PHC Argon2id**                |
//! | `secrets_2fa` | e-mail               | secret TOTP **scellé** (AEAD, voir bas) |
//! | `liens_oidc`  | sujet (`iss\|sub`)   | e-mail du compte lié                    |
//! | `licences`    | e-mail               | nom du plan (`free`/`pro`/`entreprise`) |
//! | `sessions`    | *réservée*           | jetons durables à venir (les sessions   |
//! |               |                      | d'authentification restent volatiles)   |
//!
//! **Migrations** : la version du schéma est lue à l'ouverture. Une base
//! vierge est initialisée à la version courante ; une version antérieure
//! passera par des étapes de migration successives (aucune à ce jour) ; une
//! version **plus récente** que le service est refusée (jamais réécrite à
//! l'aveugle).
//!
//! **Import de l'ancien format** : à l'initialisation d'une base vierge, si un
//! fichier JSON de l'ancien stockage existe au même chemin avec l'extension
//! `.json` (p. ex. `comptes.json` à côté de `comptes.redb`), son contenu
//! (comptes, secrets 2FA, liens OIDC) est importé **dans la transaction
//! d'initialisation** — tout ou rien. Le fichier JSON d'origine est laissé
//! intact.
//!
//! ## Sécurité
//! - seuls les **hachages PHC Argon2id** sont écrits, jamais un mot de passe ;
//! - les **secrets TOTP sont chiffrés au repos** ([`crate::chiffre::Chiffreur`],
//!   ChaCha20-Poly1305, clé dérivée du secret serveur) avec l'e-mail du compte
//!   en données associées : un blob déplacé vers un autre compte ne s'ouvre
//!   pas. Les secrets de l'ancien JSON (en clair) sont chiffrés à l'import ;
//! - le fichier doit rester protégé par les permissions du système (répertoire
//!   du service, lecture réservée au compte de service).

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use redb::{Database, ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::chiffre::Chiffreur;
use crate::licensing::Plan;

/// Version du schéma redb. Incrémentée à chaque évolution ; les migrations
/// s'exécutent en escalier à l'ouverture (voir la doc du module).
pub const VERSION_SCHEMA: u64 = 1;

/// Version du **format JSON hérité** (import uniquement).
pub const VERSION_FORMAT: u32 = 1;

/// Clé de la version du schéma dans la table `meta`.
const CLE_VERSION: &str = "version_schema";

const TABLE_META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const TABLE_COMPTES: TableDefinition<&str, &str> = TableDefinition::new("comptes");
const TABLE_SECRETS_2FA: TableDefinition<&str, &[u8]> = TableDefinition::new("secrets_2fa");
const TABLE_LIENS_OIDC: TableDefinition<&str, &str> = TableDefinition::new("liens_oidc");
const TABLE_LICENCES: TableDefinition<&str, &str> = TableDefinition::new("licences");
/// Réservée aux jetons durables (rafraîchissement) du plan 11 ; créée dès la
/// v1 du schéma pour que les services plus récents la trouvent toujours.
const TABLE_SESSIONS: TableDefinition<&str, &str> = TableDefinition::new("sessions");

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreurs du stockage persistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurStockage {
    /// Base illisible (fichier corrompu, verrouillé, permissions…).
    Ouverture(String),
    /// Schéma d'une version plus récente que le service : refus.
    VersionFuture(u64),
    /// Fichier JSON hérité illisible ou incohérent : import refusé.
    Import(String),
    /// Secret scellé impossible à déchiffrer (mauvais secret serveur, blob
    /// altéré ou déplacé) — l'e-mail du compte concerné est indiqué.
    SecretIndechiffrable(String),
    /// Erreur interne de la base (transaction, table, e/s).
    Interne(String),
}

impl fmt::Display for ErreurStockage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurStockage::Ouverture(message) => {
                write!(f, "base de comptes illisible : {message}")
            }
            ErreurStockage::VersionFuture(version) => write!(
                f,
                "schéma v{version} plus récent que le service (v{VERSION_SCHEMA})"
            ),
            ErreurStockage::Import(message) => {
                write!(f, "import du fichier JSON hérité impossible : {message}")
            }
            ErreurStockage::SecretIndechiffrable(email) => write!(
                f,
                "secret TOTP indéchiffrable pour {email} (secret serveur changé ou base altérée)"
            ),
            ErreurStockage::Interne(message) => write!(f, "erreur de la base : {message}"),
        }
    }
}

impl std::error::Error for ErreurStockage {}

/// Convertit toute erreur affichable en [`ErreurStockage::Interne`].
fn interne<E: fmt::Display>(erreur: E) -> ErreurStockage {
    ErreurStockage::Interne(erreur.to_string())
}

// ---------------------------------------------------------------------------
// Ancien format JSON (import)
// ---------------------------------------------------------------------------

/// État durable de l'**ancien stockage JSON**, conservé pour l'import.
///
/// Les champs optionnels portent `#[serde(default)]` : un fichier d'une
/// version antérieure qui ne les connaît pas reste lisible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DonneesPersistees {
    /// Version du format (voir [`VERSION_FORMAT`]).
    pub version: u32,
    /// E-mail → hachage **PHC Argon2id** (jamais le mot de passe en clair).
    pub comptes: HashMap<String, String>,
    /// E-mail → secret TOTP encodé en hexadécimal (l'ancien format le
    /// stockait **en clair** ; il est chiffré à l'import).
    #[serde(default)]
    pub secrets_2fa: HashMap<String, String>,
    /// Sujet OIDC (`iss|sub`) → e-mail du compte local lié.
    #[serde(default)]
    pub liens_oidc: HashMap<String, String>,
}

/// Données héritées prêtes à insérer (secrets décodés de l'hexadécimal).
struct ImportJson {
    comptes: HashMap<String, String>,
    secrets_2fa: Vec<(String, Vec<u8>)>,
    liens_oidc: HashMap<String, String>,
}

/// Chemin du fichier JSON hérité associé à une base : même chemin, extension
/// `.json` (`None` si c'est déjà celui de la base, ou s'il n'existe pas).
fn chemin_json_herite(chemin: &Path) -> Option<PathBuf> {
    let candidat = chemin.with_extension("json");
    (candidat != chemin && candidat.is_file()).then_some(candidat)
}

/// Lit et décode le fichier JSON hérité.
fn charger_json_herite(chemin: &Path) -> Result<ImportJson, ErreurStockage> {
    let contenu = std::fs::read_to_string(chemin)
        .map_err(|e| ErreurStockage::Import(format!("lecture de {} : {e}", chemin.display())))?;
    let donnees: DonneesPersistees = serde_json::from_str(&contenu)
        .map_err(|e| ErreurStockage::Import(format!("JSON illisible : {e}")))?;
    if donnees.version > VERSION_FORMAT {
        return Err(ErreurStockage::Import(format!(
            "format JSON v{} plus récent que le service (v{VERSION_FORMAT})",
            donnees.version
        )));
    }
    let mut secrets_2fa = Vec::with_capacity(donnees.secrets_2fa.len());
    for (email, hex) in donnees.secrets_2fa {
        let secret = hex_vers_octets(&hex)
            .ok_or_else(|| ErreurStockage::Import(format!("secret TOTP corrompu pour {email}")))?;
        secrets_2fa.push((email, secret));
    }
    Ok(ImportJson {
        comptes: donnees.comptes,
        secrets_2fa,
        liens_oidc: donnees.liens_oidc,
    })
}

// ---------------------------------------------------------------------------
// Stockage redb
// ---------------------------------------------------------------------------

/// État durable du service, tel que rechargé au démarrage (secrets TOTP
/// **déchiffrés**, prêts pour la vérification des codes).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EtatDurable {
    /// E-mail → hachage PHC Argon2id.
    pub comptes: HashMap<String, String>,
    /// E-mail → secret TOTP (octets bruts).
    pub secrets_2fa: HashMap<String, Vec<u8>>,
    /// Sujet OIDC → e-mail du compte lié.
    pub liens_oidc: HashMap<String, String>,
    /// E-mail → plan de licence.
    pub licences: HashMap<String, Plan>,
}

/// Stockage redb du service de comptes. Clonable : les clones partagent la
/// même base (redb sérialise les écrivains) et le même chiffreur.
#[derive(Clone)]
pub struct StockageRedb {
    db: Arc<Database>,
    chiffreur: Chiffreur,
}

impl StockageRedb {
    /// Ouvre (ou crée) la base au chemin donné, applique les migrations de
    /// schéma, et importe l'éventuel fichier JSON hérité si la base est
    /// vierge (voir la doc du module).
    ///
    /// # Errors
    /// [`ErreurStockage::Ouverture`] si le fichier n'est pas une base redb
    /// lisible, [`ErreurStockage::VersionFuture`] si le schéma vient d'un
    /// service plus récent, [`ErreurStockage::Import`] si le JSON hérité est
    /// illisible, [`ErreurStockage::Interne`] pour le reste.
    pub fn ouvrir(chemin: &Path, chiffreur: Chiffreur) -> Result<Self, ErreurStockage> {
        // Les répertoires parents sont créés au besoin (parité avec l'ancien
        // stockage fichier ; redb ne les crée pas lui-même).
        if let Some(parent) = chemin.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ErreurStockage::Ouverture(e.to_string()))?;
            }
        }
        let db = Database::create(chemin).map_err(|e| ErreurStockage::Ouverture(e.to_string()))?;
        let stockage = Self {
            db: Arc::new(db),
            chiffreur,
        };
        match stockage.version_schema()? {
            // Base vierge : schéma courant + éventuel import hérité, en une
            // seule transaction (tout ou rien).
            None => {
                let herite = match chemin_json_herite(chemin) {
                    Some(json) => Some(charger_json_herite(&json)?),
                    None => None,
                };
                stockage.initialiser(herite)?;
            }
            Some(version) if version > VERSION_SCHEMA => {
                return Err(ErreurStockage::VersionFuture(version));
            }
            // Version courante — les futures migrations (v1 → v2 → …)
            // s'enchaîneront ici, chacune dans sa transaction.
            Some(_) => {}
        }
        Ok(stockage)
    }

    /// Version du schéma en base (`None` si la base est vierge).
    fn version_schema(&self) -> Result<Option<u64>, ErreurStockage> {
        let txn = self.db.begin_read().map_err(interne)?;
        match txn.open_table(TABLE_META) {
            Ok(meta) => Ok(meta.get(CLE_VERSION).map_err(interne)?.map(|g| g.value())),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(interne(e)),
        }
    }

    /// Initialise une base vierge : crée toutes les tables du schéma, pose la
    /// version courante et insère les données héritées éventuelles (secrets
    /// TOTP **chiffrés** au passage) — le tout dans une transaction.
    fn initialiser(&self, herite: Option<ImportJson>) -> Result<(), ErreurStockage> {
        let txn = self.db.begin_write().map_err(interne)?;
        {
            let mut meta = txn.open_table(TABLE_META).map_err(interne)?;
            meta.insert(CLE_VERSION, VERSION_SCHEMA).map_err(interne)?;
            let mut comptes = txn.open_table(TABLE_COMPTES).map_err(interne)?;
            let mut secrets = txn.open_table(TABLE_SECRETS_2FA).map_err(interne)?;
            let mut liens = txn.open_table(TABLE_LIENS_OIDC).map_err(interne)?;
            txn.open_table(TABLE_LICENCES).map_err(interne)?;
            txn.open_table(TABLE_SESSIONS).map_err(interne)?;
            if let Some(donnees) = herite {
                for (email, phc) in &donnees.comptes {
                    comptes
                        .insert(email.as_str(), phc.as_str())
                        .map_err(interne)?;
                }
                for (email, secret) in &donnees.secrets_2fa {
                    let scelle = self.chiffreur.chiffrer(secret, email.as_bytes());
                    secrets
                        .insert(email.as_str(), scelle.as_slice())
                        .map_err(interne)?;
                }
                for (sujet, email) in &donnees.liens_oidc {
                    liens
                        .insert(sujet.as_str(), email.as_str())
                        .map_err(interne)?;
                }
            }
        }
        txn.commit().map_err(interne)
    }

    /// Recharge l'intégralité de l'état durable (instantané cohérent : une
    /// seule transaction de lecture), secrets TOTP déchiffrés.
    ///
    /// # Errors
    /// [`ErreurStockage::SecretIndechiffrable`] si un secret scellé ne s'ouvre
    /// pas (secret serveur changé, base altérée), [`ErreurStockage::Interne`]
    /// pour une erreur de la base ou un plan inconnu.
    pub fn charger(&self) -> Result<EtatDurable, ErreurStockage> {
        let txn = self.db.begin_read().map_err(interne)?;
        let mut etat = EtatDurable::default();

        let comptes = txn.open_table(TABLE_COMPTES).map_err(interne)?;
        for entree in comptes.iter().map_err(interne)? {
            let (email, phc) = entree.map_err(interne)?;
            etat.comptes
                .insert(email.value().to_string(), phc.value().to_string());
        }

        let secrets = txn.open_table(TABLE_SECRETS_2FA).map_err(interne)?;
        for entree in secrets.iter().map_err(interne)? {
            let (email, scelle) = entree.map_err(interne)?;
            let email = email.value().to_string();
            let secret = self
                .chiffreur
                .dechiffrer(scelle.value(), email.as_bytes())
                .ok_or_else(|| ErreurStockage::SecretIndechiffrable(email.clone()))?;
            etat.secrets_2fa.insert(email, secret);
        }

        let liens = txn.open_table(TABLE_LIENS_OIDC).map_err(interne)?;
        for entree in liens.iter().map_err(interne)? {
            let (sujet, email) = entree.map_err(interne)?;
            etat.liens_oidc
                .insert(sujet.value().to_string(), email.value().to_string());
        }

        let licences = txn.open_table(TABLE_LICENCES).map_err(interne)?;
        for entree in licences.iter().map_err(interne)? {
            let (email, nom) = entree.map_err(interne)?;
            let plan = Plan::depuis_nom(nom.value())
                .ok_or_else(|| interne(format!("plan inconnu en base : {}", nom.value())))?;
            etat.licences.insert(email.value().to_string(), plan);
        }
        Ok(etat)
    }

    /// Insère (ou remplace) le hachage PHC d'un compte — une transaction.
    ///
    /// # Errors
    /// [`ErreurStockage::Interne`] si la transaction échoue.
    pub fn inserer_compte(&self, email: &str, phc: &str) -> Result<(), ErreurStockage> {
        self.ecrire(TABLE_COMPTES, email, phc)
    }

    /// Scelle (AEAD, e-mail en données associées) puis écrit le secret TOTP
    /// d'un compte — une transaction.
    ///
    /// # Errors
    /// [`ErreurStockage::Interne`] si la transaction échoue.
    pub fn definir_secret_2fa(&self, email: &str, secret: &[u8]) -> Result<(), ErreurStockage> {
        let scelle = self.chiffreur.chiffrer(secret, email.as_bytes());
        let txn = self.db.begin_write().map_err(interne)?;
        {
            let mut table = txn.open_table(TABLE_SECRETS_2FA).map_err(interne)?;
            table.insert(email, scelle.as_slice()).map_err(interne)?;
        }
        txn.commit().map_err(interne)
    }

    /// Insère un lien OIDC (sujet → e-mail) — une transaction.
    ///
    /// # Errors
    /// [`ErreurStockage::Interne`] si la transaction échoue.
    pub fn inserer_lien_oidc(&self, sujet: &str, email: &str) -> Result<(), ErreurStockage> {
        self.ecrire(TABLE_LIENS_OIDC, sujet, email)
    }

    /// Écrit le plan de licence d'un compte — une transaction.
    ///
    /// # Errors
    /// [`ErreurStockage::Interne`] si la transaction échoue.
    pub fn definir_licence(&self, email: &str, plan: Plan) -> Result<(), ErreurStockage> {
        self.ecrire(TABLE_LICENCES, email, plan.nom())
    }

    /// Écriture transactionnelle d'une paire clé/valeur texte.
    fn ecrire(
        &self,
        table: TableDefinition<'static, &'static str, &'static str>,
        cle: &str,
        valeur: &str,
    ) -> Result<(), ErreurStockage> {
        let txn = self.db.begin_write().map_err(interne)?;
        {
            let mut table = txn.open_table(table).map_err(interne)?;
            table.insert(cle, valeur).map_err(interne)?;
        }
        txn.commit().map_err(interne)
    }

    /// Force la version du schéma (tests des refus de version future).
    #[cfg(test)]
    fn forcer_version_schema(&self, version: u64) -> Result<(), ErreurStockage> {
        let txn = self.db.begin_write().map_err(interne)?;
        {
            let mut meta = txn.open_table(TABLE_META).map_err(interne)?;
            meta.insert(CLE_VERSION, version).map_err(interne)?;
        }
        txn.commit().map_err(interne)
    }
}

// ---------------------------------------------------------------------------
// Encodage hexadécimal (secrets TOTP du JSON hérité, clés publiques…)
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

    /// Fichier temporaire unique (non créé) pour une base `.redb` ; supprimé
    /// au `Drop`, avec ses fichiers frères (`.json` hérité, `.cle` du secret
    /// serveur) créés par certains tests.
    pub struct FichierTemp(PathBuf);

    impl FichierTemp {
        pub fn nouveau(prefixe: &str) -> Self {
            Self(chemin_unique(prefixe, ".redb"))
        }

        pub fn chemin(&self) -> &Path {
            &self.0
        }

        /// Chemin du fichier JSON hérité associé (même nom, extension .json).
        pub fn chemin_json(&self) -> PathBuf {
            self.0.with_extension("json")
        }

        /// Chemin du fichier de clé serveur associé (`<chemin>.cle`).
        pub fn chemin_cle(&self) -> PathBuf {
            let mut nom = self.0.as_os_str().to_owned();
            nom.push(".cle");
            PathBuf::from(nom)
        }
    }

    impl Drop for FichierTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.chemin_json());
            let _ = std::fs::remove_file(self.chemin_cle());
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
    use super::test_util::FichierTemp;
    use super::*;

    fn chiffreur_test() -> Chiffreur {
        Chiffreur::depuis_secret(b"secret-serveur-des-tests-storage")
    }

    fn ouvrir(tmp: &FichierTemp) -> StockageRedb {
        StockageRedb::ouvrir(tmp.chemin(), chiffreur_test()).expect("ouverture de la base")
    }

    /// Secret TOTP volontairement reconnaissable pour la recherche d'octets.
    const SECRET_MARQUE: &[u8] = b"SECRET-TOTP-EN-CLAIR";

    #[test]
    fn aller_retour_toutes_tables() {
        let tmp = FichierTemp::nouveau("aller-retour");
        {
            let stockage = ouvrir(&tmp);
            stockage
                .inserer_compte("a@example.com", "$argon2id$v=19$m=8,t=1,p=1$sel$h")
                .expect("compte");
            stockage
                .definir_secret_2fa("a@example.com", SECRET_MARQUE)
                .expect("secret");
            stockage
                .inserer_lien_oidc("https://idp.example|sub-42", "a@example.com")
                .expect("lien");
            stockage
                .definir_licence("a@example.com", Plan::Pro)
                .expect("licence");
        } // la base est refermée : seul le fichier survit

        let etat = ouvrir(&tmp).charger().expect("rechargement");
        assert_eq!(
            etat.comptes.get("a@example.com").map(String::as_str),
            Some("$argon2id$v=19$m=8,t=1,p=1$sel$h")
        );
        assert_eq!(
            etat.secrets_2fa.get("a@example.com").map(Vec::as_slice),
            Some(SECRET_MARQUE)
        );
        assert_eq!(
            etat.liens_oidc
                .get("https://idp.example|sub-42")
                .map(String::as_str),
            Some("a@example.com")
        );
        assert_eq!(etat.licences.get("a@example.com"), Some(&Plan::Pro));

        // Réécrire une clé remplace la valeur (pas d'append).
        let stockage = ouvrir(&tmp);
        stockage
            .definir_licence("a@example.com", Plan::Free)
            .expect("licence remplacée");
        let etat = stockage.charger().expect("relecture");
        assert_eq!(etat.licences.get("a@example.com"), Some(&Plan::Free));
    }

    #[test]
    fn base_vierge_initialisee_vide() {
        let tmp = FichierTemp::nouveau("vierge");
        let stockage = ouvrir(&tmp);
        assert_eq!(
            stockage.charger().expect("chargement"),
            EtatDurable::default()
        );
        // La version du schéma est posée dès l'initialisation.
        assert_eq!(
            stockage.version_schema().expect("version"),
            Some(VERSION_SCHEMA)
        );
    }

    #[test]
    fn secret_totp_chiffre_au_repos() {
        let tmp = FichierTemp::nouveau("chiffre");
        {
            let stockage = ouvrir(&tmp);
            stockage
                .definir_secret_2fa("a@example.com", SECRET_MARQUE)
                .expect("secret");
        }
        // Ni le secret brut, ni son encodage hexadécimal (l'ancien format en
        // clair) n'apparaissent dans le fichier de la base.
        let brut = std::fs::read(tmp.chemin()).expect("lecture du fichier");
        assert!(
            !brut
                .windows(SECRET_MARQUE.len())
                .any(|fenetre| fenetre == SECRET_MARQUE),
            "le secret TOTP ne doit jamais toucher le disque en clair"
        );
        let hex = octets_vers_hex(SECRET_MARQUE);
        let texte = String::from_utf8_lossy(&brut);
        assert!(!texte.contains(&hex), "pas d'hexadécimal en clair non plus");

        // Mais il se déchiffre normalement au rechargement…
        let etat = ouvrir(&tmp).charger().expect("rechargement");
        assert_eq!(
            etat.secrets_2fa.get("a@example.com").map(Vec::as_slice),
            Some(SECRET_MARQUE)
        );
        // … et un autre secret serveur ne peut pas l'ouvrir.
        let autre = StockageRedb::ouvrir(tmp.chemin(), Chiffreur::depuis_secret(b"autre-secret"))
            .expect("ouverture");
        assert_eq!(
            autre.charger(),
            Err(ErreurStockage::SecretIndechiffrable("a@example.com".into()))
        );
    }

    #[test]
    fn import_du_json_herite_dans_une_base_vierge() {
        let tmp = FichierTemp::nouveau("import");
        // Un fichier de l'ancien format attend à côté de la future base.
        let herite = DonneesPersistees {
            version: VERSION_FORMAT,
            comptes: HashMap::from([(
                "a@example.com".to_string(),
                "$argon2id$v=19$m=8,t=1,p=1$sel$h".to_string(),
            )]),
            secrets_2fa: HashMap::from([(
                "a@example.com".to_string(),
                octets_vers_hex(SECRET_MARQUE),
            )]),
            liens_oidc: HashMap::from([(
                "https://idp|sub-1".to_string(),
                "a@example.com".to_string(),
            )]),
        };
        std::fs::write(
            tmp.chemin_json(),
            serde_json::to_string(&herite).expect("sérialisation"),
        )
        .expect("écriture du JSON hérité");

        let etat = ouvrir(&tmp).charger().expect("chargement après import");
        assert_eq!(etat.comptes.len(), 1);
        assert_eq!(
            etat.secrets_2fa.get("a@example.com").map(Vec::as_slice),
            Some(SECRET_MARQUE),
            "le secret hérité (hexadécimal en clair) est rechiffré et relisible"
        );
        assert_eq!(etat.liens_oidc.len(), 1);
        // Le secret importé est bien chiffré dans la base…
        let brut = std::fs::read(tmp.chemin()).expect("lecture");
        assert!(!brut
            .windows(SECRET_MARQUE.len())
            .any(|fenetre| fenetre == SECRET_MARQUE));
        // … et le fichier hérité est laissé intact.
        assert!(tmp.chemin_json().is_file());

        // Une base déjà initialisée n'importe pas une seconde fois : un
        // compte ajouté au JSON après coup n'apparaît pas.
        let mut modifie = herite;
        modifie
            .comptes
            .insert("b@example.com".into(), "$argon2id$…".into());
        std::fs::write(
            tmp.chemin_json(),
            serde_json::to_string(&modifie).expect("sérialisation"),
        )
        .expect("réécriture du JSON");
        let etat = ouvrir(&tmp).charger().expect("rechargement");
        assert_eq!(etat.comptes.len(), 1, "pas de second import");
    }

    #[test]
    fn import_json_corrompu_ou_futur_refuse() {
        // JSON illisible : l'initialisation échoue, rien n'est créé à moitié.
        let tmp = FichierTemp::nouveau("import-corrompu");
        std::fs::write(tmp.chemin_json(), b"{ pas du json ]").expect("écriture");
        assert!(matches!(
            StockageRedb::ouvrir(tmp.chemin(), chiffreur_test()),
            Err(ErreurStockage::Import(_))
        ));

        // Version JSON future : refusée aussi.
        let tmp = FichierTemp::nouveau("import-futur");
        let futur = DonneesPersistees {
            version: VERSION_FORMAT + 1,
            ..DonneesPersistees::default()
        };
        std::fs::write(
            tmp.chemin_json(),
            serde_json::to_string(&futur).expect("sérialisation"),
        )
        .expect("écriture");
        assert!(matches!(
            StockageRedb::ouvrir(tmp.chemin(), chiffreur_test()),
            Err(ErreurStockage::Import(_))
        ));

        // Secret hérité non hexadécimal : import refusé.
        let tmp = FichierTemp::nouveau("import-secret-corrompu");
        let corrompu = DonneesPersistees {
            version: VERSION_FORMAT,
            secrets_2fa: HashMap::from([("a@x".to_string(), "zz".to_string())]),
            ..DonneesPersistees::default()
        };
        std::fs::write(
            tmp.chemin_json(),
            serde_json::to_string(&corrompu).expect("sérialisation"),
        )
        .expect("écriture");
        assert!(matches!(
            StockageRedb::ouvrir(tmp.chemin(), chiffreur_test()),
            Err(ErreurStockage::Import(_))
        ));
    }

    #[test]
    fn cree_les_dossiers_parents() {
        let dossier = super::test_util::DossierTemp::nouveau("parents");
        let chemin = dossier.chemin().join("etage").join("comptes.redb");
        let stockage =
            StockageRedb::ouvrir(&chemin, chiffreur_test()).expect("ouverture avec parents créés");
        stockage
            .inserer_compte("a@example.com", "$argon2id$…")
            .expect("écriture");
        assert!(chemin.is_file(), "la base existe sous les dossiers créés");
    }

    #[test]
    fn fichier_corrompu_refuse() {
        let tmp = FichierTemp::nouveau("corrompu");
        std::fs::write(tmp.chemin(), b"pas une base redb du tout").expect("écriture");
        assert!(matches!(
            StockageRedb::ouvrir(tmp.chemin(), chiffreur_test()),
            Err(ErreurStockage::Ouverture(_))
        ));
    }

    #[test]
    fn version_future_refusee() {
        let tmp = FichierTemp::nouveau("version-future");
        ouvrir(&tmp)
            .forcer_version_schema(VERSION_SCHEMA + 1)
            .expect("version forcée");
        assert_eq!(
            StockageRedb::ouvrir(tmp.chemin(), chiffreur_test())
                .err()
                .expect("refus"),
            ErreurStockage::VersionFuture(VERSION_SCHEMA + 1)
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
