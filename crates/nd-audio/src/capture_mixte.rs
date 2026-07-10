//! Capteur **composite** micro + système : mélange les deux flux capturés en un
//! seul flux d'émission stéréo (plan 08, transmission du micro par-dessus l'audio
//! système).
//!
//! `nd-audio` sait déjà capturer le système (loopback) et le micro séparément,
//! mais chacun produit des paquets **Opus déjà encodés**. Pour les émettre
//! ensemble, [`CapteurMixte`] enchaîne, par trame de 20 ms :
//!
//! 1. capture d'une trame système et d'une trame micro (les deux capteurs
//!    sous-jacents restent ceux de la racine du crate — aucune ré-implémentation
//!    de la capture) ;
//! 2. décodage Opus de chacune vers du PCM `f32` ;
//! 3. mise au format de sortie **stéréo** (le micro mono est dédoublé
//!    gauche/droite via [`crate::convert::vers_stereo`]) ;
//! 4. mélange borné par [`crate::mixing::mix`] : somme pondérée par les gains
//!    puis écrêtage doux ([`crate::mixing::soft_clip`]) — anti-saturation ;
//! 5. ré-encodage Opus (profil audio générique) de la trame mélangée.
//!
//! Sortie **stéréo 48 kHz** (le format système par défaut) : le mélange se glisse
//! ainsi dans le chemin de lecture système existant sans rien y changer.
//!
//! Comme `mixing`/`convert`/`codec`, ce module est **100 % indépendant de l'OS**
//! (il ne manipule que des [`AudioCapturer`] injectés) : il se teste partout,
//! sans périphérique réel (voir les tests de ce module).

use nd_proto::Result;

use crate::codec::{DecodeurOpus, EncodeurOpus};
use crate::convert::vers_stereo;
use crate::mixing::mix;
use crate::{AudioCapturer, AudioFormat, AudioPacket};

/// Gain appliqué à la piste **système** avant mélange.
///
/// Gain plein : quand une seule des deux sources est active (cas courant — soit
/// on parle, soit un son système joue), sa sonie est intégralement préservée.
/// Les deux à pleine échelle simultanément somment jusqu'à 2.0, ramenés sous
/// ±1 par le [`crate::mixing::soft_clip`] de [`mix`] (anti-saturation).
pub const GAIN_SYSTEME: f32 = 1.0;

/// Gain appliqué à la piste **micro** avant mélange (voir [`GAIN_SYSTEME`] pour
/// la garantie d'anti-saturation ; symétrique par défaut).
pub const GAIN_MICRO: f32 = 1.0;

/// Capteur composite : deux capteurs Opus (système + micro) mélangés en une
/// unique piste d'émission stéréo.
///
/// Construit par [`CapteurMixte::nouveau`] à partir de deux [`AudioCapturer`]
/// déjà ouverts (typiquement [`crate::create_system_capturer`] et
/// [`crate::create_microphone_capturer`]). Implémente [`AudioCapturer`], donc
/// s'utilise partout où un capteur simple est attendu (dont
/// [`crate::EmetteurAudio::nouveau`]).
pub struct CapteurMixte {
    systeme: Box<dyn AudioCapturer>,
    micro: Box<dyn AudioCapturer>,
    dec_systeme: DecodeurOpus,
    dec_micro: DecodeurOpus,
    encodeur: EncodeurOpus,
    canaux_systeme: u8,
    canaux_micro: u8,
    format: AudioFormat,
}

impl CapteurMixte {
    /// Format de sortie du mélange : **stéréo 48 kHz** (le profil système).
    const FORMAT_SORTIE: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };

    /// Construit le capteur composite autour des deux capteurs fournis.
    ///
    /// Les décodeurs sont dérivés du format de chaque capteur ; l'encodeur de
    /// sortie est réglé sur [`Self::FORMAT_SORTIE`] (stéréo 48 kHz, profil audio
    /// générique — le mélange contient de l'audio système arbitraire, pas
    /// seulement de la voix). Les deux capteurs sont supposés produire des
    /// trames de 20 ms à 48 kHz (ce que garantissent les capteurs de la racine).
    pub fn nouveau(systeme: Box<dyn AudioCapturer>, micro: Box<dyn AudioCapturer>) -> Result<Self> {
        let f_systeme = systeme.format();
        let f_micro = micro.format();
        Ok(CapteurMixte {
            dec_systeme: DecodeurOpus::new(f_systeme)?,
            dec_micro: DecodeurOpus::new(f_micro)?,
            encodeur: EncodeurOpus::new(Self::FORMAT_SORTIE)?,
            canaux_systeme: f_systeme.channels,
            canaux_micro: f_micro.channels,
            format: Self::FORMAT_SORTIE,
            systeme,
            micro,
        })
    }
}

impl AudioCapturer for CapteurMixte {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        // Le système est le plancher garanti du flux : son échec interrompt la
        // trame (comme n'importe quel capteur simple).
        let paquet_systeme = self.systeme.next_packet()?;
        let pcm_systeme = self.dec_systeme.decoder(&paquet_systeme.data)?;
        let systeme_stereo = vers_stereo(&pcm_systeme, usize::from(self.canaux_systeme));

