//! Capture et lecture audio sous Linux via la **PulseAudio simple API**
//! (`pa_simple`, crates `libpulse-simple-binding` + `libpulse-binding`).
//! Voir plan 08 §Linux.
//!
//! Choix du premier jet : l'API « simple » (flux synchrones bloquants) plutôt
//! que l'API asynchrone ou PipeWire natif — elle est bien plus directe à
//! écrire correctement, et les distributions modernes sous PipeWire exposent
//! exactement la même interface via `pipewire-pulse`. Un backend PipeWire
//! natif (plan 12) pourra la remplacer sans toucher aux traits.
//!
//! Le loopback **système** passe par la source *monitor* du périphérique de
//! sortie par défaut : PulseAudio expose pour chaque sortie (« sink ») une
//! source qui rejoue tout ce qui y est mixé, et le nom magique
//! `@DEFAULT_MONITOR@` désigne celle de la sortie par défaut. Aucune
//! permission particulière n'est requise (contrairement à la capture d'écran
//! Wayland).
//!
//! Gros avantage sur WASAPI : on demande directement le format de session
//! (48 kHz `f32`) dans la spécification du flux et le serveur rééchantillonne
//! et remixe lui-même — ni [`crate::convert::Reechantillonneur`] ni passage
//! par le format de mixage natif ne sont nécessaires. Les conversions
//! octets ↔ `f32` réutilisent [`crate::convert`] (flux demandé en `F32le`,
//! l'encodage petit-boutiste de ces aides).
//!
//! Aucun bloc `unsafe` ici : tout le FFI est porté par les bindings (l'objet
//! [`Simple`] est `Send + Sync`).
//!
//! Note honnête : contrairement au loopback WASAPI, la source monitor
//! continue de délivrer des échantillons (de silence) même quand rien n'est
//! en cours de lecture — la cadence des paquets est donc naturellement
//! régulière, sans complétion artificielle côté client.

use libpulse_binding::def::BufferAttr;
use libpulse_binding::error::PAErr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use nd_proto::{NdError, Result};

use crate::codec::{echantillons_par_trame, DecodeurOpus, EncodeurOpus};
use crate::convert::{f32_vers_octets, octets_vers_f32, FormatEchantillon};
use crate::{AudioCapturer, AudioFormat, AudioPacket, AudioPlayer};

/// Nom de client présenté au serveur PulseAudio (mixeur, `pactl list clients`).
const NOM_APPLICATION: &str = "NovaDesk";

/// Nom magique PulseAudio : source *monitor* de la sortie par défaut.
const MONITEUR_PAR_DEFAUT: &str = "@DEFAULT_MONITOR@";

/// Longueur cible du tampon de lecture côté serveur (millisecondes) — même
/// ordre de grandeur que le tampon WASAPI de 200 ms côté Windows.
const TAMPON_LECTURE_MS: u32 = 200;

/// Format de session du micro : 48 kHz **mono** (voix, comme sous Windows).
const FORMAT_MICRO: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 1,
};

/// Convertit une erreur PulseAudio en [`NdError::Capture`] (le code brut est
/// conservé car `pa_strerror` peut ne rien renvoyer).
fn pulse(contexte: &str, e: PAErr) -> NdError {
    NdError::Capture(format!("pulseaudio : {contexte} : {e} (code {})", e.0))
}

/// Spécification d'échantillonnage PulseAudio pour un format de session :
/// `f32` petit-boutiste (encodage de [`crate::convert`]), le serveur
/// rééchantillonne/remixe vers ou depuis les formats natifs des périphériques.
fn spec_pour(format: AudioFormat) -> Spec {
    Spec {
        format: Format::F32le,
        rate: format.sample_rate,
        channels: format.channels,
    }
}

/// Taille en octets d'une trame de 20 ms au format de session (`f32`).
fn octets_par_trame(format: AudioFormat) -> usize {
    echantillons_par_trame(format) * usize::from(format.channels) * FormatEchantillon::F32.octets()
}

/// Moteur commun de capture PulseAudio : flux d'enregistrement bloquant →
/// PCM `f32` → trames Opus de 20 ms horodatées. Le loopback système
/// ([`PulseLoopbackCapturer`]) et le micro ([`PulseMicCapturer`]) n'en
/// diffèrent que par la source ouverte et le profil de l'encodeur.
struct MoteurCapturePulse {
    connexion: Simple,
    encodeur: EncodeurOpus,
    /// Tampon de lecture réutilisé : exactement une trame de 20 ms d'octets.
    tampon: Vec<u8>,
    /// Échantillons (par canal) déjà émis — horloge média pour l'horodatage.
    echantillons_emis: u64,
    format: AudioFormat,
}

