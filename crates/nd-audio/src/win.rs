//! Capture de l'audio **système** sous Windows par boucle de retour (loopback)
//! WASAPI, encodée en Opus. Voir plan 08 §Windows.
//!
//! Séquence : COM (MTA) → `IMMDeviceEnumerator` → périphérique de **rendu**
//! par défaut → `IAudioClient` initialisé avec `AUDCLNT_STREAMFLAGS_LOOPBACK`
//! → lecture PCM via `IAudioCaptureClient` → conversion 48 kHz stéréo `f32`
//! ([`crate::convert`]) → trames Opus de 20 ms ([`crate::codec`]).
//!
//! Ce module concentre tout le `unsafe` FFI de l'audio Windows ; il est isolé
//! derrière le trait [`AudioCapturer`] pour que le reste du moteur reste sûr.
//!
//! Note honnête : en l'absence de tout flux de rendu actif, WASAPI ne délivre
//! aucune donnée de loopback. Le capteur complète alors avec du silence pour
//! garantir une cadence régulière de paquets (valides, très compacts).
#![allow(unsafe_code)]

use std::time::{Duration, Instant};

use nd_proto::{NdError, Result};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::codec::{EncodeurOpus, TRAME_MS};
use crate::convert::{octets_vers_f32, vers_stereo, FormatEchantillon, Reechantillonneur};
use crate::{AudioCapturer, AudioFormat, AudioPacket};

/// Taille du tampon WASAPI demandé, en unités de 100 ns (ici 200 ms).
const DUREE_TAMPON_HNS: i64 = 200 * 10_000;

/// Pause entre deux sondages quand aucune donnée n'est disponible.
const PAUSE_SONDAGE: Duration = Duration::from_millis(2);

/// Convertit une erreur `windows` en `NdError::Capture`.
fn wasapi(e: windows::core::Error) -> NdError {
    NdError::Capture(format!("wasapi : {e}"))
}

/// Format de mixage natif du moteur de rendu (côté source du pipeline).
struct FormatMix {
    /// Fréquence d'échantillonnage du mix (Hz).
    frequence: u32,
    /// Nombre de voies entrelacées du mix.
    canaux: u16,
    /// Format d'un échantillon.
    echantillon: FormatEchantillon,
    /// Octets par frame toutes voies confondues (`nBlockAlign`).
    octets_par_frame: usize,
}

/// Capteur de l'audio système Windows : loopback WASAPI + encodage Opus.
pub struct WasapiLoopbackCapturer {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    mix: FormatMix,
    reechantillonneur: Reechantillonneur,
    encodeur: EncodeurOpus,
    /// PCM 48 kHz stéréo entrelacé en attente d'encodage.
    en_attente: Vec<f32>,
    /// Échantillons (par canal) déjà émis — horloge média pour l'horodatage.
    echantillons_emis: u64,
    format: AudioFormat,
}

// SAFETY : les interfaces WASAPI (`IAudioClient`, `IAudioCaptureClient`) sont
// documentées par Microsoft comme *free-threaded* (méthodes thread-safe, non
// liées à un apartment) et sont créées ici après une initialisation COM en MTA.
// `windows` 0.58 ne génère pas le marqueur `Send` pour ces interfaces alors
// qu'il le fait pour D3D11 ; on l'affirme donc manuellement. Les autres champs
// (encodeur Opus, tampons) sont `Send` par eux-mêmes.
unsafe impl Send for WasapiLoopbackCapturer {}

/// Initialise COM en MTA pour le thread courant.
///
/// `S_FALSE` (déjà initialisé) est un succès ; `RPC_E_CHANGED_MODE` (thread
/// déjà en STA) est toléré car COM reste utilisable. L'initialisation n'est
/// volontairement jamais défaite : durée de vie applicative (même choix que
/// les capteurs vidéo).
fn initialiser_com() -> Result<()> {
    // SAFETY : appel d'initialisation COM standard, paramètre réservé nul.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        Ok(())
    } else {
        Err(NdError::Capture(format!(
            "wasapi : CoInitializeEx a échoué ({hr})"
        )))
    }
}

/// Décode le `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE` renvoyé par `GetMixFormat`.
///
/// # Safety
///
/// `ptr` doit pointer vers un `WAVEFORMATEX` valide, suivi de son extension
/// (`WAVEFORMATEXTENSIBLE`) si `wFormatTag == WAVE_FORMAT_EXTENSIBLE`.
unsafe fn lire_format_mix(ptr: *const WAVEFORMATEX) -> Result<FormatMix> {
    // Copie locale : la structure est `packed`, on n'en garde aucune référence.
    let base = *ptr;
    let tag = u32::from(base.wFormatTag);
    let bits = base.wBitsPerSample;

    let non_gere = || {
        NdError::Capture(format!(
            "wasapi : format de mixage non géré (tag {tag}, {bits} bits)"
        ))
    };

    let echantillon = if tag == WAVE_FORMAT_EXTENSIBLE {
        // Le sous-format est un GUID dans l'extension de la structure.
        let sous_format = (*ptr.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat;
        if sous_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
            FormatEchantillon::F32
        } else if sous_format == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
            FormatEchantillon::I16
        } else if sous_format == KSDATAFORMAT_SUBTYPE_PCM && bits == 32 {
            FormatEchantillon::I32
        } else {
            return Err(non_gere());
        }
    } else if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
        FormatEchantillon::F32
    } else if tag == WAVE_FORMAT_PCM && bits == 16 {
        FormatEchantillon::I16
    } else if tag == WAVE_FORMAT_PCM && bits == 32 {
        FormatEchantillon::I32
    } else {
        return Err(non_gere());
    };

    if base.nChannels == 0 || base.nSamplesPerSec == 0 || base.nBlockAlign == 0 {
        return Err(NdError::Capture(
            "wasapi : format de mixage incohérent (canaux/fréquence/bloc nuls)".into(),
        ));
    }

    Ok(FormatMix {
        frequence: base.nSamplesPerSec,
        canaux: base.nChannels,
        echantillon,
        octets_par_frame: usize::from(base.nBlockAlign),
    })
}

