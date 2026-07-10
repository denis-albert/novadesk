//! Implémentation macOS de [`ScreenCapturer`] via **CoreGraphics**
//! (`CGDisplayCreateImage`). Voir plan 02 §macOS.
//!
//! Modèle « tirer » : chaque appel à [`CgCapturer::next_frame`] prend un instantané
//! du moniteur (cadencé sur `target_fps`) et le renvoie en BGRA CPU avec une unique
//! région modifiée pleine image. Le passage à **ScreenCaptureKit** (flux poussé par
//! le système, zéro-copie `IOSurface`, régions modifiées fines) est prévu dans un
//! jet ultérieur (plan 02/12) ; ce chemin CoreGraphics reste le repli universel.
//!
//! Format : `CGDisplayCreateImage` renvoie un `CGImage` 32 bits/pixel
//! `kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst`, soit un
//! agencement mémoire `B,G,R,A` → [`PixelFormat::Bgra8`]. Le nombre de bits par
//! pixel est vérifié à l'exécution.
//!
//! Permissions : depuis macOS 10.15, la capture exige l'autorisation
//! « Enregistrement de l'écran » (Réglages Système → Confidentialité et sécurité).
//! Sans elle, `CGDisplayCreateImage` échoue ou ne renvoie que le fond d'écran.
//!
//! Aucun `unsafe` ici : la crate `core-graphics` encapsule tout le FFI.
//! Écrit sans vérification locale (machine Windows) — validé par la CI `macos-latest`.

use std::time::{Duration, Instant};

use core_graphics::display::{CGDirectDisplayID, CGDisplay, CGError};
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use nd_proto::{MonitorId, NdError, Result};

use crate::{
    clamp_region, CaptureConfig, CaptureEvent, CapturedFrame, CursorState, FrameImage, MonitorInfo,
    PixelFormat, Rect, ScreenCapturer,
};

/// Convertit un code `CGError` en [`NdError::Capture`].
fn cap_cg(contexte: &str, code: CGError) -> NdError {
    NdError::Capture(format!("{contexte} : CGError {code}"))
}

/// Énumère les écrans actifs via `CGGetActiveDisplayList`.
///
/// `MonitorId(i)` = index dans la liste des écrans actifs — exactement la
/// correspondance qu'utilise [`CgCapturer::start`]. Dimensions en **pixels**
/// physiques (`CGDisplayPixelsWide/High`), position en **points** de l'espace
/// global (`CGDisplayBounds`) — les deux diffèrent du facteur d'échelle Retina.
pub(crate) fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    let ids = CGDisplay::active_displays().map_err(|c| cap_cg("CGGetActiveDisplayList", c))?;
    Ok(ids
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            MonitorInfo {
                id: MonitorId(i as u32),
                // Pas de nom lisible via l'API sûre : identifiant CoreGraphics.
                name: format!("CGDisplay-{id}"),
                width: display.pixels_wide() as u32,
                height: display.pixels_high() as u32,
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                is_primary: display.is_main(),
            }
        })
        .collect())
}

/// Capteur d'écran macOS fondé sur `CGDisplayCreateImage` (voir la doc du module).
pub(crate) struct CgCapturer {
    /// Écran capturé ; `None` tant que `start` n'a pas été appelé.
    display: Option<CGDirectDisplayID>,
    monitor: MonitorId,
    capture_cursor: bool,
    /// Sous-région partagée (« cadre d'écran »), en pixels écran ; `None` = plein
    /// écran. Bornée aux dimensions réelles à chaque frame ([`clamp_region`]).
    cadre: Option<Rect>,
    /// Intervalle entre deux instantanés (dérivé de `target_fps`).
    intervalle: Duration,
    /// Prochaine échéance de capture (cadence « tirer »).
    prochain_tick: Option<Instant>,
    start: Instant,
}

impl CgCapturer {
    /// Prépare le capteur ; l'écran est résolu dans [`ScreenCapturer::start`].
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            display: None,
            monitor: MonitorId(0),
            capture_cursor: false,
            cadre: None,
            intervalle: Duration::from_millis(16),
            prochain_tick: None,
            start: Instant::now(),
        })
    }

    /// Position du curseur en pixels, relative au coin haut-gauche de l'écran capturé.
    ///
    /// Quartz livre la position en **points** de l'espace global (origine au coin
    /// haut-gauche de l'écran principal) : on la ramène à l'écran capturé puis on
    /// applique le facteur d'échelle Retina (pixels / points).
    fn interroge_curseur(
        display: CGDisplay,
        largeur_px: u32,
        hauteur_px: u32,
    ) -> Option<CursorState> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
        let position = CGEvent::new(source).ok()?.location();
        let bounds = display.bounds();
        let echelle = if bounds.size.width > 0.0 {
            f64::from(largeur_px) / bounds.size.width
        } else {
            1.0
        };
        let x = ((position.x - bounds.origin.x) * echelle).round() as i32;
        let y = ((position.y - bounds.origin.y) * echelle).round() as i32;
        let visible = x >= 0 && y >= 0 && (x as u32) < largeur_px && (y as u32) < hauteur_px;
        Some(CursorState { x, y, visible })
    }
}

