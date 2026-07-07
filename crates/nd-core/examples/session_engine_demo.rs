//! Sonde du moteur de session : **hôte + viewer en loopback via `SessionEngine`**.
//!
//! Monte les deux rôles avec le vrai pipeline (capture DXGI → H.264 → QUIC chiffré
//! Noise → décodage), laisse tourner ~2 s, puis vérifie que :
//! - les deux machines à états atteignent `Active` (transitions poussées par le moteur) ;
//! - au moins `FRAMES_MIN` (≥ 10) `DecodedFrame` arrivent dans `frame_rx` ;
//! - `stats().fps > 0` côté viewer (fenêtre glissante) ;
//! - les `InputEvent` postés dans `input_tx` sont reçus **et appliqués** côté hôte
//!   (compteur `inputs_applied` du moteur) — mouvements relatifs qui s'annulent ;
//! - l'hôte se clôt (`Closed`) de lui-même quand le viewer raccroche.
//!
//! Lancer : `cargo run --example session_engine_demo -p nd-core`
//! (en debug, l'encodage logiciel est lent : la sonde étend l'observation jusqu'à
//! obtenir ses 10 frames, plafonnée à `DUREE_MAX`).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionRole, SessionState,
    SessionStats,
};
use nd_features::Permissions;
use nd_proto::{InputEvent, NovaId};
use nd_transport::bind;

/// Nombre minimal de frames décodées attendues dans `frame_rx`.
const FRAMES_MIN: usize = 10;
/// Durée d'observation nominale du flux.
const DUREE_FLUX: Duration = Duration::from_secs(2);
/// Plafond d'observation (tolérance pour l'encodage logiciel en profil debug).
const DUREE_MAX: Duration = Duration::from_secs(30);

/// Draine `state_rx` jusqu'à `Active` (échec sur `Closed` ou délai dépassé).
fn attendre_actif(nom: &str, state_rx: &Receiver<SessionState>) -> Result<(), String> {
    let echeance = Instant::now() + Duration::from_secs(10);
    while Instant::now() < echeance {
        match state_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(etat) => {
                println!("{nom} : état → {etat:?}");
                match etat {
                    SessionState::Active => return Ok(()),
                    SessionState::Closed => {
                        return Err(format!("{nom} : session close avant Active"));
                    }
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!("{nom} : canal d'états fermé"));
            }
        }
    }
    Err(format!("{nom} : Active jamais atteint"))
}