impl WasapiLoopbackCapturer {
    /// Ouvre le périphérique de rendu par défaut en mode loopback et démarre
    /// le flux de capture (pipeline conversion → 48 kHz stéréo → Opus).
    pub fn new() -> Result<Self> {
        initialiser_com()?;

        // Énumérateur de périphériques → périphérique de rendu par défaut.
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
                "wasapi : GetMixFormat a renvoyé nul".into(),
            ));
        }
        // SAFETY : pointeur non nul renvoyé par GetMixFormat, structure valide.
        let mix = unsafe { lire_format_mix(ptr_format) };
        // Initialisation en mode partagé + loopback sur le format de mixage.
        // SAFETY : `ptr_format` reste valide pendant l'appel ; tampon de 200 ms.
        let initialisation = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
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

        // SAFETY : le client vient d'être initialisé ; service de capture standard.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(wasapi)?;
        // SAFETY : démarre le flux ; arrêté dans Drop.
        unsafe { client.Start() }.map_err(wasapi)?;

        let format = AudioFormat::default();
        Ok(WasapiLoopbackCapturer {
            reechantillonneur: Reechantillonneur::new(mix.frequence, format.sample_rate),
            encodeur: EncodeurOpus::new(format)?,
            en_attente: Vec::new(),
            echantillons_emis: 0,
            client,
            capture,
            mix,
            format,
        })
    }

    /// Draine les paquets WASAPI disponibles vers `en_attente` (48 kHz stéréo
    /// `f32` entrelacé). Renvoie `false` si aucune frame n'était disponible.
    fn drainer_wasapi(&mut self) -> Result<bool> {
        let mut lu = false;
        loop {
            // SAFETY : le client est démarré ; simple lecture d'état.
            let disponibles = unsafe { self.capture.GetNextPacketSize() }.map_err(wasapi)?;
            if disponibles == 0 {
                return Ok(lu);
            }

            let mut ptr_donnees: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut indicateurs: u32 = 0;
            // SAFETY : pointeurs de sortie valides ; GetBuffer fournit `frames`
            // frames de `octets_par_frame` octets, valides jusqu'à ReleaseBuffer.
            unsafe {
                self.capture
                    .GetBuffer(&mut ptr_donnees, &mut frames, &mut indicateurs, None, None)
            }
            .map_err(wasapi)?;

            if frames > 0 && !ptr_donnees.is_null() {
                let silencieux = indicateurs & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let pcm_mix: Vec<f32> = if silencieux {
                    // Paquet marqué silencieux : le contenu du tampon est
                    // indéfini, on substitue des zéros.
                    vec![0.0; frames as usize * usize::from(self.mix.canaux)]
                } else {
                    let octets = frames as usize * self.mix.octets_par_frame;
                    // SAFETY : le tampon partagé fait `octets` octets et reste
                    // valide jusqu'à ReleaseBuffer ; copie/conversion immédiate.
                    let brut = unsafe { std::slice::from_raw_parts(ptr_donnees, octets) };
                    octets_vers_f32(brut, self.mix.echantillon)
                };
                let stereo = vers_stereo(&pcm_mix, usize::from(self.mix.canaux));
                let a_48k = self.reechantillonneur.traiter(&stereo);
                self.en_attente.extend_from_slice(&a_48k);
                lu = true;
            }

            // SAFETY : rend exactement le nombre de frames obtenu par GetBuffer.
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(wasapi)?;
        }
    }
}

impl AudioCapturer for WasapiLoopbackCapturer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        let valeurs_trame = self.encodeur.valeurs_par_trame();
        let debut = Instant::now();

        // Accumule le PCM converti jusqu'à une trame complète de 20 ms.
        while self.en_attente.len() < valeurs_trame {
            if !self.drainer_wasapi()? {
                // Loopback muet quand rien n'est rendu : au-delà d'une durée
                // de trame d'attente, on complète avec du silence pour garder
                // une cadence régulière de paquets.
                if debut.elapsed() >= Duration::from_millis(u64::from(TRAME_MS)) {
                    self.en_attente.resize(valeurs_trame, 0.0);
                    break;
                }
                std::thread::sleep(PAUSE_SONDAGE);
            }
        }

        let pcm: Vec<f32> = self.en_attente.drain(..valeurs_trame).collect();
        let data = self.encodeur.encoder(&pcm)?;

        // Horloge média : position en échantillons convertie en microsecondes,
        // monotone et sans dérive vis-à-vis du flux (synchro A/V, plan 08).
        let timestamp_us = self.echantillons_emis * 1_000_000 / u64::from(self.format.sample_rate);
        self.echantillons_emis += (valeurs_trame / usize::from(self.format.channels)) as u64;

        Ok(AudioPacket { data, timestamp_us })
    }
}

impl Drop for WasapiLoopbackCapturer {
    fn drop(&mut self) {
        // SAFETY : arrêt best-effort du flux ; un échec est sans conséquence.
        let _ = unsafe { self.client.Stop() };
    }
}
