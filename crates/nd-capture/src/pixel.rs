//! Conversions de pixels **pures** (aucun appel système) partagées par les
//! backends Linux :
//!
//! * [`valeur_pixel`] / [`canal`] : décodage d'un pixel X11 ZPixmap selon
//!   l'ordre d'octets du serveur et les masques RGB du visual (voir
//!   `linux.rs::zpixmap_en_bgra`) ;
//! * [`convertit_bgra`] : normalisation d'une trame RGB 32 bits
//!   (BGRA/BGRx/RGBA/RGBx, stride source arbitraire) vers du **BGRA8 dense**
//!   (voir `linux_pipewire.rs::convert_to_bgra`).
//!
//! Ce module est compilé — et surtout **testé** — sur toutes les plateformes,
//! y compris Windows : les backends macOS/Linux ne compilant pas depuis le
//! poste de développement, c'est ici que leur logique portable est verrouillée
//! par les tests (voir `plan-technique/validation-macos-linux.md`).

/// Assemble la valeur d'un pixel ZPixmap (2 à 4 octets) selon l'ordre d'octets
/// du serveur X11 (`image_byte_order` du Setup).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn valeur_pixel(octets: &[u8], msb_first: bool) -> u32 {
    if msb_first {
        octets
            .iter()
            .fold(0u32, |acc, &o| (acc << 8) | u32::from(o))
    } else {
        octets
            .iter()
            .rev()
            .fold(0u32, |acc, &o| (acc << 8) | u32::from(o))
    }
}

/// Extrait un canal 8 bits via son masque contigu (canaux 8 bits en 24/32 bpp).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn canal(valeur: u32, masque: u32) -> u8 {
    if masque == 0 {
        return 0;
    }
    ((valeur & masque) >> masque.trailing_zeros()) as u8
}

/// Calcule le *stride* (octets par ligne) d'une image **ZPixmap** X11.
///
/// X11 arrondit chaque ligne (« scanline ») à un multiple de `scanline_pad_bits`
/// **bits** (8, 16 ou 32). Le calcul est mené en octets : il n'est exact que pour
/// des pixels dont la profondeur est un multiple de 8 (`bits_par_pixel` ∈ {8, 16,
/// 24, 32}) — précisément ce que le backend Linux accepte (24 ou 32, filtré en
/// amont dans `linux.rs::zpixmap_en_bgra`). `scanline_pad_bits` est borné à ≥ 8
/// (1 octet) pour ne jamais diviser par zéro sur un `Setup` aberrant.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn stride_zpixmap(largeur: u32, bits_par_pixel: usize, scanline_pad_bits: u8) -> usize {
    let octets_par_pixel = bits_par_pixel / 8;
    let pad = usize::from(scanline_pad_bits).max(8) / 8;
    (largeur as usize * octets_par_pixel).div_ceil(pad) * pad
}

