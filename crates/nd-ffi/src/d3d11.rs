//! Rendu vidéo **zéro-copie D3D11** (Windows) : surface GPU **partagée** composée
//! directement par Flutter, alimentée par une conversion **NV12 → RGBA sur GPU**.
//!
//! # Pourquoi ce module (et pas `irondash_texture`)
//!
//! La texture historique ([`crate::texture`]) est une **PixelBuffer** : Flutter
//! téléverse chaque trame **RGBA** depuis un `Vec<u8>` CPU. `irondash_texture`
//! 0.5 *décrit* bien un mode surface GPU D3D11 (`BoxedTextureDescriptor`,
//! `d3d11texture2d_callback`…), **mais n'expose aucun constructeur public** pour
//! lui sous Windows : seul `BoxedPixelData` implémente
//! `PlatformTextureWithProvider`, et `PlatformTexture::new` est privé. La règle
//! d'orphelin interdit d'ajouter l'`impl` manquant depuis `nd-ffi`.
//!
//! On enregistre donc la texture **directement auprès de l'embedder Flutter** :
//! le registre de textures (`FlutterDesktopTextureRegistrarRef`) est obtenu via
//! `irondash_engine_context` (déjà dépendance), et les trois entrées
//! `FlutterDesktopTextureRegistrar*` sont résolues dans `flutter_windows.dll`
//! (`GetModuleHandleA`/`GetProcAddress`). C'est exactement ce que fait
//! `irondash_texture` en interne — on ne réimplémente que la glue manquante.
//!
//! # Chaîne obtenue (et ce qui reste une copie)
//!
//! ```text
//! décodeur openh264 (LOGICIEL)         ← goulot : pas de décodage matériel
//!   └─ I420 (CPU)
//!        └─ re-empaquetage NV12 (CPU, sans arithmétique couleur)   [nd_codec]
//!             └─ 1 UPLOAD CPU→GPU  (UpdateSubresource, 1,5 o/px)   ← seule copie restante
//!                  └─ VideoProcessorBlt NV12→RGBA  (GPU)           ← conversion couleur sur GPU
//!                       └─ ID3D11Texture2D RGBA **partagée** (handle DXGI)
//!                            └─ Flutter échantillonne la surface GPU directement (zéro-copie compo)
//! ```
//!
//! Gain vs PixelBuffer : (a) la conversion couleur YUV→RGB passe du **CPU au
//! GPU** ; (b) l'upload CPU→GPU tombe de **4 o/px (RGBA) à 1,5 o/px (NV12)**, soit
//! **2,7× moins d'octets** ; (c) Flutter ne **recopie plus** un tampon CPU par
//! trame — il compose la texture GPU directement (plus de `decodeImageFromPixels`,
//! plus de marshalling des pixels par le pont FRB). **Copie restante** : l'unique
//! upload NV12 CPU→GPU, **inévitable tant que le décodeur est logiciel** (le vrai
//! zéro-copie total exigerait un décodeur matériel D3D11 sortant une
//! `ID3D11Texture2D` NV12 — voir plan 03/16, hors de ce lot).
//!
//! # Repli
//!
//! Toute défaillance ici (pas de GPU, symboles embedder absents, VideoProcessor
//! indisponible…) fait échouer [`TextureD3d11::creer`] : [`crate::texture`]
//! retombe alors sur la texture **PixelBuffer** (upload RGBA), et à défaut sur le
//! flux RGBA CPU historique (`decodeImageFromPixels`). Aucun chemin n'est retiré.
//!
//! # Threads & sûreté
//!
//! * [`TextureD3d11::creer`] s'exécute sur le **thread plateforme**
//!   (`EngineContext::get` l'exige ; `nd_texture_init` y satisfait).
//! * [`TextureD3d11::pousser_frame`] s'exécute sur le **thread de drainage vidéo**
//!   ([`crate::flux`]) : tout l'accès au *device*/contexte D3D11 (non thread-safe)
//!   est sérialisé derrière un [`Mutex`]. Le *device* D3D11 est « free-threaded ».
//! * Le **rappel de surface** (`gpu_surface_callback`) est invoqué par le **thread
//!   de rastérisation** de Flutter : il ne touche **aucun** objet COM — il lit
//!   seulement le handle partagé + les dimensions publiés dans [`EtatSurface`]
//!   (atomiques). D'où l'`unsafe impl Send + Sync` (mêmes garanties que
//!   `nd_codec::mediafoundation`).
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use irondash_engine_context::EngineContext;
use nd_codec::DecodedFrame;
use windows::core::{s, Interface};
use windows::Win32::Foundation::{BOOL, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_RESOURCE_MISC_SHARED, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
// Symboles employés uniquement par la lecture de contrôle (`lire_sortie_rgba`,
// réservée aux tests) : gardés sous `cfg(test)` pour ne pas être « importés mais
// inutilisés » dans la cible bibliothèque (repli GPU sans lecture CPU).
#[cfg(test)]
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

// ---------------------------------------------------------------------------
// ABI de l'embedder Flutter (registre de textures « surface GPU »).
//
// Copie fidèle des définitions de `flutter_texture_registrar.h` (ABI stable),
// restreinte à ce dont on se sert (surface GPU par handle DXGI partagé). Mêmes
// structures que celles employées en interne par `irondash_texture`.
// ---------------------------------------------------------------------------

/// `kFlutterDesktopGpuSurfaceTexture` : la texture est une surface GPU.
const TYPE_TEXTURE_SURFACE_GPU: u32 = 1;
/// `kFlutterDesktopGpuSurfaceTypeDxgiSharedHandle` : le handle est un handle
/// **DXGI partagé** (`IDXGIResource::GetSharedHandle`).
const TYPE_SURFACE_DXGI_HANDLE_PARTAGE: u32 = 1;
/// `kFlutterDesktopPixelFormatRGBA8888` : format des pixels de la surface.
const FORMAT_PIXEL_RGBA8888: u32 = 1;

/// Descripteur d'une surface GPU rendu au rappel de Flutter (par trame).
#[repr(C)]
struct DescripteurSurfaceGpu {
    struct_size: usize,
    handle: *mut c_void,
    width: usize,
    height: usize,
    visible_width: usize,
    visible_height: usize,
    format: u32,
    release_callback: Option<unsafe extern "C" fn(*mut c_void)>,
    release_context: *mut c_void,
}

/// Configuration « surface GPU » d'une texture externe.
#[repr(C)]
struct ConfigSurfaceGpu {
    struct_size: usize,
    type_: u32,
    callback:
        Option<unsafe extern "C" fn(usize, usize, *mut c_void) -> *const DescripteurSurfaceGpu>,
    user_data: *mut c_void,
}

/// Infos d'enregistrement d'une texture externe (variante surface GPU seule ; la
/// variante active de l'union C occupe le même offset, cf. doc de module).
#[repr(C)]
struct InfoTexture {
    type_: u32,
    config: ConfigSurfaceGpu,
}

/// `FlutterDesktopTextureRegistrarRef` (opaque).
type RegistrarRef = *mut c_void;
type FnRegister = unsafe extern "C" fn(RegistrarRef, *const InfoTexture) -> i64;
type FnUnregister =
    unsafe extern "C" fn(RegistrarRef, i64, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void);
type FnMark = unsafe extern "C" fn(RegistrarRef, i64) -> bool;

/// Résout les trois entrées du registre de textures dans `flutter_windows.dll`
/// (déjà chargée dans le processus de l'app). `None` si la DLL ou un symbole
/// manque (repli PixelBuffer).
fn resoudre_fonctions_embedder() -> Option<(FnRegister, FnUnregister, FnMark)> {
    // SAFETY : résolution de symboles par nom dans un module déjà chargé ; les
    // pointeurs obtenus sont transmutés vers les signatures exactes de l'API C
    // stable de l'embedder (mêmes que `irondash_texture`).
    unsafe {
        let module = GetModuleHandleA(s!("flutter_windows.dll")).ok()?;
        let reg = GetProcAddress(
            module,
            s!("FlutterDesktopTextureRegistrarRegisterExternalTexture"),
        )?;
        let unreg = GetProcAddress(
            module,
            s!("FlutterDesktopTextureRegistrarUnregisterExternalTexture"),
        )?;
        let mark = GetProcAddress(
            module,
            s!("FlutterDesktopTextureRegistrarMarkExternalTextureFrameAvailable"),
        )?;
        Some((
            std::mem::transmute::<unsafe extern "system" fn() -> isize, FnRegister>(reg),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, FnUnregister>(unreg),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, FnMark>(mark),
        ))
    }
}

/// Rappel invoqué par le thread de rastérisation de Flutter pour obtenir la
/// surface GPU à composer. Ne touche aucun objet COM : lit le handle partagé et
/// les dimensions publiés (atomiques) et alloue un descripteur que Flutter
/// libère via [`liberer_descripteur`].
///
/// SAFETY : `user_data` est le `*const EtatSurface` obtenu par `Arc::into_raw`
/// lors de l'enregistrement ; il reste valide jusqu'au désenregistrement
/// (l'`Arc` correspondant n'est relâché que dans [`liberer_etat`]).
unsafe extern "C" fn gpu_surface_callback(
    _width: usize,
    _height: usize,
    user_data: *mut c_void,
) -> *const DescripteurSurfaceGpu {
    let etat = &*(user_data as *const EtatSurface);
    let handle = etat.handle.load(Ordering::Acquire);
    let largeur = etat.largeur.load(Ordering::Acquire) as usize;
    let hauteur = etat.hauteur.load(Ordering::Acquire) as usize;

    let descripteur = Box::new(DescripteurSurfaceGpu {
        struct_size: size_of::<DescripteurSurfaceGpu>(),
        handle: handle as *mut c_void,
        width: largeur,
        height: hauteur,
        visible_width: largeur,
        visible_height: hauteur,
        format: FORMAT_PIXEL_RGBA8888,
        release_callback: Some(liberer_descripteur),
        release_context: std::ptr::null_mut(),
    });
    let brut = Box::into_raw(descripteur);
    (*brut).release_context = brut as *mut c_void;
    brut as *const _
}

/// Libère le descripteur alloué par [`gpu_surface_callback`] (appelé par Flutter
/// quand il en a fini avec la trame).
///
/// SAFETY : `ctx` est le `release_context` posé par [`gpu_surface_callback`],
/// c'est-à-dire le `Box<DescripteurSurfaceGpu>` fuité juste avant.
unsafe extern "C" fn liberer_descripteur(ctx: *mut c_void) {
    drop(Box::from_raw(ctx as *mut DescripteurSurfaceGpu));
}

/// Libère l'`Arc<EtatSurface>` confié à Flutter à l'enregistrement (appelé par
/// l'embedder après le désenregistrement de la texture).
///
/// SAFETY : `user_data` est le pointeur d'`Arc::into_raw` passé à
/// `RegisterExternalTexture` puis à `UnregisterExternalTexture` ; il n'est
/// consommé qu'une fois (au désenregistrement).
unsafe extern "C" fn liberer_etat(user_data: *mut c_void) {
    drop(Arc::from_raw(user_data as *const EtatSurface));
}

// ---------------------------------------------------------------------------
// État partagé lu par le rappel de Flutter (sans verrou, atomiques).
// ---------------------------------------------------------------------------

/// Handle DXGI partagé + dimensions de la surface courante, publiés par le
/// thread de drainage et lus par le thread de rastérisation de Flutter.
struct EtatSurface {
    /// Handle DXGI partagé courant (`0` tant qu'aucune surface n'existe).
    handle: AtomicIsize,
    largeur: AtomicU32,
    hauteur: AtomicU32,
}

impl EtatSurface {
    fn new() -> Self {
        Self {
            handle: AtomicIsize::new(0),
            largeur: AtomicU32::new(0),
            hauteur: AtomicU32::new(0),
        }
    }

    /// Publie le handle + dimensions (ordre `Release` : le handle en dernier, de
    /// sorte que le rappel ne le voie jamais avec des dimensions périmées).
    fn publier(&self, handle: HANDLE, largeur: u32, hauteur: u32) {
        self.largeur.store(largeur, Ordering::Release);
        self.hauteur.store(hauteur, Ordering::Release);
        self.handle.store(handle.0 as isize, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Pont D3D11 : device + VideoProcessor + surface partagée redimensionnable.
// ---------------------------------------------------------------------------

/// Ressources dépendant de la résolution (recréées à chaque changement de taille).
struct RessourcesTaille {
    largeur: u32,
    hauteur: u32,
    /// Texture NV12 d'entrée (téléversée par trame via `UpdateSubresource`).
    nv12: ID3D11Texture2D,
    /// Texture RGBA de sortie, **partagée** (composée par Flutter).
    sortie: ID3D11Texture2D,
    /// Handle DXGI partagé de [`Self::sortie`] (passé à Flutter).
    handle: HANDLE,
    /// Vue d'entrée NV12 et vue de sortie RGBA du VideoProcessor.
    vue_entree: ID3D11VideoProcessorInputView,
    vue_sortie: ID3D11VideoProcessorOutputView,
    processeur: ID3D11VideoProcessor,
}

/// Device D3D11 + pipeline de conversion NV12→RGBA, propre à une texture.
struct PontVideoD3d11 {
    device: ID3D11Device,
    contexte: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_contexte: ID3D11VideoContext,
    res: Option<RessourcesTaille>,
}

/// Convertit une erreur `windows` en message lisible.
fn d3d_err(quoi: &str, e: windows::core::Error) -> String {
    format!("D3D11 {quoi} : {e}")
}

impl PontVideoD3d11 {
    /// Crée le device D3D11 matériel (+ interfaces vidéo). Aucune surface encore.
    fn nouveau() -> Result<Self, String> {
        let niveaux = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_0,
        ];
        // `VIDEO_SUPPORT` : requis pour `ID3D11VideoDevice`/VideoProcessor.
        // `BGRA_SUPPORT` : composition/échantillonnage de la surface de sortie.
        let flags = D3D11_CREATE_DEVICE_FLAG(
            D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
        );
        let mut device: Option<ID3D11Device> = None;
        let mut contexte: Option<ID3D11DeviceContext> = None;
        // SAFETY : appel FFI standard ; pointeurs de sortie valides, pas de module
        // logiciel (driver matériel).
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&niveaux),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut contexte),
            )
        }
        .map_err(|e| d3d_err("CreateDevice", e))?;

        let device = device.ok_or_else(|| "D3D11 : device nul".to_owned())?;
        let contexte = contexte.ok_or_else(|| "D3D11 : contexte nul".to_owned())?;
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(|e| d3d_err("cast VideoDevice", e))?;
        let video_contexte: ID3D11VideoContext = contexte
            .cast()
            .map_err(|e| d3d_err("cast VideoContext", e))?;

        Ok(Self {
            device,
            contexte,
            video_device,
            video_contexte,
            res: None,
        })
    }

    /// (Re)construit les ressources pour `largeur`×`hauteur` si la taille a changé.
    fn preparer(&mut self, largeur: u32, hauteur: u32) -> Result<(), String> {
        if let Some(res) = self.res.as_ref() {
            if res.largeur == largeur && res.hauteur == hauteur {
                return Ok(());
            }
        }
        // Libère l'ancienne taille avant d'allouer la nouvelle.
        self.res = None;

        let nv12 = self.creer_texture_nv12(largeur, hauteur)?;
        let sortie = self.creer_texture_sortie(largeur, hauteur)?;
        let handle = handle_partage(&sortie)?;

        // VideoProcessor : énumérateur (décrit l'entrée/sortie) → processeur.
        let contenu = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: largeur,
            InputHeight: hauteur,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: largeur,
            OutputHeight: hauteur,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY : `contenu` est valide ; l'énumérateur/processeur sont renvoyés
        // par valeur COM.
        let enumerateur: ID3D11VideoProcessorEnumerator =
            unsafe { self.video_device.CreateVideoProcessorEnumerator(&contenu) }
                .map_err(|e| d3d_err("CreateVideoProcessorEnumerator", e))?;
        // SAFETY : idem ; index de conversion 0.
        let processeur = unsafe { self.video_device.CreateVideoProcessor(&enumerateur, 0) }
            .map_err(|e| d3d_err("CreateVideoProcessor", e))?;

        let vue_entree = self.creer_vue_entree(&nv12, &enumerateur)?;
        let vue_sortie = self.creer_vue_sortie(&sortie, &enumerateur)?;

        // Espaces colorimétriques : entrée NV12 **BT.601 pleine plage** (comme
        // `nd_codec`), sortie RGB pleine plage. Bits de
        // `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` : `Nominal_Range = 1` (0-255) en
        // bits 4-5 ⇒ 0x10 ; `YCbCr_Matrix = 0` (BT.601), `RGB_Range = 0` (pleine).
        let espace = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x10 };
        // SAFETY : le processeur vient d'être créé ; réglages best-effort (sans
        // valeur de retour) honorés par le pilote.
        unsafe {
            self.video_contexte
                .VideoProcessorSetStreamColorSpace(&processeur, 0, &espace);
            self.video_contexte
                .VideoProcessorSetOutputColorSpace(&processeur, &espace);
        }

        self.res = Some(RessourcesTaille {
            largeur,
            hauteur,
            nv12,
            sortie,
            handle,
            vue_entree,
            vue_sortie,
            processeur,
        });
        Ok(())
    }

    /// Crée la texture NV12 d'entrée (mise à jour par `UpdateSubresource`).
    fn creer_texture_nv12(&self, largeur: u32, hauteur: u32) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: largeur,
            Height: hauteur,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY : `desc` valide ; pas de données initiales ; sortie valide.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| d3d_err("CreateTexture2D(NV12)", e))?;
        tex.ok_or_else(|| "D3D11 : texture NV12 nulle".to_owned())
    }

    /// Crée la texture RGBA de sortie **partagée** (rendue par le VideoProcessor,
    /// composée par Flutter).
    fn creer_texture_sortie(&self, largeur: u32, hauteur: u32) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: largeur,
            Height: hauteur,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            // Partageable : Flutter ouvre le handle DXGI sur SON device.
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY : `desc` valide ; sortie valide.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| d3d_err("CreateTexture2D(sortie)", e))?;
        tex.ok_or_else(|| "D3D11 : texture de sortie nulle".to_owned())
    }

    /// Crée la vue d'entrée NV12 du VideoProcessor.
    fn creer_vue_entree(
        &self,
        nv12: &ID3D11Texture2D,
        enumerateur: &ID3D11VideoProcessorEnumerator,
    ) -> Result<ID3D11VideoProcessorInputView, String> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut vue: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY : `nv12` et `enumerateur` valides ; `desc` décrit la sous-ressource 0.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                nv12,
                enumerateur,
                &desc,
                Some(&mut vue),
            )
        }
        .map_err(|e| d3d_err("CreateVideoProcessorInputView", e))?;
        vue.ok_or_else(|| "D3D11 : vue d'entrée nulle".to_owned())
    }

    /// Crée la vue de sortie RGBA du VideoProcessor.
    fn creer_vue_sortie(
        &self,
        sortie: &ID3D11Texture2D,
        enumerateur: &ID3D11VideoProcessorEnumerator,
    ) -> Result<ID3D11VideoProcessorOutputView, String> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut vue: Option<ID3D11VideoProcessorOutputView> = None;
        // SAFETY : `sortie` et `enumerateur` valides ; `desc` décrit le mip 0.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                sortie,
                enumerateur,
                &desc,
                Some(&mut vue),
            )
        }
        .map_err(|e| d3d_err("CreateVideoProcessorOutputView", e))?;
        vue.ok_or_else(|| "D3D11 : vue de sortie nulle".to_owned())
    }

    /// Téléverse un tampon **NV12** (plan Y + plan UV entrelacé, contigu, pas =
    /// largeur) et le convertit sur GPU dans la surface partagée. Renvoie le
    /// handle DXGI partagé courant.
    fn pousser_nv12(&mut self, nv12: &[u8], largeur: u32, hauteur: u32) -> Result<HANDLE, String> {
        let attendu = (largeur as usize) * (hauteur as usize) * 3 / 2;
        if nv12.len() < attendu {
            return Err(format!(
                "NV12 trop court : {} < {attendu} ({largeur}×{hauteur})",
                nv12.len()
            ));
        }
        self.preparer(largeur, hauteur)?;
        let res = self.res.as_ref().expect("ressources préparées");

        // Upload NV12 CPU→GPU : `UpdateSubresource` lit le plan Y (hauteur lignes)
        // puis le plan UV (hauteur/2 lignes) à partir du même pointeur, au pas
        // `largeur` — exactement la disposition de notre tampon NV12 contigu.
        // SAFETY : `res.nv12` a la taille `largeur×hauteur` NV12 ; le tampon source
        // fait au moins `attendu` octets (vérifié), pas source = `largeur`.
        unsafe {
            self.contexte.UpdateSubresource(
                &res.nv12,
                0,
                None,
                nv12.as_ptr() as *const c_void,
                largeur,
                0,
            );
        }

        self.convertir(res)?;
        Ok(res.handle)
    }

    /// Téléverse directement un tampon **RGBA** dans la surface partagée (repli
    /// pour une trame déjà en RGBA : aucune conversion GPU nécessaire). Renvoie le
    /// handle DXGI partagé courant.
    fn pousser_rgba(&mut self, rgba: &[u8], largeur: u32, hauteur: u32) -> Result<HANDLE, String> {
        let attendu = (largeur as usize) * (hauteur as usize) * 4;
        if rgba.len() < attendu {
            return Err(format!(
                "RGBA trop court : {} < {attendu} ({largeur}×{hauteur})",
                rgba.len()
            ));
        }
        self.preparer(largeur, hauteur)?;
        let res = self.res.as_ref().expect("ressources préparées");
        // SAFETY : `res.sortie` est R8G8B8A8 `largeur×hauteur` ; source ≥ `attendu`
        // octets (vérifié), pas source = `largeur*4`.
        unsafe {
            self.contexte.UpdateSubresource(
                &res.sortie,
                0,
                None,
                rgba.as_ptr() as *const c_void,
                largeur * 4,
                0,
            );
            self.contexte.Flush();
        }
        Ok(res.handle)
    }

    /// Exécute la conversion NV12→RGBA sur GPU (`VideoProcessorBlt`) puis pousse
    /// le travail au GPU (`Flush`) pour que la surface soit prête à être composée.
    fn convertir(&self, res: &RessourcesTaille) -> Result<(), String> {
        let mut flux = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            // Clone COM (AddRef) — relâché juste après le Blt.
            pInputSurface: ManuallyDrop::new(Some(res.vue_entree.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        // SAFETY : le processeur et les vues appartiennent aux mêmes ressources ;
        // `flux` vit jusqu'après l'appel, un seul flux d'entrée.
        let resultat = unsafe {
            self.video_contexte.VideoProcessorBlt(
                &res.processeur,
                &res.vue_sortie,
                0,
                std::slice::from_ref(&flux),
            )
        };
        // Relâche l'AddRef du clone (évite une fuite COM par trame).
        // SAFETY : `pInputSurface` porte le clone posé ci-dessus ; plus relu ensuite.
        unsafe { ManuallyDrop::drop(&mut flux.pInputSurface) };
        resultat.map_err(|e| d3d_err("VideoProcessorBlt", e))?;
        // SAFETY : soumet le travail GPU en attente.
        unsafe { self.contexte.Flush() };
        Ok(())
    }

    /// Lit la surface RGBA de sortie en mémoire CPU (via une texture de *staging*).
    /// **Réservé aux tests** : prouve la conversion GPU sans app Flutter.
    #[cfg(test)]
    fn lire_sortie_rgba(&self) -> Result<Vec<u8>, String> {
        let res = self.res.as_ref().ok_or("aucune surface préparée")?;
        let (largeur, hauteur) = (res.largeur, res.hauteur);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: largeur,
            Height: hauteur,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY : `desc` valide ; sortie valide.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .map_err(|e| d3d_err("CreateTexture2D(staging)", e))?;
        let staging = staging.ok_or("staging nul")?;
        // SAFETY : `staging` et `res.sortie` ont mêmes dimensions/format.
        unsafe { self.contexte.CopyResource(&staging, &res.sortie) };

        let mut mappe = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY : `staging` est mappable en lecture (STAGING + CPU_ACCESS_READ).
        unsafe {
            self.contexte
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mappe))
        }
        .map_err(|e| d3d_err("Map(staging)", e))?;

        let largeur_octets = largeur as usize * 4;
        let pas = mappe.RowPitch as usize;
        let base = mappe.pData as *const u8;
        let mut rgba = vec![0u8; largeur_octets * hauteur as usize];
        for (y, ligne) in rgba.chunks_mut(largeur_octets).enumerate() {
            // SAFETY : chaque ligne source fait `largeur_octets` octets à l'offset
            // `y*pas` dans un tampon de `pas*hauteur` octets fourni par `Map`.
            let src = unsafe { std::slice::from_raw_parts(base.add(y * pas), largeur_octets) };
            ligne.copy_from_slice(src);
        }
        // SAFETY : `staging` a été mappé juste au-dessus.
        unsafe { self.contexte.Unmap(&staging, 0) };
        Ok(rgba)
    }
}

