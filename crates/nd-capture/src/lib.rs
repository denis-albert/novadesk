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
// Backend Wayland (PipeWire + portail xdg-desktop-portal), derrière la fonction
// `wayland-pipewire` (désactivée par défaut : lie `libpipewire`, bibliothèque C
// non vérifiable en compilation croisée depuis Windows — voir `linux_pipewire`).
#[cfg(all(target_os = "linux", feature = "wayland-pipewire"))]
mod linux_pipewire;
#[cfg(all(target_os = "linux", feature = "wayland-pipewire"))]
mod linux_portal;
#[cfg(target_os = "macos")]
mod macos;
// Conversions de pixels **pures** (sans appel OS), partagées par les backends
// Linux (X11 ZPixmap, PipeWire) : compilées et testées sur toutes les plateformes,
// y compris Windows, pour couvrir par les tests le code des cibles non
// compilables depuis ce poste.
mod pixel;
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

/// Borne la sous-région `region` (« cadre d'écran ») à un cadre `w`×`h` et renvoie
/// `(x, y, largeur, hauteur)` **toujours non vide et dans les bornes**.
///
/// `None` ⇒ plein cadre `(0, 0, w, h)`. Une région partiellement hors cadre est
/// rognée ; une origine hors cadre est ramenée au dernier pixel valide (jamais
/// d'agrandissement au plein écran — la zone hors-cadre ne doit pas fuiter).
///
/// Logique **partagée par tous les backends** (DXGI, CoreGraphics, X11) — une seule
/// implémentation, testée sur toutes les plateformes, plutôt que trois copies dont
/// deux (macOS/Linux) ne seraient jamais exercées depuis le poste Windows.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn clamp_region(region: Option<Rect>, w: u32, h: u32) -> (u32, u32, u32, u32) {
    match region {
        None => (0, 0, w, h),
        Some(_) if w == 0 || h == 0 => (0, 0, w, h),
        Some(r) => {
            let x = r.x.min(w - 1);
            let y = r.y.min(h - 1);
            let rw = r.w.min(w - x).max(1);
            let rh = r.h.min(h - y).max(1);
            (x, y, rw, rh)
        }
    }
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
    /// Régions modifiées depuis la frame précédente, **en coordonnées de la frame
    /// capturée** (origine au coin haut-gauche de la sous-région si
    /// [`ScreenCapturer::set_region`] en fixe une, sinon du moniteur), bornées à
    /// `width`×`height`.
    ///
    /// **Format (contrat pour nd-codec / mode delta)** — liste des rectangles à
    /// ré-encoder pour reconstruire la frame à partir de la précédente. Le backend
    /// Windows/DXGI y **fusionne** deux sources (voir `win::read_damage`) :
    /// - les **régions redessinées** (`IDXGIOutputDuplication::GetFrameDirtyRects`) ;
    /// - la **destination des blocs déplacés** (`GetFrameMoveRects` — défilement,
    ///   fenêtre déplacée) : le contenu a été copié *vers* ce rectangle, il y est
    ///   donc neuf par rapport à la frame précédente.
    ///
    /// Les recouvrements ne sont pas dédupliqués (ré-encoder deux fois un pixel est
    /// idempotent, et les listes DXGI sont courtes). **`dirty` vide ⇔ rien n'a
    /// changé** : une source qui renseigne fidèlement ce champ (DXGI) permet à
    /// nd-codec d'activer le mode delta *par défaut* (saut des trames inchangées +
    /// conversion couleur restreinte). Les sources qui ne détectent pas encore les
    /// dommages (X11 `GetImage`, CoreGraphics) renvoient **un unique rectangle plein
    /// cadre** — jamais vide sur changement — pour rester correctes sans delta.
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

    /// Restreint la capture à une **sous-région** du moniteur — le « cadre d'écran »
    /// (*screen frame*) qu'AnyDesk laisse délimiter pour ne partager qu'une portion
    /// de l'écran.
    ///
    /// `region` est en **pixels, dans l'espace du moniteur** (origine au coin
    /// haut-gauche) ; elle est **bornée** aux dimensions réelles au moment de la
    /// capture. `None` (défaut) = plein écran. Peut être appelée avant ou après
    /// [`start`](Self::start) : l'effet vaut pour la frame suivante. Les frames
    /// renvoyées ont alors `width`/`height` égales à la sous-région, et `dirty`
    /// (comme `cursor`) est exprimé dans ses coordonnées.
    ///
    /// Ajout **rétro-compatible** : plutôt qu'un nouveau champ de [`CaptureConfig`]
    /// — qui casserait les littéraux `CaptureConfig { .. }` déjà écrits chez nd-core
    /// —, l'option passe par cette méthode à implémentation par défaut. Le défaut
    /// honore `None` (plein écran) et renvoie `NotImplemented` pour toute sous-région
    /// qu'un backend ne sait pas restreindre : jamais de fuite silencieuse de la
    /// zone hors-cadre (garantie de confidentialité).
    fn set_region(&mut self, region: Option<Rect>) -> Result<()> {
        match region {
            None => Ok(()),
            Some(_) => Err(nd_proto::NdError::NotImplemented(
                "ScreenCapturer::set_region (sous-région non gérée par ce backend)",
            )),
        }
    }
}

