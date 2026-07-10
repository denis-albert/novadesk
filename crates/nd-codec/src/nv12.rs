//! Aides **NV12** côté viewer (plan « zéro-copie D3D11 », voir plan 03/10).
//!
//! Le chemin d'affichage GPU (nd-ffi, texture D3D11 partagée) préfère recevoir
//! les images décodées en **NV12** (1,5 octet/pixel) plutôt qu'en RGBA
//! (4 octets/pixel) : le téléversement CPU→GPU est 2,7× plus petit et la
//! conversion couleur YUV→RGB est faite **sur GPU** (`ID3D11VideoProcessor`),
//! plus du tout au CPU. Ce module fournit les deux briques :
//!
//! * [`i420_vers_nv12`] — re-empaquetage des plans I420 du décodeur (openh264)
//!   vers un tampon NV12 contigu (plan Y puis plan UV entrelacé). C'est une
//!   copie mémoire pure (aucune arithmétique couleur) ;
//! * [`nv12_vers_rgba`] — conversion **CPU de repli** NV12 → RGBA, BT.601
//!   **pleine plage** (mêmes conventions que `write_rgba8` d'openh264 et que la
//!   conversion aller de `software.rs`) : utilisée quand une image NV12 doit
//!   malgré tout être affichée par le chemin CPU historique (aucune texture
//!   GPU attachée, `VideoProcessor` indisponible, relecture d'enregistrement…).

/// Re-empaquette des plans I420 (Y, U, V séparés, avec leurs strides) en un
/// tampon **NV12** contigu : plan Y (`l×h` octets, sans stride) suivi du plan
/// UV entrelacé (`l×h/2` octets, U puis V par paire). `l` et `h` doivent être
/// pairs (contrat H.264 de ce workspace). Copie pure, aucune conversion.
pub fn i420_vers_nv12(
    plan_y: &[u8],
    plan_u: &[u8],
    plan_v: &[u8],
    strides: (usize, usize, usize),
    l: usize,
    h: usize,
) -> Vec<u8> {
    let (stride_y, stride_u, stride_v) = strides;
    let mut nv12 = vec![0u8; l * h + l * h / 2];
    let (dst_y, dst_uv) = nv12.split_at_mut(l * h);

    // Plan Y : recopie ligne à ligne (le stride source peut dépasser `l`).
    for (y, ligne) in dst_y.chunks_exact_mut(l).enumerate() {
        ligne.copy_from_slice(&plan_y[y * stride_y..y * stride_y + l]);
    }
    // Plan UV : entrelace U et V (un couple par bloc 2×2).
    let (lc, hc) = (l / 2, h / 2);
    for by in 0..hc {
        let src_u = &plan_u[by * stride_u..by * stride_u + lc];
        let src_v = &plan_v[by * stride_v..by * stride_v + lc];
        let dst = &mut dst_uv[by * l..(by + 1) * l];
        for ((paire, u), v) in dst.chunks_exact_mut(2).zip(src_u).zip(src_v) {
            paire[0] = *u;
            paire[1] = *v;
        }
    }
    nv12
}

