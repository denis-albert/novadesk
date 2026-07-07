//! Service comptes / authentification NovaDesk.
//!
//! Opérations : `register(email, password)` (mot de passe haché **Argon2id**,
//! format PHC) et `login(email, password)` → jeton de session opaque (32 octets
//! aléatoires, encodés en hexadécimal).
//!
//! Persistance (module [`storage`]) : [`AccountStore::open`] attache le
//! magasin à une base **redb** transactionnelle — comptes, secrets 2FA, liens
//! OIDC et plans de licence survivent au redémarrage ; chaque mutation durable
//! persiste avant de réussir (« durable ou rien »). Seuls les **hachages PHC
//! Argon2id** sont écrits, jamais un mot de passe ; les **secrets TOTP sont
//! chiffrés au repos** (module [`chiffre`], clé dérivée du **secret serveur** :
//! fichier `<base>.cle` auto-généré, ou secret explicite via
//! [`AccountStore::ouvrir_avec_secret`] / variable `ND_ACCOUNTS_SECRET`). Un
//! fichier JSON de l'ancien format posé à côté de la base (`comptes.json`
//! pour `comptes.redb`) est importé à la première ouverture. Les sessions
//! restent volatiles par conception. [`AccountStore::new`] garde le
//! comportement purement en mémoire (tests).
//!
//! 2FA TOTP (RFC 6238, module [`totp`]) : `enable_2fa(email)` génère et stocke
//! le secret ; un compte protégé doit passer par `login_2fa(email, password,
//! code)` — `login` seul renvoie alors `DeuxFacteursRequis`. Les licences et
//! quotas de sessions vivent dans le module [`licensing`] (plans persistés via
//! [`AccountStore::attribuer_plan`]) ; le journal d'audit (conformité, RGPD)
//! et le registre des sessions actives dans le module [`audit`].
//!
//! Fédération OIDC/OAuth2 (module [`oidc`]) : PKCE S256, URL d'autorisation,
//! **échange code → jetons** au token endpoint et validation d'ID token
//! (**RS256/ES256 via JWKS**, module [`jwks`] ; HS256 pour le développement) ;
//! [`AccountStore::link_oidc`] rattache un sujet fédéré à un compte local,
//! [`AccountStore::login_oidc`] ouvre une session pour un sujet déjà lié
//! (l'authentification — y compris MFA — a eu lieu chez le fournisseur : la
//! 2FA locale ne s'applique pas à ce chemin).
//! Voir `../../plan-technique/11-backend-infrastructure.md`.
//!
//! **Jetons applicatifs** (module [`jeton`]) : après connexion, un jeton de
//! session s'échange contre un JWS **Ed25519** (`iss`, `sub`, `roles`, `plan`,
//! `iat`, `exp`) que nd-api (lot 07) vérifie hors ligne avec la clé publique
//! du service (requête `ClePubliqueJetons`).
//!
//! Serveur TCP (std pur, un thread par connexion, une requête par connexion)
//! au même format que `nd-signaling` : trames à préfixe de longueur `u32` BE.
//! Le protocole couvre inscription, connexion, **flux 2FA complet** (login →
//! challenge TOTP → validation), **flux OIDC** (démarrage / rappel), émission
//! de jetons applicatifs et licences — voir [`Request`] / [`Response`].
//!
//! Usage : `nd-accounts [adresse:port] [base_comptes.redb]`
//! (défaut `0.0.0.0:9200`, en mémoire si aucun fichier n'est donné).
//! Environnement : `ND_ACCOUNTS_SECRET` (secret serveur, hexadécimal) ;
//! `ND_OIDC_ISSUER`, `ND_OIDC_AUTH_ENDPOINT`, `ND_OIDC_TOKEN_ENDPOINT`,
//! `ND_OIDC_JWKS_URI`, `ND_OIDC_CLIENT_ID`, `ND_OIDC_REDIRECT_URI` et
//! `ND_OIDC_LIER_PAR_EMAIL=1` pour activer la fédération.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

pub mod audit;
pub mod chiffre;
pub mod jeton;
pub mod jwks;
pub mod licensing;
pub mod oidc;
pub mod storage;
pub mod totp;

use licensing::Plan;

/// Adresse d'écoute par défaut (9000 = rendez-vous, 9100 = relais).
const ADRESSE_DEFAUT: &str = "0.0.0.0:9200";

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreurs métier du service de comptes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    /// L'adresse e-mail est déjà associée à un compte.
    EmailDejaUtilise,
    /// E-mail inconnu ou mot de passe incorrect (volontairement indistincts).
    IdentifiantsInvalides,
    /// E-mail ou mot de passe vide/malformé.
    EntreeInvalide,
    /// Le compte a la 2FA activée : passer par `login_2fa` avec un code TOTP.
    DeuxFacteursRequis,
    /// La 2FA n'est pas activée sur ce compte (`login_2fa` inutile).
    DeuxFacteursNonActives,
    /// Code TOTP malformé, expiré ou incorrect.
    CodeTotpInvalide,
    /// La 2FA est déjà active : sa réinitialisation par le réseau est refusée
    /// (un mot de passe volé ne doit pas suffire à remplacer le second facteur).
    DeuxFacteursDejaActives,
    /// Compte inconnu (activation 2FA sur un e-mail non enregistré, sujet
    /// OIDC jamais lié, etc.).
    CompteInconnu,
    /// Jeton de session inconnu ou périmé.
    SessionInvalide,
    /// Le sujet OIDC est déjà lié à un **autre** compte local.
    SujetOidcDejaLie,
    /// Erreur du stockage persistant (chargement ou sauvegarde du fichier de
    /// comptes) : la mutation demandée n'a **pas** été appliquée.
    Stockage(String),
    /// Erreur interne (hachage, etc.).
    Interne(String),
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::EmailDejaUtilise => write!(f, "e-mail déjà utilisé"),
            AccountError::IdentifiantsInvalides => write!(f, "identifiants invalides"),
            AccountError::EntreeInvalide => write!(f, "e-mail ou mot de passe invalide"),
            AccountError::DeuxFacteursRequis => write!(f, "code de vérification (2FA) requis"),
            AccountError::DeuxFacteursNonActives => {
                write!(f, "la 2FA n'est pas activée sur ce compte")
            }
            AccountError::CodeTotpInvalide => write!(f, "code de vérification invalide"),
            AccountError::DeuxFacteursDejaActives => {
                write!(f, "la 2FA est déjà active sur ce compte")
            }
            AccountError::CompteInconnu => write!(f, "compte inconnu"),
            AccountError::SessionInvalide => write!(f, "session invalide ou expirée"),
            AccountError::SujetOidcDejaLie => {
                write!(f, "ce sujet OIDC est déjà lié à un autre compte")
            }
            AccountError::Stockage(msg) => write!(f, "erreur de stockage : {msg}"),
            AccountError::Interne(msg) => write!(f, "erreur interne : {msg}"),
        }
    }
}

impl std::error::Error for AccountError {}

// ---------------------------------------------------------------------------
// Logique métier
// ---------------------------------------------------------------------------

/// État interne : e-mail → hachage PHC, jeton de session → e-mail,
/// e-mail → secret TOTP pour les comptes ayant activé la 2FA, sujet OIDC
/// (`iss|sub`) → e-mail pour les identités fédérées liées, et e-mail → plan
/// de licence attribué.
#[derive(Default)]
struct Etat {
    comptes: HashMap<String, String>,
    sessions: HashMap<String, String>,
    secrets_2fa: HashMap<String, Vec<u8>>,
    liens_oidc: HashMap<String, String>,
    licences: HashMap<String, Plan>,
}

impl Etat {
    /// Reconstruit l'état depuis la base (secrets déjà déchiffrés). Les
    /// sessions repartent vides (volatiles par conception).
    fn depuis_durable(durable: storage::EtatDurable) -> Self {
        Self {
            comptes: durable.comptes,
            sessions: HashMap::new(),
            secrets_2fa: durable.secrets_2fa,
            liens_oidc: durable.liens_oidc,
            licences: durable.licences,
        }
    }
}

/// État partagé entre threads de connexion.
type EtatPartage = Arc<Mutex<Etat>>;

/// Magasin de comptes (thread-safe, clonable), en mémoire ou adossé à une
/// base redb (voir [`Self::open`]).
#[derive(Clone)]
pub struct AccountStore {
    etat: EtatPartage,
    argon: Argon2<'static>,
    /// Journal d'audit optionnel (voir [`Self::with_audit`]).
    audit: Option<audit::AuditLog>,
    /// Stockage persistant optionnel (voir [`Self::open`]) ; `None` = magasin
    /// purement en mémoire, volatil.
    stockage: Option<storage::StockageRedb>,
    /// Émetteur des jetons applicatifs Ed25519 (clé dérivée du secret
    /// serveur ; volatile pour un magasin en mémoire).
    jetons: Arc<jeton::EmetteurJetons>,
}

impl Default for AccountStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountStore {
    /// Magasin avec les paramètres Argon2id recommandés par défaut.
    #[must_use]
    pub fn new() -> Self {
        Self::with_argon2(Argon2::default())
    }

