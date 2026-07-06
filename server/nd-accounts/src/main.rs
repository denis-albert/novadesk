//! Service comptes / authentification NovaDesk.
//!
//! Opérations : `register(email, password)` (mot de passe haché **Argon2id**,
//! format PHC) et `login(email, password)` → jeton de session opaque (32 octets
//! aléatoires, encodés en hexadécimal).
//!
//! Persistance (module [`storage`]) : [`AccountStore::open`] attache le
//! magasin à un fichier JSON (écriture atomique : fichier temporaire +
//! `rename`) — comptes, secrets 2FA et liens OIDC survivent au redémarrage ;
//! `register`, `enable_2fa` et `link_oidc` persistent avant de réussir
//! (« durable ou rien »). Seuls les **hachages PHC Argon2id** sont écrits,
//! jamais un mot de passe ; les sessions restent volatiles par conception.
//! [`AccountStore::new`] garde le comportement purement en mémoire (tests).
//!
//! 2FA TOTP (RFC 6238, module [`totp`]) : `enable_2fa(email)` génère et stocke
//! le secret ; un compte protégé doit passer par `login_2fa(email, password,
//! code)` — `login` seul renvoie alors `DeuxFacteursRequis`. Les licences et
//! quotas de sessions vivent dans le module [`licensing`] ; le journal d'audit
//! (conformité, RGPD) et le registre des sessions actives dans le module
//! [`audit`] — attacher un journal via [`AccountStore::with_audit`] consigne
//! créations de compte, connexions et activations 2FA.
//!
//! Fédération OIDC/OAuth2 (module [`oidc`]) : PKCE S256, URL d'autorisation et
//! validation d'ID token JWT ; [`AccountStore::link_oidc`] rattache un sujet
//! fédéré à un compte local, [`AccountStore::login_oidc`] ouvre une session
//! pour un sujet déjà lié (l'authentification — y compris MFA — a eu lieu chez
//! le fournisseur d'identité : la 2FA locale ne s'applique pas à ce chemin).
//! Voir `../../plan-technique/11-backend-infrastructure.md`.
//!
//! Serveur TCP optionnel (std pur, un thread par connexion) au même format que
//! `nd-signaling` : trames à préfixe de longueur `u32` BE.
//!
//! Usage : `nd-accounts [adresse:port] [fichier_comptes.json]`
//! (défaut `0.0.0.0:9200`, en mémoire si aucun fichier n'est donné).

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
pub mod licensing;
pub mod oidc;
pub mod storage;
pub mod totp;

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
    /// Compte inconnu (activation 2FA sur un e-mail non enregistré, sujet
    /// OIDC jamais lié, etc.).
    CompteInconnu,
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
            AccountError::CompteInconnu => write!(f, "compte inconnu"),
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
/// e-mail → secret TOTP pour les comptes ayant activé la 2FA, et sujet OIDC
/// (`iss|sub`) → e-mail pour les identités fédérées liées.
#[derive(Default)]
struct Etat {
    comptes: HashMap<String, String>,
    sessions: HashMap<String, String>,
    secrets_2fa: HashMap<String, Vec<u8>>,
    liens_oidc: HashMap<String, String>,
}

impl Etat {
    /// Reconstruit l'état durable depuis le fichier de comptes. Les sessions
    /// repartent vides (volatiles par conception).
    fn depuis_donnees(donnees: storage::DonneesPersistees) -> Result<Self, AccountError> {
        let mut secrets_2fa = HashMap::with_capacity(donnees.secrets_2fa.len());
        for (email, hex) in donnees.secrets_2fa {
            let secret = storage::hex_vers_octets(&hex).ok_or_else(|| {
                AccountError::Stockage(format!("secret TOTP corrompu pour {email}"))
            })?;
            secrets_2fa.insert(email, secret);
        }
        Ok(Self {
            comptes: donnees.comptes,
            sessions: HashMap::new(),
            secrets_2fa,
            liens_oidc: donnees.liens_oidc,
        })
    }

