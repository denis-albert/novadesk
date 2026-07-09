//! Sonde d'intégration du **plan de contrôle de session** en boucle locale (hôte
//! `Loopback` + contrôleur `Direct`, QUIC + Noise réels). Prouve que les cinq
//! capacités additives traversent une vraie session :
//!
//! * **permissions à chaud** : le contrôleur retire un droit → l'hôte l'applique
//!   à son ensemble vivant ([`SessionHandle::current_permissions`]) **et** le
//!   filtre d'injection le lit (une entrée souris refusée ensuite, comptée
//!   `inputs_denied`) ;
//! * **préréglage de qualité** : le contrôleur change le profil ABR + le plafond
//!   → l'hôte l'applique ([`SessionHandle::quality`]) ;
//! * **liste des moniteurs** : l'hôte publie ses écrans → le contrôleur les lit
//!   ([`SessionHandle::monitors`]) ;
//! * **infos système du pair** : l'hôte publie nom d'hôte + OS → le contrôleur
//!   les lit ([`SessionHandle::peer_info`]).
//!
//! L'enregistrement à chaud (capacité 3) est prouvé côté hôte par les tests
//! unitaires du pipeline (`nd-core`) ; le multi-écran **réel** dépend du matériel
//! (un poste mono-écran ne publie qu'un moniteur — la liste traverse néanmoins).

use std::time::{Duration, Instant};

use nd_codec::ContentProfile;
use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState,
};
use nd_features::{Capability, PermissionSet, Permissions};
use nd_proto::{InputEvent, NovaId};

/// Jeu de capacités complet (toutes fonctions étendues autorisées au départ).
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
        Capability::PrivacyMode,
        Capability::TcpTunnel,
    ]
    .into_iter()
    .collect()
}

fn options_etendues() -> SessionOptions {
    SessionOptions {
        permissions: Some(permissions_completes()),
        extended_features: true,
        ..SessionOptions::default()
    }
}

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

/// Attend qu'un prédicat devienne vrai (scrutation) ; délai de garde inclus.
fn attendre_jusqu_a(delai: Duration, mut predicat: impl FnMut() -> bool) -> bool {
    let echeance = Instant::now() + delai;
    while Instant::now() < echeance {
        if predicat() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    predicat()
}

#[test]
fn plan_de_controle_traverse_la_session() {
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
        options_etendues(),
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
        options_etendues(),
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

    // --- Liste des moniteurs : l'hôte publie, le contrôleur lit ---------------
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || controleur.monitors().is_some()),
        "le contrôleur doit recevoir la liste des moniteurs publiée par l'hôte"
    );
    let moniteurs = controleur.monitors().expect("liste des moniteurs reçue");
    // Sur un poste avec écran, la liste est non vide et un seul moniteur est
    // principal ; sur un hôte sans bureau (CI headless), la liste peut être vide
    // — l'annonce a néanmoins traversé (Some).
    if !moniteurs.is_empty() {
        assert_eq!(
            moniteurs.iter().filter(|m| m.primary).count(),
            1,
            "exactement un moniteur principal attendu : {moniteurs:?}"
        );
        assert!(
            moniteurs.iter().all(|m| m.width > 0 && m.height > 0),
            "dimensions de moniteur non nulles attendues : {moniteurs:?}"
        );
    }

    // --- Infos système du pair : l'hôte publie, le contrôleur lit -------------
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || controleur.peer_info().is_some()),
        "le contrôleur doit recevoir les infos système du pair"
    );
    let infos = controleur.peer_info().expect("infos du pair reçues");
    assert!(!infos.host.is_empty(), "nom d'hôte non vide attendu");
    assert!(!infos.os.is_empty(), "OS non vide attendu");

    // --- Préréglage de qualité : le contrôleur demande, l'hôte applique -------
    controleur.set_quality(ContentProfile::Video, 5_000);
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || hote.quality()
            == (ContentProfile::Video, 5_000)),
        "l'hôte doit appliquer le préréglage de qualité (obtenu {:?})",
        hote.quality()
    );

    // --- Permissions à chaud : le contrôleur retire la souris -----------------
    let sans_souris = {
        let mut p = permissions_completes();
        p.revoke(Capability::ControlMouse);
        p
    };
    controleur.set_permissions(sans_souris);
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || !hote
            .current_permissions()
            .allows(Capability::ControlMouse)),
        "l'hôte doit avoir retiré la capacité souris de son ensemble vivant \
         (obtenu {:?})",
        hote.current_permissions()
    );
    // Le voir-écran reste accordé (renégociation ciblée, pas un blocage total).
    assert!(hote.current_permissions().allows(Capability::ViewScreen));

    // Preuve que **le filtre d'injection** lit l'ensemble vivant : une fois la
    // souris retirée, les mouvements souris du contrôleur sont refusés côté hôte.
    let refus_avant = hote.stats().inputs_denied;
    for _ in 0..20 {
        controleur
            .input_tx
            .send(InputEvent::MouseMoveRel { dx: 3.0, dy: 0.0 })
            .expect("input_tx");
    }
    assert!(
        attendre_jusqu_a(Duration::from_secs(10), || hote.stats().inputs_denied
            > refus_avant),
        "les entrées souris doivent être refusées après retrait à chaud du droit \
         (refus avant = {refus_avant}, après = {})",
        hote.stats().inputs_denied
    );

    controleur.stop();
    hote.stop();
}
