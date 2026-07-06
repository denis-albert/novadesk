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
//!
//! **Présence & expiration (plan 05)** : chaque enregistrement porte un horodatage
//! de dernière activité, rafraîchi par un *heartbeat* périodique du client
//! ([`RendezvousClient::heartbeat`]). Une entrée non rafraîchie depuis le TTL du
//! registre (défaut : [`DEFAULT_TTL`]) est considérée hors-ligne : `lookup` la
//! renvoie « introuvable » et [`Registry::sweep_expired`] (ou le balayeur lancé
//! via [`Registry::spawn_sweeper`]) la retire de la table.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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

/// TTL de présence par défaut : au-delà sans heartbeat, un pair est hors-ligne.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Entrée interne du registre : coordonnées du pair + dernière activité.
struct PeerEntry {
    /// Adresse textuelle (UDP/QUIC) publiée par le pair.
    addr: String,
    /// Certificat auto-signé (DER) du pair.
    cert: Vec<u8>,
    /// Horodatage du dernier `Register`/`Heartbeat` reçu.
    last_seen: Instant,
}

/// Table interne du registre : ID → entrée (adresse, certificat, dernière activité).
type PeerMap = HashMap<u64, PeerEntry>;

/// Registre partagé du serveur : ID → (adresse, certificat, présence).
///
/// Les clones partagent la même table ; le TTL de présence est fixé à la
/// construction ([`Registry::new`] → [`DEFAULT_TTL`], sinon [`Registry::with_ttl`]).
#[derive(Clone)]
pub struct Registry {
    peers: Arc<Mutex<PeerMap>>,
    ttl: Duration,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }
}

impl Registry {
    /// Crée un registre vide avec le TTL de présence par défaut ([`DEFAULT_TTL`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Crée un registre vide avec un TTL de présence personnalisé.
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            peers: Arc::default(),
            ttl,
        }
    }

    /// TTL de présence du registre.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Insère (ou remplace) un enregistrement, horodaté à maintenant.
    fn insert(&self, id: u64, addr: String, cert: Vec<u8>) {
        self.peers.lock().unwrap().insert(
            id,
            PeerEntry {
                addr,
                cert,
                last_seen: Instant::now(),
            },
        );
    }

    /// Rafraîchit la dernière activité d'un ID (heartbeat).
    /// Renvoie `false` si l'ID n'est pas (ou plus) enregistré.
    fn touch(&self, id: u64) -> bool {
        match self.peers.lock().unwrap().get_mut(&id) {
            Some(entry) => {
                entry.last_seen = Instant::now();
                true
            }
            None => false,
        }
    }

    /// Renvoie (adresse, certificat) si l'ID est enregistré **et** non périmé.
    fn get_fresh(&self, id: u64) -> Option<(String, Vec<u8>)> {
        let peers = self.peers.lock().unwrap();
        let entry = peers.get(&id)?;
        (entry.last_seen.elapsed() <= self.ttl).then(|| (entry.addr.clone(), entry.cert.clone()))
    }

    /// Retire les entrées non rafraîchies depuis plus de `ttl` et renvoie le
    /// nombre d'entrées retirées.
    pub fn sweep_expired(&self, ttl: Duration) -> usize {
        let mut peers = self.peers.lock().unwrap();
        let avant = peers.len();
        peers.retain(|_, entry| entry.last_seen.elapsed() <= ttl);
        avant - peers.len()
    }

    /// Nombre de pairs actuellement en ligne (non périmés au TTL du registre).
    #[must_use]
    pub fn online_count(&self) -> usize {
        self.peers
            .lock()
            .unwrap()
            .values()
            .filter(|entry| entry.last_seen.elapsed() <= self.ttl)
            .count()
    }

    /// Un ID est en ligne s'il est enregistré et rafraîchi il y a moins de `ttl`.
    #[must_use]
    pub fn is_online(&self, id: u64, ttl: Duration) -> bool {
        self.peers
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|entry| entry.last_seen.elapsed() <= ttl)
    }

    /// Lance un thread qui appelle [`Registry::sweep_expired`] (avec le TTL du
    /// registre) toutes les `interval`.
    ///
    /// La [`SweeperHandle`] renvoyée permet un arrêt propre via
    /// [`SweeperHandle::stop`] ; la jeter (`drop`) laisse le thread tourner en
    /// démon jusqu'à la fin du processus (usage typique du binaire serveur).
    #[must_use = "jeter la poignée laisse le balayeur tourner en démon"]
    pub fn spawn_sweeper(&self, interval: Duration) -> SweeperHandle {
        let ctrl = Arc::new((Mutex::new(false), Condvar::new()));
        let ctrl_thread = Arc::clone(&ctrl);
        let registry = self.clone();
        let thread = std::thread::spawn(move || {
            let (arret, cvar) = &*ctrl_thread;
            let mut stoppe = arret.lock().unwrap();
            while !*stoppe {
                // Attente bornée : réveil au bout de `interval` ou sur `stop()`.
                let (garde, _timeout) = cvar.wait_timeout(stoppe, interval).unwrap();
                stoppe = garde;
                if !*stoppe {
                    registry.sweep_expired(registry.ttl);
                }
            }
        });
        SweeperHandle { ctrl, thread }
    }
}

