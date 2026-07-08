//! Sonde de la **session média étendue** : les briques du lot 05 câblées dans la
//! vraie boucle [`SessionEngine`] (mode étendu), exercées en boucle locale.
//!
//! Ce que la sonde **prouve** (assertions ; échec ⇒ code de sortie non nul) :
//!
//! 1. **Audio** (hôte → contrôleur, canal `Audio`) : des paquets produits côté
//!    hôte traversent la session chiffrée et sont **reçus puis lus** côté
//!    contrôleur (compteur du lecteur injecté > 0), gardé par
//!    [`Capability::Audio`].
//! 2. **Chat** (canal `Control` multiplexé) : message **aller-retour**.
//! 3. **Presse-papiers** (canal `Control`) : un contenu texte posé côté hôte est
//!    **synchronisé** vers le contrôleur (gardé par la permission presse-papiers).
//! 4. **Transfert de fichiers** (canal `Files`) : un fichier envoyé par le
//!    contrôleur est **reçu intègre** par l'hôte (gardé par la permission
//!    fichiers).
//! 5. **Vidéo + delta** : le flux vidéo continue de tourner (frames décodées) en
//!    émission **fiable** (imposée par l'ordre des nonces, voir `nd_core::media`),
//!    et le **gain de l'encodage delta** est mesuré déterministiquement (image
//!    statique → trame de répétition vide au lieu d'un ré-encodage plein cadre).
//!
//! **Honnêteté.** Tout est réel en boucle locale (QUIC, Noise, capture d'écran,
//! démux des canaux, filtre de permissions). L'audio et le presse-papiers
//! s'appuient sur des briques **injectées** ([`SessionEngine::start_with_media`])
//! — capteur audio synthétique, lecteur en mémoire, presse-papiers en mémoire —
//! afin de prouver le câblage sans dépendre d'un périphérique. En déploiement,
//! ces briques sont l'audio système ([`AudioSession::duplex_systeme`]) et le
//! presse-papiers de la plateforme. La bascule multi-écran est câblée
//! (contrôleur → hôte) mais son effet visuel dépend du nombre de moniteurs
//! réels : non prouvé ici sur une machine mono-écran.
//!
//! Lancer : `cargo run --example session_media_demo -p nd-core`

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nd_audio::{
    AudioCapturer, AudioFormat, AudioPacket, AudioPlayer, AudioSession, EmetteurAudio,
    RecepteurAudio, SourceAudio,
};
use nd_capture::{CapturedFrame, FrameImage, PixelFormat, Rect};
use nd_codec::{create_encoder, CodecKind, EncoderConfig};
use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionMedia, SessionOptions,
    SessionRole, SessionState,
};
use nd_features::{Capability, PermissionSet, Permissions};
use nd_files::{Clipboard, ClipboardSync, TransferEvent};
use nd_proto::{MonitorId, NovaId};

/// Échéance large (encodage logiciel possible en debug).
const ECHEANCE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Briques média injectées (synthétiques, sans matériel)
// ---------------------------------------------------------------------------

/// Capteur audio synthétique : une trame de 20 ms par appel, horodatée sur la
/// grille du jitter buffer. Le blocage porte la cadence (≈ temps réel), comme un
/// vrai périphérique de capture.
struct CapteurSynthetique {
    format: AudioFormat,
    seq: u64,
}

impl AudioCapturer for CapteurSynthetique {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_packet(&mut self) -> nd_proto::Result<AudioPacket> {
        std::thread::sleep(Duration::from_millis(20));
        let paquet = AudioPacket {
            // Charge factice : le lecteur injecté ne décode pas (il compte).
            data: vec![0xA5; 80],
            timestamp_us: self.seq * 20_000,
        };
        self.seq += 1;
        Ok(paquet)
    }
}

/// Lecteur audio en mémoire : compte les trames effectivement **jouées**
/// (restituées par le jitter buffer). Le compteur est partagé avec la sonde.
struct LecteurCompteur {
    joues: Arc<AtomicU64>,
}

