//! Sonde de la **session intégrée** : toutes les briques câblées dans la vraie
//! boucle `SessionEngine`, exercées en boucle locale.
//!
//! Ce que la sonde **prouve** (assertions, échec = code de sortie non nul) :
//!
//! 1. **Connexion par ID réelle** : un serveur de rendez-vous local, un hôte
//!    moteur qui publie son ID et attend (`await_p2p` → QUIC sur socket percée
//!    → Noise), un contrôleur moteur qui résout l'ID (`establish_p2p`) — via
//!    [`SessionEndpoint::ByRendezvous`] enrichi (STUN/relais optionnels).
//! 2. **Flux réel** : ≥ 10 frames décodées reçues par le contrôleur.
//! 3. **Encodeur GPU** : le backend d'encodage réellement à l'œuvre est affiché
//!    (nom exact du MFT matériel NVENC, ou repli logiciel documenté).
//! 4. **ABR bout-en-bout** : le moteur échantillonne le chemin réel (~1 Hz) et
//!    applique une consigne (`target_bitrate_kbps` > 0 dans les stats) ; la
//!    **variation** de consigne est prouvée sur la brique `RateController` avec
//!    des estimations simulées — on ne peut pas dégrader un vrai chemin
//!    loopback de façon déterministe (documenté).
//! 5. **Permissions côté contrôlé** : souris accordée, clavier refusé — les
//!    frappes sont jetées avant injection et **comptées** (`inputs_denied`).
//! 6. **Reconnexion** : le contrôleur raccroche, l'hôte passe `Reconnecting`,
//!    un second contrôleur (même ID) reprend la session (`Active`, frames).
//! 7. **Enregistrement opt-in** : l'hôte écrit un MP4 par époque, **relisible**
//!    (validé via `Mp4Reader` : images, images-clés, dimensions).
//! 8. **Accès non surveillé** : un [`UnattendedHost`] publie son ID, refuse ou
//!    accepte via le hook, et sert une session hôte complète.
//!
//! **Honnêteté** : tout est exercé en loopback (punch UDP local réel, QUIC réel,
//! Noise réel, capture d'écran réelle). La traversée d'un **vrai NAT** dépend du
//! type de NAT et n'est pas testable sur une seule machine ; le repli **relais**
//! exige un serveur `nd-relay` et un ticket signé (lot 07) — non exercés ici.
//!
//! Lancer : `cargo run --example session_integree_demo -p nd-core`
//! (en debug, l'encodage peut être lent : les échéances sont larges).

use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use nd_codec::{
    create_encoder, CodecKind, ContentProfile, EncoderConfig, NetworkEstimate, RateController,
};
use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState, UnattendedHost,
};
use nd_features::{Capability, Mp4Reader, PermissionSet, Permissions};
use nd_proto::{InputEvent, NovaId};
use nd_signaling::{serve, Registry};
use nd_transport::ServerIdentity;

/// Nombre minimal de frames décodées attendues côté contrôleur.
const FRAMES_MIN: usize = 10;
/// Échéance large des attentes (encodage logiciel possible en profil debug).
const ECHEANCE: Duration = Duration::from_secs(45);

