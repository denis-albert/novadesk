//! Implémentation Linux de [`ScreenCapturer`] via **X11** (`x11rb`, Rust pur, sans
//! bibliothèque C) : `GetImage` (ZPixmap) sur la fenêtre racine, région limitée au
//! moniteur demandé (géométrie RandR). Voir plan 02 §Linux.
//!
//! Modèle « tirer » : contrairement à DXGI (Windows), X11 ne pousse pas de frames.
//! [`X11Capturer::next_frame`] cadence donc les instantanés sur `target_fps` et
//! renvoie chaque frame avec une unique région modifiée pleine image (pas de
//! détection de dommages dans ce jet — l'extension XDamage pourra l'affiner).
//!
//! **Wayland** : la capture y passe obligatoirement par le portail
//! `xdg-desktop-portal` (D-Bus `org.freedesktop.portal.ScreenCast`) et un flux
//! **PipeWire** — prévu pour un jet ultérieur (plan 02/12). En session Wayland pure
//! (`WAYLAND_DISPLAY` défini sans `DISPLAY`), [`X11Capturer::new`] renvoie donc
//! `NotImplemented`. Sous XWayland (`DISPLAY` défini), la capture X11 fonctionne
//! mais peut ne montrer que les fenêtres X11 selon le compositeur.
//!
//! Aucun `unsafe` : `x11rb` expose un protocole X11 entièrement sûr.

use std::time::{Duration, Instant};

use nd_proto::{MonitorId, NdError, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ConnectionExt as _, GetImageReply, ImageFormat, ImageOrder, Screen, Setup, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::{
    CaptureConfig, CaptureEvent, CapturedFrame, CursorState, FrameImage, MonitorInfo, PixelFormat,
    Rect, ScreenCapturer,
};

/// Session Wayland « pure » : `WAYLAND_DISPLAY` défini sans serveur X joignable
/// (`DISPLAY` absent). La capture y exige PipeWire + portail — jet ultérieur.
fn session_wayland_pure() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none()
}

/// Énumère les moniteurs X11 de l'écran par défaut (RandR ≥ 1.5).
///
/// `MonitorId(i)` = index dans la réponse RandR `GetMonitors` (moniteurs actifs) —
/// exactement la correspondance qu'utilise [`X11Capturer::start`].
pub(crate) fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    if session_wayland_pure() {
        return Err(NdError::NotImplemented(
            "nd-capture : énumération Wayland (portail xdg-desktop-portal), voir plan 02/12",
        ));
    }
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|e| NdError::Capture(format!("connexion X11 impossible : {e}")))?;
    let screen = conn.setup().roots[screen_num].clone();
    moniteurs_via_randr(&conn, &screen)
}

