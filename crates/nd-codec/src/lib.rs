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
#[derive(Debug, Clone, Copy)]
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
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedChunk>;
    /// Ajuste le débit cible à chaud (appelé par l'ABR).
    fn set_target_bitrate(&mut self, kbps: u32);
}

/// Image décodée (métadonnées ; les pixels iront à la surface de rendu, plan 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
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

/// Crée le décodeur pour le codec demandé (H.264 logiciel openh264).
pub fn create_decoder(kind: CodecKind) -> Result<Box<dyn VideoDecoder>> {
    match kind {
        CodecKind::H264 => Ok(Box::new(software::Openh264Decoder::new()?)),
        _ => Err(NdError::NotImplemented(
            "nd-codec::create_decoder : seul H.264 (logiciel) est implémenté, voir plan 03/16",
        )),
    }
}
