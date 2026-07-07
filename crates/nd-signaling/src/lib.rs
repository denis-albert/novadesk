//! `nd-signaling` — connectivité par **ID NovaDesk** via un serveur de rendez-vous.
//!
//! Le serveur associe un ID à l'adresse (UDP/QUIC) et au certificat auto-signé du pair
//! contrôlé ; un pair contrôleur résout l'ID puis établit la connexion QUIC directe
//! (voir `nd-transport`). La découverte d'adresse publique est fournie par le module
//! [`stun`] (client RFC 5389). Voir `../../plan-technique/05-connectivite-nat.md`.
//!
//! **NAT traversal (plan 05)** : au-delà de la mise en relation directe
//! (loopback/LAN), le rendez-vous coordonne l'**UDP hole punching** :
//!
//! 1. chaque pair dépose ses **candidats** (adresse locale + adresse réflexive
//!    STUN) sous son ID ([`RendezvousClient::publish_candidates`]) ;
//! 2. l'appelant récupère les candidats de la cible et dépose une **demande de
//!    punch** que le rendez-vous mémorise ([`RendezvousClient::request_punch`]) ;
//! 3. la cible relève ses demandes en attente ([`RendezvousClient::poll_punch`],
//!    typiquement dans la même boucle que le heartbeat) ;
//! 4. les deux pairs lancent alors [`punch::udp_hole_punch`] simultanément,
//!    avec des rôles opposés — voir [`punch`] pour la théorie des NAT et
//!    [`nat::detect_nat_type`] pour anticiper le repli relais (`nd-relay`).
//!
//! Le module [`connect`] câble ces étapes en un **connecteur de bout en
//! bout** : [`establish_p2p`] (appelant) / [`await_p2p`] (appelé) rendent un
//! socket UDP percé prêt à porter QUIC (via `nd-transport`), ou signalent le
//! repli relais.
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

/// Connecteur P2P de bout en bout : STUN + candidats + punch coordonnés.
pub mod connect;
/// Détection best-effort du type de NAT (comparaison de deux serveurs STUN).
pub mod nat;
/// UDP hole punching coordonné par le rendez-vous (théorie des NAT incluse).
pub mod punch;
/// Client STUN (RFC 5389) : découverte de l'adresse réflexive publique.
pub mod stun;

pub use connect::{
    await_p2p, await_p2p_with_timeout, establish_p2p, establish_p2p_with_timeout, ConnAttempt,
    DirectPath, IncomingPath, P2pIncoming,
};

/// Enregistrement d'un pair résolu par son ID.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    /// Adresse (UDP/QUIC) du pair contrôlé.
    pub addr: SocketAddr,
    /// Certificat auto-signé du pair (à épingler côté client, voir `nd-transport`).
    pub cert_der: Vec<u8>,
}

/// Demande de punch en attente, relayée par le serveur de rendez-vous.
///
/// Déposée par l'appelant via [`RendezvousClient::request_punch`], relevée par
/// la cible via [`RendezvousClient::poll_punch`] : la cible lance alors
/// [`punch::udp_hole_punch`] vers ces candidats (rôle
/// [`punch::PunchRole::Callee`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PunchDemand {
    /// ID du pair appelant qui demande l'ouverture d'un chemin.
    pub from: NovaId,
    /// Candidats de l'appelant (adresse locale + adresse réflexive STUN).
    pub candidates: Vec<SocketAddr>,
}

/// TTL de présence par défaut : au-delà sans heartbeat, un pair est hors-ligne.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Nombre maximal de candidats par pair accepté par le protocole.
///
/// En pratique deux suffisent (adresse locale + réflexive STUN) ; la marge
/// couvre le multi-interface. Au-delà, le message est rejeté (anti-abus).
pub const MAX_CANDIDATES: usize = 16;

/// Durée de vie d'une demande de punch mémorisée côté serveur : au-delà, la
/// fenêtre de simultanéité du punch est de toute façon manquée.
pub const PUNCH_TTL: Duration = Duration::from_secs(30);

/// Nombre maximal de demandes de punch mémorisées par cible (anti-abus) ;
/// la plus ancienne est évincée quand la file est pleine.
const MAX_DEMANDES_PAR_CIBLE: usize = 8;

