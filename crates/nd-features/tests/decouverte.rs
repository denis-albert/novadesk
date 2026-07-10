//! Tests d'intégration de la découverte LAN : le chemin réseau réel (sockets
//! UDP, fil d'annonce, fil d'écoute) exercé sur la **boucle locale**, donc
//! sans dépendre de la disponibilité du multicast dans l'environnement. Le
//! trajet multicast complet est couvert par un dernier test *tolérant* :
//! quand le multicast est filtré (VM, pare-feu strict), il vérifie le
//! comportement de repli (aucun blocage, tentatives comptées) et le signale
//! sur la sortie d'erreur au lieu d'échouer — la logique de décodage,
//! déduplication et expiration reste prouvée par les autres tests et par les
//! unitaires du module.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use nd_features::decouverte::{
    encoder_presence, AnnonceurPresence, EcouteurPresence, OptionsEcoute, PairDecouvert,
};
use nd_proto::NovaId;

/// Délai maximal accordé à une condition asynchrone avant d'échouer.
const DELAI_TEST: Duration = Duration::from_secs(5);
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

fn boucle_locale(port: u16) -> SocketAddr {
    (Ipv4Addr::LOCALHOST, port).into()
}

#[test]
fn l_ecouteur_voit_un_pair_annonce_en_boucle_locale() {
    // Port 0 : l'OS choisit, l'annonceur vise ce port en direct — le test ne
    // dépend ni d'un port fixe ni du multicast de l'environnement.
    let ecouteur = EcouteurPresence::demarrer_avec(0, OptionsEcoute::default()).expect("écoute");
    let cible = boucle_locale(ecouteur.adresse_locale().port());
    let annonceur = AnnonceurPresence::demarrer_vers(
        NovaId(111_222_333),
        "Poste d'essai",
        cible,
        PERIODE_RAPIDE,
    )
    .expect("annonce");

    let pair: PairDecouvert = attendre(DELAI_TEST, || {
        ecouteur
            .pairs()
            .into_iter()
            .find(|pair| pair.id == NovaId(111_222_333))
    })
    .expect("le pair annoncé doit apparaître dans les 5 s");

    assert_eq!(pair.nom, "Poste d'essai");
    assert_eq!(pair.adresse.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
    assert!(
        annonceur.annonces_emises() >= 1,
        "au moins une annonce émise"
    );
    assert!(
        ecouteur.datagrammes_recus() >= 1,
        "au moins un datagramme reçu"
    );
    assert_eq!(ecouteur.datagrammes_ignores(), 0, "aucun datagramme rejeté");

    annonceur.arreter();
    ecouteur.arreter();
}

#[test]
fn dedup_par_id_et_exclusion_du_pair_local() {
    let moi = NovaId(42);
    let ecouteur = EcouteurPresence::demarrer_avec(
        0,
        OptionsEcoute {
            exclure: Some(moi),
            ..OptionsEcoute::default()
        },
    )
    .expect("écoute");
    let cible = boucle_locale(ecouteur.adresse_locale().port());

    // Ma propre annonce, un voisin qui annonce deux fois le même id (deux
    // beacons, adresses sources différentes) et un second voisin.
    let _moi = AnnonceurPresence::demarrer_vers(moi, "moi", cible, PERIODE_RAPIDE).expect("moi");
    let _v7a = AnnonceurPresence::demarrer_vers(NovaId(7), "voisin 7 (a)", cible, PERIODE_RAPIDE)
        .expect("7a");
    let _v7b = AnnonceurPresence::demarrer_vers(NovaId(7), "voisin 7 (b)", cible, PERIODE_RAPIDE)
        .expect("7b");
    let _v8 =
        AnnonceurPresence::demarrer_vers(NovaId(8), "voisin 8", cible, PERIODE_RAPIDE).expect("8");

    let pairs = attendre(DELAI_TEST, || {
        let pairs = ecouteur.pairs();
        let ids: Vec<u64> = pairs.iter().map(|pair| pair.id.as_u64()).collect();
        (ids == [7, 8]).then_some(pairs)
    })
    .expect("exactement les pairs 7 et 8 attendus (id 7 dédupliqué, moi exclu)");
    assert_eq!(
        pairs.len(),
        2,
        "4 annonceurs → 2 entrées (dédup + exclusion)"
    );

    // Plusieurs périodes d'annonce plus tard, ma propre annonce — pourtant
    // reçue en continu — n'est toujours pas listée.
    std::thread::sleep(4 * PERIODE_RAPIDE);
    assert!(
        ecouteur.pairs().iter().all(|pair| pair.id != moi),
        "sa propre annonce doit rester exclue"
    );
}

#[test]
fn un_pair_non_revu_expire() {
    // TTL court pour observer l'expiration sans attendre les 10 s de production.
    let ttl = Duration::from_millis(300);
    let ecouteur = EcouteurPresence::demarrer_avec(
        0,
        OptionsEcoute {
            ttl,
            ..OptionsEcoute::default()
        },
    )
    .expect("écoute");
    let cible = boucle_locale(ecouteur.adresse_locale().port());
    let annonceur = AnnonceurPresence::demarrer_vers(NovaId(5), "fugace", cible, PERIODE_RAPIDE)
        .expect("annonce");

    attendre(DELAI_TEST, || (!ecouteur.pairs().is_empty()).then_some(()))
        .expect("le pair doit d'abord être vu");

    // Le beacon se tait : le pair doit disparaître une fois le TTL écoulé.
    annonceur.arreter();
    let disparu_en = Instant::now();
    attendre(DELAI_TEST, || ecouteur.pairs().is_empty().then_some(()))
        .expect("le pair non revu doit expirer");
    assert!(
        disparu_en.elapsed() >= ttl.saturating_sub(PERIODE_RAPIDE),
        "l'expiration ne doit pas précéder le TTL"
    );
    ecouteur.arreter();
}

#[test]
fn les_datagrammes_corrompus_sont_ignores_sans_paniquer() {
    let ecouteur = EcouteurPresence::demarrer(0).expect("écoute");
    let cible = boucle_locale(ecouteur.adresse_locale().port());
    let emetteur = UdpSocket::bind(boucle_locale(0)).expect("socket d'essai");

    let valide = encoder_presence(NovaId(77), "sain");
    let mut version_inconnue = valide.clone();
    version_inconnue[4] = 250;
    let corrompus: [&[u8]; 4] = [
        b"pas une annonce du tout",
        &valide[..7],      // en-tête tronqué
        &version_inconnue, // bonne magie, version inconnue
        &[0xFF; 300],      // bruit binaire volumineux
    ];
    for datagramme in corrompus {
        emetteur.send_to(datagramme, cible).expect("envoi du bruit");
    }
    emetteur
        .send_to(&valide, cible)
        .expect("envoi de l'annonce");

    // L'annonce valide passe malgré le bruit… (l'écouteur n'a pas paniqué)
    let pairs = attendre(DELAI_TEST, || {
        let pairs = ecouteur.pairs();
        (!pairs.is_empty()).then_some(pairs)
    })
    .expect("l'annonce valide doit passer malgré le bruit");
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].id, NovaId(77));
    assert_eq!(pairs[0].nom, "sain");

    // …et les quatre datagrammes corrompus sont comptés comme ignorés.
    attendre(DELAI_TEST, || {
        (ecouteur.datagrammes_ignores() >= 4).then_some(())
    })
    .unwrap_or_else(|| {
        panic!(
            "4 datagrammes corrompus attendus comme ignorés, vu {}",
            ecouteur.datagrammes_ignores()
        )
    });
    ecouteur.arreter();
}

