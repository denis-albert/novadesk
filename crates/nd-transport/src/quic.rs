//! Transport QUIC concret (crate `quinn`), reliant le trait synchrone [`Transport`]
//! à quinn (asynchrone) via un runtime Tokio global et des files `mpsc`.
//!
//! Deux chemins de données coexistent (plan 04) :
//!
//! * **Flux fiable ordonné** (un flux unidirectionnel QUIC par sens) pour les envois
//!   [`Reliability::Reliable`] (input, contrôle, fichiers) : en-tête de trame
//!   `[tag u8][moniteur u32][longueur u32]` puis charge utile ;
//! * **Datagrammes non fiables + FEC** (module [`crate::datagram`]) pour
//!   [`Reliability::UnreliableFec`] (vidéo, audio) : chaque trame est découpée en
//!   fragments ≤ MTU datagramme ([`Connection::max_datagram_size`]), protégée par
//!   Reed-Solomon (parité adaptée au taux de perte mesuré) et émise via
//!   `send_datagram` ; la réception réassemble dès que `k` fragments arrivent et
//!   livre les trames complètes dans la même file que le flux fiable —
//!   [`Transport::poll_recv`] fusionne donc naturellement les deux sources. Si le
//!   chemin datagrammes est indisponible (pair sans support, MTU trop petit, trame
//!   trop grosse), la trame repart sur le flux fiable.
//!
//! [`Transport::path_estimate`] est tirée des statistiques quinn
//! ([`Connection::stats`]) : RTT lissé, taux de perte fenêtré + lissé, débit plafond
//! `cwnd / RTT`. C'est cette estimation qui alimente l'ABR du codec (plan 03) et le
//! dimensionnement de la parité FEC.
//!
//! Un keepalive QUIC ([`quinn::TransportConfig::keep_alive_interval`]) entretient le
//! chemin (NAT) et borne la détection de coupure au délai d'inactivité ; l'état est
//! exposé par [`QuicTransport::is_connected`] / [`QuicTransport::close_reason`], et
//! [`QuicTransport::on_disconnect`] fournit le point d'ancrage (rappel de coupure).
//! La **reconnexion transparente** (rétablissement après coupure) est fournie par
//! [`crate::ReconnectingTransport`], qui scrute [`Transport::is_connected`].
//!
//! Sécurité : QUIC fournit ici le chiffrement TLS 1.3 « de saut ». Le certificat est
//! auto-signé et **épinglé** (le client fait confiance au certificat exact du serveur).
//! La confiance de bout en bout reposera sur Noise (voir plan 06).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use nd_proto::{ChannelKind, MonitorId, NdError, Reliability, Result};
use quinn::{
    Connection, ConnectionStats, Endpoint, EndpointConfig, IdleTimeout, RecvStream,
    SendDatagramError, SendStream, TokioRuntime, TransportConfig, VarInt,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, error::TryRecvError};

use crate::datagram::{Fragmenteur, Reassembleur};
use crate::{ChannelHandle, PathEstimate, Transport};

/// Préfixe écrit en tête de chaque flux pour valider l'appairage.
const MAGIC: &[u8; 4] = b"NDQ1";
/// Nom de serveur présenté au handshake TLS (doit figurer dans le SAN du certificat).
pub(crate) const SERVER_NAME: &str = "novadesk";
/// Taille max d'une charge utile de trame (garde-fou anti-abus).
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Intervalle du keepalive QUIC : des PING réguliers maintiennent les traductions
/// NAT ouvertes et font vivre la détection de coupure même quand la session est
/// inactive (bureau affiché mais utilisateur immobile).
const KEEPALIVE: Duration = Duration::from_secs(2);
/// Délai d'inactivité au bout duquel la connexion est déclarée coupée (borne haute
/// de la détection : avec le keepalive à 2 s, une coupure franche est vue en ≤ 10 s).
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Fenêtre minimale (en paquets émis) entre deux mesures du taux de perte : en deçà,
/// une perte isolée ferait des à-coups énormes sur le ratio.
const FENETRE_PERTE: u64 = 32;
/// Poids de la nouvelle mesure dans le lissage exponentiel du taux de perte.
const LISSAGE_PERTE: f32 = 0.3;

/// Runtime Tokio partagé par tout le transport (créé à la première utilisation).
pub(crate) fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("création du runtime Tokio"))
}

