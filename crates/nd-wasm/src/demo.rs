//! `demo` — génération **pure** de l'image de test du mode démo (aucune dépendance
//! wasm, testable sur l'hôte).
//!
//! [`motif_rgba`] produit une image RGBA animée qui sert :
//! * de source visuelle au mode démo « motif » (peinte directement sur le canvas via
//!   `put_image_data` — chemin RGBA→canvas, sans codec) ;
//! * de **frame d'entrée** de la boucle encode→decode du mode démo « codec » : cette
//!   image est encodée en H.264 par le `VideoEncoder` du navigateur, puis redécodée et
//!   affichée — preuve du chemin decode→canvas sans aucune infrastructure.

/// Génère une image RGBA `largeur × hauteur` (4 octets/pixel, ordre R, G, B, A)
/// représentant un motif animé par `tick` : dégradés croisés + damier en mouvement.
///
/// Le contenu varie continûment avec `tick`, ce qui donne un flux non trivial à
/// encoder (utile pour exercer réellement le codec en mode démo) et un rendu
/// visiblement animé à l'écran.
///
/// # Panics
/// Jamais : les dimensions nulles produisent un tampon vide.
#[must_use]
pub fn motif_rgba(largeur: u32, hauteur: u32, tick: u32) -> Vec<u8> {
    let l = largeur as usize;
    let h = hauteur as usize;
    let mut pixels = vec![0u8; l.saturating_mul(h).saturating_mul(4)];
    if l == 0 || h == 0 {
        return pixels;
    }
    let t = tick as usize;
    // Décalage horizontal du damier (défilement), en cases de 16 px.
    let decalage = t / 4;
    for y in 0..h {
        for x in 0..l {
            let i = (y * l + x) * 4;
            let case = ((x / 16) + (y / 16) + decalage) % 2;
            // R : dégradé horizontal ; G : dégradé vertical ; B : damier animé.
            pixels[i] = ((x * 255) / (l - 1).max(1)) as u8;
            pixels[i + 1] = ((y * 255) / (h - 1).max(1)) as u8;
            pixels[i + 2] = if case == 0 { 40 } else { 210 };
            pixels[i + 3] = 255; // opaque
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taille_du_tampon_est_rgba() {
        let img = motif_rgba(8, 4, 0);
        assert_eq!(img.len(), 8 * 4 * 4);
        // Alpha systématiquement opaque.
        assert!(img.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn dimensions_nulles_donnent_tampon_vide() {
        assert!(motif_rgba(0, 10, 3).is_empty());
        assert!(motif_rgba(10, 0, 3).is_empty());
    }

    #[test]
    fn le_motif_est_anime() {
        // Deux ticks distincts (assez espacés pour décaler le damier) diffèrent.
        let a = motif_rgba(32, 32, 0);
        let b = motif_rgba(32, 32, 4);
        assert_ne!(a, b);
    }

    #[test]
    fn coins_du_degrade() {
        let img = motif_rgba(4, 4, 0);
        // Coin haut-gauche : R et G au minimum.
        assert_eq!(img[0], 0);
        assert_eq!(img[1], 0);
        // Coin bas-droite : R et G au maximum.
        let dernier = (4 * 4 - 1) * 4;
        assert_eq!(img[dernier], 255);
        assert_eq!(img[dernier + 1], 255);
    }
}
