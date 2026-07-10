//! Sonde d'intégration du **listing de fichiers distant DANS la session** (hôte
//! `Loopback` + contrôleur `Direct`, QUIC + Noise réels) : la brique `nd_files`
//! (plan 09) routée sur le canal `Control` chiffré. Prouve que :
//!
//! * [`SessionHandle::list_remote_dir`] rapporte le contenu d'un **dossier
//!   réel** du poste hôte à travers la session (requête → permission →
//!   `nd_files::traiter_requete_liste` → réponse corrélée par chemin) ;
//! * le chemin **vide** rend les racines du poste hôte (amorce du navigateur) ;
//! * le listing est **gardé par la permission**
//!   [`Capability::FileDownload`] : après retrait à chaud du droit, la même
//!   demande rend une réponse en erreur « accès refusé » — jamais d'entrée.

use std::time::{Duration, Instant};

use nd_core::{
    SessionConfig, SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole,
    SessionState,
};
use nd_features::{Capability, PermissionSet, Permissions};
use nd_proto::NovaId;

/// Jeu de capacités complet (dont fichiers/réception, la garde du listing).
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

/// Options étendues : le plan de contrôle (donc le listing) exige le mode
/// étendu — hors de lui, la session historique vidéo + entrées ne route rien.
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
fn listing_distant_traverse_la_session_et_respecte_la_permission() {
    // Dossier réel côté « hôte » (le poste local joue les deux rôles en
    // loopback) : un sous-dossier + un fichier, pour vérifier tri et tailles.
    let dossier = std::env::temp_dir().join(format!("nd_core_listing_{}", std::process::id()));
    std::fs::create_dir_all(dossier.join("sous_dossier")).expect("création du sous-dossier");
    std::fs::write(dossier.join("fichier.bin"), b"nova").expect("écriture du fichier");
    let chemin_dossier = dossier.to_string_lossy().into_owned();

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

    // --- 1. Un dossier réel traverse la session -------------------------------
    let reponse = controleur
        .list_remote_dir(chemin_dossier.clone())
        .expect("réponse de listing");
    assert_eq!(reponse.erreur, None, "listing autorisé : aucune erreur");
    assert_eq!(
        reponse.chemin, chemin_dossier,
        "chemin échoyé (corrélation)"
    );
    let noms: Vec<&str> = reponse.entrees.iter().map(|e| e.nom.as_str()).collect();
    assert_eq!(
        noms,
        ["sous_dossier", "fichier.bin"],
        "dossiers d'abord, puis fichiers"
    );
    assert!(reponse.entrees[0].est_dossier);
    assert!(!reponse.entrees[1].est_dossier);
    assert_eq!(reponse.entrees[1].taille, 4, "taille réelle de fichier.bin");
    assert!(
        reponse.entrees[1].modifie_le.is_some(),
        "mtime attendu pour un fichier réel"
    );

    // --- 2. Chemin vide = racines du poste hôte -------------------------------
    let racines = controleur
        .list_remote_dir(String::new())
        .expect("réponse des racines");
    assert_eq!(racines.erreur, None);
    assert!(racines.chemin.is_empty());
    assert!(
        !racines.entrees.is_empty(),
        "au moins une racine attendue (C:\\ ou /)"
    );
    assert!(
        racines.entrees.iter().all(|e| e.est_dossier),
        "les racines sont toutes des dossiers navigables"
    );

    // --- 3. Refus sans permission : retrait à chaud de fichiers/réception -----
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
        .list_remote_dir(chemin_dossier.clone())
        .expect("réponse (refus)");
    assert_eq!(
        refus.chemin, chemin_dossier,
        "chemin échoyé malgré le refus"
    );
    assert!(
        refus.entrees.is_empty(),
        "jamais de listing sans droit : {:?}",
        refus.entrees
    );
    let message = refus.erreur.expect("erreur « accès refusé » attendue");
    assert!(message.contains("accès refusé"), "{message}");

    controleur.stop();
    hote.stop();
    let _ = std::fs::remove_dir_all(&dossier);
}
