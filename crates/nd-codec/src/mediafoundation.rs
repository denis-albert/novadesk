//! Backend H.264 via **Windows Media Foundation** (plan 03 « matériel d'abord »).
//!
//! ## Choix du MFT : synchrone logiciel d'abord
//!
//! Ce premier jet instancie le MFT **synchrone logiciel** de Microsoft
//! (`CLSID_MSH264EncoderMFT`, `mfh264enc.dll`, présent sur tout Windows de bureau)
//! plutôt que le MFT **matériel** (NVENC/AMF/QSV exposé par MF). Raison : les MFT
//! matériels sont *asynchrones* (modèle à événements `METransformNeedInput` /
//! `METransformHaveOutput`, déverrouillage `MF_TRANSFORM_ASYNC_UNLOCK`, boucle
//! `IMFMediaEventGenerator`) et demandent une machinerie nettement plus lourde pour
//! un résultat identique côté API. Le pipeline synchrone ci-dessous
//! (`ProcessInput`/`ProcessOutput`) est fiable et suffit à valider le flux ; le
//! passage au MFT matériel (RTX 4080 → NVENC) se fera derrière le même trait
//! [`VideoEncoder`] sans changer les appelants (voir plan 03/16).
//!
//! ## Pipeline par frame
//!
//! BGRA (depuis [`FrameImage::Cpu`]) → conversion **NV12** (BT.601, plage limitée)
//! → `IMFSample` → `ProcessInput` → boucle `ProcessOutput` → octets NAL (Annex B).
//! L'image-clé est détectée via l'attribut `MFSampleExtension_CleanPoint` de
//! l'échantillon de sortie, et forcée via `ICodecAPI` (`AVEncVideoForceKeyFrame`).
//!
//! Ce module concentre tout le `unsafe` FFI Media Foundation ; il est isolé derrière
//! le trait pour que le reste du moteur reste 100 % sûr (même approche que
//! `nd-capture::win`).
#![allow(unsafe_code)]

use std::mem::ManuallyDrop;

use nd_capture::{CapturedFrame, FrameImage};
use nd_proto::{NdError, Result};
use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, CLSID_MSH264EncoderMFT,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFSample,
    IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFSTARTUP_NOSOCKET, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::delta::{aire_totale, rects_pairs_bornes, RectPair, SuiviDelta};
use crate::{CodecCaps, CodecKind, EncodedChunk, EncoderConfig, VideoEncoder};

/// Convertit une erreur `windows` en `NdError::Codec` avec un contexte lisible.
fn mf_err(quoi: &str, e: windows::core::Error) -> NdError {
    NdError::Codec(format!("Media Foundation, {quoi} : {e}"))
}

/// Emballe deux `u32` en un `u64` (convention MF pour `MF_MT_FRAME_SIZE`
/// — largeur en poids fort — et `MF_MT_FRAME_RATE` — numérateur en poids fort).
fn emballer_u64(haut: u32, bas: u32) -> u64 {
    (u64::from(haut) << 32) | u64::from(bas)
}

/// Garde d'initialisation Media Foundation : `MFStartup` apparié à `MFShutdown`
/// dans `Drop` (compteur *processus*, donc sûr quel que soit le thread de drop).
///
/// COM (`CoInitializeEx`) est initialisé au moment de créer le MFT — voir
/// [`initialiser_com`] — et volontairement **non** décompté : `CoUninitialize` doit
/// être appelé sur le thread initialisateur, or l'encodeur est `Send` et peut être
/// libéré ailleurs. Laisser COM initialisé pour la durée de vie du thread de travail
/// est la pratique recommandée et sans fuite notable.
struct MfRuntime;

impl MfRuntime {
    fn new() -> Result<Self> {
        // SAFETY : appel FFI d'initialisation ; MFSTARTUP_NOSOCKET suffit (pas de
        // fonctionnalités réseau MF). Apparié à MFShutdown dans Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| mf_err("MFStartup", e))?;
        Ok(Self)
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        // SAFETY : apparie le MFStartup réussi de `new` (compteur processus MF).
        let _ = unsafe { MFShutdown() };
    }
}

