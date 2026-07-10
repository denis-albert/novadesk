//! `nd-core` — orchestration d'une session NovaDesk.
//!
//! Assemble les composants (transport, session sécurisée, capture/codec/input…) et
//! porte la **machine à états** de session. Les étages réels (pipelines hôte/viewer,
//! transport chiffré de bout en bout) sont câblés par l'orchestrateur réutilisable
//! [`SessionEngine`] (module `session`), qui expose l'état, les frames décodées,
//! un canal d'entrées et des statistiques continues à un consommateur (future UI,
//! voir `../../plan-technique/16-roadmap-planning.md`).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nd_capture::{
    enumerate_monitors, CaptureConfig, CapturedFrame, FrameImage, PixelFormat, Rect, ScreenCapturer,
};
use nd_codec::{
    CodecKind, ContentProfile, DecodedFrame, EncodedChunk, EncoderConfig, NetworkEstimate,
    RateController, VideoDecoder, VideoEncoder,
};
use nd_crypto::{HandshakeRole, NoiseHandshake, NoiseSession, PeerFingerprint, SecureSession};
use nd_features::{Mp4Muxer, Permissions, PrivacyState, RecordingMetadata};
use nd_input::{InputInjector, MouseButton};
use nd_proto::{
    ChannelKind, InputEvent, MonitorId, NdError, NovaId, ProtocolVersion, Reliability, Result,
};
use nd_transport::{ChannelHandle, PathEstimate, Transport};

/// Câblage des briques média/annexes (audio, fichiers, presse-papiers, chat,
/// bascule moniteur) dans la boucle de session. Voir [`media`].
mod media;
/// Établissement QUIC par ID via le rendez-vous (punch + repli relais).
mod p2p;
/// Orchestrateur de session réutilisable (threads + canaux). Voir [`SessionEngine`].
mod session;
/// Tunnel TCP de session (redirection de port relayée). Voir [`tunnel`].
mod tunnel;
/// Service hôte « accès non surveillé ». Voir [`UnattendedHost`].
mod unattended;

pub use media::ChatMessage;
// Source d'émission audio de l'hôte : type du paramètre de
// [`SessionHandle::set_audio_source`], ré-exporté pour que l'appelant n'ait pas à
// dépendre de `nd-audio`.
pub use nd_audio::SourceEmission;
pub use session::{
    raccourcis_hote_defaut, DemandeAdmissionManuelle, ListingDistant, SecretAdmission,
    SessionEndpoint, SessionEngine, SessionHandle, SessionMedia, SessionOptions, SessionStats,
    TelechargementDistant,
};
pub use tunnel::TunnelHandle;
pub use unattended::{UnattendedHost, UnattendedHostHandle};

/// Région d'écran partagée (« cadre d'écran »), mutable en cours de session et
/// observée par la boucle de diffusion hôte pour appliquer
/// [`ScreenCapturer::set_region`]. `None` = plein écran.
pub type RegionPartagee = Arc<Mutex<Option<Rect>>>;

/// Un écran de l'hôte, tel que **publié au contrôleur** sur le plan de contrôle
/// (plan de contrôle de session, capacité « liste des moniteurs »). Miroir plat
/// et transportable de [`nd_capture::MonitorInfo`] réduit aux champs utiles à
/// l'UI (l'index est celui qu'attend [`SessionHandle::switch_monitor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteMonitor {
    /// Index du moniteur (= `MonitorId`, argument de la bascule multi-écran).
    pub index: u32,
    /// Largeur en pixels.
    pub width: u32,
    /// Hauteur en pixels.
    pub height: u32,
    /// Vrai pour le moniteur principal.
    pub primary: bool,
}

/// Informations système du **pair**, publiées par l'hôte sur le plan de
/// contrôle (capacité « infos système du pair »).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Nom d'hôte de la machine distante.
    pub host: String,
    /// Système d'exploitation (chaîne libre, ex. « windows (x86_64) »).
    pub os: String,
}

/// Préréglage de qualité **partagé** avec la boucle de diffusion hôte : le
/// contrôleur (ou l'hôte) le renégocie en cours de session (capacité
/// « préréglage de qualité »). La boucle observe la `generation` — incrémentée à
/// chaque changement — pour reconfigurer l'encodeur et l'échelle ABR au vol,
/// **sous** le plafond de débit (l'ABR continue de dégrader à partir du plafond,
/// jamais au-dessus).
#[derive(Debug, Default)]
pub struct EtatQualite {
    /// Profil ABR : `true` = [`ContentProfile::Video`] (fluidité — dégrade la
    /// résolution d'abord), `false` = [`ContentProfile::Text`] (netteté —
    /// dégrade la cadence d'abord).
    pub profil_video: AtomicBool,
    /// Plafond de débit en kbit/s appliqué à l'encodeur ; `0` = aucun plafond
    /// (débit de base par défaut du pipeline).
    pub plafond_kbps: AtomicU32,
    /// Génération, incrémentée à chaque renégociation : la boucle de diffusion
    /// détecte le changement en la comparant à sa dernière valeur observée.
    pub generation: AtomicU64,
}

impl EtatQualite {
    /// Profil de contenu ABR effectif d'après le drapeau [`Self::profil_video`].
    #[must_use]
    pub fn profil(&self) -> ContentProfile {
        if self.profil_video.load(Ordering::Relaxed) {
            ContentProfile::Video
        } else {
            ContentProfile::Text
        }
    }
}

/// Demande d'**enregistrement à chaud** partagée avec la boucle de diffusion
/// hôte : `generation` passe à ≥ 1 dès le premier ordre (`set_recording`), et
/// `chemin` porte alors le fichier MP4 voulu (`None` = arrêter proprement).
/// Tant que `generation` vaut `0`, la boucle garde le comportement historique
/// (enregistrement statique de [`HostStreamOptions::recording`]).
pub type EnregistrementPartage = Arc<EtatEnregistrement>;

/// Contenu partagé d'un [`EnregistrementPartage`] (voir sa documentation).
#[derive(Debug, Default)]
pub struct EtatEnregistrement {
    /// Génération, incrémentée à chaque `set_recording` (`0` = jamais commandé).
    pub generation: AtomicU64,
    /// Chemin MP4 voulu au dernier ordre (`None` = arrêter l'enregistrement).
    pub chemin: Mutex<Option<PathBuf>>,
}

/// Rôle du poste local dans la session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// Ce poste pilote l'autre.
    Controller,
    /// Ce poste est piloté.
    Controlled,
}

/// État courant de la session (voir le pipeline en plan 01 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Aucune session active.
    Idle,
    /// Résolution de l'ID pair via le rendez-vous (plan 05).
    Resolving,
    /// Établissement du transport (NAT traversal / relais).
    Connecting,
    /// Handshake cryptographique en cours (plan 06).
    Handshaking,
    /// Session établie et média en cours.
    Active,
    /// Coupure réseau : tentative de reconnexion rapide (plan 04).
    Reconnecting,
    /// Session terminée.
    Closed,
}

