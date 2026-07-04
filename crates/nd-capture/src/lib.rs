//! `nd-capture` — abstraction de la capture d'écran.
//!
//! Le trait [`ScreenCapturer`] est implémenté par plateforme (DXGI/WGC sur Windows,
//! ScreenCaptureKit sur macOS, X11/PipeWire sur Linux). Le détail des API, du
//! zéro-copie GPU et de la détection de régions modifiées est dans
//! `../../plan-technique/02-capture-ecran.md`.
//!
//! Phase 1 : l'implémentation **Windows (DXGI Desktop Duplication)** est active (module
//! [`win`]). Elle produit pour l'instant des frames en mémoire CPU (chemin de repli)
//! via une texture de *staging* ; le chemin zéro-copie GPU (texture passée directement
//! à l'encodeur matériel, voir plan 03) sera branché avec `nd-codec`.

use nd_proto::{MonitorId, Result};

#[cfg(windows)]
mod win;

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
/// Windows : DXGI Desktop Duplication. Autres OS : à venir (Phases 4+, voir plan 16).
pub fn create_capturer() -> Result<Box<dyn ScreenCapturer>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win::DxgiCapturer::new()?))
    }
    #[cfg(not(windows))]
    {
        Err(nd_proto::NdError::NotImplemented(
            "nd-capture::create_capturer (impl macOS/Linux à venir, voir plan 02/16)",
        ))
    }
}
