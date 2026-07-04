//! Backend logiciel H.264 via la crate `openh264` (Cisco OpenH264).
//!
//! C'est le **repli logiciel** du plan 03 : il fonctionne partout, sans GPU. Les
//! backends matériels (NVENC sur NVIDIA, Media Foundation sur Windows, VideoToolbox
//! sur macOS…) seront ajoutés derrière les mêmes traits [`VideoEncoder`] /
//! [`VideoDecoder`]. L'usage est réglé sur « contenu écran temps réel »
//! (`ScreenContentRealTime`), adapté au bureau (voir plan 03 §optimisation desktop).

use nd_capture::{CapturedFrame, FrameImage};
use nd_proto::{NdError, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    Encoder, EncoderConfig as OhConfig, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;

use crate::{
    CodecCaps, CodecKind, DecodedFrame, EncodedChunk, EncoderConfig, VideoDecoder, VideoEncoder,
};

/// Convertit une erreur `openh264` en `NdError::Codec`.
fn codec_err(e: openh264::Error) -> NdError {
    NdError::Codec(e.to_string())
}

/// Encodeur H.264 logiciel.
pub struct Openh264Encoder {
    /// Encodeur openh264 ; instancié à `configure` (dépend du débit/fps).
    inner: Option<Encoder>,
}

impl Openh264Encoder {
    #[must_use]
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Default for Openh264Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoEncoder for Openh264Encoder {
    fn capabilities() -> CodecCaps {
        CodecCaps {
            hardware: false,
            kinds: vec![CodecKind::H264],
            max_width: 3840,
            max_height: 2160,
        }
    }

    fn configure(&mut self, cfg: EncoderConfig) -> Result<()> {
        if cfg.kind != CodecKind::H264 {
            return Err(NdError::Codec(
                "backend logiciel openh264 : H.264 uniquement".into(),
            ));
        }
        let oh = OhConfig::new()
            .set_bitrate_bps(cfg.target_bitrate_kbps.saturating_mul(1000))
            .max_frame_rate(cfg.max_fps as f32)
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .enable_skip_frame(false);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), oh).map_err(codec_err)?;
        self.inner = Some(enc);
        Ok(())
    }

    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedChunk> {
        let enc = self
            .inner
            .as_mut()
            .ok_or_else(|| NdError::Codec("encodeur non configuré (appeler configure)".into()))?;

        let Some(FrameImage::Cpu { data, .. }) = frame.image.as_ref() else {
            return Err(NdError::Codec("frame sans pixels CPU à encoder".into()));
        };

        let (w, h) = (frame.width as usize, frame.height as usize);
        if w % 2 != 0 || h % 2 != 0 {
            return Err(NdError::Codec(
                "dimensions impaires non supportées par H.264".into(),
            ));
        }
        if data.len() != w * h * 4 {
            return Err(NdError::Codec(
                "taille de frame incohérente (attendu largeur*hauteur*4, BGRA sans padding)".into(),
            ));
        }

        // Conversion BGRA -> I420 (chemin scalaire ; SIMD/hardware = optimisation future).
        let bgra = BgraSliceU8::new(data, (w, h));
        let yuv = YUVBuffer::from_rgb_source(bgra);

        if force_keyframe {
            enc.force_intra_frame();
        }
        let bitstream = enc.encode(&yuv).map_err(codec_err)?;
        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let payload = bitstream.to_vec();

        Ok(EncodedChunk {
            data: payload,
            is_keyframe,
            monitor: frame.monitor,
            timestamp_us: frame.timestamp_us,
        })
    }

    fn set_target_bitrate(&mut self, _kbps: u32) {
        // TODO(ABR, plan 03/04) : mise à jour du débit à chaud via
        // ENCODER_OPTION_BITRATE sur l'encodeur openh264 déjà initialisé.
    }
}

/// Décodeur H.264 logiciel (pour vérification et pour le côté viewer).
pub struct Openh264Decoder {
    inner: Decoder,
}

impl Openh264Decoder {
    pub fn new() -> Result<Self> {
        let dec = Decoder::new().map_err(codec_err)?;
        Ok(Self { inner: dec })
    }
}

impl VideoDecoder for Openh264Decoder {
    fn decode(&mut self, chunk: &EncodedChunk) -> Result<Option<DecodedFrame>> {
        // Le rendu de la YUV décodée sera confié à l'UI (voir plan 10) ; ici on renvoie
        // les dimensions décodées comme preuve de décodage.
        match self.inner.decode(&chunk.data).map_err(codec_err)? {
            Some(yuv) => {
                let (w, h) = yuv.dimensions();
                Ok(Some(DecodedFrame {
                    width: w as u32,
                    height: h as u32,
                }))
            }
            None => Ok(None),
        }
    }
}
