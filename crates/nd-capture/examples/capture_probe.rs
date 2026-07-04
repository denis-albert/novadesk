//! Sonde de capture : capture quelques frames de l'écran principal et affiche leurs
//! caractéristiques. Sert à valider l'implémentation DXGI sur une machine réelle.
//!
//! Lancer : `cargo run --example capture_probe -p nd-capture`

use nd_capture::{create_capturer, CaptureConfig, FrameImage};
use nd_proto::MonitorId;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cap = create_capturer()?;
    cap.start(CaptureConfig {
        monitor: MonitorId(0),
        target_fps: 60,
        capture_cursor: true,
    })?;

    println!("Capture démarrée. Bougez la souris / une fenêtre pour générer des frames…");

    let mut captured = 0;
    let mut attempts = 0;
    while captured < 5 && attempts < 300 {
        attempts += 1;
        match cap.next_frame() {
            Ok(frame) => {
                if let Some(FrameImage::Cpu { data, stride }) = &frame.image {
                    captured += 1;
                    // Somme de contrôle légère pour prouver qu'on lit de vrais pixels.
                    let checksum: u64 = data.iter().step_by(997).map(|&b| u64::from(b)).sum();
                    println!(
                        "frame {captured} : {}x{} stride={stride} régions_modifiées={} \
                         curseur={:?} t={}µs checksum={checksum}",
                        frame.width,
                        frame.height,
                        frame.dirty.len(),
                        frame.cursor.map(|c| (c.x, c.y)),
                        frame.timestamp_us,
                    );
                }
            }
            Err(e) => {
                eprintln!("erreur de capture : {e}");
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }

    cap.stop();
    println!("Terminé : {captured} frame(s) capturée(s) en {attempts} tentative(s).");
    Ok(())
}
