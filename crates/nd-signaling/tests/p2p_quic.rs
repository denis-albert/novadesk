//! Test d'intégration **plan 05** : chaîne complète par ID —
//! rendez-vous → candidats → hole punching → **QUIC sur la socket percée** →
//! transfert de messages dans les deux sens.
//!
//! C'est la version « in-process » (boucle locale, threads) de la sonde
//! `examples/p2p_two_process.rs` (deux processus, vraies adresses
//! d'interface). `nd-transport` n'est qu'une dépendance de dev : à
//! l'exécution, l'assemblage connecteur + transport appartient à `nd-core`.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use nd_proto::{ChannelKind, NovaId, Reliability};
use nd_signaling::{
    await_p2p, establish_p2p, serve, ConnAttempt, P2pIncoming, Registry, RendezvousClient,
};
use nd_transport::{
    accept_quic_over_socket, connect_quic_over_socket, QuicTransport, ServerIdentity, Transport,
};

/// Nombre de messages transférés dans chaque sens.
const N_MESSAGES: u32 = 10;

/// Draine `poll_recv` jusqu'au prochain message ou à l'expiration.
fn attendre_message(transport: &mut QuicTransport, timeout: Duration) -> Option<Vec<u8>> {
    let debut = Instant::now();
    while debut.elapsed() < timeout {
        if let Some((_, data)) = transport.poll_recv().expect("poll_recv") {
            return Some(data);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

#[test]
fn connexion_par_id_punch_puis_quic_et_transfert() {
    // Rendez-vous éphémère.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind rendez-vous");
    let addr_rv = listener.local_addr().expect("adresse rendez-vous");
    let registry = Registry::new();
    std::thread::spawn(move || {
        let _ = serve(listener, registry);
    });

    let id_appelant = NovaId(111_222_333);
    let id_appele = NovaId(444_555_666);

    // Côté appelé (contrôlé) : identité TLS stable, certificat publié au
    // rendez-vous, attente P2P puis QUIC serveur sur la socket percée.
    let appele = std::thread::spawn(move || {
        let rv = RendezvousClient::new(addr_rv);
        let identite = ServerIdentity::generate().expect("identité");
        // Pas d'écouteur direct dans ce scénario pur punch : l'adresse
        // enregistrée est un simple espace réservé.
        rv.register(id_appele, "0.0.0.0:0".parse().unwrap(), identite.cert_der())
            .expect("register");

        let entrant = await_p2p(&rv, id_appele, &[], Duration::from_secs(10)).expect("await_p2p");
        let P2pIncoming::Direct(chemin) = entrant else {
            panic!("punch attendu en boucle locale");
        };
        assert_eq!(chemin.from, id_appelant);

        let mut transport =
            accept_quic_over_socket(chemin.socket, &identite).expect("accept_over_socket");

        // Reçoit N messages et renvoie chacun en écho.
        let canal = transport.open_channel(ChannelKind::Control);
        let mut recus = 0u32;
        while recus < N_MESSAGES {
            let data =
                attendre_message(&mut transport, Duration::from_secs(5)).expect("message attendu");
            transport
                .send(canal, data, Reliability::Reliable)
                .expect("écho");
            recus += 1;
        }
        // Le transport est RENDU au test (pas jeté ici) : `Drop` ferme la
        // connexion immédiatement et détruirait le dernier écho encore en vol
        // — il ne doit tomber qu'après que l'appelant a tout reçu.
        (recus, transport)
    });

    // Côté appelant (contrôleur) : établissement P2P par ID puis QUIC client
    // sur la socket percée, en épinglant le certificat résolu par le lookup.
    // L'appelé s'enregistre et publie ses candidats depuis son thread : on
    // retente jusqu'à l'échéance tant qu'il n'est pas prêt.
    let rv = RendezvousClient::new(addr_rv);
    let echeance = Instant::now() + Duration::from_secs(10);
    let tentative = loop {
        match establish_p2p(&rv, id_appelant, id_appele, &[]) {
            // Pas encore enregistré (identité en cours de génération).
            Err(e) if Instant::now() < echeance => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("establish_p2p : {e}"),
            // Enregistré mais candidats pas encore publiés : on retente.
            Ok(ConnAttempt::RelayFallback { reason, .. })
                if reason.contains("aucun candidat") && Instant::now() < echeance =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(autre) => break autre,
        }
    };
    let ConnAttempt::Direct(chemin) = tentative else {
        panic!("punch attendu en boucle locale");
    };

    let mut transport =
        connect_quic_over_socket(chemin.socket, chemin.peer_addr, &chemin.peer_cert_der)
            .expect("connect_over_socket");

    // Envoie N messages et attend chaque écho.
    let canal = transport.open_channel(ChannelKind::Control);
    let mut echos = 0u32;
    for i in 0..N_MESSAGES {
        let message = format!("message-{i}").into_bytes();
        transport
            .send(canal, message.clone(), Reliability::Reliable)
            .expect("send");
        let echo = attendre_message(&mut transport, Duration::from_secs(5));
        let Some(echo) = echo else {
            panic!(
                "écho {i} attendu (connecté : {}, raison : {:?})",
                transport.is_connected(),
                transport.close_reason(),
            );
        };
        assert_eq!(echo, message, "écho fidèle");
        echos += 1;
    }
    assert_eq!(echos, N_MESSAGES);
    assert!(transport.is_connected());

    // Le thread appelé rend son transport : les deux extrémités ne sont
    // fermées qu'ici, une fois tous les échos comptés.
    let (recus, transport_appele) = appele.join().expect("thread appelé");
    assert_eq!(recus, N_MESSAGES);
    drop(transport_appele);
    drop(transport);
}