/// Obtient le handle DXGI partagé d'une texture (`IDXGIResource::GetSharedHandle`).
fn handle_partage(tex: &ID3D11Texture2D) -> Result<HANDLE, String> {
    let ressource: IDXGIResource = tex.cast().map_err(|e| d3d_err("cast IDXGIResource", e))?;
    // SAFETY : `ressource` référence une texture créée avec `MISC_SHARED`.
    unsafe { ressource.GetSharedHandle() }.map_err(|e| d3d_err("GetSharedHandle", e))
}

// ---------------------------------------------------------------------------
// Texture externe D3D11 enregistrée auprès de l'embedder Flutter.
// ---------------------------------------------------------------------------

/// Texture **zéro-copie D3D11** vivante : identifiant Flutter, registre, pont de
/// conversion et état partagé lu par le rappel de rastérisation.
pub(crate) struct TextureD3d11 {
    id: i64,
    registrar: RegistrarRef,
    unregister: FnUnregister,
    mark: FnMark,
    /// Pointeur `Arc::into_raw(EtatSurface)` confié à Flutter (libéré au Drop).
    user_data: *mut c_void,
    etat: Arc<EtatSurface>,
    /// Pipeline D3D11 (accès sérialisé : thread de drainage uniquement).
    pont: Mutex<PontVideoD3d11>,
}

