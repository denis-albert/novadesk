//! Sonde des **fonctions avancées** de la session étendue (mode étendu), en
//! boucle locale (hôte `Loopback` + contrôleur `Direct`, QUIC + Noise + capture
//! d'écran réels). Complément de `session_media_demo` (audio/chat/fichiers).
//!
//! Ce que la sonde **prouve** (assertions ; échec ⇒ code de sortie non nul) :
//!
//! 1. **Annotation / tableau blanc** (canal `Control`) : une couche de traits
//!    fait l'**aller-retour** contrôleur ↔ hôte, intègre.
//! 2. **Confidentialité** (rideau) : le contrôleur demande le mode
//!    confidentialité ; l'hôte **cesse de diffuser l'écran réel** et envoie un
//!    **cadre noir** (le contrôleur décode alors une image noire) ; l'hôte
//!    **renvoie son état**, que l'indicateur du contrôleur suit.
//! 3. **Tunnel TCP de session** : une connexion TCP locale est **relayée à
//!    travers la session** jusqu'à un service d'écho que l'hôte joint —
//!    aller-retour prouvé, octets comptés ([`nd_features::pipe_bidirectional_stats`]).
//!
//! **Honnêteté.** Tout est réel en boucle locale. Le **cadre d'écran**
//! ([`SessionHandle::set_region`]) est démontré côté transport (la demande
//! traverse la session, l'hôte la mémorise) ; son **rognage effectif** dépend du
//! backend de capture (DXGI le gère ; d'autres renvoient `NotImplemented`) et est
//! prouvé déterministiquement par les tests `tests_diffusion_avancee` de
//! `nd-core`. Le rendu du cadre noir est ici prouvé **de bout en bout** (l'image
//! décodée est noire) car une image-clé est forcée à la bascule.
//!
//! Lancer : `cargo run --example session_etendue_avancee_demo -p nd-core`

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState,
};
use nd_features::{AnnotationLayer, Capability, PermissionSet, Permissions, Stroke};
use nd_proto::NovaId;

/// Échéance large (encodage/décodage logiciels possibles en debug).
const ECHEANCE: Duration = Duration::from_secs(20);

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

fn options(dir: PathBuf) -> SessionOptions {
    SessionOptions {
        permissions: Some(permissions_completes()),
        extended_features: true,
        transfer_dir: Some(dir),
        ..SessionOptions::default()
    }
}

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

