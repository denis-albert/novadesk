//! Tranche verticale complète (Phase 1) : un **hôte** capture l'écran, l'encode en
//! H.264 et l'envoie sur QUIC ; un **viewer** reçoit et décode. Tout le vrai pipeline
//! traverse : capture DXGI → openh264 → transport QUIC → openh264 (décodage).
//!
//! Lancer : `cargo run --release --example vertical_slice -p nd-core`

use std::thread;

use nd_capture::create_capturer;
use nd_codec::{create_decoder, create_encoder, CodecKind};
use nd_core::{HostPipeline, ViewerPipeline};
use nd_transport::{bind, connect};

const N: usize = 10;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind("127.0.0.1:0".parse()?)?;
    let addr = listener.local_addr();
    let cert = listener.server_cert_der();
    println!("Tranche verticale — hôte (serveur QUIC) sur {addr}");

    // Viewer (client) : se connecte, reçoit et décode dans un thread dédié.
    let viewer = thread::spawn(move || -> Result<(usize, Option<(u32, u32)>), String> {
        let transport = connect(addr, &cert).map_err(|e| e.to_string())?;
        let decoder = create_decoder(CodecKind::H264).map_err(|e| e.to_string())?;
        let mut viewer = ViewerPipeline::new(transport, decoder);
        viewer.run(N).map_err(|e| e.to_string())
    });

    // Hôte : accepte le viewer, puis capture → encode → envoie N images.
    let host_transport = listener.accept()?;
    let capturer = create_capturer()?;
    let encoder = create_encoder(CodecKind::H264)?;
    let mut host = HostPipeline::new(capturer, encoder, host_transport)?;
    let sent = host.run(N)?;
    println!("Hôte : {sent} images capturées → encodées (H.264) → envoyées (QUIC).");

    let (decoded, dims) = viewer
        .join()
        .expect("thread viewer")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    println!("Viewer : {decoded} images reçues → décodées, dimensions {dims:?}.");

    // Garder l'hôte en vie jusqu'ici pour que le transport finisse d'émettre.
    drop(host);

    if decoded >= N - 1 && dims.is_some() {
        println!("OK : pipeline capture → encode → QUIC → décode validé de bout en bout.");
        Ok(())
    } else {
        Err(format!("pipeline incomplet : {decoded}/{N} images décodées").into())
    }
}
