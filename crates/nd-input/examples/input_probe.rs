//! Sonde d'injection d'entrées : déplace le curseur vers des cibles et vérifie via
//! `GetCursorPos` qu'il les atteint réellement, puis restaure la position d'origine.
//! Teste aussi la molette et une saisie Unicode (acceptation).
//!
//! Le curseur bouge brièvement puis revient à sa place.
//!
//! Lancer : `cargo run --example input_probe -p nd-input`
#![allow(unsafe_code)]

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nd_input::create_injector;
    use nd_proto::MonitorId;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SetCursorPos, SM_CXSCREEN, SM_CYSCREEN,
    };

    let injector = create_injector()?;

    // Sauvegarde de la position d'origine.
    let mut origin = POINT::default();
    // SAFETY : `origin` est un POINT valide.
    unsafe { GetCursorPos(&mut origin)? };

    // SAFETY : appels FFI sans effet de bord.
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    println!(
        "Écran primaire : {sw}x{sh}. Curseur d'origine : ({}, {}).",
        origin.x, origin.y
    );

    let targets = [(0.5, 0.5), (0.25, 0.25), (0.75, 0.6)];
    let mut all_ok = true;
    for (fx, fy) in targets {
        injector.mouse_move_abs(fx, fy, MonitorId(0))?;
        std::thread::sleep(std::time::Duration::from_millis(30));

        let mut p = POINT::default();
        // SAFETY : `p` est un POINT valide.
        unsafe { GetCursorPos(&mut p)? };

        let expected_x = (fx * f64::from(sw)).round() as i32;
        let expected_y = (fy * f64::from(sh)).round() as i32;
        let ex = (p.x - expected_x).abs();
        let ey = (p.y - expected_y).abs();
        let hit = ex <= 3 && ey <= 3;
        all_ok &= hit;
        println!(
            "cible ({fx:.2},{fy:.2}) -> attendu ({expected_x},{expected_y}), obtenu ({}, {}), écart ({ex},{ey}) {}",
            p.x, p.y, if hit { "OK" } else { "KO" }
        );
    }

    // Molette + une touche Unicode : sans fenêtre cible, on vérifie l'acceptation.
    injector.scroll(0.0, -1.0)?;
    injector.unicode('A')?;
    println!("Molette et saisie Unicode acceptées par le système.");

    // Restauration de la position d'origine.
    // SAFETY : coordonnées valides issues de GetCursorPos.
    unsafe { SetCursorPos(origin.x, origin.y)? };
    println!("Curseur restauré à sa position d'origine.");

    if all_ok {
        println!("OK : injection souris absolue vérifiée (le curseur suit les cibles).");
        Ok(())
    } else {
        Err("le curseur n'a pas atteint les cibles attendues".into())
    }
}

#[cfg(not(windows))]
fn main() {
    println!("input_probe : exemple spécifique à Windows (voir plan 07 pour macOS/Linux).");
}
