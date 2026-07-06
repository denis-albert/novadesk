//! Sonde de la forme du curseur : capture le bitmap du curseur courant (GDI, sans
//! duplication d'écran) et affiche dimensions, hotspot et nombre de pixels non
//! transparents — preuve que l'extraction RGBA lit de vrais pixels.
//!
//! Lancer : `cargo run --example cursor_probe -p nd-capture`

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    match nd_capture::capture_cursor_shape()? {
        Some(forme) => {
            let opaques = forme.rgba.chunks_exact(4).filter(|px| px[3] != 0).count();
            let total = (forme.width * forme.height) as usize;
            println!(
                "curseur : {}x{} hotspot=({}, {}) rgba={} octets \
                 pixels_non_transparents={opaques}/{total}",
                forme.width,
                forme.height,
                forme.hotspot_x,
                forme.hotspot_y,
                forme.rgba.len(),
            );
        }
        None => println!("aucun curseur visible actuellement."),
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("cursor_probe : exemple Windows uniquement (impl macOS/Linux à venir, plan 02/16).");
}
