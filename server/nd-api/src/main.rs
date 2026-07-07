//! Binaire `nd-api` — sert l'API applicative NovaDesk sur TCP.
//!
//! Toute la logique vit dans la bibliothèque (`nd_api`) : carnet d'adresses,
//! RBAC, groupes, partages, attribution d'ID, autorité de signature, mises à
//! jour, configuration, protocole et persistance. Ici, on ne fait
//! qu'assembler : écoute TCP + `serve`.
//!
//! Usage : `nd-api [adresse:port] [chemin-etat.json] [chemin-cle-autorite] [compte-racine]`
//! - `adresse:port` : adresse d'écoute (défaut `0.0.0.0:9300`) ;
//! - `chemin-etat.json` : fichier d'état durable (JSON atomique, voir
//!   `nd_api::storage`) ; sans chemin (ou `-`), l'état vit en mémoire ;
//! - `chemin-cle-autorite` : fichier de graine de l'autorité de signature
//!   (créé au premier démarrage, voir `nd_api::auth::Autorite`) ; sans chemin,
//!   l'autorité est **éphémère** (les jetons émis meurent avec le processus) ;
//! - `compte-racine` : compte opérateur qui amorce le RBAC ; sans lui, aucune
//!   opération d'administration n'est possible (fermé par défaut).
//!
//! La clé publique de l'autorité est affichée au démarrage : c'est elle qu'il
//! faut configurer sur `nd-rendezvous` et `nd-relay`.

use std::net::TcpListener;

use nd_api::auth::Autorite;
use nd_api::services::{serve, Services};

/// Adresse d'écoute par défaut (9000 = rendez-vous, 9100 = relais, 9200 = comptes).
const ADRESSE_DEFAUT: &str = "0.0.0.0:9300";

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let adresse = args.next().unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    let chemin = args.next().filter(|c| c != "-");
    let chemin_cle = args.next();
    let compte_racine = args.next();

    let (mut services, mode) = match &chemin {
        Some(chemin) => (Services::open(chemin)?, format!("durable ({chemin})")),
        None => (Services::new(), "en mémoire".to_string()),
    };
    let autorite = match &chemin_cle {
        Some(chemin_cle) => {
            let autorite = Autorite::charger_ou_creer(chemin_cle.as_ref())?;
            services = services.avec_autorite(autorite);
            format!("stable ({chemin_cle})")
        }
        None => "ÉPHÉMÈRE (jetons perdus au redémarrage)".to_string(),
    };
    if let Some(compte) = &compte_racine {
        services = services.avec_compte_racine(compte);
    }

    let listener = TcpListener::bind(&adresse)?;
    println!(
        "nd-api (NovaDesk protocole v{}) en écoute sur {} — état {mode}",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    println!(
        "nd-api — autorité {autorite} — clé publique : {}",
        services.cle_publique_autorite_hex()
    );
    match &compte_racine {
        Some(compte) => println!("nd-api — compte racine : {compte}"),
        None => println!("nd-api — aucun compte racine : administration verrouillée"),
    }
    serve(listener, services)
}