    /// Magasin avec une configuration Argon2 personnalisée (tests : paramètres
    /// légers). En mémoire : le secret serveur (jetons applicatifs) est tiré
    /// au hasard et meurt avec le processus.
    #[must_use]
    pub fn with_argon2(argon: Argon2<'static>) -> Self {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        Self {
            etat: EtatPartage::default(),
            argon,
            audit: None,
            stockage: None,
            jetons: Arc::new(jeton::EmetteurJetons::depuis_secret(&secret)),
        }
    }

    /// Magasin **persistant** : ouvre (ou crée) la base redb — migrations et
    /// import de l'ancien JSON compris, voir [`storage`] — puis persiste
    /// chaque mutation durable (`register`, `enable_2fa`, `link_oidc`,
    /// `attribuer_plan`) transactionnellement. Les sessions ne sont pas
    /// persistées. Le **secret serveur** (chiffrement des secrets TOTP, clé
    /// des jetons applicatifs) est lu dans le fichier `<chemin>.cle`,
    /// auto-généré au premier lancement. Paramètres Argon2id par défaut.
    ///
    /// # Errors
    /// `Stockage` si la base ou le fichier de clé sont illisibles.
    pub fn open<P: AsRef<Path>>(chemin: P) -> Result<Self, AccountError> {
        Self::open_with_argon2(chemin, Argon2::default())
    }

    /// Comme [`Self::open`], avec une configuration Argon2 personnalisée
    /// (tests : paramètres légers).
    ///
    /// # Errors
    /// Voir [`Self::open`].
    pub fn open_with_argon2<P: AsRef<Path>>(
        chemin: P,
        argon: Argon2<'static>,
    ) -> Result<Self, AccountError> {
        let secret = secret_serveur_fichier(chemin.as_ref())?;
        Self::ouvrir_avec_secret(chemin, &secret, argon)
    }

    /// Magasin persistant avec un **secret serveur explicite** (déploiements :
    /// variable `ND_ACCOUNTS_SECRET` ; tests). Le secret dérive la clé de
    /// chiffrement des secrets TOTP et la clé Ed25519 des jetons applicatifs —
    /// le changer rend les secrets TOTP en base indéchiffrables.
    ///
    /// # Errors
    /// `Stockage` si la base est illisible (corruption, version future,
    /// import JSON hérité invalide, secret TOTP indéchiffrable).
    pub fn ouvrir_avec_secret<P: AsRef<Path>>(
        chemin: P,
        secret: &[u8],
        argon: Argon2<'static>,
    ) -> Result<Self, AccountError> {
        let chiffreur = chiffre::Chiffreur::depuis_secret(secret);
        let stockage = storage::StockageRedb::ouvrir(chemin.as_ref(), chiffreur)
            .map_err(|e| AccountError::Stockage(e.to_string()))?;
        let durable = stockage
            .charger()
            .map_err(|e| AccountError::Stockage(e.to_string()))?;
        Ok(Self {
            etat: Arc::new(Mutex::new(Etat::depuis_durable(durable))),
            argon,
            audit: None,
            stockage: Some(stockage),
            jetons: Arc::new(jeton::EmetteurJetons::depuis_secret(secret)),
        })
    }

    /// Attache un journal d'audit : créations de compte, connexions (réussies
    /// ou refusées) et activations 2FA y sont consignées. Les signatures
    /// existantes ne changent pas ; les clones du magasin partagent le journal.
    #[must_use]
    pub fn with_audit(mut self, journal: audit::AuditLog) -> Self {
        self.audit = Some(journal);
        self
    }

    /// Consigne un événement dans le journal d'audit, s'il y en a un.
    /// À appeler **hors** du verrou d'état (le journal a son propre Mutex).
    fn auditer(&self, evenement: audit::AuditEvent) {
        if let Some(journal) = &self.audit {
            journal.record(evenement);
        }
    }

    /// Crée un compte : le mot de passe est haché en **Argon2id** (sel
    /// aléatoire). Sur un magasin persistant, le compte est écrit en base
    /// (une transaction) avant que l'appel réussisse (« durable ou rien »).
    /// L'E/S sous verrou d'état est un compromis assumé : les mutations sont
    /// rares (inscription, 2FA, lien OIDC, licence) et ainsi sérialisées.
    ///
    /// # Errors
    /// `EntreeInvalide` si e-mail/mot de passe vide, `EmailDejaUtilise` si le
    /// compte existe, `Stockage` si la persistance échoue (le compte n'est
    /// alors pas créé), `Interne` si le hachage échoue.
    pub fn register(&self, email: &str, password: &str) -> Result<(), AccountError> {
        if email.trim().is_empty() || password.is_empty() {
            return Err(AccountError::EntreeInvalide);
        }
        // Hachage hors verrou : opération volontairement coûteuse.
        let sel = SaltString::generate(&mut OsRng);
        let phc = self
            .argon
            .hash_password(password.as_bytes(), &sel)
            .map_err(|e| AccountError::Interne(e.to_string()))?
            .to_string();
        let mut etat = self.etat.lock().unwrap();
        if etat.comptes.contains_key(email) {
            return Err(AccountError::EmailDejaUtilise);
        }
        if let Some(stockage) = &self.stockage {
            // Durable ou rien : la base d'abord, la mémoire ensuite.
            stockage
                .inserer_compte(email, &phc)
                .map_err(|e| AccountError::Stockage(e.to_string()))?;
        }
        etat.comptes.insert(email.to_string(), phc);
        drop(etat); // audit hors verrou
        self.auditer(audit::AuditEvent::AccountCreated {
            email: email.to_string(),
        });
        Ok(())
    }

    /// Vérifie e-mail + mot de passe (Argon2id) sans ouvrir de session.
    fn verifier_identifiants(&self, email: &str, password: &str) -> Result<(), AccountError> {
        let phc = self
            .etat
            .lock()
            .unwrap()
            .comptes
            .get(email)
            .cloned()
            .ok_or(AccountError::IdentifiantsInvalides)?;
        // Vérification hors verrou, comme le hachage.
        let hachage = PasswordHash::new(&phc).map_err(|e| AccountError::Interne(e.to_string()))?;
        self.argon
            .verify_password(password.as_bytes(), &hachage)
            .map_err(|_| AccountError::IdentifiantsInvalides)
    }

    /// Ouvre une session : jeton opaque associé au compte.
    fn ouvrir_session(&self, email: &str) -> String {
        let jeton = jeton_aleatoire();
        self.etat
            .lock()
            .unwrap()
            .sessions
            .insert(jeton.clone(), email.to_string());
        jeton
    }

    /// Vérifie les identifiants et renvoie un jeton de session opaque.
    ///
    /// # Errors
    /// `IdentifiantsInvalides` si l'e-mail est inconnu ou le mot de passe faux
    /// (indistincts pour ne pas révéler l'existence d'un compte) ;
    /// `DeuxFacteursRequis` si le compte a la 2FA activée (passer par
    /// [`Self::login_2fa`]).
    pub fn login(&self, email: &str, password: &str) -> Result<String, AccountError> {
        self.verifier_identifiants_auditees(email, password)?;
        // Un compte protégé par 2FA exige le second facteur : ce n'est pas un
        // échec de connexion, rien n'est consigné.
        if self.etat.lock().unwrap().secrets_2fa.contains_key(email) {
            return Err(AccountError::DeuxFacteursRequis);
        }
        let jeton = self.ouvrir_session(email);
        self.auditer(audit::AuditEvent::LoginSuccess {
            email: email.to_string(),
        });
        Ok(jeton)
    }

    /// Comme [`Self::verifier_identifiants`], mais consigne un
    /// `LoginFailure` si les identifiants sont invalides (pas pour une
    /// erreur interne, qui ne dit rien sur la tentative).
    fn verifier_identifiants_auditees(
        &self,
        email: &str,
        password: &str,
    ) -> Result<(), AccountError> {
        let resultat = self.verifier_identifiants(email, password);
        if resultat == Err(AccountError::IdentifiantsInvalides) {
            self.auditer(audit::AuditEvent::LoginFailure {
                email: email.to_string(),
            });
        }
        resultat
    }

    /// Active la 2FA TOTP sur un compte : génère un secret de 20 octets, le
    /// stocke et le renvoie (à présenter une seule fois à l'utilisateur, p. ex.
    /// sous forme d'URI `otpauth://` en QR code). Réactiver régénère le secret
    /// (opération locale/admin ; par le réseau, passer par
    /// [`Self::activer_2fa_reseau`] qui exige le mot de passe). Sur un magasin
    /// persistant, le secret est **chiffré** (AEAD, clé serveur) puis écrit en
    /// base avant que l'appel réussisse.
    ///
    /// # Errors
    /// `CompteInconnu` si l'e-mail n'est pas enregistré, `Stockage` si la
    /// persistance échoue (l'ancien état 2FA est alors conservé).
    pub fn enable_2fa(&self, email: &str) -> Result<Vec<u8>, AccountError> {
        let mut etat = self.etat.lock().unwrap();
        if !etat.comptes.contains_key(email) {
            return Err(AccountError::CompteInconnu);
        }
        let secret = totp::generate_totp_secret();
        if let Some(stockage) = &self.stockage {
            // Durable ou rien : si l'écriture échoue, la mémoire n'a pas bougé.
            stockage
                .definir_secret_2fa(email, &secret)
                .map_err(|e| AccountError::Stockage(e.to_string()))?;
        }
        etat.secrets_2fa.insert(email.to_string(), secret.clone());
        drop(etat); // audit hors verrou
        self.auditer(audit::AuditEvent::TwoFactorEnabled {
            email: email.to_string(),
        });
        Ok(secret)
    }

    /// Activation de la 2FA **par le réseau** : exige le mot de passe du
    /// compte, et refuse de remplacer un second facteur déjà actif (un mot de
    /// passe volé ne doit pas suffire à substituer le TOTP de l'attaquant).
    ///
    /// # Errors
    /// `IdentifiantsInvalides` (mot de passe, vérifié en premier),
    /// `DeuxFacteursDejaActives` si un secret existe déjà, puis les erreurs
    /// de [`Self::enable_2fa`].
    pub fn activer_2fa_reseau(&self, email: &str, password: &str) -> Result<Vec<u8>, AccountError> {
        self.verifier_identifiants_auditees(email, password)?;
        if self.etat.lock().unwrap().secrets_2fa.contains_key(email) {
            return Err(AccountError::DeuxFacteursDejaActives);
        }
        self.enable_2fa(email)
    }

    /// Connexion avec second facteur : mot de passe **puis** code TOTP
    /// (fenêtre de ±1 pas de 30 s autour de l'heure système).
    ///
    /// # Errors
    /// `IdentifiantsInvalides` (mot de passe, vérifié en premier),
    /// `DeuxFacteursNonActives` si le compte n'a pas de 2FA,
    /// `CodeTotpInvalide` si le code est malformé ou hors fenêtre.
    pub fn login_2fa(
        &self,
        email: &str,
        password: &str,
        code: &str,
    ) -> Result<String, AccountError> {
        self.verifier_identifiants_auditees(email, password)?;
        let secret = self
            .etat
            .lock()
            .unwrap()
            .secrets_2fa
            .get(email)
            .cloned()
            .ok_or(AccountError::DeuxFacteursNonActives)?;
        if !totp::verify_totp(&secret, code, unix_maintenant()) {
            // Second facteur incorrect : c'est un échec de connexion.
            self.auditer(audit::AuditEvent::LoginFailure {
                email: email.to_string(),
            });
            return Err(AccountError::CodeTotpInvalide);
        }
        let jeton = self.ouvrir_session(email);
        self.auditer(audit::AuditEvent::LoginSuccess {
            email: email.to_string(),
        });
        Ok(jeton)
    }

    /// Résout un jeton de session en e-mail de compte (None si inconnu).
    #[must_use]
    pub fn verify_token(&self, jeton: &str) -> Option<String> {
        self.etat.lock().unwrap().sessions.get(jeton).cloned()
    }

    // -- Fédération OIDC (module [`oidc`]) ----------------------------------

    /// Lie une identité fédérée à un compte local : `subject` est
    /// l'identifiant stable de l'utilisateur chez le fournisseur — utiliser
    /// une clé globalement unique, p. ex. `"{iss}|{sub}"` d'un ID token validé
    /// par [`oidc::validate_id_token`] (l'e-mail vient typiquement du claim
    /// `email`). Relier le même sujet au même compte est idempotent. Sur un
    /// magasin persistant, le lien est écrit sur disque avant de réussir.
    ///
    /// # Errors
    /// `EntreeInvalide` si un champ est vide, `CompteInconnu` si l'e-mail
    /// n'est pas enregistré, `SujetOidcDejaLie` si le sujet pointe déjà un
    /// autre compte, `Stockage` si la persistance échoue (le lien n'est alors
    /// pas créé).
    pub fn link_oidc(&self, email: &str, subject: &str) -> Result<(), AccountError> {
        if email.trim().is_empty() || subject.trim().is_empty() {
            return Err(AccountError::EntreeInvalide);
        }
        let mut etat = self.etat.lock().unwrap();
        if !etat.comptes.contains_key(email) {
            return Err(AccountError::CompteInconnu);
        }
        match etat.liens_oidc.get(subject) {
            Some(existant) if existant == email => return Ok(()), // idempotent
            Some(_) => return Err(AccountError::SujetOidcDejaLie),
            None => {}
        }
        if let Some(stockage) = &self.stockage {
            // Durable ou rien : la base d'abord, la mémoire ensuite.
            stockage
                .inserer_lien_oidc(subject, email)
                .map_err(|e| AccountError::Stockage(e.to_string()))?;
        }
        etat.liens_oidc
            .insert(subject.to_string(), email.to_string());
        Ok(())
    }

    /// E-mail du compte local lié à un sujet OIDC (None si jamais lié).
    #[must_use]
    pub fn oidc_account(&self, subject: &str) -> Option<String> {
        self.etat.lock().unwrap().liens_oidc.get(subject).cloned()
    }

    /// Ouvre une session pour un sujet OIDC déjà lié ([`Self::link_oidc`]).
    /// À n'appeler qu'après validation de l'ID token
    /// ([`oidc::validate_id_token`]) : l'authentification — y compris MFA — a
    /// eu lieu chez le fournisseur, la 2FA locale ne s'applique pas ici.
    ///
    /// # Errors
    /// `CompteInconnu` si le sujet n'est lié à aucun compte.
    pub fn login_oidc(&self, subject: &str) -> Result<String, AccountError> {
        let email = self
            .etat
            .lock()
            .unwrap()
            .liens_oidc
            .get(subject)
            .cloned()
            .ok_or(AccountError::CompteInconnu)?;
        let jeton = self.ouvrir_session(&email);
        self.auditer(audit::AuditEvent::LoginSuccess { email });
        Ok(jeton)
    }

    // -- Licences (module [`licensing`]) -------------------------------------

    /// Attribue (ou change) le plan de licence d'un compte ; persisté sur un
    /// magasin persistant. Le plan apparaît dans le claim `plan` des jetons
    /// applicatifs — nd-api peut ainsi appliquer les quotas sans rappel ici.
    ///
    /// # Errors
    /// `CompteInconnu` si l'e-mail n'est pas enregistré, `Stockage` si la
    /// persistance échoue (le plan précédent est alors conservé).
    pub fn attribuer_plan(&self, email: &str, plan: Plan) -> Result<(), AccountError> {
        let mut etat = self.etat.lock().unwrap();
        if !etat.comptes.contains_key(email) {
            return Err(AccountError::CompteInconnu);
        }
        if let Some(stockage) = &self.stockage {
            // Durable ou rien : la base d'abord, la mémoire ensuite.
            stockage
                .definir_licence(email, plan)
                .map_err(|e| AccountError::Stockage(e.to_string()))?;
        }
        etat.licences.insert(email.to_string(), plan);
        Ok(())
    }

    /// Plan de licence d'un compte (`Free` si aucun n'a été attribué).
    ///
    /// # Errors
    /// `CompteInconnu` si l'e-mail n'est pas enregistré.
    pub fn plan_de(&self, email: &str) -> Result<Plan, AccountError> {
        let etat = self.etat.lock().unwrap();
        if !etat.comptes.contains_key(email) {
            return Err(AccountError::CompteInconnu);
        }
        Ok(etat.licences.get(email).copied().unwrap_or_default())
    }

    // -- Jetons applicatifs (module [`jeton`]) --------------------------------

    /// Échange un jeton de session opaque contre un **jeton applicatif**
    /// signé Ed25519 (claims `iss`/`sub`/`roles`/`plan`/`iat`/`exp`, durée
    /// [`jeton::DUREE_DEFAUT_S`]) que nd-api vérifie hors ligne avec
    /// [`Self::cle_publique_jetons`]. Voir le format dans la doc de [`jeton`].
    ///
    /// # Errors
    /// `SessionInvalide` si le jeton de session est inconnu ou périmé.
    pub fn emettre_jeton_applicatif(&self, jeton_session: &str) -> Result<String, AccountError> {
        let (email, plan) = {
            let etat = self.etat.lock().unwrap();
            let email = etat
                .sessions
                .get(jeton_session)
                .cloned()
                .ok_or(AccountError::SessionInvalide)?;
            let plan = etat.licences.get(&email).copied().unwrap_or_default();
            (email, plan)
        };
        Ok(self.jetons.emettre(
            &email,
            &["utilisateur"],
            plan.nom(),
            unix_maintenant(),
            jeton::DUREE_DEFAUT_S,
        ))
    }

    /// Clé publique Ed25519 (32 octets, hexadécimal) vérifiant les jetons
    /// applicatifs — celle que nd-api (lot 07) doit connaître.
    #[must_use]
    pub fn cle_publique_jetons(&self) -> String {
        self.jetons.cle_publique_hex()
    }
}

