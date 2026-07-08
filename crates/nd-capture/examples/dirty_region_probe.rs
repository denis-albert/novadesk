//! Sonde des **régions modifiées** et du **cadre d'écran** (sous-région).
//!
//! Deux volets, sur une machine réelle :
//! 1. **Dirty/move-rects** (Windows/DXGI) : bougez une fenêtre ou faites défiler ;
//!    la sonde affiche les rectangles remontés dans `CapturedFrame::dirty`
//!    (dommages `GetFrameDirtyRects` + destinations `GetFrameMoveRects` fusionnés)
//!    et vérifie qu'ils tiennent tous dans la frame.
//! 2. **Sous-région** ([`ScreenCapturer::set_region`]) : restreint la capture à un
//!    rectangle centré et vérifie que les frames ont bien les dimensions du cadre et
//!    que chaque région modifiée reste dans ces bornes.
//!
//! Lancer : `cargo run --example dirty_region_probe -p nd-capture`

use nd_capture::{create_capturer, CaptureConfig, Rect};
use nd_proto::MonitorId;

/// Vérifie que chaque rectangle modifié tient dans `w`×`h`. Renvoie le nombre de
/// rectangles hors bornes (doit être 0).
fn compte_hors_bornes(dirty: &[Rect], w: u32, h: u32) -> usize {
    dirty
        .iter()
        .filter(|r| r.x + r.w > w || r.y + r.h > h)
        .count()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cap = match create_capturer() {
        Ok(c) => c,
        Err(e) => {
            println!("create_capturer indisponible ({e}) — sonde ignorée.");
            return Ok(());
        }
    };
    if let Err(e) = cap.start(CaptureConfig {
        monitor: MonitorId(0),
        target_fps: 60,
        capture_cursor: true,
    }) {
        println!("start indisponible ({e}) — sonde ignorée (session sans bureau ?).");
        return Ok(());
    }

    // --- Volet 1 : régions modifiées plein écran -------------------------------
    println!("== Volet 1 : régions modifiées (bougez une fenêtre / faites défiler) ==");
    let mut frames = 0u32;
    let mut frames_avec_dirty = 0u32;
    let mut max_rects = 0usize;
    let mut hors_bornes = 0usize;
    let mut dims = (0u32, 0u32);
    // Borné en temps réel : sur un écran inactif, chaque `next_frame` peut attendre
    // ~100 ms (délai `AcquireNextFrame`) — on ne boucle pas indéfiniment.
    let debut = std::time::Instant::now();
    while debut.elapsed() < std::time::Duration::from_secs(5) && frames_avec_dirty < 20 {
        match cap.next_frame() {
            Ok(f) => {
                dims = (f.width, f.height);
                if f.image.is_some() {
                    frames += 1;
                }
                if !f.dirty.is_empty() {
                    frames_avec_dirty += 1;
                    max_rects = max_rects.max(f.dirty.len());
                    hors_bornes += compte_hors_bornes(&f.dirty, f.width, f.height);
                    if frames_avec_dirty <= 3 {
                        println!(
                            "  frame {}x{} : {} région(s) modifiée(s) → {:?}",
                            f.width,
                            f.height,
                            f.dirty.len(),
                            &f.dirty[..f.dirty.len().min(4)],
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("  erreur de capture : {e}");
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    println!(
        "  → {frames} frame(s) pleine(s), {frames_avec_dirty} avec régions modifiées, \
         max {max_rects} rect/frame, {hors_bornes} hors bornes (attendu 0)."
    );

    // --- Volet 2 : cadre d'écran (sous-région) ---------------------------------
    let (fw, fh) = dims;
    if fw >= 8 && fh >= 8 {
        // Cadre centré, moitié de l'écran.
        let cadre = Rect {
            x: fw / 4,
            y: fh / 4,
            w: fw / 2,
            h: fh / 2,
        };
        println!("== Volet 2 : cadre d'écran {cadre:?} (moitié centrée) ==");
        match cap.set_region(Some(cadre)) {
            Ok(()) => {
                // Même les frames sans pixels (délai écoulé) rapportent désormais les
                // dimensions du cadre : on vérifie donc sur toutes les frames.
                let mut ok_dims = 0u32;
                let mut mauvais_dims = 0u32;
                let mut avec_pixels = 0u32;
                let mut hors = 0usize;
                let debut2 = std::time::Instant::now();
                while debut2.elapsed() < std::time::Duration::from_secs(3) && ok_dims < 5 {
                    if let Ok(f) = cap.next_frame() {
                        if f.width == cadre.w && f.height == cadre.h {
                            ok_dims += 1;
                        } else {
                            mauvais_dims += 1;
                        }
                        if f.image.is_some() {
                            avec_pixels += 1;
                        }
                        hors += compte_hors_bornes(&f.dirty, f.width, f.height);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
                println!(
                    "  → {ok_dims} frame(s) aux dimensions du cadre ({}x{}) dont {avec_pixels} \
                     avec pixels, {mauvais_dims} incorrecte(s), {hors} hors bornes (attendu 0).",
                    cadre.w, cadre.h
                );
                // Retour au plein écran.
                let _ = cap.set_region(None);
            }
            Err(e) => println!("  set_region non gérée par ce backend : {e}"),
        }
    }

    cap.stop();
    println!("Terminé.");
    Ok(())
}
