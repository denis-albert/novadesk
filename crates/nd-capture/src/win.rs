//! Implémentation Windows de [`ScreenCapturer`] via **DXGI Desktop Duplication**.
//!
//! Séquence par frame : `AcquireNextFrame` → copie (éventuellement restreinte à une
//! sous-région, [`ScreenCapturer::set_region`]) de la texture du bureau vers une
//! texture de *staging* CPU-lisible → lecture des **régions modifiées** — dommages
//! (`GetFrameDirtyRects`) **et** destinations des blocs déplacés (`GetFrameMoveRects`,
//! défilement / fenêtre déplacée), fusionnés dans [`CapturedFrame::dirty`] → `Map` /
//! lecture des pixels → `ReleaseFrame`. Voir plan 02 §Windows.
//!
//! Ce module concentre tout le `unsafe` FFI de la capture Windows ; il est isolé
//! derrière le trait pour que le reste du moteur reste 100 % sûr.
#![allow(unsafe_code)]

use std::time::Instant;

use nd_proto::{MonitorId, NdError, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
};

use crate::{
    CaptureConfig, CaptureEvent, CapturedFrame, CursorState, FrameImage, MonitorInfo, PixelFormat,
    Rect, ScreenCapturer,
};

/// Convertit une erreur `windows` en `NdError::Capture` (partagé avec [`crate::win_cursor`]).
pub(crate) fn cap(e: windows::core::Error) -> NdError {
    NdError::Capture(e.to_string())
}

/// Borne la sous-région `region` (« cadre d'écran ») à un cadre `w`×`h` et renvoie
/// `(x, y, largeur, hauteur)` **toujours non vide et dans les bornes**.
///
/// `None` ⇒ plein cadre `(0, 0, w, h)`. Une région partiellement hors cadre est
/// rognée ; une origine hors cadre est ramenée au dernier pixel valide (jamais
/// d'agrandissement au plein écran — la zone hors-cadre ne doit pas fuiter).
fn clamp_region(region: Option<Rect>, w: u32, h: u32) -> (u32, u32, u32, u32) {
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

/// Intersecte un `RECT` DXGI (coordonnées moniteur) avec la fenêtre de capture
/// `[ox, ox+rw) × [oy, oy+rh)` et le **translate** dans le repère de la sous-région.
/// Renvoie `None` si l'intersection est vide. Calcul en `i64` (les bords DXGI
/// peuvent être négatifs ou déborder après addition).
fn clip_rect_into_region(r: &RECT, ox: u32, oy: u32, rw: u32, rh: u32) -> Option<Rect> {
    let (ox, oy) = (i64::from(ox), i64::from(oy));
    let (rx1, ry1) = (ox + i64::from(rw), oy + i64::from(rh));
    let l = i64::from(r.left).max(ox);
    let t = i64::from(r.top).max(oy);
    let right = i64::from(r.right).min(rx1);
    let bottom = i64::from(r.bottom).min(ry1);
    if right <= l || bottom <= t {
        return None;
    }
    Some(Rect {
        x: (l - ox) as u32,
        y: (t - oy) as u32,
        w: (right - l) as u32,
        h: (bottom - t) as u32,
    })
}

/// Énumère les moniteurs via DXGI : `IDXGIFactory1` → adaptateur 0 → `EnumOutputs`.
///
/// L'adaptateur 0 est celui que `D3D11CreateDevice(adapter = None)` sélectionne, donc
/// l'index de sortie ici est EXACTEMENT celui que [`DxgiCapturer`] passe à
/// `EnumOutputs` dans `init_duplication` : `MonitorId(i)` ⇔ sortie DXGI `i`.
pub(crate) fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    // SAFETY : appel FFI standard ; la fabrique est renvoyée par valeur COM.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(cap)?;
    // Même adaptateur que le device D3D11 du capteur (le premier énuméré).
    // SAFETY : index 0 toujours valide s'il existe au moins un adaptateur.
    let adapter = unsafe { factory.EnumAdapters1(0) }.map_err(cap)?;

    let mut monitors = Vec::new();
    let mut index = 0u32;
    loop {
        // SAFETY : appel FFI ; `DXGI_ERROR_NOT_FOUND` signale la fin de l'énumération.
        let output = match unsafe { adapter.EnumOutputs(index) } {
            Ok(o) => o,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(cap(e)),
        };
        // SAFETY : appel FFI sans effet de bord ; renvoie la description par valeur.
        let desc = unsafe { output.GetDesc() }.map_err(cap)?;

        // On garde l'index DXGI comme identifiant même si une sortie détachée est
        // sautée, pour préserver la correspondance `MonitorId(i)` ⇔ `EnumOutputs(i)`.
        if desc.AttachedToDesktop.as_bool() {
            let rc = desc.DesktopCoordinates;
            let name_len = desc
                .DeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.DeviceName.len());
            monitors.push(MonitorInfo {
                id: MonitorId(index),
                name: String::from_utf16_lossy(&desc.DeviceName[..name_len]),
                width: (rc.right - rc.left).max(0) as u32,
                height: (rc.bottom - rc.top).max(0) as u32,
                x: rc.left,
                y: rc.top,
                // Le moniteur principal a, par définition Windows, son coin
                // haut-gauche à l'origine (0, 0) du bureau virtuel.
                is_primary: rc.left == 0 && rc.top == 0,
            });
        }
        index += 1;
    }
    Ok(monitors)
}

