//! Sonde d'intégration des **raccourcis clavier hôte** dans la vraie boucle de
//! session (QUIC + Noise réels, capture et injecteur réels), en boucle locale :
//!
//! une table personnalisée ([`SessionOptions::hotkeys`]) lie une touche seule à
//! `HostAction::ReleaseMouse` ; le contrôleur envoie l'appui + le relâchement
//! par [`SessionHandle::input_tx`] ; côté hôte, le raccourci est **déclenché et
//! compté** (`SessionStats::hotkeys_applied`) et la frappe n'est **jamais
//! injectée** (`inputs_applied` reste nul — c'est tout l'intérêt : la sonde ne
//! frappe pas le poste qui exécute les tests). Les deux boucles d'injection
//! sont couvertes **séquentiellement** : historique (session simple) puis
//! étendue (récepteur démux) — une seule capture d'écran à la fois.
//!
//! Le détail du guichet (suivi des modificateurs, répétitions avalées, lecture
//! seule, déconnexion) est prouvé unitairement par `session::tests_raccourcis`.

use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState,
};
use nd_features::{Capability, HostAction, Hotkey, HotkeyMap, PermissionSet, Permissions};
use nd_proto::{InputEvent, NovaId};

/// Scancode de `F12` (jeu 1) : touche seule, sans modificateur — la sonde n'a
/// pas besoin d'envoyer Ctrl/Alt (qui, eux, seraient réellement injectés dans
/// le poste de test).
const SCAN_F12: u32 = 0x58;

/// Table personnalisée : `F12` seule → `HostAction::ReleaseMouse` (prouve au
/// passage que [`SessionOptions::hotkeys`] prime sur la table par défaut).
fn carte_f12() -> HotkeyMap<HostAction> {
    let mut carte = HotkeyMap::new();
    carte.bind(Hotkey::new(0, SCAN_F12), HostAction::ReleaseMouse);
    carte
}

/// Écran + souris + clavier accordés (le raccourci exige le clavier).
fn permissions_hote() -> PermissionSet {
    [
        Capability::ViewScreen,
        Capability::ControlMouse,
        Capability::ControlKeyboard,
    ]
    .into_iter()
    .collect()
}

fn attendre_actif(poignee: &SessionHandle, delai: Duration) -> bool {
    let echeance = Instant::now() + delai;
    let mut dernier = None;
    while dernier != Some(SessionState::Active) && Instant::now() < echeance {
        if let Ok(etat) = poignee.state_rx.recv_timeout(Duration::from_millis(100)) {
            dernier = Some(etat);
        }
    }
    dernier == Some(SessionState::Active)
}

/// Monte une session hôte/contrôleur en boucle locale (mode étendu ou non),
/// envoie `F12` (appui + relâchement) et rend les statistiques de l'hôte une
/// fois le raccourci compté (ou l'échéance passée).
fn sonde_raccourci(etendu: bool) -> nd_core::SessionStats {
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
        SessionOptions {
            permissions: Some(permissions_hote()),
            extended_features: etendu,
            hotkeys: Some(carte_f12()),
            ..SessionOptions::default()
        },
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
        SessionOptions {
            extended_features: etendu,
            ..SessionOptions::default()
        },
    )
    .expect("démarrage contrôleur");

    assert!(
        attendre_actif(&controleur, Duration::from_secs(20)),
        "le contrôleur doit devenir Actif (erreur : {:?})",
        controleur.last_error()
    );
    assert!(
        attendre_actif(&hote, Duration::from_secs(20)),
        "l'hôte doit devenir Actif (erreur : {:?})",
        hote.last_error()
    );

    // Appui + relâchement de F12, réémis tant que l'hôte n'a rien compté (le
    // canal est fiable ; la réémission n'absorbe que le démarrage des threads).
    let echeance = Instant::now() + Duration::from_secs(15);
    while hote.stats().hotkeys_applied == 0 && Instant::now() < echeance {
        controleur
            .input_tx
            .send(InputEvent::Key {
                scancode: SCAN_F12,
                down: true,
            })
            .expect("input_tx (appui)");
        controleur
            .input_tx
            .send(InputEvent::Key {
                scancode: SCAN_F12,
                down: false,
            })
            .expect("input_tx (relâchement)");
        std::thread::sleep(Duration::from_millis(300));
    }

    let stats = hote.stats();
    controleur.stop();
    hote.stop();
    stats
}

#[test]
fn raccourci_hote_declenche_compte_et_jamais_injecte() {
    // Boucle d'injection **historique** (session simple).
    let stats = sonde_raccourci(false);
    assert!(
        stats.hotkeys_applied >= 1,
        "le raccourci doit être déclenché et compté (boucle historique) : {stats:?}"
    );
    assert_eq!(
        stats.inputs_applied, 0,
        "la frappe du raccourci ne doit jamais être injectée : {stats:?}"
    );

    // Boucle d'injection **étendue** (récepteur démux).
    let stats = sonde_raccourci(true);
    assert!(
        stats.hotkeys_applied >= 1,
        "le raccourci doit être déclenché et compté (boucle étendue) : {stats:?}"
    );
    assert_eq!(
        stats.inputs_applied, 0,
        "aucune frappe injectée (boucle étendue) : {stats:?}"
    );
}