/// Installe un fournisseur cryptographique par défaut pour rustls (une seule fois).
pub(crate) fn ensure_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Identité TLS auto-signée **réutilisable** : certificat (DER) + clé privée
/// (PKCS#8). Permet de présenter le *même* certificat sur plusieurs points
/// d'entrée QUIC — l'écouteur principal ([`bind_with_identity`]), les sockets
/// percées par le hole punching ([`accept_over_socket`]) et le repli relais
/// ([`crate::accept_via_relay`]).
///
/// C'est ce certificat que le pair contrôlé publie au rendez-vous
/// (`nd-signaling::RendezvousClient::register`) et que l'appelant épingle
/// après `lookup` : l'identité doit donc être **unique et stable** pour tous
/// les chemins d'un même pair, sans quoi l'épinglage échoue.
#[derive(Clone)]
pub struct ServerIdentity {
    cert_der: Vec<u8>,
    key_pkcs8_der: Vec<u8>,
}

impl ServerIdentity {
    /// Génère une identité auto-signée neuve (SAN : nom de serveur NovaDesk).
    ///
    /// # Errors
    /// Erreur si la génération du certificat échoue.
    pub fn generate() -> Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
            .map_err(|e| NdError::Transport(format!("génération du certificat : {e}")))?;
        Ok(Self {
            cert_der: ck.cert.der().as_ref().to_vec(),
            key_pkcs8_der: ck.key_pair.serialize_der(),
        })
    }

    /// Reconstruit une identité depuis ses octets DER (persistance : garder la
    /// même identité — donc le même certificat épinglable — entre démarrages).
    #[must_use]
    pub fn from_der_parts(cert_der: Vec<u8>, key_pkcs8_der: Vec<u8>) -> Self {
        Self {
            cert_der,
            key_pkcs8_der,
        }
    }

    /// Certificat auto-signé (DER) — celui à publier au rendez-vous.
    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// Clé privée (PKCS#8 DER), pour la persistance. **Secret** : à stocker
    /// protégé (DPAPI/keychain, voir plan 06) ; ne jamais journaliser.
    #[must_use]
    pub fn key_pkcs8_der(&self) -> &[u8] {
        &self.key_pkcs8_der
    }
}

impl std::fmt::Debug for ServerIdentity {
    /// N'expose jamais la clé privée (ni le certificat complet) dans les logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("cert_der_len", &self.cert_der.len())
            .finish_non_exhaustive()
    }
}

/// Configuration TLS serveur (certificat épinglable + réglages de transport).
pub(crate) fn server_config(identity: &ServerIdentity) -> Result<quinn::ServerConfig> {
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(identity.key_pkcs8_der.clone().into());
    let mut cfg = quinn::ServerConfig::with_single_cert(vec![cert], key)
        .map_err(|e| NdError::Transport(format!("config serveur TLS : {e}")))?;
    cfg.transport_config(transport_tuning());
    Ok(cfg)
}

/// Configuration TLS client : épingle le certificat exact du serveur.
pub(crate) fn client_config(server_cert_der: &[u8]) -> Result<quinn::ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(server_cert_der.to_vec()))
        .map_err(|e| NdError::Transport(format!("ajout du certificat racine : {e}")))?;
    let mut cfg = quinn::ClientConfig::with_root_certificates(Arc::new(roots))
        .map_err(|e| NdError::Transport(format!("config client TLS : {e}")))?;
    cfg.transport_config(transport_tuning());
    Ok(cfg)
}

/// Endpoint quinn sur une **socket std déjà liée** (quinn accepte une socket
/// existante via [`Endpoint::new`] — c'est le cœur des points d'entrée
/// `*_over_socket` et du tunnel relais). À appeler dans le contexte du
/// runtime Tokio ; quinn passe lui-même la socket en non-bloquant.
pub(crate) fn endpoint_sur_socket(
    socket: std::net::UdpSocket,
    server_cfg: Option<quinn::ServerConfig>,
) -> Result<Endpoint> {
    Endpoint::new(
        EndpointConfig::default(),
        server_cfg,
        socket,
        Arc::new(TokioRuntime),
    )
    .map_err(|e| NdError::Transport(format!("endpoint sur socket existante : {e}")))
}

/// Réglages de transport communs client/serveur : keepalive et délai d'inactivité.
///
/// Les datagrammes non fiables sont actifs par défaut chez quinn (tampon de réception
/// non nul) ; il n'y a donc rien à activer explicitement pour le chemin média.
fn transport_tuning() -> Arc<TransportConfig> {
    let mut cfg = TransportConfig::default();
    cfg.keep_alive_interval(Some(KEEPALIVE));
    cfg.max_idle_timeout(Some(
        IdleTimeout::try_from(IDLE_TIMEOUT).expect("délai d'inactivité représentable"),
    ));
    Arc::new(cfg)
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
        Ok(Box::new(self.accept_quic()?))
    }

    /// Comme [`Listener::accept`], mais renvoie le type concret : donne accès à
    /// l'état de connexion ([`QuicTransport::is_connected`]) et au rappel de coupure.
    pub fn accept_quic(&self) -> Result<QuicTransport> {
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
        Ok(spawn_transport(conn, None, send, recv))
    }
}

