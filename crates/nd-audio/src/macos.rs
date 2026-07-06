//! Restitution audio sous macOS via **CoreAudio** (AudioUnit « default
//! output », crate `coreaudio-rs`). Voir plan 08 §macOS.
//!
//! # Lecture ([`CoreAudioPlayer`])
//!
//! Séquence : `AudioUnit` de type `DefaultOutput` (AUHAL branché sur la sortie
//! choisie dans Réglages Système) → format de flux **côté entrée de l'unité**
//! fixé au format de session (48 kHz stéréo `f32` entrelacé, l'AUHAL
//! rééchantillonne lui-même vers le format matériel) → rappel de rendu tiré
//! par CoreAudio qui vide une file partagée de PCM. `play` décode chaque
//! paquet Opus ([`DecodeurOpus`]) et alimente cette file ; en sous-régime le
//! rappel complète avec du silence, en sur-régime les échantillons les plus
//! anciens sont jetés (rattrapage). Le lissage réseau (gigue, ordre, trous)
//! revient au [`crate::jitter::JitterBuffer`] amont.
//!
//! Le `unsafe` FFI est porté par `coreaudio-rs` (l'`AudioUnit` est `Send`,
//! son `Drop` arrête et libère l'unité) : ce module reste 100 % sûr.
//!
//! # Capture de l'audio système — limite assumée
//!
//! macOS n'offre **aucune API publique de loopback** avant macOS 13 :
//! contrairement à WASAPI (`AUDCLNT_STREAMFLAGS_LOOPBACK`) ou aux sources
//! *monitor* de PulseAudio, CoreAudio ne sait pas rejouer le mix de sortie.
//! Les options honnêtes sont :
//!
//! * **macOS ≥ 13** : ScreenCaptureKit fournit l'audio système
//!   (`SCStreamConfiguration.capturesAudio`) avec le consentement
//!   « Enregistrement de l'écran » — c'est la voie prévue, à intégrer avec la
//!   capture d'écran ScreenCaptureKit (plans 02/08) pour partager flux et
//!   permission ;
//! * **macOS < 13** : uniquement via un périphérique virtuel tiers type
//!   BlackHole/Loopback que l'utilisateur installe et route lui-même — hors
//!   périmètre d'un client « sans installation pilote ».
//!
//! En attendant l'intégration ScreenCaptureKit, `create_system_capturer()`
//! renvoie donc [`nd_proto::NdError::NotImplemented`] sur macOS (de même que
//! le micro, qui passera par l'AUHAL en direction entrée).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
use nd_proto::{NdError, Result};

use crate::codec::DecodeurOpus;
use crate::{AudioFormat, AudioPacket, AudioPlayer};

/// Profondeur maximale de la file de PCM en attente de rendu (millisecondes).
/// Au-delà, les échantillons les plus anciens sont jetés : la latence de
/// restitution reste bornée même si l'amont pousse trop vite.
const FILE_MAX_MS: usize = 500;

/// Convertit une erreur CoreAudio en [`NdError::Capture`].
fn coreaudio(contexte: &str, e: coreaudio::Error) -> NdError {
    NdError::Capture(format!("coreaudio : {contexte} : {e}"))
}

/// File de PCM `f32` **stéréo entrelacé** partagée entre `play` (producteur)
/// et le rappel de rendu CoreAudio (consommateur, thread temps réel).
type FilePcm = Arc<Mutex<VecDeque<f32>>>;

/// Lecteur de l'audio système macOS : décodage Opus + AudioUnit de sortie.
pub struct CoreAudioPlayer {
    /// Unité de sortie ; conservée pour la durée de vie du flux (son `Drop`
    /// arrête le rendu et libère l'unité).
    _unite: AudioUnit,
    decodeur: DecodeurOpus,
    file: FilePcm,
    /// Borne de la file en nombre de valeurs `f32` ([`FILE_MAX_MS`]).
    file_max: usize,
}

