//! `nd-codec` — abstraction d'encodage/décodage vidéo.
//!
//! Stratégie « matériel d'abord » (NVENC/AMF/QSV/VideoToolbox/MediaCodec) avec repli
//! logiciel, optimisée pour le contenu bureau (texte net). Détails, budget de latence
//! et contrôle de débit adaptatif : `../../plan-technique/03-codec-video.md`.

use nd_capture::CapturedFrame;
use nd_proto::{MonitorId, NdError, Result};

/// Famille de codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    /// Socle universel (support matériel le plus large). Voir plan 03.
    H264,
    /// Meilleure compression, brevets à surveiller.
    H265,
    /// Cible libre de redevances (royalty-free).
    Av1,
    /// Repli secondaire.
    Vp9,
}

/// Capacités d'un encodeur détecté au runtime.
#[derive(Debug, Clone)]
pub struct CodecCaps {
    /// `true` si l'accélération matérielle est disponible.
    pub hardware: bool,
    pub kinds: Vec<CodecKind>,
    pub max_width: u32,
    pub max_height: u32,
}

/// Configuration d'un encodeur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub kind: CodecKind,
    pub width: u32,
    pub height: u32,
    /// Débit cible (piloté par l'ABR — voir plan 03/04).
    pub target_bitrate_kbps: u32,
    pub max_fps: u32,
}

/// Unité encodée prête à être transportée.
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    pub data: Vec<u8>,
    /// Vrai pour une image-clé / point de resynchronisation.
    pub is_keyframe: bool,
    pub monitor: MonitorId,
    pub timestamp_us: u64,
}

/// Encodeur vidéo. La méthode [`VideoEncoder::capabilities`] est exclue de la
/// v-table (`where Self: Sized`) pour préserver l'usage en objet-trait.
pub trait VideoEncoder: Send {
    /// Capacités statiques de cet encodeur.
    fn capabilities() -> CodecCaps
    where
        Self: Sized;
    /// (Re)configure l'encodeur (résolution, débit, codec).
    fn configure(&mut self, cfg: EncoderConfig) -> Result<()>;
    /// Encode une frame capturée. `force_keyframe` impose un point de resynchro.
    ///
    /// En mode delta ([`Self::set_delta_mode`]), une frame sans région modifiée
    /// (`CapturedFrame::dirty` vide, pixels facultatifs) produit une **trame de
    /// répétition** : un [`EncodedChunk`] à données vides, que le décodeur traite
    /// comme « pas de nouvelle image » ([`VideoDecoder::decode`] → `Ok(None)`).
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedChunk>;
    /// Ajuste le débit cible à chaud (appelé par l'ABR — voir [`RateController`]).
    fn set_target_bitrate(&mut self, kbps: u32);
    /// Active/désactive l'**encodage delta** : exploitation des régions modifiées
    /// (`CapturedFrame::dirty`) — saut des trames inchangées, conversion couleur
    /// restreinte aux régions annoncées, image-clé adaptative (voir le module
    /// `delta` pour la politique et les limites).
    ///
    /// **Opt-in** : ne l'activer que si la source de capture renseigne
    /// **fidèlement** `dirty` (toutes les régions modifiées, déplacements/scroll
    /// inclus). `dirty` vide signifie alors « rien n'a changé » ; chez une source
    /// qui ne suit pas les régions, il signifie « inconnu » et activer ce mode
    /// gèlerait l'image. Par défaut (implémentation fournie) : ignoré —
    /// comportement plein cadre historique.
    fn set_delta_mode(&mut self, actif: bool) {
        let _ = actif;
    }
    /// Nom lisible du backend d'encodage réellement à l'œuvre (observabilité :
    /// journal de session, sondes de diagnostic). Pour un backend matériel, c'est
    /// le **nom exact** de l'encodeur sélectionné (ex. « NVIDIA H264 Encoder
    /// MFT ») — la preuve que le GPU est bien utilisé, jamais un libellé « sur le
    /// papier ». Implémentation par défaut fournie (libellé générique) pour rester
    /// additif vis-à-vis des implémentations existantes du trait.
    fn nom_backend(&self) -> &str {
        "encodeur vidéo (backend non identifié)"
    }
}

/// Image décodée : dimensions + pixels RGBA prêts pour l'affichage (voir plan 10).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Pixels RGBA (largeur × hauteur × 4 octets), ordre R, G, B, A.
    pub rgba: Vec<u8>,
}

/// Décodeur vidéo côté viewer. Le rendu de la texture décodée est confié à l'UI
/// (voir plan 10).
pub trait VideoDecoder: Send {
    /// Décode une unité. Renvoie `Some(frame)` quand une image complète est produite ;
    /// `None` en début de flux (avant la première image-clé exploitable).
    fn decode(&mut self, chunk: &EncodedChunk) -> Result<Option<DecodedFrame>>;
}

/// Backend logiciel H.264 (openh264). Voir plan 03.
mod software;