/// Paramètres de démarrage d'une session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub role: SessionRole,
    pub local_id: NovaId,
    /// Pair à joindre (requis pour le rôle contrôleur).
    pub peer_id: Option<NovaId>,
    /// Permissions initiales (le contrôlé fait foi ; plan 13).
    pub permissions: Permissions,
}

/// Composants branchés sur une session active.
///
/// `Option` car ils sont installés au fil de la progression de la machine à états.
#[derive(Default)]
pub struct SessionComponents {
    pub transport: Option<Box<dyn Transport>>,
    pub secure: Option<Box<dyn SecureSession>>,
}

/// Une session NovaDesk et sa machine à états.
pub struct Session {
    config: SessionConfig,
    state: SessionState,
    components: SessionComponents,
}

impl Session {
    /// Crée une session au repos.
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        Session {
            config,
            state: SessionState::Idle,
            components: SessionComponents::default(),
        }
    }

    #[must_use]
    pub fn state(&self) -> SessionState {
        self.state
    }

    #[must_use]
    pub fn role(&self) -> SessionRole {
        self.config.role
    }

    #[must_use]
    pub fn permissions(&self) -> Permissions {
        self.config.permissions
    }

    /// Démarre la séquence de connexion.
    ///
    /// Le rôle contrôleur exige un `peer_id`. Les transitions ultérieures
    /// (Connecting → Handshaking → Active) seront pilotées par les événements du
    /// transport et du handshake une fois ces couches implémentées.
    pub fn begin(&mut self) -> Result<()> {
        if self.config.role == SessionRole::Controller && self.config.peer_id.is_none() {
            return Err(NdError::Protocol(
                "le rôle contrôleur nécessite un peer_id".to_owned(),
            ));
        }
        self.transition(SessionState::Resolving);
        Ok(())
    }

    /// Termine la session proprement.
    pub fn close(&mut self) {
        self.components.transport = None;
        self.components.secure = None;
        self.transition(SessionState::Closed);
    }

    fn transition(&mut self, next: SessionState) {
        // Point d'accroche pour la journalisation/observabilité (plan 11/14).
        self.state = next;
    }
}

/// Version du protocole implémentée par ce moteur.
#[must_use]
pub fn engine_version() -> ProtocolVersion {
    ProtocolVersion::CURRENT
}

/// Applique un événement d'entrée reçu à un injecteur (côté machine contrôlée).
///
/// Convertit le message de protocole [`InputEvent`] (voir `nd-proto`) en appels au
/// trait [`InputInjector`] (voir `nd-input`). Voir plan 07.
pub fn apply_input(injector: &dyn InputInjector, event: &InputEvent) -> Result<()> {
    match *event {
        InputEvent::MouseMoveAbs { x, y, monitor } => {
            injector.mouse_move_abs(x, y, MonitorId(monitor))
        }
        InputEvent::MouseMoveRel { dx, dy } => injector.mouse_move_rel(dx, dy),
        InputEvent::MouseButton { button, down } => {
            let btn = match button {
                0 => MouseButton::Left,
                1 => MouseButton::Right,
                2 => MouseButton::Middle,
                3 => MouseButton::X1,
                _ => MouseButton::X2,
            };
            injector.mouse_button(btn, down)
        }
        InputEvent::Scroll { dx, dy } => injector.scroll(dx, dy),
        InputEvent::Key { scancode, down } => injector.key(scancode, down),
        InputEvent::Unicode { codepoint } => match char::from_u32(codepoint) {
            Some(ch) => injector.unicode(ch),
            None => Ok(()),
        },
    }
}

/// Étage **hôte** de la tranche verticale : capture d'écran → encodage H.264 → envoi
/// sur le canal vidéo du transport. Assemble les composants réels (voir plan 01 §2).
///
/// Si l'écran est statique, la dernière image disponible est ré-encodée, comme le fait
/// un vrai flux temps réel (images delta minuscules).
pub struct HostPipeline {
    capturer: Box<dyn ScreenCapturer>,
    encoder: Box<dyn VideoEncoder>,
    transport: Box<dyn Transport>,
    video_channel: ChannelHandle,
    configured: bool,
    last_frame: Option<CapturedFrame>,
    sent: usize,
}

impl HostPipeline {
    /// Construit l'étage hôte : démarre la capture et ouvre le canal vidéo.
    pub fn new(
        mut capturer: Box<dyn ScreenCapturer>,
        encoder: Box<dyn VideoEncoder>,
        mut transport: Box<dyn Transport>,
    ) -> Result<Self> {
        capturer.start(CaptureConfig {
            monitor: MonitorId(0),
            target_fps: 60,
            capture_cursor: false,
        })?;
        let video_channel = transport.open_channel(ChannelKind::Video(MonitorId(0)));
        Ok(Self {
            capturer,
            encoder,
            transport,
            video_channel,
            configured: false,
            last_frame: None,
            sent: 0,
        })
    }

