//! # `linux_pipewire` — Consommation du flux PipeWire (chemin CPU)
//!
//! Deuxième moitié du backend Wayland : à partir du `PortalStream` négocié par
//! [`super::linux_portal`], on ouvre un flux PipeWire, on négocie un format vidéo
//! brut, puis on lit les trames pour les publier sous forme de [`crate::CapturedFrame`].
//!
//! ## Honnêteté sur le périmètre
//! **Compilation / exécution validées uniquement sur un vrai Linux Wayland avec
//! `libpipewire` ; le chemin CPU lit les buffers MemFd/MemPtr, le zéro-copie DMA-BUF
//! est un jet ultérieur.** Concrètement, quand PipeWire nous livre un buffer DMA-BUF
//! (mémoire GPU), `Data::data()` renvoie `None` : on saute simplement la trame dans ce
//! chemin CPU. Un futur chemin zéro-copie importera le DMA-BUF côté GPU.
//!
//! ## Architecture (miroir du backend DXGI)
//! * `start()` : négocie le portail (bloquant), crée un canal `mpsc` borné, puis lance
//!   un **thread dédié** qui fait tourner la `MainLoop` PipeWire.
//! * Le callback `process` du flux déclenche à chaque trame : il *dequeue* le buffer,
//!   convertit le format SPA négocié en **BGRA8**, et pousse la trame via un
//!   `SyncSender` (`try_send` → on jette si la file est pleine, jamais de blocage RT).
//! * `next_frame()` lit le canal avec un `recv_timeout`. En cas de timeout, il renvoie
//!   une trame « vide » (`image: None`, `dirty` vide, dernières dimensions connues),
//!   exactement comme le `empty_frame` du backend DXGI.
//! * `stop()` signale la boucle via `pipewire::channel` (→ `MainLoop::quit`) puis joint
//!   le thread.
//!
//! ## Note de version PipeWire
// NOTE (à valider sur Linux) : ce code cible **pipewire-rs 0.10** (mai 2026), qui a
// introduit les types *possédés* `MainLoopRc` / `ContextRc` / `StreamBox` — `Stream`
// étant désormais un simple wrapper *emprunté*, et les callbacks recevant `&Stream`.
// L'ancienne API 0.8/0.9 exposait `MainLoop::new` / `Context::new` / `Stream::new`.
// Les macros `properties!`, `object!`, `property!` et l'API `VideoInfoRaw` sont inchangées.
#![allow(unsafe_code)]

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire::properties::properties;
use pipewire::spa;
use pipewire::stream::StreamFlags;

use nd_proto::{MonitorId, NdError};

use crate::{
    CaptureConfig, CaptureEvent, CapturedFrame, FrameImage, PixelFormat, Rect, ScreenCapturer,
};

// NOTE (à valider sur Linux) : le chemin exact dépend de la manière dont l'appelant
// déclare les `mod` (les deux fichiers sont supposés être des modules frères).
use super::linux_portal::{negotiate_screencast, PortalStream};

/// Données partagées entre les callbacks du flux (`param_changed` écrit, `process` lit).
struct UserData {
    /// Format vidéo brut négocié, renseigné dans `param_changed`.
    format: spa::param::video::VideoInfoRaw,
}

/// Backend de capture Wayland basé sur xdg-desktop-portal + PipeWire.
pub(crate) struct PipeWireCapturer {
    monitor: MonitorId,
    target_fps: u32,
    /// Conservé pour un usage futur (mode curseur déjà passé au portail au `start`).
    #[allow(dead_code)]
    capture_cursor: bool,
    /// Seul `None` (plein écran) est honoré ; `Some(region)` renvoie `NotImplemented`.
    #[allow(dead_code)]
    region: Option<Rect>,

    /// Réception des trames produites par le thread PipeWire.
    rx: Option<Receiver<CapturedFrame>>,
    /// Émetteur de la commande d'arrêt vers la `MainLoop` (via `pipewire::channel`).
    quit_tx: Option<pipewire::channel::Sender<()>>,
    /// Poignée du thread de la boucle PipeWire.
    thread: Option<JoinHandle<()>>,

    /// Dernières dimensions connues (pour les trames vides sur timeout).
    last_w: u32,
    last_h: u32,
    /// Base de temps pour les horodatages en microsecondes.
    started_at: Option<Instant>,
}

impl PipeWireCapturer {
    pub(crate) fn new() -> Self {
        Self {
            monitor: MonitorId(0),
            target_fps: 30,
            capture_cursor: false,
            region: None,
            rx: None,
            quit_tx: None,
            thread: None,
            last_w: 0,
            last_h: 0,
            started_at: None,
        }
    }