impl CoreAudioPlayer {
    /// Ouvre la sortie par défaut, impose le format de session (48 kHz stéréo
    /// `f32` entrelacé) à l'entrée de l'unité et démarre le rendu.
    pub fn new() -> Result<Self> {
        let format = AudioFormat::default();

        let mut unite = AudioUnit::new(IOType::DefaultOutput)
            .map_err(|e| coreaudio("création de l'unité de sortie", e))?;

        // Format de flux côté *entrée* de l'unité de sortie : c'est le format
        // dans lequel notre rappel fournit le PCM ; l'AUHAL convertit ensuite
        // vers le format matériel (fréquence, voies) de la sortie réelle.
        let flux = StreamFormat {
            sample_rate: f64::from(format.sample_rate),
            sample_format: SampleFormat::F32,
            // Entrelacé : pas d'indicateur IS_NON_INTERLEAVED.
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: u32::from(format.channels),
        };
        unite
            .set_stream_format(flux, Scope::Input, Element::Output)
            .map_err(|e| coreaudio("réglage du format de flux", e))?;

        let file: FilePcm = Arc::new(Mutex::new(VecDeque::new()));
        let partage = Arc::clone(&file);

        // Rappel de rendu : CoreAudio tire les échantillons à l'horloge du
        // périphérique ; la file se vide par l'avant, silence en sous-régime.
        type Rappel = render_callback::Args<data::Interleaved<f32>>;
        unite
            .set_render_callback(move |args: Rappel| {
                let Rappel {
                    num_frames, data, ..
                } = args;
                // Un mutex empoisonné (panique côté producteur) arrête
                // proprement le rendu plutôt que de paniquer dans le rappel.
                let mut file = partage.lock().map_err(|_| ())?;
                let besoin = num_frames * data.channels;
                for valeur in data.buffer.iter_mut().take(besoin) {
                    *valeur = file.pop_front().unwrap_or(0.0);
                }
                Ok(())
            })
            .map_err(|e| coreaudio("installation du rappel de rendu", e))?;

        unite
            .start()
            .map_err(|e| coreaudio("démarrage du rendu", e))?;

        Ok(CoreAudioPlayer {
            _unite: unite,
            decodeur: DecodeurOpus::new(format)?,
            file,
            file_max: format.sample_rate as usize * usize::from(format.channels) * FILE_MAX_MS
                / 1000,
        })
    }
}

impl AudioPlayer for CoreAudioPlayer {
    /// Décode le paquet Opus et pousse le PCM dans la file du rappel de
    /// rendu ; `timestamp_us` n'est pas réinterprété ici (jitter buffer en
    /// amont). Ne bloque jamais : en sur-régime, le plus ancien est jeté.
    fn play(&mut self, packet: &AudioPacket) -> Result<()> {
        let pcm = self.decodeur.decoder(&packet.data)?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| NdError::Capture("coreaudio : file de rendu empoisonnée".into()))?;
        // Borne la latence : on jette les échantillons les plus anciens
        // (rattrapage) plutôt que de laisser la file croître sans limite.
        let excedent = (file.len() + pcm.len()).saturating_sub(self.file_max);
        if excedent > 0 {
            file.drain(..excedent.min(file.len()));
        }
        file.extend(pcm);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::EncodeurOpus;

    /// Une trame Opus de silence (20 ms, 48 kHz stéréo) prête à jouer.
    fn paquet_de_silence() -> AudioPacket {
        let mut enc = EncodeurOpus::new(AudioFormat::default()).expect("création encodeur");
        let silence = vec![0.0f32; enc.valeurs_par_trame()];
        AudioPacket {
            data: enc.encoder(&silence).expect("encodage du silence"),
            timestamp_us: 0,
        }
    }

    /// Construit le lecteur sans paniquer ; s'il y a une sortie audio, joue
    /// une trame de silence de bout en bout (Opus → file → CoreAudio).
    #[test]
    fn creation_du_lecteur_et_trame_de_silence() {
        match CoreAudioPlayer::new() {
            Ok(mut lecteur) => {
                lecteur
                    .play(&paquet_de_silence())
                    .expect("lecture d'une trame de silence");
            }
            // Machine sans sortie audio (CI headless) : l'échec doit être une
            // erreur propre, jamais une panique.
            Err(e) => eprintln!("coreaudio (rendu) indisponible ici : {e}"),
        }
    }

    /// La file est bornée : pousser bien plus que `file_max` ne fait pas
    /// croître la latence au-delà de la borne.
    #[test]
    fn file_de_rendu_bornee() {
        if let Ok(mut lecteur) = CoreAudioPlayer::new() {
            let paquet = paquet_de_silence();
            // ~2 s de silence poussés d'un trait (100 trames de 20 ms).
            for _ in 0..100 {
                lecteur.play(&paquet).expect("lecture");
            }
            let occupation = lecteur.file.lock().expect("verrou").len();
            assert!(
                occupation <= lecteur.file_max,
                "file de rendu non bornée : {occupation} > {}",
                lecteur.file_max
            );
        } else {
            eprintln!("coreaudio (rendu) indisponible ici : test de borne sauté");
        }
    }
}
