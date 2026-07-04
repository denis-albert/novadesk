//! Transport QUIC concret (crate `quinn`), reliant le trait synchrone [`Transport`]
//! à quinn (asynchrone) via un runtime Tokio global et des files `mpsc`.
//!
//! Modèle : chaque pair ouvre un flux unidirectionnel QUIC pour émettre et accepte
//! celui du pair pour recevoir. Les canaux logiques (vidéo/audio/input/…) sont
//! multiplexés par un en-tête de trame `[tag u8][moniteur u32][longueur u32]`.
//! Voir plan 04 pour la cible complète (datagrammes non fiables + FEC pour la vidéo,
//! flux séparés par canal) ; ce premier jet utilise un flux fiable et unique.
//!
//! Sécurité : QUIC fournit ici le chiffrement TLS 1.3 « de saut ». Le certificat est
//! auto-signé et **épinglé** (le client fait confiance au certificat exact du serveur).
//! La confiance de bout en bout reposera sur Noise (voir plan 06).

use std::net::SocketAddr;
use std::sync::{Arc, Once, OnceLock};

use nd_proto::{ChannelKind, MonitorId, NdError, Reliability, Result};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, error::TryRecvError};

use crate::{ChannelHandle, PathEstimate, Transport};

/// Préfixe écrit en tête de chaque flux pour valider l'appairage.
const MAGIC: &[u8; 4] = b"NDQ1";
/// Nom de serveur présenté au handshake TLS (doit figurer dans le SAN du certificat).
const SERVER_NAME: &str = "novadesk";
/// Taille max d'une charge utile de trame (garde-fou anti-abus).
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Runtime Tokio partagé par tout le transport (créé à la première utilisation).
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("création du runtime Tokio"))
}

/// Installe un fournisseur cryptographique par défaut pour rustls (une seule fois).
fn ensure_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Écouteur QUIC côté serveur (machine contrôlée / hôte).
pub struct Listener {
    endpoint: Endpoint,
    cert_der: Vec<u8>,
    local_addr: SocketAddr,
}

impl Listener {
    /// Adresse locale effective (port éphémère résolu).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Certificat auto-signé (DER) à transmettre au client pour l'épinglage.
    #[must_use]
    pub fn server_cert_der(&self) -> Vec<u8> {
        self.cert_der.clone()
    }

    /// Accepte la prochaine connexion entrante (bloquant).
    pub fn accept(&self) -> Result<Box<dyn Transport>> {
        let (conn, send, recv) = runtime().block_on(async {
            let incoming = self
                .endpoint
                .accept()
                .await
                .ok_or_else(|| NdError::Transport("endpoint fermé".into()))?;
            let conn = incoming
                .await
                .map_err(|e| NdError::Transport(format!("connexion entrante : {e}")))?;
            let streams = setup_streams(&conn).await?;
            Ok::<_, NdError>((conn, streams.0, streams.1))
        })?;
        Ok(Box::new(spawn_transport(conn, None, send, recv)))
    }
}

/// Ouvre un écouteur QUIC sur l'adresse donnée (génère un certificat auto-signé).
pub fn bind(addr: SocketAddr) -> Result<Listener> {
    ensure_provider();
    let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
        .map_err(|e| NdError::Transport(format!("génération du certificat : {e}")))?;
    let cert_der: CertificateDer<'static> = ck.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(ck.key_pair.serialize_der().into());

    let server_cfg = quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], key)
        .map_err(|e| NdError::Transport(format!("config serveur TLS : {e}")))?;

    // La création de l'endpoint (socket UDP + reactor) doit avoir lieu dans le
    // contexte du runtime Tokio.
    let (endpoint, local_addr) = runtime().block_on(async move {
        let endpoint = Endpoint::server(server_cfg, addr)
            .map_err(|e| NdError::Transport(format!("endpoint serveur : {e}")))?;
        let local = endpoint
            .local_addr()
            .map_err(|e| NdError::Transport(format!("adresse locale : {e}")))?;
        Ok::<_, NdError>((endpoint, local))
    })?;

    Ok(Listener {
        endpoint,
        cert_der: cert_der.as_ref().to_vec(),
        local_addr,
    })
}

/// Se connecte à un pair QUIC en épinglant son certificat auto-signé.
///
/// `server_cert_der` est le certificat obtenu via [`Listener::server_cert_der`]
/// (dans la vraie vie il transitera par le serveur de rendez-vous, voir plan 05).
pub fn connect(remote: SocketAddr, server_cert_der: &[u8]) -> Result<Box<dyn Transport>> {
    ensure_provider();
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(server_cert_der.to_vec()))
        .map_err(|e| NdError::Transport(format!("ajout du certificat racine : {e}")))?;
    let client_cfg = quinn::ClientConfig::with_root_certificates(Arc::new(roots))
        .map_err(|e| NdError::Transport(format!("config client TLS : {e}")))?;

    let (endpoint, conn, send, recv) = runtime().block_on(async move {
        let bind_addr: SocketAddr = "0.0.0.0:0".parse().expect("adresse de bind valide");
        let mut endpoint = Endpoint::client(bind_addr)
            .map_err(|e| NdError::Transport(format!("endpoint client : {e}")))?;
        endpoint.set_default_client_config(client_cfg);
        let conn = endpoint
            .connect(remote, SERVER_NAME)
            .map_err(|e| NdError::Transport(format!("connexion : {e}")))?
            .await
            .map_err(|e| NdError::Transport(format!("handshake : {e}")))?;
        let (send, recv) = setup_streams(&conn).await?;
        Ok::<_, NdError>((endpoint, conn, send, recv))
    })?;

    Ok(Box::new(spawn_transport(conn, Some(endpoint), send, recv)))
}

