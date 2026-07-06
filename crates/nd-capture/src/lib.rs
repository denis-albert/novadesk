//! `nd-capture` — abstraction de la capture d'écran.
//!
//! Le trait [`ScreenCapturer`] est implémenté par plateforme (DXGI/WGC sur Windows,
//! ScreenCaptureKit sur macOS, X11/PipeWire sur Linux). Le détail des API, du
//! zéro-copie GPU et de la détection de régions modifiées est dans
//! `../../plan-technique/02-capture-ecran.md`.
//!
//! Backends actifs :
//! - **Windows** : DXGI Desktop Duplication (module `win`), frames CPU via une
//!   texture de *staging* ; le chemin zéro-copie GPU (texture passée directement
//!   à l'encodeur matériel, voir plan 03) sera branché avec `nd-codec`.
//! - **macOS** : CoreGraphics `CGDisplayCreateImage` (module `macos`), instantanés
//!   cadencés ; ScreenCaptureKit (flux poussé, zéro-copie) au jet suivant.
//! - **Linux** : X11 `GetImage` via `x11rb` (module `linux`), moniteurs RandR ;
//!   Wayland (PipeWire + portail `xdg-desktop-portal`) au jet suivant.

use nd_proto::{MonitorId, Result};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_cursor;

/// Description d'un moniteur physique attaché au bureau.
///
/// Multi-écran (plan 13) : [`MonitorInfo::id`] est directement utilisable comme
/// [`CaptureConfig::monitor`] — l'index suit l'ordre d'énumération du backend de la
/// plateforme (sortie DXGI de l'adaptateur par défaut sous Windows, liste
/// `CGGetActiveDisplayList` sous macOS, réponse RandR `GetMonitors` sous Linux),
/// le même que celui employé par le capteur correspondant (`MonitorId(0)` =
/// première sortie).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: MonitorId,
    /// Nom système de l'écran (ex. `\\.\DISPLAY1` sous Windows).
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Abscisse du coin haut-gauche dans le bureau virtuel (peut être négative).
    pub x: i32,
    /// Ordonnée du coin haut-gauche dans le bureau virtuel (peut être négative).
    pub y: i32,
    /// Vrai pour le moniteur principal (origine du bureau virtuel).
    pub is_primary: bool,
}

/// Énumère les moniteurs attachés au bureau, dans l'ordre du backend plateforme.
///
/// Windows : DXGI (`IDXGIFactory1` → adaptateur par défaut → `EnumOutputs`).
/// macOS : CoreGraphics (`CGGetActiveDisplayList`). Linux : X11/RandR
/// (`GetMonitors`, repli « racine entière » sans RandR ; Wayland au jet suivant).
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    #[cfg(windows)]
    {
        win::enumerate_monitors()
    }
    #[cfg(target_os = "macos")]
    {
        macos::enumerate_monitors()
    }
    #[cfg(target_os = "linux")]
    {
        linux::enumerate_monitors()
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(nd_proto::NdError::NotImplemented(
            "nd-capture::enumerate_monitors (OS non pris en charge, voir plan 02/16)",
        ))
    }
}

/// Rectangle en pixels dans l'espace du moniteur capturé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Format de pixel d'une frame capturée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits par canal, ordre BGRA (fréquent sous Windows / DXGI).
    Bgra8,
    /// 8 bits par canal, ordre RGBA.
    Rgba8,
    /// YUV 4:2:0 semi-planaire — prêt pour l'encodeur matériel (voir plan 03).
    Nv12,
}

/// Paramètres de démarrage d'une capture.
#[derive(Debug, Clone, Copy)]
pub struct CaptureConfig {
    pub monitor: MonitorId,
    /// Cadence cible (le pipeline peut descendre plus bas si rien ne change).
    pub target_fps: u32,
    /// Capturer aussi la position du curseur (curseur matériel séparé).
    pub capture_cursor: bool,
}

/// État du curseur associé à une frame.
#[derive(Debug, Clone, Copy)]
pub struct CursorState {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

/// Forme (bitmap) du curseur système, capturée à la demande.
///
/// API autonome (plan 02 §curseur), indépendante du flux de frames : le viewer
/// dessine un curseur fidèle à partir de [`CursorState`] (position, embarquée dans
/// [`CapturedFrame`]) et de cette forme — sans réencoder la vidéo à chaque mouvement.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    /// Abscisse du point actif (pointe du curseur), relative au coin haut-gauche.
    pub hotspot_x: i32,
    /// Ordonnée du point actif, relative au coin haut-gauche.
    pub hotspot_y: i32,
    /// Pixels RGBA 8 bits, ligne 0 en haut — `width * height * 4` octets.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for CursorShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // On n'imprime pas les octets — juste la taille du buffer.
        f.debug_struct("CursorShape")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hotspot_x", &self.hotspot_x)
            .field("hotspot_y", &self.hotspot_y)
            .field("rgba_len", &self.rgba.len())
            .finish()
    }
}

