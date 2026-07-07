//! Backend H.264 **matériel** via un MFT Media Foundation asynchrone — sur ce
//! poste, l'encodeur **NVENC** du GPU NVIDIA (plan 03/16 « matériel d'abord »).
//!
//! ## Sélection de l'encodeur
//!
//! `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE |
//! MFT_ENUM_FLAG_SORTANDFILTER)` énumère les encodeurs H.264 **matériels**
//! réellement présents (exposés par le pilote GPU, ex. `nvEncMFTH264.dll`). La
//! sélection préfère l'entrée dont le nom convivial contient « NVIDIA » (mission :
//! parité de performance AnyDesk sur RTX 4080) ; à défaut, le premier encodeur
//! matériel listé (AMD/Intel) est retenu — le nom **exact** est conservé et exposé
//! par [`VideoEncoder::nom_backend`] : on n'annonce jamais « GPU » sans preuve.
//! Aucun droit administrateur requis : tout est en espace utilisateur.
//!
//! ## Protocole MFT asynchrone (contrat des MFT matériels)
//!
//! Contrairement au MFT logiciel synchrone (`mediafoundation`), un MFT matériel
//! impose le modèle à événements :
//!
//! 1. déverrouillage `MF_TRANSFORM_ASYNC_UNLOCK` (sinon toute méthode répond
//!    `MF_E_TRANSFORM_ASYNC_LOCKED`) ;
//! 2. boucle [`IMFMediaEventGenerator`] : chaque `METransformNeedInput` autorise
//!    **un** `ProcessInput`, chaque `METransformHaveOutput` autorise **un**
//!    `ProcessOutput` (crédits comptés ici dans `credits_entree` /
//!    `sorties_pretes`) ;
//! 3. les événements sont tirés en mode non bloquant (`MF_EVENT_FLAG_NO_WAIT`)
//!    avec attente courte bornée — jamais de blocage indéfini : si le matériel se
//!    tait au-delà de [`DELAI_EVENEMENT`], on renvoie une erreur claire.
//!
//! ## Pipeline par frame
//!
//! BGRA → **NV12** (conversion CPU commune à `mediafoundation`, pleine ou
//! restreinte aux régions modifiées en mode delta) → `IMFSample` mémoire système
//! (le MFT NVIDIA téléverse lui-même vers le GPU ; l'import direct de textures
//! D3D11 sans copie est l'optimisation suivante, plan 03) → `ProcessInput` →
//! événements → `ProcessOutput` → octets NAL (Annex B).
//!
//! Si l'encodeur retient ses premières entrées avant de produire (pipeline
//! matériel), la même image est re-soumise de façon **bornée** ([`MAX_REAMORCES`])
//! pour amorcer le flux, et les sorties excédentaires sont conservées dans l'ordre
//! (`sorties_pretes`) — le flux H.264 reste strictement séquentiel, au prix d'au
//! plus [`MAX_REAMORCES`] trames de latence de pipeline (0 constaté avec NVENC en
//! mode faible latence).
//!
//! Ce module concentre le `unsafe` FFI ; il est isolé derrière [`VideoEncoder`].
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nd_capture::{CapturedFrame, FrameImage};
use nd_proto::{NdError, Result};
use windows::core::{Interface, PWSTR, VARIANT};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFTransform,
    MEError, METransformHaveOutput, METransformNeedInput, MFCreateMediaType, MFMediaType_Video,
    MFTEnumEx, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NO_EVENTS_AVAILABLE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::delta::{aire_totale, rects_pairs_bornes, SuiviDelta};
use crate::mediafoundation::{
    bgra_vers_nv12, bgra_vers_nv12_rect, creer_sample_nv12_depuis, emballer_u64, initialiser_com,
    mf_err, tirer_sortie_mft, MfRuntime,
};
use crate::{CodecCaps, CodecKind, EncodedChunk, EncoderConfig, VideoEncoder};

