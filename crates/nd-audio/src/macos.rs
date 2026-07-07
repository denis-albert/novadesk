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
//! # Capture de l'audio système ([`SckSystemCapturer`], ScreenCaptureKit)
//!
//! macOS n'offre **aucune API publique de loopback** avant macOS 13 :
//! contrairement à WASAPI (`AUDCLNT_STREAMFLAGS_LOOPBACK`) ou aux sources
//! *monitor* de PulseAudio, CoreAudio ne sait pas rejouer le mix de sortie.
//!
//! * **macOS ≥ 13** : **ScreenCaptureKit** fournit l'audio système
//!   (`SCStreamConfiguration.capturesAudio`) avec le consentement
//!   « Enregistrement de l'écran ». C'est la voie implémentée ici :
//!   `SCShareableContent` (découverte de l'écran) → `SCContentFilter`
//!   (écran principal) → `SCStreamConfiguration` (`capturesAudio = true`,
//!   48 kHz, 2 voies, exclusion de l'audio du process courant) → `SCStream`
//!   avec un délégué `SCStreamOutput` de type audio. Chaque `CMSampleBuffer`
//!   audio est décodé en PCM `f32` (via
//!   `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer`), converti en
//!   stéréo entrelacé (DSP partagé [`crate::convert`]) puis encodé en Opus par
//!   trames de 20 ms — même contrat que les backends Windows/Linux.
//! * **macOS < 13** : ScreenCaptureKit absent ; seul un périphérique virtuel
//!   tiers (BlackHole/Loopback) le permettrait — hors périmètre d'un client
//!   « sans installation pilote ». [`SckSystemCapturer::new`] renvoie alors
//!   [`nd_proto::NdError::NotImplemented`] (classe `SCStream` introuvable au
//!   runtime).
//!
//! Le micro reste à faire (AUHAL en direction entrée + consentement TCC micro),
//! voir `create_microphone_capturer` dans `lib.rs`.
//!
//! ## Honnêteté / validation
//!
//! **nd-audio ne compile pas en cible croisée depuis le poste Windows de
//! développement** (`libopus_sys` se construit via CMake/MSVC, qui ne sait pas
//! cibler arm64-apple). Ce chemin ScreenCaptureKit est donc **vérifié par
//! résolution des dépendances + revue de code contre l'API réelle des bindings
//! `objc2` 0.6**, mais **compilé/exécuté uniquement sur macOS réel** (à faire en
//! CI `macos-latest` / sur appareil). Le DSP réutilisé, lui, est testé partout
//! ([`crate::convert::planaire_vers_stereo`]).
//!
//! Le `unsafe` FFI de la capture ScreenCaptureKit (bindings `objc2`) est
//! concentré dans ce module, derrière le trait [`AudioCapturer`] ; la lecture
//! ([`CoreAudioPlayer`]) reste, elle, portée par `coreaudio-rs` sans `unsafe`.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use nd_proto::{NdError, Result};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMBlockBuffer, CMSampleBuffer};
use objc2_foundation::{NSArray, NSError, NSProcessInfo, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use crate::codec::{DecodeurOpus, EncodeurOpus, TRAME_MS};
use crate::convert::{planaire_vers_stereo, vers_stereo};
use crate::{AudioCapturer, AudioFormat, AudioPacket, AudioPlayer};

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

// ===========================================================================
// Capture de l'audio système — ScreenCaptureKit (macOS 13+)
// ===========================================================================

/// Variables d'instance du délégué de sortie : file PCM partagée + borne.
struct SortieIvars {
    /// File de PCM `f32` **stéréo entrelacé** 48 kHz (producteur = délégué,
    /// consommateur = [`SckSystemCapturer::prochaine_trame`]).
    file: Arc<Mutex<VecDeque<f32>>>,
    /// Borne de la file (nombre de `f32`) : au-delà, le plus ancien est jeté.
    capacite: usize,
}

define_class!(
    /// Délégué `SCStreamOutput` recevant les `CMSampleBuffer` audio et poussant
    /// le PCM converti dans la file partagée.
    #[unsafe(super(NSObject))]
    #[name = "NovaDeskSckAudioOutput"]
    #[ivars = SortieIvars]
    struct SortieSck;

    unsafe impl NSObjectProtocol for SortieSck {}

    unsafe impl SCStreamOutput for SortieSck {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn sortie_echantillon(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            type_: SCStreamOutputType,
        ) {
            // Ne traite que l'audio système (ignore écran/micro).
            if type_.0 != SCStreamOutputType::Audio.0 {
                return;
            }
            let stereo = extraire_stereo(sample_buffer);
            if stereo.is_empty() {
                return;
            }
            let ivars = self.ivars();
            let Ok(mut file) = ivars.file.lock() else {
                return;
            };
            // Borne la latence : jette le plus ancien au-delà de la capacité.
            let exces = (file.len() + stereo.len()).saturating_sub(ivars.capacite);
            if exces > 0 {
                let a_jeter = exces.min(file.len());
                file.drain(..a_jeter);
            }
            file.extend(stereo);
        }
    }
);

/// Extrait le PCM **stéréo entrelacé** `f32` d'un `CMSampleBuffer` audio.
///
/// Récupère la `AudioBufferList` (avec un `CMBlockBuffer` retenu, libéré en fin
/// de portée via [`CFRetained`]), puis convertit selon l'agencement : plusieurs
/// tampons ⇒ voies **planaires** ([`planaire_vers_stereo`]) ; un seul tampon ⇒
/// entrelacé ([`vers_stereo`] selon `mNumberChannels`). Renvoie un vecteur vide
/// en cas d'échec (jamais de panique dans le rappel temps réel).
fn extraire_stereo(sample_buffer: &CMSampleBuffer) -> Vec<f32> {
    // `AudioBufferList` est déclarée avec `mBuffers: [AudioBuffer; 1]` ; on
    // réserve deux tampons (stéréo planaire) via une structure C compatible.
    #[repr(C)]
    struct AblDeux {
        nombre: u32,
        buffers: [AudioBuffer; 2],
    }
    // SAFETY : POD C ; l'état zéro est valide (deux `AudioBuffer` nuls).
    let mut abl: AblDeux = unsafe { std::mem::zeroed() };
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY : pointeurs de sortie valides ; `buffer_list_size` = taille réelle
    // d'`AblDeux` ; allocateurs par défaut ; drapeaux nuls.
    let statut = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(abl).cast::<AudioBufferList>(),
            std::mem::size_of::<AblDeux>(),
            None,
            None,
            0,
            std::ptr::addr_of_mut!(block),
        )
    };
    if statut != 0 {
        return Vec::new();
    }
    // Reprend possession du `CMBlockBuffer` retenu : libéré au drop de `_garde`,
    // après la copie du PCM ci-dessous (les `mData` restent valides d'ici là).
    // SAFETY : `block` est un `CMBlockBuffer` valide retenu (+1) par l'appel.
    let _garde = NonNull::new(block).map(|p| unsafe { CFRetained::from_raw(p) });

    let lire = |b: &AudioBuffer| -> &[f32] {
        let n = b.mDataByteSize as usize / std::mem::size_of::<f32>();
        if b.mData.is_null() || n == 0 {
            &[]
        } else {
            // SAFETY : `mData` pointe vers `n` `f32` valides tant que le block
            // buffer (`_garde`) vit.
            unsafe { std::slice::from_raw_parts(b.mData.cast::<f32>(), n) }
        }
    };

    match abl.nombre as usize {
        0 => Vec::new(),
        1 => {
            let b0 = &abl.buffers[0];
            vers_stereo(lire(b0), (b0.mNumberChannels as usize).max(1))
        }
        _ => planaire_vers_stereo(&[lire(&abl.buffers[0]), lire(&abl.buffers[1])]),
    }
}