/// Initialise COM (MTA) sur le thread courant si nécessaire.
///
/// `S_OK`/`S_FALSE` : initialisé (on ne décompte pas, voir [`MfRuntime`]).
/// `RPC_E_CHANGED_MODE` : le thread est déjà en STA — COM est utilisable tel quel.
fn initialiser_com() -> Result<()> {
    // SAFETY : appel FFI standard, sans paramètre réservé.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        return Err(NdError::Codec(format!(
            "Media Foundation, CoInitializeEx : {hr}"
        )));
    }
    Ok(())
}

/// Convertit une image BGRA (avec `stride` octets par ligne) en NV12
/// (BT.601, plage limitée : Y ∈ [16, 235], U/V ∈ [16, 240]).
///
/// NV12 : plan Y (`w*h` octets) suivi du plan UV entrelacé (`w*h/2` octets), chroma
/// sous-échantillonnée 2×2 (moyenne du bloc). `w` et `h` doivent être pairs.
/// Chemin scalaire ; SIMD/conversion GPU = optimisation future (plan 03).
fn bgra_vers_nv12(bgra: &[u8], stride: usize, w: usize, h: usize, nv12: &mut Vec<u8>) {
    nv12.clear();
    nv12.resize(w * h + w * h / 2, 0);
    bgra_vers_nv12_rect(bgra, stride, w, h, nv12, RectPair::plein(w, h));
}

