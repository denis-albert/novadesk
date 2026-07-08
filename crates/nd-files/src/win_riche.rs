//! Presse-papiers Windows « riche » (plan 09) : images bitmap via `CF_DIB`
//! et listes de fichiers copiés via `CF_HDROP`.
//!
//! Complète le module [`win`](crate::win) (texte `CF_UNICODETEXT`) en
//! réutilisant sa garde RAII [`OpenedClipboard`] et ses aides de blocs
//! globaux ([`poser_bloc`]/[`lire_bloc`]). Le `unsafe` FFI reste confiné à
//! ces deux modules ; les conversions DIB ↔ RGBA sont des fonctions pures,
//! testées unitairement sans toucher au presse-papiers.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use nd_proto::{NdError, Result};
use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_BITFIELDS, BI_RGB};
use windows::Win32::System::DataExchange::{
    EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use crate::win::{clip_err, lire_bloc, poser_bloc, OpenedClipboard};
use crate::ImageRgba;

/// Format « DIB » : `BITMAPINFOHEADER`, table de couleurs/masques éventuels,
/// puis pixels (valeur Win32 `CF_DIB`). Déclarée localement pour ne pas
/// activer la feature `Win32_System_Ole` qui héberge ces constantes.
const CF_DIB: u32 = 8;
/// Format « liste de fichiers déposés » (valeur Win32 `CF_HDROP`), même remarque.
const CF_HDROP: u32 = 15;

/// Taille du `BITMAPINFOHEADER` classique (40 octets).
const TAILLE_EN_TETE: usize = std::mem::size_of::<BITMAPINFOHEADER>();

/// Erreur d'encodage/décodage DIB en [`NdError::Io`] contextualisée
/// (même convention que `clip_err` du module `win`).
fn dib_err(ctx: impl std::fmt::Display) -> NdError {
    NdError::Io(std::io::Error::other(format!("presse-papiers DIB : {ctx}")))
}

// ---------------------------------------------------------------------------
// Opérations presse-papiers (FFI)
// ---------------------------------------------------------------------------

/// Lit l'image du presse-papiers (`CF_DIB`), convertie en RGBA top-down.
/// Renvoie `Ok(None)` si le presse-papiers ne contient pas d'image.
pub(crate) fn get_image() -> Result<Option<ImageRgba>> {
    let _ouvert = OpenedClipboard::open()?;

    // SAFETY : simple interrogation de disponibilité d'un format.
    if unsafe { IsClipboardFormatAvailable(CF_DIB) }.is_err() {
        return Ok(None);
    }
    let dib = lire_bloc(CF_DIB)?;
    dib_vers_rgba(&dib).map(Some)
}

/// Place une image RGBA dans le presse-papiers sous forme de DIB 32 bits
/// (`CF_DIB`, orientation bottom-up). Remplace le contenu courant.
pub(crate) fn set_image(image: &ImageRgba) -> Result<()> {
    let dib = rgba_vers_dib(image)?;

    let _ouvert = OpenedClipboard::open()?;
    // SAFETY : presse-papiers ouvert par le guard ci-dessus.
    unsafe { EmptyClipboard() }.map_err(|e| clip_err("EmptyClipboard", e))?;
    poser_bloc(CF_DIB, &dib)
}

/// Liste des fichiers copiés dans le presse-papiers (`CF_HDROP`), tels que
/// déposés par « Copier » dans l'explorateur. Liste vide si aucun fichier.
pub(crate) fn get_files() -> Result<Vec<PathBuf>> {
    let _ouvert = OpenedClipboard::open()?;

    // SAFETY : simple interrogation de disponibilité d'un format.
    if unsafe { IsClipboardFormatAvailable(CF_HDROP) }.is_err() {
        return Ok(Vec::new());
    }
    // SAFETY : presse-papiers ouvert par le guard ; le handle renvoyé pour
    // CF_HDROP est un HDROP valide tant que le presse-papiers reste ouvert.
    let handle = unsafe { GetClipboardData(CF_HDROP) }
        .map_err(|e| clip_err("GetClipboardData(CF_HDROP)", e))?;
    let hdrop = HDROP(handle.0);

    // SAFETY : l'index `u32::MAX` demande le nombre de fichiers du HDROP.
    let nb = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut chemins = Vec::with_capacity(nb as usize);
    for i in 0..nb {
        // SAFETY : sans tampon, renvoie la longueur du chemin `i` (hors NUL).
        let longueur = unsafe { DragQueryFileW(hdrop, i, None) } as usize;
        if longueur == 0 {
            continue; // entrée illisible : ignorée (défense en profondeur)
        }
        // Tampon de `longueur + 1` u16 (+1 pour le NUL final).
        let mut tampon = vec![0u16; longueur + 1];
        // SAFETY : l'API copie dans le tampon le chemin suivi d'un NUL et
        // renvoie le nombre de caractères copiés (hors NUL).
        let copie = unsafe { DragQueryFileW(hdrop, i, Some(&mut tampon)) } as usize;
        let copie = copie.min(longueur);
        chemins.push(PathBuf::from(OsString::from_wide(&tampon[..copie])));
    }
    Ok(chemins)
}

/// Place la liste `paths` dans le presse-papiers sous forme de `CF_HDROP` :
/// structure `DROPFILES` (en-tête de 20 octets) suivie des chemins en UTF-16
/// terminés par NUL, la liste étant close par un NUL supplémentaire (double NUL
/// final). Remplace le contenu courant.
pub(crate) fn set_files(paths: &[PathBuf]) -> Result<()> {
    // En-tête DROPFILES : pFiles (offset de la liste = 20), pt.x, pt.y, fNC,
    // fWide=1 (chemins larges/UTF-16). Chaque champ est un entier 32 bits LE.
    let mut bloc: Vec<u8> = Vec::new();
    bloc.extend_from_slice(&20u32.to_le_bytes()); // pFiles : la liste suit l'en-tête
    bloc.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bloc.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bloc.extend_from_slice(&0i32.to_le_bytes()); // fNC = FALSE
    bloc.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE (UTF-16)

    // Liste : chaque chemin en UTF-16 terminé par NUL, puis un NUL de clôture.
    for p in paths {
        for unite in p.as_os_str().encode_wide() {
            bloc.extend_from_slice(&unite.to_le_bytes());
        }
        bloc.extend_from_slice(&0u16.to_le_bytes());
    }
    bloc.extend_from_slice(&0u16.to_le_bytes()); // NUL final de la liste

    let _ouvert = OpenedClipboard::open()?;
    // SAFETY : presse-papiers ouvert par le guard ci-dessus.
    unsafe { EmptyClipboard() }.map_err(|e| clip_err("EmptyClipboard", e))?;
    poser_bloc(CF_HDROP, &bloc)
}

// ---------------------------------------------------------------------------
// Conversions DIB ↔ RGBA (pures, sans FFI presse-papiers)
// ---------------------------------------------------------------------------

/// Décode un bloc `CF_DIB` (`BITMAPINFOHEADER` + masques/table éventuels +
/// pixels) en image RGBA top-down. Formats gérés : 24 et 32 bits non
/// compressés (`BI_RGB`), plus 32 bits `BI_BITFIELDS` avec les masques
/// standard BGRX (R=0x00FF0000, G=0x0000FF00, B=0x000000FF). L'orientation
/// bottom-up (hauteur positive, la plus courante) et top-down (hauteur
/// négative) sont toutes deux prises en charge.
fn dib_vers_rgba(dib: &[u8]) -> Result<ImageRgba> {
    if dib.len() < TAILLE_EN_TETE {
        return Err(dib_err(format!("bloc trop court ({} octets)", dib.len())));
    }
    // SAFETY : le bloc fait au moins `size_of::<BITMAPINFOHEADER>()` octets ;
    // lecture non alignée d'une structure POD `repr(C)` sans invariant.
    let en_tete = unsafe { std::ptr::read_unaligned(dib.as_ptr().cast::<BITMAPINFOHEADER>()) };

    let bi_size = en_tete.biSize as usize;
    if bi_size < TAILLE_EN_TETE {
        return Err(dib_err(format!("biSize invalide ({bi_size})")));
    }
    let largeur = u32::try_from(en_tete.biWidth)
        .ok()
        .filter(|l| *l > 0)
        .ok_or_else(|| dib_err(format!("largeur invalide ({})", en_tete.biWidth)))?;
    if en_tete.biHeight == 0 {
        return Err(dib_err("hauteur nulle"));
    }
    // Hauteur positive = lignes stockées du bas vers le haut (bottom-up).
    let bas_en_haut = en_tete.biHeight > 0;
    let hauteur = en_tete.biHeight.unsigned_abs();

    let bpp = en_tete.biBitCount;
    let compression = en_tete.biCompression;
    let compatible = compression == BI_RGB.0 && (bpp == 24 || bpp == 32)
        || compression == BI_BITFIELDS.0 && bpp == 32;
    if !compatible {
        return Err(dib_err(format!(
            "format non géré : {bpp} bits, compression {compression} (attendu 24/32 bits BI_RGB ou 32 bits BI_BITFIELDS)"
        )));
    }
    // BI_BITFIELDS : seuls les masques standard « BGRX » sont acceptés. Quelle
    // que soit la version d'en-tête, ils sont aux offsets 40/44/48 du bloc.
    if compression == BI_BITFIELDS.0 {
        if dib.len() < TAILLE_EN_TETE + 12 {
            return Err(dib_err("bloc trop court pour les masques BI_BITFIELDS"));
        }
        let masque = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().expect("4 octets"));
        let (r, g, b) = (masque(40), masque(44), masque(48));
        if (r, g, b) != (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) {
            return Err(dib_err(format!(
                "masques BI_BITFIELDS non gérés (R={r:#010x}, G={g:#010x}, B={b:#010x})"
            )));
        }
    }

    // Début des pixels : en-tête + masques hors en-tête (BI_BITFIELDS avec
    // en-tête de 40 octets uniquement) + table de couleurs éventuelle.
    let masques_apres_en_tete = if compression == BI_BITFIELDS.0 && bi_size == TAILLE_EN_TETE {
        12
    } else {
        0
    };
    let offset_pixels = bi_size
        .checked_add(masques_apres_en_tete)
        .and_then(|o| o.checked_add((en_tete.biClrUsed as usize).checked_mul(4)?))
        .ok_or_else(|| dib_err("table de couleurs démesurée"))?;

    // Stride : chaque ligne est bourrée à un multiple de 4 octets.
    let octets_px = usize::from(bpp / 8); // 3 ou 4
    let stride = (largeur as usize)
        .checked_mul(usize::from(bpp))
        .and_then(|bits| bits.checked_add(31))
        .map(|bits| bits / 32 * 4)
        .ok_or_else(|| dib_err("dimensions démesurées"))?;
    let besoin = stride
        .checked_mul(hauteur as usize)
        .and_then(|pixels| pixels.checked_add(offset_pixels))
        .ok_or_else(|| dib_err("dimensions démesurées"))?;
    if dib.len() < besoin {
        return Err(dib_err(format!(
            "bloc de {} octets trop court pour {largeur}x{hauteur} en {bpp} bits (attendu ≥ {besoin})",
            dib.len()
        )));
    }

    let mut rgba = Vec::with_capacity(largeur as usize * hauteur as usize * 4);
    let mut alpha_max = 0u8;
    for y in 0..hauteur as usize {
        let y_source = if bas_en_haut {
            hauteur as usize - 1 - y
        } else {
            y
        };
        let ligne = &dib[offset_pixels + y_source * stride..][..largeur as usize * octets_px];
        for px in ligne.chunks_exact(octets_px) {
            // Le DIB stocke B, G, R (, X/A) ; RGBA attend R, G, B, A.
            let alpha = if octets_px == 4 { px[3] } else { 0xFF };
            alpha_max = alpha_max.max(alpha);
            rgba.extend_from_slice(&[px[2], px[1], px[0], alpha]);
        }
    }
    // Quatrième canal « réservé » des DIB 32 bits : beaucoup de producteurs y
    // écrivent 0. Une image intégralement transparente n'ayant aucun sens dans
    // un presse-papiers, elle est considérée comme opaque.
    if octets_px == 4 && alpha_max == 0 {
        for alpha in rgba.iter_mut().skip(3).step_by(4) {
            *alpha = 0xFF;
        }
    }

    Ok(ImageRgba {
        width: largeur,
        height: hauteur,
        rgba,
    })
}

