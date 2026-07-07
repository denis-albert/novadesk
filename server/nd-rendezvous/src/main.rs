//! Serveur de rendez-vous / signalisation NovaDesk.
//!
//! Associe un ID NovaDesk à l'adresse (UDP/QUIC) et au certificat du pair
//! contrôlé, et répond aux recherches par ID. Il ne voit jamais le média
//! (chiffré de bout en bout, voir plan 06).
//!
//! Depuis le plan 11, l'enregistrement exige une **preuve de possession
//! d'ID** : jeton d'attribution signé par l'autorité du déploiement + signature
//! fraîche de la clé statique liée à l'ID (voir la bibliothèque
//! [`nd_rendezvous`]). Le serveur ne démarre pas sans la clé publique de
//! l'autorité (fermé par défaut).
//!
//! Usage : `nd-rendezvous <cle-publique-autorite-hex> [adresse:port]`
//! - `cle-publique-autorite-hex` : clé publique Ed25519 (64 caractères
//!   hexadécimaux) de l'autorité — celle affichée par `nd-api` au démarrage ;
//! - `adresse:port` : adresse d'écoute (défaut `0.0.0.0:9000`).

use std::io;
use std::net::TcpListener;

use nd_api::auth::cle_publique_depuis_hex;
use nd_rendezvous::{servir_authentifie, ConfigRendezvous};
use nd_signaling::Registry;

/// Adresse d'écoute par défaut.
const ADRESSE_DEFAUT: &str = "0.0.0.0:9000";

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let cle_hex = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage : nd-rendezvous <cle-publique-autorite-hex> [adresse:port] \
             (clé affichée par nd-api au démarrage)",
        )
    })?;
    let cle_autorite = cle_publique_depuis_hex(&cle_hex).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "clé publique d'autorité invalide (64 caractères hexadécimaux attendus)",
        )
    })?;
    let adresse = args.next().unwrap_or_else(|| ADRESSE_DEFAUT.to_string());

    let listener = TcpListener::bind(&adresse)?;
    println!(
        "nd-rendezvous (NovaDesk protocole v{}) en écoute sur {} — \
         enregistrement par preuve de possession d'ID (autorité : {cle_hex})",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    servir_authentifie(
        listener,
        Registry::new(),
        ConfigRendezvous::new(cle_autorite),
    )
}