/// État partagé d'une complétion asynchrone ScreenCaptureKit (pont bloquant).
///
/// Les objets Objective-C ne traversent pas les threads via le typage Rust :
/// on transfère l'objet retenu sous forme de `usize` (propriété +1) et les
/// erreurs sous forme de `String` extraite — jamais l'`NSError` lui-même.
struct Completion {
    /// Pointeur (+1) de l'objet retenu transféré au thread appelant, ou 0.
    pointeur: usize,
    /// Message d'erreur extrait (`localizedDescription`), le cas échéant.
    erreur: Option<String>,
    fait: bool,
}

impl Completion {
    fn en_attente() -> Arc<(Mutex<Completion>, Condvar)> {
        Arc::new((
            Mutex::new(Completion {
                pointeur: 0,
                erreur: None,
                fait: false,
            }),
            Condvar::new(),
        ))
    }
}

/// Attend la complétion (5 s max) sans paniquer.
fn attendre(etat: &Arc<(Mutex<Completion>, Condvar)>) -> Result<()> {
    let (lock, cvar) = &**etat;
    let mut garde = lock.lock().unwrap_or_else(|p| p.into_inner());
    while !garde.fait {
        let (g, delai) = cvar
            .wait_timeout(garde, Duration::from_secs(5))
            .unwrap_or_else(|p| p.into_inner());
        garde = g;
        if delai.timed_out() && !garde.fait {
            return Err(NdError::Capture(
                "screencapturekit : délai d'attente de la complétion dépassé".into(),
            ));
        }
    }
    Ok(())
}

