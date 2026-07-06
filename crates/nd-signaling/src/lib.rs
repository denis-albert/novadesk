//! `nd-signaling` — connectivité par **ID NovaDesk** via un serveur de rendez-vous.
//!
//! Le serveur associe un ID à l'adresse (UDP/QUIC) et au certificat auto-signé du pair
//! contrôlé ; un pair contrôleur résout l'ID puis établit la connexion QUIC directe
//! (voir `nd-transport`). Ce premier jet fait de la **mise en relation directe**
//! (loopback/LAN) ; la découverte d'adresse publique est fournie par le module
//! [`stun`] (client RFC 5389), le hole punching et le relais viendront ensuite.
//! Voir `../../plan-technique/05-connectivite-nat.md`.
//!
//! Implémentation std pure (TCP bloquant, un thread par connexion). Le serveur de
//! production sera asynchrone et à l'échelle (voir plan 11).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use nd_proto::{NdError, NovaId, Result};

/// Client STUN (RFC 5389) : découverte de l'adresse réflexive publique.
pub mod stun;

/// Enregistrement d'un pair résolu par son ID.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    /// Adresse (UDP/QUIC) du pair contrôlé.
    pub addr: SocketAddr,
    /// Certificat auto-signé du pair (à épingler côté client, voir `nd-transport`).
    pub cert_der: Vec<u8>,
}

/// Table interne du registre : ID → (adresse textuelle, certificat DER).
type PeerMap = HashMap<u64, (String, Vec<u8>)>;

/// Registre partagé du serveur : ID → (adresse, certificat).
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<PeerMap>>);

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Protocole (privé)
// ---------------------------------------------------------------------------

enum Request {
    Register {
        id: u64,
        addr: String,
        cert: Vec<u8>,
    },
    Lookup {
        id: u64,
    },
}

enum Response {
    Registered,
    Found { addr: String, cert: Vec<u8> },
    NotFound,
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

fn read_bytes(d: &[u8], p: &mut usize) -> Option<Vec<u8>> {
    let len = read_u32(d, p)? as usize;
    let b = d.get(*p..*p + len)?.to_vec();
    *p += len;
    Some(b)
}

impl Request {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::Register { id, addr, cert } => {
                out.push(1);
                out.extend_from_slice(&id.to_be_bytes());
                put_bytes(&mut out, addr.as_bytes());
                put_bytes(&mut out, cert);
            }
            Request::Lookup { id } => {
                out.push(2);
                out.extend_from_slice(&id.to_be_bytes());
            }
        }
        out
    }

    fn from_bytes(d: &[u8]) -> Option<Request> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            1 => {
                let id = read_u64(d, &mut p)?;
                let addr = String::from_utf8(read_bytes(d, &mut p)?).ok()?;
                let cert = read_bytes(d, &mut p)?;
                Some(Request::Register { id, addr, cert })
            }
            2 => Some(Request::Lookup {
                id: read_u64(d, &mut p)?,
            }),
            _ => None,
        }
    }
}

impl Response {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::Registered => out.push(0),
            Response::Found { addr, cert } => {
                out.push(1);
                put_bytes(&mut out, addr.as_bytes());
                put_bytes(&mut out, cert);
            }
            Response::NotFound => out.push(2),
        }
        out
    }

    fn from_bytes(d: &[u8]) -> Option<Response> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            0 => Some(Response::Registered),
            1 => {
                let addr = String::from_utf8(read_bytes(d, &mut p)?).ok()?;
                let cert = read_bytes(d, &mut p)?;
                Some(Response::Found { addr, cert })
            }
            2 => Some(Response::NotFound),
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
    if len > 1 << 20 {
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

/// Boucle de service du rendez-vous (bloquante, un thread par connexion).
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
pub fn serve(listener: TcpListener, registry: Registry) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let reg = registry.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &reg);
        });
    }
    Ok(())
}

fn handle_conn(mut stream: TcpStream, registry: &Registry) -> std::io::Result<()> {
    let req_bytes = read_frame(&mut stream)?;
    let resp = match Request::from_bytes(&req_bytes) {
        Some(Request::Register { id, addr, cert }) => {
            registry.0.lock().unwrap().insert(id, (addr, cert));
            Response::Registered
        }
        Some(Request::Lookup { id }) => match registry.0.lock().unwrap().get(&id) {
            Some((addr, cert)) => Response::Found {
                addr: addr.clone(),
                cert: cert.clone(),
            },
            None => Response::NotFound,
        },
        None => Response::NotFound,
    };
    write_frame(&mut stream, &resp.to_bytes())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Client du serveur de rendez-vous.
pub struct RendezvousClient {
    server: SocketAddr,
}

impl RendezvousClient {
    #[must_use]
    pub fn new(server: SocketAddr) -> Self {
        Self { server }
    }

    fn round_trip(&self, req: &Request) -> Result<Response> {
        let mut stream = TcpStream::connect(self.server)?;
        write_frame(&mut stream, &req.to_bytes())?;
        let resp = read_frame(&mut stream)?;
        Response::from_bytes(&resp)
            .ok_or_else(|| NdError::Protocol("réponse rendez-vous invalide".into()))
    }

    /// Publie l'ID local avec son adresse (UDP/QUIC) et son certificat.
    pub fn register(&self, id: NovaId, addr: SocketAddr, cert_der: &[u8]) -> Result<()> {
        let req = Request::Register {
            id: id.as_u64(),
            addr: addr.to_string(),
            cert: cert_der.to_vec(),
        };
        match self.round_trip(&req)? {
            Response::Registered => Ok(()),
            _ => Err(NdError::Protocol("échec d'enregistrement".into())),
        }
    }

    /// Résout un ID pair en adresse + certificat.
    pub fn lookup(&self, id: NovaId) -> Result<PeerRecord> {
        let req = Request::Lookup { id: id.as_u64() };
        match self.round_trip(&req)? {
            Response::Found { addr, cert } => {
                let addr = addr
                    .parse()
                    .map_err(|_| NdError::Protocol("adresse pair invalide".into()))?;
                Ok(PeerRecord {
                    addr,
                    cert_der: cert,
                })
            }
            Response::NotFound => Err(NdError::Protocol(format!("ID {id} introuvable"))),
            _ => Err(NdError::Protocol("réponse inattendue".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let reqs = [
            Request::Register {
                id: 42,
                addr: "127.0.0.1:5000".into(),
                cert: vec![1, 2, 3, 4],
            },
            Request::Lookup { id: 42 },
        ];
        for r in &reqs {
            assert!(Request::from_bytes(&r.to_bytes()).is_some());
        }
        assert!(Request::from_bytes(&[]).is_none());
    }

    #[test]
    fn response_roundtrip() {
        let bytes = Response::Found {
            addr: "10.0.0.1:9".into(),
            cert: vec![9, 9],
        }
        .to_bytes();
        match Response::from_bytes(&bytes) {
            Some(Response::Found { addr, cert }) => {
                assert_eq!(addr, "10.0.0.1:9");
                assert_eq!(cert, vec![9, 9]);
            }
            _ => panic!("désérialisation Found échouée"),
        }
    }
}
