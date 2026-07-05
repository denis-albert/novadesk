//! Enveloppe **sûre** du codec Opus : trames de 20 ms, PCM `f32` entrelacé.
//!
//! L'encodeur consomme exactement une trame (20 ms × fréquence × canaux
//! valeurs `f32`) et produit un paquet Opus autonome, prêt pour le transport
//! en datagrammes non fiables (plan 08). Le décodeur sert au viewer et à la
//! vérification de bout en bout (exemple `audio_probe`).
//!
//! Le FFI `libopus_sys` (libopus 1.5 vendoré, lien statique) est confiné à ce
//! module : chaque bloc `unsafe` est justifié par un commentaire `// SAFETY:`
//! et l'API publique reste 100 % sûre.
#![allow(unsafe_code)]

use std::ffi::CStr;
use std::os::raw::c_int;

use nd_proto::{NdError, Result};

use crate::AudioFormat;

/// Durée d'une trame Opus produite par le pipeline (millisecondes).
pub const TRAME_MS: u32 = 20;

/// Taille maximale d'un paquet Opus encodé (octets) — large pour 20 ms stéréo.
const PAQUET_MAX: usize = 4000;

/// Durée maximale d'une trame Opus admise au décodage (millisecondes).
const TRAME_DECODAGE_MAX_MS: usize = 120;

/// Nombre d'échantillons PCM **par canal** dans une trame de 20 ms.
#[must_use]
pub fn echantillons_par_trame(format: AudioFormat) -> usize {
    format.sample_rate as usize * TRAME_MS as usize / 1000
}