    /// Fabrique une trame « vide » (mirroir du `empty_frame` DXGI) : pas d'image, pas
    /// de zone sale, mais on conserve les dernières dimensions connues.
    fn empty_frame(&self) -> CapturedFrame {
        CapturedFrame {
            width: self.last_w,
            height: self.last_h,
            monitor: self.monitor,
            format: PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: self
                .started_at
                .map(|t| t.elapsed().as_micros() as u64)
                .unwrap_or(0),
            image: None,
        }
    }
}

impl Default for PipeWireCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenCapturer for PipeWireCapturer {
    fn start(&mut self, cfg: CaptureConfig) -> nd_proto::Result<()> {
        // Redémarrage idempotent : on arrête une éventuelle session précédente.
        self.stop();

        self.monitor = cfg.monitor;
        self.target_fps = cfg.target_fps.max(1);
        self.capture_cursor = cfg.capture_cursor;

        // (bloquant) Poignée de main du portail : peut afficher une boîte de consentement.
        let portal = negotiate_screencast(cfg.capture_cursor)?;

        // Indice de taille : évite un premier `empty_frame` en 0x0.
        if let Some((w, h)) = portal.size {
            self.last_w = w;
            self.last_h = h;
        }

        // Canal des trames : borné (drop de la plus récente si plein), jamais de blocage.
        let (frame_tx, frame_rx) = sync_channel::<CapturedFrame>(3);
        // Canal de contrôle PipeWire : `send(())` depuis `stop()` → `MainLoop::quit`.
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();

        let monitor = cfg.monitor;

        let handle = std::thread::Builder::new()
            .name("nd-capture-pw".into())
            .spawn(move || {
                if let Err(e) = run_pipewire_loop(portal, monitor, frame_tx, quit_rx) {
                    eprintln!("[nd-capture] boucle PipeWire terminée sur erreur : {e}");
                }
            })
            .map_err(|e| NdError::Capture(format!("échec du spawn du thread PipeWire : {e}")))?;

        self.rx = Some(frame_rx);
        self.quit_tx = Some(quit_tx);
        self.thread = Some(handle);
        self.started_at = Some(Instant::now());
        Ok(())
    }

    fn next_frame(&mut self) -> nd_proto::Result<CapturedFrame> {
        let Some(rx) = self.rx.as_ref() else {
            return Err(NdError::Capture("next_frame appelé avant start".into()));
        };

        // Timeout ≈ deux intervalles de trame, borné à [10 ms, 250 ms].
        let ms = (2000 / self.target_fps.max(1)).clamp(10, 250) as u64;

        match rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(frame) => {
                // Mémorise les dimensions pour les futures trames vides.
                self.last_w = frame.width;
                self.last_h = frame.height;
                Ok(frame)
            }
            // Pas de nouvelle trame à temps : on renvoie une trame vide (comme DXGI).
            Err(RecvTimeoutError::Timeout) => Ok(self.empty_frame()),
            // Le thread PipeWire a rendu l'âme : on remonte l'erreur.
            Err(RecvTimeoutError::Disconnected) => {
                Err(NdError::Capture("le thread PipeWire s'est arrêté".into()))
            }
        }
    }

    fn poll_event(&mut self) -> Option<CaptureEvent> {
        // Aucun événement (résolution / bureau sécurisé) n'est remonté par ce backend
        // pour l'instant. Un changement de résolution se traduira par de nouvelles
        // dimensions dans `param_changed`, non par un `CaptureEvent`.
        None
    }

    fn stop(&mut self) {
        // Demande l'arrêt de la MainLoop (le récepteur attaché appellera `quit()`)…
        if let Some(tx) = self.quit_tx.take() {
            let _ = tx.send(());
        }
        // … puis attend la fin propre du thread (qui libère la session du portail).
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
        self.rx = None;
        self.started_at = None;
    }

    fn set_region(&mut self, region: Option<Rect>) -> nd_proto::Result<()> {
        match region {
            None => {
                self.region = None;
                Ok(())
            }
            // Les flux du portail ScreenCast ne permettent pas le rognage sous-région
            // sans travail supplémentaire (recadrage logiciel côté consommateur).
            Some(_) => Err(NdError::NotImplemented(
                "PipeWireCapturer : rognage sous-région non supporté sur un flux portail",
            )),
        }
    }
}

