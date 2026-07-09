//! Tests d'intégration de l'API de session live (`nd_ffi::api`) : cycle de vie,
//! frames vidéo et statistiques **en loopback** via le moteur réel
//! (capture → H.264 → QUIC chiffré Noise → décodage), sur le modèle de la sonde
//! du lot 01 (`nd-core/examples/session_engine_demo.rs`).

use std::time::{Duration, Instant};

use nd_ffi::{
    approve_incoming, collect_video_frames, new_session_config, send_input, session_last_error,
    session_listen_info, session_stats, start_session, start_session_with_options,
    start_unattended_host, stop_session, stop_unattended_host, unattended_stats,
    wait_session_state, InputEventDto, PermissionsDto, SessionEndpointDto, SessionOptionsDto,
    SessionRoleDto, SessionStateDto, SessionStatsDto, VideoFrameDto,
};

// ---------------------------------------------------------------------------
// Conversions des nouveaux DTO
// ---------------------------------------------------------------------------

#[test]
fn conversion_frame_decodee_en_dto() {
    let frame = nd_codec::DecodedFrame {
        width: 2,
        height: 3,
        rgba: vec![7u8; 2 * 3 * 4],
    };
    let dto = VideoFrameDto::from(frame);
    assert_eq!((dto.width, dto.height), (2, 3));
    assert_eq!(dto.rgba.len(), 2 * 3 * 4);
    assert!(dto.rgba.iter().all(|&octet| octet == 7));
}

#[test]
fn conversion_stats_moteur_en_dto() {
    // `..Default::default()` : les champs non exercés ici (dont ceux que le
    // moteur gagne au fil des lots, ex. `hotkeys_applied`) restent à zéro sans
    // casser ce littéral à chaque enrichissement de `SessionStats`.
    let stats = nd_core::SessionStats {
        fps: 12.5_f32,
        rtt_us: 850,
        bytes_in: 1_024,
        bytes_out: 2_048,
        frames_decoded: 42,
        inputs_applied: 6,
        ..Default::default()
    };
    let dto = SessionStatsDto::from(stats);
    // Le cœur mesure le fps en f32 ; le DTO l'expose en f64, sans perte.
    assert!(
        (dto.fps - 12.5_f64).abs() < f64::EPSILON,
        "fps = {}",
        dto.fps
    );
    assert_eq!(dto.rtt_us, 850);
    assert_eq!(dto.bytes_in, 1_024);
    assert_eq!(dto.bytes_out, 2_048);
    assert_eq!(dto.frames, 42);
}

// ---------------------------------------------------------------------------
// Erreurs lisibles (sans démarrer de moteur)
// ---------------------------------------------------------------------------

#[test]
fn start_session_refuse_une_adresse_directe_illisible() {
    let config = new_session_config(
        SessionRoleDto::Controller,
        111_111_111,
        Some(222_222_222),
        PermissionsDto::full(),
    )
    .expect("configuration valide");
    let erreur = start_session(
        config,
        SessionEndpointDto::Direct {
            addr: "pas-une-adresse".to_owned(),
            cert_der: vec![1, 2, 3],
        },
    )
    .unwrap_err();
    assert!(erreur.contains("invalide"), "message peu utile : {erreur}");
}

#[test]
fn session_inconnue_erreurs_lisibles() {
    // Identifiant jamais attribué (le compteur démarre à 1 et n'atteint pas ce nombre).
    let id = 424_242_424;
    for erreur in [
        session_listen_info(id).unwrap_err(),
        session_stats(id).unwrap_err(),
        session_last_error(id).unwrap_err(),
        send_input(id, InputEventDto::Scroll { dx: 0.0, dy: 1.0 }).unwrap_err(),
        wait_session_state(id, 10).unwrap_err(),
        collect_video_frames(id, 1, 10).unwrap_err(),
        stop_session(id).unwrap_err(),
    ] {
        assert!(erreur.contains("inconnue"), "message peu utile : {erreur}");
    }
}

// ---------------------------------------------------------------------------
// Statistiques enrichies et endpoint par rendez-vous (lot §2)
// ---------------------------------------------------------------------------

#[test]
fn conversion_stats_champs_enrichis() {
    // Voir `conversion_stats_moteur_en_dto` : `..Default::default()` absorbe
    // les champs ajoutés à `SessionStats` par les lots ultérieurs du moteur.
    let stats = nd_core::SessionStats {
        fps: 30.0_f32,
        rtt_us: 1_200,
        bytes_in: 10,
        bytes_out: 20,
        frames_decoded: 100,
        inputs_applied: 5,
        inputs_denied: 3,
        target_bitrate_kbps: 4_500,
        abr_level: 2,
        frames_recorded: 90,
        reconnects: 1,
        ..Default::default()
    };
    let dto = SessionStatsDto::from(stats);
    assert_eq!(dto.inputs_denied, 3);
    assert_eq!(dto.target_bitrate_kbps, 4_500);
    assert_eq!(dto.abr_level, 2);
    assert_eq!(dto.frames_recorded, 90);
    assert_eq!(dto.reconnects, 1);
    // Le backend d'encodage n'est pas porté par SessionStats : renseigné à part
    // par la façade (depuis la poignée), il vaut None dans la conversion pure.
    assert_eq!(dto.encoder_backend, None);
}