/// Ouvre un écouteur QUIC sur l'adresse donnée (génère un certificat auto-signé).
///
/// Pour partager le certificat avec d'autres points d'entrée (sockets percées,
/// relais), préférer [`bind_with_identity`] avec une [`ServerIdentity`] commune.
pub fn bind(addr: SocketAddr) -> Result<Listener> {
    bind_with_identity(addr, &ServerIdentity::generate()?)
}

/// Comme [`bind`], mais avec une identité TLS **fournie** : l'écouteur, les
/// acceptations sur socket percée ([`accept_over_socket`]) et le repli relais
/// ([`crate::accept_via_relay`]) présentent alors le même certificat — celui
/// que le pair publie au rendez-vous et que l'appelant épingle.
pub fn bind_with_identity(addr: SocketAddr, identity: &ServerIdentity) -> Result<Listener> {
    ensure_provider();
    let server_cfg = server_config(identity)?;

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
        cert_der: identity.cert_der().to_vec(),
        local_addr,
    })
}

/// Se connecte à un pair QUIC en épinglant son certificat auto-signé.
///
/// `server_cert_der` est le certificat obtenu via [`Listener::server_cert_der`]
/// (dans la vraie vie il transitera par le serveur de rendez-vous, voir plan 05).
pub fn connect(remote: SocketAddr, server_cert_der: &[u8]) -> Result<Box<dyn Transport>> {
    Ok(Box::new(connect_quic(remote, server_cert_der)?))
}

/// Comme [`connect`], mais renvoie le type concret : donne accès à l'état de
/// connexion ([`QuicTransport::is_connected`]) et au rappel de coupure.
pub fn connect_quic(remote: SocketAddr, server_cert_der: &[u8]) -> Result<QuicTransport> {
    ensure_provider();
    let client_cfg = client_config(server_cert_der)?;

    let (endpoint, conn, send, recv) = runtime().block_on(async move {
        let bind_addr: SocketAddr = "0.0.0.0:0".parse().expect("adresse de bind valide");
        let mut endpoint = Endpoint::client(bind_addr)
            .map_err(|e| NdError::Transport(format!("endpoint client : {e}")))?;
        endpoint.set_default_client_config(client_cfg);
        connect_sur_endpoint(endpoint, remote).await
    })?;

    Ok(spawn_transport(conn, Some(endpoint), send, recv))
}

// ---------------------------------------------------------------------------
// QUIC sur socket UDP existante (plan 05 : porter QUIC sur la socket percée)
// ---------------------------------------------------------------------------

/// Se connecte à un pair QUIC **sur une socket UDP déjà ouverte** — typiquement
/// la socket rendue par le hole punching (`nd-signaling` :
/// `establish_p2p` → `DirectPath { socket, peer_addr, peer_cert_der }`), dont
/// le mapping NAT est déjà percé. La socket ne doit **pas** être rebindée ni
/// connectée ; quinn la reprend telle quelle ([`Endpoint::new`]) et la passe
/// en non-bloquant.
///
/// Les sondes de punch résiduelles qui arrivent encore sur la socket ne sont
/// pas des paquets QUIC valides : quinn les ignore silencieusement.
///
/// Côté appelé, le pendant est [`accept_over_socket`] ; l'appelant est le
/// client QUIC, l'appelé le serveur (il possède l'identité TLS).
///
/// # Errors
/// Erreur de configuration TLS, de création d'endpoint ou de handshake.
pub fn connect_over_socket(
    socket: std::net::UdpSocket,
    peer_addr: SocketAddr,
    server_cert_der: &[u8],
) -> Result<Box<dyn Transport>> {
    Ok(Box::new(connect_quic_over_socket(
        socket,
        peer_addr,
        server_cert_der,
    )?))
}