/// Capteur d'écran Windows fondé sur DXGI Desktop Duplication.
pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: Option<IDXGIOutputDuplication>,
    width: u32,
    height: u32,
    output_index: u32,
    monitor: MonitorId,
    capture_cursor: bool,
    /// Sous-région partagée (« cadre d'écran »), en pixels moniteur ; `None` = plein
    /// écran. Bornée aux dimensions réelles à chaque frame (voir [`clamp_region`]).
    region: Option<Rect>,
    start: Instant,
}

impl DxgiCapturer {
    /// Crée le device D3D11. La duplication est initialisée dans [`ScreenCapturer::start`].
    pub fn new() -> Result<Self> {
        let feature_levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY : appel FFI standard ; les pointeurs de sortie sont des `Option`
        // valides et le driver matériel n'exige pas de module logiciel.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(cap)?;

        let device = device.ok_or_else(|| NdError::Capture("device D3D11 nul".into()))?;
        let context = context.ok_or_else(|| NdError::Capture("contexte D3D11 nul".into()))?;

        Ok(Self {
            device,
            context,
            dupl: None,
            width: 0,
            height: 0,
            output_index: 0,
            monitor: MonitorId(0),
            capture_cursor: false,
            region: None,
            start: Instant::now(),
        })
    }

    /// (Ré)initialise la duplication de sortie pour l'index de moniteur donné.
    fn init_duplication(&mut self, output_index: u32) -> Result<()> {
        let dxgi_device: IDXGIDevice = self.device.cast().map_err(cap)?;
        // SAFETY : `dxgi_device` provient du même device ; index de sortie validé par l'API.
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.map_err(cap)?;
        let output = unsafe { adapter.EnumOutputs(output_index) }.map_err(cap)?;
        let output1: IDXGIOutput1 = output.cast().map_err(cap)?;
        let dupl = unsafe { output1.DuplicateOutput(&self.device) }.map_err(cap)?;

        // SAFETY : appel FFI sans effet de bord ; renvoie la description par valeur.
        let desc = unsafe { dupl.GetDesc() };
        self.width = desc.ModeDesc.Width;
        self.height = desc.ModeDesc.Height;
        self.output_index = output_index;
        self.dupl = Some(dupl);
        Ok(())
    }

    /// Frame « vide » : rien n'a changé depuis la dernière capture. Ses dimensions
    /// suivent la sous-région active (cohérence avec les frames pleines).
    fn empty_frame(&self) -> CapturedFrame {
        let (_, _, rw, rh) = clamp_region(self.region, self.width, self.height);
        CapturedFrame {
            width: rw,
            height: rh,
            monitor: self.monitor,
            format: PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: self.start.elapsed().as_micros() as u64,
            image: None,
        }
    }