impl ScreenCapturer for CgCapturer {
    fn start(&mut self, cfg: CaptureConfig) -> Result<()> {
        let ids = CGDisplay::active_displays().map_err(|c| cap_cg("CGGetActiveDisplayList", c))?;
        let id = ids.get(cfg.monitor.0 as usize).copied().ok_or_else(|| {
            NdError::Capture(format!(
                "moniteur {:?} introuvable ({} écran(s) actif(s))",
                cfg.monitor,
                ids.len()
            ))
        })?;
        self.display = Some(id);
        self.monitor = cfg.monitor;
        self.capture_cursor = cfg.capture_cursor;
        self.intervalle = Duration::from_secs_f64(1.0 / f64::from(cfg.target_fps.max(1)));
        self.prochain_tick = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let id = self
            .display
            .ok_or_else(|| NdError::Capture("capture non démarrée (appeler start)".into()))?;

        // Cadence : CGDisplayCreateImage est un modèle « tirer », on suit target_fps.
        let maintenant = Instant::now();
        if let Some(tick) = self.prochain_tick {
            if tick > maintenant {
                std::thread::sleep(tick - maintenant);
            }
        }
        self.prochain_tick = Some(Instant::now() + self.intervalle);

        let display = CGDisplay::new(id);
        let image = display.image().ok_or_else(|| {
            NdError::Capture(
                "CGDisplayCreateImage a échoué — autorisation « Enregistrement de l'écran » \
                 manquante ? (Réglages Système → Confidentialité et sécurité)"
                    .into(),
            )
        })?;

        let largeur = image.width() as u32;
        let hauteur = image.height() as u32;
        let bpp = image.bits_per_pixel();
        if bpp != 32 {
            return Err(NdError::Capture(format!(
                "CGImage à {bpp} bits/pixel non géré (32 attendus)"
            )));
        }
        let stride_source = image.bytes_per_row();
        let stride_dest = largeur as usize * 4;
        let donnees = image.data();
        let source = donnees.bytes();
        if stride_source < stride_dest || source.len() < stride_source * hauteur as usize {
            return Err(NdError::Capture(format!(
                "CGImage incohérent : {} octets, stride {stride_source}, {largeur}x{hauteur}",
                source.len()
            )));
        }

        // Recopie ligne à ligne : le stride CoreGraphics peut dépasser `largeur * 4`.
        let mut bgra = vec![0u8; stride_dest * hauteur as usize];
        for (dest, src) in bgra
            .chunks_exact_mut(stride_dest)
            .zip(source.chunks_exact(stride_source))
        {
            dest.copy_from_slice(&src[..stride_dest]);
        }

        // Curseur dans le repère de l'écran, translaté ensuite dans le cadre.
        let cursor_plein = if self.capture_cursor {
            Self::interroge_curseur(display, largeur, hauteur)
        } else {
            None
        };

        // Sous-région partagée (« cadre d'écran ») : CoreGraphics ne restreint pas la
        // capture, on recadre le tampon BGRA déjà lu (le plein écran n'est jamais
        // exposé — seule la découpe entre dans la frame).
        let (ox, oy, rw, rh) = clamp_region(self.cadre, largeur, hauteur);
        let (data, out_w, out_h) = if rw == largeur && rh == hauteur {
            (bgra, largeur, hauteur)
        } else {
            let dst_stride = rw as usize * 4;
            let mut crop = vec![0u8; dst_stride * rh as usize];
            for ligne in 0..rh as usize {
                let s = (oy as usize + ligne) * stride_dest + ox as usize * 4;
                let d = ligne * dst_stride;
                crop[d..d + dst_stride].copy_from_slice(&bgra[s..s + dst_stride]);
            }
            (crop, rw, rh)
        };
        let cursor = cursor_plein.map(|c| {
            let x = c.x - ox as i32;
            let y = c.y - oy as i32;
            CursorState {
                x,
                y,
                visible: x >= 0 && y >= 0 && (x as u32) < rw && (y as u32) < rh,
            }
        });

        Ok(CapturedFrame {
            width: out_w,
            height: out_h,
            monitor: self.monitor,
            format: PixelFormat::Bgra8,
            // Pas de détection de dommages avec CGDisplayCreateImage : toute la
            // frame est réputée modifiée (ScreenCaptureKit l'affinera, plan 02/12).
            dirty: vec![Rect {
                x: 0,
                y: 0,
                w: out_w,
                h: out_h,
            }],
            cursor,
            timestamp_us: self.start.elapsed().as_micros() as u64,
            image: Some(FrameImage::Cpu {
                data,
                stride: out_w as usize * 4,
            }),
        })
    }

    fn poll_event(&mut self) -> Option<CaptureEvent> {
        // Les reconfigurations d'écran (CGDisplayRegisterReconfigurationCallback,
        // puis ScreenCaptureKit) seront branchées dans un jet ultérieur — plan 02/13.
        None
    }

    fn stop(&mut self) {
        self.display = None;
        self.prochain_tick = None;
    }

    /// Fixe le « cadre d'écran » (sous-région, en pixels écran). Borné aux dimensions
    /// réelles à la frame suivante ([`clamp_region`]).
    fn set_region(&mut self, region: Option<Rect>) -> Result<()> {
        self.cadre = region;
        Ok(())
    }
}