/// Capture la forme (bitmap RGBA) du curseur actuellement affiché.
///
/// Renvoie `Ok(None)` si aucun curseur n'est visible. Windows : approche GDI
/// autonome, **sans duplication d'écran** (`GetCursorInfo` → `GetIconInfo` →
/// `GetDIBits`), gérant les curseurs couleur (avec ou sans canal alpha) et
/// monochromes (masques AND/XOR). Autres OS : à venir (Phases 4+, voir plan 16).
pub fn capture_cursor_shape() -> Result<Option<CursorShape>> {
    #[cfg(windows)]
    {
        win_cursor::capture_cursor_shape()
    }
    #[cfg(not(windows))]
    {
        Err(nd_proto::NdError::NotImplemented(
            "nd-capture::capture_cursor_shape (impl macOS/Linux à venir, voir plan 02/16)",
        ))
    }
}

/// Données image d'une frame.
///
/// Le squelette expose la variante CPU (pixels lus en mémoire). La variante GPU
/// (poignée de texture pour le zéro-copie vers l'encodeur) sera ajoutée avec
/// l'intégration `nd-codec` — voir plan 02 §zéro-copie et 03.
#[derive(Clone)]
pub enum FrameImage {
    /// Pixels en mémoire CPU, `stride` octets par ligne.
    Cpu { data: Vec<u8>, stride: usize },
}

impl std::fmt::Debug for FrameImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // On n'imprime pas les octets (frame plein écran) — juste la taille.
            FrameImage::Cpu { data, stride } => f
                .debug_struct("Cpu")
                .field("len", &data.len())
                .field("stride", stride)
                .finish(),
        }
    }
}

/// Une frame capturée.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Moniteur d'où provient la frame.
    pub monitor: MonitorId,
    pub format: PixelFormat,
    /// Régions modifiées depuis la frame précédente (vide = rien n'a changé).
    pub dirty: Vec<Rect>,
    pub cursor: Option<CursorState>,
    /// Horodatage de capture (microsecondes, horloge monotone commune A/V).
    pub timestamp_us: u64,
    /// Image ; `None` = pas de nouveau contenu (délai d'attente écoulé sans changement).
    pub image: Option<FrameImage>,
}

/// Événement remonté par le capteur en dehors du flux de frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEvent {
    /// La résolution ou la configuration d'affichage a changé (hotplug, rotation).
    ResolutionChanged,
    /// La session a été verrouillée / le bureau sécurisé est actif (voir plan 02/07).
    SecureDesktop,
}

/// Source de capture d'écran, implémentée par plateforme.
pub trait ScreenCapturer: Send {
    /// Démarre la capture du moniteur configuré.
    fn start(&mut self, cfg: CaptureConfig) -> Result<()>;
    /// Récupère la prochaine frame (bloquant jusqu'à disponibilité ou délai).
    fn next_frame(&mut self) -> Result<CapturedFrame>;
    /// Consomme le prochain événement de capture s'il y en a un.
    fn poll_event(&mut self) -> Option<CaptureEvent>;
    /// Arrête la capture et libère les ressources.
    fn stop(&mut self);
}

/// Crée le capteur adapté à la plateforme courante.
///
/// Windows : DXGI Desktop Duplication. macOS : CoreGraphics
/// (`CGDisplayCreateImage` ; ScreenCaptureKit au jet suivant). Linux : X11
/// `GetImage` via `x11rb` (Wayland/PipeWire au jet suivant — `NotImplemented`
/// en session Wayland pure).
pub fn create_capturer() -> Result<Box<dyn ScreenCapturer>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win::DxgiCapturer::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::CgCapturer::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::X11Capturer::new()?))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(nd_proto::NdError::NotImplemented(
            "nd-capture::create_capturer (OS non pris en charge, voir plan 02/16)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `capture_cursor_shape` ne panique jamais et, quand une forme est renvoyée,
    /// ses dimensions et la taille de son buffer RGBA (`w*h*4`) sont cohérentes.
    #[test]
    fn capture_forme_curseur_ne_panique_pas() {
        match capture_cursor_shape() {
            Ok(Some(shape)) => {
                assert!(shape.width > 0 && shape.height > 0, "dimensions nulles");
                assert_eq!(
                    shape.rgba.len(),
                    shape.width as usize * shape.height as usize * 4,
                    "taille du buffer RGBA incohérente : {shape:?}"
                );
            }
            // Aucun curseur affiché : acceptable (session sans souris).
            Ok(None) => {}
            // Hors Windows (NotImplemented) ou session sans bureau interactif : acceptable.
            Err(_) => {}
        }
    }
}
