//! Sonde multi-écran (plan 13) : énumère les moniteurs attachés au bureau puis
//! capture une frame de CHACUN via [`nd_capture::create_capturer`], preuve que le
//! capteur DXGI fonctionne avec un `MonitorId` quelconque (pas seulement 0).
//!
//! Lancer : `cargo run --example monitors_probe -p nd-capture`

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nd_capture::{create_capturer, enumerate_monitors, CaptureConfig, FrameImage};

    let monitors = enumerate_monitors()?;
    println!("{} moniteur(s) détecté(s) :", monitors.len());
    for m in &monitors {
        println!(
            "  {:?} « {} » : {}x{} @ ({}, {}){}",
            m.id,
            m.name,
            m.width,
            m.height,
            m.x,
            m.y,
            if m.is_primary { " [principal]" } else { "" },
        );
    }

    // Preuve multi-écran : une frame de chaque moniteur, avec un capteur dédié.
    for m in &monitors {
        let mut cap = create_capturer()?;
        cap.start(CaptureConfig {
            monitor: m.id,
            target_fps: 60,
            capture_cursor: false,
        })?;

        // La duplication livre en général la frame initiale tout de suite, mais un
        // écran parfaitement statique peut faire expirer quelques acquisitions.
        let mut captured = false;
        for _ in 0..120 {
            let frame = cap.next_frame()?;
            if let Some(FrameImage::Cpu { data, stride }) = &frame.image {
                println!(
                    "  capture {:?} : frame {}x{} stride={stride} ({} octets)",
                    m.id,
                    frame.width,
                    frame.height,
                    data.len(),
                );
                captured = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        if !captured {
            println!(
                "  capture {:?} : aucune frame reçue (écran statique ?) — duplication \
                 néanmoins établie sur la sortie {}",
                m.id, m.id.0,
            );
        }
        cap.stop();
    }

    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "monitors_probe : exemple Windows uniquement (impl macOS/Linux à venir, voir plan 02/16)."
    );
}
