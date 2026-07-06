//! Sonde du presse-papiers riche (plan 09) : aller-retour d'une image RGBA
//! générée via `CF_DIB` (`set_image` puis `get_image`, dimensions et pixels
//! comparés octet par octet) et lecture de la liste des fichiers copiés
//! (`CF_HDROP`), éventuellement vide. Le contenu précédent du presse-papiers
//! (texte ou image) est restauré en fin de sonde, par politesse.

type Res = std::result::Result<(), Box<dyn std::error::Error>>;

fn main() {
    match run() {
        Ok(()) => println!("\nverdict : OK"),
        Err(e) => {
            eprintln!("\nverdict : ECHEC — {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn run() -> Res {
    let clip = nd_files::open_clipboard()?;

    // --- Sauvegarde du contenu courant pour le restaurer en fin de sonde
    //     (best-effort : un format exotique non géré est simplement ignoré).
    let texte_avant = clip.get_text().ok().flatten();
    let image_avant = clip.get_image().ok().flatten();

    // --- 1. Liste des fichiers copiés, lue AVANT d'écraser le presse-papiers.
    let fichiers = clip.get_files()?;
    if fichiers.is_empty() {
        println!("fichiers copiés : aucun (pas de CF_HDROP dans le presse-papiers)");
    } else {
        println!(
            "fichiers copiés : {} chemin(s) via CF_HDROP",
            fichiers.len()
        );
        for chemin in &fichiers {
            println!("  - {}", chemin.display());
        }
    }

    // --- 2. Aller-retour d'une petite image RGBA générée (CF_DIB).
    let image = image_test(31, 17);
    clip.set_image(&image)?;
    let relue = clip.get_image()?;

    // --- Restauration avant le verdict (le probe ne doit rien laisser traîner).
    if let Some(texte) = &texte_avant {
        let _ = clip.set_text(texte);
    } else if let Some(ancienne) = &image_avant {
        let _ = clip.set_image(ancienne);
    }

    let relue = relue.ok_or("presse-papiers sans image juste après set_image")?;
    if (relue.width, relue.height) != (image.width, image.height) {
        return Err(format!(
            "dimensions relues {}x{} != {}x{} attendues",
            relue.width, relue.height, image.width, image.height
        )
        .into());
    }
    if relue.rgba != image.rgba {
        let differents = relue
            .rgba
            .iter()
            .zip(&image.rgba)
            .filter(|(a, b)| a != b)
            .count();
        return Err(format!(
            "pixels relus différents ({differents} octets sur {})",
            image.rgba.len()
        )
        .into());
    }
    println!(
        "image : aller-retour set_image/get_image OK ({}x{}, {} octets RGBA identiques)",
        image.width,
        image.height,
        image.rgba.len()
    );
    Ok(())
}

/// Petite image de test déterministe : dégradés distincts par canal et alpha
/// variable jamais nul (pour vérifier qu'il traverse le DIB sans altération).
#[cfg(windows)]
fn image_test(width: u32, height: u32) -> nd_files::ImageRgba {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[
                (x * 8 % 256) as u8,
                (y * 16 % 256) as u8,
                ((x + y) * 5 % 256) as u8,
                200 + (x % 55) as u8,
            ]);
        }
    }
    nd_files::ImageRgba {
        width,
        height,
        rgba,
    }
}

#[cfg(not(windows))]
fn run() -> Res {
    println!("presse-papiers riche : ignoré (implémentation Windows uniquement à ce stade)");
    Ok(())
}