impl MoteurCapturePulse {
    /// Ouvre un flux d'enregistrement sur `source` (`None` = source par
    /// défaut) au format de session de `encodeur`.
    fn ouvrir(source: Option<&str>, description: &str, encodeur: EncodeurOpus) -> Result<Self> {
        let format = encodeur.format();
        // Fragments serveur de la taille d'une trame : le réveil de `read`
        // colle à la cadence de 20 ms. `u32::MAX` = « valeur par défaut du
        // serveur » pour les autres champs (sémantique PulseAudio).
        let attributs = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: octets_par_trame(format) as u32,
        };
        let connexion = Simple::new(
            None, // serveur par défaut
            NOM_APPLICATION,
            Direction::Record,
            source,
            description,
            &spec_pour(format),
            None, // plan de canaux par défaut
            Some(&attributs),
        )
        .map_err(|e| pulse("ouverture du flux de capture", e))?;

        Ok(MoteurCapturePulse {
            connexion,
            tampon: vec![0u8; octets_par_trame(format)],
            encodeur,
            echantillons_emis: 0,
            format,
        })
    }

    /// Prochaine trame Opus de 20 ms. `read` bloque jusqu'à une trame pleine :
    /// la cadence est celle du serveur, sans sondage actif.
    fn prochaine_trame(&mut self) -> Result<AudioPacket> {
        self.connexion
            .read(&mut self.tampon)
            .map_err(|e| pulse("lecture du flux de capture", e))?;
        let pcm = octets_vers_f32(&self.tampon, FormatEchantillon::F32);
        let data = self.encodeur.encoder(&pcm)?;

        // Horloge média : position en échantillons convertie en microsecondes,
        // monotone et sans dérive vis-à-vis du flux (synchro A/V, plan 08).
        let timestamp_us = self.echantillons_emis * 1_000_000 / u64::from(self.format.sample_rate);
        self.echantillons_emis += echantillons_par_trame(self.format) as u64;

        Ok(AudioPacket { data, timestamp_us })
    }
}

/// Capteur de l'audio système Linux : source *monitor* PulseAudio + Opus.
pub struct PulseLoopbackCapturer {
    moteur: MoteurCapturePulse,
}

impl PulseLoopbackCapturer {
    /// Ouvre la source monitor de la sortie par défaut (`@DEFAULT_MONITOR@`)
    /// et démarre le flux (pipeline 48 kHz stéréo → Opus). La connexion est
    /// fermée au `Drop` (par les bindings).
    pub fn new() -> Result<Self> {
        Ok(PulseLoopbackCapturer {
            moteur: MoteurCapturePulse::ouvrir(
                Some(MONITEUR_PAR_DEFAUT),
                "Capture de l'audio système",
                EncodeurOpus::new(AudioFormat::default())?,
            )?,
        })
    }
}

impl AudioCapturer for PulseLoopbackCapturer {
    fn format(&self) -> AudioFormat {
        self.moteur.format
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        self.moteur.prochaine_trame()
    }
}

/// Capteur du microphone Linux : source par défaut PulseAudio + Opus voix.
pub struct PulseMicCapturer {
    moteur: MoteurCapturePulse,
}

impl PulseMicCapturer {
    /// Ouvre la source d'entrée par défaut et démarre le flux de capture
    /// (pipeline 48 kHz mono → Opus voix ~28 kbps, DTX — comme sous Windows).
    pub fn new() -> Result<Self> {
        Ok(PulseMicCapturer {
            moteur: MoteurCapturePulse::ouvrir(
                None, // source par défaut (micro)
                "Capture du microphone",
                EncodeurOpus::new_voix(FORMAT_MICRO)?,
            )?,
        })
    }
}

impl AudioCapturer for PulseMicCapturer {
    fn format(&self) -> AudioFormat {
        self.moteur.format
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        self.moteur.prochaine_trame()
    }
}

/// Lecteur de l'audio système Linux : décodage Opus + flux de lecture
/// PulseAudio sur la sortie par défaut.
pub struct PulsePlayer {
    connexion: Simple,
    decodeur: DecodeurOpus,
}

