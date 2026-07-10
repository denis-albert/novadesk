//! Tests d'intégration des **fonctions média étendues** de la façade UI
//! (`nd_ffi::api`, lot « session media ») : conversions plates des DTO de chat et
//! de transfert de fichiers, erreurs lisibles sur identifiant inconnu, et — quand
//! l'environnement le permet — session étendue en loopback via le moteur réel.

use std::time::{Duration, Instant};

use nd_ffi::{
    collect_video_frames, new_session_config, send_chat, send_files, session_download_file,
    session_last_error, session_listen_info, session_set_audio_source, set_audio_enabled,
    start_session_with_options, stop_session, switch_monitor, wait_session_state, ChatMessageDto,
    PermissionsDto, SessionEndpointDto, SessionOptionsDto, SessionRoleDto, SessionStateDto,
    TransferEventDto,
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
        // Mode valide : l'erreur restante est bien l'absence de session.
        session_set_audio_source(id, "systeme".to_owned()).unwrap_err(),
        session_download_file(id, "C:\\x.bin".to_owned(), "C:\\tmp".to_owned()).unwrap_err(),
    ] {
        assert!(erreur.contains("inconnue"), "message peu utile : {erreur}");
    }
}

/// Un **mode de source audio invalide** échoue avec un message français —
/// l'analyse précède la recherche de session (aucune session requise) ; les trois
/// modes valides passent l'analyse et n'échouent que sur l'absence de session.
#[test]
fn session_set_audio_source_mode_invalide_erreur_fr() {
    let err = session_set_audio_source(u64::MAX, "chuchotement".to_owned()).unwrap_err();
    assert!(
        err.contains("source audio inconnue"),
        "message peu utile : {err}"
    );
    for mode in ["systeme", "micro", "mixe"] {
        let err = session_set_audio_source(u64::MAX, mode.to_owned()).unwrap_err();
        assert!(err.contains("inconnue"), "mode « {mode} » : {err}");
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
        invitation: None,
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
    // Source d'émission audio de l'hôte : chaque mode valide part sans erreur.
    for mode in ["systeme", "micro", "mixe"] {
        session_set_audio_source(id_viewer, mode.to_owned())
            .unwrap_or_else(|e| panic!("bascule de source audio « {mode} » : {e}"));
    }

    // 5b. Téléchargement distant **réel** : un fichier du poste hôte (loopback,
    //     donc le même poste) est reconstruit localement par le viewer, par
    //     tranches. On vérifie le chemin local rendu et le contenu à l'octet près.
    //     Contenu > 1 MiB (tranche max) : la boucle offset → `fin` itère.
    let source_dl = std::env::temp_dir().join(format!("nd_ffi_dl_src_{}.bin", std::process::id()));
    let contenu: Vec<u8> = (0..1_048_576u32 + 5_000)
        .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
        .collect();
    std::fs::write(&source_dl, &contenu).expect("écriture de la source à télécharger");
    let dossier_recu = std::env::temp_dir().join(format!("nd_ffi_dl_dst_{}", std::process::id()));
    std::fs::create_dir_all(&dossier_recu).expect("création du dossier de réception");

    let ecrit = session_download_file(
        id_viewer,
        source_dl.to_string_lossy().into_owned(),
        dossier_recu.to_string_lossy().into_owned(),
    )
    .unwrap_or_else(|e| {
        panic!(
            "téléchargement distant (erreur hôte : {:?}) : {e}",
            session_last_error(id_hote)
        )
    });
    assert!(
        std::path::Path::new(&ecrit).starts_with(&dossier_recu),
        "le fichier téléchargé doit être écrit SOUS le dossier local : {ecrit}"
    );
    assert_eq!(
        std::fs::read(&ecrit).expect("relecture du fichier téléchargé"),
        contenu,
        "le fichier local doit avoir exactement le contenu de la source"
    );

    // 6. Arrêt propre des deux moteurs ; identifiants retirés de la table.
    stop_session(id_viewer).expect("arrêt du viewer");
    stop_session(id_hote).expect("arrêt de l'hôte");
    assert!(stop_session(id_viewer).is_err(), "viewer déjà arrêté");

    let _ = std::fs::remove_file(&fichier);
    let _ = std::fs::remove_file(&source_dl);
    let _ = std::fs::remove_dir_all(&dossier_recu);
}