fn attendre_jusqu_a(mut predicat: impl FnMut() -> bool) -> bool {
    let echeance = Instant::now() + ECHEANCE;
    while Instant::now() < echeance {
        if predicat() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    predicat()
}

/// Serveur d'écho TCP local : la « cible distante » que l'hôte joint via le
/// tunnel. Renvoie chaque octet reçu ; rend son adresse.
fn serveur_echo() -> std::io::Result<SocketAddr> {
    let ecouteur = TcpListener::bind("127.0.0.1:0")?;
    let addr = ecouteur.local_addr()?;
    std::thread::spawn(move || {
        for flux in ecouteur.incoming() {
            let Ok(mut flux) = flux else { break };
            std::thread::spawn(move || {
                let mut tampon = [0u8; 4096];
                while let Ok(n) = flux.read(&mut tampon) {
                    if n == 0 || flux.write_all(&tampon[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(addr)
}

/// Vrai si la frame décodée est (quasi) noire — le cadre de confidentialité. On
/// compare la **moyenne par canal** : le cadre noir décodé est très sombre
/// (~0–30/255, marge pour l'arrondi YUV du codec logiciel), l'écran réel clair.
fn frame_noire(rgba: &[u8]) -> bool {
    if rgba.is_empty() {
        return false;
    }
    let somme: u64 = rgba
        .chunks_exact(4)
        .map(|p| u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2]))
        .sum();
    let canaux = (rgba.len() / 4 * 3).max(1) as u64;
    somme / canaux < 48
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("NovaDesk — sonde des fonctions avancées de session (boucle locale)");

    let base = std::env::temp_dir().join(format!("nd-avancee-demo-{}", std::process::id()));
    std::fs::create_dir_all(&base)?;

    let ecouteur = nd_transport::bind("127.0.0.1:0".parse()?)?;
    let addr = ecouteur.local_addr();
    let cert = ecouteur.server_cert_der();

    let hote = SessionEngine::start_with_options(
        SessionConfig {
            role: SessionRole::Controlled,
            local_id: NovaId(111_111_111),
            peer_id: None,
            permissions: Permissions::full(),
        },
        SessionEndpoint::Loopback { listener: ecouteur },
        options(base.clone()),
    )?;
    let controleur = SessionEngine::start_with_options(
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

    // --- 1. Annotation : aller-retour --------------------------------------
    let mut couche = AnnotationLayer::new();
    couche.add(Stroke::Arrow {
        from: (2.0, 2.0),
        to: (50.0, 60.0),
        color: 0xFF00_00FF,
        width: 3.0,
    });
    couche.add(Stroke::Text {
        position: (10.0, 80.0),
        contenu: "Regardez ici".to_owned(),
        color: 0x00FF_00FF,
        size: 16.0,
    });
    controleur.send_annotation(couche.clone());
    let recue_hote = attendre_annotation(&hote).expect("l'hôte doit recevoir l'annotation");
    assert_eq!(recue_hote.strokes(), couche.strokes());
    hote.send_annotation(recue_hote);
    assert!(
        attendre_annotation(&controleur).is_some(),
        "le contrôleur doit recevoir l'annotation en retour"
    );
    println!("Annotation : couche (flèche + texte) prouvée en aller-retour.");

    // --- 2. Cadre d'écran : la demande traverse la session ------------------
    controleur.set_region(Some((0, 0, 64, 64)));
    assert!(
        attendre_jusqu_a(|| hote.requested_region() == Some((0, 0, 64, 64))),
        "l'hôte doit avoir reçu la demande de cadre d'écran (obtenu {:?})",
        hote.requested_region()
    );
    println!(
        "Cadre d'écran : demande (0,0,64,64) reçue par l'hôte = {:?} (rognage effectif \
         selon backend ; prouvé en unitaire).",
        hote.requested_region()
    );

    // --- 3. Confidentialité : cadre noir + état renvoyé ---------------------
    controleur.set_privacy(true);
    // On draine `frame_rx` (file bornée) **en continu** dès la demande : le cadre
    // noir est une image-clé forcée à la bascule (les cadres noirs suivants sont
    // des répétitions vides non re-livrées), il ne doit pas être perdu par
    // saturation du tampon. L'indicateur suit l'état renvoyé par l'hôte.
    let echeance = Instant::now() + ECHEANCE;
    let mut vu_noir = false;
    let mut indicateur = false;
    while !(vu_noir && indicateur) && Instant::now() < echeance {
        if controleur.privacy_active() {
            indicateur = true;
        }
        if let Ok(frame) = controleur.frame_rx.recv_timeout(Duration::from_millis(100)) {
            if frame_noire(&frame.rgba) {
                vu_noir = true;
            }
        }
    }
    assert!(
        indicateur,
        "l'indicateur de confidentialité du contrôleur doit s'allumer"
    );
    assert!(
        vu_noir,
        "sous confidentialité, le contrôleur doit décoder un cadre noir"
    );
    controleur.set_privacy(false);
    assert!(
        attendre_jusqu_a(|| !controleur.privacy_active()),
        "le rideau doit se lever"
    );
    println!("Confidentialité : cadre noir diffusé + état suivi par l'indicateur.");

    // --- 4. Tunnel TCP : connexion locale relayée jusqu'à un service --------
    let echo = serveur_echo()?;
    let tunnel = controleur.open_tunnel(0, echo)?;
    let mut client = TcpStream::connect(tunnel.local_addr())?;
    client.set_read_timeout(Some(ECHEANCE))?;
    let message = b"NovaDesk tunnel de session";
    client.write_all(message)?;
    let mut recu = vec![0u8; message.len()];
    client.read_exact(&mut recu)?;
    assert_eq!(&recu, message, "l'écho doit revenir intact par le tunnel");
    assert!(
        attendre_jusqu_a(|| tunnel.stats().octets_total() >= message.len() as u64),
        "les octets relayés doivent être comptés (obtenu {:?})",
        tunnel.stats()
    );
    let stats = tunnel.stats();
    println!(
        "Tunnel : {} octets relayés (a→b {}, b→a {}), {} connexion(s), écho intègre.",
        stats.octets_total(),
        stats.octets_a_vers_b,
        stats.octets_b_vers_a,
        stats.connexions
    );

    drop(client);
    tunnel.close();
    controleur.stop();
    hote.stop();
    let _ = std::fs::remove_dir_all(&base);

    println!();
    println!(
        "OK : fonctions avancées validées — annotation aller-retour, cadre d'écran transmis, \
         confidentialité (cadre noir + indicateur), tunnel TCP relayé à travers la session."
    );
    Ok(())
}

/// Attend une couche d'annotation reçue sur la poignée.
fn attendre_annotation(poignee: &SessionHandle) -> Option<AnnotationLayer> {
    let echeance = Instant::now() + ECHEANCE;
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