/// Encode une image RGBA top-down en bloc `CF_DIB` : `BITMAPINFOHEADER`
/// 32 bits `BI_RGB` suivi des pixels BGRA en orientation bottom-up (la
/// convention DIB, hauteur positive). Le 32 bits évite tout bourrage de ligne
/// et préserve le canal alpha.
fn rgba_vers_dib(image: &ImageRgba) -> Result<Vec<u8>> {
    let largeur = image.width as usize;
    let hauteur = image.height as usize;
    let taille_pixels = largeur
        .checked_mul(hauteur)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| dib_err("dimensions démesurées"))?;
    if image.width == 0 || image.height == 0 || image.rgba.len() != taille_pixels {
        return Err(dib_err(format!(
            "image incohérente : {}x{} mais {} octets RGBA (attendu {taille_pixels})",
            image.width,
            image.height,
            image.rgba.len()
        )));
    }

    let en_tete = BITMAPINFOHEADER {
        biSize: TAILLE_EN_TETE as u32,
        biWidth: i32::try_from(image.width).map_err(|_| dib_err("largeur > i32::MAX"))?,
        // Hauteur positive : bottom-up (les lignes sont écrites en ordre inversé).
        biHeight: i32::try_from(image.height).map_err(|_| dib_err("hauteur > i32::MAX"))?,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: u32::try_from(taille_pixels)
            .map_err(|_| dib_err("image trop grande pour un DIB"))?,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    let mut dib = Vec::with_capacity(TAILLE_EN_TETE + taille_pixels);
    // SAFETY : `BITMAPINFOHEADER` est un POD `repr(C)` de 40 octets sans
    // bourrage interne (champs alignés naturellement) : sa représentation
    // mémoire est exactement l'en-tête DIB attendu par le presse-papiers.
    let octets_en_tete = unsafe {
        std::slice::from_raw_parts(
            (&en_tete as *const BITMAPINFOHEADER).cast::<u8>(),
            TAILLE_EN_TETE,
        )
    };
    dib.extend_from_slice(octets_en_tete);

    // Pixels BGRA, lignes du bas vers le haut (bottom-up).
    for y in (0..hauteur).rev() {
        let ligne = &image.rgba[y * largeur * 4..][..largeur * 4];
        for px in ligne.chunks_exact(4) {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Ok(dib)
}

// ---------------------------------------------------------------------------
// Tests (conversions pures uniquement : aucun accès au presse-papiers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Image de test déterministe (canaux variés, alpha jamais nul).
    fn image_test(width: u32, height: u32) -> ImageRgba {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[
                    (x * 40 + 3) as u8,
                    (y * 25 + 7) as u8,
                    ((x + y) * 11) as u8,
                    200 + (x % 50) as u8,
                ]);
            }
        }
        ImageRgba {
            width,
            height,
            rgba,
        }
    }

    /// En-tête `BITMAPINFOHEADER` de 40 octets sérialisé à la main
    /// (indépendant de `rgba_vers_dib`, pour tester la lecture seule).
    fn en_tete_octets(largeur: i32, hauteur: i32, bpp: u16, compression: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(40);
        v.extend_from_slice(&40u32.to_le_bytes()); // biSize
        v.extend_from_slice(&largeur.to_le_bytes());
        v.extend_from_slice(&hauteur.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&compression.to_le_bytes());
        v.extend_from_slice(&[0u8; 20]); // biSizeImage… biClrImportant à zéro
        v
    }

    #[test]
    fn aller_retour_rgba_dib_rgba() {
        // Largeur impaire : sans incidence en 32 bits (pas de bourrage), mais
        // vérifie qu'aucun alignement parasite ne s'invite.
        let image = image_test(5, 3);
        let dib = rgba_vers_dib(&image).unwrap();
        assert_eq!(dib.len(), 40 + 5 * 3 * 4);
        // En-tête : 40 octets, 32 bits, BI_RGB, hauteur positive (bottom-up).
        assert_eq!(&dib[0..4], &40u32.to_le_bytes());
        assert_eq!(&dib[4..8], &5i32.to_le_bytes());
        assert_eq!(&dib[8..12], &3i32.to_le_bytes());
        assert_eq!(&dib[14..16], &32u16.to_le_bytes());
        assert_eq!(&dib[16..20], &BI_RGB.0.to_le_bytes());
        // Premier pixel stocké = coin bas-gauche, en BGRA.
        let bas_gauche = &image.rgba[(3 - 1) * 5 * 4..][..4];
        assert_eq!(
            &dib[40..44],
            &[bas_gauche[2], bas_gauche[1], bas_gauche[0], bas_gauche[3]]
        );

        let relue = dib_vers_rgba(&dib).unwrap();
        assert_eq!(relue, image);
    }

    #[test]
    fn dib_24_bits_bottom_up_avec_bourrage() {
        // 2x2 en 24 bits : lignes de 6 octets bourrées à 8 (multiple de 4).
        // Ligne stockée en premier = bas de l'image (bottom-up).
        let mut dib = en_tete_octets(2, 2, 24, BI_RGB.0);
        dib.extend_from_slice(&[1, 2, 3, 4, 5, 6, 0xAA, 0xAA]); // bas : BGR, BGR, bourrage
        dib.extend_from_slice(&[7, 8, 9, 10, 11, 12, 0xAA, 0xAA]); // haut
        let image = dib_vers_rgba(&dib).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        // RGBA top-down : ligne du haut d'abord, B/G/R inversés en R/G/B, alpha opaque.
        assert_eq!(
            image.rgba,
            vec![9, 8, 7, 255, 12, 11, 10, 255, 3, 2, 1, 255, 6, 5, 4, 255]
        );
    }

    #[test]
    fn dib_32_bits_top_down() {
        // Hauteur négative : lignes stockées du haut vers le bas, sans réordonnancement.
        let mut dib = en_tete_octets(1, -2, 32, BI_RGB.0);
        dib.extend_from_slice(&[1, 2, 3, 40]); // haut : BGRA
        dib.extend_from_slice(&[4, 5, 6, 50]); // bas
        let image = dib_vers_rgba(&dib).unwrap();
        assert_eq!((image.width, image.height), (1, 2));
        assert_eq!(image.rgba, vec![3, 2, 1, 40, 6, 5, 4, 50]);
    }

    #[test]
    fn dib_32_bits_bitfields_masques_standard() {
        // BI_BITFIELDS avec en-tête de 40 octets : masques BGRX standard aux
        // offsets 40..52, pixels ensuite.
        let mut dib = en_tete_octets(1, 1, 32, BI_BITFIELDS.0);
        dib.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // masque R
        dib.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // masque G
        dib.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // masque B
        dib.extend_from_slice(&[9, 8, 7, 0]); // BGRX (quatrième octet non défini)
        let image = dib_vers_rgba(&dib).unwrap();
        // Alpha intégralement nul → considéré opaque.
        assert_eq!(image.rgba, vec![7, 8, 9, 255]);

        // Masques non standard → refus explicite.
        let mut exotique = en_tete_octets(1, 1, 32, BI_BITFIELDS.0);
        exotique.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // R et B permutés
        exotique.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
        exotique.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
        exotique.extend_from_slice(&[9, 8, 7, 0]);
        assert!(dib_vers_rgba(&exotique).is_err());
    }

    #[test]
    fn dib_32_bits_alpha_tout_nul_devient_opaque() {
        let mut dib = en_tete_octets(2, 1, 32, BI_RGB.0);
        dib.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        let image = dib_vers_rgba(&dib).unwrap();
        assert_eq!(image.rgba, vec![3, 2, 1, 255, 6, 5, 4, 255]);

        // À l'inverse, un alpha partiellement non nul est préservé tel quel.
        let mut dib = en_tete_octets(2, 1, 32, BI_RGB.0);
        dib.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 128]);
        let image = dib_vers_rgba(&dib).unwrap();
        assert_eq!(image.rgba, vec![3, 2, 1, 0, 6, 5, 4, 128]);
    }

    #[test]
    fn dib_invalides_rejetes() {
        // Bloc plus court qu'un en-tête.
        assert!(dib_vers_rgba(&[0u8; 12]).is_err());
        // Profondeur non gérée (8 bits palette).
        let mut dib = en_tete_octets(1, 1, 8, BI_RGB.0);
        dib.extend_from_slice(&[0u8; 8]);
        assert!(dib_vers_rgba(&dib).is_err());
        // Pixels manquants par rapport aux dimensions annoncées.
        let mut dib = en_tete_octets(4, 4, 32, BI_RGB.0);
        dib.extend_from_slice(&[0u8; 16]); // 1 ligne au lieu de 4
        assert!(dib_vers_rgba(&dib).is_err());
        // Largeur négative.
        let mut dib = en_tete_octets(-1, 1, 32, BI_RGB.0);
        dib.extend_from_slice(&[0u8; 4]);
        assert!(dib_vers_rgba(&dib).is_err());
    }

    #[test]
    fn rgba_incoherente_rejetee() {
        // Longueur de pixels incohérente avec les dimensions.
        let image = ImageRgba {
            width: 2,
            height: 2,
            rgba: vec![0u8; 15],
        };
        assert!(rgba_vers_dib(&image).is_err());
        // Dimension nulle.
        let vide = ImageRgba {
            width: 0,
            height: 1,
            rgba: Vec::new(),
        };
        assert!(rgba_vers_dib(&vide).is_err());
    }
}
