//! Tests d'intégration des **fonctions média étendues** de la façade UI
//! (`nd_ffi::api`, lot « session media ») : conversions plates des DTO de chat et
//! de transfert de fichiers, erreurs lisibles sur identifiant inconnu, et — quand
//! l'environnement le permet — session étendue en loopback via le moteur réel.

use std::time::{Duration, Instant};

use nd_ffi::{
    collect_video_frames, new_session_config, send_chat, send_files, session_last_error,
    session_listen_info, set_audio_enabled, start_session_with_options, stop_session,
    switch_monitor, wait_session_state, ChatMessageDto, PermissionsDto, SessionEndpointDto,
    SessionOptionsDto, SessionRoleDto, SessionStateDto, TransferEventDto,
};

// ---------------------------------------------------------------------------
// Conversions plates : chat
// ---------------------------------------------------------------------------

#[test]
fn conversion_chat_message_recu_et_echo() {
    // Message reçu du pair.
    let recu = nd_core::ChatMessage {
        from_remote: true,
        text: "bonjour éàü".to_owned(),
    };
    let dto = ChatMessageDto::from(recu);
    assert!(dto.from_remote);
    assert_eq!(dto.text, "bonjour éàü");

    // Écho local d'un message émis.
    let echo = nd_core::ChatMessage {
        from_remote: false,
        text: String::new(),
    };
    let dto = ChatMessageDto::from(echo);
    assert!(!dto.from_remote);
    assert!(dto.text.is_empty());
}

// ---------------------------------------------------------------------------
// Conversions plates : évènement de transfert (aplatissement des variantes)
// ---------------------------------------------------------------------------

#[test]
fn conversion_transfer_started() {
    let dto = TransferEventDto::from(nd_files::TransferEvent::FileStarted {
        index: 1,
        name: "a.bin".to_owned(),
        size: 4_096,
        resume_offset: 1_024,
    });
    assert_eq!(dto.kind, "started");
    assert_eq!(dto.file_index, Some(1));
    assert_eq!(dto.file_name.as_deref(), Some("a.bin"));
    // L'offset de reprise est exposé comme « octets déjà présents » du fichier.
    assert_eq!(dto.bytes_done, Some(1_024));
    assert_eq!(dto.bytes_total, Some(4_096));
    // Champs de session et de progression absents hors « progress ».
    assert_eq!(dto.session_bytes_done, None);
    assert_eq!(dto.percent, None);
    assert_eq!(dto.bytes_per_sec, None);
    assert_eq!(dto.eta_secs, None);
}

#[test]
fn conversion_transfer_progress() {
    let info = nd_files::TransferProgressInfo {
        file_index: 2,
        file_name: "gros.bin".to_owned(),
        file_bytes_done: 500,
        file_bytes_total: 1_000,
        session_bytes_done: 1_500,
        session_bytes_total: 3_000,
        bytes_per_sec: 1_234.5,
        eta_secs: Some(1.25),
    };
    let dto = TransferEventDto::from(nd_files::TransferEvent::Progress(info));
    assert_eq!(dto.kind, "progress");
    assert_eq!(dto.file_index, Some(2));
    assert_eq!(dto.file_name.as_deref(), Some("gros.bin"));
    assert_eq!(dto.bytes_done, Some(500));
    assert_eq!(dto.bytes_total, Some(1_000));
    assert_eq!(dto.session_bytes_done, Some(1_500));
    assert_eq!(dto.session_bytes_total, Some(3_000));
    // percent = ratio de session × 100 = 1500 / 3000 × 100 = 50.
    assert_eq!(dto.percent, Some(50.0));
    assert_eq!(dto.bytes_per_sec, Some(1_234.5));
    assert_eq!(dto.eta_secs, Some(1.25));
}

#[test]
fn conversion_transfer_completed() {
    let dto = TransferEventDto::from(nd_files::TransferEvent::FileCompleted {
        index: 0,
        name: "fini.bin".to_owned(),
        size: 2_048,
    });
    assert_eq!(dto.kind, "completed");
    assert_eq!(dto.file_index, Some(0));
    assert_eq!(dto.file_name.as_deref(), Some("fini.bin"));
    // Fichier terminé : octets faits = taille totale.
    assert_eq!(dto.bytes_done, Some(2_048));
    assert_eq!(dto.bytes_total, Some(2_048));
    assert_eq!(dto.percent, None);
}

#[test]
fn conversion_transfer_finished_et_cancelled() {
    for (event, attendu) in [
        (nd_files::TransferEvent::Finished, "finished"),
        (nd_files::TransferEvent::Cancelled, "cancelled"),
    ] {
        let dto = TransferEventDto::from(event);
        assert_eq!(dto.kind, attendu);
        // Évènements de file : aucun champ par fichier ni de progression.
        assert_eq!(dto.file_index, None);
        assert_eq!(dto.file_name, None);
        assert_eq!(dto.bytes_done, None);
        assert_eq!(dto.bytes_total, None);
        assert_eq!(dto.session_bytes_done, None);
        assert_eq!(dto.session_bytes_total, None);
        assert_eq!(dto.percent, None);
        assert_eq!(dto.bytes_per_sec, None);
        assert_eq!(dto.eta_secs, None);
    }
}

// ---------------------------------------------------------------------------
// Erreurs lisibles : commandes média sur un identifiant inconnu
// ---------------------------------------------------------------------------