    /// Lit **toutes** les régions modifiées de la frame courante et les fusionne dans
    /// une liste [`Rect`] en coordonnées de la sous-région `[ox, oy, rw, rh]` :
    ///
    /// 1. **Blocs déplacés** (`GetFrameMoveRects`) : on retient la `DestinationRect`
    ///    de chaque déplacement — le contenu y est neuf (défilement, fenêtre bougée).
    /// 2. **Dommages** (`GetFrameDirtyRects`) : régions redessinées.
    ///
    /// Chaque rectangle est intersecté avec la sous-région et translaté ; ceux qui en
    /// sortent sont ignorés. Les deux listes sont lues dans des tampons **typés**
    /// (donc correctement alignés — pas d'accès non aligné), chacun dimensionné à
    /// partir de `TotalMetadataBufferSize` (borne haute commune aux deux listes).
    ///
    /// **Correction pour le delta** : si la frame a bien été *présentée*
    /// (`LastPresentTime != 0`) mais qu'aucune métadonnée de rectangle n'est fournie,
    /// on renvoie un plein cadre (sous-région entière) — une trame présentée ne doit
    /// jamais avoir un `dirty` vide, sous peine d'être sautée à tort par nd-codec.
    fn read_damage(
        &self,
        dupl: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        ox: u32,
        oy: u32,
        rw: u32,
        rh: u32,
    ) -> Vec<Rect> {
        let bytes = info.TotalMetadataBufferSize as usize;
        let mut out = Vec::new();
        // Nombre de rectangles bruts rapportés par DXGI (avant intersection) : sert à
        // distinguer « aucune métadonnée » de « métadonnée entièrement hors sous-région ».
        let mut raw_count = 0usize;

        if bytes > 0 {
            // 1. Blocs déplacés — chaque élément fait `size_of::<DXGI_OUTDUPL_MOVE_RECT>()`.
            let sz_move = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
            let cap_move = bytes / sz_move;
            if cap_move > 0 {
                let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); cap_move];
                let mut required = 0u32;
                // SAFETY : `moves` est un tampon typé de `cap_move` éléments ; l'API y
                // écrit au plus `cap_move * sz_move` octets et renseigne `required`.
                let res = unsafe {
                    dupl.GetFrameMoveRects(
                        (cap_move * sz_move) as u32,
                        moves.as_mut_ptr(),
                        &mut required,
                    )
                };
                if res.is_ok() {
                    let n = (required as usize / sz_move).min(cap_move);
                    raw_count += n;
                    for m in &moves[..n] {
                        if let Some(rc) = clip_rect_into_region(&m.DestinationRect, ox, oy, rw, rh)
                        {
                            out.push(rc);
                        }
                    }
                }
            }

            // 2. Dommages — chaque élément fait `size_of::<RECT>()`.
            let sz_dirty = std::mem::size_of::<RECT>();
            let cap_dirty = bytes / sz_dirty;
            if cap_dirty > 0 {
                let mut dirties = vec![RECT::default(); cap_dirty];
                let mut required = 0u32;
                // SAFETY : `dirties` est un tampon typé de `cap_dirty` éléments ; l'API y
                // écrit au plus `cap_dirty * sz_dirty` octets et renseigne `required`.
                let res = unsafe {
                    dupl.GetFrameDirtyRects(
                        (cap_dirty * sz_dirty) as u32,
                        dirties.as_mut_ptr(),
                        &mut required,
                    )
                };
                if res.is_ok() {
                    let n = (required as usize / sz_dirty).min(cap_dirty);
                    raw_count += n;
                    for r in &dirties[..n] {
                        if let Some(rc) = clip_rect_into_region(r, ox, oy, rw, rh) {
                            out.push(rc);
                        }
                    }
                }
            }
        }

        // Present réel sans aucun rectangle exploitable → plein cadre conservateur.
        // (Si des rectangles existaient mais tombaient tous hors sous-région, `out`
        // reste vide à raison : la sous-région, elle, n'a pas changé.)
        if info.LastPresentTime != 0 && raw_count == 0 {
            out.push(Rect {
                x: 0,
                y: 0,
                w: rw,
                h: rh,
            });
        }
        out
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn start(&mut self, cfg: CaptureConfig) -> Result<()> {
        self.capture_cursor = cfg.capture_cursor;
        self.monitor = cfg.monitor;
        self.init_duplication(cfg.monitor.0)
    }

    fn next_frame(&mut self) -> Result<CapturedFrame> {
        // Clone du handle (AddRef COM) pour ne pas emprunter `self` pendant le travail.
        let dupl = self
            .dupl
            .as_ref()
            .ok_or_else(|| NdError::Capture("capture non démarrée (appeler start)".into()))?
            .clone();

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY : buffers de sortie valides ; délai de 100 ms.
        let acq = unsafe { dupl.AcquireNextFrame(100, &mut info, &mut resource) };
        if let Err(e) = acq {
            let code = e.code();
            if code == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(self.empty_frame());
            }
            if code == DXGI_ERROR_ACCESS_LOST {
                let idx = self.output_index;
                self.init_duplication(idx)?;
                return Ok(self.empty_frame());
            }
            return Err(NdError::Capture(format!("AcquireNextFrame : {e}")));
        }

        // Traitement de la frame acquise ; `ReleaseFrame` est appelé quoi qu'il arrive.
        let outcome = self.process_acquired(&dupl, &info, resource);
        // SAFETY : une frame a bien été acquise ci-dessus.
        let _ = unsafe { dupl.ReleaseFrame() };
        outcome
    }

    fn poll_event(&mut self) -> Option<CaptureEvent> {
        None
    }

    fn stop(&mut self) {
        self.dupl = None;
    }

    /// Fixe le « cadre d'écran ». Aucune (ré)initialisation : la sous-région est
    /// simplement rognée aux dimensions réelles à la frame suivante ([`clamp_region`]).
    fn set_region(&mut self, region: Option<Rect>) -> Result<()> {
        self.region = region;
        Ok(())
    }
}