/// Poignée du balayeur périodique lancé par [`Registry::spawn_sweeper`].
///
/// `stop()` arrête proprement le thread ; un simple `drop` le laisse tourner
/// en démon (le balayage continue jusqu'à la fin du processus).
pub struct SweeperHandle {
    ctrl: Arc<(Mutex<bool>, Condvar)>,
    thread: std::thread::JoinHandle<()>,
}

impl SweeperHandle {
    /// Demande l'arrêt du balayeur et attend la fin de son thread.
    pub fn stop(self) {
        {
            let (arret, cvar) = &*self.ctrl;
            *arret.lock().unwrap() = true;
            cvar.notify_all();
        }
        let _ = self.thread.join();
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
    /// Rafraîchit la présence d'un ID déjà enregistré (plan 05).
    Heartbeat {
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
            Request::Heartbeat { id } => {
                out.push(3);
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
            3 => Some(Request::Heartbeat {
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
            registry.insert(id, addr, cert);
            Response::Registered
        }
        // Un pair périmé (non rafraîchi depuis le TTL) est « introuvable »,
        // même si le balayeur ne l'a pas encore retiré de la table.
        Some(Request::Lookup { id }) => match registry.get_fresh(id) {
            Some((addr, cert)) => Response::Found { addr, cert },
            None => Response::NotFound,
        },
        Some(Request::Heartbeat { id }) => {
            if registry.touch(id) {
                Response::Registered
            } else {
                Response::NotFound
            }
        }
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

    /// Rafraîchit la présence de l'ID auprès du serveur (heartbeat) : repousse
    /// l'expiration TTL de l'enregistrement. À appeler périodiquement (bien
    /// plus souvent que le TTL du serveur, p. ex. TTL/3).
    ///
    /// # Errors
    /// Renvoie une erreur si l'ID n'est pas (ou plus) enregistré côté serveur
    /// — il faut alors refaire un [`RendezvousClient::register`] — ou en cas
    /// d'erreur réseau/protocole.
    pub fn heartbeat(&self, id: NovaId) -> Result<()> {
        let req = Request::Heartbeat { id: id.as_u64() };
        match self.round_trip(&req)? {
            Response::Registered => Ok(()),
            Response::NotFound => Err(NdError::Protocol(format!("ID {id} non enregistré"))),
            _ => Err(NdError::Protocol("réponse inattendue".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Démarre un serveur de rendez-vous éphémère (port 0) avec le TTL donné.
    fn serveur_de_test(ttl: Duration) -> (Registry, RendezvousClient) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let registry = Registry::with_ttl(ttl);
        let reg = registry.clone();
        std::thread::spawn(move || {
            let _ = serve(listener, reg);
        });
        (registry, RendezvousClient::new(addr))
    }

    fn adresse_bidon() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    #[test]
    fn request_roundtrip() {
        let reqs = [
            Request::Register {
                id: 42,
                addr: "127.0.0.1:5000".into(),
                cert: vec![1, 2, 3, 4],
            },
            Request::Lookup { id: 42 },
            Request::Heartbeat { id: 42 },
        ];
        for r in &reqs {
            assert!(Request::from_bytes(&r.to_bytes()).is_some());
        }
        assert!(Request::from_bytes(&[]).is_none());
    }

    #[test]
    fn heartbeat_roundtrip_preserve_l_id() {
        let bytes = Request::Heartbeat { id: 77 }.to_bytes();
        match Request::from_bytes(&bytes) {
            Some(Request::Heartbeat { id }) => assert_eq!(id, 77),
            _ => panic!("désérialisation Heartbeat échouée"),
        }
    }

    #[test]
    fn register_rend_le_pair_en_ligne() {
        let ttl = Duration::from_secs(60);
        let (registry, client) = serveur_de_test(ttl);
        client
            .register(NovaId(42), adresse_bidon(), &[1, 2, 3])
            .unwrap();

        assert!(registry.is_online(42, ttl));
        assert!(!registry.is_online(43, ttl));
        assert_eq!(registry.online_count(), 1);
        assert_eq!(client.lookup(NovaId(42)).unwrap().cert_der, vec![1, 2, 3]);
    }

    #[test]
    fn expiration_sweep_retire_et_lookup_echoue() {
        let ttl = Duration::from_millis(50);
        let (registry, client) = serveur_de_test(ttl);
        client.register(NovaId(7), adresse_bidon(), &[9]).unwrap();
        assert!(client.lookup(NovaId(7)).is_ok());

        std::thread::sleep(Duration::from_millis(200));

        // Périmé : hors-ligne et introuvable, même avant le balayage.
        assert!(!registry.is_online(7, ttl));
        assert_eq!(registry.online_count(), 0);
        assert!(client.lookup(NovaId(7)).is_err());

        // Le balayage retire l'entrée ; un second passage ne retire rien.
        assert_eq!(registry.sweep_expired(ttl), 1);
        assert_eq!(registry.sweep_expired(ttl), 0);
        assert!(registry.peers.lock().unwrap().is_empty());
        assert!(client.lookup(NovaId(7)).is_err());
    }

    #[test]
    fn heartbeat_rafraichit_et_empeche_expiration() {
        let ttl = Duration::from_millis(300);
        let (registry, client) = serveur_de_test(ttl);
        client
            .register(NovaId(9), adresse_bidon(), &[4, 5])
            .unwrap();

        // 4 battements espacés de 100 ms : ~400 ms écoulées (> TTL), mais
        // chaque battement repousse l'expiration.
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(100));
            client.heartbeat(NovaId(9)).unwrap();
        }
        assert_eq!(registry.sweep_expired(ttl), 0);
        assert!(registry.is_online(9, ttl));
        assert!(client.lookup(NovaId(9)).is_ok());

        // Sans battement, l'entrée finit par expirer.
        std::thread::sleep(Duration::from_millis(400));
        assert!(!registry.is_online(9, ttl));
        assert_eq!(registry.sweep_expired(ttl), 1);
    }

    #[test]
    fn heartbeat_id_inconnu_echoue() {
        let (_registry, client) = serveur_de_test(Duration::from_secs(60));
        assert!(client.heartbeat(NovaId(999)).is_err());
    }

    #[test]
    fn balayeur_periodique_retire_les_perimees() {
        let registry = Registry::with_ttl(Duration::from_millis(40));
        registry.insert(1, "127.0.0.1:1".into(), vec![]);
        assert_eq!(registry.online_count(), 1);

        let poignee = registry.spawn_sweeper(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(300));

        // L'entrée périmée a été retirée de la table par le balayeur.
        assert!(registry.peers.lock().unwrap().is_empty());
        assert_eq!(registry.online_count(), 0);
        poignee.stop();
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