/// Lit le secret serveur dans `<chemin>.cle` (32 octets, hexadécimal) ; le
/// génère (aléa système) et l'écrit au premier lancement. La perte de ce
/// fichier rend les secrets TOTP en base indéchiffrables et change la clé des
/// jetons applicatifs — le sauvegarder avec la base.
fn secret_serveur_fichier(chemin_base: &Path) -> Result<Vec<u8>, AccountError> {
    let mut nom = chemin_base.as_os_str().to_owned();
    nom.push(".cle");
    let chemin_cle = std::path::PathBuf::from(nom);
    match std::fs::read_to_string(&chemin_cle) {
        Ok(contenu) => storage::hex_vers_octets(contenu.trim()).ok_or_else(|| {
            AccountError::Stockage(format!(
                "fichier de clé {} illisible (hexadécimal attendu)",
                chemin_cle.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut secret = [0u8; 32];
            OsRng.fill_bytes(&mut secret);
            std::fs::write(&chemin_cle, storage::octets_vers_hex(&secret))
                .map_err(|e| AccountError::Stockage(format!("écriture du fichier de clé : {e}")))?;
            println!(
                "nd-accounts : secret serveur généré dans {} (à sauvegarder avec la base)",
                chemin_cle.display()
            );
            Ok(secret.to_vec())
        }
        Err(e) => Err(AccountError::Stockage(format!(
            "lecture du fichier de clé {} : {e}",
            chemin_cle.display()
        ))),
    }
}

/// Temps Unix courant en secondes (0 si l'horloge est antérieure à l'époque).
pub(crate) fn unix_maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Jeton opaque : 32 octets d'aléa système, encodés en hexadécimal.
fn jeton_aleatoire() -> String {
    use fmt::Write as _;
    let mut octets = [0u8; 32];
    OsRng.fill_bytes(&mut octets);
    let mut s = String::with_capacity(64);
    for o in octets {
        let _ = write!(s, "{o:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Protocole (trames u32 BE + charge utile, comme nd-signaling)
// ---------------------------------------------------------------------------

/// Requêtes du protocole (préfixées d'un octet d'étiquette). Couvre les flux
/// jadis inatteignables par le réseau : **2FA complet** (`Login` →
/// [`Response::DeuxFacteursRequis`] → `LoginDeuxFacteurs`), **OIDC**
/// (`DemarrerOidc` → URL d'autorisation ; `RappelOidc` avec le `state` et le
/// code renvoyés par le fournisseur), **jetons applicatifs** (`EmettreJeton`,
/// `ClePubliqueJetons` pour nd-api) et **licences** (`AttribuerPlan` — auto-
/// service documenté, en attendant un rôle admin/facturation —,
/// `ConsulterLicence`).
enum Request {
    /// 1 — Création de compte.
    Register { email: String, password: String },
    /// 2 — Connexion (peut répondre `DeuxFacteursRequis`).
    Login { email: String, password: String },
    /// 3 — Connexion avec second facteur TOTP (validation du challenge).
    LoginDeuxFacteurs {
        email: String,
        password: String,
        code: String,
    },
    /// 4 — Activation de la 2FA (mot de passe exigé ; refusée si déjà active).
    ActiverDeuxFacteurs { email: String, password: String },
    /// 5 — Démarrage du flux OIDC : renvoie l'URL d'autorisation et le state.
    DemarrerOidc,
    /// 6 — Rappel OIDC : state + code d'autorisation → session locale.
    RappelOidc { state: String, code: String },
    /// 7 — Échange d'un jeton de session contre un jeton applicatif Ed25519.
    EmettreJeton { session: String },
    /// 8 — Clé publique Ed25519 des jetons applicatifs (pour nd-api).
    ClePubliqueJetons,
    /// 9 — Attribution d'un plan au compte de la session.
    AttribuerPlan { session: String, plan: String },
    /// 10 — Licence du compte de la session (plan + quota).
    ConsulterLicence { session: String },
}

/// Réponses du protocole (préfixées d'un octet d'étiquette).
enum Response {
    /// 0 — Succès sans donnée.
    Ok,
    /// 1 — Jeton de session opaque (connexion réussie).
    Token { jeton: String },
    /// 2 — Refus ou échec, avec message lisible.
    Erreur { message: String },
    /// 3 — Challenge 2FA : identifiants corrects, code TOTP attendu
    /// (répondre par `LoginDeuxFacteurs`).
    DeuxFacteursRequis,
    /// 4 — Secret TOTP fraîchement activé (hexadécimal ; à présenter une
    /// seule fois, p. ex. en QR code `otpauth://`).
    SecretTotp { secret_hex: String },
    /// 5 — URL d'autorisation OIDC à ouvrir dans le navigateur + state.
    AutorisationOidc { url: String, state: String },
    /// 6 — Jeton applicatif signé Ed25519 (format : doc du module [`jeton`]).
    JetonApplicatif { jeton: String },
    /// 7 — Clé publique Ed25519 (32 octets, hexadécimal).
    ClePublique { hex: String },
    /// 8 — Licence : nom du plan et quota de sessions (0 = illimité).
    Licence { plan: String, max_sessions: u32 },
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn read_u8(d: &[u8], p: &mut usize) -> Option<u8> {
    let v = *d.get(*p)?;
    *p += 1;
    Some(v)
}

fn read_u32(d: &[u8], p: &mut usize) -> Option<u32> {
    let v = u32::from_be_bytes(d.get(*p..*p + 4)?.try_into().ok()?);
    *p += 4;
    Some(v)
}

fn read_string(d: &[u8], p: &mut usize) -> Option<String> {
    let len = read_u32(d, p)? as usize;
    let s = String::from_utf8(d.get(*p..*p + len)?.to_vec()).ok()?;
    *p += len;
    Some(s)
}

impl Request {
    /// Sérialisation côté client : utilisée par les tests (le serveur désérialise).
    #[cfg_attr(not(test), allow(dead_code))]
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::Register { email, password } => {
                out.push(1);
                put_bytes(&mut out, email.as_bytes());
                put_bytes(&mut out, password.as_bytes());
            }
            Request::Login { email, password } => {
                out.push(2);
                put_bytes(&mut out, email.as_bytes());
                put_bytes(&mut out, password.as_bytes());
            }
            Request::LoginDeuxFacteurs {
                email,
                password,
                code,
            } => {
                out.push(3);
                put_bytes(&mut out, email.as_bytes());
                put_bytes(&mut out, password.as_bytes());
                put_bytes(&mut out, code.as_bytes());
            }
            Request::ActiverDeuxFacteurs { email, password } => {
                out.push(4);
                put_bytes(&mut out, email.as_bytes());
                put_bytes(&mut out, password.as_bytes());
            }
            Request::DemarrerOidc => out.push(5),
            Request::RappelOidc { state, code } => {
                out.push(6);
                put_bytes(&mut out, state.as_bytes());
                put_bytes(&mut out, code.as_bytes());
            }
            Request::EmettreJeton { session } => {
                out.push(7);
                put_bytes(&mut out, session.as_bytes());
            }
            Request::ClePubliqueJetons => out.push(8),
            Request::AttribuerPlan { session, plan } => {
                out.push(9);
                put_bytes(&mut out, session.as_bytes());
                put_bytes(&mut out, plan.as_bytes());
            }
            Request::ConsulterLicence { session } => {
                out.push(10);
                put_bytes(&mut out, session.as_bytes());
            }
        }
        out
    }

    fn from_bytes(d: &[u8]) -> Option<Request> {
        let mut p = 0;
        let requete = match read_u8(d, &mut p)? {
            1 => Request::Register {
                email: read_string(d, &mut p)?,
                password: read_string(d, &mut p)?,
            },
            2 => Request::Login {
                email: read_string(d, &mut p)?,
                password: read_string(d, &mut p)?,
            },
            3 => Request::LoginDeuxFacteurs {
                email: read_string(d, &mut p)?,
                password: read_string(d, &mut p)?,
                code: read_string(d, &mut p)?,
            },
            4 => Request::ActiverDeuxFacteurs {
                email: read_string(d, &mut p)?,
                password: read_string(d, &mut p)?,
            },
            5 => Request::DemarrerOidc,
            6 => Request::RappelOidc {
                state: read_string(d, &mut p)?,
                code: read_string(d, &mut p)?,
            },
            7 => Request::EmettreJeton {
                session: read_string(d, &mut p)?,
            },
            8 => Request::ClePubliqueJetons,
            9 => Request::AttribuerPlan {
                session: read_string(d, &mut p)?,
                plan: read_string(d, &mut p)?,
            },
            10 => Request::ConsulterLicence {
                session: read_string(d, &mut p)?,
            },
            _ => return None,
        };
        // Toute la trame doit avoir été consommée (pas d'octets orphelins).
        (p == d.len()).then_some(requete)
    }
}

impl Response {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::Ok => out.push(0),
            Response::Token { jeton } => {
                out.push(1);
                put_bytes(&mut out, jeton.as_bytes());
            }
            Response::Erreur { message } => {
                out.push(2);
                put_bytes(&mut out, message.as_bytes());
            }
            Response::DeuxFacteursRequis => out.push(3),
            Response::SecretTotp { secret_hex } => {
                out.push(4);
                put_bytes(&mut out, secret_hex.as_bytes());
            }
            Response::AutorisationOidc { url, state } => {
                out.push(5);
                put_bytes(&mut out, url.as_bytes());
                put_bytes(&mut out, state.as_bytes());
            }
            Response::JetonApplicatif { jeton } => {
                out.push(6);
                put_bytes(&mut out, jeton.as_bytes());
            }
            Response::ClePublique { hex } => {
                out.push(7);
                put_bytes(&mut out, hex.as_bytes());
            }
            Response::Licence { plan, max_sessions } => {
                out.push(8);
                put_bytes(&mut out, plan.as_bytes());
                out.extend_from_slice(&max_sessions.to_be_bytes());
            }
        }
        out
    }

    /// Désérialisation côté client : utilisée par les tests (le serveur sérialise).
    #[cfg_attr(not(test), allow(dead_code))]
    fn from_bytes(d: &[u8]) -> Option<Response> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            0 => Some(Response::Ok),
            1 => Some(Response::Token {
                jeton: read_string(d, &mut p)?,
            }),
            2 => Some(Response::Erreur {
                message: read_string(d, &mut p)?,
            }),
            3 => Some(Response::DeuxFacteursRequis),
            4 => Some(Response::SecretTotp {
                secret_hex: read_string(d, &mut p)?,
            }),
            5 => Some(Response::AutorisationOidc {
                url: read_string(d, &mut p)?,
                state: read_string(d, &mut p)?,
            }),
            6 => Some(Response::JetonApplicatif {
                jeton: read_string(d, &mut p)?,
            }),
            7 => Some(Response::ClePublique {
                hex: read_string(d, &mut p)?,
            }),
            8 => Some(Response::Licence {
                plan: read_string(d, &mut p)?,
                max_sessions: read_u32(d, &mut p)?,
            }),
            _ => None,
        }
    }
}

/// Trame : préfixe de longueur (u32 BE) + charge utile.
fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)
}

fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 1 << 16 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "trame trop grande",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Serveur
// ---------------------------------------------------------------------------

/// Service réseau : magasin de comptes + flux OIDC optionnel (fournisseur
/// configuré). Clonable : les clones partagent magasin et flux.
#[derive(Clone)]
pub struct Service {
    store: AccountStore,
    oidc: Option<Arc<oidc::FluxOidc>>,
}

impl Service {
    /// Service sans fédération OIDC (requêtes OIDC → « non configuré »).
    #[must_use]
    pub fn nouveau(store: AccountStore) -> Self {
        Self { store, oidc: None }
    }

    /// Service avec fédération OIDC active.
    #[must_use]
    pub fn avec_oidc(store: AccountStore, flux: Arc<oidc::FluxOidc>) -> Self {
        Self {
            store,
            oidc: Some(flux),
        }
    }

    /// Traite une requête et produit la réponse (cœur du protocole).
    fn traiter(&self, requete: Request) -> Response {
        match requete {
            Request::Register { email, password } => selon(
                self.store
                    .register(&email, &password)
                    .map(|()| Response::Ok),
            ),
            // Flux 2FA, étape 1 : identifiants corrects mais compte protégé →
            // challenge TOTP explicite (pas une erreur).
            Request::Login { email, password } => match self.store.login(&email, &password) {
                Ok(jeton) => Response::Token { jeton },
                Err(AccountError::DeuxFacteursRequis) => Response::DeuxFacteursRequis,
                Err(e) => erreur(&e),
            },
            // Flux 2FA, étape 2 : validation du code TOTP.
            Request::LoginDeuxFacteurs {
                email,
                password,
                code,
            } => selon(
                self.store
                    .login_2fa(&email, &password, &code)
                    .map(|jeton| Response::Token { jeton }),
            ),
            Request::ActiverDeuxFacteurs { email, password } => selon(
                self.store
                    .activer_2fa_reseau(&email, &password)
                    .map(|secret| Response::SecretTotp {
                        secret_hex: storage::octets_vers_hex(&secret),
                    }),
            ),
            Request::DemarrerOidc => match &self.oidc {
                Some(flux) => {
                    let (url, state) = flux.demarrer(unix_maintenant());
                    Response::AutorisationOidc { url, state }
                }
                None => Response::Erreur {
                    message: "fédération OIDC non configurée".into(),
                },
            },
            Request::RappelOidc { state, code } => match &self.oidc {
                Some(flux) => self.rappel_oidc(flux, &state, &code),
                None => Response::Erreur {
                    message: "fédération OIDC non configurée".into(),
                },
            },
            Request::EmettreJeton { session } => selon(
                self.store
                    .emettre_jeton_applicatif(&session)
                    .map(|jeton| Response::JetonApplicatif { jeton }),
            ),
            Request::ClePubliqueJetons => Response::ClePublique {
                hex: self.store.cle_publique_jetons(),
            },
            Request::AttribuerPlan { session, plan } => {
                let Some(email) = self.store.verify_token(&session) else {
                    return erreur(&AccountError::SessionInvalide);
                };
                let Some(plan) = Plan::depuis_nom(&plan) else {
                    return Response::Erreur {
                        message: format!("plan inconnu : {plan}"),
                    };
                };
                selon(
                    self.store
                        .attribuer_plan(&email, plan)
                        .map(|()| Response::Ok),
                )
            }
            Request::ConsulterLicence { session } => {
                let Some(email) = self.store.verify_token(&session) else {
                    return erreur(&AccountError::SessionInvalide);
                };
                selon(self.store.plan_de(&email).map(|plan| Response::Licence {
                    plan: plan.nom().to_string(),
                    max_sessions: plan.max_sessions().unwrap_or(0),
                }))
            }
        }
    }

