//! `nd-signaling` — client de connectivité : enregistrement auprès du serveur de
//! rendez-vous, découverte d'adresse (STUN), NAT traversal (ICE/hole punching) et
//! repli sur relais. Voir `../../plan-technique/05-connectivite-nat.md`.

use nd_proto::{NdError, NovaId, Result};

/// Configuration du client de connectivité.
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    /// URL du serveur de rendez-vous (peut être auto-hébergé, voir plan 05/11).
    pub rendezvous_url: String,
}

/// Résultat d'une tentative d'établissement de chemin vers un pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Connexion pair-à-pair directe établie (cas nominal).
    DirectP2P,
    /// Repli via serveur de relais (NAT symétrique/CGNAT).
    Relayed,
}

/// Adresse d'un pair résolue, prête à être passée au transport (voir plan 04).
#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    pub remote_addr: String,
    pub path: PathKind,
}

/// Client de signalisation/connectivité.
pub trait SignalingClient: Send {
    /// Enregistre l'ID local auprès du serveur de rendez-vous et maintient la présence.
    fn register(&mut self, id: NovaId) -> Result<()>;
    /// Résout et établit un chemin réseau vers le pair désigné.
    fn resolve(&mut self, peer: NovaId) -> Result<ResolvedPeer>;
}

/// Crée un client de signalisation. Non implémenté à ce stade.
pub fn connect(_cfg: SignalingConfig) -> Result<Box<dyn SignalingClient>> {
    Err(NdError::NotImplemented(
        "nd-signaling::connect (rendez-vous/ICE/relais à venir, voir plan 05/16)",
    ))
}