/// Corps du thread : monte le pipeline PipeWire et fait tourner la boucle jusqu'au
/// signal d'arrêt. Toute erreur d'initialisation est renvoyée sous forme de `String`
/// (journalisée par l'appelant).
fn run_pipewire_loop(
    portal: PortalStream,
    monitor: MonitorId,
    frame_tx: SyncSender<CapturedFrame>,
    quit_rx: pipewire::channel::Receiver<()>,
) -> Result<(), String> {
    // On récupère le fd (consommé par `connect_fd_rc`) et la poignée de session.
    // `session_alive` DOIT survivre à `mainloop.run()` : on la libère explicitement
    // à la toute fin (voir `drop(session_alive)`), ce qui garantit qu'elle n'est pas
    // droppée prématurément par le compilateur.
    let (fd, node_id, _size, session_alive) = portal.into_parts();

    // Initialise la bibliothèque PipeWire (idempotent, refcompté). Fonction sûre.
    pipewire::init();

    // Boucle principale (type possédé, ref-compté et `Clone` en 0.10).
    let mainloop =
        pipewire::main_loop::MainLoopRc::new(None).map_err(|e| format!("MainLoop : {e}"))?;

    // Récepteur de contrôle : un `send(())` depuis un autre thread déclenche `quit()`.
    let _attached = quit_rx.attach(mainloop.loop_(), {
        let ml = mainloop.clone();
        move |_| ml.quit()
    });

    // Contexte + connexion via le fd du portail (variante `_rc` → `CoreRc` possédé).
    let context =
        pipewire::context::ContextRc::new(&mainloop, None).map_err(|e| format!("Context : {e}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| format!("connect_fd : {e}"))?;

    // Le flux d'entrée. `properties!` renvoie une `PropertiesBox` consommée par `new`.
    let stream = pipewire::stream::StreamBox::new(
        &core,
        "novadesk-screencast",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("Stream : {e}"))?;

    // Base de temps monotone pour les horodatages des trames.
    let start_ts = Instant::now();

    // On enregistre les callbacks. Le `StreamListener` renvoyé DOIT rester vivant pour
    // que les callbacks continuent d'être appelés — d'où la liaison `_listener`.
    let _listener = stream
        .add_local_listener_with_user_data(UserData {
            format: spa::param::video::VideoInfoRaw::new(),
        })
        // Signature 0.10 : FnMut(&Stream, &mut D, StreamState, StreamState).
        .state_changed(|_stream, _ud, old, new| {
            eprintln!("[nd-capture] état PipeWire : {old:?} -> {new:?}");
        })
        // Négociation de format : PipeWire nous notifie le format retenu.
        // Signature 0.10 : FnMut(&Stream, &mut D, u32, Option<&Pod>).
        .param_changed(|_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            // NOTE (à valider sur Linux) : `format_utils::parse_format` renvoie
            // `(MediaType, MediaSubtype)` — helper canonique de pipewire-rs.
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            // Renseigne `VideoInfoRaw` (format pixel + taille + framerate).
            if ud.format.parse(param).is_err() {
                eprintln!("[nd-capture] échec du parse VideoInfoRaw");
            }
        })
        // Livraison des trames. Signature 0.10 : FnMut(&Stream, &mut D).
        .process(move |stream, ud| {
            // `dequeue_buffer` récupère un buffer prêt ; `None` si rien à traiter.
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];

            // Dimensions et format négociés (VideoInfoRaw est `Copy`).
            let size = ud.format.size();
            let (w, h) = (size.width as usize, size.height as usize);
            if w == 0 || h == 0 {
                return;
            }
            let vformat = ud.format.format();

            // On lit d'abord le `Chunk` (emprunt immuable), AVANT `data.data()` qui
            // emprunte `data` de façon mutable. On copie donc les scalaires ici.
            let chunk = data.chunk();
            let offset = chunk.offset() as usize;
            let mut src_stride = chunk.stride() as usize;
            let chunk_size = chunk.size() as usize;

            // SAFETY : aucun bloc `unsafe` n'est nécessaire ici. pipewire-rs 0.10 expose
            // le buffer mmap (MemPtr / MemFd) sous forme d'un `&mut [u8]` déjà borné et
            // vérifié : `Data::data()` fait la carto et renvoie `Some(slice)`. Pour un
            // buffer **DMA-BUF** (mémoire GPU non cartographiée) elle renvoie `None`,
            // ce qui, dans ce chemin CPU, revient à sauter la trame — comportement voulu.
            // Le futur chemin zéro-copie DMA-BUF, lui, manipulera des `RawFd` / pointeurs
            // bruts et nécessitera de vrais blocs `unsafe` accompagnés de `// SAFETY:`.
            let Some(map) = data.data() else {
                return;
            };
            if map.len() < offset + chunk_size {
                return;
            }
            let src = &map[offset..offset + chunk_size];

            // Certains producteurs laissent le stride à 0 : on le calcule alors densément.
            if src_stride == 0 {
                src_stride = w * 4;
            }

            let Some(bgra) = convert_to_bgra(src, w, h, src_stride, vformat) else {
                return;
            };

            let frame = CapturedFrame {
                width: size.width,
                height: size.height,
                monitor,
                format: PixelFormat::Bgra8,
                // Chemin CPU simple : on marque toute l'image comme « sale ».
                dirty: vec![Rect {
                    x: 0,
                    y: 0,
                    w: size.width,
                    h: size.height,
                }],
                // Curseur : incrusté dans les pixels (Embedded) ou masqué (Hidden) — dans
                // les deux cas, pas de position hors bande à exposer ici.
                cursor: None,
                timestamp_us: start_ts.elapsed().as_micros() as u64,
                image: Some(FrameImage::Cpu {
                    data: bgra,
                    stride: w * 4,
                }),
            };

            // `try_send` : si le consommateur est en retard (file pleine), on jette cette
            // trame plutôt que de bloquer le thread temps réel de PipeWire.
            let _ = frame_tx.try_send(frame);
        })
        .register()
        .map_err(|e| format!("register listener : {e}"))?;

    // Construit le POD de format proposé et connecte le flux sur le nœud du portail.
    let pod_bytes = build_format_pod();
    let pod = spa::pod::Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| "POD de format invalide".to_string())?;
    let mut params = [pod];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            // AUTOCONNECT : laisse le gestionnaire de session lier le nœud.
            // MAP_BUFFERS : cartographie les buffers pour que `Data::data()` renvoie un
            // slice CPU lisible (indispensable au chemin CPU).
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("stream.connect : {e}"))?;

    // Boucle bloquante jusqu'à réception de la commande d'arrêt (→ `quit()`).
    mainloop.run();

    // Fermeture explicite de la session du portail APRÈS l'arrêt de la boucle et le
    // démontage implicite du flux/contexte. Garantit l'ordre : flux tué, puis session.
    drop(session_alive);
    Ok(())
}

