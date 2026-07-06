//! Capture Windows de la **forme** (bitmap) du curseur via GDI — API autonome.
//!
//! Contrairement au flux DXGI (module [`crate::win`]), cette capture ne dépend pas
//! d'une duplication d'écran : `GetCursorInfo` → `GetIconInfo` → extraction des
//! bitmaps masque/couleur en RGBA via `GetDIBits`. Voir plan 02 §curseur.
//!
//! Deux familles de curseurs sont gérées :
//! - **couleur** (`hbmColor` non nul) : pixels BGRA 32 bits ; l'alpha vient du canal
//!   alpha s'il est renseigné (curseurs modernes), sinon du masque AND (anciens
//!   curseurs couleur, alpha uniformément nul) ;
//! - **monochrome** (`hbmColor` nul) : `hbmMask` fait **double hauteur** — masque AND
//!   en haut, masque XOR en bas — combinés en noir/blanc/transparent.
//!
//! Ce module concentre tout le `unsafe` FFI de cette capture ; chaque bloc est
//! documenté `// SAFETY:`. Les objets GDI (bitmaps copiés par `GetIconInfo`, DC
//! écran) sont libérés par gardes RAII — aucune fuite, même en cas d'erreur.
#![allow(unsafe_code)]

use nd_proto::{NdError, Result};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, ICONINFO,
};

use crate::win::cap;
use crate::CursorShape;

/// Garde RAII d'un bitmap GDI possédé (copies renvoyées par `GetIconInfo`).
struct BitmapGuard(HBITMAP);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY : handle bitmap possédé par cette garde, supprimé une seule fois.
            let _ = unsafe { DeleteObject(self.0) };
        }
    }
}

/// Garde RAII du DC écran obtenu par `GetDC(None)`.
struct ScreenDcGuard(HDC);

impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY : DC obtenu par `GetDC(None)`, rendu une seule fois.
            unsafe { ReleaseDC(None, self.0) };
        }
    }
}

/// Lit un bitmap GDI en pixels **BGRA 32 bits**, lignes du haut vers le bas.
///
/// La conversion vers 32 bpp est faite par GDI lui-même (un masque monochrome est
/// étendu en noir `0x000000` / blanc `0xFFFFFF`), ce qui unifie le décodage.
fn read_bitmap_bgra(hdc: HDC, hbm: HBITMAP, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // Hauteur négative = DIB « top-down » : la ligne 0 est en haut.
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; width as usize * height as usize * 4];
    // SAFETY : `buf` fait exactement width*height*4 octets, la taille décrite par
    // l'en-tête (32 bpp sans compression, stride = width*4 sans bourrage) ; `bi`
    // et le pointeur de sortie restent valides pendant l'appel.
    let lines = unsafe {
        GetDIBits(
            hdc,
            hbm,
            0,
            height,
            Some(buf.as_mut_ptr().cast()),
            &mut bi,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return Err(NdError::Capture("GetDIBits a échoué".into()));
    }
    Ok(buf)
}

/// Décode un curseur **couleur** : BGRA du bitmap couleur + alpha (canal ou masque AND).
fn decode_color_cursor(hdc: HDC, color: HBITMAP, mask: HBITMAP, w: u32, h: u32) -> Result<Vec<u8>> {
    let bgra = read_bitmap_bgra(hdc, color, w, h)?;
    // Masque AND étendu en 32 bpp : noir (0) = opaque, blanc (255) = transparent.
    let and_mask = read_bitmap_bgra(hdc, mask, w, h)?;

    // Les curseurs 32 bpp modernes portent un canal alpha ; les anciens curseurs
    // couleur ont un alpha uniformément nul → on le dérive alors du masque AND.
    let has_alpha = bgra.chunks_exact(4).any(|px| px[3] != 0);

    let mut rgba = Vec::with_capacity(bgra.len());
    for (px, m) in bgra.chunks_exact(4).zip(and_mask.chunks_exact(4)) {
        let a = if has_alpha {
            px[3]
        } else if m[0] == 0 {
            255
        } else {
            0
        };
        // BGRA → RGBA.
        rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
    }
    Ok(rgba)
}