    /// Capture, encode et envoie jusqu'à `target` images. Renvoie le nombre envoyé.
    pub fn run(&mut self, target: usize) -> Result<usize> {
        let max_attempts = target.saturating_mul(50) + 1000;
        let mut attempts = 0usize;
        while self.sent < target && attempts < max_attempts {
            attempts += 1;
            let frame = self.capturer.next_frame()?;
            if frame.image.is_some() {
                if !self.configured {
                    self.encoder.configure(EncoderConfig {
                        kind: CodecKind::H264,
                        width: frame.width,
                        height: frame.height,
                        target_bitrate_kbps: 8_000,
                        max_fps: 60,
                    })?;
                    self.configured = true;
                }
                self.last_frame = Some(frame);
            }
            if !self.configured {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let force_keyframe = self.sent == 0;
            let chunk = {
                let frame = self
                    .last_frame
                    .as_ref()
                    .expect("configuré implique une image capturée");
                self.encoder.encode(frame, force_keyframe)?
            };
            self.transport
                .send(self.video_channel, chunk.data, Reliability::UnreliableFec)?;
            self.sent += 1;
        }
        Ok(self.sent)
    }

    /// Mode « flux continu » : capture, encode et envoie jusqu'à la levée du signal
    /// `stop`. Écran statique : la dernière image disponible est ré-encodée (deltas
    /// minuscules), comme [`HostPipeline::run`]. La cadence est bornée (~80 img/s)
    /// pour laisser du temps CPU au reste de la session.
    ///
    /// Une erreur d'envoi (pair déconnecté) termine la diffusion **sans** erreur :
    /// c'est la fin normale d'une session dont l'autre extrémité est partie. Renvoie
    /// le nombre d'images envoyées par cet appel.
    pub fn run_streaming(&mut self, stop: Arc<AtomicBool>) -> Result<usize> {
        let mut envoyees = 0usize;
        while !stop.load(Ordering::Relaxed) {
            let frame = self.capturer.next_frame()?;
            if frame.image.is_some() {
                if !self.configured {
                    self.encoder.configure(EncoderConfig {
                        kind: CodecKind::H264,
                        width: frame.width,
                        height: frame.height,
                        target_bitrate_kbps: 8_000,
                        max_fps: 60,
                    })?;
                    self.configured = true;
                }
                self.last_frame = Some(frame);
            }
            if !self.configured {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let chunk = {
                let frame = self
                    .last_frame
                    .as_ref()
                    .expect("configuré implique une image capturée");
                self.encoder.encode(frame, self.sent == 0)?
            };
            if self
                .transport
                .send(self.video_channel, chunk.data, Reliability::UnreliableFec)
                .is_err()
            {
                break;
            }
            envoyees += 1;
            self.sent += 1;
            std::thread::sleep(Duration::from_millis(12));
        }
        Ok(envoyees)
    }

    /// Mode « flux continu **piloté** » : comme [`HostPipeline::run_streaming`],
    /// enrichi des briques temps réel du plan 03/04/13 :
    ///
    /// * **ABR bout-en-bout** : toutes les [`HostStreamOptions::abr_period`]
    ///   (~1 Hz), l'estimation du chemin ([`Transport::path_estimate`]) est
    ///   convertie ([`NetworkEstimate::from_path`]) et intégrée par le
    ///   [`RateController`], qui applique le débit du palier retenu à
    ///   l'encodeur (`set_target_bitrate`). Les estimations sans mesure de
    ///   débit (`estimated_bandwidth_kbps == 0`, chemin pas encore jaugé) sont
    ///   ignorées pour ne pas plonger au plancher en début de session.
    /// * **Encodage delta** (opt-in, voir [`HostStreamOptions::delta_mode`]) :
    ///   les frames capturées sont passées telles quelles à l'encodeur — une
    ///   frame sans région modifiée devient une *trame de répétition* quasi
    ///   gratuite au lieu d'un ré-encodage plein cadre.
    /// * **Enregistrement MP4** (opt-in) : chaque [`EncodedChunk`] non vide est
    ///   poussé dans un [`Mp4Muxer`] ; le fichier est clos (**relisible**) en
    ///   fin de flux. Une erreur d'écriture termine le flux en erreur (pas
    ///   d'enregistrement silencieusement tronqué).
    /// * **Observabilité** : `on_tick` est rappelé à chaque image envoyée avec
    ///   l'instantané [`HostStreamTick`] (consigne ABR, palier, enregistrement).
    ///
    /// Une erreur d'envoi (pair déconnecté) termine la diffusion **sans**
    /// erreur, comme [`HostPipeline::run_streaming`]. Renvoie le rapport de fin
    /// de flux (le fichier d'enregistrement n'y figure que s'il a été clos avec
    /// au moins une image ; un fichier resté vide est supprimé).
    ///
    /// # Errors
    /// Erreur de capture, d'encodage ou d'enregistrement.
    pub fn run_streaming_pilote(
        &mut self,
        stop: Arc<AtomicBool>,
        options: HostStreamOptions,
        mut on_tick: impl FnMut(HostStreamTick),
    ) -> Result<HostStreamReport> {
        self.encoder.set_delta_mode(options.delta_mode);
        let mut regulateur: Option<RateController> = None;
        let mut enregistreur: Option<EnregistreurMp4> = None;
        let mut prochain_abr = Instant::now();
        let mut debit_cible_kbps = 0u32;
        let mut envoyees = 0u64;
        // Configuration d'encodage retenue (dimensions/débit de base) : mémorisée
        // pour (r)ouvrir un enregistrement à chaud après coup.
        let mut config_base: Option<EncoderConfig> = None;
        // Enregistrement à chaud : chemin du muxeur actuellement ouvert (`None` =
        // aucun), images des muxeurs déjà clos (cumul pour l'observabilité), et
        // dernière génération de préréglage de qualité observée.
        let mut enr_chemin_ouvert: Option<PathBuf> = None;
        let mut frames_enr_cumul = 0u64;
        let mut generation_qualite = 0u64;
        // Bascule multi-écran : moniteur diffusé et resynchronisation (image-clé)
        // forcée après un changement de moniteur (résolution potentiellement
        // différente → décodeur à recaler).
        let mut moniteur_courant = 0u32;
        let mut resync_moniteur = false;
        // Cadre d'écran courant (dernière région appliquée à la capture) et état
        // de confidentialité précédent (détection de bascule pour l'image-clé).
        let mut region_courante: Option<Rect> = None;
        let mut confidentiel_precedent = false;

        while !stop.load(Ordering::Relaxed) {
            // Bascule moniteur demandée par le contrôleur : re-cible la capture
            // au vol (best-effort ; un index invalide est ignoré et annulé).
            if let Some(demande) = &options.monitor_switch {
                let voulu = demande.load(Ordering::Relaxed);
                if voulu != moniteur_courant {
                    if appliquer_bascule_moniteur(self.capturer.as_mut(), voulu) {
                        moniteur_courant = voulu;
                        self.configured = false;
                        resync_moniteur = true;
                    } else {
                        demande.store(moniteur_courant, Ordering::Relaxed);
                    }
                }
            }

            // Cadre d'écran demandé par le contrôleur : restreint la zone
            // partagée au vol (best-effort). Un backend qui ne sait pas
            // restreindre **rejette** la demande (retour au cadre courant) —
            // jamais de fuite silencieuse de la zone hors-cadre.
            if let Some(demande) = &options.region_switch {
                let voulue = *demande.lock().expect("verrou du cadre d'écran");
                if voulue != region_courante {
                    if self.capturer.set_region(voulue).is_ok() {
                        region_courante = voulue;
                        self.configured = false; // dimensions changées → reconfigurer
                        resync_moniteur = true; // image-clé de resynchronisation
                    } else {
                        *demande.lock().expect("verrou du cadre d'écran") = region_courante;
                    }
                }
            }

            // Préréglage de qualité renégocié (profil ABR + plafond de débit) :
            // reconfigure l'encodeur et reconstruit l'échelle ABR **sous** le
            // nouveau plafond (voir la reconfiguration ci-dessous), avec une
            // image-clé de resynchronisation pour une transition nette.
            if let Some(qualite) = &options.quality {
                let generation = qualite.generation.load(Ordering::Relaxed);
                if generation != generation_qualite {
                    generation_qualite = generation;
                    self.configured = false;
                    resync_moniteur = true;
                }
            }

            let capturee = self.capturer.next_frame()?;
            let image_fraiche = capturee.image.is_some();
            if image_fraiche && !self.configured {
                let base = EncoderConfig {
                    kind: CodecKind::H264,
                    width: capturee.width,
                    height: capturee.height,
                    // Débit de base du palier 0 de l'ABR, **plafonné** par le
                    // préréglage de qualité s'il en fixe un : l'échelle dégrade
                    // ensuite à partir de ce plafond, jamais au-dessus.
                    target_bitrate_kbps: debit_base_kbps(&options),
                    max_fps: 60,
                };
                self.encoder.configure(base)?;
                config_base = Some(base);
                debit_cible_kbps = base.target_bitrate_kbps;
                // Le préréglage de qualité prime sur le profil ABR statique.
                regulateur =
                    profil_abr_effectif(&options).map(|profil| RateController::new(base, profil));
                self.configured = true;
            }
            if !self.configured {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }

            // Enregistrement (statique au démarrage puis piloté à chaud) : ouvre
            // ou **clôt proprement** (fichier relisible) le muxeur MP4 selon le
            // chemin voulu. Un muxeur clos ne se rouvre pas — un nouveau chemin
            // ouvre donc un nouveau fichier ; l'ouverture attend la configuration
            // (dimensions connues). Une bascule moniteur/qualité (qui remet
            // `configured` à faux) laisse le chemin inchangé : le muxeur perdure.
            let enr_voulu = enregistrement_voulu(&options);
            if enr_voulu != enr_chemin_ouvert {
                if let Some(enregistreur) = enregistreur.take() {
                    let (frames, _chemin) = enregistreur.clore()?;
                    frames_enr_cumul += frames;
                }
                if let (Some(chemin), Some(base)) = (enr_voulu.as_ref(), config_base.as_ref()) {
                    enregistreur = Some(EnregistreurMp4::ouvrir(chemin, *base)?);
                }
                enr_chemin_ouvert = enr_voulu;
            }

            // Régulation ABR : échantillonnage périodique du chemin réseau.
            if let Some(regulateur) = regulateur.as_mut() {
                let maintenant = Instant::now();
                if maintenant >= prochain_abr {
                    prochain_abr = maintenant + options.abr_period;
                    let chemin = self.transport.path_estimate();
                    if chemin.estimated_bandwidth_kbps > 0 {
                        let cible = regulateur.apply_network_estimate(
                            self.encoder.as_mut(),
                            NetworkEstimate::from_path(
                                chemin.rtt_us,
                                chemin.loss_ratio,
                                chemin.estimated_bandwidth_kbps,
                            ),
                        );
                        debit_cible_kbps = cible.target_bitrate_kbps;
                    }
                }
            }

            // Mode confidentialité : quand le rideau est levé, l'écran réel
            // n'est **jamais** encodé — un cadre noir opaque part à la place. La
            // bascule (dans un sens comme dans l'autre) force une image-clé pour
            // une transition nette côté contrôleur.
            let confidentiel = options
                .privacy
                .as_ref()
                .is_some_and(|drapeau| drapeau.load(Ordering::Relaxed));
            let bascule_confidentialite = confidentiel != confidentiel_precedent;
            confidentiel_precedent = confidentiel;

            // Encodage : en mode delta, la frame capturée passe telle quelle
            // (`dirty` fidèle exigé ; image absente = trame de répétition) ; en
            // mode plein cadre, la dernière image disponible est ré-encodée.
            let force_keyframe = self.sent == 0 || resync_moniteur || bascule_confidentialite;
            resync_moniteur = false;
            let chunk = if confidentiel {
                // Rideau : cadre noir aux dimensions courantes (jamais l'écran réel).
                let (largeur, hauteur) = self.dimensions_diffusion(&capturee);
                let noire = frame_noire(largeur, hauteur, capturee.timestamp_us);
                let chunk = self.encoder.encode(&noire, force_keyframe)?;
                self.last_frame = Some(noire);
                chunk
            } else if options.delta_mode {
                let chunk = self.encoder.encode(&capturee, force_keyframe)?;
                if image_fraiche {
                    self.last_frame = Some(capturee);
                }
                chunk
            } else {
                if image_fraiche {
                    self.last_frame = Some(capturee);
                }
                let frame = self
                    .last_frame
                    .as_ref()
                    .expect("configuré implique une image capturée");
                self.encoder.encode(frame, force_keyframe)?
            };

            // Enregistrement : les trames de répétition (données vides) ne
            // portent aucune image — la précédente dure simplement plus longtemps.
            if let Some(enregistreur) = enregistreur.as_mut() {
                if !chunk.data.is_empty() {
                    enregistreur.muxer.record_video_chunk(&chunk)?;
                    enregistreur.frames += 1;
                }
            }

            if self
                .transport
                .send(self.video_channel, chunk.data, options.video_reliability)
                .is_err()
            {
                break;
            }
            envoyees += 1;
            self.sent += 1;
            on_tick(HostStreamTick {
                target_bitrate_kbps: debit_cible_kbps,
                abr_level: regulateur
                    .as_ref()
                    .map_or(0, |r| u32::try_from(r.palier()).unwrap_or(u32::MAX)),
                // Cumul des muxeurs déjà clos (enregistrement à chaud) + muxeur courant.
                frames_recorded: frames_enr_cumul + enregistreur.as_ref().map_or(0, |e| e.frames),
            });
            std::thread::sleep(Duration::from_millis(12));
        }

        let (frames_dernier, chemin_clos) = match enregistreur {
            Some(enregistreur) => enregistreur.clore()?,
            None => (0, None),
        };
        Ok(HostStreamReport {
            frames_sent: envoyees,
            frames_recorded: frames_enr_cumul + frames_dernier,
            recording_path: chemin_clos,
        })
    }

    /// Dimensions du cadre à diffuser : celles de la frame fraîchement capturée
    /// si elle porte une image, sinon celles de la dernière image connue (le
    /// cadre noir de confidentialité reprend la taille de l'écran partagé).
    fn dimensions_diffusion(&self, capturee: &CapturedFrame) -> (u32, u32) {
        if capturee.image.is_some() {
            (capturee.width, capturee.height)
        } else if let Some(precedente) = &self.last_frame {
            (precedente.width, precedente.height)
        } else {
            (capturee.width, capturee.height)
        }
    }
}

/// Construit un **cadre noir opaque** aux dimensions données, à diffuser à la
/// place de l'écran réel pendant le mode confidentialité. Les pixels
/// proviennent de [`PrivacyState::render_screen_cache`] (volet du rideau
/// réalisable sans droits administrateur) ; le tampon étant uniformément noir,
/// l'ordre des canaux est indifférent (on l'étiquette [`PixelFormat::Bgra8`],
/// comme la capture Windows).
fn frame_noire(largeur: u32, hauteur: u32, timestamp_us: u64) -> CapturedFrame {
    let rideau = PrivacyState {
        black_screen: true,
        block_local_input: false,
        disable_wallpaper: false,
    };
    let pixels = rideau
        .render_screen_cache(largeur, hauteur)
        .map_or_else(Vec::new, |cache| cache.pixels().to_vec());
    CapturedFrame {
        width: largeur,
        height: hauteur,
        monitor: MonitorId(0),
        format: PixelFormat::Bgra8,
        dirty: vec![Rect {
            x: 0,
            y: 0,
            w: largeur,
            h: hauteur,
        }],
        cursor: None,
        timestamp_us,
        image: Some(FrameImage::Cpu {
            data: pixels,
            stride: largeur as usize * 4,
        }),
    }
}

/// Re-cible la capture sur le moniteur `index` (bascule multi-écran, plan 13).
///
/// Best-effort : vérifie d'abord que le moniteur existe ([`enumerate_monitors`])
/// puis relance la capture dessus. Renvoie `false` — **sans** changer d'écran —
/// si l'index est hors bornes ou si le backend refuse la reconfiguration ; le
/// flux continue alors sur le moniteur courant. Sur une machine mono-écran, seul
/// l'index 0 réussit (la voie de commande reste néanmoins prouvée de bout en
/// bout, voir `examples/session_media_demo.rs`).
fn appliquer_bascule_moniteur(capturer: &mut dyn ScreenCapturer, index: u32) -> bool {
    let existe =
        enumerate_monitors().is_ok_and(|liste| liste.iter().any(|m| m.id == MonitorId(index)));
    if !existe {
        return false;
    }
    capturer
        .start(CaptureConfig {
            monitor: MonitorId(index),
            target_fps: 60,
            capture_cursor: false,
        })
        .is_ok()
}

/// Débit de base par défaut (palier 0 de l'ABR) sans plafond de qualité, kbit/s.
const DEBIT_BASE_PAR_DEFAUT_KBPS: u32 = 8_000;

/// Profil ABR effectif de la boucle de diffusion : le préréglage de qualité
/// partagé ([`HostStreamOptions::quality`]) **prime** sur le profil statique des
/// options ; à défaut, le profil statique est conservé.
fn profil_abr_effectif(options: &HostStreamOptions) -> Option<ContentProfile> {
    match &options.quality {
        Some(qualite) => Some(qualite.profil()),
        None => options.abr_profile,
    }
}

/// Débit de base (kbit/s) du palier 0 de l'échelle ABR : **plafonné** par le
/// préréglage de qualité s'il en fixe un (> 0), sinon le plein régime. L'échelle
/// dégrade ensuite à partir de ce plafond, jamais au-dessus.
fn debit_base_kbps(options: &HostStreamOptions) -> u32 {
    let plafond = options
        .quality
        .as_ref()
        .map_or(0, |qualite| qualite.plafond_kbps.load(Ordering::Relaxed));
    if plafond == 0 {
        DEBIT_BASE_PAR_DEFAUT_KBPS
    } else {
        plafond.min(DEBIT_BASE_PAR_DEFAUT_KBPS)
    }
}

/// Chemin d'enregistrement voulu : le pilotage **à chaud** ([`SessionHandle`] →
/// `set_recording`, matérialisé par [`HostStreamOptions::recording_switch`])
/// prime dès son premier ordre (génération ≥ 1) ; sinon l'enregistrement
/// statique d'[`HostStreamOptions::recording`] (comportement historique).
fn enregistrement_voulu(options: &HostStreamOptions) -> Option<PathBuf> {
    match &options.recording_switch {
        Some(commande) if commande.generation.load(Ordering::Relaxed) > 0 => commande
            .chemin
            .lock()
            .expect("verrou d'enregistrement à chaud")
            .clone(),
        _ => options.recording.clone(),
    }
}

/// Options du flux hôte piloté ([`HostPipeline::run_streaming_pilote`]).
#[derive(Debug, Clone)]
pub struct HostStreamOptions {
    /// Profil de contenu de l'échelle ABR ; `None` coupe la régulation (le
    /// débit reste celui de la configuration de base).
    pub abr_profile: Option<ContentProfile>,
    /// Période d'échantillonnage du chemin réseau pour l'ABR (~1 Hz conseillé).
    pub abr_period: Duration,
    /// Encodage delta **opt-in** : n'activer que si la source de capture
    /// renseigne fidèlement `CapturedFrame::dirty` (toutes les régions
    /// modifiées, défilements inclus). Le capteur DXGI actuel ne rapporte pas
    /// les régions déplacées (`GetFrameMoveRects`) : laisser à `false` avec la
    /// capture d'écran réelle (voir `nd-codec::delta`).
    pub delta_mode: bool,
    /// Chemin d'un fichier MP4 à enregistrer (opt-in) : chaque image encodée y
    /// est poussée, le fichier est clos et relisible en fin de flux.
    pub recording: Option<PathBuf>,
    /// Fiabilité d'émission du flux vidéo. `UnreliableFec` (défaut) =
    /// datagrammes protégés par FEC, comportement historique. `Reliable` = flux
    /// ordonné : requis quand la direction `hôte → contrôleur` porte aussi un
    /// plan de contrôle fiable (audio, presse-papiers, chat, fichiers) — sans
    /// quoi le mélange fiable/datagrammes désynchronise le nonce Noise (voir
    /// [`crate::media`]).
    pub video_reliability: Reliability,
    /// Index du moniteur à diffuser, partagé avec le récepteur (bascule
    /// multi-écran). `None` = pas de bascule (moniteur 0 fixe). Quand la valeur
    /// change, le flux re-cible la capture au vol (best-effort : dépend du
    /// nombre de moniteurs réels).
    pub monitor_switch: Option<Arc<AtomicU32>>,
    /// **Mode confidentialité** (rideau) partagé avec le récepteur : quand le
    /// drapeau est levé, la boucle **cesse d'encoder l'écran réel** et diffuse
    /// un cadre noir opaque (rendu via [`PrivacyState::render_screen_cache`]) —
    /// une image-clé est forcée à chaque bascule pour une transition nette.
    /// `None` = fonction inactive (comportement historique).
    pub privacy: Option<Arc<AtomicBool>>,
    /// **Région / cadre d'écran** partagée : quand la valeur change, la boucle
    /// applique [`ScreenCapturer::set_region`] au vol (best-effort — un backend
    /// qui ne sait pas restreindre **rejette** la demande sans jamais laisser
    /// fuir la zone hors-cadre). `None` = plein écran (pas de restriction).
    pub region_switch: Option<RegionPartagee>,
    /// **Préréglage de qualité** partagé (profil ABR + plafond de débit) : quand
    /// sa `generation` change, la boucle reconfigure l'encodeur et reconstruit
    /// l'échelle ABR sous le nouveau plafond (image-clé de resynchro forcée).
    /// `None` = qualité fixe issue d'[`Self::abr_profile`] (comportement
    /// historique).
    pub quality: Option<Arc<EtatQualite>>,
    /// **Enregistrement à chaud** partagé : quand sa `generation` passe à ≥ 1, la
    /// boucle ouvre (ou ferme proprement) le muxeur MP4 selon son `chemin`,
    /// **au lieu** de l'enregistrement statique d'[`Self::recording`]. `None` =
    /// pas de pilotage à chaud (seul [`Self::recording`] agit, comme avant).
    pub recording_switch: Option<EnregistrementPartage>,
}

impl Default for HostStreamOptions {
    /// ABR actif (profil bureautique [`ContentProfile::Text`], ~1 Hz), delta
    /// coupé, pas d'enregistrement, vidéo en datagrammes+FEC, pas de bascule,
    /// ni confidentialité, ni restriction de région, ni pilotage de qualité ou
    /// d'enregistrement à chaud.
    fn default() -> Self {
        HostStreamOptions {
            abr_profile: Some(ContentProfile::Text),
            abr_period: Duration::from_secs(1),
            delta_mode: false,
            recording: None,
            video_reliability: Reliability::UnreliableFec,
            monitor_switch: None,
            privacy: None,
            region_switch: None,
            quality: None,
            recording_switch: None,
        }
    }
}

/// Instantané poussé par [`HostPipeline::run_streaming_pilote`] à chaque image
/// envoyée (observabilité : statistiques de session, sondes).
#[derive(Debug, Clone, Copy)]
pub struct HostStreamTick {
    /// Débit cible actuellement appliqué à l'encodeur, en kbit/s.
    pub target_bitrate_kbps: u32,
    /// Palier ABR courant (0 = plein régime ; croît en dégradant).
    pub abr_level: u32,
    /// Images écrites dans l'enregistreur depuis le début du flux.
    pub frames_recorded: u64,
}

/// Rapport de fin d'un flux hôte piloté ([`HostPipeline::run_streaming_pilote`]).
#[derive(Debug, Clone)]
pub struct HostStreamReport {
    /// Images envoyées par cet appel.
    pub frames_sent: u64,
    /// Images écrites dans le fichier d'enregistrement.
    pub frames_recorded: u64,
    /// Fichier MP4 clos (relisible), si l'enregistrement était actif et qu'au
    /// moins une image y a été écrite.
    pub recording_path: Option<PathBuf>,
}

/// Enregistreur MP4 du flux hôte : muxeur ouvert paresseusement (les
/// dimensions ne sont connues qu'à la première image capturée).
struct EnregistreurMp4 {
    muxer: Mp4Muxer<File>,
    chemin: PathBuf,
    frames: u64,
}

impl EnregistreurMp4 {
    /// Ouvre le fichier et le muxeur avec les métadonnées de la configuration
    /// d'encodage (dimensions réelles, cadence nominale).
    fn ouvrir(chemin: &Path, cfg: EncoderConfig) -> Result<Self> {
        let fichier = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(chemin)?;
        let muxer = Mp4Muxer::new(
            fichier,
            RecordingMetadata {
                width: cfg.width,
                height: cfg.height,
                fps: cfg.max_fps,
                codec: "h264".to_owned(),
                start_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |e| u64::try_from(e.as_millis()).unwrap_or(u64::MAX)),
            },
        )?;
        Ok(Self {
            muxer,
            chemin: chemin.to_path_buf(),
            frames: 0,
        })
    }

    /// Clôt le fichier : `(images écrites, chemin du MP4 relisible)`. Un
    /// enregistrement resté vide (aucune image) est supprimé plutôt que de
    /// laisser un fichier non décodable.
    fn clore(self) -> Result<(u64, Option<PathBuf>)> {
        if self.frames == 0 {
            drop(self.muxer);
            let _ = std::fs::remove_file(&self.chemin);
            return Ok((0, None));
        }
        self.muxer.finish()?;
        Ok((self.frames, Some(self.chemin)))
    }
}

/// Étage **viewer** de la tranche verticale : réception → décodage H.264 (voir plan 01 §2).
pub struct ViewerPipeline {
    transport: Box<dyn Transport>,
    decoder: Box<dyn VideoDecoder>,
    decoded: usize,
    last_dimensions: Option<(u32, u32)>,
}

impl ViewerPipeline {
    /// Construit l'étage viewer.
    #[must_use]
    pub fn new(transport: Box<dyn Transport>, decoder: Box<dyn VideoDecoder>) -> Self {
        Self {
            transport,
            decoder,
            decoded: 0,
            last_dimensions: None,
        }
    }

