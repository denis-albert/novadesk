//! Session **chiffrée de bout en bout** (Noise par-dessus QUIC) — plan 06.
//!
//! Deux pairs établissent une connexion QUIC, réalisent le handshake Noise XX sur le
//! canal de contrôle, puis échangent des messages chiffrés de bout en bout. On vérifie
//! que chaque pair a appris la clé publique de l'autre (empreintes croisées) : c'est ce
//! qui garantit qu'un relais intermédiaire ne pourrait rien déchiffrer.
//!
//! Lancer : `cargo run --example e2e_session -p nd-core`

use std::thread;
use std::time::Duration;

use nd_core::{establish, EncryptedTransport};
use nd_crypto::{generate_static_keypair, HandshakeRole, PeerFingerprint};
use nd_proto::{ChannelKind, Reliability};
use nd_transport::{bind, connect, Transport};

const N: usize = 4;

/// Envoie `N` messages chiffrés préfixés par `prefix`, puis en reçoit `N` qui doivent
/// commencer par `expect`. Renvoie le nombre reçu et validé.
fn echanger(enc: &mut EncryptedTransport, prefix: &str, expect: &str) -> Result<usize, String> {
    let ch = enc.open_channel(ChannelKind::Control);
    for i in 0..N {
        enc.send(
            ch,
            format!("{prefix} #{i}").into_bytes(),
            Reliability::Reliable,
        )
        .map_err(|e| e.to_string())?;
    }
    let mut recus = 0;
    let mut tentatives = 0;
    while recus < N && tentatives < 4000 {
        tentatives += 1;
        match enc.poll_recv().map_err(|e| e.to_string())? {
            Some((_h, data)) => {
                let texte = String::from_utf8_lossy(&data);
                if texte.starts_with(expect) {
                    recus += 1;
                } else {
                    return Err(format!("message déchiffré inattendu : {texte}"));
                }
            }
            None => thread::sleep(Duration::from_millis(2)),
        }
    }
    Ok(recus)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Chaque pair a sa paire de clés statique X25519 (identité cryptographique).
    let cles_hote = generate_static_keypair()?;
    let cles_viewer = generate_static_keypair()?;
    let empreinte_hote = PeerFingerprint::from_public_key(&cles_hote.public);
    let empreinte_viewer = PeerFingerprint::from_public_key(&cles_viewer.public);

    let listener = bind("127.0.0.1:0".parse()?)?;
    let addr = listener.local_addr();
    let cert = listener.server_cert_der();
    println!("Session E2E — hôte (serveur QUIC) sur {addr}");

    // Hôte = répondeur du handshake Noise.
    let hote_prive = cles_hote.private.clone();
    let hote = thread::spawn(
        move || -> Result<(usize, Option<PeerFingerprint>), String> {
            let inner = listener.accept().map_err(|e| e.to_string())?;
            let mut enc = establish(inner, HandshakeRole::Responder, &hote_prive)
                .map_err(|e| e.to_string())?;
            let recus = echanger(&mut enc, "hôte", "viewer")?;
            Ok((recus, enc.remote_fingerprint()))
        },
    );

    // Viewer = initiateur du handshake Noise.
    let inner = connect(addr, &cert)?;
    let mut enc = establish(inner, HandshakeRole::Initiator, &cles_viewer.private)?;
    let viewer_recus = echanger(&mut enc, "viewer", "hôte")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let viewer_voit = enc.remote_fingerprint();

    let (hote_recus, hote_voit) = hote
        .join()
        .expect("thread hôte")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    println!(
        "Empreintes statiques — hôte {}  /  viewer {}",
        empreinte_hote.short_hex(),
        empreinte_viewer.short_hex()
    );
    println!(
        "Vérification MITM (SAS) — le viewer voit l'hôte : {} (attendu {})",
        viewer_voit
            .as_ref()
            .map_or_else(|| "?".into(), PeerFingerprint::sas),
        empreinte_hote.sas()
    );
    println!("Messages E2E — viewer a reçu {viewer_recus}/{N}, hôte a reçu {hote_recus}/{N}.");

    let empreintes_croisees_ok = viewer_voit.as_ref().map(|f| f.0) == Some(empreinte_hote.0)
        && hote_voit.as_ref().map(|f| f.0) == Some(empreinte_viewer.0);

    if viewer_recus == N && hote_recus == N && empreintes_croisees_ok {
        println!("OK : chiffrement de bout en bout (Noise) validé par-dessus QUIC — empreintes croisées vérifiées, le relais ne verrait que du chiffré.");
        Ok(())
    } else {
        Err("échec E2E : messages ou empreintes incorrects".into())
    }
}
