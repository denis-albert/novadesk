//! Capture du **microphone** sous Windows via WASAPI (voix bidirectionnelle),
//! encodée en Opus profil voix. Voir plan 08 §Windows.
//!
//! Séquence : COM (MTA) → `IMMDeviceEnumerator` → périphérique de **capture**
//! par défaut pour les communications (`eCapture`/`eCommunications`) →
//! `IAudioClient` en mode partagé **sans** indicateur de flux (contrairement
//! au loopback système, qui n'est qu'un indicateur de plus) →
//! `IAudioCaptureClient` → conversion 48 kHz ([`crate::convert`]) → mono →
//! trames Opus « voix » de 20 ms (~28 kbps, DTX) ([`crate::codec`]).
//!
//! Tout le `unsafe` FFI (ouverture, drainage, arrêt du flux au `Drop`) est
//! porté par le moteur commun [`crate::win::MoteurCapture`], partagé avec le
//! loopback : ce module reste 100 % sûr.
//!
//! Note honnête : micro muet ou signal nul → le capteur émet des trames de
//! silence valides à cadence régulière, très compactes grâce au DTX.

use nd_proto::Result;
use windows::Win32::Media::Audio::{eCapture, eCommunications};

use crate::codec::EncodeurOpus;
use crate::win::MoteurCapture;
use crate::{AudioCapturer, AudioFormat, AudioPacket};

/// Format de session du micro : 48 kHz **mono** — suffisant pour la voix et
/// moitié moins de données que le stéréo du loopback système.
const FORMAT_MICRO: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 1,
};

/// Capteur du microphone Windows : capture WASAPI partagée + Opus voix.
pub struct WasapiMicCapturer {
    moteur: MoteurCapture,
}

impl WasapiMicCapturer {
    /// Ouvre le micro par défaut (rôle communications) et démarre le flux de
    /// capture (pipeline conversion → 48 kHz mono → Opus voix). Le flux est
    /// arrêté au `Drop` (via le moteur commun).
    pub fn new() -> Result<Self> {
        Ok(WasapiMicCapturer {
            // Mode partagé pur : aucun indicateur de flux (pas de loopback).
            moteur: MoteurCapture::ouvrir(
                eCapture,
                eCommunications,
                0,
                EncodeurOpus::new_voix(FORMAT_MICRO)?,
            )?,
        })
    }
}

impl AudioCapturer for WasapiMicCapturer {
    fn format(&self) -> AudioFormat {
        self.moteur.format()
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        self.moteur.prochaine_trame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{echantillons_par_trame, DecodeurOpus};

    /// Une trame mono connue de 20 ms : sinusoïde 220 Hz à 48 kHz (registre
    /// grave de la voix).
    fn trame_voix_connue() -> Vec<f32> {
        (0..echantillons_par_trame(FORMAT_MICRO))
            .map(|i| {
                let t = i as f32 / FORMAT_MICRO.sample_rate as f32;
                (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.5
            })
            .collect()
    }

    /// Chemin d'encodage voix : une trame connue s'encode en un paquet de
    /// taille « voix » (~28 kbps) et se redécode en une trame pleine.
    #[test]
    fn encodage_voix_trame_connue() {
        let mut enc = EncodeurOpus::new_voix(FORMAT_MICRO).expect("création encodeur voix");
        let pcm = trame_voix_connue();
        assert_eq!(pcm.len(), enc.valeurs_par_trame());

        let paquet = enc.encoder(&pcm).expect("encodage voix");
        // ~28 kbps → ~70 octets pour 20 ms ; marge large pour le VBR.
        assert!(
            !paquet.is_empty() && paquet.len() <= 200,
            "paquet voix inattendu : {} octets",
            paquet.len()
        );

        let mut dec = DecodeurOpus::new(FORMAT_MICRO).expect("création décodeur");
        let decode = dec.decoder(&paquet).expect("décodage");
        assert_eq!(decode.len(), pcm.len());
    }

    /// Avec DTX, une trame de silence reste un paquet valide et minuscule qui
    /// se redécode en une trame pleine (de silence).
    #[test]
    fn silence_voix_dtx_reste_decodable() {
        let mut enc = EncodeurOpus::new_voix(FORMAT_MICRO).expect("création encodeur voix");
        let silence = vec![0.0f32; enc.valeurs_par_trame()];
        let paquet = enc.encoder(&silence).expect("encodage du silence");
        assert!(!paquet.is_empty() && paquet.len() < 64);

        let mut dec = DecodeurOpus::new(FORMAT_MICRO).expect("création décodeur");
        let decode = dec.decoder(&paquet).expect("décodage du silence");
        assert_eq!(decode.len(), echantillons_par_trame(FORMAT_MICRO));
        assert!(decode.iter().all(|v| v.abs() < 0.05));
    }

    /// Construit le capteur sans paniquer ; sans micro (CI headless), l'échec
    /// doit être une erreur propre, jamais une panique.
    #[test]
    fn creation_du_capteur_sans_panique() {
        match WasapiMicCapturer::new() {
            Ok(capteur) => {
                assert_eq!(capteur.format().sample_rate, 48_000);
                assert_eq!(capteur.format().channels, 1);
            }
            Err(e) => eprintln!("wasapi (micro) indisponible ici : {e}"),
        }
    }

    /// `create_microphone_capturer` suit le même contrat que le constructeur.
    #[test]
    fn create_microphone_capturer_sans_panique() {
        match crate::create_microphone_capturer() {
            Ok(capteur) => assert_eq!(capteur.format().channels, 1),
            Err(e) => eprintln!("wasapi (micro) indisponible ici : {e}"),
        }
    }
}