        // Le micro est **best-effort** : une trame micro défaillante
        // (périphérique retiré à chaud, erreur de décodage ponctuelle) est
        // remplacée par du silence — le système continue de sortir *sans
        // coupure ni panique*. Le repli durable de source (statut
        // `micro_disponible`) reste du ressort d'[`crate::AudioSession`].
        let micro_stereo = match self.micro.next_packet() {
            Ok(paquet_micro) => match self.dec_micro.decoder(&paquet_micro.data) {
                Ok(pcm_micro) => vers_stereo(&pcm_micro, usize::from(self.canaux_micro)),
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };

        // Somme pondérée bornée (soft-clip), puis calage sur une trame stéréo
        // pleine : `resize` tronque un surplus improbable et complète par du
        // silence une source plus courte, garantissant la taille exacte exigée
        // par l'encodeur.
        let mut melange = mix(
            &[systeme_stereo.as_slice(), micro_stereo.as_slice()],
            &[GAIN_SYSTEME, GAIN_MICRO],
        );
        melange.resize(self.encodeur.valeurs_par_trame(), 0.0);

        let data = self.encodeur.encoder(&melange)?;
        // Horodatage média : celui de la piste système (référence stéréo).
        Ok(AudioPacket {
            data,
            timestamp_us: paquet_systeme.timestamp_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{echantillons_par_trame, EncodeurOpus};
    use crate::mixing::SEUIL_SOFT_CLIP;

    /// Capteur synthétique : encode une sinusoïde continue en Opus, trame par
    /// trame, avec un horodatage média régulier (20 ms). Remplace un
    /// périphérique réel pour éprouver le mélange sans matériel.
    struct CapteurSinus {
        format: AudioFormat,
        enc: EncodeurOpus,
        freq: f32,
        trame: u64,
    }

    impl CapteurSinus {
        fn nouveau(format: AudioFormat, freq: f32) -> Self {
            CapteurSinus {
                format,
                enc: EncodeurOpus::new(format).expect("encodeur"),
                freq,
                trame: 0,
            }
        }
    }

    impl AudioCapturer for CapteurSinus {
        fn format(&self) -> AudioFormat {
            self.format
        }

        fn next_packet(&mut self) -> Result<AudioPacket> {
            let par_canal = echantillons_par_trame(self.format);
            let base = self.trame as usize * par_canal;
            let mut pcm = Vec::with_capacity(self.enc.valeurs_par_trame());
            for i in 0..par_canal {
                let t = (base + i) as f32 / self.format.sample_rate as f32;
                let v = (std::f32::consts::TAU * self.freq * t).sin() * 0.5;
                for _ in 0..self.format.channels {
                    pcm.push(v);
                }
            }
            let data = self.enc.encoder(&pcm)?;
            let timestamp_us =
                self.trame * par_canal as u64 * 1_000_000 / u64::from(self.format.sample_rate);
            self.trame += 1;
            Ok(AudioPacket { data, timestamp_us })
        }
    }

    /// Capteur qui échoue toujours (micro simulé absent en cours de flux).
    struct CapteurDefaillant {
        format: AudioFormat,
    }

    impl AudioCapturer for CapteurDefaillant {
        fn format(&self) -> AudioFormat {
            self.format
        }

        fn next_packet(&mut self) -> Result<AudioPacket> {
            Err(nd_proto::NdError::Capture("micro simulé défaillant".into()))
        }
    }

    const FORMAT_MICRO: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 1,
    };

    #[test]
    fn sortie_stereo_decodable_et_horodatee() {
        let systeme = Box::new(CapteurSinus::nouveau(AudioFormat::default(), 440.0));
        let micro = Box::new(CapteurSinus::nouveau(FORMAT_MICRO, 220.0));
        let mut mixte = CapteurMixte::nouveau(systeme, micro).expect("capteur mixte");

        // Le format annoncé est stéréo 48 kHz.
        assert_eq!(mixte.format().channels, 2);
        assert_eq!(mixte.format().sample_rate, 48_000);

        let p0 = mixte.next_packet().expect("trame 0");
        assert!(!p0.data.is_empty());
        assert_eq!(p0.timestamp_us, 0);
        let p1 = mixte.next_packet().expect("trame 1");
        assert_eq!(p1.timestamp_us, 20_000);

        // La trame mélangée se redécode en une trame stéréo pleine de 20 ms.
        let mut dec = DecodeurOpus::new(AudioFormat::default()).expect("décodeur");
        let pcm = dec.decoder(&p0.data).expect("décodage");
        assert_eq!(
            pcm.len(),
            echantillons_par_trame(AudioFormat::default()) * 2
        );
    }

    #[test]
    fn melange_micro_systeme_borne_sous_pleine_echelle() {
        // Deux flux pleine échelle mélangés aux gains exacts de `CapteurMixte` :
        // la somme (jusqu'à 2.0) est ramenée sous ±1 par le soft-clip, sans
        // jamais dépasser — preuve chiffrée de l'anti-saturation.
        let systeme = vec![1.0f32; 1920];
        let micro = vec![1.0f32; 1920];
        let sortie = mix(
            &[systeme.as_slice(), micro.as_slice()],
            &[GAIN_SYSTEME, GAIN_MICRO],
        );
        let pic = sortie.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(pic <= 1.0, "dépassement : pic = {pic}");
        assert!(pic > SEUIL_SOFT_CLIP, "le pic doit approcher 1 : {pic}");
    }

    #[test]
    fn micro_defaillant_ne_coupe_pas_le_systeme() {
        // Micro qui échoue à chaque trame : le composite émet quand même une
        // trame stéréo valide (le système seul), sans panique ni erreur.
        let systeme = Box::new(CapteurSinus::nouveau(AudioFormat::default(), 440.0));
        let micro = Box::new(CapteurDefaillant {
            format: FORMAT_MICRO,
        });
        let mut mixte = CapteurMixte::nouveau(systeme, micro).expect("capteur mixte");

        let p = mixte.next_packet().expect("trame malgré micro KO");
        assert!(!p.data.is_empty());
        let mut dec = DecodeurOpus::new(AudioFormat::default()).expect("décodeur");
        assert_eq!(
            dec.decoder(&p.data).expect("décodage").len(),
            echantillons_par_trame(AudioFormat::default()) * 2
        );
    }
}