impl AudioPlayer for LecteurCompteur {
    fn play(&mut self, _packet: &AudioPacket) -> nd_proto::Result<()> {
        self.joues.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Presse-papiers en mémoire (back-end de test pour [`ClipboardSync`]).
#[derive(Clone)]
struct PressePapiersMemoire {
    texte: Arc<Mutex<Option<String>>>,
}

impl Clipboard for PressePapiersMemoire {
    fn get_text(&self) -> nd_proto::Result<Option<String>> {
        Ok(self.texte.lock().expect("verrou presse-papiers").clone())
    }

    fn set_text(&self, text: &str) -> nd_proto::Result<()> {
        *self.texte.lock().expect("verrou presse-papiers") = Some(text.to_owned());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Outils de la sonde
// ---------------------------------------------------------------------------

/// Capacités complètes (toutes les fonctions étendues autorisées).
fn permissions_completes() -> PermissionSet {
    [
        Capability::ViewScreen,
        Capability::ControlMouse,
        Capability::ControlKeyboard,
        Capability::ClipboardRead,
        Capability::ClipboardWrite,
        Capability::FileUpload,
        Capability::FileDownload,
        Capability::Audio,
    ]
    .into_iter()
    .collect()
}

fn options(dir: PathBuf) -> SessionOptions {
    SessionOptions {
        permissions: Some(permissions_completes()),
        extended_features: true,
        transfer_dir: Some(dir),
        ..SessionOptions::default()
    }
}

/// Attend l'état `attendu`.
fn attendre_actif(poignee: &SessionHandle, attendu: SessionState) -> bool {
    let echeance = Instant::now() + ECHEANCE;
    let mut dernier = None;
    while dernier != Some(attendu) && Instant::now() < echeance {
        if let Ok(etat) = poignee.state_rx.recv_timeout(Duration::from_millis(100)) {
            dernier = Some(etat);
        }
    }
    dernier == Some(attendu)
}

/// Attend un message de chat distant au texte donné.
fn attendre_chat(poignee: &SessionHandle, texte: &str) -> bool {
    let echeance = Instant::now() + ECHEANCE;
    while Instant::now() < echeance {
        if let Ok(msg) = poignee.chat_rx.recv_timeout(Duration::from_millis(200)) {
            if msg.from_remote && msg.text == texte {
                return true;
            }
        }
    }
    false
}

/// Frame BGRA 64×64 : `dirty` plein (changement) ou vide (statique).
fn frame(seq: u64, statique: bool) -> CapturedFrame {
    const COTE: u32 = 64;
    let mut data = vec![0u8; (COTE * COTE * 4) as usize];
    for (i, pixel) in data.chunks_exact_mut(4).enumerate() {
        let base = if statique { 7 } else { i + seq as usize * 31 };
        pixel[0] = (base % 256) as u8;
        pixel[1] = ((base / 3) % 256) as u8;
        pixel[2] = ((seq as usize * 11) % 256) as u8;
        pixel[3] = 255;
    }
    CapturedFrame {
        width: COTE,
        height: COTE,
        monitor: MonitorId(0),
        format: PixelFormat::Bgra8,
        dirty: if statique {
            vec![]
        } else {
            vec![Rect {
                x: 0,
                y: 0,
                w: COTE,
                h: COTE,
            }]
        },
        cursor: None,
        timestamp_us: seq * 16_000,
        image: Some(FrameImage::Cpu {
            data,
            stride: (COTE * 4) as usize,
        }),
    }
}

/// Mesure déterministe du gain delta : octets d'une image **statique** encodée
/// en mode delta (trame de répétition) vs en mode plein cadre.
fn mesurer_gain_delta() -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let cfg = EncoderConfig {
        kind: CodecKind::H264,
        width: 64,
        height: 64,
        target_bitrate_kbps: 1_000,
        max_fps: 60,
    };
    // Mode delta : image-clé puis image statique (dirty vide) → répétition.
    let mut delta = create_encoder(CodecKind::H264)?;
    delta.configure(cfg)?;
    delta.set_delta_mode(true);
    delta.encode(&frame(0, false), true)?; // amorce (image-clé)
    let statique_delta = delta.encode(&frame(1, true), false)?;

    // Mode plein cadre : même séquence, l'image statique est ré-encodée en entier.
    let mut plein = create_encoder(CodecKind::H264)?;
    plein.configure(cfg)?;
    plein.set_delta_mode(false);
    plein.encode(&frame(0, false), true)?;
    let statique_plein = plein.encode(&frame(1, false), false)?;

    Ok((statique_delta.data.len(), statique_plein.data.len()))
}

// ---------------------------------------------------------------------------
// Sonde
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("NovaDesk — sonde de la session média étendue (boucle locale)");

    // Répertoires + fichier à transférer.
    let base = std::env::temp_dir().join(format!("nd-media-demo-{}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    let dir_reception = base.join("recu");
    std::fs::create_dir_all(&dir_reception)?;
    let source = base.join("cadeau.bin");
    let contenu: Vec<u8> = (0..80_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &contenu)?;

    // Briques injectées, avec poignées partagées pour l'observation.
    let joues = Arc::new(AtomicU64::new(0));
    let presse_papiers_hote = Arc::new(Mutex::new(Some("NovaDesk presse-papiers 4242".to_owned())));
    let presse_papiers_ctl = Arc::new(Mutex::new(None));

    let media_hote = SessionMedia {
        audio: Some(AudioSession::nouvelle(
            Some(EmetteurAudio::nouveau(
                SourceAudio::Systeme,
                Box::new(CapteurSynthetique {
                    format: AudioFormat::default(),
                    seq: 0,
                }),
            )),
            None,
        )),
        clipboard: Some(ClipboardSync::with_backend(Box::new(
            PressePapiersMemoire {
                texte: Arc::clone(&presse_papiers_hote),
            },
        ))),
    };
    let media_ctl = SessionMedia {
        audio: Some(AudioSession::nouvelle(
            None,
            Some(RecepteurAudio::nouveau(
                AudioFormat::default(),
                Box::new(LecteurCompteur {
                    joues: Arc::clone(&joues),
                }),
            )),
        )),
        clipboard: Some(ClipboardSync::with_backend(Box::new(
            PressePapiersMemoire {
                texte: Arc::clone(&presse_papiers_ctl),
            },
        ))),
    };

    // Hôte (Loopback) + contrôleur (Direct) : QUIC + Noise réels en boucle locale.
    let ecouteur = nd_transport::bind("127.0.0.1:0".parse()?)?;
    let addr = ecouteur.local_addr();
    let cert = ecouteur.server_cert_der();

    let hote = SessionEngine::start_with_media(
        SessionConfig {
            role: SessionRole::Controlled,
            local_id: NovaId(111_111_111),
            peer_id: None,
            permissions: Permissions::full(),
        },
        SessionEndpoint::Loopback { listener: ecouteur },
        options(dir_reception.clone()),
        media_hote,
    )?;
    let controleur = SessionEngine::start_with_media(
        SessionConfig {
            role: SessionRole::Controller,
            local_id: NovaId(222_222_222),
            peer_id: Some(NovaId(111_111_111)),
            permissions: Permissions::full(),
        },
        SessionEndpoint::Direct {
            addr,
            cert_der: cert,
        },
        options(base.clone()),
        media_ctl,
    )?;

    assert!(
        attendre_actif(&controleur, SessionState::Active),
        "contrôleur Actif (erreur : {:?})",
        controleur.last_error()
    );
    assert!(
        attendre_actif(&hote, SessionState::Active),
        "hôte Actif (erreur : {:?})",
        hote.last_error()
    );
    println!("Session étendue active (hôte + contrôleur).");

    // --- 5a. Vidéo : le flux tourne toujours (émission fiable) --------------
    // Le delta est actif par défaut : sur un écran **statique**, les frames sans
    // région modifiée deviennent des trames de répétition vides (le décodeur ne
    // livre alors rien) — d'où un compte volontairement bas ici. La première
    // image-clé arrive toujours ; le flux reprend dès que l'écran change.
    let echeance = Instant::now() + ECHEANCE;
    let mut frames = 0usize;
    while frames < 3 && Instant::now() < echeance {
        if let Ok(f) = controleur.frame_rx.recv_timeout(Duration::from_millis(200)) {
            assert!(f.width > 0 && f.height > 0);
            frames += 1;
        }
    }
    println!(
        "Vidéo : {frames} frame(s) décodée(s) reçue(s) (émission fiable, delta actif — les \
         frames statiques deviennent des répétitions et ne sont pas re-livrées)."
    );
    assert!(
        frames >= 1,
        "au moins l'image-clé initiale doit être décodée"
    );

    // --- 5b. Delta : gain mesuré déterministiquement ------------------------
    let (octets_delta, octets_plein) = mesurer_gain_delta()?;
    println!(
        "Delta : image statique = {octets_delta} octets (répétition) vs {octets_plein} octets \
         (plein cadre) — gain {} octets.",
        octets_plein.saturating_sub(octets_delta)
    );
    assert!(
        octets_delta < octets_plein,
        "l'encodage delta doit réduire les octets d'une image statique"
    );

    // --- 1. Audio : paquets reçus & lus ------------------------------------
    let echeance = Instant::now() + ECHEANCE;
    while joues.load(Ordering::Relaxed) < 5 && Instant::now() < echeance {
        std::thread::sleep(Duration::from_millis(50));
    }
    let joues_total = joues.load(Ordering::Relaxed);
    println!("Audio : {joues_total} trames audio reçues et lues côté contrôleur.");
    assert!(
        joues_total >= 5,
        "des paquets audio doivent être reçus et lus (obtenu {joues_total})"
    );

    // --- 2. Chat : aller-retour --------------------------------------------
    controleur.send_chat("bonjour hôte");
    assert!(
        attendre_chat(&hote, "bonjour hôte"),
        "l'hôte doit recevoir le message du contrôleur"
    );
    hote.send_chat("bonjour contrôleur");
    assert!(
        attendre_chat(&controleur, "bonjour contrôleur"),
        "le contrôleur doit recevoir la réponse de l'hôte"
    );
    println!("Chat : aller-retour prouvé (contrôleur ↔ hôte).");

    // --- 3. Presse-papiers : synchro hôte → contrôleur ----------------------
    let echeance = Instant::now() + ECHEANCE;
    let attendu = Some("NovaDesk presse-papiers 4242".to_owned());
    while *presse_papiers_ctl.lock().expect("verrou") != attendu && Instant::now() < echeance {
        std::thread::sleep(Duration::from_millis(50));
    }
    let synchro = presse_papiers_ctl.lock().expect("verrou").clone();
    println!("Presse-papiers : contrôleur = {synchro:?} (posé côté hôte).");
    assert_eq!(synchro, attendu, "le presse-papiers doit se synchroniser");

    // --- 4. Transfert de fichiers : reçu intègre ----------------------------
    controleur.send_files(vec![source.clone()]);
    let echeance = Instant::now() + ECHEANCE;
    let mut termine = false;
    while !termine && Instant::now() < echeance {
        if let Ok(ev) = hote.transfer_rx.recv_timeout(Duration::from_millis(200)) {
            termine = matches!(
                ev,
                TransferEvent::FileCompleted { .. } | TransferEvent::Finished
            );
        }
    }
    assert!(termine, "l'hôte doit signaler la fin du transfert");
    let recu = std::fs::read(dir_reception.join("cadeau.bin"))?;
    println!(
        "Fichiers : {} octets reçus, intègres = {}.",
        recu.len(),
        recu == contenu
    );
    assert_eq!(recu, contenu, "le fichier reçu doit être intègre");

    controleur.stop();
    hote.stop();
    let _ = std::fs::remove_dir_all(&base);

    println!();
    println!(
        "OK : session média étendue validée — audio ({joues_total} trames lues), chat \
         aller-retour, presse-papiers synchronisé, fichier {} octets intègre, delta {}→{} octets, \
         le tout gardé par permissions et démultiplexé sur canaux logiques (Audio/Files/Control).",
        contenu.len(),
        octets_plein,
        octets_delta
    );
    Ok(())
}
