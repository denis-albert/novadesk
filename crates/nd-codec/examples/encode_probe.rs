//! Sonde d'encodage : capture l'écran, encode chaque frame en H.264 et affiche la
//! taille, le type (clé/delta) et le taux de compression, puis re-décode le flux pour
//! prouver qu'il est valide.
//!
//! Lancer (release recommandé pour la vitesse de l'encodeur logiciel) :
//! `cargo run --release --example encode_probe -p nd-codec`

use nd_capture::{create_capturer, CaptureConfig, FrameImage};
use nd_codec::{create_encoder, CodecKind, EncoderConfig};
use nd_proto::MonitorId;
use openh264::decoder::Decoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cap = create_capturer()?;
    cap.start(CaptureConfig {
        monitor: MonitorId(0),
        target_fps: 60,
        capture_cursor: false,
    })?;

    let mut enc = create_encoder(CodecKind::H264)?;
    let mut decoder = Decoder::new()?;

    let mut configured = false;
    let mut encoded = 0;
    let mut attempts = 0;
    let mut total_raw: u64 = 0;
    let mut total_h264: u64 = 0;
    let mut saw_keyframe = false;

    println!("Encodage H.264 (logiciel). Bougez une fenêtre pour générer des frames…");

    while encoded < 8 && attempts < 500 {
        attempts += 1;
        let frame = cap.next_frame()?;
        let Some(FrameImage::Cpu { .. }) = frame.image else {
            std::thread::sleep(std::time::Duration::from_millis(8));
            continue;
        };

        if !configured {
            enc.configure(EncoderConfig {
                kind: CodecKind::H264,
                width: frame.width,
                height: frame.height,
                target_bitrate_kbps: 8_000,
                max_fps: 60,
            })?;
            configured = true;
        }

        let force_key = encoded == 0; // première frame en image-clé
        let chunk = enc.encode(&frame, force_key)?;
        encoded += 1;

        let raw = u64::from(frame.width) * u64::from(frame.height) * 4;
        let h264 = chunk.data.len() as u64;
        total_raw += raw;
        total_h264 += h264;
        saw_keyframe |= chunk.is_keyframe;

        // Validation : le flux doit se re-décoder sans erreur.
        let decoded_ok = decoder.decode(&chunk.data)?.is_some();
        let ratio = raw as f64 / h264.max(1) as f64;

        println!(
            "frame {encoded} : {}x{} | H.264 {h264} o | clé={} | ratio={ratio:.1}x | redécodé={decoded_ok}",
            frame.width, frame.height, chunk.is_keyframe,
        );
    }

    cap.stop();

    let avg_ratio = total_raw as f64 / total_h264.max(1) as f64;
    println!(
        "Terminé : {encoded} frame(s) encodée(s), keyframe vue={saw_keyframe}, \
         compression moyenne={avg_ratio:.1}x ({total_raw} o bruts -> {total_h264} o H.264)."
    );
    Ok(())
}