/// Énumération RandR des moniteurs de `screen` ; repli sans RandR (ou < 1.5) :
/// un moniteur unique couvrant toute la fenêtre racine.
fn moniteurs_via_randr<C: Connection>(conn: &C, screen: &Screen) -> Result<Vec<MonitorInfo>> {
    let reply = conn
        .randr_get_monitors(screen.root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let Some(reply) = reply.filter(|r| !r.monitors.is_empty()) else {
        // RandR absent ou trop ancien : la racine entière fait office de moniteur 0.
        return Ok(vec![MonitorInfo {
            id: MonitorId(0),
            name: "X11-racine".into(),
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
            x: 0,
            y: 0,
            is_primary: true,
        }]);
    };

    let mut moniteurs = Vec::with_capacity(reply.monitors.len());
    for (i, m) in reply.monitors.iter().enumerate() {
        // Nom lisible : l'atome RandR du moniteur (ex. « DP-1 », « eDP-1 »).
        let name = conn
            .get_atom_name(m.name)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_else(|| format!("RANDR-{i}"));
        moniteurs.push(MonitorInfo {
            id: MonitorId(i as u32),
            name,
            width: u32::from(m.width),
            height: u32::from(m.height),
            x: i32::from(m.x),
            y: i32::from(m.y),
            is_primary: m.primary,
        });
    }
    Ok(moniteurs)
}

/// Région (pixels) d'un moniteur dans l'espace de la fenêtre racine X11.
#[derive(Debug, Clone, Copy)]
struct Region {
    x: i16,
    y: i16,
    largeur: u16,
    hauteur: u16,
}

/// Capteur d'écran Linux fondé sur X11 `GetImage` (voir la doc du module).
pub(crate) struct X11Capturer {
    conn: RustConnection,
    screen_num: usize,
    monitor: MonitorId,
    /// Géométrie du moniteur capturé ; `None` tant que `start` n'a pas été appelé.
    region: Option<Region>,
    capture_cursor: bool,
    /// Intervalle entre deux instantanés (dérivé de `target_fps`).
    intervalle: Duration,
    /// Prochaine échéance de capture (cadence « tirer »).
    prochain_tick: Option<Instant>,
    start: Instant,
}

impl X11Capturer {
    /// Se connecte au serveur X (`$DISPLAY`). En session Wayland pure, renvoie
    /// `NotImplemented` (le chemin PipeWire + portail viendra dans un jet ultérieur).
    pub(crate) fn new() -> Result<Self> {
        if session_wayland_pure() {
            return Err(NdError::NotImplemented(
                "nd-capture : capture Wayland (PipeWire + portail xdg-desktop-portal), voir plan 02/12",
            ));
        }
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| NdError::Capture(format!("connexion X11 impossible : {e}")))?;
        Ok(Self {
            conn,
            screen_num,
            monitor: MonitorId(0),
            region: None,
            capture_cursor: false,
            intervalle: Duration::from_millis(16),
            prochain_tick: None,
            start: Instant::now(),
        })
    }

    /// Position du curseur (`QueryPointer`), relative au moniteur capturé.
    fn interroge_curseur(&self, root: Window, region: Region) -> Option<CursorState> {
        let p = self.conn.query_pointer(root).ok()?.reply().ok()?;
        let x = i32::from(p.root_x) - i32::from(region.x);
        let y = i32::from(p.root_y) - i32::from(region.y);
        let visible = p.same_screen
            && (0..i32::from(region.largeur)).contains(&x)
            && (0..i32::from(region.hauteur)).contains(&y);
        Some(CursorState { x, y, visible })
    }
}

impl ScreenCapturer for X11Capturer {
    fn start(&mut self, cfg: CaptureConfig) -> Result<()> {
        self.capture_cursor = cfg.capture_cursor;
        self.monitor = cfg.monitor;
        self.intervalle = Duration::from_secs_f64(1.0 / f64::from(cfg.target_fps.max(1)));
        self.prochain_tick = None;

        let screen = self.conn.setup().roots[self.screen_num].clone();
        let moniteurs = moniteurs_via_randr(&self.conn, &screen)?;
        let m = moniteurs.get(cfg.monitor.0 as usize).ok_or_else(|| {
            NdError::Capture(format!(
                "moniteur {:?} introuvable ({} détecté(s))",
                cfg.monitor,
                moniteurs.len()
            ))
        })?;
        // La géométrie RandR d'origine tient en i16/u16 : les conversions ne peuvent
        // pas échouer pour des moniteurs réels, mais on reste défensif.
        self.region = Some(Region {
            x: i16::try_from(m.x).map_err(|e| NdError::Capture(e.to_string()))?,
            y: i16::try_from(m.y).map_err(|e| NdError::Capture(e.to_string()))?,
            largeur: u16::try_from(m.width).map_err(|e| NdError::Capture(e.to_string()))?,
            hauteur: u16::try_from(m.height).map_err(|e| NdError::Capture(e.to_string()))?,
        });
        Ok(())
    }

    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let region = self
            .region
            .ok_or_else(|| NdError::Capture("capture non démarrée (appeler start)".into()))?;

        // Cadence : X11 ne pousse pas de frames, on s'aligne sur target_fps.
        let maintenant = Instant::now();
        if let Some(tick) = self.prochain_tick {
            if tick > maintenant {
                std::thread::sleep(tick - maintenant);
            }
        }
        self.prochain_tick = Some(Instant::now() + self.intervalle);