/// Découvre le contenu partageable (écrans) en bloquant sur la complétion.
fn decouvrir_contenu() -> Result<Retained<SCShareableContent>> {
    let etat = Completion::en_attente();
    let etat_bloc = Arc::clone(&etat);
    let handler = RcBlock::new(
        move |contenu: *mut SCShareableContent, erreur: *mut NSError| {
            let (lock, cvar) = &*etat_bloc;
            let mut e = lock.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(c) = NonNull::new(contenu) {
                // SAFETY : `contenu` est un `SCShareableContent` valide (+0) ; on le
                // retient (+1) et on transfère la propriété via un `usize`.
                if let Some(r) = unsafe { Retained::retain(c.as_ptr()) } {
                    e.pointeur = Retained::into_raw(r) as usize;
                }
            } else if let Some(err) = NonNull::new(erreur) {
                // SAFETY : `err` est un `NSError` valide le temps du rappel.
                e.erreur = Some(unsafe { err.as_ref() }.localizedDescription().to_string());
            }
            e.fait = true;
            cvar.notify_one();
        },
    );
    // SAFETY : méthode de classe lançant la découverte ; le bloc est appelé une
    // fois à la fin (durée de vie couverte par l'attente ci-dessous).
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    attendre(&etat)?;

    let (lock, _) = &*etat;
    let e = lock.lock().unwrap_or_else(|p| p.into_inner());
    if e.pointeur != 0 {
        // SAFETY : `pointeur` est un `SCShareableContent` retenu (+1) transféré
        // par le bloc ; on en reprend la propriété.
        unsafe { Retained::from_raw(e.pointeur as *mut SCShareableContent) }
            .ok_or_else(|| NdError::Capture("screencapturekit : contenu partageable nul".into()))
    } else {
        Err(NdError::Capture(format!(
            "screencapturekit : découverte du contenu a échoué : {}",
            e.erreur.as_deref().unwrap_or("erreur inconnue")
        )))
    }
}

