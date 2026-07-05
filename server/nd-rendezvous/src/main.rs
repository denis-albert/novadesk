//! Serveur de rendez-vous / signalisation NovaDesk.
//!
//! Associe un ID NovaDesk à l'adresse (UDP/QUIC) et au certificat du pair contrôlé,
//! et répond aux recherches par ID. Il ne voit jamais le média (chiffré de bout en
//! bout, voir plan 06). NAT traversal (STUN, hole punching) et relais à venir
//! (plan 05). Voir aussi `../../plan-technique/11-backend-infrastructure.md`.
//!
//! Usage : `nd-rendezvous [adresse:port]` (défaut `0.0.0.0:9000`).

use std::net::TcpListener;

use nd_signaling::{serve, Registry};

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:9000".to_string());
    let listener = TcpListener::bind(&addr)?;
    println!(
        "nd-rendezvous (NovaDesk protocole v{}) en écoute sur {}",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    serve(listener, Registry::new())
}
