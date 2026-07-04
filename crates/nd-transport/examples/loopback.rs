//! Bouclage QUIC : un serveur et un client sur la machine locale échangent des
//! messages multiplexés (canaux vidéo + input), pour valider le transport de bout en
//! bout (connexion, chiffrement TLS, framing, files).
//!
//! Lancer : `cargo run --example loopback -p nd-transport`

use std::thread;
use std::time::Duration;

use nd_proto::{ChannelKind, MonitorId, Reliability};
use nd_transport::{bind, connect};

const N: usize = 5;
const VIDEO_LEN: usize = 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind("127.0.0.1:0".parse()?)?;
    let addr = listener.local_addr();
    let cert = listener.server_cert_der();
    println!("Serveur QUIC en écoute sur {addr}");

    // Client : se connecte, ouvre deux canaux et envoie N messages sur chacun.
    let client = thread::spawn(move || -> Result<(), String> {
        let mut conn = connect(addr, &cert).map_err(|e| e.to_string())?;
        let h_video = conn.open_channel(ChannelKind::Video(MonitorId(0)));
        let h_input = conn.open_channel(ChannelKind::Input);
        for i in 0..N {
            let frame = vec![i as u8; VIDEO_LEN];
            conn.send(h_video, frame, Reliability::UnreliableFec)
                .map_err(|e| e.to_string())?;
            conn.send(h_input, vec![0xAA, i as u8], Reliability::Reliable)
                .map_err(|e| e.to_string())?;
        }
        println!("Client : {N} frames vidéo + {N} messages input envoyés.");
        // Laisser les tâches d'émission vider les files avant de fermer.
        thread::sleep(Duration::from_millis(600));
        Ok(())
    });

    // Serveur : accepte et draine les messages entrants.
    let mut server = listener.accept()?;
    println!(
        "Serveur : client connecté (RTT ~{} µs).",
        server.path_estimate().rtt_us
    );

    let expected = 2 * N;
    let mut got = 0;
    let mut video_msgs = 0;
    let mut input_msgs = 0;
    let mut video_ok = true;
    let mut attempts = 0;
    while got < expected && attempts < 3000 {
        attempts += 1;
        match server.poll_recv()? {
            Some((_handle, data)) => {
                got += 1;
                if data.len() == VIDEO_LEN {
                    video_msgs += 1;
                    // Une frame « i » est remplie du même octet i.
                    if !data.iter().all(|&b| b == data[0]) {
                        video_ok = false;
                    }
                } else if data.len() == 2 && data[0] == 0xAA {
                    input_msgs += 1;
                }
            }
            None => thread::sleep(Duration::from_millis(2)),
        }
    }

    let _ = client.join();
    println!(
        "Serveur : {got}/{expected} messages reçus — vidéo={video_msgs} (intègres={video_ok}), input={input_msgs}."
    );
    if got == expected && video_ok && video_msgs == N && input_msgs == N {
        println!("OK : transport QUIC validé de bout en bout.");
        Ok(())
    } else {
        Err("transport incomplet : messages manquants ou corrompus".into())
    }
}