    /// Rappel OIDC : valide le flux ([`oidc::FluxOidc::rappel`]) puis rattache
    /// le sujet fédéré (`iss|sub`) à un compte local — déjà lié, ou lié à la
    /// volée par e-mail vérifié si le fournisseur est configuré de confiance
    /// ([`oidc::OptionsFlux::lier_par_email`]).
    fn rappel_oidc(&self, flux: &oidc::FluxOidc, state: &str, code: &str) -> Response {
        let claims = match flux.rappel(state, code, unix_maintenant()) {
            Ok(claims) => claims,
            Err(e) => {
                return Response::Erreur {
                    message: e.to_string(),
                }
            }
        };
        let sujet = format!("{}|{}", claims.emetteur, claims.sujet);
        // Sujet déjà lié : connexion directe.
        if self.store.oidc_account(&sujet).is_some() {
            return selon(
                self.store
                    .login_oidc(&sujet)
                    .map(|jeton| Response::Token { jeton }),
            );
        }
        // Sinon, liaison automatique par e-mail vérifié — si configurée.
        if flux.lier_par_email() {
            if let Some(email) = &claims.email {
                if let Err(e) = self.store.link_oidc(email, &sujet) {
                    return erreur(&e);
                }
                return selon(
                    self.store
                        .login_oidc(&sujet)
                        .map(|jeton| Response::Token { jeton }),
                );
            }
        }
        Response::Erreur {
            message: "identité fédérée non liée à un compte local".into(),
        }
    }
}

/// Convertit un résultat métier en réponse (l'erreur devient un message).
fn selon(resultat: Result<Response, AccountError>) -> Response {
    resultat.unwrap_or_else(|e| erreur(&e))
}

/// Réponse d'erreur à partir d'une erreur métier.
fn erreur(e: &AccountError) -> Response {
    Response::Erreur {
        message: e.to_string(),
    }
}

/// Boucle de service (bloquante, un thread par connexion, une requête par connexion).
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
pub fn serve(listener: TcpListener, service: Service) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let service = service.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &service);
        });
    }
    Ok(())
}

fn handle_conn(mut stream: TcpStream, service: &Service) -> std::io::Result<()> {
    let req_bytes = read_frame(&mut stream)?;
    let resp = match Request::from_bytes(&req_bytes) {
        Some(requete) => service.traiter(requete),
        None => Response::Erreur {
            message: "requête invalide".into(),
        },
    };
    write_frame(&mut stream, &resp.to_bytes())
}