/// Délai maximal d'attente d'un événement du MFT matériel (demande d'entrée ou
/// sortie prête). Large : l'initialisation de la session GPU peut prendre quelques
/// centaines de millisecondes sur la toute première frame ; au-delà de ce délai,
/// le matériel est considéré défaillant et une erreur claire est renvoyée.
const DELAI_EVENEMENT: Duration = Duration::from_secs(5);

/// Pause entre deux tirages d'événements en mode non bloquant (compromis
/// latence/CPU : NVENC répond en pratique en ~1 ms, la pause reste négligeable
/// devant le budget de 16,7 ms par frame à 60 fps).
const PAUSE_SCRUTATION: Duration = Duration::from_micros(200);

/// Attente avant de considérer que le MFT « retient » ses premières entrées et de
/// ré-amorcer le pipeline (re-soumission de la même image, voir doc de module).
const DELAI_AVANT_REAMORCE: Duration = Duration::from_millis(50);

/// Nombre maximal de ré-amorces (mêmes garde-fous que le backend synchrone).
const MAX_REAMORCES: u32 = 4;

/// Récupère le nom convivial (`MFT_FRIENDLY_NAME_Attribute`) d'un encodeur énuméré.
fn nom_convivial(activate: &IMFActivate) -> String {
    let mut ptr = PWSTR::null();
    let mut longueur = 0u32;
    // SAFETY : GetAllocatedString alloue une chaîne UTF-16 de `longueur` caractères
    // via CoTaskMemAlloc ; on la copie puis on la libère aussitôt (CoTaskMemFree).
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut longueur)
            .is_ok()
            && !ptr.is_null()
        {
            let nom =
                String::from_utf16_lossy(std::slice::from_raw_parts(ptr.0, longueur as usize));
            CoTaskMemFree(Some(ptr.0 as *const _));
            return nom;
        }
    }
    "MFT matériel sans nom".to_string()
}

/// Énumère les encodeurs H.264 **matériels** et retient le meilleur candidat :
/// l'entrée NVIDIA si présente (NVENC), sinon le premier encodeur matériel listé.
/// Erreur si la machine n'en expose aucun (l'appelant se replie alors sur le
/// logiciel — voir [`crate::create_hardware_encoder`]).
fn choisir_encodeur_materiel() -> Result<(IMFActivate, String)> {
    let filtre_sortie = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut tableau: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut nombre = 0u32;
    // SAFETY : appel FFI d'énumération ; `tableau` reçoit un bloc CoTaskMemAlloc de
    // `nombre` pointeurs IMFActivate dont on prend la propriété ci-dessous.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&filtre_sortie),
            &mut tableau,
            &mut nombre,
        )
    }
    .map_err(|e| mf_err("MFTEnumEx (encodeurs H.264 matériels)", e))?;

    if tableau.is_null() || nombre == 0 {
        if !tableau.is_null() {
            // SAFETY : bloc alloué par MFTEnumEx, vide — libération simple.
            unsafe { CoTaskMemFree(Some(tableau as *const _)) };
        }
        return Err(NdError::Codec(
            "aucun encodeur H.264 matériel (MFT) énuméré sur cette machine".into(),
        ));
    }

    // SAFETY : `tableau` contient exactement `nombre` IMFActivate valides ; `take`
    // transfère chaque référence COM vers `candidats` (pas d'AddRef superflu), puis
    // le bloc de pointeurs lui-même est rendu à CoTaskMemFree.
    let candidats: Vec<(IMFActivate, String)> = unsafe {
        let tranche = std::slice::from_raw_parts_mut(tableau, nombre as usize);
        let candidats = tranche
            .iter_mut()
            .filter_map(|logement| logement.take())
            .map(|activate| {
                let nom = nom_convivial(&activate);
                (activate, nom)
            })
            .collect();
        CoTaskMemFree(Some(tableau as *const _));
        candidats
    };

    // Préférence NVIDIA (mission RTX 4080/NVENC) ; sinon premier matériel listé
    // (SORTANDFILTER place déjà le plus pertinent en tête). Les autres candidats
    // sont relâchés par Drop.
    let index = candidats
        .iter()
        .position(|(_, nom)| nom.to_lowercase().contains("nvidia"))
        .unwrap_or(0);
    let mut candidats = candidats;
    Ok(candidats.swap_remove(index))
}

