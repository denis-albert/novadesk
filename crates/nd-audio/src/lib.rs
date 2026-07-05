//! `nd-audio` — capture (loopback système + micro) et lecture audio, codec Opus.
//!
//! Transport via datagrammes non fiables + jitter buffer, synchro A/V par horodatage
//! commun avec la vidéo. Détails par OS (WASAPI/CoreAudio/PipeWire) :
//! `../../plan-technique/08-audio.md`.

pub mod codec;
pub mod convert;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
use nd_proto::NdError;
use nd_proto::Result;

pub use codec::{echantillons_par_trame, DecodeurOpus, EncodeurOpus, TRAME_MS};
#[cfg(windows)]
pub use win::WasapiLoopbackCapturer;

/// Format audio PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
}

impl Default for AudioFormat {
    fn default() -> Self {
        // 48 kHz stéréo : entrée native d'Opus (voir plan 08).
        AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

/// Paquet audio encodé (Opus) prêt pour le transport.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: Vec<u8>,
    pub timestamp_us: u64,
}

/// Source de capture audio (loopback système ou micro).
pub trait AudioCapturer: Send {
    fn format(&self) -> AudioFormat;
    /// Prochaine trame encodée Opus.
    fn next_packet(&mut self) -> Result<AudioPacket>;
}

/// Sortie de lecture audio côté viewer.
pub trait AudioPlayer: Send {
    /// Remet un paquet Opus au jitter buffer / décodeur pour lecture.
    fn play(&mut self, packet: &AudioPacket) -> Result<()>;
}

/// Crée un capteur de l'audio système (loopback).
///
/// Windows : boucle de retour WASAPI sur le périphérique de rendu par défaut,
/// convertie en 48 kHz stéréo et encodée en Opus (trames de 20 ms).
#[cfg(windows)]
pub fn create_system_capturer() -> Result<Box<dyn AudioCapturer>> {
    Ok(Box::new(win::WasapiLoopbackCapturer::new()?))
}

/// Crée un capteur de l'audio système (loopback). Non implémenté sur cet OS.
#[cfg(not(windows))]
pub fn create_system_capturer() -> Result<Box<dyn AudioCapturer>> {
    Err(NdError::NotImplemented(
        "nd-audio::create_system_capturer (CoreAudio/PipeWire à venir, voir plan 08/16)",
    ))
}
