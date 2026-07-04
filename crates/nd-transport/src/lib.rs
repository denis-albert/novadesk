//! `nd-transport` — abstraction du transport temps réel au-dessus de QUIC.
//!
//! Multiplexe des canaux logiques (vidéo/audio/input/fichiers/contrôle) sur une seule
//! connexion QUIC, avec datagrammes non fiables + FEC pour le média et flux fiables
//! pour l'input/contrôle/fichiers. Congestion, format de trames et FEC :
//! `../../plan-technique/04-transport-reseau.md`.
//!
//! Note : le squelette utilise `Vec<u8>` ; l'implémentation passera à `bytes::Bytes`
//! (zéro-copie) et à `quinn` pour QUIC.

use nd_proto::{ChannelKind, Reliability, Result};

/// Poignée opaque d'un canal logique ouvert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelHandle(pub u32);

/// Statistiques de chemin réseau, alimentant l'ABR du codec (voir plan 03/04).
#[derive(Debug, Clone, Copy, Default)]
pub struct PathEstimate {
    pub rtt_us: u64,
    pub loss_ratio: f32,
    pub estimated_bandwidth_kbps: u32,
}

/// Transport multiplexé entre deux pairs.
pub trait Transport: Send {
    /// Ouvre (ou retrouve) le canal logique d'un type donné.
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle;
    /// Envoie une charge utile sur un canal avec la fiabilité demandée.
    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, reliability: Reliability) -> Result<()>;
    /// Récupère la prochaine charge utile reçue, s'il y en a une.
    fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>>;
    /// Dernière estimation du chemin réseau.
    fn path_estimate(&self) -> PathEstimate;
}

mod quic;
pub use quic::{bind, connect, Listener, QuicTransport};
