//! Connexion **par ID** via le serveur de rendez-vous (Phase 2) :
//! - un serveur de rendez-vous tourne en local ;
//! - l'hôte ouvre un écouteur QUIC et publie (ID → adresse + certificat) ;
//! - le viewer résout l'ID, obtient l'adresse + le certificat, et se connecte.
//!
//! Mise en relation directe (loopback) ; le NAT traversal et le relais viendront
//! ensuite (voir plan 05). Lancer : `cargo run --example connect_by_id -p nd-core`

use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use nd_proto::{ChannelKind, NovaId, Reliability};
use nd_signaling::{serve, Registry, RendezvousClient};
use nd_transport::{bind, connect};

const MSG_N: usize = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Serveur de rendez-vous local.
    let rv_listener = TcpListener::bind("127.0.0.1:0")?;
    let rv_addr = rv_listener.local_addr()?;
    thread::spawn(move || {
        let _ = serve(rv_listener, Registry::new());
    });
    println!("Rendez-vous en écoute sur {rv_addr}");

    let host_id = NovaId(123_456_789);

    // 2. Hôte : écouteur QUIC, publication de l'ID, puis émission de messages.
    let host = thread::spawn(move || -> Result<(), String> {
        let listener = bind("127.0.0.1:0".parse().unwrap()).map_err(|e| e.to_string())?;
        let quic_addr = listener.local_addr();
        let cert = listener.server_cert_der();
        RendezvousClient::new(rv_addr)
            .register(host_id, quic_addr, &cert)
            .map_err(|e| e.to_string())?;
        println!("Hôte : ID {host_id} publié → {quic_addr}");

        let mut transport = listener.accept().map_err(|e| e.to_string())?;
        let ch = transport.open_channel(ChannelKind::Control);
        for i in 0..MSG_N {
            transport
                .send(ch, vec![i as u8; 256], Reliability::Reliable)
                .map_err(|e| e.to_string())?;
        }
        thread::sleep(Duration::from_millis(400));
        Ok(())
    });

    // 3. Viewer : résolution de l'ID (avec quelques tentatives), puis connexion.
    let rv = RendezvousClient::new(rv_addr);
    let mut record = None;
    for _ in 0..40 {
        if let Ok(r) = rv.lookup(host_id) {
            record = Some(r);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let record = record.ok_or("ID jamais résolu (hôte non enregistré)")?;
    println!(
        "Viewer : ID {host_id} résolu → {} (certificat {} octets)",
        record.addr,
        record.cert_der.len()
    );

    let mut transport = connect(record.addr, &record.cert_der)?;
    let mut got = 0;
    let mut attempts = 0;
    while got < MSG_N && attempts < 3000 {
        attempts += 1;
        match transport.poll_recv()? {
            Some((_h, data)) if data.len() == 256 => got += 1,
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(2)),
        }
    }

    let _ = host.join();
    println!("Viewer : connecté par ID, {got}/{MSG_N} messages reçus.");
    if got == MSG_N {
        println!("OK : connexion par ID via rendez-vous validée (Phase 2).");
        Ok(())
    } else {
        Err("échec de la connexion par ID".into())
    }
}