    /// Reçoit et décode jusqu'à `target` images.
    /// Renvoie `(nombre décodé, dernières dimensions vues)`.
    pub fn run(&mut self, target: usize) -> Result<(usize, Option<(u32, u32)>)> {
        let max_attempts = target.saturating_mul(200) + 5000;
        let mut attempts = 0usize;
        while self.decoded < target && attempts < max_attempts {
            attempts += 1;
            match self.transport.poll_recv()? {
                Some((_handle, data)) => {
                    let chunk = EncodedChunk {
                        data,
                        is_keyframe: false,
                        monitor: MonitorId(0),
                        timestamp_us: 0,
                    };
                    if let Some(frame) = self.decoder.decode(&chunk)? {
                        self.decoded += 1;
                        self.last_dimensions = Some((frame.width, frame.height));
                    }
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        Ok((self.decoded, self.last_dimensions))
    }

    /// Draine le transport : décode **tout** ce qui est en attente et renvoie la
    /// frame la plus récente (les frames en retard d'une même rafale sont décodées
    /// — le décodeur H.264 a besoin de chaque unité — puis sautées à la livraison).
    fn drainer_rafale(&mut self) -> Result<Option<DecodedFrame>> {
        let mut plus_recente = None;
        while let Some((_canal, donnees)) = self.transport.poll_recv()? {
            let chunk = EncodedChunk {
                data: donnees,
                is_keyframe: false,
                monitor: MonitorId(0),
                timestamp_us: 0,
            };
            if let Some(frame) = self.decoder.decode(&chunk)? {
                self.decoded += 1;
                self.last_dimensions = Some((frame.width, frame.height));
                plus_recente = Some(frame);
            }
        }
        Ok(plus_recente)
    }

    /// Mode « flux continu » : reçoit et décode jusqu'à la levée du signal `stop`,
    /// en passant au callback la frame **la plus récente** de chaque rafale (skip
    /// des frames en retard, comme la fenêtre de démo `viewer_window`). Le callback
    /// est le point de branchement du consommateur (UI, canal, enregistreur…).
    ///
    /// Renvoie le nombre de frames livrées au callback. [`ViewerPipeline::run`]
    /// reste disponible pour un décompte borné.
    pub fn run_streaming(
        &mut self,
        mut on_frame: impl FnMut(DecodedFrame),
        stop: Arc<AtomicBool>,
    ) -> Result<usize> {
        let mut livrees = 0usize;
        while !stop.load(Ordering::Relaxed) {
            match self.drainer_rafale()? {
                Some(frame) => {
                    on_frame(frame);
                    livrees += 1;
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        Ok(livrees)
    }
}

/// Longueur maximale de clair par message Noise (marge sous 65535 − tag AEAD).
const NOISE_MAX_PLAINTEXT: usize = 60_000;

/// Transport **chiffré de bout en bout** : enveloppe un [`Transport`] et chiffre toutes
/// les charges via une session Noise (voir plan 06). Le transport/relais sous-jacent ne
/// voit que du ciphertext — connaissance nulle côté serveur.
pub struct EncryptedTransport {
    inner: Box<dyn Transport>,
    session: NoiseSession,
}

impl EncryptedTransport {
    /// Empreinte de la clé statique locale (à afficher/comparer, voir plan 06 §SAS).
    #[must_use]
    pub fn local_fingerprint(&self) -> PeerFingerprint {
        self.session.local_fingerprint()
    }

    /// Empreinte de la clé statique du pair distant (après handshake).
    #[must_use]
    pub fn remote_fingerprint(&self) -> Option<PeerFingerprint> {
        self.session.remote_fingerprint()
    }
}

/// Établit une session chiffrée de bout en bout par-dessus un transport, en réalisant
/// le handshake Noise XX sur le canal de contrôle. Voir plan 06.
pub fn establish(
    mut inner: Box<dyn Transport>,
    role: HandshakeRole,
    static_private_key: &[u8],
) -> Result<EncryptedTransport> {
    let mut handshake = NoiseHandshake::new(role, static_private_key)?;
    let control = inner.open_channel(ChannelKind::Control);
    // XX : l'initiateur écrit le premier message, puis on alterne écriture/lecture.
    let mut my_turn_to_write = matches!(role, HandshakeRole::Initiator);
    while !handshake.is_finished() {
        if my_turn_to_write {
            let msg = handshake.write_message(&[])?;
            inner.send(control, msg, Reliability::Reliable)?;
        } else {
            let msg = recv_blocking(inner.as_mut())?;
            handshake.read_message(&msg)?;
        }
        my_turn_to_write = !my_turn_to_write;
    }
    let session = handshake.into_session()?;
    Ok(EncryptedTransport { inner, session })
}

/// Attend (avec délai de garde) le prochain message reçu du transport.
fn recv_blocking(inner: &mut dyn Transport) -> Result<Vec<u8>> {
    for _ in 0..3000 {
        if let Some((_handle, data)) = inner.poll_recv()? {
            return Ok(data);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(NdError::Crypto("délai de handshake Noise dépassé".into()))
}

fn read_be_u32(d: &[u8], p: &mut usize) -> Result<u32> {
    let bytes = d
        .get(*p..*p + 4)
        .ok_or_else(|| NdError::Crypto("cadre chiffré tronqué".into()))?;
    *p += 4;
    Ok(u32::from_be_bytes(
        bytes.try_into().expect("tranche de 4 octets"),
    ))
}

impl Transport for EncryptedTransport {
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
        self.inner.open_channel(kind)
    }

    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, reliability: Reliability) -> Result<()> {
        // Découpe le clair en morceaux ≤ NOISE_MAX_PLAINTEXT, chiffre chacun, et encadre :
        // [u32 n][ (u32 len, ciphertext) × n ]. Ordre préservé (flux fiable ordonné) → les
        // compteurs de nonce Noise restent synchronisés entre les deux pairs.
        let mut framed = Vec::with_capacity(data.len() + 32);
        if data.is_empty() {
            framed.extend_from_slice(&1u32.to_be_bytes());
            let ct = self.session.encrypt(&[])?;
            framed.extend_from_slice(&(ct.len() as u32).to_be_bytes());
            framed.extend_from_slice(&ct);
        } else {
            let count = data.len().div_ceil(NOISE_MAX_PLAINTEXT) as u32;
            framed.extend_from_slice(&count.to_be_bytes());
            for chunk in data.chunks(NOISE_MAX_PLAINTEXT) {
                let ct = self.session.encrypt(chunk)?;
                framed.extend_from_slice(&(ct.len() as u32).to_be_bytes());
                framed.extend_from_slice(&ct);
            }
        }
        self.inner.send(ch, framed, reliability)
    }

    fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
        let Some((handle, framed)) = self.inner.poll_recv()? else {
            return Ok(None);
        };
        let mut pos = 0usize;
        let count = read_be_u32(&framed, &mut pos)? as usize;
        let mut plaintext = Vec::new();
        for _ in 0..count {
            let clen = read_be_u32(&framed, &mut pos)? as usize;
            let end = pos
                .checked_add(clen)
                .ok_or_else(|| NdError::Crypto("cadre chiffré invalide".into()))?;
            let ciphertext = framed
                .get(pos..end)
                .ok_or_else(|| NdError::Crypto("cadre chiffré tronqué".into()))?;
            pos = end;
            let pt = self.session.decrypt(ciphertext)?;
            plaintext.extend_from_slice(&pt);
        }
        Ok(Some((handle, plaintext)))
    }

    fn path_estimate(&self) -> PathEstimate {
        self.inner.path_estimate()
    }

    /// Délègue l'état de connexion au transport sous-jacent : sans cette
    /// délégation, [`nd_transport::ReconnectingTransport`] (qui scrute
    /// `is_connected`) ne verrait jamais la coupure à travers la couche chiffrée.
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(role: SessionRole, peer: Option<NovaId>) -> SessionConfig {
        SessionConfig {
            role,
            local_id: NovaId(123_456_789),
            peer_id: peer,
            permissions: Permissions::default(),
        }
    }

    #[test]
    fn nouvelle_session_est_idle() {
        let s = Session::new(cfg(SessionRole::Controlled, None));
        assert_eq!(s.state(), SessionState::Idle);
    }

    #[test]
    fn controleur_sans_pair_echoue() {
        let mut s = Session::new(cfg(SessionRole::Controller, None));
        assert!(s.begin().is_err());
    }

    #[test]
    fn controleur_avec_pair_passe_en_resolving() {
        let mut s = Session::new(cfg(SessionRole::Controller, Some(NovaId(987_654_321))));
        s.begin().unwrap();
        assert_eq!(s.state(), SessionState::Resolving);
    }

    #[test]
    fn close_remet_en_closed() {
        let mut s = Session::new(cfg(SessionRole::Controlled, None));
        s.close();
        assert_eq!(s.state(), SessionState::Closed);
    }
}

/// Preuve **déterministe** (sans matériel) du mode confidentialité et du cadre
/// d'écran dans la boucle de diffusion hôte : un capteur factice à motif clair,
/// un transport collecteur, l'encodeur/décodeur **logiciels** réels. On décode
/// ce qui part sur le fil et on vérifie le contenu (noir sous rideau, clair
/// sinon) et les dimensions (rognées au cadre demandé).
#[cfg(test)]
mod tests_diffusion_avancee {
    use super::*;
    use nd_capture::{CaptureEvent, CapturedFrame};
    use nd_codec::{create_decoder, create_encoder};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// Capteur factice : rend des frames au **motif clair uniforme** (240),
    /// éventuellement rognées à la région active ; journalise les régions vues.
    struct CapteurFactice {
        largeur: u32,
        hauteur: u32,
        region: Option<Rect>,
        regions_vues: Arc<Mutex<Vec<Option<Rect>>>>,
    }

    impl ScreenCapturer for CapteurFactice {
        fn start(&mut self, _cfg: CaptureConfig) -> Result<()> {
            Ok(())
        }

        fn next_frame(&mut self) -> Result<CapturedFrame> {
            let (w, h) = self
                .region
                .map_or((self.largeur, self.hauteur), |r| (r.w, r.h));
            let data = vec![240u8; (w * h * 4) as usize];
            Ok(CapturedFrame {
                width: w,
                height: h,
                monitor: MonitorId(0),
                format: PixelFormat::Bgra8,
                dirty: vec![Rect { x: 0, y: 0, w, h }],
                cursor: None,
                timestamp_us: 0,
                image: Some(FrameImage::Cpu {
                    data,
                    stride: (w * 4) as usize,
                }),
            })
        }

        fn poll_event(&mut self) -> Option<CaptureEvent> {
            None
        }

        fn stop(&mut self) {}

        fn set_region(&mut self, region: Option<Rect>) -> Result<()> {
            self.region = region;
            self.regions_vues
                .lock()
                .expect("verrou régions")
                .push(region);
            Ok(())
        }
    }

    /// Transport collecteur : mémorise les charges vidéo émises, ne reçoit rien.
    struct TransportCollecteur {
        envoyes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Transport for TransportCollecteur {
        fn open_channel(&mut self, _kind: ChannelKind) -> ChannelHandle {
            ChannelHandle(0)
        }

        fn send(&mut self, _ch: ChannelHandle, data: Vec<u8>, _r: Reliability) -> Result<()> {
            self.envoyes.lock().expect("verrou envoyés").push(data);
            Ok(())
        }

        fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
            Ok(None)
        }

        fn path_estimate(&self) -> PathEstimate {
            PathEstimate::default()
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    /// Fait tourner la boucle de diffusion ~250 ms avec le capteur factice et
    /// rend `(charges vidéo émises, régions demandées à la capture)`.
    fn diffuser(
        privacy: Option<Arc<AtomicBool>>,
        region_switch: Option<RegionPartagee>,
    ) -> (Vec<Vec<u8>>, Vec<Option<Rect>>) {
        let regions_vues = Arc::new(Mutex::new(Vec::new()));
        let envoyes = Arc::new(Mutex::new(Vec::new()));
        let capteur = Box::new(CapteurFactice {
            largeur: 64,
            hauteur: 64,
            region: None,
            regions_vues: Arc::clone(&regions_vues),
        });
        let encodeur = create_encoder(CodecKind::H264).expect("encodeur logiciel");
        let transport = Box::new(TransportCollecteur {
            envoyes: Arc::clone(&envoyes),
        });
        let mut hote = HostPipeline::new(capteur, encodeur, transport).expect("pipeline");
        let options = HostStreamOptions {
            abr_profile: None,
            privacy,
            region_switch,
            ..HostStreamOptions::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_boucle = Arc::clone(&stop);
        let jh = std::thread::spawn(move || {
            let _ = hote.run_streaming_pilote(stop_boucle, options, |_tick| {});
        });
        std::thread::sleep(Duration::from_millis(250));
        stop.store(true, Ordering::Relaxed);
        jh.join().expect("thread de diffusion");
        let envoyes = envoyes.lock().expect("verrou envoyés").clone();
        let regions = regions_vues.lock().expect("verrou régions").clone();
        (envoyes, regions)
    }

    /// Décode les charges collectées et rend `(dernière frame, rouge moyen)`.
    fn decoder(charges: &[Vec<u8>]) -> Option<(DecodedFrame, u32)> {
        let mut decodeur = create_decoder(CodecKind::H264).expect("décodeur logiciel");
        let mut derniere = None;
        for data in charges {
            let chunk = EncodedChunk {
                data: data.clone(),
                is_keyframe: false,
                monitor: MonitorId(0),
                timestamp_us: 0,
            };
            if let Ok(Some(frame)) = decodeur.decode(&chunk) {
                derniere = Some(frame);
            }
        }
        derniere.map(|frame| {
            let somme: u32 = frame.rgba.chunks_exact(4).map(|p| u32::from(p[0])).sum();
            let moyenne = somme / (frame.rgba.len() / 4).max(1) as u32;
            (frame, moyenne)
        })
    }

    #[test]
    fn confidentialite_diffuse_un_cadre_noir() {
        let privacy = Arc::new(AtomicBool::new(true)); // rideau dès le départ
        let (charges, _regions) = diffuser(Some(privacy), None);
        assert!(!charges.is_empty(), "des cadres doivent être émis");
        let (frame, rouge_moyen) = decoder(&charges).expect("au moins un cadre décodé");
        // Le motif capté est clair (240) ; sous rideau, le décodé est noir.
        assert!(
            rouge_moyen < 64,
            "cadre attendu noir sous confidentialité (rouge moyen = {rouge_moyen})"
        );
        assert_eq!((frame.width, frame.height), (64, 64));
    }

    #[test]
    fn cadre_ecran_restreint_la_zone_et_laisse_passer_l_ecran() {
        // Confidentialité coupée : l'écran réel (clair) doit passer, rogné au cadre.
        let region: RegionPartagee = Arc::new(Mutex::new(Some(Rect {
            x: 16,
            y: 8,
            w: 32,
            h: 32,
        })));
        let (charges, regions) = diffuser(None, Some(Arc::clone(&region)));
        assert!(
            regions.contains(&Some(Rect {
                x: 16,
                y: 8,
                w: 32,
                h: 32
            })),
            "la capture doit avoir reçu le cadre d'écran demandé : {regions:?}"
        );
        let (frame, rouge_moyen) = decoder(&charges).expect("au moins un cadre décodé");
        assert_eq!(
            (frame.width, frame.height),
            (32, 32),
            "les cadres diffusés doivent être rognés à la sous-région"
        );
        assert!(
            rouge_moyen > 160,
            "hors confidentialité, l'écran réel (clair) doit passer (rouge moyen = {rouge_moyen})"
        );
    }
}
