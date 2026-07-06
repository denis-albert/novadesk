//! Binaire `nd-api` — sert l'API applicative NovaDesk sur TCP.
//!
//! Toute la logique vit dans la bibliothèque (`nd_api`) : carnet d'adresses,
//! RBAC, groupes, partages, mises à jour, configuration, protocole et
//! persistance. Ici, on ne fait qu'assembler : écoute TCP + `serve`.
//!
//! Usage : `nd-api [adresse:port] [chemin-etat.json]`
//! - `adresse:port` : adresse d'écoute (défaut `0.0.0.0:9300`) ;
//! - `chemin-etat.json` : fichier d'état durable (JSON atomique, voir
//!   `nd_api::storage`) ; sans chemin, l'état vit en mémoire.

use std::net::TcpListener;

use nd_api::services::{serve, Services};

/// Adresse d'écoute par défaut (9000 = rendez-vous, 9100 = relais, 9200 = comptes).
const ADRESSE_DEFAUT: &str = "0.0.0.0:9300";

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let adresse = args.next().unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    let chemin = args.next();
    let (services, mode) = match &chemin {
        Some(chemin) => (Services::open(chemin)?, format!("durable ({chemin})")),
        None => (Services::new(), "en mémoire".to_string()),
    };
    let listener = TcpListener::bind(&adresse)?;
    println!(
        "nd-api (NovaDesk protocole v{}) en écoute sur {} — état {mode}",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    serve(listener, services)
}