/// Décode un curseur **monochrome** : `hbmMask` double hauteur (AND en haut, XOR en bas).
fn decode_mono_cursor(hdc: HDC, mask: HBITMAP, w: u32, h: u32) -> Result<Vec<u8>> {
    // On lit les deux moitiés d'un coup (2·h lignes), étendues en 32 bpp par GDI.
    let full = read_bitmap_bgra(hdc, mask, w, h * 2)?;
    let (and_half, xor_half) = full.split_at(w as usize * h as usize * 4);

    let mut rgba = Vec::with_capacity(and_half.len());
    for (a_px, x_px) in and_half.chunks_exact(4).zip(xor_half.chunks_exact(4)) {
        let and_set = a_px[0] != 0; // blanc = bit AND à 1
        let xor_set = x_px[0] != 0; // blanc = bit XOR à 1
        let (v, alpha) = match (and_set, xor_set) {
            (false, false) => (0u8, 255u8), // écran AND 0, XOR 0 → noir opaque
            (false, true) => (255, 255),    // écran AND 0, XOR 1 → blanc opaque
            (true, false) => (0, 0),        // écran inchangé → transparent
            // AND=1, XOR=1 : inversion de l'écran, non représentable en RGBA
            // statique → repli habituel des viewers : noir opaque (le curseur
            // reste visible sur fond clair).
            (true, true) => (0, 255),
        };
        rgba.extend_from_slice(&[v, v, v, alpha]);
    }
    Ok(rgba)
}

/// Capture la forme du curseur actuellement affiché (voir [`crate::capture_cursor_shape`]).
pub(crate) fn capture_cursor_shape() -> Result<Option<CursorShape>> {
    // 1) Curseur courant : visible ?
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY : `ci` est un buffer de sortie valide, `cbSize` renseigné comme exigé.
    unsafe { GetCursorInfo(&mut ci) }.map_err(cap)?;
    if (ci.flags.0 & CURSOR_SHOWING.0) == 0 || ci.hCursor.is_invalid() {
        return Ok(None);
    }

    // 2) Hotspot + bitmaps masque/couleur. `GetIconInfo` renvoie des COPIES des
    //    bitmaps : elles nous appartiennent → gardes RAII (DeleteObject au drop).
    let mut ii = ICONINFO::default();
    // SAFETY : `hCursor` est valide (vérifié ci-dessus) ; `ii` est un buffer de
    // sortie valide.
    unsafe { GetIconInfo(ci.hCursor, &mut ii) }.map_err(cap)?;
    let mask = BitmapGuard(ii.hbmMask);
    let color = BitmapGuard(ii.hbmColor);

    if mask.0.is_invalid() {
        return Err(NdError::Capture(
            "GetIconInfo : masque de curseur nul".into(),
        ));
    }

    // 3) Dimensions réelles depuis le masque (toujours présent). Pour un curseur
    //    monochrome, le masque fait double hauteur (AND + XOR empilés).
    let mut bm = BITMAP::default();
    // SAFETY : handle bitmap valide ; `bm` reçoit exactement `size_of::<BITMAP>()`
    // octets écrits par GDI.
    let got = unsafe {
        GetObjectW(
            mask.0,
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if got == 0 {
        return Err(NdError::Capture("GetObjectW(hbmMask) a échoué".into()));
    }

    let is_mono = color.0.is_invalid();
    let width = bm.bmWidth.max(0) as u32;
    let mask_height = bm.bmHeight.max(0) as u32;
    let height = if is_mono {
        mask_height / 2
    } else {
        mask_height
    };
    if width == 0 || height == 0 {
        return Err(NdError::Capture("bitmap de curseur de taille nulle".into()));
    }

    // 4) Extraction des pixels en RGBA via un DC écran (requis par GetDIBits).
    // SAFETY : `GetDC(None)` renvoie le DC de l'écran ; la garde le rend au drop.
    let dc = ScreenDcGuard(unsafe { GetDC(None) });
    if dc.0.is_invalid() {
        return Err(NdError::Capture("GetDC(écran) a échoué".into()));
    }

    let rgba = if is_mono {
        decode_mono_cursor(dc.0, mask.0, width, height)?
    } else {
        decode_color_cursor(dc.0, color.0, mask.0, width, height)?
    };

    Ok(Some(CursorShape {
        width,
        height,
        hotspot_x: ii.xHotspot as i32,
        hotspot_y: ii.yHotspot as i32,
        rgba,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'appel direct de l'implémentation Windows ne panique pas et produit une
    /// forme cohérente (dimensions, buffer, hotspot dans l'image) quand il y en a une.
    #[test]
    fn forme_curseur_windows_coherente() {
        // Session sans bureau interactif (service, CI) : l'échec est acceptable.
        let Ok(shape) = capture_cursor_shape() else {
            return;
        };
        let Some(shape) = shape else {
            return; // Aucun curseur affiché : rien d'autre à valider.
        };
        assert!(shape.width > 0 && shape.height > 0, "dimensions nulles");
        assert_eq!(
            shape.rgba.len(),
            shape.width as usize * shape.height as usize * 4,
            "taille du buffer RGBA incohérente"
        );
        assert!(
            shape.hotspot_x >= 0 && (shape.hotspot_x as u32) < shape.width,
            "hotspot_x hors de l'image : {shape:?}"
        );
        assert!(
            shape.hotspot_y >= 0 && (shape.hotspot_y as u32) < shape.height,
            "hotspot_y hors de l'image : {shape:?}"
        );
    }
}