#[test]
fn start_session_refuse_un_rendezvous_illisible() {
    let config = new_session_config(
        SessionRoleDto::Controller,
        111_111_111,
        Some(222_222_222),
        PermissionsDto::full(),
    )
    .expect("configuration valide");
    let erreur = start_session(
        config,
        SessionEndpointDto::ByRendezvous {
            server: "pas-une-adresse".to_owned(),
            stun_servers: vec![],
            relay: None,
        },
    )
    .unwrap_err();
    assert!(erreur.contains("invalide"), "message peu utile : {erreur}");
    assert!(
        erreur.contains("rendez-vous"),
        "message peu utile : {erreur}"
    );
}

#[test]
fn start_session_refuse_un_serveur_stun_illisible() {
    let config = new_session_config(
        SessionRoleDto::Controller,
        111_111_111,
        Some(222_222_222),
        PermissionsDto::full(),
    )
    .expect("configuration valide");
    // Serveur de rendez-vous valide mais STUN illisible : l'erreur situe le STUN.
    let erreur = start_session(
        config,
        SessionEndpointDto::ByRendezvous {
            server: "127.0.0.1:9000".to_owned(),
            stun_servers: vec!["pas-stun".to_owned()],
            relay: None,
        },
    )
    .unwrap_err();
    assert!(erreur.contains("STUN"), "message peu utile : {erreur}");
}

#[test]
fn start_session_with_options_refuse_une_adresse_illisible() {
    let config = new_session_config(
        SessionRoleDto::Controlled,
        111_111_111,
        None,
        PermissionsDto::full(),
    )
    .expect("configuration valide");
    let options = SessionOptionsDto {
        permissions: PermissionsDto::full(),
        recording_path: None,
        delta_mode: false,
        extended_features: false,
        transfer_dir: None,
        transport_reconnect: false,
    };
    let erreur = start_session_with_options(
        config,
        SessionEndpointDto::Direct {
            addr: "pas-une-adresse".to_owned(),
            cert_der: vec![1, 2, 3],
        },
        options,
    )
    .unwrap_err();
    assert!(erreur.contains("invalide"), "message peu utile : {erreur}");
}

// ---------------------------------------------------------------------------
// Hôte « accès non surveillé » : erreurs lisibles de la façade
// ---------------------------------------------------------------------------

#[test]
fn hote_non_surveille_erreurs_sur_identifiant_inconnu() {
    let host_id = 987_654_321;
    for erreur in [
        unattended_stats(host_id).unwrap_err(),
        approve_incoming(host_id, 42, true).unwrap_err(),
        stop_unattended_host(host_id).unwrap_err(),
    ] {
        assert!(erreur.contains("inconnu"), "message peu utile : {erreur}");
    }
}

#[test]
fn start_unattended_host_refuse_un_rendezvous_illisible() {
    let erreur = start_unattended_host(
        424_242_424,
        "pas-une-adresse".to_owned(),
        vec![],
        PermissionsDto::view_only(),
    )
    .unwrap_err();
    assert!(erreur.contains("invalide"), "message peu utile : {erreur}");
}

// ---------------------------------------------------------------------------
// Session loopback complète (hôte + viewer via l'API de la façade)
// ---------------------------------------------------------------------------

/// Délai maximal pour atteindre `Active` (handshake QUIC + Noise en loopback).
const DELAI_ACTIF: Duration = Duration::from_secs(20);
/// Frames décodées exigées du flux réel.
const FRAMES_MIN: u32 = 3;
/// Délai maximal de collecte (encodage logiciel lent en profil debug).
const DELAI_FRAMES_MS: u64 = 60_000;

/// Message d'échec enrichi des dernières erreurs des deux moteurs.
fn diagnostic(message: &str, id_viewer: u64, id_hote: u64) -> String {
    format!(
        "{message} (erreur viewer : {:?} / erreur hôte : {:?})",
        session_last_error(id_viewer),
        session_last_error(id_hote)
    )
}