/// Active l'encodeur choisi et le prépare au protocole asynchrone : vérification
/// `MF_TRANSFORM_ASYNC`, déverrouillage `MF_TRANSFORM_ASYNC_UNLOCK`, générateur
/// d'événements. Erreur si le MFT n'est pas réellement instanciable ici.
fn activer_et_deverrouiller(
    activate: &IMFActivate,
) -> Result<(IMFTransform, IMFMediaEventGenerator)> {
    // SAFETY : ActivateObject instancie l'objet COM du MFT (chargement de la DLL du
    // pilote) ; apparié à ShutdownObject dans `configure`/`Drop`.
    let mft: IMFTransform = unsafe { activate.ActivateObject() }
        .map_err(|e| mf_err("IMFActivate::ActivateObject (MFT matériel)", e))?;

    // SAFETY : GetAttributes/GetUINT32/SetUINT32 sont autorisés même sur un MFT
    // asynchrone encore verrouillé (exceptions documentées du verrouillage).
    let attributs =
        unsafe { mft.GetAttributes() }.map_err(|e| mf_err("GetAttributes (MFT matériel)", e))?;
    // SAFETY : lecture d'un attribut optionnel (absent ⇒ MFT synchrone).
    let asynchrone = unsafe { attributs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1;
    if !asynchrone {
        return Err(NdError::Codec(
            "MFT matériel non asynchrone : hors du contrat des MFT matériels, backend refusé"
                .into(),
        ));
    }
    // SAFETY : déverrouillage requis avant tout ProcessMessage/Input/Output
    // (contrat des MFT asynchrones).
    unsafe { attributs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
        .map_err(|e| mf_err("MF_TRANSFORM_ASYNC_UNLOCK", e))?;

    let evenements = mft
        .cast::<IMFMediaEventGenerator>()
        .map_err(|e| mf_err("cast IMFMediaEventGenerator", e))?;
    Ok((mft, evenements))
}

/// Identifiants des flux d'entrée/sortie du MFT (un encodeur en a un de chaque ;
/// `E_NOTIMPL` signifie « numérotation implicite 0..n-1 », contrat `IMFTransform`).
fn ids_flux(mft: &IMFTransform) -> Result<(u32, u32)> {
    let mut entrees = [0u32; 1];
    let mut sorties = [0u32; 1];
    // SAFETY : tableaux de taille 1, suffisants pour un encodeur mono-flux.
    match unsafe { mft.GetStreamIDs(&mut entrees, &mut sorties) } {
        Ok(()) => Ok((entrees[0], sorties[0])),
        Err(e) if e.code() == E_NOTIMPL => Ok((0, 0)),
        Err(e) => Err(mf_err("GetStreamIDs", e)),
    }
}

/// Encodeur H.264 fondé sur le MFT **matériel asynchrone** (voir doc de module).
pub struct NvencEncoder {
    /// Garde MFStartup/MFShutdown (doit survivre au MFT → déclarée avant lui).
    _runtime: MfRuntime,
    /// Objet d'activation de l'encodeur retenu (permet de recréer une instance
    /// fraîche à chaque `configure` : ShutdownObject puis ActivateObject).
    activate: IMFActivate,
    /// Nom convivial **exact** de l'encodeur sélectionné (preuve matériel,
    /// ex. « NVIDIA H264 Encoder MFT ») — exposé par [`VideoEncoder::nom_backend`].
    nom: String,
    /// Instance MFT courante (créée par `new`, recréée par `configure`).
    mft: Option<IMFTransform>,
    /// Générateur d'événements du MFT (protocole asynchrone).
    evenements: Option<IMFMediaEventGenerator>,
    /// Réglage fin (débit à chaud, image-clé forcée) ; `None` si non exposé.
    codec_api: Option<ICodecAPI>,
    cfg: Option<EncoderConfig>,
    /// Vrai après START_OF_STREAM : la prochaine (re)configuration recrée le MFT.
    flux_demarre: bool,
    /// Identifiants des flux d'entrée/sortie (souvent 0/0, jamais supposé).
    id_entree: u32,
    id_sortie: u32,
    /// Durée d'une frame en unités MF (100 ns).
    duree_frame_100ns: i64,
    /// Taille conseillée du tampon de sortie si le MFT n'alloue pas lui-même.
    taille_sortie: usize,
    /// `true` si le MFT fournit ses échantillons de sortie (cas matériel usuel).
    fournit_echantillons: bool,
    /// Nombre de frames soumises (horodatages d'entrée monotones réguliers).
    frames_soumises: u64,
    /// Crédits `METransformNeedInput` non consommés (1 crédit = 1 ProcessInput).
    credits_entree: u32,
    /// Sorties encodées déjà tirées du MFT, dans l'ordre du flux (voir doc de
    /// module : latence de pipeline bornée, 0 constaté avec NVENC).
    sorties_pretes: VecDeque<(Vec<u8>, bool)>,
    /// Canevas NV12 persistant (conversion pleine ou restreinte, mode delta).
    nv12: Vec<u8>,
    /// Vrai si `nv12` contient une image complète de la configuration courante.
    nv12_valide: bool,
    /// État du mode delta (saut de trames, image-clé après repos) — voir `delta`.
    suivi: SuiviDelta,
}

// SAFETY : le MFT matériel est un objet COM libre (« both/free-threaded ») créé en
// contexte MTA ; ses interfaces peuvent être déplacées entre threads du MTA et
// l'accès est exclusif (`&mut self`) via le trait. COM n'est jamais décompté au
// drop (voir `MfRuntime`), donc aucune opération liée au thread d'origine n'a lieu
// à la libération.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Prépare l'encodeur matériel : énumère les MFT H.264 matériels, sélectionne
    /// le candidat NVIDIA (sinon le premier matériel), l'**instancie** et le
    /// déverrouille — la réussite de `new` prouve donc qu'un encodeur GPU est
    /// réellement utilisable ici (pas seulement listé). Erreur sinon : l'appelant
    /// ([`crate::create_hardware_encoder`]) se replie sur le logiciel.
    pub fn new() -> Result<Self> {
        let runtime = MfRuntime::new()?;
        initialiser_com()?;
        let (activate, nom) = choisir_encodeur_materiel()?;
        let (mft, evenements) = activer_et_deverrouiller(&activate)?;
        Ok(Self {
            _runtime: runtime,
            activate,
            nom,
            mft: Some(mft),
            evenements: Some(evenements),
            codec_api: None,
            cfg: None,
            flux_demarre: false,
            id_entree: 0,
            id_sortie: 0,
            duree_frame_100ns: 0,
            taille_sortie: 0,
            fournit_echantillons: true,
            frames_soumises: 0,
            credits_entree: 0,
            sorties_pretes: VecDeque::new(),
            nv12: Vec::new(),
            nv12_valide: false,
            suivi: SuiviDelta::new(),
        })
    }

    /// Applique un réglage `ICodecAPI` en mode meilleur effort (les propriétés non
    /// prises en charge ne doivent pas faire échouer l'encodage).
    fn regler_codec_api(&self, propriete: &windows::core::GUID, valeur: &VARIANT) {
        if let Some(api) = self.codec_api.as_ref() {
            // SAFETY : références valides ; sans effet si non supporté (ignoré).
            let _ = unsafe { api.SetValue(propriete, valeur) };
        }
    }

    /// Draine **sans bloquer** tous les événements en attente du MFT : crédite les
    /// demandes d'entrée, matérialise immédiatement chaque sortie annoncée (un
    /// crédit `METransformHaveOutput` = exactement un `ProcessOutput`), fait
    /// remonter `MEError` en erreur claire.
    fn pomper_evenements(&mut self) -> Result<()> {
        let Some(evenements) = self.evenements.clone() else {
            return Ok(());
        };
        loop {
            // SAFETY : tirage non bloquant ; MF_E_NO_EVENTS_AVAILABLE = file vide.
            let evenement = match unsafe { evenements.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(evenement) => evenement,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(mf_err("IMFMediaEventGenerator::GetEvent", e)),
            };
            // SAFETY : lecture du type (u32) d'un événement valide.
            let genre =
                unsafe { evenement.GetType() }.map_err(|e| mf_err("IMFMediaEvent::GetType", e))?;
            if genre == METransformNeedInput.0 as u32 {
                self.credits_entree = self.credits_entree.saturating_add(1);
            } else if genre == METransformHaveOutput.0 as u32 {
                self.recolter_une_sortie()?;
            } else if genre == MEError.0 as u32 {
                // SAFETY : le statut d'un MEError porte le HRESULT de la panne.
                let statut = unsafe { evenement.GetStatus() }
                    .map(|hr| hr.message())
                    .unwrap_or_else(|_| "statut illisible".into());
                return Err(NdError::Codec(format!(
                    "MFT matériel « {} » : événement MEError ({statut})",
                    self.nom
                )));
            }
            // Autres événements (METransformDrainComplete, marqueurs…) : ignorés,
            // ce backend ne draine jamais (flux continu de bureau à distance).
        }
    }

    /// Consomme un crédit `METransformHaveOutput` : tire une sortie du MFT et la
    /// range dans `sorties_pretes` (ordre du flux préservé).
    fn recolter_une_sortie(&mut self) -> Result<()> {
        let Some(mft) = self.mft.clone() else {
            return Ok(());
        };
        if let Some(sortie) = tirer_sortie_mft(
            &mft,
            self.id_sortie,
            self.fournit_echantillons,
            self.taille_sortie,
        )? {
            self.sorties_pretes.push_back(sortie);
        }
        Ok(())
    }

    /// Attend un crédit `METransformNeedInput` (délai borné) puis soumet le canevas
    /// NV12 courant, horodaté de façon monotone.
    fn soumettre_canevas(&mut self) -> Result<()> {
        let debut = Instant::now();
        while self.credits_entree == 0 {
            self.pomper_evenements()?;
            if self.credits_entree > 0 {
                break;
            }
            if debut.elapsed() > DELAI_EVENEMENT {
                return Err(NdError::Codec(format!(
                    "MFT matériel « {} » : aucune demande d'entrée après {DELAI_EVENEMENT:?}",
                    self.nom
                )));
            }
            std::thread::sleep(PAUSE_SCRUTATION);
        }

        let mft = self
            .mft
            .clone()
            .ok_or_else(|| NdError::Codec("encodeur non configuré (appeler configure)".into()))?;
        let horodatage = self.frames_soumises as i64 * self.duree_frame_100ns;
        let sample = creer_sample_nv12_depuis(&self.nv12, horodatage, self.duree_frame_100ns)?;
        self.frames_soumises += 1;
        // SAFETY : sample construit à l'instant ; un crédit NeedInput est détenu
        // (contrat asynchrone : 1 crédit = 1 ProcessInput).
        unsafe { mft.ProcessInput(self.id_entree, &sample, 0) }
            .map_err(|e| mf_err("ProcessInput (MFT matériel)", e))?;
        self.credits_entree -= 1;
        Ok(())
    }

    /// Attend la prochaine sortie encodée (ordre du flux). Si le MFT réclame plus
    /// d'entrées avant sa première sortie (amorçage du pipeline matériel), la même
    /// image est re-soumise de façon bornée — voir doc de module.
    fn attendre_sortie(&mut self) -> Result<(Vec<u8>, bool)> {
        let debut = Instant::now();
        let mut reamorces = 0u32;
        loop {
            if let Some(sortie) = self.sorties_pretes.pop_front() {
                return Ok(sortie);
            }
            self.pomper_evenements()?;
            if let Some(sortie) = self.sorties_pretes.pop_front() {
                return Ok(sortie);
            }
            if self.credits_entree > 0
                && reamorces < MAX_REAMORCES
                && debut.elapsed() > DELAI_AVANT_REAMORCE
            {
                reamorces += 1;
                self.soumettre_canevas()?;
                continue;
            }
            if debut.elapsed() > DELAI_EVENEMENT {
                return Err(NdError::Codec(format!(
                    "MFT matériel « {} » : aucune sortie après {DELAI_EVENEMENT:?} \
                     ({reamorces} ré-amorce(s))",
                    self.nom
                )));
            }
            std::thread::sleep(PAUSE_SCRUTATION);
        }
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        // Relâche d'abord les interfaces, puis libère l'instance (et sa session
        // GPU) via l'objet d'activation — chemin de disposal documenté d'un MFT
        // activé par IMFActivate. Meilleur effort : rien à faire en cas d'échec.
        self.codec_api = None;
        self.evenements = None;
        self.mft = None;
        // SAFETY : ShutdownObject apparie les ActivateObject de new/configure.
        let _ = unsafe { self.activate.ShutdownObject() };
    }
}

impl VideoEncoder for NvencEncoder {
    fn capabilities() -> CodecCaps {
        CodecCaps {
            // Encodeur matériel réel — c'est tout l'objet de ce backend ; `new`
            // n'aboutit que si un MFT matériel est effectivement instancié.
            hardware: true,
            kinds: vec![CodecKind::H264],
            // Limite H.264 de NVENC (RTX série 40) : 4096×4096.
            max_width: 4096,
            max_height: 4096,
        }
    }

    fn configure(&mut self, cfg: EncoderConfig) -> Result<()> {
        if cfg.kind != CodecKind::H264 {
            return Err(NdError::Codec(
                "backend MFT matériel : H.264 uniquement (plan 03)".into(),
            ));
        }
        if !cfg.width.is_multiple_of(2)
            || !cfg.height.is_multiple_of(2)
            || cfg.width == 0
            || cfg.height == 0
        {
            return Err(NdError::Codec(
                "MFT matériel : dimensions nulles ou impaires non supportées (NV12)".into(),
            ));
        }
        if cfg.max_fps == 0 {
            return Err(NdError::Codec(
                "MFT matériel : max_fps doit être non nul".into(),
            ));
        }

        initialiser_com()?;

        // Reconfiguration = instance fraîche (chemin simple et fiable, comme le
        // backend synchrone) : l'instance précédente est arrêtée via l'activation,
        // puis un nouvel objet est activé et déverrouillé.
        if self.flux_demarre {
            self.codec_api = None;
            self.evenements = None;
            self.mft = None;
            // SAFETY : apparie l'ActivateObject précédent ; l'échec est ignoré
            // (instance déjà hors service), la re-création tranche.
            let _ = unsafe { self.activate.ShutdownObject() };
            let (mft, evenements) = activer_et_deverrouiller(&self.activate)?;
            self.mft = Some(mft);
            self.evenements = Some(evenements);
            self.flux_demarre = false;
        }
        let mft = self
            .mft
            .clone()
            .ok_or_else(|| NdError::Codec("MFT matériel absent (état interne invalide)".into()))?;

        // Nouveau flux : crédits/compteurs remis à zéro, canevas invalide.
        self.frames_soumises = 0;
        self.credits_entree = 0;
        self.sorties_pretes.clear();
        self.nv12_valide = false;
        self.suivi.reinitialiser();

        let (id_entree, id_sortie) = ids_flux(&mft)?;
        self.id_entree = id_entree;
        self.id_sortie = id_sortie;

        // Réglage fin (meilleur effort) : CBR + faible latence — une sortie par
        // entrée, pas de B-frames (latence d'un bureau à distance).
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
        if let Ok(attributs) = unsafe { mft.GetAttributes() } {
            // SAFETY : clé et valeur valides ; meilleur effort.
            let _ = unsafe { attributs.SetUINT32(&MF_LOW_LATENCY, 1) };
        }

        let taille = emballer_u64(cfg.width, cfg.height);
        let cadence = emballer_u64(cfg.max_fps, 1);

        // SAFETY : construction des types de média — mêmes clés/valeurs constantes
        // que le backend synchrone, chaque HRESULT vérifié ; ordre imposé par le
        // contrat MFT encodeur (type de SORTIE d'abord, puis type d'entrée).
        unsafe {
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
            mft.SetOutputType(self.id_sortie, &sortie, 0)
                .map_err(|e| mf_err("SetOutputType (MFT matériel)", e))?;

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
            // Tampon mémoire système contigu : une ligne = `width` octets de Y.
            entree
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width)
                .map_err(|e| mf_err("MF_MT_DEFAULT_STRIDE (entrée)", e))?;
            mft.SetInputType(self.id_entree, &entree, 0)
                .map_err(|e| mf_err("SetInputType (MFT matériel)", e))?;

            // Propriété des échantillons de sortie : un MFT matériel fournit
            // (ou peut fournir) les siens ; sinon on allouera `taille_sortie`.
            let info = mft
                .GetOutputStreamInfo(self.id_sortie)
                .map_err(|e| mf_err("GetOutputStreamInfo", e))?;
            self.fournit_echantillons = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            self.taille_sortie = if info.cbSize > 0 {
                info.cbSize as usize
            } else {
                (cfg.width as usize) * (cfg.height as usize) * 4
            };

            // Démarrage du flux : les événements METransformNeedInput commencent
            // à arriver après START_OF_STREAM.
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| mf_err("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING", e))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| mf_err("MFT_MESSAGE_NOTIFY_START_OF_STREAM", e))?;
        }

        self.duree_frame_100ns = 10_000_000 / i64::from(cfg.max_fps);
        self.flux_demarre = true;
        self.cfg = Some(cfg);
        Ok(())
    }

    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedChunk> {
        let cfg = self
            .cfg
            .ok_or_else(|| NdError::Codec("encodeur non configuré (appeler configure)".into()))?;
        if !self.flux_demarre || self.mft.is_none() {
            return Err(NdError::Codec(
                "encodeur non configuré (appeler configure)".into(),
            ));
        }

        if frame.width != cfg.width || frame.height != cfg.height {
            return Err(NdError::Codec(format!(
                "frame {}x{} ≠ configuration {}x{} (reconfigurer l'encodeur)",
                frame.width, frame.height, cfg.width, cfg.height
            )));
        }
        let (w, h) = (frame.width as usize, frame.height as usize);
        let compatible = self.nv12_valide && self.nv12.len() == w * h + w * h / 2;

        // Saut d'encodage (mode delta) : rien n'a changé → trame de répétition à
        // données vides, sans conversion ni passage par le GPU.
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
        // après une longue période statique (voir `delta`). S'applique à la
        // prochaine frame soumise (meilleur effort ICodecAPI).
        let force_cle = force_keyframe
            || (self.suivi.actif() && self.suivi.keyframe_apres_repos(aire_modifiee, aire_image));
        if force_cle {
            self.regler_codec_api(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
        }

        // Protocole asynchrone : récolter ce qui est déjà prêt, soumettre la frame
        // courante (1 crédit NeedInput), puis rendre la prochaine sortie du flux.
        self.pomper_evenements()?;
        self.soumettre_canevas()?;
        let (donnees, image_cle) = self.attendre_sortie()?;

        self.suivi.note_encodage();

        Ok(EncodedChunk {
            data: donnees,
            is_keyframe: image_cle,
            monitor: frame.monitor,
            timestamp_us: frame.timestamp_us,
        })
    }

    /// Mise à jour du débit à chaud (pilotée par l'ABR, plan 03/04), même contrat
    /// consolidé que le backend synchrone : consigne mémorisée dans la
    /// configuration puis appliquée via `ICodecAPI` (`AVEncCommonMeanBitRate`) —
    /// NVENC accepte ce réglage en cours de flux (mode CBR posé à `configure`),
    /// sans image-clé parasite. Un MFT qui l'ignore reste fonctionnel au débit
    /// précédent (limite documentée, identique à `mediafoundation`).
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

    fn nom_backend(&self) -> &str {
        &self.nom
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

    /// Crée l'encodeur matériel, ou saute proprement le test si la machine n'en
    /// expose aucun (parc CI hétérogène) — le repli logiciel a ses propres tests.
    fn encodeur_ou_saut(test: &str) -> Option<NvencEncoder> {
        match NvencEncoder::new() {
            Ok(enc) => Some(enc),
            Err(e) => {
                eprintln!("{test} : sauté, encodeur matériel indisponible ici ({e})");
                None
            }
        }
    }

    /// Aller-retour complet : encode 4 frames via le MFT matériel puis re-décode le
    /// flux avec openh264 pour prouver sa validité (première frame = image-clé).
    #[test]
    fn nvenc_encode_puis_redecodage_openh264() {
        let Some(mut enc) = encodeur_ou_saut("nvenc_encode_puis_redecodage_openh264") else {
            return;
        };
        let (w, h) = (320u32, 240u32);
        enc.configure(EncoderConfig {
            kind: CodecKind::H264,
            width: w,
            height: h,
            target_bitrate_kbps: 2_000,
            max_fps: 30,
        })
        .expect("configure MFT matériel");
        assert!(!enc.nom_backend().is_empty());

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
    /// à données vides), une frame avec régions est encodée normalement — même
    /// contrat que les autres backends.
    #[test]
    fn nvenc_delta_saut_et_reprise() {
        let Some(mut enc) = encodeur_ou_saut("nvenc_delta_saut_et_reprise") else {
            return;
        };
        let (w, h) = (320u32, 240u32);
        enc.set_delta_mode(true);
        enc.configure(EncoderConfig {
            kind: CodecKind::H264,
            width: w,
            height: h,
            target_bitrate_kbps: 2_000,
            max_fps: 30,
        })
        .expect("configure MFT matériel");

        let mut pleine = frame_test(w, h, 0);
        pleine.dirty = vec![nd_capture::Rect { x: 0, y: 0, w, h }];
        let premiere = enc.encode(&pleine, true).expect("première frame");
        assert!(!premiere.data.is_empty());

        let mut statique = frame_test(w, h, 0);
        statique.dirty = Vec::new();
        let saut = enc.encode(&statique, false).expect("saut");
        assert!(saut.data.is_empty(), "trame de répétition attendue");
        assert!(!saut.is_keyframe);

        let mut bouge = frame_test(w, h, 64);
        bouge.dirty = vec![nd_capture::Rect {
            x: 0,
            y: 0,
            w,
            h: 16,
        }];
        let chunk = enc.encode(&bouge, false).expect("frame delta");
        assert!(!chunk.data.is_empty());
    }

    /// Une reconfiguration (changement de résolution) recrée l'instance MFT et le
    /// flux repart sur une image-clé décodable.
    #[test]
    fn nvenc_reconfiguration_change_de_resolution() {
        let Some(mut enc) = encodeur_ou_saut("nvenc_reconfiguration_change_de_resolution") else {
            return;
        };
        for (w, h) in [(320u32, 240u32), (640u32, 480u32)] {
            enc.configure(EncoderConfig {
                kind: CodecKind::H264,
                width: w,
                height: h,
                target_bitrate_kbps: 2_000,
                max_fps: 30,
            })
            .expect("configure MFT matériel");
            let mut dec = crate::create_decoder(CodecKind::H264).expect("décodeur openh264");
            let chunk = enc.encode(&frame_test(w, h, 0), true).expect("encode");
            assert!(chunk.is_keyframe, "nouveau flux ⇒ image-clé");
            let img = dec
                .decode(&chunk)
                .expect("flux H.264 invalide")
                .expect("image décodée");
            assert_eq!((img.width, img.height), (w, h));
        }
    }
}