/// Backend plateforme Windows : H.264 via Media Foundation (MFT logiciel
/// synchrone — repli du matériel). Voir plan 03.
#[cfg(windows)]
mod mediafoundation;

/// Backend plateforme Windows : H.264 via le MFT **matériel asynchrone** (NVENC
/// sur GPU NVIDIA), énuméré par `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE)`. Voir plan
/// 03/16 « matériel d'abord ».
#[cfg(windows)]
mod nvenc;

/// Négociation de codec entre pairs et échelle de débit adaptatif (ABR). Voir plan 03.
mod negotiation;

/// Encodage delta : exploitation des régions modifiées (`CapturedFrame::dirty`) —
/// saut de trames, conversion partielle, image-clé adaptative. Voir plan 03.
mod delta;

/// Contrôleur ABR : boucle fermée estimations réseau → `set_target_bitrate`.
mod rate;

/// Mesure de qualité pour le banc de test (PSNR/MSE/SSIM, export Y4M). Voir plan 14.
/// 100 % portable : aucune FFI, aucune dépendance plateforme.
mod metrics;

pub use metrics::{mse_rgba, psnr_luma, psnr_par_canal_rgba, psnr_rgba, ssim_luma, write_y4m};
pub use negotiation::{
    available_encoders, negotiate, BitrateLadder, ContentProfile, NetworkEstimate,
};
pub use rate::RateController;

/// Crée l'encodeur pour le codec demandé.
///
/// H.264 : backend **logiciel** openh264 (le matériel — NVENC / Media Foundation —
/// viendra derrière ce même trait, voir plan 03/16). Autres codecs : à venir.
pub fn create_encoder(kind: CodecKind) -> Result<Box<dyn VideoEncoder>> {
    match kind {
        CodecKind::H264 => Ok(Box::new(software::Openh264Encoder::new())),
        _ => Err(NdError::NotImplemented(
            "nd-codec::create_encoder : seul H.264 (logiciel) est implémenté, voir plan 03/16",
        )),
    }
}

/// Crée l'encodeur **plateforme/matériel** pour le codec demandé (plan 03
/// « matériel d'abord »).
///
/// Windows + H.264, dans l'ordre :
///
/// 1. **MFT matériel asynchrone** (module `nvenc`) — sur ce parc, l'encodeur
///    **NVENC** du GPU NVIDIA ; le nom exact du MFT sélectionné est exposé par
///    [`VideoEncoder::nom_backend`] (preuve matériel). Il honore
///    `set_target_bitrate` (reconfiguration à chaud sans image-clé parasite) et le
///    mode delta (saut de trames + conversion partielle).
/// 2. **Repli documenté** : si aucun MFT matériel n'est énuméré/instanciable
///    (machine sans GPU dédié, pilote absent, session distante restreinte…), le
///    MFT **logiciel** Media Foundation prend le relais — même trait, mêmes
///    appelants, un avertissement clair est émis une fois sur stderr (pas de
///    façade de journalisation dans le workspace à ce jour). Ce chemin ne panique
///    jamais : il dégrade proprement.
///
/// Autres plateformes ou codecs : `NdError::NotImplemented`. Ce chemin s'ajoute à
/// [`create_encoder`] (repli logiciel openh264) sans le remplacer : l'appelant
/// tente d'abord le matériel puis se replie (plan 03/16).
pub fn create_hardware_encoder(kind: CodecKind) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(windows)]
    if kind == CodecKind::H264 {
        match nvenc::NvencEncoder::new() {
            Ok(enc) => return Ok(Box::new(enc)),
            Err(e) => {
                // Repli logiciel documenté (voir doc ci-dessus) ; avertissement
                // émis une seule fois par processus pour ne pas noyer stderr
                // (cette fonction sert aussi de sonde d'inventaire).
                static REPLI_LOGGE: std::sync::Once = std::sync::Once::new();
                REPLI_LOGGE.call_once(|| {
                    eprintln!(
                        "nd-codec : encodeur H.264 matériel indisponible ({e}) ; \
                         repli sur le MFT logiciel Media Foundation (plan 03/16)."
                    );
                });
                return Ok(Box::new(mediafoundation::MediaFoundationEncoder::new()?));
            }
        }
    }
    #[cfg(not(windows))]
    let _ = kind;
    Err(NdError::NotImplemented(
        "nd-codec::create_hardware_encoder : H.264 Media Foundation sur Windows uniquement (plan 03/16)",
    ))
}

/// Crée le décodeur pour le codec demandé (H.264 logiciel openh264).
pub fn create_decoder(kind: CodecKind) -> Result<Box<dyn VideoDecoder>> {
    match kind {
        CodecKind::H264 => Ok(Box::new(software::Openh264Decoder::new()?)),
        _ => Err(NdError::NotImplemented(
            "nd-codec::create_decoder : seul H.264 (logiciel) est implémenté, voir plan 03/16",
        )),
    }
}
