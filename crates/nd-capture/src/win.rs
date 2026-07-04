//! Implémentation Windows de [`ScreenCapturer`] via **DXGI Desktop Duplication**.
//!
//! Séquence par frame : `AcquireNextFrame` → copie de la texture du bureau vers une
//! texture de *staging* CPU-lisible → lecture des régions modifiées (`GetFrameDirtyRects`)
//! → `Map`/lecture des pixels → `ReleaseFrame`. Voir plan 02 §Windows.
//!
//! Ce module concentre tout le `unsafe` FFI de la capture Windows ; il est isolé
//! derrière le trait pour que le reste du moteur reste 100 % sûr.
#![allow(unsafe_code)]

use std::time::Instant;

use nd_proto::{NdError, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutputDuplication, IDXGIOutput1, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::{CaptureConfig, CaptureEvent, CapturedFrame, CursorState, FrameImage, PixelFormat, Rect, ScreenCapturer};

/// Convertit une erreur `windows` en `NdError::Capture`.
fn cap(e: windows::core::Error) -> NdError {
    NdError::Capture(e.to_string())
}

/// Capteur d'écran Windows fondé sur DXGI Desktop Duplication.
pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: Option<IDXGIOutputDuplication>,
    width: u32,
    height: u32,
    output_index: u32,
    capture_cursor: bool,
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
            capture_cursor: false,
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

    /// Frame « vide » : rien n'a changé depuis la dernière capture.
    fn empty_frame(&self) -> CapturedFrame {
        CapturedFrame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: self.start.elapsed().as_micros() as u64,
            image: None,
        }
    }

    /// Lit les régions modifiées associées à la frame courante.
    fn read_dirty(&self, dupl: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) -> Vec<Rect> {
        let size = info.TotalMetadataBufferSize;
        let mut dirty = Vec::new();
        if size == 0 {
            return dirty;
        }
        let mut buf = vec![0u8; size as usize];
        let mut required = 0u32;
        // SAFETY : `buf` fait `size` octets ; l'API écrit au plus `size` et renseigne `required`.
        let res = unsafe {
            dupl.GetFrameDirtyRects(size, buf.as_mut_ptr().cast::<RECT>(), &mut required)
        };
        if res.is_err() {
            return dirty;
        }
        let count = required as usize / std::mem::size_of::<RECT>();
        // SAFETY : `buf` contient `count` `RECT` valides écrits par l'API.
        let rects = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<RECT>(), count) };
        for r in rects {
            dirty.push(Rect {
                x: r.left.max(0) as u32,
                y: r.top.max(0) as u32,
                w: (r.right - r.left).max(0) as u32,
                h: (r.bottom - r.top).max(0) as u32,
            });
        }
        dirty
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn start(&mut self, cfg: CaptureConfig) -> Result<()> {
        self.capture_cursor = cfg.capture_cursor;
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
}

impl DxgiCapturer {
    /// Copie la texture acquise vers une texture de staging et lit les pixels CPU.
    fn process_acquired(
        &self,
        dupl: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<CapturedFrame> {
        let resource = resource.ok_or_else(|| NdError::Capture("ressource de frame nulle".into()))?;
        let frame_tex: ID3D11Texture2D = resource.cast().map_err(cap)?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY : `desc` est un buffer de sortie valide.
        unsafe { frame_tex.GetDesc(&mut desc) };
        let w = desc.Width;
        let h = desc.Height;

        // Texture de staging CPU-lisible, même format/dimensions.
        let mut sdesc = desc;
        sdesc.Usage = D3D11_USAGE_STAGING;
        sdesc.BindFlags = 0;
        sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        sdesc.MiscFlags = 0;

        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY : `sdesc` valide ; pas de données initiales ; pointeur de sortie valide.
        unsafe { self.device.CreateTexture2D(&sdesc, None, Some(&mut staging)) }.map_err(cap)?;
        let staging = staging.ok_or_else(|| NdError::Capture("texture de staging nulle".into()))?;

        // SAFETY : source et destination sont des textures compatibles.
        unsafe { self.context.CopyResource(&staging, &frame_tex) };

        let dirty = self.read_dirty(dupl, info);

        let cursor = if self.capture_cursor && info.PointerPosition.Visible.as_bool() {
            Some(CursorState {
                x: info.PointerPosition.Position.x,
                y: info.PointerPosition.Position.y,
                visible: true,
            })
        } else {
            None
        };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY : `staging` est mappable en lecture (USAGE_STAGING + CPU_ACCESS_READ).
        unsafe { self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .map_err(cap)?;

        let row_bytes = w as usize * 4;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        let mut data = vec![0u8; row_bytes * h as usize];
        for (y, row) in data.chunks_mut(row_bytes).enumerate() {
            // SAFETY : chaque ligne source fait `row_bytes` octets, à l'offset `y*pitch`
            // dans un buffer de `pitch * h` octets fourni par `Map`.
            let src = unsafe { std::slice::from_raw_parts(base.add(y * pitch), row_bytes) };
            row.copy_from_slice(src);
        }

        // SAFETY : `staging` a été mappé juste au-dessus.
        unsafe { self.context.Unmap(&staging, 0) };

        Ok(CapturedFrame {
            width: w,
            height: h,
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