/// Convertit le rectangle `r` (aligné pair, borné — contrat [`RectPair`]) du tampon
/// BGRA vers le tampon NV12 persistant `nv12` (déjà dimensionné à `w*h*3/2`).
/// C'est le cœur de la **conversion partielle** du mode delta : seule la surface
/// annoncée modifiée est reconvertie, le reste du plan NV12 est conservé tel quel.
fn bgra_vers_nv12_rect(
    bgra: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    nv12: &mut [u8],
    r: RectPair,
) {
    let (plan_y, plan_uv) = nv12.split_at_mut(w * h);

    for y in r.y..r.y + r.h {
        let ligne = &bgra[y * stride + r.x * 4..y * stride + (r.x + r.w) * 4];
        let dst = &mut plan_y[y * w + r.x..y * w + r.x + r.w];
        for (px, dy) in ligne.chunks_exact(4).zip(dst.iter_mut()) {
            let (b, g, rr) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
            *dy = (((66 * rr + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }

    // Chroma : moyenne RGB de chaque bloc 2×2, puis U/V (un couple par bloc).
    for by in r.y / 2..(r.y + r.h) / 2 {
        let dst = &mut plan_uv[by * w..(by + 1) * w];
        for bx in r.x / 2..(r.x + r.w) / 2 {
            let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                let off = (by * 2 + dy) * stride + bx * 2 * 4;
                for dx in 0..2 {
                    let px = &bgra[off + dx * 4..off + dx * 4 + 3];
                    sb += i32::from(px[0]);
                    sg += i32::from(px[1]);
                    sr += i32::from(px[2]);
                }
            }
            let (b, g, rr) = (sb / 4, sg / 4, sr / 4);
            dst[bx * 2] = ((((-38 * rr - 74 * g + 112 * b) + 128) >> 8) + 128) as u8;
            dst[bx * 2 + 1] = ((((112 * rr - 94 * g - 18 * b) + 128) >> 8) + 128) as u8;
        }
    }
}

/// Encodeur H.264 fondé sur un MFT Media Foundation (voir doc de module).
pub struct MediaFoundationEncoder {
    /// Garde MFStartup/MFShutdown (doit survivre au MFT → déclaré avant lui).
    _runtime: MfRuntime,
    /// MFT H.264, instancié par [`VideoEncoder::configure`].
    mft: Option<IMFTransform>,
    /// Réglage fin du codec (débit à chaud, image-clé forcée) ; `None` si le MFT
    /// n'expose pas `ICodecAPI` (on continue alors en mode dégradé documenté).
    codec_api: Option<ICodecAPI>,
    cfg: Option<EncoderConfig>,
    /// Durée d'une frame en unités MF (100 ns).
    duree_frame_100ns: i64,
    /// Taille conseillée du tampon de sortie (`GetOutputStreamInfo.cbSize`).
    taille_sortie: usize,
    /// `true` si le MFT alloue lui-même ses échantillons de sortie (cas des MFT
    /// matériels ; le MFT logiciel MS ne le fait pas, mais on gère les deux).
    fournit_echantillons: bool,
    /// Nombre de frames soumises (horodatages d'entrée monotones réguliers).
    frames_soumises: u64,
    /// Tampon NV12 réutilisé entre les frames (évite une allocation par frame).
    /// En mode delta, il sert de **canevas persistant** : seules les régions
    /// modifiées sont reconverties (voir [`bgra_vers_nv12_rect`]).
    nv12: Vec<u8>,
    /// Vrai si `nv12` contient une image complète de la configuration courante
    /// (une conversion pleine a eu lieu depuis le dernier `configure`).
    nv12_valide: bool,
    /// État du mode delta (saut de trames, image-clé après repos) — voir `delta`.
    suivi: SuiviDelta,
}

// SAFETY : le MFT H.264 logiciel de Microsoft est un objet COM « both/free-threaded »
// créé ici en contexte MTA ; les pointeurs COM qu'il expose peuvent être déplacés
// entre threads du MTA, et l'accès est exclusif (`&mut self`) via le trait. COM
// n'est jamais décompté au drop (voir [`MfRuntime`]), donc aucune opération liée au
// thread d'origine n'a lieu à la libération.
unsafe impl Send for MediaFoundationEncoder {}

impl MediaFoundationEncoder {
    /// Prépare l'environnement Media Foundation. Le MFT est créé par `configure`
    /// (il dépend de la résolution/du débit).
    pub fn new() -> Result<Self> {
        Ok(Self {
            _runtime: MfRuntime::new()?,
            mft: None,
            codec_api: None,
            cfg: None,
            duree_frame_100ns: 0,
            taille_sortie: 0,
            fournit_echantillons: false,
            frames_soumises: 0,
            nv12: Vec::new(),
            nv12_valide: false,
            suivi: SuiviDelta::new(),
        })
    }

    /// Applique un réglage `ICodecAPI` en mode meilleur effort : les propriétés non
    /// prises en charge par un MFT donné ne doivent pas faire échouer l'encodage.
    fn regler_codec_api(&self, propriete: &windows::core::GUID, valeur: &VARIANT) {
        if let Some(api) = self.codec_api.as_ref() {
            // SAFETY : `propriete` et `valeur` sont des références valides ; l'appel
            // est sans effet si la propriété n'est pas supportée (erreur ignorée).
            let _ = unsafe { api.SetValue(propriete, valeur) };
        }
    }

    /// Construit un `IMFSample` NV12 à partir de `self.nv12` (horodatage courant),
    /// et avance le compteur de frames soumises.
    fn creer_sample_nv12(&mut self) -> Result<IMFSample> {
        let horodatage = self.frames_soumises as i64 * self.duree_frame_100ns;
        self.frames_soumises += 1;

        // SAFETY : appels FFI de construction ; `len` est la taille exacte allouée,
        // le Lock/Unlock encadre strictement la copie dans le tampon MF.
        unsafe {
            let len = self.nv12.len() as u32;
            let tampon =
                MFCreateMemoryBuffer(len).map_err(|e| mf_err("MFCreateMemoryBuffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            tampon
                .Lock(&mut ptr, None, None)
                .map_err(|e| mf_err("IMFMediaBuffer::Lock", e))?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            tampon
                .Unlock()
                .map_err(|e| mf_err("IMFMediaBuffer::Unlock", e))?;
            tampon
                .SetCurrentLength(len)
                .map_err(|e| mf_err("SetCurrentLength", e))?;

            let sample = MFCreateSample().map_err(|e| mf_err("MFCreateSample", e))?;
            sample
                .AddBuffer(&tampon)
                .map_err(|e| mf_err("IMFSample::AddBuffer", e))?;
            sample
                .SetSampleTime(horodatage)
                .map_err(|e| mf_err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(self.duree_frame_100ns)
                .map_err(|e| mf_err("SetSampleDuration", e))?;
            Ok(sample)
        }
    }

    /// Tente de tirer un échantillon encodé du MFT.
    ///
    /// `Ok(None)` : le MFT demande plus d'entrée (`MF_E_TRANSFORM_NEED_MORE_INPUT`).
    /// Un changement de type de sortie (`MF_E_TRANSFORM_STREAM_CHANGE`) est
    /// renégocié sur place puis l'appel est retenté.
    fn tirer_sortie(&mut self, mft: &IMFTransform) -> Result<Option<(Vec<u8>, bool)>> {
        loop {
            // Échantillon de sortie : alloué par nous, sauf si le MFT fournit le sien.
            let notre_sample = if self.fournit_echantillons {
                None
            } else {
                // SAFETY : construction d'un sample + tampon de la taille conseillée.
                unsafe {
                    let tampon = MFCreateMemoryBuffer(self.taille_sortie as u32)
                        .map_err(|e| mf_err("MFCreateMemoryBuffer (sortie)", e))?;
                    let sample = MFCreateSample().map_err(|e| mf_err("MFCreateSample", e))?;
                    sample
                        .AddBuffer(&tampon)
                        .map_err(|e| mf_err("IMFSample::AddBuffer", e))?;
                    Some(sample)
                }
            };

            let mut sorties = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(notre_sample.clone()),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut statut = 0u32;
            // SAFETY : `sorties` vit jusqu'après l'appel ; les champs ManuallyDrop
            // sont récupérés/libérés juste en dessous quoi qu'il arrive.
            let res = unsafe { mft.ProcessOutput(0, &mut sorties, &mut statut) };

            // Reprend la propriété des COM placés/écrits dans la struct FFI pour ne
            // rien fuiter (le sample fourni par le MFT arrive par ce canal).
            // SAFETY : ProcessOutput est terminé ; ces champs ne sont plus relus.
            let sample_sorti = unsafe { ManuallyDrop::take(&mut sorties[0].pSample) };
            // SAFETY : idem — libère la collection d'événements éventuelle.
            drop(unsafe { ManuallyDrop::take(&mut sorties[0].pEvents) });

            match res {
                Ok(()) => {
                    let sample = sample_sorti.or(notre_sample).ok_or_else(|| {
                        NdError::Codec("Media Foundation : ProcessOutput sans échantillon".into())
                    })?;
                    return Ok(Some(lire_sample_encode(&sample)?));
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Renégociation du type de sortie (rare pour un encodeur, mais
                    // requis par le contrat MFT), puis nouvel essai.
                    // SAFETY : appels FFI ; le type retourné est appliqué tel quel.
                    unsafe {
                        let t = mft
                            .GetOutputAvailableType(0, 0)
                            .map_err(|e| mf_err("GetOutputAvailableType", e))?;
                        mft.SetOutputType(0, &t, 0)
                            .map_err(|e| mf_err("SetOutputType (renégociation)", e))?;
                    }
                }
                Err(e) => return Err(mf_err("ProcessOutput", e)),
            }
        }
    }
}

/// Extrait les octets NAL (Annex B) et l'indicateur d'image-clé d'un échantillon.
fn lire_sample_encode(sample: &IMFSample) -> Result<(Vec<u8>, bool)> {
    // SAFETY : lectures FFI encadrées ; `ptr` est valide pour `len` octets entre
    // Lock et Unlock, et la copie est faite avant Unlock.
    unsafe {
        let tampon = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| mf_err("ConvertToContiguousBuffer", e))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        tampon
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| mf_err("IMFMediaBuffer::Lock (sortie)", e))?;
        let donnees = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        tampon
            .Unlock()
            .map_err(|e| mf_err("IMFMediaBuffer::Unlock (sortie)", e))?;

        // Image-clé : MFSampleExtension_CleanPoint == 1 (absent ⇒ frame delta).
        let image_cle = sample
            .GetUINT32(&MFSampleExtension_CleanPoint)
            .map(|v| v != 0)
            .unwrap_or(false);
        Ok((donnees, image_cle))
    }
}

impl VideoEncoder for MediaFoundationEncoder {
    fn capabilities() -> CodecCaps {
        CodecCaps {
            // Premier jet : MFT *logiciel* MS (voir doc de module) — on l'annonce
            // honnêtement ; passera à `true` avec le MFT matériel asynchrone.
            hardware: false,
            kinds: vec![CodecKind::H264],
            // Limite documentée du H.264 Video Encoder de Microsoft (niveau 5.2).
            max_width: 4096,
            max_height: 2304,
        }
    }

    fn configure(&mut self, cfg: EncoderConfig) -> Result<()> {
        if cfg.kind != CodecKind::H264 {
            return Err(NdError::Codec(
                "backend Media Foundation : H.264 uniquement (plan 03)".into(),
            ));
        }
        if !cfg.width.is_multiple_of(2)
            || !cfg.height.is_multiple_of(2)
            || cfg.width == 0
            || cfg.height == 0
        {
            return Err(NdError::Codec(
                "Media Foundation : dimensions nulles ou impaires non supportées (NV12)".into(),
            ));
        }
        if cfg.max_fps == 0 {
            return Err(NdError::Codec(
                "Media Foundation : max_fps doit être non nul".into(),
            ));
        }

        // Reconfiguration = recréation complète du MFT : chemin simple et fiable
        // (une reconfiguration est rare : changement de résolution/moniteur).
        self.mft = None;
        self.codec_api = None;
        self.frames_soumises = 0;
        // Nouveau flux : canevas NV12 invalide (conversion pleine exigée) et
        // compteurs delta remis à zéro (le mode actif est conservé).
        self.nv12_valide = false;
        self.suivi.reinitialiser();

        initialiser_com()?;

        // SAFETY : appels FFI de configuration, exécutés dans l'ordre imposé par le
        // contrat MFT encodeur (type de SORTIE d'abord, puis type d'entrée, puis
        // notifications de streaming). Chaque sortie d'API est vérifiée.
        let mft: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| mf_err("CoCreateInstance(CLSID_MSH264EncoderMFT)", e))?;

        // Réglage fin (meilleur effort) : CBR + faible latence, pour garantir « une
        // sortie par entrée » (pas de réordonnancement ni de B-frames).
        self.codec_api = mft.cast::<ICodecAPI>().ok();
        self.regler_codec_api(
            &CODECAPI_AVEncCommonRateControlMode,
            &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        self.regler_codec_api(
            &CODECAPI_AVEncCommonMeanBitRate,
            &VARIANT::from(cfg.target_bitrate_kbps.saturating_mul(1000)),
        );
        self.regler_codec_api(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
        // SAFETY : attribut UINT32 optionnel sur le magasin d'attributs du MFT.
        if let Ok(attrs) = unsafe { mft.GetAttributes() } {
            // SAFETY : clé et valeur valides ; meilleur effort.
            let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };
        }

        let taille = emballer_u64(cfg.width, cfg.height);
        let cadence = emballer_u64(cfg.max_fps, 1);

        // SAFETY : construction des types de média ; toutes les clés/valeurs sont
        // des constantes MF valides et chaque HRESULT est vérifié.
        unsafe {
            // Type de sortie : H.264 Baseline progressif au débit demandé.
            let sortie = MFCreateMediaType().map_err(|e| mf_err("MFCreateMediaType", e))?;
            sortie
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| mf_err("MF_MT_MAJOR_TYPE (sortie)", e))?;
            sortie
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| mf_err("MF_MT_SUBTYPE (sortie)", e))?;
            sortie
                .SetUINT32(
                    &MF_MT_AVG_BITRATE,
                    cfg.target_bitrate_kbps.saturating_mul(1000),
                )
                .map_err(|e| mf_err("MF_MT_AVG_BITRATE", e))?;
            sortie
                .SetUINT64(&MF_MT_FRAME_SIZE, taille)
                .map_err(|e| mf_err("MF_MT_FRAME_SIZE (sortie)", e))?;
            sortie
                .SetUINT64(&MF_MT_FRAME_RATE, cadence)
                .map_err(|e| mf_err("MF_MT_FRAME_RATE (sortie)", e))?;
            sortie
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| mf_err("MF_MT_INTERLACE_MODE (sortie)", e))?;
            sortie
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                .map_err(|e| mf_err("MF_MT_MPEG2_PROFILE", e))?;
            mft.SetOutputType(0, &sortie, 0)
                .map_err(|e| mf_err("SetOutputType", e))?;

            // Type d'entrée : NV12 aux mêmes dimensions/cadence.
            let entree = MFCreateMediaType().map_err(|e| mf_err("MFCreateMediaType", e))?;
            entree
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| mf_err("MF_MT_MAJOR_TYPE (entrée)", e))?;
            entree
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| mf_err("MF_MT_SUBTYPE (entrée)", e))?;
            entree
                .SetUINT64(&MF_MT_FRAME_SIZE, taille)
                .map_err(|e| mf_err("MF_MT_FRAME_SIZE (entrée)", e))?;
            entree
                .SetUINT64(&MF_MT_FRAME_RATE, cadence)
                .map_err(|e| mf_err("MF_MT_FRAME_RATE (entrée)", e))?;
            entree
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| mf_err("MF_MT_INTERLACE_MODE (entrée)", e))?;
            mft.SetInputType(0, &entree, 0)
                .map_err(|e| mf_err("SetInputType", e))?;

            // Taille/propriété des échantillons de sortie, puis démarrage du flux.
            let info = mft
                .GetOutputStreamInfo(0)
                .map_err(|e| mf_err("GetOutputStreamInfo", e))?;
            self.fournit_echantillons =
                info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            // Repli si le MFT n'annonce pas de taille : borne large « frame brute ».
            self.taille_sortie = if info.cbSize > 0 {
                info.cbSize as usize
            } else {
                (cfg.width as usize) * (cfg.height as usize) * 4
            };

            // NB : pas de MFT_MESSAGE_COMMAND_FLUSH ici — le MFT vient d'être créé
            // (rien à purger) et l'encodeur H.264 de MS rejette ce message avant le
            // début du streaming (E_FAIL constaté).
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| mf_err("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING", e))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| mf_err("MFT_MESSAGE_NOTIFY_START_OF_STREAM", e))?;
        }

        self.duree_frame_100ns = 10_000_000 / i64::from(cfg.max_fps);
        self.mft = Some(mft);
        self.cfg = Some(cfg);
        Ok(())
    }

    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedChunk> {
        let mft = self
            .mft
            .as_ref()
            .ok_or_else(|| NdError::Codec("encodeur non configuré (appeler configure)".into()))?
            .clone(); // clone COM (AddRef) pour libérer l'emprunt sur `self`
        let cfg = self.cfg.expect("cfg présent si mft présent");

        if frame.width != cfg.width || frame.height != cfg.height {
            return Err(NdError::Codec(format!(
                "frame {}x{} ≠ configuration {}x{} (reconfigurer l'encodeur)",
                frame.width, frame.height, cfg.width, cfg.height
            )));
        }
        let (w, h) = (frame.width as usize, frame.height as usize);
        let compatible = self.nv12_valide && self.nv12.len() == w * h + w * h / 2;

        // Saut d'encodage (mode delta) : rien n'a changé → trame de répétition à
        // données vides, sans conversion ni passage par le MFT (le décodeur la
        // traite comme « pas de nouvelle image », voir `software::Openh264Decoder`).
        if self.suivi.doit_sauter(frame, force_keyframe, compatible) {
            self.suivi.note_saut();
            return Ok(EncodedChunk {
                data: Vec::new(),
                is_keyframe: false,
                monitor: frame.monitor,
                timestamp_us: frame.timestamp_us,
            });
        }

        let Some(FrameImage::Cpu { data, stride }) = frame.image.as_ref() else {
            return Err(NdError::Codec("frame sans pixels CPU à encoder".into()));
        };
        if *stride < w * 4 || data.len() < *stride * h {
            return Err(NdError::Codec(
                "taille de frame incohérente (attendu stride ≥ largeur*4 et stride*hauteur octets)"
                    .into(),
            ));
        }

        // BGRA → NV12 dans le canevas persistant : conversion restreinte aux
        // régions modifiées si le mode delta est actif et le canevas à jour,
        // pleine sinon (voir module `delta`).
        let mut nv12 = std::mem::take(&mut self.nv12);
        let aire_image = (w * h) as u64;
        let mut aire_modifiee = aire_image;
        if self.suivi.actif() && compatible {
            let rects = rects_pairs_bornes(&frame.dirty, frame.width, frame.height);
            aire_modifiee = aire_totale(&rects, aire_image);
            for r in &rects {
                bgra_vers_nv12_rect(data, *stride, w, h, &mut nv12, *r);
            }
        } else {
            bgra_vers_nv12(data, *stride, w, h, &mut nv12);
        }
        self.nv12 = nv12;
        self.nv12_valide = true;

        // Image-clé : demandée par l'appelant, ou resynchronisation adaptative
        // après une longue période statique (voir `delta`).
        let force_cle = force_keyframe
            || (self.suivi.actif() && self.suivi.keyframe_apres_repos(aire_modifiee, aire_image));
        if force_cle {
            // Meilleur effort : s'applique à la prochaine frame soumise.
            self.regler_codec_api(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
        }

        let sample = self.creer_sample_nv12()?;
        // SAFETY : le sample vient d'être construit et appartient à l'appel.
        unsafe { mft.ProcessInput(0, &sample, 0) }.map_err(|e| mf_err("ProcessInput", e))?;

        // En mode faible latence le MFT produit une sortie par entrée ; s'il retient
        // malgré tout ses premières frames, on lui re-soumet la même image NV12
        // (contenu identique, horodatage avancé), de façon bornée. Chaque sortie est
        // restituée dans l'ordre : le flux H.264 reste valide.
        let mut tentatives = 0u32;
        let (donnees, image_cle) = loop {
            if let Some(sortie) = self.tirer_sortie(&mft)? {
                break sortie;
            }
            tentatives += 1;
            if tentatives > 4 {
                return Err(NdError::Codec(
                    "Media Foundation : aucune sortie après 4 entrées (MFT inattendu)".into(),
                ));
            }
            let sample = self.creer_sample_nv12()?;
            // SAFETY : idem ProcessInput ci-dessus.
            unsafe { mft.ProcessInput(0, &sample, 0) }.map_err(|e| mf_err("ProcessInput", e))?;
        };

        self.suivi.note_encodage();

        Ok(EncodedChunk {
            data: donnees,
            is_keyframe: image_cle,
            monitor: frame.monitor,
            timestamp_us: frame.timestamp_us,
        })
    }

    /// Mise à jour du débit à chaud, **meilleur effort consolidé** (pilotée par
    /// l'ABR, plan 03/04) :
    ///
    /// - la consigne (bornée à 1 kbit/s minimum) est d'abord mémorisée dans la
    ///   configuration — une reconfiguration ultérieure (`configure`) et
    ///   l'observabilité la voient donc toujours ;
    /// - puis appliquée via `ICodecAPI::SetValue(AVEncCommonMeanBitRate)` si le
    ///   MFT l'expose. Le MFT H.264 logiciel de Microsoft accepte ce réglage en
    ///   cours de flux (mode CBR posé à `configure`) ; un MFT qui l'ignore reste
    ///   fonctionnel au débit précédent — c'est la **limite documentée** de ce
    ///   backend, sans équivalent du retour d'état d'openh264.
    fn set_target_bitrate(&mut self, kbps: u32) {
        let kbps = kbps.max(1);
        if let Some(cfg) = self.cfg.as_mut() {
            cfg.target_bitrate_kbps = kbps;
        }
        self.regler_codec_api(
            &CODECAPI_AVEncCommonMeanBitRate,
            &VARIANT::from(kbps.saturating_mul(1000)),
        );
    }

    fn set_delta_mode(&mut self, actif: bool) {
        self.suivi.set_actif(actif);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nd_proto::MonitorId;

    /// Frame BGRA synthétique : dégradé horizontal, décalé de `phase` pixels.
    fn frame_test(w: u32, h: u32, phase: u32) -> CapturedFrame {
        let stride = w as usize * 4;
        let mut data = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * stride + x * 4;
                data[o] = ((x as u32 + phase) % 256) as u8; // B
                data[o + 1] = (y % 256) as u8; // G
                data[o + 2] = 128; // R
                data[o + 3] = 255; // A
            }
        }
        CapturedFrame {
            width: w,
            height: h,
            monitor: MonitorId(0),
            format: nd_capture::PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: u64::from(phase) * 16_667,
            image: Some(FrameImage::Cpu { data, stride }),
        }
    }

    /// La conversion NV12 doit produire les constantes BT.601 attendues sur des
    /// aplats connus (noir, blanc, gris neutre).
    #[test]
    fn conversion_nv12_aplats() {
        let cas = [
            ([0u8, 0, 0, 255], 16u8, 128u8, 128u8), // noir
            ([255, 255, 255, 255], 235, 128, 128),  // blanc
            ([128, 128, 128, 255], 126, 128, 128),  // gris moyen
        ];
        for (bgra, y_attendu, u_attendu, v_attendu) in cas {
            let (w, h) = (4usize, 4usize);
            let data: Vec<u8> = bgra.iter().copied().cycle().take(w * h * 4).collect();
            let mut nv12 = Vec::new();
            bgra_vers_nv12(&data, w * 4, w, h, &mut nv12);
            assert_eq!(nv12.len(), w * h + w * h / 2);
            assert!(nv12[..w * h].iter().all(|&v| v == y_attendu), "plan Y");
            let uv = &nv12[w * h..];
            assert!(uv.iter().step_by(2).all(|&v| v == u_attendu), "plan U");
            assert!(
                uv.iter().skip(1).step_by(2).all(|&v| v == v_attendu),
                "plan V"
            );
        }
    }

    /// Aller-retour complet : encode 4 frames via le MFT puis re-décode le flux
    /// avec openh264 pour prouver sa validité (première frame = image-clé).
    #[test]
    fn encode_mf_puis_redecodage_openh264() {
        let (w, h) = (320u32, 240u32);
        let mut enc = MediaFoundationEncoder::new().expect("init Media Foundation");
        enc.configure(EncoderConfig {
            kind: CodecKind::H264,
            width: w,
            height: h,
            target_bitrate_kbps: 2_000,
            max_fps: 30,
        })
        .expect("configure MFT H.264");

        let mut dec = crate::create_decoder(CodecKind::H264).expect("décodeur openh264");
        let mut cle_vue = false;
        let mut decodees = 0;
        for i in 0..4u32 {
            let frame = frame_test(w, h, i * 8);
            let chunk = enc.encode(&frame, i == 0).expect("encode");
            assert!(!chunk.data.is_empty(), "chunk vide");
            if i == 0 {
                assert!(
                    chunk.is_keyframe,
                    "la première frame doit être une image-clé"
                );
            }
            cle_vue |= chunk.is_keyframe;
            if let Some(img) = dec.decode(&chunk).expect("flux H.264 invalide") {
                assert_eq!((img.width, img.height), (w, h));
                decodees += 1;
            }
        }
        assert!(cle_vue);
        assert!(decodees > 0, "aucune frame re-décodée");
    }

    /// Mode delta : une frame sans région modifiée est sautée (trame de répétition
    /// à données vides) ; une frame avec régions est encodée normalement, en ne
    /// reconvertissant que la surface annoncée. Sans mode delta (défaut), rien ne
    /// change (comportement historique).
    #[test]
    fn delta_saut_et_conversion_partielle() {
        let (w, h) = (320u32, 240u32);
        let cfg = EncoderConfig {
            kind: CodecKind::H264,
            width: w,
            height: h,
            target_bitrate_kbps: 2_000,
            max_fps: 30,
        };

        let mut enc = MediaFoundationEncoder::new().expect("init Media Foundation");
        enc.set_delta_mode(true);
        enc.configure(cfg).expect("configure MFT H.264");

        // Frame 1 : conversion pleine + image-clé.
        let mut pleine = frame_test(w, h, 0);
        pleine.dirty = vec![nd_capture::Rect { x: 0, y: 0, w, h }];
        let premiere = enc.encode(&pleine, true).expect("première frame");
        assert!(!premiere.data.is_empty());

        // Frame 2 : aucune région modifiée → trame de répétition, MFT non sollicité.
        let mut statique = frame_test(w, h, 0);
        statique.dirty = Vec::new();
        let saut = enc.encode(&statique, false).expect("saut");
        assert!(saut.data.is_empty(), "trame de répétition attendue");
        assert!(!saut.is_keyframe);

        // Frame 3 : une région annoncée → trame réelle (conversion partielle).
        let mut bouge = frame_test(w, h, 64);
        bouge.dirty = vec![nd_capture::Rect {
            x: 0,
            y: 0,
            w,
            h: 16,
        }];
        let chunk = enc.encode(&bouge, false).expect("frame delta");
        assert!(!chunk.data.is_empty());

        // Sans mode delta : la frame statique est ré-encodée plein cadre.
        let mut enc_plein = MediaFoundationEncoder::new().expect("init Media Foundation");
        enc_plein.configure(cfg).expect("configure MFT H.264");
        enc_plein.encode(&pleine, true).expect("première");
        let chunk = enc_plein.encode(&statique, false).expect("re-encode");
        assert!(
            !chunk.data.is_empty(),
            "sans mode delta, la frame est ré-encodée plein cadre"
        );
    }
}
