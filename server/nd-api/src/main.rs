//! API applicative NovaDesk — carnet d'adresses, premier jet **en mémoire**.
//!
//! Opérations : `add_contact(jeton, contact_id, alias)` et `list_contacts(jeton)`.
//! Le carnet est rangé par jeton de session ; la vérification du jeton est
//! volontairement minimale pour ce jet (tout jeton non vide est accepté) — la
//! validation croisée avec `nd-accounts`, le RBAC, les licences et le service de
//! mises à jour viendront ensuite. Voir `../../plan-technique/11-backend-infrastructure.md`.
//!
//! Serveur TCP optionnel (std pur, un thread par connexion) au même format que
//! `nd-signaling` : trames à préfixe de longueur `u32` BE.
//!
//! Usage : `nd-api [adresse:port]` (défaut `0.0.0.0:9300`).

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Adresse d'écoute par défaut (9000 = rendez-vous, 9100 = relais, 9200 = comptes).
const ADRESSE_DEFAUT: &str = "0.0.0.0:9300";

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreurs métier de l'API applicative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Jeton de session vide ou absent.
    JetonInvalide,
    /// Alias de contact vide.
    AliasVide,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::JetonInvalide => write!(f, "jeton invalide ou absent"),
            ApiError::AliasVide => write!(f, "alias de contact vide"),
        }
    }
}

impl std::error::Error for ApiError {}

// ---------------------------------------------------------------------------
// Logique métier
// ---------------------------------------------------------------------------

/// Entrée du carnet d'adresses : ID NovaDesk + alias lisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// ID NovaDesk du pair (voir `nd_proto::NovaId`).
    pub id: u64,
    /// Alias choisi par l'utilisateur (« PC bureau », ...).
    pub alias: String,
}

/// Table interne : jeton de session → contacts du compte.
type CarnetMap = HashMap<String, Vec<Contact>>;

/// Carnet d'adresses partagé, en mémoire (thread-safe, clonable).
#[derive(Clone, Default)]
pub struct AddressBook(Arc<Mutex<CarnetMap>>);

impl AddressBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute (ou met à jour l'alias d')un contact du compte identifié par `jeton`.
    ///
    /// # Errors
    /// `JetonInvalide` si le jeton est vide, `AliasVide` si l'alias est vide.
    pub fn add_contact(&self, jeton: &str, contact_id: u64, alias: &str) -> Result<(), ApiError> {
        verifier_jeton(jeton)?;
        if alias.trim().is_empty() {
            return Err(ApiError::AliasVide);
        }
        let mut carnet = self.0.lock().unwrap();
        let contacts = carnet.entry(jeton.to_string()).or_default();
        match contacts.iter_mut().find(|c| c.id == contact_id) {
            // Même ID déjà présent : on met l'alias à jour.
            Some(existant) => existant.alias = alias.to_string(),
            None => contacts.push(Contact {
                id: contact_id,
                alias: alias.to_string(),
            }),
        }
        Ok(())
    }

    /// Liste les contacts du compte identifié par `jeton` (vide si aucun).
    ///
    /// # Errors
    /// `JetonInvalide` si le jeton est vide.
    pub fn list_contacts(&self, jeton: &str) -> Result<Vec<Contact>, ApiError> {
        verifier_jeton(jeton)?;
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(jeton)
            .cloned()
            .unwrap_or_default())
    }
}

/// Vérification minimale pour ce jet : tout jeton non vide est accepté.
/// (La validation auprès de `nd-accounts` viendra avec la persistance.)
fn verifier_jeton(jeton: &str) -> Result<(), ApiError> {
    if jeton.trim().is_empty() {
        Err(ApiError::JetonInvalide)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Protocole (trames u32 BE + charge utile, comme nd-signaling)
// ---------------------------------------------------------------------------

enum Request {
    AddContact {
        jeton: String,
        id: u64,
        alias: String,
    },
    ListContacts {
        jeton: String,
    },
}

enum Response {
    Ok,
    Contacts(Vec<Contact>),
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

fn read_u64(d: &[u8], p: &mut usize) -> Option<u64> {
    let v = u64::from_be_bytes(d.get(*p..*p + 8)?.try_into().ok()?);
    *p += 8;
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
            Request::AddContact { jeton, id, alias } => {
                out.push(1);
                put_bytes(&mut out, jeton.as_bytes());
                out.extend_from_slice(&id.to_be_bytes());
                put_bytes(&mut out, alias.as_bytes());
            }
            Request::ListContacts { jeton } => {
                out.push(2);
                put_bytes(&mut out, jeton.as_bytes());
            }
        }
        out
    }

    fn from_bytes(d: &[u8]) -> Option<Request> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            1 => {
                let jeton = read_string(d, &mut p)?;
                let id = read_u64(d, &mut p)?;
                let alias = read_string(d, &mut p)?;
                Some(Request::AddContact { jeton, id, alias })
            }
            2 => Some(Request::ListContacts {
                jeton: read_string(d, &mut p)?,
            }),
            _ => None,
        }
    }
}

