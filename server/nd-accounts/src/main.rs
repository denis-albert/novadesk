//! Service comptes / authentification NovaDesk — premier jet **en mémoire**.
//!
//! Opérations : `register(email, password)` (mot de passe haché **Argon2id**,
//! format PHC) et `login(email, password)` → jeton de session opaque (32 octets
//! aléatoires, encodés en hexadécimal). Aucune base de données : tout vit dans
//! un `Arc<Mutex<...>>`. OAuth2/OIDC, JWT, 2FA et SSO viendront ensuite.
//! Voir `../../plan-technique/11-backend-infrastructure.md`.
//!
//! Serveur TCP optionnel (std pur, un thread par connexion) au même format que
//! `nd-signaling` : trames à préfixe de longueur `u32` BE.
//!
//! Usage : `nd-accounts [adresse:port]` (défaut `0.0.0.0:9200`).

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

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
    /// Erreur interne (hachage, etc.).
    Interne(String),
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::EmailDejaUtilise => write!(f, "e-mail déjà utilisé"),
            AccountError::IdentifiantsInvalides => write!(f, "identifiants invalides"),
            AccountError::EntreeInvalide => write!(f, "e-mail ou mot de passe invalide"),
            AccountError::Interne(msg) => write!(f, "erreur interne : {msg}"),
        }
    }
}

impl std::error::Error for AccountError {}

// ---------------------------------------------------------------------------
// Logique métier
// ---------------------------------------------------------------------------

/// État interne : e-mail → hachage PHC, et jeton de session → e-mail.
#[derive(Default)]
struct Etat {
    comptes: HashMap<String, String>,
    sessions: HashMap<String, String>,
}

/// État partagé entre threads de connexion.
type EtatPartage = Arc<Mutex<Etat>>;

/// Magasin de comptes en mémoire (thread-safe, clonable).
#[derive(Clone)]
pub struct AccountStore {
    etat: EtatPartage,
    argon: Argon2<'static>,
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
        }
    }

    /// Crée un compte : le mot de passe est haché en **Argon2id** (sel aléatoire).
    ///
    /// # Errors
    /// `EntreeInvalide` si e-mail/mot de passe vide, `EmailDejaUtilise` si le
    /// compte existe, `Interne` si le hachage échoue.
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
        Ok(())
    }

    /// Vérifie les identifiants et renvoie un jeton de session opaque.
    ///
    /// # Errors
    /// `IdentifiantsInvalides` si l'e-mail est inconnu ou le mot de passe faux
    /// (indistincts pour ne pas révéler l'existence d'un compte).
    pub fn login(&self, email: &str, password: &str) -> Result<String, AccountError> {
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
            .map_err(|_| AccountError::IdentifiantsInvalides)?;
        let jeton = jeton_aleatoire();
        self.etat
            .lock()
            .unwrap()
            .sessions
            .insert(jeton.clone(), email.to_string());
        Ok(jeton)
    }

    /// Résout un jeton de session en e-mail de compte (None si inconnu).
    #[must_use]
    pub fn verify_token(&self, jeton: &str) -> Option<String> {
        self.etat.lock().unwrap().sessions.get(jeton).cloned()
    }
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
    let listener = TcpListener::bind(&addr)?;
    println!(
        "nd-accounts (NovaDesk protocole v{}) en écoute sur {} — comptes en mémoire",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    serve(listener, AccountStore::new())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{Algorithm, Params, Version};

    /// Magasin avec des paramètres Argon2id légers (tests rapides en debug).
    fn store_test() -> AccountStore {
        let params = Params::new(Params::MIN_M_COST, 1, 1, None).expect("params argon2");
        AccountStore::with_argon2(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
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
