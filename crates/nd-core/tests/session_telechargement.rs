//! Sondes d'intégration du **téléchargement de fichier distant** et de la
//! **bascule de source audio de l'hôte**, routés DANS la session (hôte
//! `Loopback` + contrôleur `Direct`, QUIC + Noise réels) sur le canal `Control`
//! chiffré. Prouve que :
//!
//! * [`SessionHandle::download_remote_file`] reconstitue **par tranches** un
//!   fichier réel du poste hôte (boucle offset → `fin`, > 1 tranche) et l'écrit
//!   localement à l'octet près, sous le dossier demandé ;
//! * le téléchargement est **gardé par la permission** [`Capability::FileDownload`]
//!   (la même que le listing) : après retrait à chaud du droit, la même demande
//!   échoue « accès refusé » — jamais de contenu sans droit ;
//! * [`SessionHandle::set_audio_source`] **traverse** la session (système / micro
//!   / mixé) sans rompre le canal chiffré (le nonce Noise reste synchronisé).

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState, SourceEmission,
};
use nd_features::{Capability, PermissionSet, Permissions};
use nd_proto::NovaId;

/// Une seule session loopback (capture + encodeur réels) à la fois : deux
/// sessions vidéo concurrentes sur la même machine se privent mutuellement de CPU
/// et de capture (stalls > 10 s en debug). La sérialisation reproduit le confort
/// d'un binaire à test unique — la convention des sondes réseau du crate (voir
/// `admission_non_surveillee.rs`).
static UN_SEUL_CAS: Mutex<()> = Mutex::new(());

/// Jeu de capacités complet (dont fichiers/réception, la garde du téléchargement).
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

/// Options étendues : le plan de contrôle (donc le téléchargement et la bascule
/// audio) exige le mode étendu — hors de lui, la session historique ne route rien.
fn options_etendues() -> SessionOptions {
    SessionOptions {
        permissions: Some(permissions_completes()),
        extended_features: true,
        ..SessionOptions::default()
    }
}

/// Motif déterministe non trivial (chaque offset produit un octet distinct de
/// ses voisins, sans période courte évidente).
fn motif(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
        .collect()
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

/// Monte une paire hôte (`Loopback`) + contrôleur (`Direct`) étendue, tous deux
/// devenus `Active`, et rend les deux poignées.
fn paire_active() -> (SessionHandle, SessionHandle) {
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
    (hote, controleur)
}

#[test]
fn telechargement_distant_par_tranches_et_permission() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    // Fichier réel côté « hôte » (le poste local joue les deux rôles), plus grand
    // que la tranche max (1 MiB) : la boucle offset → `fin` fera plusieurs
    // allers-retours (au moins 3 tranches ici).
    let base = std::env::temp_dir().join(format!("nd_core_dl_{}", std::process::id()));
    let source = base.join("source");
    let dossier_local = base.join("recu");
    std::fs::create_dir_all(&source).expect("création de la source");
    std::fs::create_dir_all(&dossier_local).expect("création du dossier local");
    let contenu = motif(2 * 1024 * 1024 + 12_345);
    let chemin_distant = source.join("gros.bin");
    std::fs::write(&chemin_distant, &contenu).expect("écriture de la source");
    let chemin_distant = chemin_distant.to_string_lossy().into_owned();

    let (hote, controleur) = paire_active();

    // --- 1. Téléchargement par tranches : contenu exact, chemin local rendu ---
    let ecrit = controleur
        .download_remote_file(chemin_distant.clone(), &dossier_local)
        .expect("téléchargement réussi");
    assert!(
        ecrit.starts_with(&dossier_local),
        "le fichier doit être écrit SOUS le dossier local demandé : {ecrit:?}"
    );
    assert_eq!(
        ecrit.file_name().and_then(|n| n.to_str()),
        Some("gros.bin"),
        "le nom local doit être le composant de base du chemin distant"
    );
    assert_eq!(
        std::fs::read(&ecrit).expect("relecture locale"),
        contenu,
        "le fichier local doit avoir exactement le contenu de la source"
    );

    // --- 2. Refus sans permission : retrait à chaud de fichiers/réception -----
    let sans_fichiers = {
        let mut p = permissions_completes();
        p.revoke(Capability::FileDownload);
        p
    };
    controleur.set_permissions(sans_fichiers);
    assert!(
        attendre_jusqu_a(Duration::from_secs(15), || !hote
            .current_permissions()
            .allows(Capability::FileDownload)),
        "l'hôte doit avoir retiré fichiers/réception de son ensemble vivant \
         (obtenu {:?})",
        hote.current_permissions()
    );

    let refus = controleur
        .download_remote_file(chemin_distant.clone(), &dossier_local)
        .expect_err("le téléchargement sans permission doit échouer");
    assert!(
        refus.to_string().contains("accès refusé"),
        "jamais de contenu sans droit ; erreur obtenue : {refus}"
    );

    controleur.stop();
    hote.stop();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn bascule_source_audio_traverse_la_session() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let (hote, controleur) = paire_active();

    // Chaque source demandée traverse le canal `Control` chiffré ; l'hôte applique
    // `definir_source_emission` (repli système si le micro manque).
    for source in [
        SourceEmission::MicroSeul,
        SourceEmission::SystemeEtMicro,
        SourceEmission::SystemeSeul,
    ] {
        controleur.set_audio_source(source);
    }

    // Preuve **déterministe** que les bascules N'ONT PAS rompu le canal `Control`
    // chiffré (nonce Noise) : une requête de listing — qui emprunte le **même
    // canal**, corrélée, à délai borné — aboutit toujours après les bascules. Une
    // désync du nonce empêcherait toute réponse ultérieure.
    let apres = controleur
        .list_remote_dir(String::new())
        .expect("le canal Control doit rester fonctionnel après les bascules de source audio");
    assert_eq!(
        apres.erreur, None,
        "le listing (même canal) doit réussir après les bascules audio"
    );
    assert!(
        !apres.entrees.is_empty(),
        "au moins une racine attendue — le canal chiffré est intact"
    );
    assert_eq!(
        hote.last_error(),
        None,
        "la bascule de source audio ne doit pas rompre le canal chiffré côté hôte"
    );
    assert_eq!(
        controleur.last_error(),
        None,
        "la bascule de source audio ne doit pas rompre le canal côté contrôleur"
    );

    controleur.stop();
    hote.stop();
}