/// Entrée interne du registre : coordonnées du pair + dernière activité.
struct PeerEntry {
    /// Adresse textuelle (UDP/QUIC) publiée par le pair.
    addr: String,
    /// Certificat auto-signé (DER) du pair.
    cert: Vec<u8>,
    /// Candidats de hole punching publiés par le pair (adresses textuelles),
    /// vides tant que le pair n'a rien déposé. Réinitialisés à chaque
    /// `Register` (une nouvelle adresse rend les anciens candidats caducs).
    candidats: Vec<String>,
    /// Horodatage du dernier `Register`/`Heartbeat`/`PublishCandidates` reçu.
    last_seen: Instant,
}

/// Table interne du registre : ID → entrée (adresse, certificat, dernière activité).
type PeerMap = HashMap<u64, PeerEntry>;

/// Demande de punch mémorisée côté serveur, en attente de relève par la cible.
struct DemandeEnAttente {
    /// ID de l'appelant.
    de: u64,
    /// Candidats de l'appelant (adresses textuelles).
    candidats: Vec<String>,
    /// Horodatage du dépôt (expiration au bout de [`PUNCH_TTL`]).
    deposee: Instant,
}

/// Files de demandes de punch : ID cible → demandes en attente.
type PunchMap = HashMap<u64, Vec<DemandeEnAttente>>;

