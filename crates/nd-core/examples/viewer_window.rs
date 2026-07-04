//! Fenêtre viewer de démonstration : affiche **en direct** l'écran distant (ici en
//! loopback, donc le bureau local) reçu et décodé depuis QUIC. Preuve visuelle du
//! pipeline complet : capture → encode → QUIC → décode → **affichage**.
//!
//! Vue seule (aucune entrée injectée, pour éviter la boucle de retour en loopback ;
//! le chemin des entrées est prouvé par l'exemple `control_loop`). L'UI définitive
//! sera en Flutter (voir plan 10) ; ceci est une fenêtre de démo (minifb) qui se ferme
//! avec Échap ou automatiquement après quelques secondes.
//!
//! Lancer : `cargo run --release --example viewer_window -p nd-core`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use minifb::{Key, Window, WindowOptions};
use nd_capture::{create_capturer, CaptureConfig, CapturedFrame};
use nd_codec::{
    create_decoder, create_encoder, CodecKind, DecodedFrame, EncodedChunk, EncoderConfig,
};
use nd_proto::{ChannelKind, MonitorId, Reliability};
use nd_transport::{bind, connect};

const WIN_W: usize = 1280;
const WIN_H: usize = 720;
const MAX_SECONDS: u64 = 8;

/// Redimensionne (plus proche voisin) une image RGBA vers le tampon 0RGB de la fenêtre.
fn scale_into(frame: &DecodedFrame, dst: &mut [u32]) {
    let sw = frame.width as usize;
    let sh = frame.height as usize;
    if sw == 0 || sh == 0 || frame.rgba.len() < sw * sh * 4 {
        return;
    }
    for dy in 0..WIN_H {
        let sy = dy * sh / WIN_H;
        for dx in 0..WIN_W {
            let sx = dx * sw / WIN_W;
            let i = (sy * sw + sx) * 4;
            let r = u32::from(frame.rgba[i]);
            let g = u32::from(frame.rgba[i + 1]);
            let b = u32::from(frame.rgba[i + 2]);
            dst[dy * WIN_W + dx] = (r << 16) | (g << 8) | b;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind("127.0.0.1:0".parse()?)?;
    let addr = listener.local_addr();
    let cert = listener.server_cert_der();
    println!("Viewer window — hôte (serveur QUIC) sur {addr}");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_host = Arc::clone(&stop);

    // Hôte : capture → encode → envoie la vidéo en continu jusqu'à l'arrêt.
    let host = thread::spawn(move || -> Result<(), String> {
        let mut transport = listener.accept().map_err(|e| e.to_string())?;
        let mut capturer = create_capturer().map_err(|e| e.to_string())?;
        capturer
            .start(CaptureConfig {
                monitor: MonitorId(0),
                target_fps: 60,
                capture_cursor: true,
            })
            .map_err(|e| e.to_string())?;
        let mut encoder = create_encoder(CodecKind::H264).map_err(|e| e.to_string())?;
        let video_ch = transport.open_channel(ChannelKind::Video(MonitorId(0)));

        let mut configured = false;
        let mut last: Option<CapturedFrame> = None;
        let mut first = true;
        while !stop_host.load(Ordering::Relaxed) {
            let frame = capturer.next_frame().map_err(|e| e.to_string())?;
            if frame.image.is_some() {
                if !configured {
                    encoder
                        .configure(EncoderConfig {
                            kind: CodecKind::H264,
                            width: frame.width,
                            height: frame.height,
                            target_bitrate_kbps: 12_000,
                            max_fps: 60,
                        })
                        .map_err(|e| e.to_string())?;
                    configured = true;
                }
                last = Some(frame);
            }
            if configured {
                if let Some(f) = &last {
                    let chunk = encoder.encode(f, first).map_err(|e| e.to_string())?;
                    first = false;
                    if transport
                        .send(video_ch, chunk.data, Reliability::UnreliableFec)
                        .is_err()
                    {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(12));
        }
        Ok(())
    });

    // Viewer (thread principal) : connexion, décodage, affichage minifb.
    let mut transport = connect(addr, &cert)?;
    let mut decoder = create_decoder(CodecKind::H264)?;

    let mut window = Window::new(
        "NovaDesk — viewer (loopback, vue seule)",
        WIN_W,
        WIN_H,
        WindowOptions::default(),
    )?;
    let mut buffer = vec![0u32; WIN_W * WIN_H];

    let start = Instant::now();
    let mut decoded = 0usize;
    let mut presented = 0usize;
    while window.is_open()
        && !window.is_key_down(Key::Escape)
        && start.elapsed() < Duration::from_secs(MAX_SECONDS)
    {
        // Décoder toutes les images en attente ; ne garder que la plus récente.
        let mut newest: Option<DecodedFrame> = None;
        while let Some((_h, data)) = transport.poll_recv()? {
            let chunk = EncodedChunk {
                data,
                is_keyframe: false,
                monitor: MonitorId(0),
                timestamp_us: 0,
            };
            if let Some(f) = decoder.decode(&chunk)? {
                decoded += 1;
                newest = Some(f);
            }
        }
        if let Some(f) = &newest {
            scale_into(f, &mut buffer);
        }
        window.update_with_buffer(&buffer, WIN_W, WIN_H)?;
        presented += 1;
        thread::sleep(Duration::from_millis(8));
    }

    stop.store(true, Ordering::Relaxed);
    let _ = host.join();

    println!("Viewer fermé : {decoded} images décodées, {presented} rafraîchissements présentés.");
    if decoded >= 1 {
        println!("OK : affichage de l'écran distant validé (pipeline complet jusqu'à la fenêtre).");
        Ok(())
    } else {
        Err("aucune image décodée à afficher".into())
    }
}