/// Démarre la capture en bloquant sur la complétion (erreur remontée en clair).
fn demarrer(flux: &SCStream) -> Result<()> {
    let etat = Completion::en_attente();
    let etat_bloc = Arc::clone(&etat);
    let handler = RcBlock::new(move |erreur: *mut NSError| {
        let (lock, cvar) = &*etat_bloc;
        let mut e = lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(err) = NonNull::new(erreur) {
            // SAFETY : `err` est un `NSError` valide le temps du rappel.
            e.erreur = Some(unsafe { err.as_ref() }.localizedDescription().to_string());
        }
        e.fait = true;
        cvar.notify_one();
    });
    // SAFETY : démarre le flux ; le bloc de complétion est appelé une fois.
    unsafe { flux.startCaptureWithCompletionHandler(Some(&handler)) };
    attendre(&etat)?;

    let (lock, _) = &*etat;
    let e = lock.lock().unwrap_or_else(|p| p.into_inner());
    match &e.erreur {
        Some(msg) => Err(NdError::Capture(format!(
            "screencapturekit : démarrage de la capture a échoué : {msg}"
        ))),
        None => Ok(()),
    }
}

/// Capteur de l'audio système macOS : flux ScreenCaptureKit audio → Opus.
pub struct SckSystemCapturer {
    /// Flux SCK conservé pour la durée de vie de la capture (son `Drop` via
    /// `objc2` relâche le flux ; on arrête proprement dans notre `Drop`).
    flux: Retained<SCStream>,
    /// Délégué de sortie conservé vivant tant que le flux l'utilise.
    _delegue: Retained<SortieSck>,
    /// File partagée alimentée par le délégué (48 kHz stéréo entrelacé).
    file: Arc<Mutex<VecDeque<f32>>>,
    encodeur: EncodeurOpus,
    /// Échantillons (par canal) déjà émis — horloge média pour l'horodatage.
    echantillons_emis: u64,
    format: AudioFormat,
}

// SAFETY : les objets ScreenCaptureKit sont pilotés depuis un seul thread
// (création, `Drop`) ; les rappels audio arrivent sur la file série dédiée et ne
// touchent que la `file` protégée par `Mutex`. Aucun accès concurrent non
// synchronisé — le type peut donc traverser les threads (même justification que
// le moteur WASAPI côté Windows).
unsafe impl Send for SckSystemCapturer {}

