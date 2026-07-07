//! `nd-transport` — abstraction du transport temps réel au-dessus de QUIC.
//!
//! Multiplexe des canaux logiques (vidéo/audio/input/fichiers/contrôle) sur une seule
//! connexion QUIC (`quinn`), avec datagrammes non fiables + FEC pour le média
//! ([`nd_proto::Reliability::UnreliableFec`], modules [`fec`] et `datagram`) et flux
//! fiable ordonné pour l'input/contrôle/fichiers. Congestion, format de trames et
//! FEC : `../../plan-technique/04-transport-reseau.md`.
//!
//! **Points d'entrée connectivité (plan 05)** — même pile QUIC, trois chemins :
//!
//! * adresse directe : [`bind`] / [`connect`] (LAN, loopback, IP publique) ;
//! * socket UDP **percée** par le hole punching (`nd-signaling::connect`) :
//!   [`connect_over_socket`] / [`accept_over_socket`], quinn reprenant la
//!   socket déjà ouverte — le mapping NAT percé est conservé ;
//! * repli **relais** (`nd-relay`, tunnel TCP aveugle) :
//!   [`connect_via_relay`] / [`accept_via_relay`].
//!
//! Le pair contrôlé présente la même [`ServerIdentity`] sur tous les chemins :
//! le certificat publié au rendez-vous reste épinglable partout.
//!
//! Note : l'API du trait reste en `Vec<u8>` ; le passage à `bytes::Bytes` de bout en
//! bout (zéro-copie) viendra quand `nd-codec` produira ses trames dans des tampons
//! partagés.

use nd_proto::{ChannelKind, Reliability, Result};

/// Poignée opaque d'un canal logique ouvert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelHandle(pub u32);

/// Statistiques de chemin réseau, alimentant l'ABR du codec (voir plan 03/04).
///
/// Renseignées depuis les statistiques de la connexion quinn : voir
/// [`Transport::path_estimate`] du transport QUIC.
#[derive(Debug, Clone, Copy, Default)]
pub struct PathEstimate {
    /// RTT lissé du chemin, en microsecondes.
    pub rtt_us: u64,
    /// Taux de perte de paquets, fenêtré puis lissé, dans [0, 1]. Sert aussi à
    /// dimensionner la parité FEC ([`fec::FecParams::adapt`]).
    pub loss_ratio: f32,
    /// Débit plafond estimé (fenêtre de congestion / RTT), en kbit/s.
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

mod datagram;
pub mod fec;
mod quic;
mod relay;

pub use quic::{
    accept_over_socket, accept_quic_over_socket, bind, bind_with_identity, connect,
    connect_over_socket, connect_quic, connect_quic_over_socket, Listener, QuicTransport,
    ServerIdentity,
};
pub use relay::{
    accept_quic_via_relay, accept_via_relay, connect_quic_via_relay, connect_via_relay,
};
