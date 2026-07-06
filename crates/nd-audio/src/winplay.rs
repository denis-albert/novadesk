//! Restitution audio sous Windows : rendu WASAPI en mode partagé, alimenté
//! par des paquets Opus décodés localement. Voir plan 08 §7 (Windows).
//!
//! Séquence : COM (MTA) → `IMMDeviceEnumerator` → périphérique de **rendu**
//! par défaut → `IAudioClient` (mode partagé, cadencé par sondage du niveau
//! de remplissage) → `IAudioRenderClient`. Chaque paquet est décodé par
//! [`DecodeurOpus`] (48 kHz stéréo `f32`), converti au format de mixage du
//! périphérique ([`crate::convert`]) puis écrit dans le tampon de rendu.
//!
//! Ce module concentre le `unsafe` FFI de la lecture audio Windows ; il est
//! isolé derrière le trait [`AudioPlayer`] pour que le reste du moteur reste
//! sûr. Le lissage temporel (gigue, réordonnancement, trous) est du ressort
//! du [`crate::jitter::JitterBuffer`], en amont de `play`.
#![allow(unsafe_code)]

use std::time::{Duration, Instant};

use nd_proto::{NdError, Result};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

use crate::codec::DecodeurOpus;
use crate::convert::{depuis_stereo, f32_vers_octets, Reechantillonneur};
use crate::win::{
    initialiser_com, lire_format_mix, wasapi, FormatMix, DUREE_TAMPON_HNS, PAUSE_SONDAGE,
};
use crate::{AudioFormat, AudioPacket, AudioPlayer};

/// Attente maximale d'espace libre dans le tampon de rendu avant abandon
/// (le tampon fait 200 ms, une trame 20 ms : ne bloque jamais en pratique).
const ATTENTE_TAMPON_MAX: Duration = Duration::from_millis(500);

/// Lecteur de l'audio système Windows : décodage Opus + rendu WASAPI partagé.
pub struct WasapiPlayer {
    client: IAudioClient,
    rendu: IAudioRenderClient,
    mix: FormatMix,
    /// Capacité totale du tampon de rendu, en frames.
    tampon_frames: u32,
    decodeur: DecodeurOpus,
    /// 48 kHz (sortie Opus) → fréquence du format de mixage.
    reechantillonneur: Reechantillonneur,
}

// SAFETY : les interfaces WASAPI (`IAudioClient`, `IAudioRenderClient`) sont
// documentées par Microsoft comme *free-threaded* et créées après une
// initialisation COM en MTA ; `windows` 0.58 ne génère pas le marqueur `Send`
// pour elles, on l'affirme donc manuellement (même justification que pour
// `WasapiLoopbackCapturer`). Les autres champs sont `Send` par eux-mêmes.
unsafe impl Send for WasapiPlayer {}

impl WasapiPlayer {
    /// Ouvre le périphérique de rendu par défaut en mode partagé et démarre le
    /// flux (pipeline Opus → 48 kHz stéréo → format de mixage → tampon WASAPI).
    pub fn new() -> Result<Self> {
        initialiser_com()?;

        // Énumérateur de périphériques → sortie de rendu par défaut.
        // SAFETY : appels COM standards après initialisation ; les types de
        // sortie sont gérés par windows-rs.
        let enumerateur: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(wasapi)?;
        // SAFETY : énumérateur valide ; on demande la sortie de rendu console.
        let peripherique: IMMDevice =
            unsafe { enumerateur.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(wasapi)?;
        // SAFETY : activation COM de l'interface client audio sur le périphérique.
        let client: IAudioClient =
            unsafe { peripherique.Activate(CLSCTX_ALL, None) }.map_err(wasapi)?;

        // Format de mixage natif, alloué par COM (à libérer via CoTaskMemFree).
        // SAFETY : GetMixFormat renvoie un pointeur possédé par l'appelant.
        let ptr_format = unsafe { client.GetMixFormat() }.map_err(wasapi)?;
        if ptr_format.is_null() {
            return Err(NdError::Capture(
                "wasapi (rendu) : GetMixFormat a renvoyé nul".into(),
            ));
        }
        // SAFETY : pointeur non nul renvoyé par GetMixFormat, structure valide.
        let mix = unsafe { lire_format_mix(ptr_format) };
        // Initialisation en mode partagé sur le format de mixage (aucun flag :
        // cadencement par sondage de GetCurrentPadding, tampon de 200 ms).
        // SAFETY : `ptr_format` reste valide pendant l'appel.
        let initialisation = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                DUREE_TAMPON_HNS,
                0,
                ptr_format,
                None,
            )
        };
        // SAFETY : pointeur alloué par GetMixFormat, libéré une seule fois ici.
        unsafe { CoTaskMemFree(Some(ptr_format.cast())) };
        let mix = mix?;
        initialisation.map_err(wasapi)?;

        // SAFETY : le client vient d'être initialisé ; lectures d'état et
        // service de rendu standards.
        let tampon_frames = unsafe { client.GetBufferSize() }.map_err(wasapi)?;
        // SAFETY : client initialisé ; service de rendu standard.
        let rendu: IAudioRenderClient = unsafe { client.GetService() }.map_err(wasapi)?;
        // SAFETY : démarre le flux ; arrêté dans Drop.
        unsafe { client.Start() }.map_err(wasapi)?;