    /// Instantané durable de l'état (hachages PHC, secrets TOTP en
    /// hexadécimal, liens OIDC — jamais les sessions ni un mot de passe).
    fn instantane(&self) -> storage::DonneesPersistees {
        storage::DonneesPersistees {
            version: storage::VERSION_FORMAT,
            comptes: self.comptes.clone(),
            secrets_2fa: self
                .secrets_2fa
                .iter()
                .map(|(email, secret)| (email.clone(), storage::octets_vers_hex(secret)))
                .collect(),
            liens_oidc: self.liens_oidc.clone(),
        }
    }
}

/// État partagé entre threads de connexion.
type EtatPartage = Arc<Mutex<Etat>>;

/// Magasin de comptes (thread-safe, clonable), en mémoire ou adossé à un
/// fichier (voir [`Self::open`]).
#[derive(Clone)]
pub struct AccountStore {
    etat: EtatPartage,
    argon: Argon2<'static>,
    /// Journal d'audit optionnel (voir [`Self::with_audit`]).
    audit: Option<audit::AuditLog>,
    /// Stockage persistant optionnel (voir [`Self::open`]) ; `None` = magasin
    /// purement en mémoire, volatil.
    stockage: Option<storage::StockageFichier>,
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

    /// Magasin avec une configuration Argon2 personnalisée (tests : paramètres légers).
    #[must_use]
    pub fn with_argon2(argon: Argon2<'static>) -> Self {
        Self {
            etat: EtatPartage::default(),
            argon,
            audit: None,
            stockage: None,
        }
    }

    /// Magasin **persistant** : charge le fichier de comptes s'il existe
    /// (comptes, secrets 2FA, liens OIDC), puis persiste chaque mutation
    /// durable (`register`, `enable_2fa`, `link_oidc`) par écriture atomique.
    /// Les sessions ne sont pas persistées. Paramètres Argon2id par défaut.
    ///
    /// # Errors
    /// `Stockage` si le fichier existe mais est illisible (JSON corrompu,
    /// version de format plus récente que le service, secret TOTP corrompu).
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
        let stockage = storage::StockageFichier::new(chemin.as_ref());
        let etat = match stockage
            .charger()
            .map_err(|e| AccountError::Stockage(e.to_string()))?
        {
            Some(donnees) => Etat::depuis_donnees(donnees)?,
            None => Etat::default(),
        };
        Ok(Self {
            etat: Arc::new(Mutex::new(etat)),
            argon,
            audit: None,
            stockage: Some(stockage),
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

    /// Persiste l'état durable si un stockage est attaché. À appeler **sous**
    /// le verrou d'état : l'instantané est cohérent et les écritures
    /// concurrentes des clones sont sérialisées. Les mutations sont rares
    /// (inscription, activation 2FA, lien OIDC) : l'E/S sous verrou est un
    /// compromis assumé et documenté.
    fn persister(&self, etat: &Etat) -> Result<(), AccountError> {
        let Some(stockage) = &self.stockage else {
            return Ok(());
        };
        stockage
            .sauvegarder(&etat.instantane())
            .map_err(|e| AccountError::Stockage(e.to_string()))
    }

    /// Crée un compte : le mot de passe est haché en **Argon2id** (sel
    /// aléatoire). Sur un magasin persistant, le compte est écrit sur disque
    /// avant que l'appel réussisse (« durable ou rien »).
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
        etat.comptes.insert(email.to_string(), phc);
        if let Err(e) = self.persister(&etat) {
            etat.comptes.remove(email); // durable ou rien
            return Err(e);
        }
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
    /// sous forme d'URI `otpauth://` en QR code). Réactiver régénère le secret.
    /// Sur un magasin persistant, le secret est écrit sur disque avant que
    /// l'appel réussisse (en clair pour l'instant — voir la doc de [`storage`] :
    /// le chiffrement au repos par clé serveur viendra).
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
        let precedent = etat.secrets_2fa.insert(email.to_string(), secret.clone());
        if let Err(e) = self.persister(&etat) {
            // Durable ou rien : on restaure l'état 2FA antérieur.
            match precedent {
                Some(ancien) => etat.secrets_2fa.insert(email.to_string(), ancien),
                None => etat.secrets_2fa.remove(email),
            };
            return Err(e);
        }
        drop(etat); // audit hors verrou
        self.auditer(audit::AuditEvent::TwoFactorEnabled {
            email: email.to_string(),
        });
        Ok(secret)
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
        etat.liens_oidc
            .insert(subject.to_string(), email.to_string());
        if let Err(e) = self.persister(&etat) {
            etat.liens_oidc.remove(subject); // durable ou rien
            return Err(e);
        }
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

enum Request {
    Register { email: String, password: String },
    Login { email: String, password: String },
}

enum Response {
    Ok,
    Token { jeton: String },
    Erreur { message: String },
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
        }
        out
    }

    fn from_bytes(d: &[u8]) -> Option<Request> {
        let mut p = 0;
        let tag = read_u8(d, &mut p)?;
        let email = read_string(d, &mut p)?;
        let password = read_string(d, &mut p)?;
        match tag {
            1 => Some(Request::Register { email, password }),
            2 => Some(Request::Login { email, password }),
            _ => None,
        }
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

/// Boucle de service (bloquante, un thread par connexion, une requête par connexion).
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
pub fn serve(listener: TcpListener, store: AccountStore) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let store = store.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &store);
        });
    }
    Ok(())
}