/// Message d'erreur libopus pour le code renvoyé par l'API C.
fn texte_erreur(code: c_int) -> String {
    // SAFETY : opus_strerror renvoie une chaîne C statique valide (ou nulle)
    // pour n'importe quel code, sans transfert de propriété.
    let ptr = unsafe { libopus_sys::opus_strerror(code) };
    if ptr.is_null() {
        format!("code {code}")
    } else {
        // SAFETY : chaîne statique terminée par NUL renvoyée par libopus.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Construit un [`NdError::Codec`] à partir d'un code d'erreur libopus.
fn codec(contexte: &str, code: c_int) -> NdError {
    NdError::Codec(format!("opus : {contexte} : {}", texte_erreur(code)))
}

/// Valide le nombre de canaux du format pour libopus (mono ou stéréo).
fn canaux(format: AudioFormat) -> Result<c_int> {
    match format.channels {
        1 | 2 => Ok(c_int::from(format.channels)),
        n => Err(NdError::Codec(format!(
            "opus : {n} canaux non supportés (mono ou stéréo)"
        ))),
    }
}

/// Encodeur Opus par trames de 20 ms (entrée : PCM `f32` entrelacé).
pub struct EncodeurOpus {
    etat: *mut libopus_sys::OpusEncoder,
    format: AudioFormat,
    echantillons_trame: usize,
}

// SAFETY : l'état libopus n'a aucune affinité de thread ; il est possédé
// exclusivement par cette structure (jamais partagé, accès via `&mut self`),
// le déplacer entre threads est donc sûr.
unsafe impl Send for EncodeurOpus {}

impl EncodeurOpus {
    /// Crée un encodeur pour le format donné (48 kHz recommandé, natif Opus).
    pub fn new(format: AudioFormat) -> Result<Self> {
        let nb_canaux = canaux(format)?;
        let mut code: c_int = 0;
        // SAFETY : paramètres validés (canaux 1-2, fréquence contrôlée par
        // libopus qui signale toute valeur invalide via `code`) ; `code` est
        // un pointeur de sortie valide le temps de l'appel.
        let etat = unsafe {
            libopus_sys::opus_encoder_create(
                format.sample_rate as i32,
                nb_canaux,
                libopus_sys::OPUS_APPLICATION_AUDIO as c_int,
                &mut code,
            )
        };
        if etat.is_null() || code != libopus_sys::OPUS_OK as c_int {
            return Err(codec("création de l'encodeur", code));
        }
        Ok(EncodeurOpus {
            etat,
            format,
            echantillons_trame: echantillons_par_trame(format),
        })
    }

    /// Nombre de valeurs `f32` attendues par appel (échantillons × canaux).
    #[must_use]
    pub fn valeurs_par_trame(&self) -> usize {
        self.echantillons_trame * usize::from(self.format.channels)
    }

    /// Encode exactement une trame de 20 ms de PCM `f32` entrelacé.
    pub fn encoder(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        let attendu = self.valeurs_par_trame();
        if pcm.len() != attendu {
            return Err(NdError::Codec(format!(
                "opus : trame de {} valeurs au lieu de {attendu}",
                pcm.len()
            )));
        }
        let mut sortie = vec![0u8; PAQUET_MAX];
        // SAFETY : `pcm` contient exactement `echantillons_trame` frames de
        // `channels` valeurs ; `sortie` fait PAQUET_MAX octets ; `etat` est
        // valide (créé dans `new`, détruit dans `Drop`).
        let n = unsafe {
            libopus_sys::opus_encode_float(
                self.etat,
                pcm.as_ptr(),
                self.echantillons_trame as c_int,
                sortie.as_mut_ptr(),
                PAQUET_MAX as i32,
            )
        };
        if n < 0 {
            return Err(codec("encodage", n));
        }
        sortie.truncate(n as usize);
        Ok(sortie)
    }
}

impl Drop for EncodeurOpus {
    fn drop(&mut self) {
        // SAFETY : état créé par opus_encoder_create, détruit une seule fois.
        unsafe { libopus_sys::opus_encoder_destroy(self.etat) };
    }
}

/// Décodeur Opus symétrique (lecture côté viewer, vérification en test).
pub struct DecodeurOpus {
    etat: *mut libopus_sys::OpusDecoder,
    format: AudioFormat,
}

// SAFETY : mêmes garanties que pour [`EncodeurOpus`] : état possédé en
// exclusivité, aucune affinité de thread côté libopus.
unsafe impl Send for DecodeurOpus {}

impl DecodeurOpus {
    /// Crée un décodeur pour le format donné.
    pub fn new(format: AudioFormat) -> Result<Self> {
        let nb_canaux = canaux(format)?;
        let mut code: c_int = 0;
        // SAFETY : paramètres validés comme pour l'encodeur ; `code` est un
        // pointeur de sortie valide le temps de l'appel.
        let etat = unsafe {
            libopus_sys::opus_decoder_create(format.sample_rate as i32, nb_canaux, &mut code)
        };
        if etat.is_null() || code != libopus_sys::OPUS_OK as c_int {
            return Err(codec("création du décodeur", code));
        }
        Ok(DecodeurOpus { etat, format })
    }

    /// Décode un paquet Opus vers du PCM `f32` entrelacé.
    pub fn decoder(&mut self, paquet: &[u8]) -> Result<Vec<f32>> {
        let nb_canaux = usize::from(self.format.channels);
        // Tampon dimensionné pour la plus longue trame Opus admise (120 ms).
        let capacite_par_canal = self.format.sample_rate as usize * TRAME_DECODAGE_MAX_MS / 1000;
        let mut pcm = vec![0.0f32; capacite_par_canal * nb_canaux];
        // SAFETY : `paquet` fait `paquet.len()` octets ; `pcm` peut recevoir
        // `capacite_par_canal` frames de `nb_canaux` valeurs ; `etat` est
        // valide. `fec = 0` : pas de dissimulation de perte ici.
        let n = unsafe {
            libopus_sys::opus_decode_float(
                self.etat,
                paquet.as_ptr(),
                paquet.len() as i32,
                pcm.as_mut_ptr(),
                capacite_par_canal as c_int,
                0,
            )
        };
        if n < 0 {
            return Err(codec("décodage", n));
        }
        pcm.truncate(n as usize * nb_canaux);
        Ok(pcm)
    }
}

impl Drop for DecodeurOpus {
    fn drop(&mut self) {
        // SAFETY : état créé par opus_decoder_create, détruit une seule fois.
        unsafe { libopus_sys::opus_decoder_destroy(self.etat) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une trame de 20 ms : sinusoïde 440 Hz stéréo à 48 kHz.
    fn trame_sinus(format: AudioFormat) -> Vec<f32> {
        let par_canal = echantillons_par_trame(format);
        let mut pcm = Vec::with_capacity(par_canal * usize::from(format.channels));
        for i in 0..par_canal {
            let t = i as f32 / format.sample_rate as f32;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            for _ in 0..format.channels {
                pcm.push(v);
            }
        }
        pcm
    }

    #[test]
    fn trame_par_defaut_960_echantillons() {
        assert_eq!(echantillons_par_trame(AudioFormat::default()), 960);
    }

    #[test]
    fn aller_retour_encode_decode() {
        let format = AudioFormat::default();
        let mut enc = EncodeurOpus::new(format).expect("création encodeur");
        let mut dec = DecodeurOpus::new(format).expect("création décodeur");

        let pcm = trame_sinus(format);
        let paquet = enc.encoder(&pcm).expect("encodage");
        assert!(!paquet.is_empty() && paquet.len() <= 4000);

        let decode = dec.decoder(&paquet).expect("décodage");
        // Une trame de 20 ms doit ressortir intacte en nombre d'échantillons.
        assert_eq!(decode.len(), pcm.len());
    }

    #[test]
    fn trame_de_silence_compacte() {
        let format = AudioFormat::default();
        let mut enc = EncodeurOpus::new(format).expect("création encodeur");
        let silence = vec![0.0f32; enc.valeurs_par_trame()];
        let paquet = enc.encoder(&silence).expect("encodage du silence");
        // Le silence se code en quelques octets seulement.
        assert!(!paquet.is_empty() && paquet.len() < 32);
    }

    #[test]
    fn taille_de_trame_invalide_refusee() {
        let mut enc = EncodeurOpus::new(AudioFormat::default()).expect("création encodeur");
        assert!(enc.encoder(&[0.0; 100]).is_err());
    }

    #[test]
    fn format_a_trois_canaux_refuse() {
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 3,
        };
        assert!(EncodeurOpus::new(format).is_err());
        assert!(DecodeurOpus::new(format).is_err());
    }

    #[test]
    fn frequence_invalide_refusee() {
        let format = AudioFormat {
            sample_rate: 44_100, // pas une fréquence Opus (8/12/16/24/48 kHz)
            channels: 2,
        };
        assert!(EncodeurOpus::new(format).is_err());
    }
}