/// Convertit une trame SPA (BGRx/BGRA/RGBx/RGBA) vers du **BGRA8** dense (stride = 4·w).
///
/// * BGRx / BGRA : copie directe ligne à ligne (alpha forcé opaque pour BGRx).
/// * RGBx / RGBA : échange R↔B pour obtenir du BGRA (alpha forcé opaque pour RGBx).
///
/// Renvoie `None` pour tout format non géré par ce chemin CPU (NV12, YUY2, I420…).
fn convert_to_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
    format: spa::param::video::VideoFormat,
) -> Option<Vec<u8>> {
    use spa::param::video::VideoFormat as VF;

    // `swap_rb` : faut-il permuter les canaux rouge et bleu ?
    // `has_alpha` : le format porte-t-il un vrai canal alpha (sinon on force 255) ?
    let (swap_rb, has_alpha) = match format {
        VF::BGRA => (false, true),
        VF::BGRx => (false, false),
        VF::RGBA => (true, true),
        VF::RGBx => (true, false),
        _ => return None,
    };

    let dst_stride = width * 4;
    let mut out = vec![0u8; dst_stride * height];

    for y in 0..height {
        let s_off = y * src_stride;
        let d_off = y * dst_stride;
        // Garde-fou : ne jamais déborder du buffer source (padding, tailles limites…).
        if s_off + dst_stride > src.len() {
            break;
        }
        let s = &src[s_off..s_off + dst_stride];
        let d = &mut out[d_off..d_off + dst_stride];

        if swap_rb {
            for x in 0..width {
                let p = x * 4;
                d[p] = s[p + 2]; // B <- R
                d[p + 1] = s[p + 1]; // G
                d[p + 2] = s[p]; // R <- B
                d[p + 3] = if has_alpha { s[p + 3] } else { 255 };
            }
        } else {
            // Ordre de canaux déjà BGRA : copie directe…
            d.copy_from_slice(s);
            // … puis on force l'opacité si le format n'a pas d'alpha (BGRx).
            if !has_alpha {
                for x in 0..width {
                    d[x * 4 + 3] = 255;
                }
            }
        }
    }

    Some(out)
}

/// Construit le POD `EnumFormat` proposé à PipeWire : vidéo brute, un jeu de formats
/// pixel gérables par le chemin CPU, une plage de tailles et de framerates.
///
/// Les macros `object!` / `property!` de `libspa::pod` produisent l'objet ; on le
/// sérialise ensuite en octets bruts via `PodSerializer` pour `Stream::connect`.
fn build_format_pod() -> Vec<u8> {
    use spa::param::video::VideoFormat as VF;

    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        // media type / subtype : vidéo brute.
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        // Formats pixel acceptés (le 1er sert de valeur par défaut du choix Enum).
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VF::BGRx,
            VF::BGRx,
            VF::BGRA,
            VF::RGBx,
            VF::RGBA
        ),
        // Plage de tailles : défaut 1920x1080, de 1x1 à 8192x8192.
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        // Plage de framerates : défaut 60, de 0 (variable) à 240 fps.
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );

    // Sérialisation POD → octets. `Value::Object` enveloppe l'objet produit par `object!`.
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("sérialisation du POD de format")
    .0
    .into_inner()
}