        let setup = self.conn.setup();
        let screen = &setup.roots[self.screen_num];
        let root = screen.root;
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                region.x,
                region.y,
                region.largeur,
                region.hauteur,
                u32::MAX,
            )
            .map_err(|e| NdError::Capture(format!("GetImage : {e}")))?
            .reply()
            .map_err(|e| NdError::Capture(format!("GetImage : {e}")))?;

        let largeur = u32::from(region.largeur);
        let hauteur = u32::from(region.hauteur);
        let bgra = zpixmap_en_bgra(setup, screen, &reply, largeur, hauteur)?;

        let cursor = if self.capture_cursor {
            self.interroge_curseur(root, region)
        } else {
            None
        };

        Ok(CapturedFrame {
            width: largeur,
            height: hauteur,
            monitor: self.monitor,
            format: PixelFormat::Bgra8,
            // Pas de détection de dommages dans ce jet : toute la frame est réputée
            // modifiée (XDamage pourra l'affiner, voir plan 02 §Linux).
            dirty: vec![Rect {
                x: 0,
                y: 0,
                w: largeur,
                h: hauteur,
            }],
            cursor,
            timestamp_us: self.start.elapsed().as_micros() as u64,
            image: Some(FrameImage::Cpu {
                data: bgra,
                stride: largeur as usize * 4,
            }),
        })
    }

    fn poll_event(&mut self) -> Option<CaptureEvent> {
        // Les événements RandR (changement de résolution, hotplug) seront branchés
        // dans un jet ultérieur — voir plan 02/13.
        None
    }

    fn stop(&mut self) {
        self.region = None;
        self.prochain_tick = None;
    }
}

/// Convertit une réponse `GetImage` (ZPixmap 24/32 bpp) en pixels BGRA 8 bits
/// (stride de sortie : `largeur * 4`), en respectant l'ordre d'octets du serveur
/// et les masques RGB du visual.
fn zpixmap_en_bgra(
    setup: &Setup,
    screen: &Screen,
    reply: &GetImageReply,
    largeur: u32,
    hauteur: u32,
) -> Result<Vec<u8>> {
    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == reply.depth)
        .ok_or_else(|| {
            NdError::Capture(format!(
                "format ZPixmap introuvable pour la profondeur {}",
                reply.depth
            ))
        })?;
    let bpp = usize::from(format.bits_per_pixel);
    if bpp != 24 && bpp != 32 {
        return Err(NdError::Capture(format!(
            "ZPixmap à {bpp} bits/pixel non géré (24 ou 32 attendus)"
        )));
    }
    let octets_par_pixel = bpp / 8;
    // `scanline_pad` est exprimé en bits (8, 16 ou 32) ; chaque ligne est arrondie
    // au multiple supérieur.
    let pad = usize::from(format.scanline_pad).max(8) / 8;
    let stride_source = (largeur as usize * octets_par_pixel).div_ceil(pad) * pad;
    if reply.data.len() < stride_source * hauteur as usize {
        return Err(NdError::Capture(format!(
            "GetImage : {} octets reçus, {} attendus",
            reply.data.len(),
            stride_source * hauteur as usize
        )));
    }

    // Masques RGB du visual de la frame ; à défaut, l'agencement 8:8:8 classique.
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == reply.visual);
    let (rouge, vert, bleu) = visual.map_or((0x00ff_0000, 0x0000_ff00, 0x0000_00ff), |v| {
        (v.red_mask, v.green_mask, v.blue_mask)
    });
    let msb_first = setup.image_byte_order == ImageOrder::MSB_FIRST;

    let stride_dest = largeur as usize * 4;
    let mut bgra = vec![0u8; stride_dest * hauteur as usize];
    for (ligne_source, ligne_dest) in reply
        .data
        .chunks_exact(stride_source)
        .zip(bgra.chunks_exact_mut(stride_dest))
    {
        for (pixel, dest) in ligne_source
            .chunks_exact(octets_par_pixel)
            .zip(ligne_dest.chunks_exact_mut(4))
        {
            let v = valeur_pixel(pixel, msb_first);
            dest[0] = canal(v, bleu);
            dest[1] = canal(v, vert);
            dest[2] = canal(v, rouge);
            // GetImage ne renvoie pas d'alpha exploitable : frame opaque.
            dest[3] = 0xff;
        }
    }
    Ok(bgra)
}

/// Assemble la valeur d'un pixel ZPixmap (2 à 4 octets) selon l'ordre d'octets
/// du serveur (`image_byte_order` du Setup).
fn valeur_pixel(octets: &[u8], msb_first: bool) -> u32 {
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
fn canal(valeur: u32, masque: u32) -> u8 {
    if masque == 0 {
        return 0;
    }
    ((valeur & masque) >> masque.trailing_zeros()) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'assemblage d'un pixel respecte l'ordre d'octets du serveur.
    #[test]
    fn valeur_pixel_selon_ordre_octets() {
        let octets = [0x11, 0x22, 0x33, 0x44];
        assert_eq!(valeur_pixel(&octets, false), 0x4433_2211);
        assert_eq!(valeur_pixel(&octets, true), 0x1122_3344);
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
}