/// Comme [`connect_over_socket`], mais renvoie le type concret.
///
/// # Errors
/// Voir [`connect_over_socket`].
pub fn connect_quic_over_socket(
    socket: std::net::UdpSocket,
    peer_addr: SocketAddr,
    server_cert_der: &[u8],
) -> Result<QuicTransport> {
    ensure_provider();
    let client_cfg = client_config(server_cert_der)?;
    let (endpoint, conn, send, recv) = runtime().block_on(async move {
        let mut endpoint = endpoint_sur_socket(socket, None)?;
        endpoint.set_default_client_config(client_cfg);
        connect_sur_endpoint(endpoint, peer_addr).await
    })?;
    Ok(spawn_transport(conn, Some(endpoint), send, recv))
}

/// Accepte **une** connexion QUIC entrante **sur une socket UDP déjà
/// ouverte** — la socket percée côté appelé (`nd-signaling` : `await_p2p` →
/// `IncomingPath`). Bloque jusqu'au handshake (l'appelant se connecte sitôt
/// son punch confirmé ; le délai d'inactivité QUIC borne l'attente d'un
/// handshake entamé, mais pas l'attente d'un premier paquet — appeler depuis
/// le thread d'établissement, comme [`Listener::accept`]).
///
/// `identity` doit être l'identité dont le **certificat a été publié au
/// rendez-vous** : l'appelant l'épingle (voir [`ServerIdentity`]).
/// La socket percée étant dédiée à un seul pair, on n'accepte qu'une
/// connexion ; l'endpoint vit ensuite avec le transport rendu.
///
/// # Errors
/// Erreur de configuration TLS, de création d'endpoint ou de handshake.
pub fn accept_over_socket(
    socket: std::net::UdpSocket,
    identity: &ServerIdentity,
) -> Result<Box<dyn Transport>> {
    Ok(Box::new(accept_quic_over_socket(socket, identity)?))
}

/// Comme [`accept_over_socket`], mais renvoie le type concret.
///
/// # Errors
/// Voir [`accept_over_socket`].
pub fn accept_quic_over_socket(
    socket: std::net::UdpSocket,
    identity: &ServerIdentity,
) -> Result<QuicTransport> {
    ensure_provider();
    let server_cfg = server_config(identity)?;
    let (endpoint, conn, send, recv) = runtime().block_on(async move {
        let endpoint = endpoint_sur_socket(socket, Some(server_cfg))?;
        accept_sur_endpoint(endpoint).await
    })?;
    Ok(spawn_transport(conn, Some(endpoint), send, recv))
}

/// Handshake **client** sur un endpoint prêt (configuration client posée) puis
/// ouverture des flux d'appairage. Partagé par [`connect_quic`],
/// [`connect_quic_over_socket`] et le tunnel relais ([`crate::relay`]).
pub(crate) async fn connect_sur_endpoint(
    endpoint: Endpoint,
    remote: SocketAddr,
) -> Result<(Endpoint, Connection, SendStream, RecvStream)> {
    let conn = endpoint
        .connect(remote, SERVER_NAME)
        .map_err(|e| NdError::Transport(format!("connexion : {e}")))?
        .await
        .map_err(|e| NdError::Transport(format!("handshake : {e}")))?;
    let (send, recv) = setup_streams(&conn).await?;
    Ok((endpoint, conn, send, recv))
}

/// Accepte **une** connexion entrante sur un endpoint serveur puis ouvre les
/// flux d'appairage. Partagé par [`accept_quic_over_socket`] et le tunnel
/// relais ([`crate::relay`]).
pub(crate) async fn accept_sur_endpoint(
    endpoint: Endpoint,
) -> Result<(Endpoint, Connection, SendStream, RecvStream)> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| NdError::Transport("endpoint fermé".into()))?;
    let conn = incoming
        .await
        .map_err(|e| NdError::Transport(format!("connexion entrante : {e}")))?;
    let (send, recv) = setup_streams(&conn).await?;
    Ok((endpoint, conn, send, recv))
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

/// Tag + moniteur pour l'en-tête de trame d'un canal (flux fiable et datagrammes).
pub(crate) fn kind_tag(kind: ChannelKind) -> (u8, u32) {
    match kind {
        ChannelKind::Control => (0, 0),
        ChannelKind::Audio => (1, 0),
        ChannelKind::Input => (2, 0),
        ChannelKind::Files => (3, 0),
        ChannelKind::Video(m) => (4, m.0),
    }
}

/// Reconstruit le canal depuis le tag/moniteur reçus.
pub(crate) fn tag_kind(tag: u8, monitor: u32) -> Option<ChannelKind> {
    match tag {
        0 => Some(ChannelKind::Control),
        1 => Some(ChannelKind::Audio),
        2 => Some(ChannelKind::Input),
        3 => Some(ChannelKind::Files),
        4 => Some(ChannelKind::Video(MonitorId(monitor))),
        _ => None,
    }
}

