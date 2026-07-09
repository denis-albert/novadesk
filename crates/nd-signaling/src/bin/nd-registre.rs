//! `nd-registre` — serveur de **rendez-vous local exécutable** (plan 05).
//!
//! Lance le registre « nu » (non authentifié) de [`nd_signaling`] : celui que
//! [`nd_signaling::establish_p2p`] / [`nd_signaling::await_p2p`] consomment via
//! [`nd_signaling::RendezvousClient`]. Mince enveloppe autour de
//! [`nd_signaling::serve`] : bind d'un `TcpListener`, création d'un
//! [`nd_signaling::Registry`], puis boucle de service indéfinie.
//!
//! # Ce que le serveur expose
//!
//! Toutes les requêtes du protocole de rendez-vous, servies par `serve` sur la
//! même socket TCP :
//!
//! - `register` / `lookup` — publication et résolution d'un ID → (adresse, cert) ;
//! - `heartbeat` — présence (TTL, défaut [`nd_signaling::DEFAULT_TTL`]) ;
//! - `publish_candidates` / `peer_candidates` — échange des candidats de punch
//!   (adresse locale + adresse réflexive STUN) ;
//! - `request_punch` / `poll_punch` — coordination de l'**UDP hole punching**.
//!
//! Ces deux derniers couples sont indispensables au punch en boucle locale
//! (loopback) et en LAN : sans eux, `establish_p2p` retomberait sur le relais.
//!
//! # Lancement
//!
//! ```text
//! cargo run -p nd-signaling --bin nd-registre -- 127.0.0.1:9000
//! ```
//!
//! `nd-registre [adresse:port]` — défaut `0.0.0.0:9000` (toutes interfaces).
//! L'adresse peut être une IP:port (`127.0.0.1:9000`, `0.0.0.0:9000`) ou un
//! nom résoluble (`localhost:9000`). Le serveur imprime l'adresse réellement
//! liée (utile si le port 0 est demandé) puis tourne jusqu'à `Ctrl+C`.
//!
//! # Arrêt
//!
//! `Ctrl+C` (best-effort) : le registre est **purement en mémoire** (aucun
//! fichier, aucune ressource externe), donc la terminaison du processus est
//! intrinsèquement propre — rien à vider ni à fermer.
//!
//! # Test 2-instances (vraie session par ID en LAN/loopback)
//!
//! Ce registre partagé remplace le rendez-vous éphémère embarqué dans les
//! tests : trois terminaux, un registre commun, deux pairs qui se trouvent
//! par leur ID.
//!
//! 1. **Terminal 1 — le registre** :
//!    ```text
//!    cargo run -p nd-signaling --bin nd-registre -- 127.0.0.1:9000
//!    ```
//! 2. **Terminal 2 — le pair appelé** (contrôlé) : un
//!    [`nd_signaling::RendezvousClient::new`] pointé sur `127.0.0.1:9000`,
//!    qui `register(id_appele, …)` puis boucle sur
//!    [`nd_signaling::await_p2p`] (publie ses candidats, relève les demandes
//!    de punch).
//! 3. **Terminal 3 — le pair appelant** (contrôleur) : un `RendezvousClient`
//!    sur la même adresse, qui appelle [`nd_signaling::establish_p2p`] avec
//!    `peer_id = id_appele` (résout l'ID, dépose sa demande de punch, perce le
//!    chemin). Les deux pairs tendent ensuite la socket percée à
//!    `nd-transport` (QUIC) — voir `nd-core`.
//!
//! La sonde `examples/p2p_two_process.rs` illustre la logique des deux pairs
//! (elle embarque son propre registre) ; `nd-registre` fournit la variante à
//! **registre partagé** que `nd-core` interroge pour une session réelle.

use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::process::ExitCode;

use nd_signaling::{serve, Registry, DEFAULT_TTL};

/// Adresse d'écoute par défaut : toutes les interfaces, port 9000.
const ADRESSE_DEFAUT: &str = "0.0.0.0:9000";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    let adresse = match adresse_ecoute(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("nd-registre : {e}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind(adresse) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("nd-registre : impossible d'écouter sur {adresse} : {e}");
            return ExitCode::FAILURE;
        }
    };
    // Adresse réellement liée : port effectif si 0 a été demandé.
    let liee = listener.local_addr().unwrap_or(adresse);

    let registry = Registry::new();
    // Balayeur de présence : retire périodiquement les pairs périmés (TTL).
    // Laissé en démon jusqu'à l'arrêt du processus (cf. doc `spawn_sweeper`).
    let _balayeur = registry.spawn_sweeper(DEFAULT_TTL / 2);

    println!("nd-registre : registre de rendez-vous NovaDesk à l'écoute sur {liee}");
    println!(
        "  expose : register / lookup / heartbeat / publish_candidates / \
         peer_candidates / request_punch / poll_punch"
    );
    println!(
        "  TTL de présence : {DEFAULT_TTL:?} — Ctrl+C pour arrêter \
         (état en mémoire, arrêt immédiat sûr)"
    );

    // Boucle bloquante (un thread par connexion) : tourne indéfiniment.
    // L'arrêt propre est best-effort : `Ctrl+C` termine le processus ; le
    // registre étant purement en mémoire, la terminaison est déjà propre.
    match serve(listener, registry) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nd-registre : arrêt sur erreur d'acceptation : {e}");
            ExitCode::FAILURE
        }
    }
}

/// Détermine l'adresse d'écoute : premier argument `adresse:port`, ou
/// [`ADRESSE_DEFAUT`] à défaut. Accepte une IP:port ou un nom résoluble ; en
/// cas d'ambiguïté (nom → plusieurs adresses), la première est retenue.
///
/// # Errors
/// Renvoie un message si l'argument n'est pas une adresse `adresse:port`
/// valide/résoluble.
fn adresse_ecoute(args: &[String]) -> Result<SocketAddr, String> {
    let brut = args.get(1).map_or(ADRESSE_DEFAUT, String::as_str);
    brut.to_socket_addrs()
        .map_err(|e| {
            format!(
                "adresse d'écoute invalide « {brut} » : {e} \
                 (format attendu : adresse:port, p. ex. 127.0.0.1:9000)"
            )
        })?
        .next()
        .ok_or_else(|| format!("aucune adresse résolue pour « {brut} »"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn defaut_sans_argument() {
        let a = adresse_ecoute(&args(&["nd-registre"])).unwrap();
        assert_eq!(a, ADRESSE_DEFAUT.parse::<SocketAddr>().unwrap());
        assert_eq!(a.port(), 9000);
    }

    #[test]
    fn argument_explicite_respecte() {
        let a = adresse_ecoute(&args(&["nd-registre", "127.0.0.1:1234"])).unwrap();
        assert_eq!(a, "127.0.0.1:1234".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn argument_invalide_rejete() {
        assert!(adresse_ecoute(&args(&["nd-registre", "127.0.0.1:pas_un_port"])).is_err());
        assert!(adresse_ecoute(&args(&["nd-registre", "sans_port"])).is_err());
    }
}