/// Attend que la session `id` atteigne `Active` via les lectures synchrones d'états.
fn attendre_actif(id: u64, id_pair: u64) {
    let echeance = Instant::now() + DELAI_ACTIF;
    while Instant::now() < echeance {
        match wait_session_state(id, 250).expect("session connue") {
            Some(SessionStateDto::Active) => return,
            Some(SessionStateDto::Closed) => {
                panic!("{}", diagnostic("session close avant Active", id, id_pair));
            }
            _ => {}
        }
    }
    panic!("{}", diagnostic("Active jamais atteint", id, id_pair));
}

#[test]
fn session_loopback_frames_stats_et_arret() {
    // 1. Hôte (contrôlé) : écoute loopback ; l'adresse et le certificat sont publiés
    //    par la façade elle-même.
    let config_hote = new_session_config(
        SessionRoleDto::Controlled,
        111_111_111,
        None,
        PermissionsDto::full(),
    )
    .expect("configuration hôte valide");
    let id_hote = start_session(config_hote, SessionEndpointDto::Loopback).expect("démarrage hôte");
    let ecoute = session_listen_info(id_hote).expect("coordonnées d'écoute du loopback");
    assert!(
        ecoute.addr.starts_with("127.0.0.1:"),
        "adresse inattendue : {}",
        ecoute.addr
    );
    assert!(!ecoute.cert_der.is_empty(), "certificat DER vide");

    // Une session directe n'écoute pas : l'info d'écoute est réservée au loopback.
    // (Vérifié plus bas sur le viewer.)

    // 2. Viewer (contrôleur) : connexion directe à l'écouteur de l'hôte.
    let config_viewer = new_session_config(
        SessionRoleDto::Controller,
        222_222_222,
        Some(111_111_111),
        PermissionsDto::full(),
    )
    .expect("configuration viewer valide");
    let id_viewer = start_session(
        config_viewer,
        SessionEndpointDto::Direct {
            addr: ecoute.addr,
            cert_der: ecoute.cert_der,
        },
    )
    .expect("démarrage viewer");
    assert!(
        session_listen_info(id_viewer).is_err(),
        "un endpoint direct ne publie pas de coordonnées d'écoute"
    );

    // 3. Machine à états : le viewer doit atteindre Active (transitions bufferisées,
    //    lues par la voie synchrone — le StreamSink exige l'app Dart en face).
    attendre_actif(id_viewer, id_hote);

    // 4. Frames vidéo réelles : dimensions cohérentes et tampon RGBA plein.
    let frames = collect_video_frames(id_viewer, FRAMES_MIN, DELAI_FRAMES_MS)
        .expect("collecte de frames sur une session connue");
    assert_eq!(
        frames.len(),
        FRAMES_MIN as usize,
        "{}",
        diagnostic("frames décodées insuffisantes", id_viewer, id_hote)
    );
    for frame in &frames {
        assert!(
            frame.width > 0 && frame.height > 0,
            "dimensions nulles : {}×{}",
            frame.width,
            frame.height
        );
        assert_eq!(
            frame.rgba.len(),
            frame.width as usize * frame.height as usize * 4,
            "tampon RGBA incohérent pour {}×{}",
            frame.width,
            frame.height
        );
    }

    // 5. Statistiques : le compteur cumulatif de frames et les octets reçus bougent.
    let stats = session_stats(id_viewer).expect("statistiques du viewer");
    assert!(stats.frames >= u64::from(FRAMES_MIN), "stats = {stats:?}");
    assert!(stats.bytes_in > 0, "stats = {stats:?}");

    // 6. Entrées : deux mouvements relatifs qui s'annulent (comme la sonde du lot 01)
    //    partent sans erreur sur le canal chiffré.
    send_input(id_viewer, InputEventDto::MouseMoveRel { dx: 1.0, dy: 0.0 })
        .expect("envoi d'entrée");
    send_input(id_viewer, InputEventDto::MouseMoveRel { dx: -1.0, dy: 0.0 })
        .expect("envoi d'entrée");

    // 7. Arrêt : le viewer raccroche ; l'hôte constate le départ et se clôt de
    //    lui-même (état final Closed observé par la voie synchrone).
    stop_session(id_viewer).expect("arrêt du viewer");
    let echeance = Instant::now() + Duration::from_secs(10);
    let mut hote_clos = false;
    while !hote_clos && Instant::now() < echeance {
        match wait_session_state(id_hote, 250).expect("session hôte connue") {
            Some(SessionStateDto::Closed) => hote_clos = true,
            Some(_) => {}
            None => {}
        }
    }
    assert!(
        hote_clos,
        "l'hôte ne s'est pas clos après le départ du viewer"
    );
    stop_session(id_hote).expect("arrêt de l'hôte");

    // 8. Les identifiants sont retirés de la table : tout ré-emploi échoue proprement.
    assert!(stop_session(id_viewer).is_err());
    assert!(session_stats(id_hote).is_err());
}