/// Construit le flux OIDC depuis l'environnement (`ND_OIDC_*`), s'il est
/// configuré (au minimum : issuer, endpoints, client_id, redirect_uri).
fn flux_oidc_depuis_env() -> Option<Arc<oidc::FluxOidc>> {
    let variable = |nom: &str| std::env::var(nom).ok().filter(|v| !v.trim().is_empty());
    let config = oidc::OidcConfig {
        issuer: variable("ND_OIDC_ISSUER")?,
        authorization_endpoint: variable("ND_OIDC_AUTH_ENDPOINT")?,
        token_endpoint: variable("ND_OIDC_TOKEN_ENDPOINT")?,
        jwks_uri: variable("ND_OIDC_JWKS_URI")?,
        client_id: variable("ND_OIDC_CLIENT_ID")?,
        redirect_uri: variable("ND_OIDC_REDIRECT_URI")?,
        scopes: vec!["openid".into(), "email".into()],
    };
    let options = oidc::OptionsFlux {
        lier_par_email: variable("ND_OIDC_LIER_PAR_EMAIL").is_some_and(|v| v == "1"),
        ..oidc::OptionsFlux::default()
    };
    Some(Arc::new(oidc::FluxOidc::new(config, options)))
}

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    // Second argument optionnel : chemin de la base de comptes (persistance).
    let chemin = std::env::args().nth(2);
    let (store, mode) = match &chemin {
        Some(chemin) => {
            // Secret serveur : variable d'environnement (hexadécimal), sinon
            // fichier `<base>.cle` auto-généré.
            let store = match std::env::var("ND_ACCOUNTS_SECRET")
                .ok()
                .and_then(|hex| storage::hex_vers_octets(hex.trim()))
            {
                Some(secret) => {
                    AccountStore::ouvrir_avec_secret(chemin, &secret, Argon2::default())
                }
                None => AccountStore::open(chemin),
            }
            .map_err(std::io::Error::other)?;
            (store, format!("comptes persistés dans {chemin}"))
        }
        None => (AccountStore::new(), "comptes en mémoire".to_string()),
    };
    let service = match flux_oidc_depuis_env() {
        Some(flux) => Service::avec_oidc(store, flux),
        None => Service::nouveau(store),
    };
    let listener = TcpListener::bind(&addr)?;
    println!(
        "nd-accounts (NovaDesk protocole v{}) en écoute sur {} — {mode}",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    serve(listener, service)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::FichierTemp;
    use argon2::{Algorithm, Params, Version};

    /// Paramètres Argon2id légers (tests rapides en debug).
    fn argon2_leger() -> Argon2<'static> {
        let params = Params::new(Params::MIN_M_COST, 1, 1, None).expect("params argon2");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }

    /// Magasin en mémoire avec des paramètres Argon2id légers.
    fn store_test() -> AccountStore {
        AccountStore::with_argon2(argon2_leger())
    }

    /// Magasin persistant (fichier donné) avec des paramètres légers.
    fn store_persistant(chemin: &Path) -> AccountStore {
        AccountStore::open_with_argon2(chemin, argon2_leger()).expect("ouverture du magasin")
    }

    #[test]
    fn register_puis_login_ok() {
        let store = store_test();
        store
            .register("alice@example.com", "s3cret!")
            .expect("register");
        let jeton = store.login("alice@example.com", "s3cret!").expect("login");
        assert_eq!(jeton.len(), 64, "jeton = 32 octets en hexadécimal");
        assert_eq!(
            store.verify_token(&jeton).as_deref(),
            Some("alice@example.com")
        );
    }

    #[test]
    fn login_mauvais_mot_de_passe_refuse() {
        let store = store_test();
        store
            .register("bob@example.com", "bon-mdp")
            .expect("register");
        assert_eq!(
            store.login("bob@example.com", "mauvais-mdp"),
            Err(AccountError::IdentifiantsInvalides)
        );
        // E-mail inconnu : même erreur, indistincte.
        assert_eq!(
            store.login("inconnu@example.com", "bon-mdp"),
            Err(AccountError::IdentifiantsInvalides)
        );
    }

    #[test]
    fn register_email_deja_utilise_refuse() {
        let store = store_test();
        store
            .register("carol@example.com", "mdp1")
            .expect("register");
        assert_eq!(
            store.register("carol@example.com", "mdp2"),
            Err(AccountError::EmailDejaUtilise)
        );
        // Entrées vides refusées.
        assert_eq!(store.register("", "mdp"), Err(AccountError::EntreeInvalide));
        assert_eq!(
            store.register("d@example.com", ""),
            Err(AccountError::EntreeInvalide)
        );
    }

    #[test]
    fn hachage_est_argon2id_et_verifiable() {
        let store = store_test();
        store.register("eve@example.com", "mdp").expect("register");
        let phc = store
            .etat
            .lock()
            .unwrap()
            .comptes
            .get("eve@example.com")
            .cloned()
            .expect("hachage stocké");
        assert!(phc.starts_with("$argon2id$"), "format PHC Argon2id : {phc}");
        // Vérification directe via l'API password-hash.
        let hachage = PasswordHash::new(&phc).expect("PHC valide");
        assert!(Argon2::default().verify_password(b"mdp", &hachage).is_ok());
        assert!(Argon2::default()
            .verify_password(b"autre", &hachage)
            .is_err());
    }

    #[test]
    fn jetons_uniques() {
        let store = store_test();
        store.register("fred@example.com", "mdp").expect("register");
        let j1 = store.login("fred@example.com", "mdp").expect("login 1");
        let j2 = store.login("fred@example.com", "mdp").expect("login 2");
        assert_ne!(j1, j2, "chaque session reçoit un jeton distinct");
    }

    #[test]
    fn enable_2fa_compte_inconnu_refuse() {
        let store = store_test();
        assert_eq!(
            store.enable_2fa("personne@example.com"),
            Err(AccountError::CompteInconnu)
        );
    }

    #[test]
    fn deux_facteurs_actives_login_exige_le_code() {
        let store = store_test();
        store.register("gina@example.com", "mdp").expect("register");

        // Avant activation : login simple OK.
        assert!(store.login("gina@example.com", "mdp").is_ok());

        let secret = store.enable_2fa("gina@example.com").expect("enable_2fa");
        assert_eq!(secret.len(), 20, "secret TOTP de 20 octets");

        // Après activation : `login` seul est refusé, le mot de passe reste
        // vérifié en premier (erreur indistincte si faux).
        assert_eq!(
            store.login("gina@example.com", "mdp"),
            Err(AccountError::DeuxFacteursRequis)
        );
        assert_eq!(
            store.login("gina@example.com", "mauvais"),
            Err(AccountError::IdentifiantsInvalides)
        );

        // Bon mot de passe + code du pas courant : session ouverte.
        let code = totp::totp_at(&secret, unix_maintenant());
        let jeton = store
            .login_2fa("gina@example.com", "mdp", &code)
            .expect("login_2fa");
        assert_eq!(
            store.verify_token(&jeton).as_deref(),
            Some("gina@example.com")
        );
    }

    #[test]
    fn login_2fa_mauvais_code_refuse() {
        let store = store_test();
        store.register("hugo@example.com", "mdp").expect("register");
        let secret = store.enable_2fa("hugo@example.com").expect("enable_2fa");

        // Code garanti hors fenêtre : différent des codes des pas -2..=+2
        // (l'horloge peut avancer d'un pas pendant le test). Au plus 5 codes
        // valides : parmi dix candidats, l'un est forcément faux.
        let maintenant = unix_maintenant();
        let valides: Vec<String> = (-2i64..=2)
            .map(|k| {
                let t = maintenant.saturating_add_signed(k * totp::PERIODE_S as i64);
                totp::totp_at(&secret, t)
            })
            .collect();
        let faux = (0..10u32)
            .map(|n| format!("{n:06}"))
            .find(|c| !valides.contains(c))
            .expect("au moins un candidat hors fenêtre");
        assert_eq!(
            store.login_2fa("hugo@example.com", "mdp", &faux),
            Err(AccountError::CodeTotpInvalide)
        );

        // Mauvais mot de passe : refusé avant même le contrôle TOTP.
        let code = totp::totp_at(&secret, maintenant);
        assert_eq!(
            store.login_2fa("hugo@example.com", "mauvais", &code),
            Err(AccountError::IdentifiantsInvalides)
        );

        // Compte sans 2FA : `login_2fa` le signale explicitement.
        store.register("iris@example.com", "mdp").expect("register");
        assert_eq!(
            store.login_2fa("iris@example.com", "mdp", "123456"),
            Err(AccountError::DeuxFacteursNonActives)
        );
    }

    #[test]
    fn login_2fa_fenetre_de_tolerance_acceptee() {
        let store = store_test();
        store.register("jean@example.com", "mdp").expect("register");
        let secret = store.enable_2fa("jean@example.com").expect("enable_2fa");
        // Code du pas *suivant* : accepté grâce à la fenêtre ±1 (et robuste
        // même si l'horloge franchit un pas pendant le test).
        let code = totp::totp_at(&secret, unix_maintenant() + totp::PERIODE_S);
        assert!(store.login_2fa("jean@example.com", "mdp", &code).is_ok());
    }

    #[test]
    fn audit_consigne_les_evenements_de_compte() {
        let journal = audit::AuditLog::new();
        let store = store_test().with_audit(journal.clone());

        store.register("kim@example.com", "mdp").expect("register");
        store.login("kim@example.com", "mdp").expect("login");
        assert_eq!(
            store.login("kim@example.com", "mauvais"),
            Err(AccountError::IdentifiantsInvalides)
        );
        let secret = store.enable_2fa("kim@example.com").expect("enable_2fa");

        // Compte 2FA : `login` seul demande le second facteur — ce n'est pas
        // un échec, rien de plus n'est consigné.
        assert_eq!(
            store.login("kim@example.com", "mdp"),
            Err(AccountError::DeuxFacteursRequis)
        );

        let code = totp::totp_at(&secret, unix_maintenant());
        store
            .login_2fa("kim@example.com", "mdp", &code)
            .expect("login_2fa");

        let evts = journal.for_account("kim@example.com");
        assert_eq!(evts.len(), 5, "création, succès, échec, 2FA, succès 2FA");
        assert!(matches!(
            evts[0].event,
            audit::AuditEvent::AccountCreated { .. }
        ));
        assert!(matches!(
            evts[1].event,
            audit::AuditEvent::LoginSuccess { .. }
        ));
        assert!(matches!(
            evts[2].event,
            audit::AuditEvent::LoginFailure { .. }
        ));
        assert!(matches!(
            evts[3].event,
            audit::AuditEvent::TwoFactorEnabled { .. }
        ));
        assert!(matches!(
            evts[4].event,
            audit::AuditEvent::LoginSuccess { .. }
        ));
        assert_eq!(journal.count(), 5);

        // E-mail inconnu : l'échec est tout de même consigné (journal d'accès).
        assert_eq!(
            store.login("inconnu@example.com", "mdp"),
            Err(AccountError::IdentifiantsInvalides)
        );
        assert_eq!(journal.for_account("inconnu@example.com").len(), 1);
    }

    // -- Persistance (module `storage`) --------------------------------------

    #[test]
    fn persistance_register_puis_reouverture() {
        let tmp = FichierTemp::nouveau("reouverture");
        {
            let store = store_persistant(tmp.chemin());
            store
                .register("alice@example.com", "s3cret!")
                .expect("register");
        } // le magasin est refermé : seul le fichier survit

        {
            let store = store_persistant(tmp.chemin());
            let jeton = store
                .login("alice@example.com", "s3cret!")
                .expect("login après réouverture");
            assert_eq!(
                store.verify_token(&jeton).as_deref(),
                Some("alice@example.com")
            );
            // Mauvais mot de passe : refusé sur le magasin rechargé.
            assert_eq!(
                store.login("alice@example.com", "mauvais"),
                Err(AccountError::IdentifiantsInvalides)
            );
            // L'unicité d'e-mail est elle aussi rechargée.
            assert_eq!(
                store.register("alice@example.com", "autre"),
                Err(AccountError::EmailDejaUtilise)
            );

            // Une inscription après réouverture est persistée à son tour.
            store.register("bob@example.com", "mdp2").expect("register");
        } // la base (verrou exclusif redb) doit être refermée avant réouverture

        let store = store_persistant(tmp.chemin());
        assert!(store.login("bob@example.com", "mdp2").is_ok());
    }

    #[test]
    fn persistance_2fa_survit_a_la_reouverture() {
        let tmp = FichierTemp::nouveau("2fa");
        let secret = {
            let store = store_persistant(tmp.chemin());
            store.register("gina@example.com", "mdp").expect("register");
            store.enable_2fa("gina@example.com").expect("enable_2fa")
        };

        let store = store_persistant(tmp.chemin());
        // La 2FA est toujours exigée après redémarrage...
        assert_eq!(
            store.login("gina@example.com", "mdp"),
            Err(AccountError::DeuxFacteursRequis)
        );
        // ... et le secret rechargé accepte les codes TOTP courants.
        let code = totp::totp_at(&secret, unix_maintenant());
        let jeton = store
            .login_2fa("gina@example.com", "mdp", &code)
            .expect("login_2fa après réouverture");
        assert_eq!(
            store.verify_token(&jeton).as_deref(),
            Some("gina@example.com")
        );
    }

    #[test]
    fn persistance_sessions_volatiles() {
        let tmp = FichierTemp::nouveau("sessions");
        let jeton = {
            let store = store_persistant(tmp.chemin());
            store.register("carl@example.com", "mdp").expect("register");
            store.login("carl@example.com", "mdp").expect("login")
        };
        // Un redémarrage invalide les sessions ouvertes (par conception).
        let store = store_persistant(tmp.chemin());
        assert_eq!(store.verify_token(&jeton), None);
    }

    #[test]
    fn persistance_fichier_sans_mot_de_passe_en_clair() {
        let tmp = FichierTemp::nouveau("sans-mdp");
        {
            let store = store_persistant(tmp.chemin());
            store
                .register("dan@example.com", "MotDePasseTresSecret123")
                .expect("register");
        } // la base est refermée : son fichier n'est plus verrouillé

        // La base est binaire : on cherche les motifs dans les octets bruts.
        let brut = std::fs::read(tmp.chemin()).expect("base écrite");
        let contient = |motif: &[u8]| brut.windows(motif.len()).any(|fenetre| fenetre == motif);
        assert!(
            !contient(b"MotDePasseTresSecret123"),
            "le mot de passe en clair ne doit jamais toucher le disque"
        );
        assert!(
            contient(b"$argon2id$"),
            "seul le hachage PHC Argon2id est persisté"
        );
    }

    #[test]
    fn persistance_secret_totp_chiffre_en_base() {
        let tmp = FichierTemp::nouveau("totp-chiffre");
        let secret = {
            let store = store_persistant(tmp.chemin());
            store.register("gala@example.com", "mdp").expect("register");
            store.enable_2fa("gala@example.com").expect("enable_2fa")
        };
        // Ni les octets bruts du secret, ni son encodage hexadécimal (l'ancien
        // format en clair) n'apparaissent dans le fichier de la base.
        let brut = std::fs::read(tmp.chemin()).expect("base écrite");
        assert!(
            !brut.windows(secret.len()).any(|f| f == secret.as_slice()),
            "le secret TOTP ne doit jamais toucher le disque en clair"
        );
        let hex = storage::octets_vers_hex(&secret);
        assert!(
            !brut.windows(hex.len()).any(|f| f == hex.as_bytes()),
            "pas d'hexadécimal en clair non plus"
        );
        // Le secret rechargé (déchiffré) reste utilisable pour un login 2FA.
        let store = store_persistant(tmp.chemin());
        let code = totp::totp_at(&secret, unix_maintenant());
        assert!(store.login_2fa("gala@example.com", "mdp", &code).is_ok());
    }

    #[test]
    fn persistance_import_du_json_herite() {
        let tmp = FichierTemp::nouveau("import-herite");
        // Prépare un fichier de l'ancien format avec un vrai hachage et un
        // secret TOTP en clair (hexadécimal), comme l'aurait écrit l'ancien
        // service.
        let (phc, secret) = {
            let ancien = store_test();
            ancien
                .register("vera@example.com", "mdp")
                .expect("register");
            let phc = ancien
                .etat
                .lock()
                .unwrap()
                .comptes
                .get("vera@example.com")
                .cloned()
                .expect("hachage");
            (phc, totp::generate_totp_secret())
        };
        let herite = storage::DonneesPersistees {
            version: storage::VERSION_FORMAT,
            comptes: HashMap::from([("vera@example.com".to_string(), phc)]),
            secrets_2fa: HashMap::from([(
                "vera@example.com".to_string(),
                storage::octets_vers_hex(&secret),
            )]),
            liens_oidc: HashMap::from([(
                "https://idp.example|sub-vera".to_string(),
                "vera@example.com".to_string(),
            )]),
        };
        std::fs::write(
            tmp.chemin_json(),
            serde_json::to_string(&herite).expect("sérialisation"),
        )
        .expect("écriture du JSON hérité");

        // L'ouverture de la base vierge importe tout : mot de passe, 2FA
        // (secret rechiffré) et lien OIDC fonctionnent immédiatement.
        let store = store_persistant(tmp.chemin());
        assert_eq!(
            store.login("vera@example.com", "mdp"),
            Err(AccountError::DeuxFacteursRequis),
            "compte et 2FA importés"
        );
        let code = totp::totp_at(&secret, unix_maintenant());
        assert!(store.login_2fa("vera@example.com", "mdp", &code).is_ok());
        assert!(store.login_oidc("https://idp.example|sub-vera").is_ok());
    }

    // -- Fédération OIDC (`link_oidc` / `login_oidc`) -------------------------

    #[test]
    fn link_oidc_regles_de_liaison() {
        let store = store_test(); // fonctionne aussi sans stockage attaché
        assert_eq!(
            store.link_oidc("x@example.com", "https://idp|sub-1"),
            Err(AccountError::CompteInconnu)
        );

        store.register("x@example.com", "mdp").expect("register");
        store.register("y@example.com", "mdp").expect("register");
        store
            .link_oidc("x@example.com", "https://idp|sub-1")
            .expect("premier lien");
        // Relier le même sujet au même compte : idempotent.
        store
            .link_oidc("x@example.com", "https://idp|sub-1")
            .expect("lien idempotent");
        // Le même sujet ne peut pas pointer un autre compte.
        assert_eq!(
            store.link_oidc("y@example.com", "https://idp|sub-1"),
            Err(AccountError::SujetOidcDejaLie)
        );
        // Entrées vides refusées.
        assert_eq!(
            store.link_oidc("", "https://idp|sub-2"),
            Err(AccountError::EntreeInvalide)
        );
        assert_eq!(
            store.link_oidc("x@example.com", "  "),
            Err(AccountError::EntreeInvalide)
        );

        assert_eq!(
            store.oidc_account("https://idp|sub-1").as_deref(),
            Some("x@example.com")
        );
        assert_eq!(store.oidc_account("https://idp|inconnu"), None);
    }

    #[test]
    fn login_oidc_ouvre_une_session_pour_un_sujet_lie() {
        let journal = audit::AuditLog::new();
        let store = store_test().with_audit(journal.clone());
        store.register("zoe@example.com", "mdp").expect("register");
        store
            .link_oidc("zoe@example.com", "https://idp|sub-zoe")
            .expect("lien");

        let jeton = store.login_oidc("https://idp|sub-zoe").expect("login_oidc");
        assert_eq!(
            store.verify_token(&jeton).as_deref(),
            Some("zoe@example.com")
        );
        // La connexion fédérée est consignée comme une connexion réussie.
        assert!(matches!(
            journal
                .for_account("zoe@example.com")
                .last()
                .expect("évt")
                .event,
            audit::AuditEvent::LoginSuccess { .. }
        ));

        // Sujet jamais lié : refus.
        assert_eq!(
            store.login_oidc("https://idp|autre"),
            Err(AccountError::CompteInconnu)
        );
    }

    #[test]
    fn persistance_liens_oidc_survivent_a_la_reouverture() {
        let tmp = FichierTemp::nouveau("oidc");
        {
            let store = store_persistant(tmp.chemin());
            store
                .register("carol@example.com", "mdp")
                .expect("register");
            store
                .link_oidc("carol@example.com", "https://idp.example|sub-42")
                .expect("lien");
        }

        let store = store_persistant(tmp.chemin());
        assert_eq!(
            store.oidc_account("https://idp.example|sub-42").as_deref(),
            Some("carol@example.com")
        );
        let jeton = store
            .login_oidc("https://idp.example|sub-42")
            .expect("login_oidc après réouverture");
        assert_eq!(
            store.verify_token(&jeton).as_deref(),
            Some("carol@example.com")
        );
    }

    #[test]
    fn ouverture_fichier_corrompu_refusee() {
        let tmp = FichierTemp::nouveau("ouverture-corrompue");
        std::fs::write(tmp.chemin(), b"{ pas du json ]").expect("écriture");
        let resultat = AccountStore::open_with_argon2(tmp.chemin(), argon2_leger());
        assert!(
            matches!(resultat, Err(AccountError::Stockage(_))),
            "erreur Stockage attendue sur un fichier corrompu"
        );
    }

    #[test]
    fn protocole_aller_retour() {
        let reqs = [
            Request::Register {
                email: "a@b.c".into(),
                password: "mdp".into(),
            },
            Request::Login {
                email: "a@b.c".into(),
                password: "mdp".into(),
            },
            Request::LoginDeuxFacteurs {
                email: "a@b.c".into(),
                password: "mdp".into(),
                code: "123456".into(),
            },
            Request::ActiverDeuxFacteurs {
                email: "a@b.c".into(),
                password: "mdp".into(),
            },
            Request::DemarrerOidc,
            Request::RappelOidc {
                state: "etat".into(),
                code: "code".into(),
            },
            Request::EmettreJeton {
                session: "jeton".into(),
            },
            Request::ClePubliqueJetons,
            Request::AttribuerPlan {
                session: "jeton".into(),
                plan: "pro".into(),
            },
            Request::ConsulterLicence {
                session: "jeton".into(),
            },
        ];
        for r in &reqs {
            assert!(Request::from_bytes(&r.to_bytes()).is_some());
        }
        assert!(Request::from_bytes(&[]).is_none());
        assert!(Request::from_bytes(&[99]).is_none(), "étiquette inconnue");
        // Octets excédentaires après une requête complète : trame refusée.
        let mut trop_long = Request::DemarrerOidc.to_bytes();
        trop_long.push(0);
        assert!(Request::from_bytes(&trop_long).is_none());

        // Réponses : chaque variante fait l'aller-retour.
        let reponses = [
            Response::Ok,
            Response::Token {
                jeton: "abc123".into(),
            },
            Response::Erreur {
                message: "boom".into(),
            },
            Response::DeuxFacteursRequis,
            Response::SecretTotp {
                secret_hex: "01ff".into(),
            },
            Response::AutorisationOidc {
                url: "https://idp/authorize?x=1".into(),
                state: "etat".into(),
            },
            Response::JetonApplicatif {
                jeton: "a.b.c".into(),
            },
            Response::ClePublique { hex: "00ab".into() },
            Response::Licence {
                plan: "pro".into(),
                max_sessions: 10,
            },
        ];
        for reponse in &reponses {
            assert!(Response::from_bytes(&reponse.to_bytes()).is_some());
        }
        match Response::from_bytes(
            &Response::Licence {
                plan: "pro".into(),
                max_sessions: 10,
            }
            .to_bytes(),
        ) {
            Some(Response::Licence { plan, max_sessions }) => {
                assert_eq!(plan, "pro");
                assert_eq!(max_sessions, 10);
            }
            _ => panic!("désérialisation Licence échouée"),
        }
    }

    // -- Licences et jetons applicatifs ---------------------------------------

    #[test]
    fn attribuer_et_consulter_un_plan_persiste() {
        let tmp = FichierTemp::nouveau("licences");
        {
            let store = store_persistant(tmp.chemin());
            store.register("lea@example.com", "mdp").expect("register");
            // Sans attribution : plan Free par défaut.
            assert_eq!(store.plan_de("lea@example.com"), Ok(Plan::Free));
            store
                .attribuer_plan("lea@example.com", Plan::Pro)
                .expect("attribution");
            // Compte inconnu : refus des deux côtés.
            assert_eq!(
                store.attribuer_plan("inconnue@example.com", Plan::Pro),
                Err(AccountError::CompteInconnu)
            );
            assert_eq!(
                store.plan_de("inconnue@example.com"),
                Err(AccountError::CompteInconnu)
            );
        }
        // Le plan survit à la réouverture.
        let store = store_persistant(tmp.chemin());
        assert_eq!(store.plan_de("lea@example.com"), Ok(Plan::Pro));
    }

    #[test]
    fn jeton_applicatif_emis_et_verifiable() {
        let store = store_test();
        store.register("noe@example.com", "mdp").expect("register");
        store
            .attribuer_plan("noe@example.com", Plan::Entreprise)
            .expect("plan");
        let session = store.login("noe@example.com", "mdp").expect("login");

        let jws = store
            .emettre_jeton_applicatif(&session)
            .expect("émission du jeton applicatif");
        // Vérification hors ligne, comme le fera nd-api (lot 07).
        let claims = jeton::verifier_jeton(&jws, &store.cle_publique_jetons(), unix_maintenant())
            .expect("jeton vérifiable avec la clé publique");
        assert_eq!(claims.emetteur, jeton::EMETTEUR);
        assert_eq!(claims.sujet, "noe@example.com");
        assert_eq!(claims.roles, vec!["utilisateur"]);
        assert_eq!(claims.plan, "entreprise");
        assert!(claims.expiration > claims.emis_a);

        // Session inconnue : refus.
        assert_eq!(
            store.emettre_jeton_applicatif("jeton-fantome"),
            Err(AccountError::SessionInvalide)
        );
    }

    #[test]
    fn activer_2fa_reseau_regles_de_securite() {
        let store = store_test();
        store.register("sam@example.com", "mdp").expect("register");
        // Mot de passe faux : refus (et rien n'est activé).
        assert_eq!(
            store.activer_2fa_reseau("sam@example.com", "mauvais"),
            Err(AccountError::IdentifiantsInvalides)
        );
        assert!(store.login("sam@example.com", "mdp").is_ok());
        // Bon mot de passe : activation.
        let secret = store
            .activer_2fa_reseau("sam@example.com", "mdp")
            .expect("activation");
        assert_eq!(secret.len(), 20);
        // Déjà active : le remplacement par le réseau est refusé.
        assert_eq!(
            store.activer_2fa_reseau("sam@example.com", "mdp"),
            Err(AccountError::DeuxFacteursDejaActives)
        );
    }

    // -- Serveur TCP ------------------------------------------------------------

    /// Démarre un serveur sur un port éphémère et rend son adresse.
    fn serveur(service: Service) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("adresse locale");
        std::thread::spawn(move || {
            let _ = serve(listener, service);
        });
        addr
    }

    /// Une requête, une réponse (une connexion par échange, comme le service).
    fn aller_retour(addr: std::net::SocketAddr, req: &Request) -> Response {
        let mut s = TcpStream::connect(addr).expect("connexion");
        write_frame(&mut s, &req.to_bytes()).expect("écriture");
        Response::from_bytes(&read_frame(&mut s).expect("lecture")).expect("réponse")
    }

    #[test]
    fn serveur_tcp_register_login() {
        let addr = serveur(Service::nouveau(store_test()));

        let inscription = aller_retour(
            addr,
            &Request::Register {
                email: "tcp@example.com".into(),
                password: "mdp".into(),
            },
        );
        assert!(matches!(inscription, Response::Ok));

        match aller_retour(
            addr,
            &Request::Login {
                email: "tcp@example.com".into(),
                password: "mdp".into(),
            },
        ) {
            Response::Token { jeton } => assert_eq!(jeton.len(), 64),
            _ => panic!("login TCP : jeton attendu"),
        }
    }

    #[test]
    fn serveur_tcp_flux_2fa_complet() {
        let store = store_test();
        let addr = serveur(Service::nouveau(store.clone()));
        let email = "flux2fa@example.com";

        // Inscription puis activation de la 2FA **par le réseau**.
        assert!(matches!(
            aller_retour(
                addr,
                &Request::Register {
                    email: email.into(),
                    password: "mdp".into(),
                }
            ),
            Response::Ok
        ));
        let secret = match aller_retour(
            addr,
            &Request::ActiverDeuxFacteurs {
                email: email.into(),
                password: "mdp".into(),
            },
        ) {
            Response::SecretTotp { secret_hex } => {
                storage::hex_vers_octets(&secret_hex).expect("secret hexadécimal")
            }
            autre => panic!("secret TOTP attendu, reçu {:?}", autre.to_bytes()),
        };

        // Étape 1 du flux : login → challenge TOTP explicite.
        assert!(matches!(
            aller_retour(
                addr,
                &Request::Login {
                    email: email.into(),
                    password: "mdp".into(),
                }
            ),
            Response::DeuxFacteursRequis
        ));

        // Mauvais code : refus.
        assert!(matches!(
            aller_retour(
                addr,
                &Request::LoginDeuxFacteurs {
                    email: email.into(),
                    password: "mdp".into(),
                    code: "000000".into(),
                }
            ),
            Response::Erreur { .. }
        ));

        // Étape 2 : validation du code TOTP courant → session ouverte.
        let code = totp::totp_at(&secret, unix_maintenant());
        let jeton = match aller_retour(
            addr,
            &Request::LoginDeuxFacteurs {
                email: email.into(),
                password: "mdp".into(),
                code,
            },
        ) {
            Response::Token { jeton } => jeton,
            _ => panic!("login 2FA TCP : jeton attendu"),
        };
        assert_eq!(store.verify_token(&jeton).as_deref(), Some(email));
    }

    #[test]
    fn serveur_tcp_jeton_applicatif_et_licence() {
        let store = store_test();
        let addr = serveur(Service::nouveau(store.clone()));
        store.register("api@example.com", "mdp").expect("register");
        let session = store.login("api@example.com", "mdp").expect("login");

        // Attribution d'un plan par le réseau (session du compte requise).
        assert!(matches!(
            aller_retour(
                addr,
                &Request::AttribuerPlan {
                    session: session.clone(),
                    plan: "pro".into(),
                }
            ),
            Response::Ok
        ));
        // Plan inconnu ou session invalide : refus.
        assert!(matches!(
            aller_retour(
                addr,
                &Request::AttribuerPlan {
                    session: session.clone(),
                    plan: "platine".into(),
                }
            ),
            Response::Erreur { .. }
        ));
        assert!(matches!(
            aller_retour(
                addr,
                &Request::ConsulterLicence {
                    session: "fantome".into(),
                }
            ),
            Response::Erreur { .. }
        ));
        match aller_retour(
            addr,
            &Request::ConsulterLicence {
                session: session.clone(),
            },
        ) {
            Response::Licence { plan, max_sessions } => {
                assert_eq!(plan, "pro");
                assert_eq!(max_sessions, 10);
            }
            _ => panic!("licence attendue"),
        }

        // Jeton applicatif émis par le réseau, vérifié avec la clé publique
        // récupérée par le réseau — exactement le chemin de nd-api.
        let cle = match aller_retour(addr, &Request::ClePubliqueJetons) {
            Response::ClePublique { hex } => hex,
            _ => panic!("clé publique attendue"),
        };
        let jws = match aller_retour(addr, &Request::EmettreJeton { session }) {
            Response::JetonApplicatif { jeton } => jeton,
            _ => panic!("jeton applicatif attendu"),
        };
        let claims =
            jeton::verifier_jeton(&jws, &cle, unix_maintenant()).expect("jeton vérifiable");
        assert_eq!(claims.sujet, "api@example.com");
        assert_eq!(claims.plan, "pro");
    }

    #[test]
    fn serveur_tcp_flux_oidc_complet() {
        use crate::jwks::test_idp;

        // Fournisseur simulé (clés RFC 7515) + flux OIDC avec liaison par
        // e-mail vérifié (fournisseur de confiance).
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let config = oidc::OidcConfig {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: idp.token_endpoint(),
            jwks_uri: idp.jwks_uri(),
            client_id: "novadesk-client".into(),
            redirect_uri: "http://127.0.0.1/rappel".into(),
            scopes: vec!["openid".into(), "email".into()],
        };
        let options = oidc::OptionsFlux {
            lier_par_email: true,
            ..oidc::OptionsFlux::default()
        };
        let flux = Arc::new(oidc::FluxOidc::new(config, options));

        let store = store_test();
        store
            .register("carol@example.com", "mdp")
            .expect("register");
        let addr = serveur(Service::avec_oidc(store.clone(), flux));

        // 1. Démarrage : URL d'autorisation + state.
        let (url, state) = match aller_retour(addr, &Request::DemarrerOidc) {
            Response::AutorisationOidc { url, state } => (url, state),
            _ => panic!("URL d'autorisation attendue"),
        };
        assert!(url.contains("code_challenge_method=S256"));

        // 2. « L'utilisatrice s'authentifie chez le fournisseur » : celui-ci
        // préparera un ID token RS256 portant le nonce de l'URL.
        let nonce = {
            let marqueur = "&nonce=";
            let debut = url.find(marqueur).expect("nonce dans l'URL") + marqueur.len();
            url[debut..].split('&').next().expect("valeur").to_string()
        };
        let maintenant = unix_maintenant();
        let charge = serde_json::json!({
            "iss": "https://idp.example.com",
            "sub": "sub-carol",
            "aud": "novadesk-client",
            "exp": maintenant + 300,
            "nonce": nonce,
            "email": "carol@example.com",
        });
        *idp.reponse_jetons.lock().unwrap() = serde_json::json!({
            "id_token": test_idp::signer_rs256(&charge, Some(test_idp::KID_RSA)),
        })
        .to_string();

        // 3. Rappel : échange du code, validation JWKS, liaison par e-mail
        // vérifié, session locale ouverte.
        let jeton = match aller_retour(
            addr,
            &Request::RappelOidc {
                state: state.clone(),
                code: "code-tcp-1".into(),
            },
        ) {
            Response::Token { jeton } => jeton,
            Response::Erreur { message } => panic!("rappel OIDC refusé : {message}"),
            _ => panic!("jeton de session attendu"),
        };
        assert_eq!(
            store.verify_token(&jeton).as_deref(),
            Some("carol@example.com")
        );
        // Le sujet fédéré est désormais lié : visible côté magasin.
        assert_eq!(
            store
                .oidc_account("https://idp.example.com|sub-carol")
                .as_deref(),
            Some("carol@example.com")
        );

        // 4. Anti-rejeu : le même state est refusé une seconde fois.
        assert!(matches!(
            aller_retour(
                addr,
                &Request::RappelOidc {
                    state,
                    code: "code-tcp-1".into(),
                }
            ),
            Response::Erreur { .. }
        ));
    }

    #[test]
    fn serveur_tcp_oidc_non_configure_et_sujet_non_lie() {
        use crate::jwks::test_idp;

        // Sans flux configuré : les requêtes OIDC répondent une erreur claire.
        let addr = serveur(Service::nouveau(store_test()));
        assert!(matches!(
            aller_retour(addr, &Request::DemarrerOidc),
            Response::Erreur { .. }
        ));

        // Avec flux mais **sans** liaison par e-mail : un sujet inconnu est
        // refusé même avec un ID token parfaitement valide.
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let config = oidc::OidcConfig {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: idp.token_endpoint(),
            jwks_uri: idp.jwks_uri(),
            client_id: "novadesk-client".into(),
            redirect_uri: "http://127.0.0.1/rappel".into(),
            scopes: vec![],
        };
        let flux = Arc::new(oidc::FluxOidc::new(config, oidc::OptionsFlux::default()));
        let store = store_test();
        store.register("greg@example.com", "mdp").expect("register");
        let addr = serveur(Service::avec_oidc(store.clone(), flux));

        let (url, state) = match aller_retour(addr, &Request::DemarrerOidc) {
            Response::AutorisationOidc { url, state } => (url, state),
            _ => panic!("URL d'autorisation attendue"),
        };
        let nonce = {
            let marqueur = "&nonce=";
            let debut = url.find(marqueur).expect("nonce") + marqueur.len();
            url[debut..].split('&').next().expect("valeur").to_string()
        };
        let maintenant = unix_maintenant();
        let charge = serde_json::json!({
            "iss": "https://idp.example.com",
            "sub": "sub-greg",
            "aud": "novadesk-client",
            "exp": maintenant + 300,
            "nonce": nonce,
            "email": "greg@example.com",
        });
        *idp.reponse_jetons.lock().unwrap() = serde_json::json!({
            "id_token": test_idp::signer_rs256(&charge, Some(test_idp::KID_RSA)),
        })
        .to_string();
        match aller_retour(
            addr,
            &Request::RappelOidc {
                state,
                code: "c".into(),
            },
        ) {
            Response::Erreur { message } => {
                assert!(message.contains("non liée"), "message : {message}");
            }
            _ => panic!("refus attendu pour un sujet non lié"),
        }
        // Rien n'a été lié en douce.
        assert_eq!(store.oidc_account("https://idp.example.com|sub-greg"), None);
    }
}
