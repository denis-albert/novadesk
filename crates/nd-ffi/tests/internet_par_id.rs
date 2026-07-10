//! Test d'intégration **bout-en-bout (« Internet par ID », côté client)** : la
//! chaîne **réelle** de la façade `nd-ffi` est exercée contre les vrais serveurs
//! montés en process —
//!
//! 1. `nd-api` (autorité complète : émission des jetons applicatifs et des
//!    jetons d'enregistrement d'ID) ;
//! 2. la façade de rendez-vous **de production** (`nd_rendezvous::servir_authentifie`),
//!    qui **refuse** le `Register` nu et n'accepte qu'un enregistrement
//!    **authentifié** (jeton + preuve de possession).
//!
//! Le test prouve que, via les seules fonctions `nd-ffi` :
//! * `acquire_network_id` obtient un NovaId + un jeton d'enregistrement signé
//!   auprès de `nd-api`, les **persiste** (chiffrés au repos) et est
//!   **idempotent** ;
//! * `start_unattended_host` fait s'enregistrer l'hôte de façon **authentifiée**
//!   au rendez-vous de production — donc l'ID devient **résolvable** (`lookup`),
//!   ce qu'un enregistrement nu n'obtiendrait jamais de cette façade ;
//! * un **contrôleur** rejoint l'hôte authentifié (hole punching réussi via le
//!   rendez-vous de production).
//!
//! `nd-api`, `nd-rendezvous` et `nd-signaling` sont ici des dépendances de dev
//! (même cycle toléré que `crates/nd-signaling/tests/register_authentifie.rs`).

use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use nd_api::auth::{cle_publique_depuis_hex, Autorite};
use nd_api::{serve, Services};
use nd_ffi::api::{
    acquire_network_id, network_identity, start_unattended_host, stop_unattended_host,
    PermissionsDto,
};
use nd_proto::NovaId;
use nd_rendezvous::{servir_authentifie, ConfigRendezvous};
use nd_signaling::{establish_p2p, ConnAttempt, Registry, RendezvousClient};

/// Durée de vie du jeton applicatif de test.
const UNE_HEURE: Duration = Duration::from_secs(3600);

/// Démarre un serveur bloquant sur un port éphémère et renvoie son adresse.
fn demarrer<F>(servir: F) -> SocketAddr
where
    F: FnOnce(TcpListener) -> std::io::Result<()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let adresse = listener.local_addr().expect("adresse locale");
    std::thread::spawn(move || {
        let _ = servir(listener);
    });
    adresse
}

/// Attend qu'une condition devienne vraie (au plus `delai`), en sondant.
fn attendre(delai: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let echeance = Instant::now() + delai;
    while Instant::now() < echeance {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

#[test]
fn acquisition_puis_hote_authentifie_resolvable_et_controleur_connecte() {
    // Répertoire de données isolé : la façade (magasin global de `nd-ffi`) y
    // persiste l'identité réseau. À poser AVANT tout appel de façade.
    let donnees = tempfile::tempdir().expect("répertoire de données");
    std::env::set_var("NOVADESK_DATA_DIR", donnees.path());

    // 1. nd-api avec autorité de déploiement déterministe (émet jetons + IDs).
    let services = Services::new().avec_autorite(Autorite::depuis_graine(&[7u8; 32]));
    let jeton_compte = services
        .emettre_jeton("compte-ffi", UNE_HEURE)
        .expect("jeton applicatif");
    let cle_autorite = cle_publique_depuis_hex(&services.cle_publique_autorite_hex())
        .expect("clé publique d'autorité");
    let adresse_api = demarrer(move |l| serve(l, services));

    // 2. Rendez-vous DE PRODUCTION : vérification seule (clé publique), refuse le
    //    `Register` nu.
    let registry = Registry::new();
    let reg = registry.clone();
    let config = ConfigRendezvous::new(cle_autorite);
    let adresse_rv = demarrer(move |l| servir_authentifie(l, reg, config));

    // 3. Acquisition via la FAÇADE : NovaId + jeton signé, persistés.
    let identite = acquire_network_id(adresse_api.to_string(), jeton_compte.clone())
        .expect("acquisition de l'identité réseau");
    assert!(
        (100_000_000..1_000_000_000).contains(&identite.id),
        "NovaId à 9 chiffres attendu : {}",
        identite.id
    );
    assert_eq!(identite.id_formate, NovaId(identite.id).to_string());

    // Idempotence : un second appel réutilise l'identité (pas de réallocation).
    let rejoue = acquire_network_id(adresse_api.to_string(), jeton_compte)
        .expect("réacquisition idempotente");
    assert_eq!(rejoue, identite, "l'identité réseau doit être réutilisée");

    // Persistée et relisible telle quelle.
    assert_eq!(
        network_identity().expect("lecture identité réseau"),
        Some(identite.clone()),
    );

    // 4. Hôte « accès non surveillé » démarré via la FAÇADE : il lit l'identité
    //    réseau persistée et s'enregistre de façon AUTHENTIFIÉE au rendez-vous
    //    de production.
    let host_id = start_unattended_host(
        identite.id,
        adresse_rv.to_string(),
        vec![],
        PermissionsDto {
            keyboard: false,
            mouse: false,
            clipboard: false,
            files: false,
            audio: false,
            view_only: true,
        },
    )
    .expect("démarrage de l'hôte non surveillé");

    // 5. L'enregistrement authentifié est accepté → l'ID est EN LIGNE et
    //    RÉSOLVABLE (un `Register` nu aurait été refusé par cette façade).
    let rv = RendezvousClient::new(adresse_rv);
    assert!(
        attendre(Duration::from_secs(10), || registry.online_count() == 1),
        "l'hôte doit être enregistré (authentifié) et en ligne",
    );
    let fiche = rv
        .lookup(NovaId(identite.id))
        .expect("l'ID réseau doit être résolvable");
    assert!(!fiche.cert_der.is_empty(), "certificat publié");

    // 6. Un CONTRÔLEUR rejoint l'hôte authentifié : hole punching réussi via le
    //    rendez-vous de production (l'hôte publie ses candidats dans sa boucle
    //    d'attente). On réessaie tant que l'hôte n'a pas encore publié.
    let controleur = NovaId(424_242_424);
    let echeance = Instant::now() + Duration::from_secs(20);
    let mut connecte = None;
    while connecte.is_none() && Instant::now() < echeance {
        match establish_p2p(&rv, controleur, NovaId(identite.id), &[]) {
            Ok(ConnAttempt::Direct(chemin)) => connecte = Some(chemin),
            Ok(ConnAttempt::RelayFallback { .. }) | Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let chemin = connecte.expect("le contrôleur doit percer jusqu'à l'hôte authentifié");
    assert_eq!(
        chemin.peer_cert_der, fiche.cert_der,
        "certificat de l'hôte épinglé, identique à celui publié au rendez-vous",
    );

    stop_unattended_host(host_id).expect("arrêt de l'hôte");
}