/// Convertit une trame RGB 32 bits/pixel vers du **BGRA8 dense** (stride de
/// sortie = `4 · width`).
///
/// * `swap_rb` : permuter les canaux rouge et bleu (formats RGBA/RGBx) ;
/// * `has_alpha` : le format source porte un vrai canal alpha (sinon 255).
///
/// Garde-fou : une ligne source qui déborderait de `src` interrompt la copie —
/// les lignes restantes sortent noires opaques plutôt que de paniquer (le flux
/// PipeWire peut livrer un chunk plus court qu'annoncé).
#[cfg_attr(
    not(all(target_os = "linux", feature = "wayland-pipewire")),
    allow(dead_code)
)]
pub(crate) fn convertit_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
    swap_rb: bool,
    has_alpha: bool,
) -> Vec<u8> {
    let dst_stride = width * 4;
    let mut out = vec![0u8; dst_stride * height];

    for y in 0..height {
        let s_off = y * src_stride;
        let d_off = y * dst_stride;
        // Garde-fou : ne jamais déborder du buffer source (padding, tailles limites…).
        if s_off + dst_stride > src.len() {
            break;
        }
        let s = &src[s_off..s_off + dst_stride];
        let d = &mut out[d_off..d_off + dst_stride];

        if swap_rb {
            for x in 0..width {
                let p = x * 4;
                d[p] = s[p + 2]; // B <- R
                d[p + 1] = s[p + 1]; // G
                d[p + 2] = s[p]; // R <- B
                d[p + 3] = if has_alpha { s[p + 3] } else { 255 };
            }
        } else {
            // Ordre de canaux déjà BGRA : copie directe…
            d.copy_from_slice(s);
            // … puis on force l'opacité si le format n'a pas d'alpha (BGRx).
            if !has_alpha {
                for x in 0..width {
                    d[x * 4 + 3] = 255;
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'assemblage d'un pixel respecte l'ordre d'octets du serveur X11.
    #[test]
    fn valeur_pixel_selon_ordre_octets() {
        let octets = [0x11, 0x22, 0x33, 0x44];
        assert_eq!(valeur_pixel(&octets, false), 0x4433_2211);
        assert_eq!(valeur_pixel(&octets, true), 0x1122_3344);
        // Pixels de 3 octets (ZPixmap 24 bpp) : mêmes conventions.
        assert_eq!(valeur_pixel(&octets[..3], false), 0x0033_2211);
        assert_eq!(valeur_pixel(&octets[..3], true), 0x0011_2233);
    }

    /// L'extraction de canal gère les décalages et le masque nul.
    #[test]
    fn canal_extrait_selon_masque() {
        let v = 0x00a1_b2c3;
        assert_eq!(canal(v, 0x00ff_0000), 0xa1);
        assert_eq!(canal(v, 0x0000_ff00), 0xb2);
        assert_eq!(canal(v, 0x0000_00ff), 0xc3);
        assert_eq!(canal(v, 0), 0);
    }

    /// Stride ZPixmap 32 bpp : 4 octets/pixel, déjà aligné 32 bits → `4 · largeur`.
    #[test]
    fn stride_zpixmap_32bpp_dense() {
        assert_eq!(stride_zpixmap(1920, 32, 32), 7_680);
        assert_eq!(stride_zpixmap(1, 32, 32), 4);
        assert_eq!(stride_zpixmap(100, 32, 8), 400); // multiple de 1 octet aussi
    }

    /// Stride ZPixmap 24 bpp (3 octets/pixel) : arrondi selon `scanline_pad`.
    #[test]
    fn stride_zpixmap_24bpp_padding() {
        // Padding 32 bits (4 octets) : 10·3 = 30 → arrondi à 32 ; 4·3 = 12 déjà pair.
        assert_eq!(stride_zpixmap(10, 24, 32), 32);
        assert_eq!(stride_zpixmap(4, 24, 32), 12);
        // Padding 16 bits (2 octets) : 3·3 = 9 → arrondi à 10.
        assert_eq!(stride_zpixmap(3, 24, 16), 10);
        // Padding 8 bits (1 octet) : aucun arrondi.
        assert_eq!(stride_zpixmap(10, 24, 8), 30);
    }

    /// `scanline_pad` aberrant (0) borné à 8 bits : pas de division par zéro.
    #[test]
    fn stride_zpixmap_pad_zero_borne() {
        assert_eq!(stride_zpixmap(4, 32, 0), 16);
        assert_eq!(stride_zpixmap(0, 32, 0), 0);
    }

    /// BGRA (alpha réel, pas de permutation) : copie à l'identique.
    #[test]
    fn bgra_copie_identite() {
        // 2×1 : (B,G,R,A) = (1,2,3,4) puis (5,6,7,8).
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(convertit_bgra(&src, 2, 1, 8, false, true), src.to_vec());
    }

    /// BGRx (sans alpha) : copie + opacité forcée à 255.
    #[test]
    fn bgrx_force_alpha_opaque() {
        let src = [1u8, 2, 3, 0, 5, 6, 7, 0];
        assert_eq!(
            convertit_bgra(&src, 2, 1, 8, false, false),
            vec![1, 2, 3, 255, 5, 6, 7, 255]
        );
    }

    /// RGBA : permutation R↔B, alpha conservé.
    #[test]
    fn rgba_permute_rouge_bleu() {
        // Source (R,G,B,A) = (10,20,30,40) → BGRA = (30,20,10,40).
        let src = [10u8, 20, 30, 40];
        assert_eq!(
            convertit_bgra(&src, 1, 1, 4, true, true),
            vec![30, 20, 10, 40]
        );
    }

    /// RGBx : permutation R↔B + opacité forcée.
    #[test]
    fn rgbx_permute_et_force_alpha() {
        let src = [10u8, 20, 30, 0];
        assert_eq!(
            convertit_bgra(&src, 1, 1, 4, true, false),
            vec![30, 20, 10, 255]
        );
    }

    /// Le stride source (padding en fin de ligne) est ignoré : la sortie est dense.
    #[test]
    fn stride_source_avec_padding() {
        // 1 pixel/ligne, stride source de 8 octets (4 utiles + 4 de padding).
        let src = [
            1u8, 2, 3, 4, 0xEE, 0xEE, 0xEE, 0xEE, // ligne 0
            5, 6, 7, 8, 0xEE, 0xEE, 0xEE, 0xEE, // ligne 1
        ];
        assert_eq!(
            convertit_bgra(&src, 1, 2, 8, false, true),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    /// Source plus courte qu'annoncé : arrêt propre, lignes restantes à zéro
    /// (jamais de panique — chunk PipeWire tronqué).
    #[test]
    fn source_tronquee_sans_panique() {
        let src = [1u8, 2, 3, 4]; // une seule ligne fournie sur deux annoncées
        assert_eq!(
            convertit_bgra(&src, 1, 2, 4, false, true),
            vec![1, 2, 3, 4, 0, 0, 0, 0]
        );
    }

    /// Dimensions nulles : sortie vide, aucun débordement.
    #[test]
    fn dimensions_nulles() {
        assert!(convertit_bgra(&[], 0, 0, 0, false, true).is_empty());
        assert!(convertit_bgra(&[1, 2, 3, 4], 0, 5, 4, true, false).is_empty());
    }
}