/// Registre partagé du serveur : ID → (adresse, certificat, présence).
///
/// Les clones partagent la même table ; le TTL de présence est fixé à la
/// construction ([`Registry::new`] → [`DEFAULT_TTL`], sinon [`Registry::with_ttl`]).
#[derive(Clone)]
pub struct Registry {
    peers: Arc<Mutex<PeerMap>>,
    /// Demandes de punch en attente de relève, par ID cible.
    punches: Arc<Mutex<PunchMap>>,
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
            punches: Arc::default(),
            ttl,
        }
    }

    /// TTL de présence du registre.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Insère (ou remplace) un enregistrement, horodaté à maintenant.
    /// Les candidats de punch d'un éventuel ancien enregistrement sont
    /// abandonnés (l'adresse a pu changer, ils sont caducs).
    fn insert(&self, id: u64, addr: String, cert: Vec<u8>) {
        self.peers.lock().unwrap().insert(
            id,
            PeerEntry {
                addr,
                cert,
                candidats: Vec::new(),
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

    /// Remplace les candidats de punch d'un ID enregistré et non périmé, et
    /// rafraîchit sa dernière activité (déposer des candidats est un signe de
    /// vie). Renvoie `false` si l'ID est inconnu ou périmé.
    fn set_candidates(&self, id: u64, candidats: Vec<String>) -> bool {
        match self.peers.lock().unwrap().get_mut(&id) {
            Some(entry) if entry.last_seen.elapsed() <= self.ttl => {
                entry.candidats = candidats;
                entry.last_seen = Instant::now();
                true
            }
            _ => false,
        }
    }

    /// Candidats publiés par un ID enregistré et non périmé (`None` sinon ;
    /// la liste peut être vide si le pair n'a rien déposé).
    fn candidates_of(&self, id: u64) -> Option<Vec<String>> {
        let peers = self.peers.lock().unwrap();
        let entry = peers.get(&id)?;
        (entry.last_seen.elapsed() <= self.ttl).then(|| entry.candidats.clone())
    }

    /// Mémorise une demande de punch pour `cible` (qui doit être en ligne).
    /// File bornée à [`MAX_DEMANDES_PAR_CIBLE`] : la plus ancienne est évincée.
    fn push_punch(&self, cible: u64, de: u64, candidats: Vec<String>) -> bool {
        if !self.is_online(cible, self.ttl) {
            return false;
        }
        let mut punches = self.punches.lock().unwrap();
        let file = punches.entry(cible).or_default();
        if file.len() >= MAX_DEMANDES_PAR_CIBLE {
            file.remove(0);
        }
        file.push(DemandeEnAttente {
            de,
            candidats,
            deposee: Instant::now(),
        });
        true
    }

    /// Relève (et vide) les demandes de punch non périmées pour `id`.
    fn drain_punches(&self, id: u64) -> Vec<(u64, Vec<String>)> {
        self.punches
            .lock()
            .unwrap()
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.deposee.elapsed() <= PUNCH_TTL)
            .map(|d| (d.de, d.candidats))
            .collect()
    }

    /// Retire les entrées non rafraîchies depuis plus de `ttl` et renvoie le
    /// nombre de **pairs** retirés. Purge au passage les demandes de punch
    /// périmées ([`PUNCH_TTL`]) ou visant un pair retiré/absent.
    pub fn sweep_expired(&self, ttl: Duration) -> usize {
        let mut peers = self.peers.lock().unwrap();
        let avant = peers.len();
        peers.retain(|_, entry| entry.last_seen.elapsed() <= ttl);
        let retires = avant - peers.len();
        self.punches.lock().unwrap().retain(|cible, file| {
            file.retain(|d| d.deposee.elapsed() <= PUNCH_TTL);
            !file.is_empty() && peers.contains_key(cible)
        });
        retires
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
    /// Dépose (remplace) les candidats de punch de l'ID : adresse locale +
    /// adresse réflexive STUN, en textuel (plan 05, NAT traversal).
    PublishCandidates {
        id: u64,
        candidates: Vec<String>,
    },
    /// Récupère les candidats publiés par un pair (plan 05, NAT traversal).
    GetCandidates {
        id: u64,
    },
    /// Demande de punch de `from` vers `target` : le serveur mémorise la
    /// demande (avec les candidats de l'appelant) pour que la cible la relève,
    /// et renvoie en retour les candidats de la cible (plan 05, NAT traversal).
    Punch {
        from: u64,
        target: u64,
        candidates: Vec<String>,
    },
    /// Relève (et vide) les demandes de punch en attente pour l'ID
    /// (plan 05, NAT traversal).
    PollPunch {
        id: u64,
    },
}

/// Demande de punch sur le fil : ID appelant + ses candidats (textuels).
struct DemandeFil {
    de: u64,
    candidats: Vec<String>,
}

enum Response {
    Registered,
    Found {
        addr: String,
        cert: Vec<u8>,
    },
    NotFound,
    /// Candidats publiés par le pair demandé (possiblement vides).
    Candidates {
        candidates: Vec<String>,
    },
    /// Demandes de punch en attente pour l'ID interrogé.
    PunchRequests {
        requests: Vec<DemandeFil>,
    },
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

/// Encode une liste de chaînes : compteur u16 + chaînes préfixées.
fn put_liste_chaines(out: &mut Vec<u8>, items: &[String]) {
    out.extend_from_slice(&(items.len() as u16).to_be_bytes());
    for item in items {
        put_bytes(out, item.as_bytes());
    }
}

fn read_u8(d: &[u8], p: &mut usize) -> Option<u8> {
    let v = *d.get(*p)?;
    *p += 1;
    Some(v)
}

fn read_u16(d: &[u8], p: &mut usize) -> Option<u16> {
    let v = u16::from_be_bytes(d.get(*p..*p + 2)?.try_into().ok()?);
    *p += 2;
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

fn read_chaine(d: &[u8], p: &mut usize) -> Option<String> {
    String::from_utf8(read_bytes(d, p)?).ok()
}

/// Décode une liste de chaînes, bornée à `max` éléments (anti-abus).
fn read_liste_chaines(d: &[u8], p: &mut usize, max: usize) -> Option<Vec<String>> {
    let n = usize::from(read_u16(d, p)?);
    if n > max {
        return None;
    }
    (0..n).map(|_| read_chaine(d, p)).collect()
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
            Request::PublishCandidates { id, candidates } => {
                out.push(4);
                out.extend_from_slice(&id.to_be_bytes());
                put_liste_chaines(&mut out, candidates);
            }
            Request::GetCandidates { id } => {
                out.push(5);
                out.extend_from_slice(&id.to_be_bytes());
            }
            Request::Punch {
                from,
                target,
                candidates,
            } => {
                out.push(6);
                out.extend_from_slice(&from.to_be_bytes());
                out.extend_from_slice(&target.to_be_bytes());
                put_liste_chaines(&mut out, candidates);
            }
            Request::PollPunch { id } => {
                out.push(7);
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
                let addr = read_chaine(d, &mut p)?;
                let cert = read_bytes(d, &mut p)?;
                Some(Request::Register { id, addr, cert })
            }
            2 => Some(Request::Lookup {
                id: read_u64(d, &mut p)?,
            }),
            3 => Some(Request::Heartbeat {
                id: read_u64(d, &mut p)?,
            }),
            4 => Some(Request::PublishCandidates {
                id: read_u64(d, &mut p)?,
                candidates: read_liste_chaines(d, &mut p, MAX_CANDIDATES)?,
            }),
            5 => Some(Request::GetCandidates {
                id: read_u64(d, &mut p)?,
            }),
            6 => Some(Request::Punch {
                from: read_u64(d, &mut p)?,
                target: read_u64(d, &mut p)?,
                candidates: read_liste_chaines(d, &mut p, MAX_CANDIDATES)?,
            }),
            7 => Some(Request::PollPunch {
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
            Response::Candidates { candidates } => {
                out.push(3);
                put_liste_chaines(&mut out, candidates);
            }
            Response::PunchRequests { requests } => {
                out.push(4);
                out.extend_from_slice(&(requests.len() as u16).to_be_bytes());
                for demande in requests {
                    out.extend_from_slice(&demande.de.to_be_bytes());
                    put_liste_chaines(&mut out, &demande.candidats);
                }
            }
        }
        out
    }

    fn from_bytes(d: &[u8]) -> Option<Response> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            0 => Some(Response::Registered),
            1 => {
                let addr = read_chaine(d, &mut p)?;
                let cert = read_bytes(d, &mut p)?;
                Some(Response::Found { addr, cert })
            }
            2 => Some(Response::NotFound),
            3 => Some(Response::Candidates {
                candidates: read_liste_chaines(d, &mut p, MAX_CANDIDATES)?,
            }),
            4 => {
                let n = usize::from(read_u16(d, &mut p)?);
                if n > MAX_DEMANDES_PAR_CIBLE {
                    return None;
                }
                let requests = (0..n)
                    .map(|_| {
                        Some(DemandeFil {
                            de: read_u64(d, &mut p)?,
                            candidats: read_liste_chaines(d, &mut p, MAX_CANDIDATES)?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Response::PunchRequests { requests })
            }
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
        // Dépôt de candidats : refusé si l'ID n'est pas enregistré (le pair
        // doit d'abord faire un `Register`) ou s'il est périmé.
        Some(Request::PublishCandidates { id, candidates }) => {
            if registry.set_candidates(id, candidates) {
                Response::Registered
            } else {
                Response::NotFound
            }
        }
        Some(Request::GetCandidates { id }) => match registry.candidates_of(id) {
            Some(candidates) => Response::Candidates { candidates },
            None => Response::NotFound,
        },
        // Demande de punch : la cible doit être en ligne ; on mémorise la
        // demande (candidats de l'appelant inclus) et on renvoie les candidats
        // de la cible — un seul aller-retour pour l'appelant.
        Some(Request::Punch {
            from,
            target,
            candidates,
        }) => match registry.candidates_of(target) {
            Some(candidats_cible) => {
                registry.push_punch(target, from, candidates);
                Response::Candidates {
                    candidates: candidats_cible,
                }
            }
            None => Response::NotFound,
        },
        // Relève des demandes de punch : réservée aux ID enregistrés en ligne
        // (mêmes conditions que le heartbeat).
        Some(Request::PollPunch { id }) => {
            if registry.is_online(id, registry.ttl) {
                let requests = registry
                    .drain_punches(id)
                    .into_iter()
                    .map(|(de, candidats)| DemandeFil { de, candidats })
                    .collect();
                Response::PunchRequests { requests }
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

    /// Adresse du serveur de rendez-vous interrogé par ce client. Sert de
    /// référence de routage au connecteur ([`connect`]) pour déterminer
    /// l'interface de sortie du candidat local.
    #[must_use]
    pub fn server_addr(&self) -> SocketAddr {
        self.server
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

    /// Dépose (remplace) les candidats de punch de l'ID local : adresse
    /// locale + adresse réflexive découverte via [`stun::discover_public_addr`].
    /// L'ID doit déjà être enregistré ([`RendezvousClient::register`]) et en
    /// ligne. Une liste vide efface les candidats publiés.
    ///
    /// # Errors
    /// Erreur si la liste dépasse [`MAX_CANDIDATES`], si l'ID n'est pas (ou
    /// plus) enregistré, ou en cas d'erreur réseau/protocole.
    pub fn publish_candidates(&self, id: NovaId, candidates: &[SocketAddr]) -> Result<()> {
        let req = Request::PublishCandidates {
            id: id.as_u64(),
            candidates: encoder_candidats(candidates)?,
        };
        match self.round_trip(&req)? {
            Response::Registered => Ok(()),
            Response::NotFound => Err(NdError::Protocol(format!("ID {id} non enregistré"))),
            _ => Err(NdError::Protocol("réponse inattendue".into())),
        }
    }

    /// Récupère les candidats de punch publiés par un pair (liste vide si le
    /// pair est en ligne mais n'a rien déposé).
    ///
    /// # Errors
    /// Erreur si l'ID est introuvable/hors-ligne, ou en cas d'erreur
    /// réseau/protocole.
    pub fn peer_candidates(&self, id: NovaId) -> Result<Vec<SocketAddr>> {
        let req = Request::GetCandidates { id: id.as_u64() };
        match self.round_trip(&req)? {
            Response::Candidates { candidates } => decoder_candidats(&candidates),
            Response::NotFound => Err(NdError::Protocol(format!("ID {id} introuvable"))),
            _ => Err(NdError::Protocol("réponse inattendue".into())),
        }
    }

    /// Demande de punch : dépose au rendez-vous une demande de `from` vers
    /// `target` avec les `candidates` de l'appelant, et récupère en retour les
    /// candidats de la cible. La cible relèvera la demande via
    /// [`RendezvousClient::poll_punch`] ; les deux pairs appellent alors
    /// [`punch::udp_hole_punch`] simultanément (appelant :
    /// [`punch::PunchRole::Caller`]).
    ///
    /// # Errors
    /// Erreur si la liste dépasse [`MAX_CANDIDATES`], si la cible est
    /// introuvable/hors-ligne, ou en cas d'erreur réseau/protocole.
    pub fn request_punch(
        &self,
        from: NovaId,
        target: NovaId,
        candidates: &[SocketAddr],
    ) -> Result<Vec<SocketAddr>> {
        let req = Request::Punch {
            from: from.as_u64(),
            target: target.as_u64(),
            candidates: encoder_candidats(candidates)?,
        };
        match self.round_trip(&req)? {
            Response::Candidates { candidates } => decoder_candidats(&candidates),
            Response::NotFound => Err(NdError::Protocol(format!("ID {target} introuvable"))),
            _ => Err(NdError::Protocol("réponse inattendue".into())),
        }
    }

    /// Relève (et vide) les demandes de punch en attente pour l'ID local.
    /// À appeler périodiquement, typiquement dans la même boucle que le
    /// [`RendezvousClient::heartbeat`] : pour chaque demande, lancer
    /// [`punch::udp_hole_punch`] vers ses candidats (rôle
    /// [`punch::PunchRole::Callee`]).
    ///
    /// # Errors
    /// Erreur si l'ID n'est pas (ou plus) enregistré, ou en cas d'erreur
    /// réseau/protocole.
    pub fn poll_punch(&self, id: NovaId) -> Result<Vec<PunchDemand>> {
        let req = Request::PollPunch { id: id.as_u64() };
        match self.round_trip(&req)? {
            Response::PunchRequests { requests } => requests
                .into_iter()
                .map(|d| {
                    Ok(PunchDemand {
                        from: NovaId(d.de),
                        candidates: decoder_candidats(&d.candidats)?,
                    })
                })
                .collect(),
            Response::NotFound => Err(NdError::Protocol(format!("ID {id} non enregistré"))),
            _ => Err(NdError::Protocol("réponse inattendue".into())),
        }
    }
}

/// Encode des candidats en textuel pour le fil, en vérifiant la borne
/// [`MAX_CANDIDATES`].
fn encoder_candidats(candidats: &[SocketAddr]) -> Result<Vec<String>> {
    if candidats.len() > MAX_CANDIDATES {
        return Err(NdError::Protocol(format!(
            "trop de candidats ({}, maximum {MAX_CANDIDATES})",
            candidats.len()
        )));
    }
    Ok(candidats.iter().map(ToString::to_string).collect())
}

/// Décode des candidats textuels reçus du fil en adresses.
fn decoder_candidats(candidats: &[String]) -> Result<Vec<SocketAddr>> {
    candidats
        .iter()
        .map(|c| {
            c.parse()
                .map_err(|_| NdError::Protocol(format!("candidat invalide : {c}")))
        })
        .collect()
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

    // --- Nouveaux messages NAT traversal (plan 05) -------------------------

    #[test]
    fn publish_candidates_roundtrip_preserve_les_champs() {
        let bytes = Request::PublishCandidates {
            id: 11,
            candidates: vec!["192.168.1.2:7000".into(), "203.0.113.5:41000".into()],
        }
        .to_bytes();
        match Request::from_bytes(&bytes) {
            Some(Request::PublishCandidates { id, candidates }) => {
                assert_eq!(id, 11);
                assert_eq!(candidates, vec!["192.168.1.2:7000", "203.0.113.5:41000"]);
            }
            _ => panic!("désérialisation PublishCandidates échouée"),
        }
        // Liste vide (effacement) : valide aussi.
        let bytes = Request::PublishCandidates {
            id: 12,
            candidates: vec![],
        }
        .to_bytes();
        assert!(matches!(
            Request::from_bytes(&bytes),
            Some(Request::PublishCandidates { id: 12, candidates }) if candidates.is_empty()
        ));
    }

    #[test]
    fn get_candidates_et_poll_punch_roundtrip() {
        match Request::from_bytes(&Request::GetCandidates { id: 21 }.to_bytes()) {
            Some(Request::GetCandidates { id }) => assert_eq!(id, 21),
            _ => panic!("désérialisation GetCandidates échouée"),
        }
        match Request::from_bytes(&Request::PollPunch { id: 22 }.to_bytes()) {
            Some(Request::PollPunch { id }) => assert_eq!(id, 22),
            _ => panic!("désérialisation PollPunch échouée"),
        }
    }

    #[test]
    fn request_punch_roundtrip_preserve_les_champs() {
        let bytes = Request::Punch {
            from: 100,
            target: 200,
            candidates: vec!["10.0.0.2:6000".into()],
        }
        .to_bytes();
        match Request::from_bytes(&bytes) {
            Some(Request::Punch {
                from,
                target,
                candidates,
            }) => {
                assert_eq!(from, 100);
                assert_eq!(target, 200);
                assert_eq!(candidates, vec!["10.0.0.2:6000"]);
            }
            _ => panic!("désérialisation RequestPunch échouée"),
        }
    }

    #[test]
    fn response_candidates_et_punch_requests_roundtrip() {
        let bytes = Response::Candidates {
            candidates: vec!["198.51.100.2:443".into()],
        }
        .to_bytes();
        match Response::from_bytes(&bytes) {
            Some(Response::Candidates { candidates }) => {
                assert_eq!(candidates, vec!["198.51.100.2:443"]);
            }
            _ => panic!("désérialisation Candidates échouée"),
        }

        let bytes = Response::PunchRequests {
            requests: vec![
                DemandeFil {
                    de: 100,
                    candidats: vec!["10.0.0.2:6000".into(), "203.0.113.5:41000".into()],
                },
                DemandeFil {
                    de: 101,
                    candidats: vec![],
                },
            ],
        }
        .to_bytes();
        match Response::from_bytes(&bytes) {
            Some(Response::PunchRequests { requests }) => {
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0].de, 100);
                assert_eq!(
                    requests[0].candidats,
                    vec!["10.0.0.2:6000", "203.0.113.5:41000"]
                );
                assert_eq!(requests[1].de, 101);
                assert!(requests[1].candidats.is_empty());
            }
            _ => panic!("désérialisation PunchRequests échouée"),
        }
    }

    #[test]
    fn parse_rejette_les_listes_de_candidats_trop_longues() {
        let trop = vec!["127.0.0.1:1".to_string(); MAX_CANDIDATES + 1];
        let bytes = Request::PublishCandidates {
            id: 1,
            candidates: trop,
        }
        .to_bytes();
        assert!(Request::from_bytes(&bytes).is_none());
    }

    #[test]
    fn echange_de_candidats_et_demande_de_punch() {
        let (_registry, client) = serveur_de_test(Duration::from_secs(60));
        let cible = NovaId(100);
        let appelant = NovaId(200);
        client.register(cible, adresse_bidon(), &[1]).unwrap();

        // La cible dépose ses candidats (locale + réflexive STUN).
        let candidats_cible: Vec<SocketAddr> = vec![
            "192.168.1.10:7000".parse().unwrap(),
            "203.0.113.5:41000".parse().unwrap(),
        ];
        client.publish_candidates(cible, &candidats_cible).unwrap();
        assert_eq!(client.peer_candidates(cible).unwrap(), candidats_cible);

        // L'appelant demande un punch : il récupère les candidats de la cible
        // et le serveur mémorise sa demande.
        let candidats_appelant: Vec<SocketAddr> = vec!["10.0.0.2:6000".parse().unwrap()];
        let recus = client
            .request_punch(appelant, cible, &candidats_appelant)
            .unwrap();
        assert_eq!(recus, candidats_cible);

        // La cible relève la demande… une seule fois (la file est vidée).
        let demandes = client.poll_punch(cible).unwrap();
        assert_eq!(demandes.len(), 1);
        assert_eq!(demandes[0].from, appelant);
        assert_eq!(demandes[0].candidates, candidats_appelant);
        assert!(client.poll_punch(cible).unwrap().is_empty());
    }

    #[test]
    fn nouveau_register_reinitialise_les_candidats() {
        let (_registry, client) = serveur_de_test(Duration::from_secs(60));
        let id = NovaId(31);
        client.register(id, adresse_bidon(), &[1]).unwrap();
        client
            .publish_candidates(id, &["192.168.1.10:7000".parse().unwrap()])
            .unwrap();
        assert_eq!(client.peer_candidates(id).unwrap().len(), 1);

        // Ré-enregistrement (nouvelle adresse) : les candidats sont caducs.
        client.register(id, adresse_bidon(), &[1]).unwrap();
        assert!(client.peer_candidates(id).unwrap().is_empty());
    }

    #[test]
    fn candidats_et_punch_exigent_un_pair_en_ligne() {
        let ttl = Duration::from_millis(50);
        let (_registry, client) = serveur_de_test(ttl);
        let inconnu = NovaId(404);
        let candidats: Vec<SocketAddr> = vec!["10.0.0.2:6000".parse().unwrap()];

        // ID jamais enregistré : tout échoue.
        assert!(client.publish_candidates(inconnu, &candidats).is_err());
        assert!(client.peer_candidates(inconnu).is_err());
        assert!(client
            .request_punch(NovaId(1), inconnu, &candidats)
            .is_err());
        assert!(client.poll_punch(inconnu).is_err());

        // ID enregistré puis périmé (sans heartbeat) : idem.
        let perime = NovaId(50);
        client.register(perime, adresse_bidon(), &[1]).unwrap();
        client.publish_candidates(perime, &candidats).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(client.publish_candidates(perime, &candidats).is_err());
        assert!(client.peer_candidates(perime).is_err());
        assert!(client.request_punch(NovaId(1), perime, &candidats).is_err());
        assert!(client.poll_punch(perime).is_err());
    }

    #[test]
    fn trop_de_candidats_refuse_cote_client() {
        let (_registry, client) = serveur_de_test(Duration::from_secs(60));
        let id = NovaId(60);
        client.register(id, adresse_bidon(), &[1]).unwrap();
        let trop: Vec<SocketAddr> = (0..=MAX_CANDIDATES as u16)
            .map(|i| format!("127.0.0.1:{}", 1000 + i).parse().unwrap())
            .collect();
        assert!(client.publish_candidates(id, &trop).is_err());
    }

    #[test]
    fn file_de_punch_bornee_evince_les_plus_anciennes() {
        let (registry, client) = serveur_de_test(Duration::from_secs(60));
        let cible = NovaId(70);
        client.register(cible, adresse_bidon(), &[1]).unwrap();

        for i in 0..(MAX_DEMANDES_PAR_CIBLE as u64 + 3) {
            let ok = registry.push_punch(cible.as_u64(), 1000 + i, vec![]);
            assert!(ok);
        }
        let demandes = client.poll_punch(cible).unwrap();
        assert_eq!(demandes.len(), MAX_DEMANDES_PAR_CIBLE);
        // Les plus anciennes (1000, 1001, 1002) ont été évincées.
        assert_eq!(demandes[0].from, NovaId(1003));
    }

    #[test]
    fn sweep_purge_les_demandes_de_punch_orphelines() {
        let ttl = Duration::from_millis(50);
        let registry = Registry::with_ttl(ttl);
        registry.insert(80, "127.0.0.1:1".into(), vec![]);
        assert!(registry.push_punch(80, 81, vec!["10.0.0.2:6000".into()]));
        assert_eq!(registry.punches.lock().unwrap().len(), 1);

        // La cible expire : le balayage retire le pair ET sa file de punch.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(registry.sweep_expired(ttl), 1);
        assert!(registry.punches.lock().unwrap().is_empty());
    }
}