/// Conversion **CPU de repli** NV12 → RGBA, BT.601 **pleine plage**
/// (coefficients entiers ×256 : R = Y + 1,402 (V−128) ; G = Y − 0,344 (U−128)
/// − 0,714 (V−128) ; B = Y + 1,772 (U−128)). Alpha opaque. Renvoie `None` si le
/// tampon est plus petit que `l×h×3/2` ou si les dimensions sont impaires.
pub fn nv12_vers_rgba(nv12: &[u8], largeur: u32, hauteur: u32) -> Option<Vec<u8>> {
    let (l, h) = (largeur as usize, hauteur as usize);
    if l == 0 || h == 0 || l % 2 != 0 || h % 2 != 0 || nv12.len() < l * h + l * h / 2 {
        return None;
    }
    let (plan_y, plan_uv) = nv12.split_at(l * h);
    let mut rgba = vec![255u8; l * h * 4];
    for y in 0..h {
        let ligne_uv = &plan_uv[(y / 2) * l..(y / 2 + 1) * l];
        for x in 0..l {
            let yy = i32::from(plan_y[y * l + x]);
            let u = i32::from(ligne_uv[(x / 2) * 2]) - 128;
            let v = i32::from(ligne_uv[(x / 2) * 2 + 1]) - 128;
            let o = (y * l + x) * 4;
            rgba[o] = (yy + ((359 * v) >> 8)).clamp(0, 255) as u8;
            rgba[o + 1] = (yy - ((88 * u + 183 * v) >> 8)).clamp(0, 255) as u8;
            rgba[o + 2] = (yy + ((454 * u) >> 8)).clamp(0, 255) as u8;
        }
    }
    Some(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'entrelacement I420 → NV12 place chaque échantillon au bon endroit,
    /// strides sources plus larges que l'image compris.
    #[test]
    fn i420_vers_nv12_entrelace_fidele() {
        let (l, h) = (4usize, 2usize);
        // Strides volontairement plus larges (marges à ignorer).
        let plan_y = [1, 2, 3, 4, 99, 99, /* ligne 2 */ 5, 6, 7, 8, 99, 99];
        let plan_u = [10, 11, 99];
        let plan_v = [20, 21, 99];
        let nv12 = i420_vers_nv12(&plan_y, &plan_u, &plan_v, (6, 3, 3), l, h);
        assert_eq!(&nv12[..8], &[1, 2, 3, 4, 5, 6, 7, 8], "plan Y");
        assert_eq!(&nv12[8..], &[10, 20, 11, 21], "plan UV entrelacé");
    }

    /// La conversion de repli produit les aplats attendus (BT.601 pleine
    /// plage) : gris neutre, noir, blanc.
    #[test]
    fn nv12_vers_rgba_aplats() {
        let cas = [
            (128u8, [128u8, 128, 128]), // gris neutre
            (0, [0, 0, 0]),             // noir
            (255, [255, 255, 255]),     // blanc
        ];
        for (y, attendu) in cas {
            let (l, h) = (4u32, 2u32);
            let mut nv12 = vec![y; 8];
            nv12.extend_from_slice(&[128; 4]); // chroma neutre
            let rgba = nv12_vers_rgba(&nv12, l, h).expect("conversion");
            assert_eq!(rgba.len(), 4 * 2 * 4);
            for px in rgba.chunks_exact(4) {
                assert_eq!(&px[..3], &attendu, "Y = {y}");
                assert_eq!(px[3], 255, "alpha opaque");
            }
        }
    }

    /// Tampon trop court ou dimensions impaires : refus propre (`None`).
    #[test]
    fn nv12_vers_rgba_refuse_entrees_invalides() {
        assert!(nv12_vers_rgba(&[0; 5], 4, 2).is_none(), "tampon trop court");
        assert!(nv12_vers_rgba(&[0; 24], 3, 2).is_none(), "largeur impaire");
        assert!(nv12_vers_rgba(&[], 0, 0).is_none(), "dimensions nulles");
    }

    /// Aller-retour avec la conversion aller de `software.rs` (BGRA → I420
    /// pleine plage) : re-empaquetage NV12 puis retour RGBA ≈ couleur d'origine.
    #[test]
    fn aller_retour_couleur_coherent() {
        // Aplat orangé : B=32, G=128, R=224 (BGRA) → attendu RGBA (224,128,32).
        let (l, h) = (8usize, 8usize);
        // Conversion aller (mêmes coefficients que software.rs, pleine plage).
        let (b, g, r) = (32i32, 128i32, 224i32);
        let y = ((77 * r + 150 * g + 29 * b + 128) >> 8) as u8;
        let u = ((((-43 * r - 85 * g + 128 * b) + 128) >> 8) + 128).clamp(0, 255) as u8;
        let v = ((((128 * r - 107 * g - 21 * b) + 128) >> 8) + 128).clamp(0, 255) as u8;
        let mut nv12 = vec![y; l * h];
        for _ in 0..l * h / 4 {
            nv12.push(u);
            nv12.push(v);
        }
        let rgba = nv12_vers_rgba(&nv12, l as u32, h as u32).expect("conversion");
        for px in rgba.chunks_exact(4) {
            assert!((i32::from(px[0]) - r).abs() <= 4, "R : {} ≉ {r}", px[0]);
            assert!((i32::from(px[1]) - g).abs() <= 4, "G : {} ≉ {g}", px[1]);
            assert!((i32::from(px[2]) - b).abs() <= 4, "B : {} ≉ {b}", px[2]);
        }
    }
}