impl DxgiCapturer {
    /// Copie la texture acquise vers une texture de staging et lit les pixels CPU.
    fn process_acquired(
        &self,
        dupl: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<CapturedFrame> {
        let resource =
            resource.ok_or_else(|| NdError::Capture("ressource de frame nulle".into()))?;
        let frame_tex: ID3D11Texture2D = resource.cast().map_err(cap)?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY : `desc` est un buffer de sortie valide.
        unsafe { frame_tex.GetDesc(&mut desc) };
        let w = desc.Width;
        let h = desc.Height;

        // Sous-région partagée (« cadre d'écran »), bornée aux dimensions réelles.
        let (ox, oy, rw, rh) = clamp_region(self.region, w, h);
        let plein_cadre = ox == 0 && oy == 0 && rw == w && rh == h;

        // Texture de staging CPU-lisible, aux dimensions de la sous-région : on ne
        // recopie (et ne lit) que la zone partagée.
        let mut sdesc = desc;
        sdesc.Width = rw;
        sdesc.Height = rh;
        sdesc.Usage = D3D11_USAGE_STAGING;
        sdesc.BindFlags = 0;
        sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        sdesc.MiscFlags = 0;

        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY : `sdesc` valide ; pas de données initiales ; pointeur de sortie valide.
        unsafe {
            self.device
                .CreateTexture2D(&sdesc, None, Some(&mut staging))
        }
        .map_err(cap)?;
        let staging = staging.ok_or_else(|| NdError::Capture("texture de staging nulle".into()))?;

        if plein_cadre {
            // SAFETY : source et destination sont des textures compatibles (mêmes dims).
            unsafe { self.context.CopyResource(&staging, &frame_tex) };
        } else {
            // Recopie du seul rectangle `[ox, oy, rw, rh]` du bureau vers (0, 0).
            let boite = D3D11_BOX {
                left: ox,
                top: oy,
                front: 0,
                right: ox + rw,
                bottom: oy + rh,
                back: 1,
            };
            // SAFETY : `boite` est incluse dans la texture source ; la destination
            // (staging) mesure exactement `rw`×`rh` ; sous-ressource 0 dans les deux.
            unsafe {
                self.context.CopySubresourceRegion(
                    &staging,
                    0,
                    0,
                    0,
                    0,
                    &frame_tex,
                    0,
                    Some(&boite),
                );
            }
        }

        let dirty = self.read_damage(dupl, info, ox, oy, rw, rh);

        // Position du curseur ramenée dans le repère de la sous-région ; visible
        // seulement s'il tombe dedans (pas de fuite hors cadre).
        let cursor = if self.capture_cursor && info.PointerPosition.Visible.as_bool() {
            let cx = info.PointerPosition.Position.x - ox as i32;
            let cy = info.PointerPosition.Position.y - oy as i32;
            Some(CursorState {
                x: cx,
                y: cy,
                visible: cx >= 0 && cy >= 0 && (cx as u32) < rw && (cy as u32) < rh,
            })
        } else {
            None
        };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY : `staging` est mappable en lecture (USAGE_STAGING + CPU_ACCESS_READ).
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(cap)?;

        let row_bytes = rw as usize * 4;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        let mut data = vec![0u8; row_bytes * rh as usize];
        for (y, row) in data.chunks_mut(row_bytes).enumerate() {
            // SAFETY : chaque ligne source fait `row_bytes` octets, à l'offset `y*pitch`
            // dans un buffer de `pitch * rh` octets fourni par `Map`.
            let src = unsafe { std::slice::from_raw_parts(base.add(y * pitch), row_bytes) };
            row.copy_from_slice(src);
        }

        // SAFETY : `staging` a été mappé juste au-dessus.
        unsafe { self.context.Unmap(&staging, 0) };

        Ok(CapturedFrame {
            width: rw,
            height: rh,
            monitor: self.monitor,
            format: PixelFormat::Bgra8,
            dirty,
            cursor,
            timestamp_us: self.start.elapsed().as_micros() as u64,
            image: Some(FrameImage::Cpu {
                data,
                stride: row_bytes,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les invariants de l'énumération : identifiants croissants (index DXGI),
    /// dimensions non nulles et un seul moniteur principal.
    #[test]
    fn enumeration_moniteurs_coherente() {
        // Sur une machine sans bureau (session de service), l'énumération peut
        // échouer ou être vide : on ne valide les invariants que si elle aboutit.
        let Ok(monitors) = enumerate_monitors() else {
            return;
        };
        let mut prev: Option<u32> = None;
        for m in &monitors {
            if let Some(p) = prev {
                assert!(m.id.0 > p, "identifiants non strictement croissants");
            }
            prev = Some(m.id.0);
            assert!(m.width > 0 && m.height > 0, "dimensions nulles pour {m:?}");
            assert!(!m.name.is_empty(), "nom vide pour {m:?}");
        }
        let primaries = monitors.iter().filter(|m| m.is_primary).count();
        assert!(
            monitors.is_empty() || primaries == 1,
            "exactement un moniteur principal attendu, trouvé {primaries}"
        );
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// `clamp_region` : plein cadre par défaut, rognage dans les bornes, jamais vide,
    /// jamais d'agrandissement au plein écran sur origine hors cadre.
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
    }

    /// `clip_rect_into_region` : intersection + translation dans le repère de la
    /// sous-région ; `None` hors cadre ; bords négatifs/débordants bornés.
    #[test]
    fn clip_rect_translate_dans_la_region() {
        // Sous-région [100,100, 200x200]. Rect moniteur [150,150, 400,400] →
        // intersection [150,150,300,300] → translaté (50,50, 150x150).
        assert_eq!(
            clip_rect_into_region(&rect(150, 150, 400, 400), 100, 100, 200, 200),
            Some(Rect {
                x: 50,
                y: 50,
                w: 150,
                h: 150
            })
        );
        // Rect englobant toute la sous-région → sous-région entière (0,0,rw,rh).
        assert_eq!(
            clip_rect_into_region(&rect(0, 0, 10000, 10000), 100, 100, 200, 200),
            Some(Rect {
                x: 0,
                y: 0,
                w: 200,
                h: 200
            })
        );
        // Rect entièrement hors sous-région → None.
        assert_eq!(
            clip_rect_into_region(&rect(0, 0, 50, 50), 100, 100, 200, 200),
            None
        );
        // Bords négatifs bornés (l'intersection démarre à l'origine de la région).
        assert_eq!(
            clip_rect_into_region(&rect(-40, -40, 120, 120), 0, 0, 200, 200),
            Some(Rect {
                x: 0,
                y: 0,
                w: 120,
                h: 120
            })
        );
    }
}