#[test]
fn commandes_media_sur_session_inconnue() {
    // Identifiant jamais attribué (le compteur démarre à 1).
    let id = 606_060_606;
    for erreur in [
        send_chat(id, "salut".to_owned()).unwrap_err(),
        send_files(id, vec!["/tmp/x.bin".to_owned()]).unwrap_err(),
        set_audio_enabled(id, true).unwrap_err(),
        switch_monitor(id, 1).unwrap_err(),
    ] {
        assert!(erreur.contains("inconnue"), "message peu utile : {erreur}");
    }
}

// ---------------------------------------------------------------------------
// Session étendue complète en loopback (hôte + viewer via la façade)
// ---------------------------------------------------------------------------

/// Délai maximal pour atteindre `Active` (handshake QUIC + Noise en loopback).
const DELAI_ACTIF: Duration = Duration::from_secs(20);
/// Frames décodées exigées du flux réel (le mode étendu passe la vidéo en fiable).
const FRAMES_MIN: u32 = 3;
/// Délai maximal de collecte (encodage logiciel lent en profil debug).
const DELAI_FRAMES_MS: u64 = 60_000;

/// Options du mode étendu : toutes les fonctions média annexes activées.
fn options_etendues() -> SessionOptionsDto {
    SessionOptionsDto {
        permissions: PermissionsDto::full(),
        recording_path: None,
        delta_mode: false,
        extended_features: true,
        transfer_dir: None,
        transport_reconnect: false,
        mot_de_passe: None,
    }
}

/// Attend que la session `id` atteigne `Active` via les lectures synchrones
/// d'états (le `StreamSink` exigerait l'app Dart en face).
fn attendre_actif(id: u64, id_pair: u64) {
    let echeance = Instant::now() + DELAI_ACTIF;
    while Instant::now() < echeance {
        match wait_session_state(id, 250).expect("session connue") {
            Some(SessionStateDto::Active) => return,
            Some(SessionStateDto::Closed) => panic!(
                "session close avant Active (erreur : {:?} / pair : {:?})",
                session_last_error(id),
                session_last_error(id_pair)
            ),
            _ => {}
        }
    }
    panic!(
        "Active jamais atteint (erreur : {:?} / pair : {:?})",
        session_last_error(id),
        session_last_error(id_pair)
    );
}

#[test]
fn session_etendue_loopback_chat_fichiers_audio_moniteur() {
    // Fichier réel à proposer au transfert (envoi piloté par le viewer).
    let fichier = std::env::temp_dir().join(format!("nd_ffi_ext_{}.bin", std::process::id()));
    std::fs::write(&fichier, b"charge utile de transfert NovaDesk").expect("écriture du fichier");

    // 1. Hôte (contrôlé) en mode étendu, à l'écoute en loopback.
    let config_hote = new_session_config(
        SessionRoleDto::Controlled,
        111_111_111,
        None,
        PermissionsDto::full(),
    )
    .expect("configuration hôte valide");
    let id_hote = start_session_with_options(
        config_hote,
        SessionEndpointDto::Loopback,
        options_etendues(),
    )
    .expect("démarrage hôte étendu");
    let ecoute = session_listen_info(id_hote).expect("coordonnées d'écoute loopback");

    // 2. Viewer (contrôleur) en mode étendu, connexion directe à l'hôte.
    let config_viewer = new_session_config(
        SessionRoleDto::Controller,
        222_222_222,
        Some(111_111_111),
        PermissionsDto::full(),
    )
    .expect("configuration viewer valide");
    let id_viewer = start_session_with_options(
        config_viewer,
        SessionEndpointDto::Direct {
            addr: ecoute.addr,
            cert_der: ecoute.cert_der,
        },
        options_etendues(),
    )
    .expect("démarrage viewer étendu");

    // 3. La négociation étendue (vidéo fiable + canaux annexes) atteint Active.
    attendre_actif(id_viewer, id_hote);

    // 4. La vidéo circule toujours en mode étendu (frames décodées réelles).
    let frames = collect_video_frames(id_viewer, FRAMES_MIN, DELAI_FRAMES_MS)
        .expect("collecte de frames sur une session étendue");
    assert_eq!(
        frames.len(),
        FRAMES_MIN as usize,
        "frames décodées insuffisantes en mode étendu (erreur hôte : {:?})",
        session_last_error(id_hote)
    );

    // 5. Fonctions média annexes : chaque commande part sans erreur (l'effet
    //    réel dépend du pair/permissions ; ici on prouve le câblage de la façade).
    send_chat(id_viewer, "bonjour depuis le viewer".to_owned()).expect("envoi chat");
    send_files(id_viewer, vec![fichier.to_string_lossy().into_owned()]).expect("envoi fichier");
    set_audio_enabled(id_viewer, false).expect("bascule audio");
    set_audio_enabled(id_viewer, true).expect("bascule audio");
    switch_monitor(id_viewer, 0).expect("bascule moniteur");

    // 6. Arrêt propre des deux moteurs ; identifiants retirés de la table.
    stop_session(id_viewer).expect("arrêt du viewer");
    stop_session(id_hote).expect("arrêt de l'hôte");
    assert!(stop_session(id_viewer).is_err(), "viewer déjà arrêté");

    let _ = std::fs::remove_file(&fichier);
}
