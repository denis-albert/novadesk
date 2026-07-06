//! Outils de mesure de qualité vidéo pour le banc de test — plan 14.
//!
//! Objectif : comparer une image **décodée** ([`crate::DecodedFrame`], pixels RGBA)
//! à l'image **originale** capturée, afin de chiffrer la perte introduite par la
//! chaîne encodeur → décodeur (plan 03) :
//!
//! - [`mse_rgba`] / [`psnr_rgba`] : erreur quadratique moyenne et PSNR (dB) sur les
//!   octets RGBA ; variantes [`psnr_par_canal_rgba`] et [`psnr_luma`].
//! - [`ssim_luma`] : SSIM **simplifié** sur la luminance (approximation, voir la
//!   documentation de la fonction).
//! - [`write_y4m`] : export d'une suite de frames RGBA en flux Y4M (YUV4MPEG2),
//!   pour inspection visuelle avec un lecteur standard (ffplay, mpv…).
//!
//! Module 100 % portable : aucune FFI, aucune dépendance nouvelle, indépendant de
//! l'OS — utilisable tel quel dans les tests d'intégration de n'importe quelle
//! plateforme.

use std::io::{self, Write};

use nd_proto::{NdError, Result};

// ---------------------------------------------------------------------------
// 1. MSE / PSNR
// ---------------------------------------------------------------------------

/// Valeur maximale d'un échantillon 8 bits (dynamique du signal pour le PSNR).
const MAX_ECHANTILLON: f64 = 255.0;

/// Vérifie que `a` et `b` sont deux tampons RGBA comparables : même taille, non
/// vides, et multiples de 4 octets (R, G, B, A). Erreur [`NdError::Codec`] sinon.
fn verifier_rgba(a: &[u8], b: &[u8]) -> Result<()> {
    if a.len() != b.len() {
        return Err(NdError::Codec(format!(
            "metrics : tailles incohérentes ({} octets contre {})",
            a.len(),
            b.len()
        )));
    }
    if a.is_empty() {
        return Err(NdError::Codec("metrics : tampon RGBA vide".into()));
    }
    if !a.len().is_multiple_of(4) {
        return Err(NdError::Codec(format!(
            "metrics : taille non multiple de 4 ({} octets) — RGBA attendu",
            a.len()
        )));
    }
    Ok(())
}

/// PSNR (dB) déduit d'une MSE : `+∞` si la MSE est nulle (images identiques).
fn psnr_depuis_mse(mse: f64) -> f64 {
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (MAX_ECHANTILLON * MAX_ECHANTILLON / mse).log10()
    }
}

/// Erreur quadratique moyenne (MSE) entre deux tampons RGBA, sur **tous** les
/// octets (canal alpha compris).
///
/// Erreur [`NdError::Codec`] si les tailles diffèrent, sont vides ou ne sont pas
/// des multiples de 4.
pub fn mse_rgba(a: &[u8], b: &[u8]) -> Result<f64> {
    verifier_rgba(a, b)?;
    let somme: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = f64::from(x) - f64::from(y);
            d * d
        })
        .sum();
    Ok(somme / a.len() as f64)
}

/// PSNR (en dB) entre deux tampons RGBA — `+∞` si les images sont identiques.
///
/// Repères usuels : > 40 dB ≈ dégradation invisible, 30–40 dB ≈ bonne qualité,
/// < 30 dB ≈ artefacts visibles. Mêmes vérifications de tailles que [`mse_rgba`].
pub fn psnr_rgba(a: &[u8], b: &[u8]) -> Result<f64> {
    Ok(psnr_depuis_mse(mse_rgba(a, b)?))
}