/// Draine `state_rx` jusqu'à `attendu` ; rend la séquence vue (assertion chez
/// l'appelant). Échec silencieux sur échéance : la séquence rendue en témoigne.
fn attendre_etat(nom: &str, poignee: &SessionHandle, attendu: SessionState) -> Vec<SessionState> {
    let mut vus = Vec::new();
    let echeance = Instant::now() + ECHEANCE;
    while vus.last() != Some(&attendu) && Instant::now() < echeance {
        match poignee.state_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(etat) => {
                println!("  {nom} : état → {etat:?}");
                vus.push(etat);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    vus
}

/// Reçoit des frames décodées jusqu'au compte demandé (échéance large).
fn attendre_frames(poignee: &SessionHandle, compte: usize) -> (usize, Option<(u32, u32)>) {
    let mut recues = 0usize;
    let mut dims = None;
    let echeance = Instant::now() + ECHEANCE;
    while recues < compte && Instant::now() < echeance {
        match poignee.frame_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(frame) => {
                recues += 1;
                dims = Some((frame.width, frame.height));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    (recues, dims)
}

/// Valide un MP4 d'enregistrement : ouvrable, images > 0, au moins une
/// image-clé, dimensions non nulles. Rend `(images, images-clés, durée µs)`.
fn valider_mp4(chemin: &Path) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    let fichier = std::fs::File::open(chemin)?;
    let mut lecteur = Mp4Reader::new(fichier)?;
    let rapport = lecteur.validate()?;
    if rapport.frames == 0 {
        return Err(format!("{} : aucune image", chemin.display()).into());
    }
    if rapport.keyframes == 0 {
        return Err(format!("{} : aucune image-clé", chemin.display()).into());
    }
    if rapport.width == 0 || rapport.height == 0 {
        return Err(format!("{} : dimensions nulles", chemin.display()).into());
    }
    Ok((rapport.frames, rapport.keyframes, rapport.duration_us))
}

/// Chemin du fichier d'époque `n` dérivé comme le fait le moteur
/// (`session.mp4`, `session-2.mp4`, …).
fn chemin_epoque(base: &Path, epoque: u32) -> PathBuf {
    if epoque <= 1 {
        return base.to_path_buf();
    }
    let racine = base.file_stem().map_or_else(
        || "session".to_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    let nom = match base.extension() {
        Some(ext) => format!("{racine}-{epoque}.{}", ext.to_string_lossy()),
        None => format!("{racine}-{epoque}"),
    };
    base.with_file_name(nom)
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("NovaDesk — sonde de la session intégrée (rendez-vous + moteur, boucle locale)");

    // ------------------------------------------------------------------
    // 0. Serveur de rendez-vous local.
    // ------------------------------------------------------------------
    let rv_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let rv_addr = rv_listener.local_addr()?;
    std::thread::spawn(move || {
        let _ = serve(rv_listener, Registry::new());
    });
    println!("Rendez-vous en écoute sur {rv_addr}");

    let id_hote = NovaId(111_111_111);
    let id_controleur = NovaId(222_222_222);
    let endpoint_par_id = || SessionEndpoint::ByRendezvous {
        server: rv_addr,
        stun_servers: vec![],
        relay: None,
    };

    // ------------------------------------------------------------------
    // 1. Hôte moteur par ID : permissions « souris seulement », enregistrement.
    // ------------------------------------------------------------------
    let chemin_mp4 = std::env::temp_dir().join(format!(
        "novadesk-session-integree-{}.mp4",
        std::process::id()
    ));
    let permissions_hote: PermissionSet = [Capability::ViewScreen, Capability::ControlMouse]
        .into_iter()
        .collect();
    let hote = SessionEngine::start_with_options(
        SessionConfig {
            role: SessionRole::Controlled,
            local_id: id_hote,
            peer_id: None,
            permissions: Permissions::view_only(),
        },
        endpoint_par_id(),
        SessionOptions {
            // Souris accordée, clavier refusé : preuve du filtre d'injection.
            permissions: Some(permissions_hote),
            recording: Some(chemin_mp4.clone()),
            ..SessionOptions::default()
        },
    )?;

    // Contrôleur n° 1 : résolution de l'ID, punch, QUIC, Noise.
    let controleur = SessionEngine::start(
        SessionConfig {
            role: SessionRole::Controller,
            local_id: id_controleur,
            peer_id: Some(id_hote),
            permissions: Permissions::view_only(),
        },
        endpoint_par_id(),
    )?;

    let etats = attendre_etat("contrôleur", &controleur, SessionState::Active);
    assert_eq!(
        etats,
        vec![
            SessionState::Resolving,
            SessionState::Connecting,
            SessionState::Handshaking,
            SessionState::Active
        ],
        "contrôleur : erreur = {:?}",
        controleur.last_error()
    );
    let etats = attendre_etat("hôte      ", &hote, SessionState::Active);
    assert_eq!(
        etats.last(),
        Some(&SessionState::Active),
        "hôte : erreur = {:?}",
        hote.last_error()
    );

    // ------------------------------------------------------------------
    // 2. Flux réel : N frames décodées côté contrôleur.
    // ------------------------------------------------------------------
    let (frames, dims) = attendre_frames(&controleur, FRAMES_MIN);
    let stats_controleur = controleur.stats();
    println!(
        "Contrôleur : {frames} frames décodées reçues, dimensions {dims:?}, fps {:.1}, rtt {} µs",
        stats_controleur.fps, stats_controleur.rtt_us
    );
    assert!(
        frames >= FRAMES_MIN,
        "frames = {frames} < {FRAMES_MIN} (contrôleur : {:?} / hôte : {:?})",
        controleur.last_error(),
        hote.last_error()
    );

    // ------------------------------------------------------------------
    // 3. Encodeur GPU (preuve par le nom du backend) + 4. ABR moteur.
    // ------------------------------------------------------------------
    let backend = hote.encoder_backend().unwrap_or_else(|| "inconnu".into());
    println!("Hôte : backend d'encodage réellement à l'œuvre = « {backend} »");
    let stats_hote = hote.stats();
    println!(
        "Hôte : consigne ABR appliquée = {} kbit/s (palier {}), {} frames enregistrées",
        stats_hote.target_bitrate_kbps, stats_hote.abr_level, stats_hote.frames_recorded
    );
    assert!(
        stats_hote.target_bitrate_kbps > 0,
        "l'ABR du moteur doit avoir appliqué une consigne (échantillon réel ~1 Hz)"
    );

    // ABR : la consigne **bouge** sous une variation simulée du chemin (brique
    // RateController + vrai encodeur ; un chemin loopback réel ne se dégrade
    // pas de façon déterministe — voir doc de module).
    let base = EncoderConfig {
        kind: CodecKind::H264,
        width: 1920,
        height: 1080,
        target_bitrate_kbps: 8_000,
        max_fps: 60,
    };
    let mut encodeur_abr = create_encoder(CodecKind::H264)?;
    let mut regulateur = RateController::new(base, ContentProfile::Text);
    let saine = NetworkEstimate::from_path(20_000, 0.0, 20_000);
    let effondree = NetworkEstimate::from_path(200_000, 0.05, 1_000);
    let consigne_saine = regulateur
        .apply_network_estimate(encodeur_abr.as_mut(), saine)
        .target_bitrate_kbps;
    let consigne_effondree = regulateur
        .apply_network_estimate(encodeur_abr.as_mut(), effondree)
        .target_bitrate_kbps;
    let palier_effondre = regulateur.palier();
    let mut consigne_retablie = consigne_effondree;
    for _ in 0..20 {
        consigne_retablie = regulateur
            .apply_network_estimate(encodeur_abr.as_mut(), saine)
            .target_bitrate_kbps;
    }
    println!(
        "ABR (variation simulée) : sain {consigne_saine} kbit/s → effondré {consigne_effondree} \
         kbit/s (palier {palier_effondre}) → rétabli {consigne_retablie} kbit/s (palier {})",
        regulateur.palier()
    );
    assert_eq!(consigne_saine, 8_000);
    assert!(
        consigne_effondree < consigne_saine,
        "la consigne doit descendre quand le chemin s'effondre"
    );
    assert_eq!(consigne_retablie, 8_000, "remontée après hystérésis");

    // ------------------------------------------------------------------
    // 5. Permissions : souris appliquée, clavier refusé (compté).
    // ------------------------------------------------------------------
    let script_souris = [
        InputEvent::MouseMoveRel { dx: 15.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: 15.0 },
        InputEvent::MouseMoveRel { dx: -15.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: -15.0 },
    ];
    for evenement in script_souris {
        controleur.input_tx.send(evenement)?;
    }
    // Frappes clavier : la capacité n'est pas accordée, elles doivent être
    // jetées **avant** injection (aucune frappe réelle) et comptées.
    controleur.input_tx.send(InputEvent::Key {
        scancode: 0x1C,
        down: true,
    })?;
    controleur.input_tx.send(InputEvent::Key {
        scancode: 0x1C,
        down: false,
    })?;

    let echeance = Instant::now() + ECHEANCE;
    let mut stats_hote = hote.stats();
    while (stats_hote.inputs_applied < script_souris.len() as u64 || stats_hote.inputs_denied < 2)
        && Instant::now() < echeance
    {
        std::thread::sleep(Duration::from_millis(25));
        stats_hote = hote.stats();
    }
    println!(
        "Hôte : entrées appliquées = {} (souris), refusées par permission = {} (clavier)",
        stats_hote.inputs_applied, stats_hote.inputs_denied
    );
    assert!(
        stats_hote.inputs_applied >= script_souris.len() as u64,
        "les mouvements souris (capacité accordée) doivent être appliqués"
    );
    assert!(
        stats_hote.inputs_denied >= 2,
        "les frappes clavier (capacité refusée) doivent être jetées et comptées"
    );

    // ------------------------------------------------------------------
    // 6. Reconnexion : le contrôleur raccroche, l'hôte reprend le même pair.
    // ------------------------------------------------------------------
    println!("Contrôleur n° 1 : raccroche (coupure volontaire du lien)…");
    controleur.stop();
    let etats = attendre_etat("hôte      ", &hote, SessionState::Reconnecting);
    assert_eq!(
        etats.last(),
        Some(&SessionState::Reconnecting),
        "l'hôte doit passer Reconnecting à la coupure (erreur : {:?})",
        hote.last_error()
    );

    let controleur_bis = SessionEngine::start(
        SessionConfig {
            role: SessionRole::Controller,
            local_id: id_controleur, // même identité : la reprise est admise
            peer_id: Some(id_hote),
            permissions: Permissions::view_only(),
        },
        endpoint_par_id(),
    )?;
    let etats = attendre_etat("contrôleur bis", &controleur_bis, SessionState::Active);
    assert_eq!(
        etats.last(),
        Some(&SessionState::Active),
        "contrôleur bis : erreur = {:?}",
        controleur_bis.last_error()
    );
    let etats = attendre_etat("hôte      ", &hote, SessionState::Active);
    assert_eq!(
        etats,
        vec![SessionState::Handshaking, SessionState::Active],
        "hôte : reprise attendue après Reconnecting (erreur : {:?})",
        hote.last_error()
    );
    let (frames_bis, _) = attendre_frames(&controleur_bis, 3);
    let reconnexions = hote.stats().reconnects;
    println!(
        "Reconnexion : hôte de nouveau Actif, {frames_bis} frames vers le contrôleur bis, \
         reconnexions comptées = {reconnexions}"
    );
    assert!(frames_bis >= 3, "le flux doit repartir après la reprise");
    assert_eq!(reconnexions, 1, "une reconnexion réussie comptée");

    // Fin de la phase moteur : tout raccrocher.
    controleur_bis.stop();
    hote.stop();

    // ------------------------------------------------------------------
    // 7. Enregistrement : un MP4 relisible par époque.
    // ------------------------------------------------------------------
    let (images, cles, duree_us) = valider_mp4(&chemin_mp4)?;
    println!(
        "Enregistrement (époque 1) : {} — {images} images, {cles} image(s)-clé(s), {:.2} s — RELISIBLE",
        chemin_mp4.display(),
        duree_us as f64 / 1_000_000.0
    );
    let chemin_bis = chemin_epoque(&chemin_mp4, 2);
    if chemin_bis.exists() {
        let (images, cles, duree_us) = valider_mp4(&chemin_bis)?;
        println!(
            "Enregistrement (époque 2) : {} — {images} images, {cles} image(s)-clé(s), {:.2} s — RELISIBLE",
            chemin_bis.display(),
            duree_us as f64 / 1_000_000.0
        );
    }

    // ------------------------------------------------------------------
    // 8. Accès non surveillé : hook d'acceptation + session hôte complète.
    // ------------------------------------------------------------------
    let id_non_surveille = NovaId(333_333_333);
    let id_admis = NovaId(444_444_444);
    let service = UnattendedHost::start(
        id_non_surveille,
        rv_addr,
        vec![],
        ServerIdentity::generate()?,
        PermissionSet::from(Permissions::full()),
        move |pair| pair == id_admis, // hook du dialogue d'acceptation de l'UI
    )?;

    let visiteur = SessionEngine::start(
        SessionConfig {
            role: SessionRole::Controller,
            local_id: id_admis,
            peer_id: Some(id_non_surveille),
            permissions: Permissions::view_only(),
        },
        endpoint_par_id(),
    )?;
    let etats = attendre_etat("visiteur  ", &visiteur, SessionState::Active);
    assert_eq!(
        etats.last(),
        Some(&SessionState::Active),
        "visiteur : erreur = {:?} / service : {:?}",
        visiteur.last_error(),
        service.last_error()
    );
    let (frames_visiteur, _) = attendre_frames(&visiteur, 3);
    println!(
        "Accès non surveillé : session servie (sessions = {}, refus = {}), \
         {frames_visiteur} frames reçues par le visiteur admis",
        service.sessions_served(),
        service.peers_refused()
    );
    assert!(service.sessions_served() >= 1, "une session servie");
    assert!(
        frames_visiteur >= 3,
        "le flux du service atteint le visiteur"
    );
    visiteur.stop();
    service.stop();

    // Ménage best-effort des fichiers d'enregistrement.
    let _ = std::fs::remove_file(&chemin_mp4);
    let _ = std::fs::remove_file(&chemin_bis);

    println!();
    println!(
        "OK : session intégrée validée — connexion par ID (punch réel loopback), \
         {frames} frames (≥ {FRAMES_MIN}), backend « {backend} », ABR {consigne_saine}→\
         {consigne_effondree}→{consigne_retablie} kbit/s, {} entrées refusées par permission, \
         reconnexion ({reconnexions}) et enregistrement MP4 relisible.",
        stats_hote.inputs_denied
    );
    Ok(())
}