        let format = AudioFormat::default();
        Ok(WasapiPlayer {
            reechantillonneur: Reechantillonneur::new(format.sample_rate, mix.frequence),
            decodeur: DecodeurOpus::new(format)?,
            client,
            rendu,
            mix,
            tampon_frames,
        })
    }

    /// Écrit des octets PCM (déjà au format de mixage) dans le tampon de
    /// rendu, par morceaux selon l'espace libre.
    fn ecrire_pcm(&mut self, octets: &[u8]) -> Result<()> {
        let octets_par_frame = self.mix.octets_par_frame;
        let total_frames = octets.len() / octets_par_frame;
        let mut ecrites = 0usize;
        let debut = Instant::now();

        while ecrites < total_frames {
            // SAFETY : le client est démarré ; simple lecture d'état.
            let occupation = unsafe { self.client.GetCurrentPadding() }.map_err(wasapi)?;
            let libres = self.tampon_frames.saturating_sub(occupation) as usize;
            if libres == 0 {
                if debut.elapsed() >= ATTENTE_TAMPON_MAX {
                    return Err(NdError::Capture(
                        "wasapi (rendu) : tampon saturé, le périphérique ne consomme pas".into(),
                    ));
                }
                std::thread::sleep(PAUSE_SONDAGE);
                continue;
            }

            let n = libres.min(total_frames - ecrites);
            // SAFETY : GetBuffer fournit `n` frames de `octets_par_frame`
            // octets, valides jusqu'à ReleaseBuffer.
            let ptr = unsafe { self.rendu.GetBuffer(n as u32) }.map_err(wasapi)?;
            if ptr.is_null() {
                return Err(NdError::Capture(
                    "wasapi (rendu) : GetBuffer a renvoyé nul".into(),
                ));
            }
            // SAFETY : copie de `n × octets_par_frame` octets depuis notre
            // tampon (assez long : `ecrites + n ≤ total_frames`) vers le
            // tampon WASAPI dimensionné pour `n` frames.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    octets.as_ptr().add(ecrites * octets_par_frame),
                    ptr,
                    n * octets_par_frame,
                );
            }
            // SAFETY : rend exactement le nombre de frames demandé à GetBuffer.
            unsafe { self.rendu.ReleaseBuffer(n as u32, 0) }.map_err(wasapi)?;
            ecrites += n;
        }
        Ok(())
    }
}

impl AudioPlayer for WasapiPlayer {
    /// Décode le paquet Opus et pousse le PCM vers le périphérique. Le
    /// cadencement/réordonnancement amont revient au jitter buffer ;
    /// `timestamp_us` n'est pas réinterprété ici.
    fn play(&mut self, packet: &AudioPacket) -> Result<()> {
        // Opus → PCM 48 kHz stéréo f32 (format de session, plan 08).
        let stereo_48k = self.decodeur.decoder(&packet.data)?;
        // → fréquence du mix, puis nombre de voies du mix, puis octets natifs.
        let stereo_mix = self.reechantillonneur.traiter(&stereo_48k);
        let voies_mix = depuis_stereo(&stereo_mix, usize::from(self.mix.canaux));
        let octets = f32_vers_octets(&voies_mix, self.mix.echantillon);
        self.ecrire_pcm(&octets)
    }
}

impl Drop for WasapiPlayer {
    fn drop(&mut self) {
        // SAFETY : arrêt best-effort du flux ; un échec est sans conséquence.
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{echantillons_par_trame, EncodeurOpus};

    /// Une trame Opus de silence (20 ms, 48 kHz stéréo) prête à jouer.
    fn paquet_de_silence() -> AudioPacket {
        let mut enc = EncodeurOpus::new(AudioFormat::default()).expect("création encodeur");
        let silence = vec![0.0f32; enc.valeurs_par_trame()];
        AudioPacket {
            data: enc.encoder(&silence).expect("encodage du silence"),
            timestamp_us: 0,
        }
    }

    /// Chemin de décodage Opus → PCM : une trame de silence encodée ressort
    /// avec exactement les échantillons d'une trame de 20 ms, quasi nulle.
    #[test]
    fn decodage_opus_vers_pcm_taille_exacte() {
        let format = AudioFormat::default();
        let paquet = paquet_de_silence();
        let mut dec = DecodeurOpus::new(format).expect("création décodeur");
        let pcm = dec.decoder(&paquet.data).expect("décodage");
        assert_eq!(
            pcm.len(),
            echantillons_par_trame(format) * usize::from(format.channels)
        );
        assert!(
            pcm.iter().all(|v| v.abs() < 0.05),
            "le silence doit rester quasi nul"
        );
    }

    /// Construit le lecteur sans paniquer ; s'il y a un périphérique de rendu,
    /// joue une trame de silence de bout en bout (Opus → WASAPI).
    #[test]
    fn creation_du_lecteur_et_trame_de_silence() {
        match WasapiPlayer::new() {
            Ok(mut lecteur) => {
                lecteur
                    .play(&paquet_de_silence())
                    .expect("lecture d'une trame de silence");
            }
            // Machine sans sortie audio (CI headless) : l'échec doit être une
            // erreur propre, jamais une panique.
            Err(e) => eprintln!("wasapi (rendu) indisponible ici : {e}"),
        }
    }

    /// `create_system_player` suit le même contrat que le constructeur.
    #[test]
    fn create_system_player_sans_panique() {
        match crate::create_system_player() {
            Ok(_) => {}
            Err(e) => eprintln!("wasapi (rendu) indisponible ici : {e}"),
        }
    }
}