// SAFETY : tout accès aux objets COM (device/contexte/VideoProcessor, non
// thread-safe) est sérialisé par `pont: Mutex<…>` et n'a lieu que sur le thread
// de drainage vidéo ; le device D3D11 est « free-threaded » (pointeurs COM
// déplaçables entre threads). Le rappel de Flutter ne lit que `etat` (atomiques)
// et ne touche aucun COM. `registrar`/`user_data`/pointeurs de fonctions sont des
// handles de l'embedder, valides pour toute la durée de vie de la texture.
unsafe impl Send for TextureD3d11 {}
unsafe impl Sync for TextureD3d11 {}

impl TextureD3d11 {
    /// Crée une surface GPU partagée et l'enregistre comme texture externe du
    /// moteur `engine_handle`. **À appeler sur le thread plateforme.**
    pub(crate) fn creer(engine_handle: i64) -> Result<Self, String> {
        let (register, unregister, mark) =
            resoudre_fonctions_embedder().ok_or("symboles flutter_windows.dll introuvables")?;
        let registrar = EngineContext::get()
            .map_err(|e| format!("EngineContext indisponible : {e:?}"))?
            .get_texture_registry(engine_handle)
            .map_err(|e| format!("registre de textures indisponible : {e:?}"))?;

        let mut pont = PontVideoD3d11::nouveau()?;
        // Surface initiale 2×2 noire : le handle est valide dès l'enregistrement
        // (le rappel de Flutter ne renvoie jamais un handle nul).
        let nv12_noir = [0u8, 0, 0, 0, 128, 128];
        let handle = pont.pousser_nv12(&nv12_noir, 2, 2)?;

        let etat = Arc::new(EtatSurface::new());
        etat.publier(handle, 2, 2);
        // Un compte de référence confié à Flutter (relâché dans `liberer_etat`).
        let user_data = Arc::into_raw(Arc::clone(&etat)) as *mut c_void;

        let info = InfoTexture {
            type_: TYPE_TEXTURE_SURFACE_GPU,
            config: ConfigSurfaceGpu {
                struct_size: size_of::<ConfigSurfaceGpu>(),
                type_: TYPE_SURFACE_DXGI_HANDLE_PARTAGE,
                callback: Some(gpu_surface_callback),
                user_data,
            },
        };
        // SAFETY : `registrar` valide (obtenu ci-dessus) ; `info` vit jusqu'après
        // l'appel (l'embedder en copie le contenu) ; `user_data` reste valide
        // jusqu'au désenregistrement.
        let id = unsafe { register(registrar, &info) };
        if id < 0 {
            // Reprend le compte de référence fuité (enregistrement échoué).
            // SAFETY : `user_data` vient d'`Arc::into_raw` ci-dessus, non confié.
            unsafe { drop(Arc::from_raw(user_data as *const EtatSurface)) };
            return Err("RegisterExternalTexture a échoué".to_owned());
        }

        Ok(Self {
            id,
            registrar,
            unregister,
            mark,
            user_data,
            etat,
            pont: Mutex::new(pont),
        })
    }