/// Tâche d'émission : sérialise les messages sortants sur le flux QUIC fiable.
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

/// Tâche de réception fiable : dé-sérialise les trames entrantes et les pousse dans
/// la file commune (partagée avec le chemin datagrammes).
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

/// Tâche de réception des datagrammes : réassemble les lots FEC ([`Reassembleur`])
/// et livre les charges complètes dans la même file que le flux fiable — c'est ce
/// qui permet à [`Transport::poll_recv`] de fusionner les deux sources.
async fn datagram_reader(conn: Connection, tx: mpsc::UnboundedSender<(ChannelKind, Vec<u8>)>) {
    let mut reassembleur = Reassembleur::default();
    while let Ok(datagramme) = conn.read_datagram().await {
        if let Some((kind, charge)) = reassembleur.absorber(&datagramme) {
            if tx.send((kind, charge)).is_err() {
                break;
            }
        }
    }
}

/// Assemble un [`QuicTransport`] et lance les tâches d'E/S (flux + datagrammes).
pub(crate) fn spawn_transport(
    conn: Connection,
    endpoint: Option<Endpoint>,
    send: SendStream,
    recv: RecvStream,
) -> QuicTransport {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    runtime().spawn(writer(send, out_rx));
    runtime().spawn(reader(recv, in_tx.clone()));
    runtime().spawn(datagram_reader(conn.clone(), in_tx));
    QuicTransport {
        conn,
        endpoint,
        outbound_tx: out_tx,
        inbound_rx: in_rx,
        channels: Vec::new(),
        fragmenteur: Fragmenteur::default(),
        estimateur: Mutex::new(EstimateurPertes::default()),
    }
}

/// Estimateur du taux de perte à fenêtre glissante, bâti sur les compteurs cumulés
/// de quinn (`sent_packets` / `lost_packets`) : on mesure le ratio sur les paquets
/// émis depuis le dernier relevé (au moins [`FENETRE_PERTE`]) puis on lisse en
/// exponentiel pour amortir les rafales.
#[derive(Default)]
struct EstimateurPertes {
    /// `sent_packets` au dernier relevé.
    emis_prec: u64,
    /// `lost_packets` au dernier relevé.
    perdus_prec: u64,
    /// Taux de perte lissé, dans [0, 1].
    perte_lissee: f32,
}

impl EstimateurPertes {
    /// Met à jour l'estimation depuis les statistiques et renvoie le taux lissé.
    fn observer(&mut self, stats: &ConnectionStats) -> f32 {
        let emis = stats.path.sent_packets;
        let perdus = stats.path.lost_packets;
        let d_emis = emis.saturating_sub(self.emis_prec);
        if d_emis >= FENETRE_PERTE {
            let d_perdus = perdus.saturating_sub(self.perdus_prec);
            #[allow(clippy::cast_precision_loss)]
            let brut = (d_perdus as f32 / d_emis as f32).clamp(0.0, 1.0);
            self.perte_lissee += LISSAGE_PERTE * (brut - self.perte_lissee);
            self.emis_prec = emis;
            self.perdus_prec = perdus;
        }
        self.perte_lissee
    }
}

/// Débit plafond estimé depuis la fenêtre de congestion : `cwnd / RTT`, en kbit/s.
///
/// C'est la borne que le contrôleur de congestion accorde au chemin ; l'ABR du codec
/// visera en dessous (voir plan 03).
fn debit_kbps(cwnd_octets: u64, rtt_us: u64) -> u32 {
    if rtt_us == 0 {
        return 0;
    }
    // octets/s = cwnd / (rtt_us / 1e6) ; kbit/s = octets/s × 8 / 1000.
    let kbps = cwnd_octets.saturating_mul(8_000) / rtt_us;
    u32::try_from(kbps).unwrap_or(u32::MAX)
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
    /// Fragmentation + FEC du chemin datagrammes (émission).
    fragmenteur: Fragmenteur,
    /// Taux de perte fenêtré (verrouillé : `path_estimate` ne prend que `&self`).
    estimateur: Mutex<EstimateurPertes>,
}

impl QuicTransport {
    /// Clone de la connexion quinn sous-jacente (interne : le tunnel relais
    /// s'en sert pour attacher la fin de vie du pont à celle de la connexion).
    pub(crate) fn connection(&self) -> Connection {
        self.conn.clone()
    }

