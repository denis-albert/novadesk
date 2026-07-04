//! `nd-audio` — capture (loopback système + micro) et lecture audio, codec Opus.
//!
//! Transport via datagrammes non fiables + jitter buffer, synchro A/V par horodatage
//! commun avec la vidéo. Détails par OS (WASAPI/CoreAudio/PipeWire) :
//! `../../plan-technique/08-audio.md`.

use nd_proto::{NdError, Result};

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

/// Crée un capteur de l'audio système (loopback). Non implémenté à ce stade.
pub fn create_system_capturer() -> Result<Box<dyn AudioCapturer>> {
    Err(NdError::NotImplemented(
        "nd-audio::create_system_capturer (WASAPI/CoreAudio/PipeWire à venir, voir plan 08/16)",
    ))
}