impl PulsePlayer {
    /// Ouvre un flux de lecture sur la sortie par défaut au format de session
    /// (48 kHz stéréo `f32`) avec un tampon serveur cible de 200 ms.
    pub fn new() -> Result<Self> {
        let format = AudioFormat::default();
        let octets_par_seconde = format.sample_rate as usize
            * usize::from(format.channels)
            * FormatEchantillon::F32.octets();
        // `tlength` borne la latence côté serveur ; le reste aux valeurs par
        // défaut du serveur (`u32::MAX`, sémantique PulseAudio).
        let attributs = BufferAttr {
            maxlength: u32::MAX,
            tlength: (octets_par_seconde * TAMPON_LECTURE_MS as usize / 1000) as u32,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        };
        let connexion = Simple::new(
            None, // serveur par défaut
            NOM_APPLICATION,
            Direction::Playback,
            None, // sortie par défaut
            "Lecture de session",
            &spec_pour(format),
            None, // plan de canaux par défaut
            Some(&attributs),
        )
        .map_err(|e| pulse("ouverture du flux de lecture", e))?;

        Ok(PulsePlayer {
            connexion,
            decodeur: DecodeurOpus::new(format)?,
        })
    }
}

impl AudioPlayer for PulsePlayer {
    /// Décode le paquet Opus et pousse le PCM vers le serveur. `write` bloque
    /// quand le tampon serveur est plein : le débit s'aligne naturellement sur
    /// l'horloge du périphérique. Le lissage réseau (gigue, ordre, trous)
    /// revient au [`crate::jitter::JitterBuffer`] amont ; `timestamp_us` n'est
    /// pas réinterprété ici.
    fn play(&mut self, packet: &AudioPacket) -> Result<()> {
        let pcm = self.decodeur.decoder(&packet.data)?;
        let octets = f32_vers_octets(&pcm, FormatEchantillon::F32);
        self.connexion
            .write(&octets)
            .map_err(|e| pulse("écriture du flux de lecture", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une trame de 20 ms doit peser exactement 7 680 octets en 48 kHz stéréo
    /// `f32` (960 échantillons × 2 canaux × 4 octets).
    #[test]
    fn taille_de_trame_stereo() {
        assert_eq!(octets_par_trame(AudioFormat::default()), 7_680);
    }

    /// La spécification PulseAudio dérivée du format de session est valide.
    #[test]
    fn spec_valide() {
        assert!(spec_pour(AudioFormat::default()).is_valid());
        assert!(spec_pour(FORMAT_MICRO).is_valid());
    }

    /// Construit le capteur loopback sans paniquer ; sans serveur PulseAudio
    /// (CI headless), l'échec doit être une erreur propre, jamais une panique.
    #[test]
    fn creation_du_capteur_loopback_sans_panique() {
        match PulseLoopbackCapturer::new() {
            Ok(capteur) => {
                assert_eq!(capteur.format().sample_rate, 48_000);
                assert_eq!(capteur.format().channels, 2);
            }
            Err(e) => eprintln!("pulseaudio (loopback) indisponible ici : {e}"),
        }
    }

    /// Construit le capteur micro sans paniquer (même contrat).
    #[test]
    fn creation_du_capteur_micro_sans_panique() {
        match PulseMicCapturer::new() {
            Ok(capteur) => assert_eq!(capteur.format().channels, 1),
            Err(e) => eprintln!("pulseaudio (micro) indisponible ici : {e}"),
        }
    }

    /// Construit le lecteur sans paniquer ; s'il y a un serveur, joue une
    /// trame de silence de bout en bout (Opus → PulseAudio).
    #[test]
    fn creation_du_lecteur_et_trame_de_silence() {
        let mut enc = EncodeurOpus::new(AudioFormat::default()).expect("création encodeur");
        let silence = vec![0.0f32; enc.valeurs_par_trame()];
        let paquet = AudioPacket {
            data: enc.encoder(&silence).expect("encodage du silence"),
            timestamp_us: 0,
        };
        match PulsePlayer::new() {
            Ok(mut lecteur) => lecteur
                .play(&paquet)
                .expect("lecture d'une trame de silence"),
            Err(e) => eprintln!("pulseaudio (lecture) indisponible ici : {e}"),
        }
    }
}