impl Response {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::Ok => out.push(0),
            Response::Contacts(contacts) => {
                out.push(1);
                out.extend_from_slice(&(contacts.len() as u32).to_be_bytes());
                for c in contacts {
                    out.extend_from_slice(&c.id.to_be_bytes());
                    put_bytes(&mut out, c.alias.as_bytes());
                }
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
            1 => {
                let n = read_u32(d, &mut p)?;
                let mut contacts = Vec::new();
                for _ in 0..n {
                    contacts.push(Contact {
                        id: read_u64(d, &mut p)?,
                        alias: read_string(d, &mut p)?,
                    });
                }
                Some(Response::Contacts(contacts))
            }
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
pub fn serve(listener: TcpListener, carnet: AddressBook) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let carnet = carnet.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &carnet);
        });
    }
    Ok(())
}

fn handle_conn(mut stream: TcpStream, carnet: &AddressBook) -> std::io::Result<()> {
    let req_bytes = read_frame(&mut stream)?;
    let resp = match Request::from_bytes(&req_bytes) {
        Some(Request::AddContact { jeton, id, alias }) => {
            match carnet.add_contact(&jeton, id, &alias) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Erreur {
                    message: e.to_string(),
                },
            }
        }
        Some(Request::ListContacts { jeton }) => match carnet.list_contacts(&jeton) {
            Ok(contacts) => Response::Contacts(contacts),
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
        "nd-api (NovaDesk protocole v{}) en écoute sur {} — carnet d'adresses en mémoire",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    serve(listener, AddressBook::new())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_puis_list_contacts() {
        let carnet = AddressBook::new();
        carnet
            .add_contact("jeton-a", 111_222_333, "PC bureau")
            .expect("add 1");
        carnet
            .add_contact("jeton-a", 444_555_666, "Portable")
            .expect("add 2");
        let contacts = carnet.list_contacts("jeton-a").expect("list");
        assert_eq!(
            contacts,
            vec![
                Contact {
                    id: 111_222_333,
                    alias: "PC bureau".into()
                },
                Contact {
                    id: 444_555_666,
                    alias: "Portable".into()
                },
            ]
        );
    }

    #[test]
    fn carnets_isoles_par_jeton() {
        let carnet = AddressBook::new();
        carnet.add_contact("jeton-a", 1, "A").expect("add a");
        carnet.add_contact("jeton-b", 2, "B").expect("add b");
        assert_eq!(carnet.list_contacts("jeton-a").expect("list a").len(), 1);
        assert_eq!(carnet.list_contacts("jeton-b").expect("list b").len(), 1);
        // Jeton jamais vu : carnet vide, pas d'erreur.
        assert!(carnet.list_contacts("jeton-c").expect("list c").is_empty());
    }

    #[test]
    fn meme_id_met_alias_a_jour() {
        let carnet = AddressBook::new();
        carnet
            .add_contact("jeton-a", 42, "Ancien nom")
            .expect("add");
        carnet
            .add_contact("jeton-a", 42, "Nouveau nom")
            .expect("maj");
        let contacts = carnet.list_contacts("jeton-a").expect("list");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].alias, "Nouveau nom");
    }

    #[test]
    fn jeton_vide_refuse() {
        let carnet = AddressBook::new();
        assert_eq!(carnet.add_contact("", 1, "X"), Err(ApiError::JetonInvalide));
        assert_eq!(carnet.list_contacts("  "), Err(ApiError::JetonInvalide));
        // Alias vide refusé aussi.
        assert_eq!(carnet.add_contact("jeton", 1, ""), Err(ApiError::AliasVide));
    }

    #[test]
    fn protocole_aller_retour() {
        let reqs = [
            Request::AddContact {
                jeton: "t".into(),
                id: 42,
                alias: "PC".into(),
            },
            Request::ListContacts { jeton: "t".into() },
        ];
        for r in &reqs {
            assert!(Request::from_bytes(&r.to_bytes()).is_some());
        }
        assert!(Request::from_bytes(&[]).is_none());

        let bytes = Response::Contacts(vec![Contact {
            id: 7,
            alias: "Portable".into(),
        }])
        .to_bytes();
        match Response::from_bytes(&bytes) {
            Some(Response::Contacts(contacts)) => {
                assert_eq!(contacts.len(), 1);
                assert_eq!(contacts[0].id, 7);
                assert_eq!(contacts[0].alias, "Portable");
            }
            _ => panic!("désérialisation Contacts échouée"),
        }
    }

    #[test]
    fn serveur_tcp_add_puis_list() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("adresse locale");
        std::thread::spawn(move || {
            let _ = serve(listener, AddressBook::new());
        });

        let aller_retour = |req: &Request| -> Response {
            let mut s = TcpStream::connect(addr).expect("connexion");
            write_frame(&mut s, &req.to_bytes()).expect("écriture");
            Response::from_bytes(&read_frame(&mut s).expect("lecture")).expect("réponse")
        };

        let ajout = aller_retour(&Request::AddContact {
            jeton: "jeton-tcp".into(),
            id: 99,
            alias: "Serveur salon".into(),
        });
        assert!(matches!(ajout, Response::Ok));

        match aller_retour(&Request::ListContacts {
            jeton: "jeton-tcp".into(),
        }) {
            Response::Contacts(contacts) => {
                assert_eq!(contacts.len(), 1);
                assert_eq!(contacts[0].id, 99);
                assert_eq!(contacts[0].alias, "Serveur salon");
            }
            _ => panic!("list TCP : contacts attendus"),
        }
    }
}