impl SckSystemCapturer {
    /// Ouvre un flux ScreenCaptureKit audio sur l'écran principal (audio système
    /// global), 48 kHz stéréo, en excluant l'audio du process courant. Requiert
    /// macOS 13+ (sinon `NotImplemented`) et le consentement « Enregistrement de
    /// l'écran ».
    pub fn new() -> Result<Self> {
        // Gate macOS 13+ : l'audio ScreenCaptureKit (`capturesAudio`) n'existe
        // qu'à partir de macOS 13.0. La classe `SCStream` existe dès 12.3, mais
        // appeler `setCapturesAudio:` avant 13 déclencherait un sélecteur inconnu
        // (exception Objective-C) : on renvoie donc `NotImplemented` (repli
        // documenté vers un périphérique virtuel tiers type BlackHole).
        if NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            < 13
        {
            return Err(NdError::NotImplemented(
                "nd-audio : capture système macOS — ScreenCaptureKit audio requiert macOS 13+ ; \
                 avant, seul un périphérique virtuel tiers (BlackHole) le permet (voir src/macos.rs)",
            ));
        }
        let format = AudioFormat::default();

        // Découverte de l'écran principal (l'audio système est global : le filtre
        // écran ne restreint pas le mix, il satisfait l'API SCK).
        let contenu = decouvrir_contenu()?;
        // SAFETY : `displays` renvoie la liste des écrans partageables.
        let ecrans = unsafe { contenu.displays() };
        let ecran = ecrans
            .firstObject()
            .ok_or_else(|| NdError::Capture("screencapturekit : aucun écran partageable".into()))?;
        let sans_fenetre: Retained<NSArray<SCWindow>> = NSArray::new();
        // SAFETY : initialise un filtre sur l'écran principal, sans exclusion.
        let filtre = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &ecran,
                &sans_fenetre,
            )
        };

        // Configuration : audio système 48 kHz stéréo, hors audio du process.
        // SAFETY : réglages sur une configuration fraîchement créée.
        let config = unsafe {
            let c = SCStreamConfiguration::new();
            c.setCapturesAudio(true);
            c.setSampleRate(format.sample_rate as isize);
            c.setChannelCount(isize::from(format.channels));
            c.setExcludesCurrentProcessAudio(true);
            c
        };

        // File partagée + délégué.
        let file: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let capacite =
            format.sample_rate as usize * usize::from(format.channels) * FILE_MAX_MS / 1000;
        let delegue = SortieSck::alloc().set_ivars(SortieIvars {
            file: Arc::clone(&file),
            capacite,
        });
        // SAFETY : initialisation standard d'une instance à ivars renseignés.
        let delegue: Retained<SortieSck> = unsafe { msg_send![super(delegue), init] };

        // Flux SCK (sans délégué d'état de flux : on gère l'audio via la sortie).
        // SAFETY : filtre et config valides.
        let flux = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filtre,
                &config,
                None,
            )
        };

        // File série dédiée aux rappels audio (attribut série = valeur par défaut).
        let file_rappels = DispatchQueue::new("com.novadesk.sck.audio", DispatchQueueAttr::SERIAL);
        let sortie = ProtocolObject::from_ref(&*delegue);
        // SAFETY : ajoute la sortie audio sur la file série ; erreur remontée en clair.
        unsafe {
            SCStream::addStreamOutput_type_sampleHandlerQueue_error(
                &flux,
                sortie,
                SCStreamOutputType::Audio,
                Some(&file_rappels),
            )
        }
        .map_err(|e| {
            NdError::Capture(format!(
                "screencapturekit : addStreamOutput (audio) a échoué : {}",
                e.localizedDescription()
            ))
        })?;

        demarrer(&flux)?;

        Ok(SckSystemCapturer {
            flux,
            _delegue: delegue,
            file,
            encodeur: EncodeurOpus::new(format)?,
            echantillons_emis: 0,
            format,
        })
    }

    /// Prochaine trame Opus de 20 ms : draine une trame de PCM stéréo de la file
    /// (complétée par du silence si le flux est momentanément muet, comme le
    /// loopback WASAPI), encode puis horodate.
    fn prochaine_trame(&mut self) -> Result<AudioPacket> {
        let besoin = self.encodeur.valeurs_par_trame();
        let debut = Instant::now();
        loop {
            {
                let file = self.file.lock().map_err(|_| {
                    NdError::Capture("screencapturekit : file de capture empoisonnée".into())
                })?;
                if file.len() >= besoin {
                    break;
                }
            }
            // Flux muet (rien joué) : au-delà d'une durée de trame, complète en
            // silence pour garder une cadence régulière de paquets.
            if debut.elapsed() >= Duration::from_millis(u64::from(TRAME_MS)) {
                let mut file = self.file.lock().map_err(|_| {
                    NdError::Capture("screencapturekit : file de capture empoisonnée".into())
                })?;
                if file.len() < besoin {
                    file.resize(besoin, 0.0);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        let pcm: Vec<f32> = {
            let mut file = self.file.lock().map_err(|_| {
                NdError::Capture("screencapturekit : file de capture empoisonnée".into())
            })?;
            file.drain(..besoin).collect()
        };
        let data = self.encodeur.encoder(&pcm)?;

        let timestamp_us = self.echantillons_emis * 1_000_000 / u64::from(self.format.sample_rate);
        self.echantillons_emis += (besoin / usize::from(self.format.channels)) as u64;
        Ok(AudioPacket { data, timestamp_us })
    }
}

impl AudioCapturer for SckSystemCapturer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_packet(&mut self) -> Result<AudioPacket> {
        self.prochaine_trame()
    }
}

impl Drop for SckSystemCapturer {
    fn drop(&mut self) {
        // Arrêt best-effort du flux (le bloc de complétion est ignoré ici).
        // SAFETY : `flux` est un `SCStream` valide ; arrêt sans handler.
        unsafe { self.flux.stopCaptureWithCompletionHandler(None) };
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