fn handle_conn(mut stream: TcpStream, store: &AccountStore) -> std::io::Result<()> {
    let req_bytes = read_frame(&mut stream)?;
    let resp = match Request::from_bytes(&req_bytes) {
        Some(Request::Register { email, password }) => match store.register(&email, &password) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Erreur {
                message: e.to_string(),
            },
        },
        Some(Request::Login { email, password }) => match store.login(&email, &password) {
            Ok(jeton) => Response::Token { jeton },
            Err(e) => Response::Erreur {
                message: e.to_string(),
            },
        },
        None => Response::Erreur {
            message: "requête invalide".into(),
        },
    };
    write_frame(&mut stream, &resp.to_bytes())
}

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    // Second argument optionnel : chemin du fichier de comptes (persistance).
    let chemin = std::env::args().nth(2);
    let (store, mode) = match &chemin {
        Some(chemin) => {
            let store = AccountStore::open(chemin).map_err(std::io::Error::other)?;
            (store, format!("comptes persistés dans {chemin}"))
        }
        None => (AccountStore::new(), "comptes en mémoire".to_string()),
    };
    let listener = TcpListener::bind(&addr)?;
    println!(
        "nd-accounts (NovaDesk protocole v{}) en écoute sur {} — {mode}",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    serve(listener, store)
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
        let store = store_persistant(tmp.chemin());
        store
            .register("dan@example.com", "MotDePasseTresSecret123")
            .expect("register");

        let contenu = std::fs::read_to_string(tmp.chemin()).expect("fichier écrit");
        assert!(
            !contenu.contains("MotDePasseTresSecret123"),
            "le mot de passe en clair ne doit jamais toucher le disque"
        );
        assert!(
            contenu.contains("$argon2id$"),
            "seul le hachage PHC Argon2id est persisté"
        );
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
        ];
        for r in &reqs {
            assert!(Request::from_bytes(&r.to_bytes()).is_some());
        }
        assert!(Request::from_bytes(&[]).is_none());

        let bytes = Response::Token {
            jeton: "abc123".into(),
        }
        .to_bytes();
        match Response::from_bytes(&bytes) {
            Some(Response::Token { jeton }) => assert_eq!(jeton, "abc123"),
            _ => panic!("désérialisation Token échouée"),
        }
    }

    #[test]
    fn serveur_tcp_register_login() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("adresse locale");
        std::thread::spawn(move || {
            let _ = serve(listener, store_test());
        });

        let aller_retour = |req: &Request| -> Response {
            let mut s = TcpStream::connect(addr).expect("connexion");
            write_frame(&mut s, &req.to_bytes()).expect("écriture");
            Response::from_bytes(&read_frame(&mut s).expect("lecture")).expect("réponse")
        };

        let inscription = aller_retour(&Request::Register {
            email: "tcp@example.com".into(),
            password: "mdp".into(),
        });
        assert!(matches!(inscription, Response::Ok));

        match aller_retour(&Request::Login {
            email: "tcp@example.com".into(),
            password: "mdp".into(),
        }) {
            Response::Token { jeton } => assert_eq!(jeton.len(), 64),
            _ => panic!("login TCP : jeton attendu"),
        }
    }
}