    /// La connexion est-elle toujours ouverte ?
    ///
    /// Passe à `false` dès que quinn constate la coupure : fermeture par le pair,
    /// erreur transport, ou délai d'inactivité ([`IDLE_TIMEOUT`]) — le keepalive
    /// garantit que ce délai n'expire que si le chemin est réellement mort.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    /// Raison de la fermeture si la connexion est coupée, `None` sinon.
    #[must_use]
    pub fn close_reason(&self) -> Option<String> {
        self.conn.close_reason().map(|e| e.to_string())
    }

    /// Enregistre un rappel invoqué (une seule fois) à la coupure de la connexion,
    /// avec la raison en argument.
    ///
    /// C'est le point d'ancrage de la future reconnexion transparente (jet suivant) :
    /// `nd-core` y branchera la re-négociation via le rendez-vous (plan 05).
    pub fn on_disconnect<F>(&self, rappel: F)
    where
        F: FnOnce(String) + Send + 'static,
    {
        let conn = self.conn.clone();
        runtime().spawn(async move {
            let raison = conn.closed().await;
            rappel(raison.to_string());
        });
    }

    /// Envoi sur le flux fiable ordonné (et repli du chemin datagrammes).
    fn envoyer_fiable(&self, kind: ChannelKind, data: Vec<u8>) -> Result<()> {
        self.outbound_tx
            .send((kind, data))
            .map_err(|_| NdError::Transport("connexion fermée".into()))
    }

    /// Envoi média : fragmente en datagrammes ≤ MTU + parité FEC, et replie sur le
    /// flux fiable quand le chemin datagrammes ne convient pas.
    fn envoyer_datagrammes(&mut self, kind: ChannelKind, data: Vec<u8>) -> Result<()> {
        // MTU datagramme courant ; `None` = datagrammes indisponibles sur ce chemin.
        let Some(mtu) = self.conn.max_datagram_size() else {
            return self.envoyer_fiable(kind, data);
        };
        let perte = self.path_estimate().loss_ratio;
        let Some(datagrammes) = self.fragmenteur.fragmenter(kind, &data, mtu, perte)? else {
            // Trame trop grosse pour le chemin datagrammes : flux fiable (rare —
            // typiquement une très grosse image clé).
            return self.envoyer_fiable(kind, data);
        };
        for datagramme in datagrammes {
            match self.conn.send_datagram(datagramme) {
                Ok(()) => {}
                // Le pair ne prend pas les datagrammes : trame entière en fiable
                // (l'erreur survient dès le premier fragment, rien n'est parti).
                Err(SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled) => {
                    return self.envoyer_fiable(kind, data);
                }
                // Le MTU a rétréci entre le calcul et l'envoi : fragment perdu,
                // la parité FEC est là pour couvrir exactement ce genre de trou.
                Err(SendDatagramError::TooLarge) => {}
                Err(SendDatagramError::ConnectionLost(e)) => {
                    return Err(NdError::Transport(format!("connexion perdue : {e}")));
                }
            }
        }
        Ok(())
    }
}

impl Transport for QuicTransport {
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
        if let Some(i) = self.channels.iter().position(|k| *k == kind) {
            return ChannelHandle(i as u32);
        }
        self.channels.push(kind);
        ChannelHandle((self.channels.len() - 1) as u32)
    }

    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, reliability: Reliability) -> Result<()> {
        let kind = *self
            .channels
            .get(ch.0 as usize)
            .ok_or_else(|| NdError::Transport("handle de canal inconnu".into()))?;
        match reliability {
            Reliability::Reliable => self.envoyer_fiable(kind, data),
            Reliability::UnreliableFec => self.envoyer_datagrammes(kind, data),
        }
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
        let stats = self.conn.stats();
        let rtt_us = stats.path.rtt.as_micros() as u64;
        let loss_ratio = self
            .estimateur
            .lock()
            .expect("verrou de l'estimateur de pertes")
            .observer(&stats);
        PathEstimate {
            rtt_us,
            loss_ratio,
            estimated_bandwidth_kbps: debit_kbps(stats.path.cwnd, rtt_us),
        }
    }

    /// État réel de la connexion QUIC : c'est le signal que
    /// [`crate::ReconnectingTransport`] scrute pour la reconnexion transparente
    /// (voir aussi la méthode inhérente [`QuicTransport::is_connected`]).
    fn is_connected(&self) -> bool {
        self.conn.close_reason().is_none()
    }
}

