//! Tests d'intégration du lot « listing distant & découverte LAN »
//! (`nd_ffi::api`) : cycle de vie de la découverte (instance unique
//! idempotente, arrêt, redémarrage), un voisin réel vu via le chemin réseau
//! **en boucle locale** (sans dépendre du multicast de l'environnement, comme
//! les tests de la brique `nd_features::decouverte`), et l'erreur propre de
//! `session_list_remote_dir` sur une session inconnue. Le trajet complet du
//! listing **dans la session** (permission comprise) est prouvé côté moteur
//! par `nd-core/tests/session_listing_fichiers.rs`.

use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use nd_features::decouverte::AnnonceurPresence;
use nd_ffi::{discovery_peers, discovery_start, discovery_stop, session_list_remote_dir};
use nd_proto::NovaId;

/// Cadence d'annonce accélérée pour les tests (production : 2 s).
const PERIODE_RAPIDE: Duration = Duration::from_millis(50);

/// Scrute `condition` toutes les 20 ms jusqu'à `delai` ; `Some` dès succès.
fn attendre<T>(delai: Duration, mut condition: impl FnMut() -> Option<T>) -> Option<T> {
    let echeance = Instant::now() + delai;
    loop {
        if let Some(valeur) = condition() {
            return Some(valeur);
        }
        if Instant::now() >= echeance {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Listing distant : erreur propre sans session
// ---------------------------------------------------------------------------

/// Une session inconnue échoue immédiatement avec un message français clair —
/// sans attendre le délai de réponse du listing (l'erreur précède l'envoi).
#[test]
fn session_list_remote_dir_erreur_propre_sur_session_inconnue() {
    let depart = Instant::now();
    let err = session_list_remote_dir(u64::MAX, "C:\\".to_owned()).unwrap_err();
    assert!(err.contains("inconnue"), "message peu utile : {err}");
    assert!(
        depart.elapsed() < Duration::from_secs(2),
        "l'erreur doit être immédiate (pas d'attente de réponse) : {:?}",
        depart.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Découverte LAN : cycle de vie complet en un seul test (l'instance de
// découverte est un singleton du processus — les phases doivent s'enchaîner).
// ---------------------------------------------------------------------------

#[test]
fn decouverte_cycle_de_vie_et_pairs() {
    // Identité locale isolée dans un répertoire temporaire : la découverte
    // annonce l'ID persistant du poste (magasin d'état) — le test ne doit pas
    // toucher au vrai répertoire de données. Posé avant le premier appel qui
    // initialise le magasin (l'autre test de ce binaire n'y touche pas).
    let donnees = tempfile::tempdir().expect("répertoire de données temporaire");
    std::env::set_var("NOVADESK_DATA_DIR", donnees.path());

    // Port UDP libre réservé puis libéré (même astuce que les tests de la
    // brique) : le test ne squatte pas le port par défaut du parc, qu'une
    // vraie instance NovaDesk de la machine pourrait occuper.
    let port = {
        let sonde = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("sonde de port");
        sonde.local_addr().expect("adresse de la sonde").port()
    };

    // --- Port occupé : erreur française claire, rien ne démarre --------------
    {
        let bloqueur = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).expect("bloqueur");
        let err = discovery_start("Poste bloqué".to_owned(), port).unwrap_err();
        assert!(
            err.contains("impossible") && err.contains(&port.to_string()),
            "message peu utile : {err}"
        );
        assert!(
            discovery_peers().is_empty(),
            "rien ne doit vivre après un échec de démarrage"
        );
        drop(bloqueur);
    }

    // --- Démarrage + idempotence (une seule instance vivante) ----------------
    discovery_start("Poste test".to_owned(), port).expect("démarrage de la découverte");
    discovery_start("Autre nom, sans effet".to_owned(), port)
        .expect("second démarrage : idempotent, sans erreur");

    // --- Un voisin (id distinct) annonce vers le port d'écoute ---------------
    // Deux instances de la brique cohabitent ainsi : l'annonceur du singleton
    // (id local du magasin) et ce voisin — l'écouteur du singleton le voit.
    let voisin = AnnonceurPresence::demarrer_vers(
        NovaId(555_666_777),
        "Voisin été",
        (Ipv4Addr::LOCALHOST, port).into(),
        PERIODE_RAPIDE,
    )
    .expect("annonceur voisin");

    let pair = attendre(Duration::from_secs(10), || {
        discovery_peers().into_iter().find(|p| p.id == 555_666_777)
    })
    .expect("le voisin annoncé doit apparaître dans les pairs découverts");
    assert_eq!(pair.id_formate, "555 666 777", "id groupé par 3");
    assert_eq!(pair.nom, "Voisin été");
    assert!(
        pair.adresse.starts_with("127.0.0.1:"),
        "adresse source « ip:port » attendue : {}",
        pair.adresse
    );

    // Instantané dédupliqué : les annonces répétées du même id ne créent
    // qu'une entrée, et l'id local du poste n'y figure jamais.
    std::thread::sleep(4 * PERIODE_RAPIDE);
    let pairs = discovery_peers();
    assert_eq!(
        pairs.iter().filter(|p| p.id == 555_666_777).count(),
        1,
        "une seule entrée par id : {pairs:?}"
    );
    let id_local = nd_ffi::local_identity().expect("identité locale").id;
    assert!(
        pairs.iter().all(|p| p.id != id_local),
        "sa propre annonce doit rester exclue : {pairs:?}"
    );

    // --- Arrêt : plus aucun pair, arrêt idempotent ----------------------------
    discovery_stop().expect("arrêt de la découverte");
    assert!(
        discovery_peers().is_empty(),
        "après l'arrêt : instantané vide"
    );
    discovery_stop().expect("arrêt idempotent (déjà arrêtée)");

    // --- Redémarrage : le port se relie, le voisin encore annoncé réapparaît -
    discovery_start("Poste test bis".to_owned(), port).expect("redémarrage après arrêt");
    attendre(Duration::from_secs(10), || {
        discovery_peers().into_iter().find(|p| p.id == 555_666_777)
    })
    .expect("après redémarrage, le voisin toujours annoncé doit réapparaître");

    voisin.arreter();
    discovery_stop().expect("arrêt final");
}
