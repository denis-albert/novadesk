//! Reconnexion **transparente** en bouclage QUIC : un client enveloppé dans un
//! [`ReconnectingTransport`](nd_transport::ReconnectingTransport) survit à une
//! coupure simulée (chute du serveur) sans que le code appelant ne change — le
//! même `send` et le même handle de canal continuent après rétablissement.
//!
//! Lancer : `cargo run --example reconnexion -p nd-transport`

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nd_proto::{ChannelKind, Reliability};
use nd_transport::{bind, connect, ReconnectingTransport, Transport};

/// Draine `poll_recv` jusqu'au prochain message ou à l'expiration.
fn attendre(transport: &mut Box<dyn Transport>, timeout: Duration) -> Option<Vec<u8>> {
    let debut = Instant::now();
    while debut.elapsed() < timeout {
        if let Some((_handle, data)) = transport.poll_recv().ok()? {
            return Some(data);
        }
        thread::sleep(Duration::from_millis(2));
    }
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind("127.0.0.1:0".parse()?)?;
    let addr = listener.local_addr();
    let cert = listener.server_cert_der();
    println!("Serveur QUIC en écoute sur {addr}");

    // Boucle d'acceptation : l'écouteur survit aux connexions individuelles ; il
    // pourra donc accepter la connexion de reconnexion.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(transport) = listener.accept() {
            if tx.send(transport).is_err() {
                break;
            }
        }
    });

    // Client enveloppé : la fabrique de reconnexion re-`connect` au même écouteur
    // (même certificat épinglé). Tout le reste du code ignore la reconnexion.
    let cert_fabrique = cert.clone();
    let mut client =
        ReconnectingTransport::new(connect(addr, &cert)?, move || connect(addr, &cert_fabrique));

    // Première connexion + échange.
    let mut serveur = rx.recv_timeout(Duration::from_secs(5))?;
    let canal = client.open_channel(ChannelKind::Control);
    client.send(
        canal,
        b"message avant coupure".to_vec(),
        Reliability::Reliable,
    )?;
    let recu = attendre(&mut serveur, Duration::from_secs(5)).ok_or("message 1 non reçu")?;
    println!("Reçu : {:?}", String::from_utf8_lossy(&recu));

    // Coupure simulée : la chute du serveur ferme la connexion du client.
    println!("--- coupure du serveur ---");
    drop(serveur);
    let debut = Instant::now();
    while client.is_connected() && debut.elapsed() < Duration::from_secs(15) {
        thread::sleep(Duration::from_millis(10));
    }
    println!(
        "Coupure détectée (reconnexions jusqu'ici : {})",
        client.reconnexions()
    );

    // Le même handle et le même `send` continuent : reconnexion transparente.
    client.send(
        canal,
        b"message apres coupure".to_vec(),
        Reliability::Reliable,
    )?;
    let mut serveur2 = rx.recv_timeout(Duration::from_secs(5))?;
    let recu = attendre(&mut serveur2, Duration::from_secs(5)).ok_or("message 2 non reçu")?;
    println!(
        "Reçu après reconnexion : {:?}",
        String::from_utf8_lossy(&recu)
    );
    println!("Reconnexions totales : {}", client.reconnexions());

    if client.reconnexions() >= 1 && recu == b"message apres coupure" {
        println!("OK : reconnexion transparente validée en bouclage.");
        Ok(())
    } else {
        Err("la reconnexion transparente n'a pas abouti".into())
    }
}