/// Ouvre le flux d'émission, écrit le MAGIC, accepte le flux du pair et valide son MAGIC.
async fn setup_streams(conn: &Connection) -> Result<(SendStream, RecvStream)> {
    let mut send = conn
        .open_uni()
        .await
        .map_err(|e| NdError::Transport(format!("open_uni : {e}")))?;
    send.write_all(MAGIC)
        .await
        .map_err(|e| NdError::Transport(format!("écriture magic : {e}")))?;

    let mut recv = conn
        .accept_uni()
        .await
        .map_err(|e| NdError::Transport(format!("accept_uni : {e}")))?;
    let mut buf = [0u8; MAGIC.len()];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| NdError::Transport(format!("lecture magic : {e}")))?;
    if &buf != MAGIC {
        return Err(NdError::Transport("magic d'appairage invalide".into()));
    }
    Ok((send, recv))
}

/// Tag + moniteur pour l'en-tête de trame d'un canal.
fn kind_tag(kind: ChannelKind) -> (u8, u32) {
    match kind {
        ChannelKind::Control => (0, 0),
        ChannelKind::Audio => (1, 0),
        ChannelKind::Input => (2, 0),
        ChannelKind::Files => (3, 0),
        ChannelKind::Video(m) => (4, m.0),
    }
}

/// Reconstruit le canal depuis le tag/moniteur reçus.
fn tag_kind(tag: u8, monitor: u32) -> Option<ChannelKind> {
    match tag {
        0 => Some(ChannelKind::Control),
        1 => Some(ChannelKind::Audio),
        2 => Some(ChannelKind::Input),
        3 => Some(ChannelKind::Files),
        4 => Some(ChannelKind::Video(MonitorId(monitor))),
        _ => None,
    }
}

/// Tâche d'émission : sérialise les messages sortants sur le flux QUIC.
async fn writer(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<(ChannelKind, Vec<u8>)>) {
    while let Some((kind, data)) = rx.recv().await {
        let (tag, monitor) = kind_tag(kind);
        let mut hdr = [0u8; 9];
        hdr[0] = tag;
        hdr[1..5].copy_from_slice(&monitor.to_be_bytes());
        hdr[5..9].copy_from_slice(&(data.len() as u32).to_be_bytes());
        if send.write_all(&hdr).await.is_err() || send.write_all(&data).await.is_err() {
            break;
        }
    }
    let _ = send.finish();
}

/// Tâche de réception : dé-sérialise les trames entrantes et les pousse dans la file.
async fn reader(mut recv: RecvStream, tx: mpsc::UnboundedSender<(ChannelKind, Vec<u8>)>) {
    loop {
        let mut hdr = [0u8; 9];
        if recv.read_exact(&mut hdr).await.is_err() {
            break;
        }
        let tag = hdr[0];
        let monitor = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
        let len = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
        if len > MAX_FRAME {
            break;
        }
        let mut payload = vec![0u8; len];
        if recv.read_exact(&mut payload).await.is_err() {
            break;
        }
        if let Some(kind) = tag_kind(tag, monitor) {
            if tx.send((kind, payload)).is_err() {
                break;
            }
        }
    }
}

/// Assemble un [`QuicTransport`] et lance les tâches d'E/S.
fn spawn_transport(
    conn: Connection,
    endpoint: Option<Endpoint>,
    send: SendStream,
    recv: RecvStream,
) -> QuicTransport {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    runtime().spawn(writer(send, out_rx));
    runtime().spawn(reader(recv, in_tx));
    QuicTransport {
        conn,
        endpoint,
        outbound_tx: out_tx,
        inbound_rx: in_rx,
        channels: Vec::new(),
    }
}

/// Transport QUIC : implémente le trait [`Transport`] synchrone.
pub struct QuicTransport {
    conn: Connection,
    /// Conservé uniquement pour maintenir l'endpoint client en vie (RAII).
    #[allow(dead_code)]
    endpoint: Option<Endpoint>,
    outbound_tx: mpsc::UnboundedSender<(ChannelKind, Vec<u8>)>,
    inbound_rx: mpsc::UnboundedReceiver<(ChannelKind, Vec<u8>)>,
    /// Index de canal -> type de canal.
    channels: Vec<ChannelKind>,
}

impl Transport for QuicTransport {
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
        if let Some(i) = self.channels.iter().position(|k| *k == kind) {
            return ChannelHandle(i as u32);
        }
        self.channels.push(kind);
        ChannelHandle((self.channels.len() - 1) as u32)
    }

    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, _reliability: Reliability) -> Result<()> {
        // La fiabilité est ignorée pour l'instant (flux fiable unique) ; datagrammes +
        // FEC pour la vidéo à venir (plan 04).
        let kind = *self
            .channels
            .get(ch.0 as usize)
            .ok_or_else(|| NdError::Transport("handle de canal inconnu".into()))?;
        self.outbound_tx
            .send((kind, data))
            .map_err(|_| NdError::Transport("connexion fermée".into()))
    }

    fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
        match self.inbound_rx.try_recv() {
            Ok((kind, data)) => {
                let handle = self.open_channel(kind);
                Ok(Some((handle, data)))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Ok(None),
        }
    }

    fn path_estimate(&self) -> PathEstimate {
        PathEstimate {
            rtt_us: self.conn.rtt().as_micros() as u64,
            loss_ratio: 0.0,
            estimated_bandwidth_kbps: 0,
        }
    }
}