impl Drop for QuicTransport {
    fn drop(&mut self) {
        // Fermeture explicite : les tâches d'E/S détiennent des clones de la
        // connexion (flux, datagrammes), sans quoi elle survivrait au transport —
        // le keepalive la maintiendrait ouverte et le pair ne verrait jamais la fin.
        self.conn
            .close(VarInt::from_u32(0), b"fermeture du transport");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// Paire (serveur, client) connectée en bouclage local.
    fn paire() -> (QuicTransport, QuicTransport) {
        let listener = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = listener.local_addr();
        let cert = listener.server_cert_der();
        let client = thread::spawn(move || connect_quic(addr, &cert).expect("connect"));
        let serveur = listener.accept_quic().expect("accept");
        (serveur, client.join().expect("thread client"))
    }

    /// Draine `poll_recv` jusqu'au prochain message ou à l'expiration du délai.
    fn attendre_message(
        transport: &mut QuicTransport,
        timeout: Duration,
    ) -> Option<(ChannelHandle, Vec<u8>)> {
        let debut = Instant::now();
        while debut.elapsed() < timeout {
            if let Some(message) = transport.poll_recv().expect("poll_recv") {
                return Some(message);
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    #[test]
    fn datagrammes_fec_grosse_charge_en_bouclage() {
        let (mut serveur, mut client) = paire();
        let h_video = client.open_channel(ChannelKind::Video(MonitorId(0)));
        let trame: Vec<u8> = (0..60_000).map(|i| (i % 251) as u8).collect();

        // Chemin non fiable : en cas de malchance (plus de datagrammes perdus que de
        // parité), on renvoie la trame — exactement ce que ferait l'émetteur vidéo.
        let mut recue = None;
        for _ in 0..5 {
            client
                .send(h_video, trame.clone(), Reliability::UnreliableFec)
                .expect("send vidéo");
            if let Some(message) = attendre_message(&mut serveur, Duration::from_secs(2)) {
                recue = Some(message);
                break;
            }
        }
        let (handle, data) = recue.expect("charge vidéo reconstruite via datagrammes + FEC");
        assert_eq!(data, trame, "charge reconstruite à l'identique");
        // Le canal est bien re-créé côté réception avec le bon type.
        assert_eq!(
            handle,
            serveur.open_channel(ChannelKind::Video(MonitorId(0)))
        );
    }

    #[test]
    fn poll_recv_fusionne_flux_fiable_et_datagrammes() {
        let (mut serveur, mut client) = paire();
        let h_video = client.open_channel(ChannelKind::Video(MonitorId(1)));
        let h_input = client.open_channel(ChannelKind::Input);
        let trame: Vec<u8> = (0..30_000).map(|i| (i % 199) as u8).collect();
        let evenement = vec![0xAB, 0x01];

        client
            .send(h_input, evenement.clone(), Reliability::Reliable)
            .expect("send input");
        let mut video_recue = None;
        let mut input_recu = None;
        for _ in 0..5 {
            client
                .send(h_video, trame.clone(), Reliability::UnreliableFec)
                .expect("send vidéo");
            let debut = Instant::now();
            while debut.elapsed() < Duration::from_secs(2)
                && (video_recue.is_none() || input_recu.is_none())
            {
                match serveur.poll_recv().expect("poll_recv") {
                    Some((_, data)) if data.len() == trame.len() => video_recue = Some(data),
                    Some((_, data)) => input_recu = Some(data),
                    None => thread::sleep(Duration::from_millis(2)),
                }
            }
            if video_recue.is_some() && input_recu.is_some() {
                break;
            }
        }
        assert_eq!(input_recu.expect("message input (flux fiable)"), evenement);
        assert_eq!(video_recue.expect("trame vidéo (datagrammes)"), trame);
    }

    #[test]
    fn flux_fiable_intact_pour_grosses_charges() {
        let (mut serveur, mut client) = paire();
        let h_files = client.open_channel(ChannelKind::Files);
        let bloc: Vec<u8> = (0..1_000_000).map(|i| (i % 241) as u8).collect();
        client
            .send(h_files, bloc.clone(), Reliability::Reliable)
            .expect("send fichier");
        let (_, data) =
            attendre_message(&mut serveur, Duration::from_secs(10)).expect("bloc fiable reçu");
        assert_eq!(data, bloc);
    }

    #[test]
    fn path_estimate_renseigne_apres_echange() {
        let (mut serveur, mut client) = paire();
        let h_input = client.open_channel(ChannelKind::Input);
        for i in 0..40 {
            client
                .send(h_input, vec![i as u8; 512], Reliability::Reliable)
                .expect("send input");
        }
        let mut recus = 0;
        while recus < 40 {
            match attendre_message(&mut serveur, Duration::from_secs(5)) {
                Some(_) => recus += 1,
                None => break,
            }
        }
        assert_eq!(recus, 40, "trafic de mesure reçu");

        let estimation = client.path_estimate();
        assert!(estimation.rtt_us > 0, "RTT mesuré : {estimation:?}");
        assert!(
            estimation.estimated_bandwidth_kbps > 0,
            "débit estimé : {estimation:?}"
        );
        assert!(
            (0.0..=1.0).contains(&estimation.loss_ratio),
            "taux de perte borné : {estimation:?}"
        );
    }

    /// QUIC porté sur des sockets UDP **pré-ouvertes** (chemin « socket
    /// percée » du plan 05) : identité fournie, échange bidirectionnel,
    /// sondes de punch résiduelles tolérées (quinn les ignore).
    #[test]
    fn quic_sur_sockets_preouvertes() {
        let identite = ServerIdentity::generate().expect("identité");
        let cert = identite.cert_der().to_vec();
        let sock_serveur = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind serveur");
        let sock_client = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client");
        let addr_serveur = sock_serveur.local_addr().expect("adresse serveur");
        let addr_client = sock_client.local_addr().expect("adresse client");

        // Sondes résiduelles du punch, envoyées avant le handshake : pas des
        // paquets QUIC, elles ne doivent pas gêner l'établissement.
        sock_client
            .send_to(b"NDPUNCH1\x01\x00", addr_serveur)
            .expect("sonde vers serveur");
        sock_serveur
            .send_to(b"NDPUNCH1\x02\x01", addr_client)
            .expect("sonde vers client");

        let serveur = thread::spawn(move || {
            accept_quic_over_socket(sock_serveur, &identite).expect("accept_over_socket")
        });
        let mut client = connect_quic_over_socket(sock_client, addr_serveur, &cert)
            .expect("connect_over_socket");
        let mut serveur = serveur.join().expect("thread serveur");

        // Aller : client → serveur.
        let h = client.open_channel(ChannelKind::Control);
        client
            .send(h, b"ping punch".to_vec(), Reliability::Reliable)
            .expect("send client");
        let (_, data) =
            attendre_message(&mut serveur, Duration::from_secs(5)).expect("message aller");
        assert_eq!(data, b"ping punch");

        // Retour : serveur → client.
        let h = serveur.open_channel(ChannelKind::Control);
        serveur
            .send(h, b"pong punch".to_vec(), Reliability::Reliable)
            .expect("send serveur");
        let (_, data) =
            attendre_message(&mut client, Duration::from_secs(5)).expect("message retour");
        assert_eq!(data, b"pong punch");
    }

    /// L'écouteur construit avec une identité fournie présente bien le
    /// certificat de l'identité (celui que le rendez-vous publiera).
    #[test]
    fn identite_partagee_entre_ecouteur_et_pairs() {
        let identite = ServerIdentity::generate().expect("identité");
        let listener =
            bind_with_identity("127.0.0.1:0".parse().expect("adresse"), &identite).expect("bind");
        assert_eq!(listener.server_cert_der(), identite.cert_der());

        // Un client qui n'épingle que le certificat de l'identité se connecte.
        let addr = listener.local_addr();
        let cert = identite.cert_der().to_vec();
        let client = thread::spawn(move || connect_quic(addr, &cert).expect("connect"));
        let _serveur = listener.accept_quic().expect("accept");
        assert!(client.join().expect("thread client").is_connected());
    }

    /// Une identité persistée (octets DER) reconstruit le même certificat et
    /// une configuration serveur valide.
    #[test]
    fn identite_persistee_reconstruit_le_meme_certificat() {
        let identite = ServerIdentity::generate().expect("identité");
        let rechargee = ServerIdentity::from_der_parts(
            identite.cert_der().to_vec(),
            identite.key_pkcs8_der().to_vec(),
        );
        assert_eq!(rechargee.cert_der(), identite.cert_der());
        assert!(server_config(&rechargee).is_ok(), "clé privée valide");
    }

    #[test]
    fn detection_de_coupure_et_rappel() {
        let (serveur, client) = paire();
        assert!(serveur.is_connected());
        assert!(client.is_connected());
        assert!(serveur.close_reason().is_none());

        let (tx, rx) = std::sync::mpsc::channel();
        serveur.on_disconnect(move |raison| {
            let _ = tx.send(raison);
        });
        // La chute du transport client ferme la connexion (voir `Drop`).
        drop(client);

        let raison = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("rappel de coupure invoqué");
        assert!(!raison.is_empty());
        assert!(!serveur.is_connected());
        assert!(serveur.close_reason().is_some());
    }
}