    /// Identifiant de texture Flutter.
    pub(crate) fn id(&self) -> i64 {
        self.id
    }

    /// Achemine une trame décodée vers la surface GPU (NV12 → conversion GPU, ou
    /// RGBA → upload direct), puis signale « image disponible » à Flutter. Une
    /// trame sans pixels (répétition delta) est ignorée (l'image reste affichée).
    pub(crate) fn pousser_frame(&self, frame: &DecodedFrame) -> Result<(), String> {
        let handle = {
            let mut pont = self.pont.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(nv12) = frame.nv12.as_ref() {
                pont.pousser_nv12(nv12, frame.width, frame.height)?
            } else if !frame.rgba.is_empty() {
                pont.pousser_rgba(&frame.rgba, frame.width, frame.height)?
            } else {
                return Ok(());
            }
        };
        self.etat.publier(handle, frame.width, frame.height);
        // Signale la nouvelle trame. La fonction de l'embedder est sûre à appeler
        // depuis n'importe quel thread (elle poste vers le thread de rastérisation).
        // SAFETY : `registrar`/`id` sont ceux de cette texture, vivante ici.
        unsafe { (self.mark)(self.registrar, self.id) };
        Ok(())
    }
}

impl Drop for TextureD3d11 {
    fn drop(&mut self) {
        // Désenregistre la texture ; l'embedder libère ensuite l'`Arc<EtatSurface>`
        // via `liberer_etat`.
        // SAFETY : `registrar`/`id` valides ; `user_data` = le pointeur confié à
        // l'enregistrement, consommé une seule fois par `liberer_etat`.
        unsafe {
            (self.unregister)(self.registrar, self.id, Some(liberer_etat), self.user_data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fabrique un tampon NV12 `largeur×hauteur` : dégradé de luminance
    /// horizontal + chroma variable (bandes), pour exercer la conversion couleur.
    fn nv12_test(largeur: u32, hauteur: u32) -> Vec<u8> {
        let (l, h) = (largeur as usize, hauteur as usize);
        let mut nv12 = vec![0u8; l * h + l * h / 2];
        for y in 0..h {
            for x in 0..l {
                nv12[y * l + x] = ((x * 255) / l.max(1)) as u8; // Y : dégradé
            }
        }
        let uv = &mut nv12[l * h..];
        for by in 0..h / 2 {
            for bx in 0..l / 2 {
                let o = by * l + bx * 2;
                uv[o] = ((bx * 255) / (l / 2).max(1)) as u8; // U
                uv[o + 1] = ((by * 255) / (h / 2).max(1)) as u8; // V
            }
        }
        nv12
    }

    /// La conversion NV12→RGBA **sur GPU** (VideoProcessor D3D11) est fidèle à la
    /// conversion CPU de référence ([`nd_codec::nv12_vers_rgba`], BT.601 pleine
    /// plage) : PSNR luma élevé, et un aplat gris neutre reste ~128 (preuve du
    /// traitement en **pleine plage**). Sur une machine sans GPU/VideoProcessor,
    /// le test se **saute** proprement (comme l'énumération DXGI de `nd-capture`).
    #[test]
    fn conversion_nv12_gpu_fidele_au_cpu() {
        let mut pont = match PontVideoD3d11::nouveau() {
            Ok(p) => p,
            Err(_) => return, // pas de GPU (CI headless) → saut
        };
        let (largeur, hauteur) = (64u32, 64u32);
        let nv12 = nv12_test(largeur, hauteur);
        if pont.pousser_nv12(&nv12, largeur, hauteur).is_err() {
            return; // VideoProcessor indisponible → saut
        }
        let rgba_gpu = match pont.lire_sortie_rgba() {
            Ok(v) => v,
            Err(_) => return,
        };
        let rgba_cpu = nd_codec::nv12_vers_rgba(&nv12, largeur, hauteur).expect("référence CPU");
        assert_eq!(rgba_gpu.len(), rgba_cpu.len(), "tailles RGBA identiques");

        let psnr = nd_codec::psnr_luma(&rgba_cpu, &rgba_gpu).expect("psnr");
        assert!(
            psnr > 20.0,
            "conversion NV12→RGBA GPU incohérente vs CPU (PSNR luma = {psnr:.1} dB)"
        );

        // Aplat gris neutre (Y=U=V=128) : indépendant de la matrice couleur, il
        // valide le traitement en **pleine plage** (Y=128 ⇒ RGB ~128).
        let gris = vec![128u8; (largeur * hauteur + largeur * hauteur / 2) as usize];
        pont.pousser_nv12(&gris, largeur, hauteur)
            .expect("push gris");
        let rgba_gris = pont.lire_sortie_rgba().expect("lecture gris");
        for px in rgba_gris.chunks_exact(4).take(16) {
            for (canal, &v) in px[..3].iter().enumerate() {
                assert!(
                    (i32::from(v) - 128).abs() <= 6,
                    "gris neutre pleine plage attendu ~128, canal {canal} = {v}"
                );
            }
            assert_eq!(px[3], 255, "alpha opaque");
        }
    }
}
