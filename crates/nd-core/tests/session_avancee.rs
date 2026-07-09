//! Sonde d'intégration des **fonctions avancées** de la session étendue, en
//! boucle locale (hôte `Loopback` + contrôleur `Direct`, QUIC + Noise réels) :
//!
//! * **confidentialité** : demande du contrôleur → l'hôte l'applique (gardé par
//!   [`Capability::PrivacyMode`]) et **renvoie son état** ; l'indicateur du
//!   contrôleur ([`SessionHandle::privacy_active`]) suit ;
//! * **annotation / tableau blanc** : couche échangée **aller-retour** sur le
//!   canal `Control` ;
//! * **cadre d'écran** : une demande de région traverse la session (l'hôte la
//!   mémorise, [`SessionHandle::requested_region`]) ;
//! * **tunnel TCP de session** : une connexion TCP locale est **relayée à
//!   travers la session** jusqu'à un service que l'hôte joint, aller-retour
//!   prouvé, octets comptés.
//!
//! Le rendu du cadre noir de confidentialité et le rognage effectif du cadre
//! d'écran sont prouvés **déterministiquement** (sans matériel) par les tests
//! unitaires `tests_diffusion_avancee` de `nd-core` ; ici on valide le **câblage
//! du plan de contrôle** dans la vraie boucle de session.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState,
};
use nd_features::{AnnotationLayer, Capability, PermissionSet, Permissions, Stroke};
use nd_proto::NovaId;

/// Jeu de capacités complet, **confidentialité et tunnel compris**.
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

fn options_etendues(dir: PathBuf) -> SessionOptions {
    SessionOptions {
        permissions: Some(permissions_completes()),
        extended_features: true,
        transfer_dir: Some(dir),
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

/// Attend qu'un prédicat devienne vrai (scrutation), délai de garde inclus.
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

/// Reçoit une couche d'annotation dont la première forme est la flèche attendue.
fn attendre_annotation(poignee: &SessionHandle, delai: Duration) -> Option<AnnotationLayer> {
    let echeance = Instant::now() + delai;
    while Instant::now() < echeance {
        if let Ok(couche) = poignee
            .annotation_rx
            .recv_timeout(Duration::from_millis(200))
        {
            return Some(couche);
        }
    }
    None
}

/// Un serveur d'écho TCP local (la « cible distante » que l'hôte joint via le
/// tunnel) : renvoie chaque octet reçu. Rend son adresse.
fn serveur_echo() -> std::net::SocketAddr {
    let ecouteur = TcpListener::bind("127.0.0.1:0").expect("bind écho");
    let addr = ecouteur.local_addr().expect("adresse écho");
    std::thread::spawn(move || {
        for flux in ecouteur.incoming() {
            let Ok(mut flux) = flux else { break };
            std::thread::spawn(move || {
                let mut tampon = [0u8; 4096];
                loop {
                    match flux.read(&mut tampon) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if flux.write_all(&tampon[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

#[test]
fn session_etendue_confidentialite_annotation_region_et_tunnel() {
    let base = std::env::temp_dir().join(format!("nd-avancee-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&base);

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
        options_etendues(base.clone()),
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

    // --- Annotation : aller-retour -----------------------------------------
    let mut couche = AnnotationLayer::new();
    couche.add(Stroke::Arrow {
        from: (1.0, 2.0),
        to: (30.0, 40.0),
        color: 0xFF00_00FF,
        width: 3.0,
    });
    controleur.send_annotation(couche.clone());
    let recue_hote = attendre_annotation(&hote, Duration::from_secs(10)).expect("annotation hôte");
    assert_eq!(recue_hote.strokes(), couche.strokes());

    let mut reponse = AnnotationLayer::new();
    reponse.add(Stroke::Rect {
        min: (0.0, 0.0),
        max: (10.0, 10.0),
        color: 0x00FF_00FF,
        width: 1.0,
    });
    hote.send_annotation(reponse.clone());
    let recue_ctl =
        attendre_annotation(&controleur, Duration::from_secs(10)).expect("annotation contrôleur");
    assert_eq!(recue_ctl.strokes(), reponse.strokes());

    // --- Confidentialité : demande → application hôte → état renvoyé ---------
    controleur.set_privacy(true);
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || controleur.privacy_active()),
        "l'indicateur de confidentialité du contrôleur doit suivre l'hôte"
    );
    assert!(hote.privacy_active(), "l'hôte doit appliquer le rideau");
    controleur.set_privacy(false);
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || !controleur.privacy_active()),
        "le rideau doit se lever"
    );

    // --- Cadre d'écran : la demande traverse la session ---------------------
    controleur.set_region(Some((0, 0, 32, 32)));
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || hote.requested_region()
            == Some((0, 0, 32, 32))),
        "l'hôte doit avoir reçu la demande de cadre d'écran (obtenu {:?})",
        hote.requested_region()
    );

    // --- Tunnel TCP : connexion locale relayée jusqu'à un service distant ----
    let echo = serveur_echo();
    let tunnel = controleur
        .open_tunnel(0, echo)
        .expect("ouverture du tunnel");
    let mut client = TcpStream::connect(tunnel.local_addr()).expect("connexion au tunnel local");
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("délai de lecture");
    client.write_all(b"nova-tunnel").expect("écriture tunnel");
    let mut recu = [0u8; 11];
    client
        .read_exact(&mut recu)
        .expect("écho relayé par la session");
    assert_eq!(
        &recu, b"nova-tunnel",
        "l'écho doit revenir intact par le tunnel"
    );

    assert!(
        attendre_jusqu_a(Duration::from_secs(5), || tunnel.stats().octets_total()
            >= 11),
        "les octets relayés doivent être comptés (obtenu {:?})",
        tunnel.stats()
    );

    drop(client);
    tunnel.close();
    controleur.stop();
    hote.stop();
    let _ = std::fs::remove_dir_all(&base);
}
