//! Sonde d'intégration des **canaux annexes** de la session étendue, en boucle
//! locale (hôte `Loopback` + contrôleur `Direct`, QUIC + Noise réels) :
//!
//! * **chat** bidirectionnel (canal `Control` multiplexé) — aller-retour ;
//! * **transfert de fichiers** (canal `Files`) — un fichier reçu, intègre.
//!
//! L'audio et le presse-papiers (qui exigent des briques injectées) sont prouvés
//! par `examples/session_media_demo.rs`. Ici on valide le **câblage réel** dans
//! la boucle de session (démux, permissions, threads) sans matériel.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState,
};
use nd_features::{Capability, PermissionSet, Permissions};
use nd_files::TransferEvent;
use nd_proto::NovaId;

/// Jeu de capacités complet (toutes les fonctions étendues autorisées).
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

/// Options d'une session étendue avec répertoire de réception donné.
fn options_etendues(dir: PathBuf) -> SessionOptions {
    SessionOptions {
        permissions: Some(permissions_completes()),
        extended_features: true,
        transfer_dir: Some(dir),
        ..SessionOptions::default()
    }
}

/// Attend l'état `attendu` (échec silencieux sur échéance).
fn attendre_actif(poignee: &SessionHandle, attendu: SessionState, delai: Duration) -> bool {
    let echeance = Instant::now() + delai;
    let mut dernier = None;
    while dernier != Some(attendu) && Instant::now() < echeance {
        if let Ok(etat) = poignee.state_rx.recv_timeout(Duration::from_millis(100)) {
            dernier = Some(etat);
        }
    }
    dernier == Some(attendu)
}

#[test]
fn session_etendue_chat_et_transfert_de_fichiers() {
    // Répertoire de réception + fichier source à transférer.
    let base = std::env::temp_dir().join(format!("nd-media-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&base);
    let dir_reception = base.join("recu");
    let _ = std::fs::create_dir_all(&dir_reception);
    let source = base.join("cadeau.bin");
    let contenu: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &contenu).expect("écriture du fichier source");

    // Hôte (Loopback) : publie un écouteur QUIC ; contrôleur (Direct) : s'y connecte.
    let ecouteur = nd_transport::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = ecouteur.local_addr();
    let cert = ecouteur.server_cert_der();

    let hote = SessionEngine::start_with_options(
        SessionConfig {
            role: SessionRole::Controlled,
            local_id: NovaId(1),
            peer_id: None,
            permissions: Permissions::full(),
        },
        SessionEndpoint::Loopback { listener: ecouteur },
        options_etendues(dir_reception.clone()),
    )
    .expect("démarrage hôte");

    let controleur = SessionEngine::start_with_options(
        SessionConfig {
            role: SessionRole::Controller,
            local_id: NovaId(2),
            peer_id: Some(NovaId(1)),
            permissions: Permissions::full(),
        },
        SessionEndpoint::Direct {
            addr,
            cert_der: cert,
        },
        // Le contrôleur n'a pas besoin d'un répertoire de réception ici.
        options_etendues(base.clone()),
    )
    .expect("démarrage contrôleur");

    assert!(
        attendre_actif(&controleur, SessionState::Active, Duration::from_secs(20)),
        "le contrôleur doit devenir Actif (erreur : {:?})",
        controleur.last_error()
    );
    assert!(
        attendre_actif(&hote, SessionState::Active, Duration::from_secs(20)),
        "l'hôte doit devenir Actif (erreur : {:?})",
        hote.last_error()
    );

    // --- Chat aller-retour -------------------------------------------------
    controleur.send_chat("bonjour hôte");
    let recu_hote = attendre_chat(&hote, "bonjour hôte", Duration::from_secs(10));
    assert!(recu_hote, "l'hôte doit recevoir le message du contrôleur");

    hote.send_chat("bonjour contrôleur");
    let recu_ctl = attendre_chat(&controleur, "bonjour contrôleur", Duration::from_secs(10));
    assert!(recu_ctl, "le contrôleur doit recevoir la réponse de l'hôte");

    // --- Transfert de fichiers (contrôleur → hôte) -------------------------
    controleur.send_files(vec![source.clone()]);
    let termine = attendre_transfert_termine(&hote, Duration::from_secs(30));
    assert!(termine, "l'hôte doit signaler la fin du transfert");

    let recu = dir_reception.join("cadeau.bin");
    let octets = std::fs::read(&recu).expect("le fichier reçu doit exister");
    assert_eq!(octets, contenu, "le fichier reçu doit être intègre");

    controleur.stop();
    hote.stop();
    let _ = std::fs::remove_dir_all(&base);
}

/// Attend un [`ChatMessage`] distant au texte donné.
fn attendre_chat(poignee: &SessionHandle, texte: &str, delai: Duration) -> bool {
    let echeance = Instant::now() + delai;
    while Instant::now() < echeance {
        if let Ok(msg) = poignee.chat_rx.recv_timeout(Duration::from_millis(200)) {
            if msg.from_remote && msg.text == texte {
                return true;
            }
        }
    }
    false
}

/// Attend un événement [`TransferEvent::FileCompleted`] ou `Finished`.
fn attendre_transfert_termine(poignee: &SessionHandle, delai: Duration) -> bool {
    let echeance = Instant::now() + delai;
    while Instant::now() < echeance {
        if let Ok(ev) = poignee.transfer_rx.recv_timeout(Duration::from_millis(200)) {
            if matches!(
                ev,
                TransferEvent::FileCompleted { .. } | TransferEvent::Finished
            ) {
                return true;
            }
        }
    }
    false
}