/// Message d'erreur enrichi des dernières erreurs des deux moteurs.
fn echec(message: &str, viewer: &SessionHandle, hote: &SessionHandle) -> String {
    format!(
        "{message} (erreur viewer : {:?} / erreur hôte : {:?})",
        viewer.last_error(),
        hote.last_error()
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("NovaDesk — sonde SessionEngine : hôte + viewer en loopback");

    // 1. Écouteur QUIC : l'hôte accepte dessus, le viewer se connecte en direct.
    let ecouteur = bind("127.0.0.1:0".parse()?)?;
    let addr = ecouteur.local_addr();
    let cert = ecouteur.server_cert_der();
    let id_hote = NovaId(111_111_111);
    println!("Écouteur QUIC de l'hôte : {addr}");

    // 2. Démarrage des deux moteurs (2 threads par rôle : pilote + auxiliaire).
    let hote = SessionEngine::start(
        SessionConfig {
            role: SessionRole::Controlled,
            local_id: id_hote,
            peer_id: None,
            permissions: Permissions::default(),
        },
        SessionEndpoint::Loopback { listener: ecouteur },
    )?;
    let viewer = SessionEngine::start(
        SessionConfig {
            role: SessionRole::Controller,
            local_id: NovaId(222_222_222),
            peer_id: Some(id_hote),
            permissions: Permissions::default(),
        },
        SessionEndpoint::Direct {
            addr,
            cert_der: cert,
        },
    )?;

    // 3. Machine à états : les deux côtés doivent atteindre Active.
    attendre_actif("viewer", &viewer.state_rx).map_err(|e| echec(&e, &viewer, &hote))?;
    attendre_actif("hôte  ", &hote.state_rx).map_err(|e| echec(&e, &viewer, &hote))?;

    // 4. ~2 s de flux : compter les frames arrivées dans frame_rx ; envoyer des
    // entrées à mi-course (mouvements relatifs qui s'annulent, comme control_loop).
    let script = [
        InputEvent::MouseMoveRel { dx: 20.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: 20.0 },
        InputEvent::MouseMoveRel { dx: -20.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: -20.0 },
        InputEvent::MouseMoveRel { dx: 12.0, dy: 12.0 },
        InputEvent::MouseMoveRel {
            dx: -12.0,
            dy: -12.0,
        },
    ];
    let debut = Instant::now();
    let mut frames_recues = 0usize;
    let mut dimensions = None;
    let mut envoyees = 0usize;
    let mut stats_apres_frame = SessionStats::default();
    while (frames_recues < FRAMES_MIN || debut.elapsed() < DUREE_FLUX)
        && debut.elapsed() < DUREE_MAX
    {
        match viewer.frame_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(frame) => {
                frames_recues += 1;
                dimensions = Some((frame.width, frame.height));
                // Instantané pris juste après une frame : le fps de la fenêtre
                // glissante reflète le flux réellement en cours.
                stats_apres_frame = viewer.stats();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if envoyees < script.len() && debut.elapsed() >= Duration::from_millis(600) {
            viewer.input_tx.send(script[envoyees])?;
            envoyees += 1;
        }
    }
    let duree_observee = debut.elapsed();

    // 5. Les entrées doivent avoir été appliquées côté hôte (compteur du moteur).
    let echeance = Instant::now() + Duration::from_secs(3);
    let mut appliquees = hote.stats().inputs_applied;
    while appliquees < script.len() as u64 && Instant::now() < echeance {
        std::thread::sleep(Duration::from_millis(25));
        appliquees = hote.stats().inputs_applied;
    }

    let stats_hote = hote.stats();
    println!();
    println!("— Résumé de la sonde ({duree_observee:.1?} de flux) —");
    println!(
        "Viewer : {frames_recues} frames décodées reçues dans frame_rx, dimensions {dimensions:?}"
    );
    println!(
        "Viewer : fps {:.1}, rtt {} µs, {} octets reçus, {} octets émis, {} frames livrées",
        stats_apres_frame.fps,
        stats_apres_frame.rtt_us,
        stats_apres_frame.bytes_in,
        stats_apres_frame.bytes_out,
        stats_apres_frame.frames_decoded
    );
    println!(
        "Hôte   : {appliquees}/{} entrées appliquées, {} octets émis, {} octets reçus",
        script.len(),
        stats_hote.bytes_out,
        stats_hote.bytes_in
    );

    // 6. Verdict.
    if frames_recues < FRAMES_MIN {
        return Err(echec(
            &format!("frames insuffisantes : {frames_recues} < {FRAMES_MIN}"),
            &viewer,
            &hote,
        )
        .into());
    }
    if stats_apres_frame.fps <= 0.0 {
        return Err(echec("fps nul côté viewer", &viewer, &hote).into());
    }
    if appliquees != script.len() as u64 {
        return Err(echec(
            &format!("entrées appliquées : {appliquees}/{}", script.len()),
            &viewer,
            &hote,
        )
        .into());
    }

    // 7. Arrêt : le viewer raccroche ; l'hôte doit constater le départ et se clore.
    viewer.stop();
    let echeance = Instant::now() + Duration::from_secs(5);
    let mut hote_clos = false;
    while !hote_clos && Instant::now() < echeance {
        match hote.state_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(SessionState::Closed) => hote_clos = true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    println!("Hôte   : Closed observé après le départ du viewer : {hote_clos}");
    hote.stop();
    if !hote_clos {
        return Err("l'hôte ne s'est pas clos après le départ du viewer".into());
    }

    println!(
        "OK : moteur de session validé — {frames_recues} frames reçues (≥ {FRAMES_MIN}), \
         fps {:.1} (> 0), entrées {appliquees}/{} appliquées, hôte clos proprement.",
        stats_apres_frame.fps,
        script.len()
    );
    Ok(())
}