/// PSNR (dB) par canal, dans l'ordre `[R, G, B, A]` — `+∞` pour un canal identique.
///
/// Utile pour repérer une dérive de chrominance que le PSNR global moyenne (la
/// conversion RGB→YUV 4:2:0 du codec dégrade la couleur plus que la luminance).
pub fn psnr_par_canal_rgba(a: &[u8], b: &[u8]) -> Result<[f64; 4]> {
    verifier_rgba(a, b)?;
    let mut sommes = [0.0f64; 4];
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for canal in 0..4 {
            let d = f64::from(pa[canal]) - f64::from(pb[canal]);
            sommes[canal] += d * d;
        }
    }
    let nb_pixels = (a.len() / 4) as f64;
    Ok(sommes.map(|s| psnr_depuis_mse(s / nb_pixels)))
}

/// Luminance (BT.601, pleine échelle 0..255) d'un pixel RGB.
fn luma_bt601(r: u8, g: u8, b: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

/// Plan de luminance (un `f64` par pixel) d'un tampon RGBA.
fn plan_luma(rgba: &[u8]) -> Vec<f64> {
    rgba.chunks_exact(4)
        .map(|p| luma_bt601(p[0], p[1], p[2]))
        .collect()
}

/// PSNR (dB) calculé sur la **luminance** seule (BT.601) — `+∞` si identiques.
///
/// C'est la variante la plus proche de l'œil : les codecs sous-échantillonnent la
/// chrominance (4:2:0), la luminance porte l'essentiel de la netteté du texte.
pub fn psnr_luma(a: &[u8], b: &[u8]) -> Result<f64> {
    verifier_rgba(a, b)?;
    let (la, lb) = (plan_luma(a), plan_luma(b));
    let somme: f64 = la
        .iter()
        .zip(&lb)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum();
    Ok(psnr_depuis_mse(somme / la.len() as f64))
}

// ---------------------------------------------------------------------------
// 2. SSIM simplifié (luminance, blocs 8x8)
// ---------------------------------------------------------------------------

/// Côté des blocs sur lesquels le SSIM est moyenné.
const COTE_BLOC: usize = 8;

/// Constantes de stabilisation du SSIM : `C1 = (K1·L)²`, `C2 = (K2·L)²` avec
/// `K1 = 0,01`, `K2 = 0,03` et `L = 255` (valeurs canoniques de Wang et al. 2004).
const SSIM_C1: f64 = (0.01 * MAX_ECHANTILLON) * (0.01 * MAX_ECHANTILLON);
const SSIM_C2: f64 = (0.03 * MAX_ECHANTILLON) * (0.03 * MAX_ECHANTILLON);

/// SSIM **simplifié** entre deux images RGBA, calculé sur la luminance (BT.601).
///
/// ⚠ **Approximation** du SSIM de référence (Wang et al. 2004), suffisante pour le
/// banc de test (comparer deux réglages d'encodeur, détecter une régression) mais
/// non comparable chiffre à chiffre aux implémentations canoniques :
///
/// - moyennes/variances sur des blocs **8x8 disjoints** (pas de fenêtre gaussienne
///   11x11 glissante) ; les blocs de bord, tronqués, sont inclus ;
/// - la carte de similarité est moyennée uniformément sur les blocs.
///
/// `width`/`height` en pixels ; `a` et `b` doivent faire `width × height × 4`
/// octets (RGBA), sinon erreur [`NdError::Codec`]. Renvoie une valeur dans `0..=1`
/// (1 = images identiques ; le résultat, théoriquement dans `-1..=1`, est ramené à
/// `0..=1` par troncature).
pub fn ssim_luma(a: &[u8], b: &[u8], width: u32, height: u32) -> Result<f64> {
    verifier_rgba(a, b)?;
    let (w, h) = (width as usize, height as usize);
    let attendu = w
        .checked_mul(h)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n > 0)
        .ok_or_else(|| {
            NdError::Codec(format!(
                "metrics::ssim_luma : dimensions invalides {width}x{height}"
            ))
        })?;
    if a.len() != attendu {
        return Err(NdError::Codec(format!(
            "metrics::ssim_luma : {} octets au lieu de {attendu} pour {width}x{height} RGBA",
            a.len()
        )));
    }

    let (la, lb) = (plan_luma(a), plan_luma(b));
    let mut somme_ssim = 0.0f64;
    let mut nb_blocs = 0u64;

    for by in (0..h).step_by(COTE_BLOC) {
        for bx in (0..w).step_by(COTE_BLOC) {
            let haut_bloc = COTE_BLOC.min(h - by);
            let large_bloc = COTE_BLOC.min(w - bx);
            let n = (haut_bloc * large_bloc) as f64;

            // Passe 1 : moyennes du bloc.
            let (mut sx, mut sy) = (0.0f64, 0.0f64);
            for dy in 0..haut_bloc {
                let ligne = (by + dy) * w + bx;
                for dx in 0..large_bloc {
                    sx += la[ligne + dx];
                    sy += lb[ligne + dx];
                }
            }
            let (mx, my) = (sx / n, sy / n);

            // Passe 2 : variances et covariance (population) du bloc.
            let (mut vx, mut vy, mut cov) = (0.0f64, 0.0f64, 0.0f64);
            for dy in 0..haut_bloc {
                let ligne = (by + dy) * w + bx;
                for dx in 0..large_bloc {
                    let ex = la[ligne + dx] - mx;
                    let ey = lb[ligne + dx] - my;
                    vx += ex * ex;
                    vy += ey * ey;
                    cov += ex * ey;
                }
            }
            let (vx, vy, cov) = (vx / n, vy / n, cov / n);

            somme_ssim += ((2.0 * mx * my + SSIM_C1) * (2.0 * cov + SSIM_C2))
                / ((mx * mx + my * my + SSIM_C1) * (vx + vy + SSIM_C2));
            nb_blocs += 1;
        }
    }

    Ok((somme_ssim / nb_blocs as f64).clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------
// 3. Export Y4M (débogage visuel)
// ---------------------------------------------------------------------------

/// Convertit une frame RGBA en trois plans YUV 4:2:0 (conversion BT.601 pleine
/// échelle « JPEG », chrominance moyennée par blocs 2x2 — d'où l'étiquette
/// `C420jpeg` de l'en-tête Y4M).
fn rgba_vers_yuv420(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut plan_y = vec![0u8; w * h];
    let (wc, hc) = (w / 2, h / 2);
    let mut plan_u = vec![0u8; wc * hc];
    let mut plan_v = vec![0u8; wc * hc];

    // Luminance : un échantillon par pixel.
    for (pixel, y) in rgba.chunks_exact(4).zip(plan_y.iter_mut()) {
        *y = luma_bt601(pixel[0], pixel[1], pixel[2])
            .round()
            .clamp(0.0, 255.0) as u8;
    }

    // Chrominance : U/V par pixel, moyennés sur chaque bloc 2x2.
    for by in 0..hc {
        for bx in 0..wc {
            let (mut su, mut sv) = (0.0f64, 0.0f64);
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((by * 2 + dy) * w + bx * 2 + dx) * 4;
                    let (r, g, b) = (
                        f64::from(rgba[i]),
                        f64::from(rgba[i + 1]),
                        f64::from(rgba[i + 2]),
                    );
                    su += 128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b;
                    sv += 128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b;
                }
            }
            plan_u[by * wc + bx] = (su / 4.0).round().clamp(0.0, 255.0) as u8;
            plan_v[by * wc + bx] = (sv / 4.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    (plan_y, plan_u, plan_v)
}

/// Écrit `frames_rgba` (une suite de frames RGBA de `width × height` pixels) sous
/// forme de flux **Y4M** (YUV4MPEG2, 4:2:0), à `fps` images/seconde.
///
/// Outil de **débogage** : le fichier produit s'ouvre tel quel dans ffplay/mpv/VLC
/// (`ffplay sortie.y4m`) pour inspecter visuellement un flux décodé. Le canal
/// alpha est ignoré (Y4M ne le transporte pas).
///
/// Contraintes (sinon `io::Error` de type [`io::ErrorKind::InvalidInput`]) :
/// `width` et `height` **pairs** et non nuls (sous-échantillonnage 4:2:0), `fps`
/// non nul, et chaque frame de exactement `width × height × 4` octets.
pub fn write_y4m<W: Write>(
    sortie: &mut W,
    frames_rgba: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> io::Result<()> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write_y4m : dimensions paires et non nulles requises pour du 4:2:0 ({width}x{height})"),
        ));
    }
    if fps == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_y4m : fps nul",
        ));
    }
    let (w, h) = (width as usize, height as usize);
    let taille_attendue = w * h * 4;

    // En-tête du flux : `Ip` = progressif, `A1:1` = pixels carrés, `C420jpeg` =
    // 4:2:0 avec chrominance moyennée 2x2 (voir `rgba_vers_yuv420`).
    writeln!(
        sortie,
        "YUV4MPEG2 W{width} H{height} F{fps}:1 Ip A1:1 C420jpeg"
    )?;

    for (i, frame) in frames_rgba.iter().enumerate() {
        if frame.len() != taille_attendue {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_y4m : frame {i} de {} octets au lieu de {taille_attendue} ({width}x{height} RGBA)",
                    frame.len()
                ),
            ));
        }
        let (plan_y, plan_u, plan_v) = rgba_vers_yuv420(frame, w, h);
        sortie.write_all(b"FRAME\n")?;
        sortie.write_all(&plan_y)?;
        sortie.write_all(&plan_u)?;
        sortie.write_all(&plan_v)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Générateur pseudo-aléatoire déterministe (xorshift32) — pas de dépendance.
    fn xorshift32(etat: &mut u32) -> u32 {
        *etat ^= *etat << 13;
        *etat ^= *etat >> 17;
        *etat ^= *etat << 5;
        *etat
    }

    /// Image RGBA de test avec du relief (dégradé + damier), `w × h` pixels.
    fn image_test(w: usize, h: usize) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let damier = if (x / 4 + y / 4) % 2 == 0 { 40 } else { 0 };
                rgba.push(((x * 255) / w.max(1)) as u8); // R : dégradé horizontal
                rgba.push(((y * 255) / h.max(1)) as u8); // G : dégradé vertical
                rgba.push((128 + damier) as u8); // B : damier
                rgba.push(255); // A opaque
            }
        }
        rgba
    }

    /// Copie de `base` bruitée : chaque octet R/G/B décalé de ±`amplitude` au plus
    /// (alpha préservé), de façon déterministe.
    fn bruiter(base: &[u8], amplitude: i32, graine: u32) -> Vec<u8> {
        let mut etat = graine;
        base.iter()
            .enumerate()
            .map(|(i, &octet)| {
                if i % 4 == 3 {
                    return octet; // alpha intact
                }
                let plage = 2 * amplitude + 1;
                let delta = (xorshift32(&mut etat) % plage as u32) as i32 - amplitude;
                (i32::from(octet) + delta).clamp(0, 255) as u8
            })
            .collect()
    }

    // ---- MSE / PSNR ----

    #[test]
    fn mse_delta_connu() {
        // Delta uniforme de 2 sur chaque octet → MSE = 2² = 4, exactement.
        let a = vec![10u8; 64];
        let b = vec![12u8; 64];
        assert_eq!(mse_rgba(&a, &b).unwrap(), 4.0);
        // MSE d'images identiques = 0.
        assert_eq!(mse_rgba(&a, &a).unwrap(), 0.0);
    }

    #[test]
    fn psnr_identiques_infini() {
        let img = image_test(16, 16);
        assert_eq!(psnr_rgba(&img, &img).unwrap(), f64::INFINITY);
        assert_eq!(psnr_luma(&img, &img).unwrap(), f64::INFINITY);
    }

    #[test]
    fn psnr_valeur_connue() {
        // MSE = 4 → PSNR = 10·log10(255²/4) ≈ 42,1103 dB.
        let a = vec![10u8; 64];
        let b = vec![12u8; 64];
        let attendu = 10.0 * (255.0f64 * 255.0 / 4.0).log10();
        assert!((psnr_rgba(&a, &b).unwrap() - attendu).abs() < 1e-12);
    }

    #[test]
    fn psnr_decroit_avec_le_bruit() {
        let base = image_test(32, 32);
        let peu_bruitee = bruiter(&base, 2, 0xDEAD_BEEF);
        let tres_bruitee = bruiter(&base, 40, 0xDEAD_BEEF);
        let psnr_faible_bruit = psnr_rgba(&base, &peu_bruitee).unwrap();
        let psnr_fort_bruit = psnr_rgba(&base, &tres_bruitee).unwrap();
        assert!(psnr_faible_bruit.is_finite());
        assert!(
            psnr_faible_bruit > psnr_fort_bruit,
            "PSNR doit décroître avec le bruit ({psnr_faible_bruit} dB contre {psnr_fort_bruit} dB)"
        );
    }

    #[test]
    fn psnr_par_canal_isole_le_canal_touche() {
        let a = image_test(8, 8);
        // Ne perturbe que le canal R.
        let mut b = a.clone();
        for pixel in b.chunks_exact_mut(4) {
            pixel[0] = pixel[0].wrapping_add(8);
        }
        let [r, g, bb, alpha] = psnr_par_canal_rgba(&a, &b).unwrap();
        assert!(r.is_finite());
        assert_eq!(g, f64::INFINITY);
        assert_eq!(bb, f64::INFINITY);
        assert_eq!(alpha, f64::INFINITY);
    }

    #[test]
    fn tailles_incoherentes_refusees() {
        let a = vec![0u8; 64];
        let b = vec![0u8; 60];
        assert!(matches!(mse_rgba(&a, &b), Err(NdError::Codec(_))));
        assert!(matches!(psnr_rgba(&a, &b), Err(NdError::Codec(_))));
        assert!(matches!(psnr_luma(&a, &b), Err(NdError::Codec(_))));
        assert!(matches!(
            psnr_par_canal_rgba(&a, &b),
            Err(NdError::Codec(_))
        ));
        // Vide et non multiple de 4 : refusés aussi.
        assert!(mse_rgba(&[], &[]).is_err());
        assert!(mse_rgba(&[1, 2, 3], &[1, 2, 3]).is_err());
    }

    // ---- SSIM ----

    #[test]
    fn ssim_identiques_proche_de_un() {
        let img = image_test(32, 24);
        let s = ssim_luma(&img, &img, 32, 24).unwrap();
        assert!(s > 0.9999, "SSIM d'images identiques ≈ 1 (obtenu : {s})");
        assert!(s <= 1.0);
    }

    #[test]
    fn ssim_decroit_avec_le_bruit() {
        let base = image_test(32, 32);
        let peu_bruitee = bruiter(&base, 4, 42);
        let tres_bruitee = bruiter(&base, 60, 42);
        let s_faible = ssim_luma(&base, &peu_bruitee, 32, 32).unwrap();
        let s_fort = ssim_luma(&base, &tres_bruitee, 32, 32).unwrap();
        assert!(s_faible < 1.0);
        assert!(
            s_faible > s_fort,
            "le SSIM doit décroître avec le bruit ({s_faible} contre {s_fort})"
        );
        assert!((0.0..=1.0).contains(&s_faible));
        assert!((0.0..=1.0).contains(&s_fort));
    }

    #[test]
    fn ssim_dimensions_partielles_supportees() {
        // 13x9 : blocs de bord tronqués (largeur/hauteur non multiples de 8).
        let img = image_test(13, 9);
        let s = ssim_luma(&img, &img, 13, 9).unwrap();
        assert!(s > 0.9999);
    }

    #[test]
    fn ssim_tailles_incoherentes_refusees() {
        let img = image_test(16, 16);
        // Tampon incompatible avec les dimensions annoncées.
        assert!(matches!(
            ssim_luma(&img, &img, 16, 8),
            Err(NdError::Codec(_))
        ));
        // Dimensions nulles.
        assert!(ssim_luma(&img, &img, 0, 0).is_err());
    }

    // ---- Y4M ----

    #[test]
    fn y4m_entete_et_taille() {
        let (w, h, n) = (8u32, 6u32, 3usize);
        let frames = vec![image_test(w as usize, h as usize); n];
        let mut sortie = Vec::new();
        write_y4m(&mut sortie, &frames, w, h, 30).unwrap();

        // En-tête `YUV4MPEG2` valide, terminé par '\n'.
        let entete_attendu = b"YUV4MPEG2 W8 H6 F30:1 Ip A1:1 C420jpeg\n";
        assert!(
            sortie.starts_with(entete_attendu),
            "en-tête Y4M inattendu : {:?}",
            String::from_utf8_lossy(&sortie[..entete_attendu.len().min(sortie.len())])
        );

        // Taille exacte : en-tête + N × (\"FRAME\\n\" + 1,5 octet/pixel en 4:2:0).
        let taille_frame = 6 + (w * h + 2 * (w / 2) * (h / 2)) as usize;
        assert_eq!(sortie.len(), entete_attendu.len() + n * taille_frame);

        // Chaque frame est bien annoncée par un marqueur `FRAME\n`.
        let nb_marqueurs = sortie.windows(6).filter(|f| f == b"FRAME\n").count();
        assert_eq!(nb_marqueurs, n);
    }

    #[test]
    fn y4m_conversion_blanc_et_noir() {
        // Blanc pur : Y = 255, U = V = 128 ; noir pur : Y = 0, U = V = 128.
        let (w, h) = (2u32, 2u32);
        let blanc = vec![255u8; 16];
        let noir = {
            let mut f = vec![0u8; 16];
            for pixel in f.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            f
        };
        let mut sortie = Vec::new();
        write_y4m(&mut sortie, &[blanc, noir], w, h, 1).unwrap();
        let entete = sortie.iter().position(|&o| o == b'\n').unwrap() + 1;
        // Frame 1 (blanche) : marqueur (6) + Y (4 octets) + U (1) + V (1).
        let y0 = entete + 6;
        assert_eq!(&sortie[y0..y0 + 4], &[255, 255, 255, 255]);
        assert_eq!(sortie[y0 + 4], 128); // U
        assert_eq!(sortie[y0 + 5], 128); // V
                                         // Frame 2 (noire).
        let y1 = y0 + 6 + 6;
        assert_eq!(&sortie[y1..y1 + 4], &[0, 0, 0, 0]);
        assert_eq!(sortie[y1 + 4], 128);
        assert_eq!(sortie[y1 + 5], 128);
    }

    #[test]
    fn y4m_entrees_invalides_refusees() {
        let mut sortie = Vec::new();
        let frame_ok = image_test(4, 4);

        // Dimensions impaires (4:2:0 impossible).
        let err = write_y4m(&mut sortie, &[image_test(3, 4)], 3, 4, 30).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // fps nul.
        let err = write_y4m(&mut sortie, std::slice::from_ref(&frame_ok), 4, 4, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Frame de mauvaise taille.
        let err = write_y4m(&mut sortie, &[vec![0u8; 10]], 4, 4, 30).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Cas nominal sans frame : en-tête seul, pas d'erreur.
        let mut vide = Vec::new();
        write_y4m(&mut vide, &[], 4, 4, 30).unwrap();
        assert!(vide.starts_with(b"YUV4MPEG2 "));
    }
}