#[test]
fn arret_rapide_meme_avec_une_longue_periode() {
    let debut = Instant::now();
    {
        let ecouteur = EcouteurPresence::demarrer(0).expect("écoute");
        let annonceur = AnnonceurPresence::demarrer_vers(
            NovaId(1),
            "éphémère",
            boucle_locale(ecouteur.adresse_locale().port()),
            Duration::from_secs(3_600), // le prochain tick serait dans une heure…
        )
        .expect("annonce");
        annonceur.arreter(); // …mais l'arrêt interrompt l'attente immédiatement
        drop(ecouteur); // l'écouteur s'arrête au drop (≤ un délai de scrutation)
    }
    assert!(
        debut.elapsed() < Duration::from_secs(2),
        "l'arrêt doit être quasi immédiat (mesuré : {:?})",
        debut.elapsed()
    );
}

/// Trajet multicast de bout en bout — test **tolérant** : là où le multicast
/// est filtré, il valide le repli (le beacon a tenté sans bloquer ni paniquer)
/// et le documente sur stderr au lieu d'échouer.
#[test]
fn multicast_de_bout_en_bout_si_l_environnement_le_permet() {
    // Réserve un port UDP libre puis le libère pour l'écouteur (fenêtre de
    // course minime, acceptable dans un test).
    let port = {
        let sonde = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("sonde de port");
        sonde.local_addr().expect("adresse de la sonde").port()
    };

    let ecouteur = EcouteurPresence::demarrer(port).expect("écoute");
    let annonceur = AnnonceurPresence::demarrer_avec_periode(
        NovaId(654_321),
        "Poste multicast",
        port,
        PERIODE_RAPIDE,
    )
    .expect("annonce");

    match attendre(Duration::from_secs(3), || {
        ecouteur
            .pairs()
            .into_iter()
            .find(|pair| pair.id == NovaId(654_321))
    }) {
        Some(pair) => {
            assert_eq!(pair.nom, "Poste multicast");
            assert!(ecouteur.datagrammes_recus() >= 1);
        }
        None => {
            // Multicast (et diffusion) indisponibles ici : le module ne doit ni
            // paniquer ni bloquer, et le beacon doit avoir tenté à cadence
            // normale (~60 ticks en 3 s à 50 ms).
            let tentatives = annonceur.annonces_emises() + annonceur.echecs_emission();
            eprintln!(
                "multicast indisponible dans cet environnement (multicast_actif={}, \
                 émises={}, échecs={}) — trajet réseau validé en boucle locale uniquement",
                ecouteur.multicast_actif(),
                annonceur.annonces_emises(),
                annonceur.echecs_emission(),
            );
            assert!(
                tentatives >= 2,
                "le beacon doit continuer à tenter d'émettre sans bloquer"
            );
        }
    }
    annonceur.arreter();
    ecouteur.arreter();
}