/// Crée le capteur adapté à la plateforme courante.
///
/// Windows : DXGI Desktop Duplication. macOS : CoreGraphics
/// (`CGDisplayCreateImage` ; ScreenCaptureKit au jet suivant). Linux : X11
/// `GetImage` via `x11rb` ; en session **Wayland pure**, le backend PipeWire +
/// portail `xdg-desktop-portal` est utilisé si la fonction `wayland-pipewire` est
/// activée (sinon `NotImplemented`, voir `linux_pipewire`).
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
        linux::create_capturer()
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

    /// `clamp_region` : plein cadre par défaut, rognage dans les bornes, jamais vide,
    /// jamais d'agrandissement au plein écran sur origine hors cadre. Ces invariants
    /// protègent la garantie de confidentialité du « cadre d'écran » sur **tous** les
    /// backends (DXGI, CoreGraphics, X11) puisque la fonction est partagée.
    #[test]
    fn clamp_region_borne_sans_deborder() {
        // None → plein cadre.
        assert_eq!(clamp_region(None, 1920, 1080), (0, 0, 1920, 1080));
        // Région interne inchangée.
        assert_eq!(
            clamp_region(
                Some(Rect {
                    x: 100,
                    y: 50,
                    w: 640,
                    h: 480
                }),
                1920,
                1080
            ),
            (100, 50, 640, 480)
        );
        // Débordement droite/bas → rogné à la limite.
        assert_eq!(
            clamp_region(
                Some(Rect {
                    x: 1800,
                    y: 1000,
                    w: 400,
                    h: 400
                }),
                1920,
                1080
            ),
            (1800, 1000, 120, 80)
        );
        // Origine hors cadre → ramenée au dernier pixel, largeur/hauteur ≥ 1
        // (pas de repli plein écran : la zone hors-cadre ne fuite pas).
        assert_eq!(
            clamp_region(
                Some(Rect {
                    x: 5000,
                    y: 5000,
                    w: 10,
                    h: 10
                }),
                1920,
                1080
            ),
            (1919, 1079, 1, 1)
        );
        // Région de dimensions nulles → jamais vide (1×1 au point demandé).
        assert_eq!(
            clamp_region(
                Some(Rect {
                    x: 10,
                    y: 20,
                    w: 0,
                    h: 0
                }),
                1920,
                1080
            ),
            (10, 20, 1, 1)
        );
        // Cadre dégénéré (0×0) : renvoyé tel quel, sans soustraction débordante.
        assert_eq!(
            clamp_region(
                Some(Rect {
                    x: 1,
                    y: 1,
                    w: 1,
                    h: 1
                }),
                0,
                0
            ),
            (0, 0, 0, 0)
        );
    }

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
