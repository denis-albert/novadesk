//! Test d'intégration **bout-en-bout (plan 11, moitié serveur B2)** : le jeton
//! d'enregistrement *réellement émis par nd-api* — attribution d'un `NovaId`
//! via [`nd_api::ApiClient::allocate_id`], donc à travers toute la chaîne
//! d'émission (protocole TCP `AllocateId` → magasins → autorité de signature →
//! réponse `IdAlloue`) — est accepté par le **vrai** serveur de rendez-vous de
//! production (`server/nd-rendezvous`, [`servir_authentifie`]) monté en
//! process, puis l'ID est résolvable par `lookup`.
//!
//! C'est la contre-épreuve **serveur** du test **client** de `nd-signaling`
//! (`tests/register_authentifie.rs`) : là, le jeton était forgé localement à
//! partir de l'autorité ; ici il transite par l'émission réelle, et la façade
//! de rendez-vous n'est configurée qu'avec la **clé publique** de cette
//! autorité ([`ConfigRendezvous::cle_autorite`]) — exactement le point de
//! jonction du déploiement (nd-api détient la clé privée, nd-rendezvous n'en
//! connaît que la clé publique).
//!
//! Rejets prouvés (VÉRIF) : un jeton présenté pour un **autre ID**, une trame
//! **expirée** (horodatage hors de la fenêtre de tolérance — le TTL d'anti-
//! rejeu de l'enregistrement), et un jeton d'une **fausse autorité**.
//!
//! `nd-rendezvous` et `nd-signaling` ne sont ici que des dépendances de dev
//! (cycle toléré, comme pour `crates/nd-signaling/tests/register_authentifie.rs`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use nd_api::auth::{
    cle_publique_depuis_hex, maintenant_unix, Autorite, JetonEnregistrement, SigningKey,
};
use nd_api::{serve, ApiClient, Services};
use nd_proto::NovaId;
use nd_rendezvous::{servir_authentifie, trame_register_authentifie, ConfigRendezvous};
use nd_signaling::{Registry, RendezvousClient};

/// Durée de vie des jetons applicatifs de test.
const UNE_HEURE: Duration = Duration::from_secs(3600);

/// Adresse (fictive) publiée par le pair à l'enregistrement.
fn adresse_publiee() -> SocketAddr {
    "203.0.113.9:45000".parse().expect("adresse")
}

/// Démarre un serveur bloquant (`serve`/`servir_authentifie`) sur un port
/// éphémère et renvoie son adresse.
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

/// Chaîne montée en process : un serveur nd-api (autorité complète) et la
/// façade de rendez-vous de production configurée avec la **seule clé publique**
/// de l'autorité nd-api.
struct Bancs {
    /// Client nd-api authentifié (compte « alice »), pour `allocate_id`.
    alice: ApiClient,
    /// Client du rendez-vous de production.
    rv: RendezvousClient,
    /// Registre partagé de la façade (pour compter les pairs en ligne).
    registry: Registry,
}

/// Monte la chaîne complète (nd-api + rendez-vous).
fn bancs() -> Bancs {
    // Serveur nd-api avec une autorité de déploiement déterministe : elle émet
    // jetons applicatifs et jetons d'enregistrement.
    let services = Services::new().avec_autorite(Autorite::depuis_graine(&[7u8; 32]));
    let jeton_alice = services
        .emettre_jeton("alice", UNE_HEURE)
        .expect("jeton alice");
    // Tout ce que le rendez-vous connaît de l'autorité : sa clé publique.
    let cle_autorite = cle_publique_depuis_hex(&services.cle_publique_autorite_hex())
        .expect("clé publique d'autorité");
    let adresse_api = demarrer(move |l| serve(l, services));
    let alice = ApiClient::connect(adresse_api, jeton_alice).expect("client alice");

    // Façade de rendez-vous de production : vérification seule (clé publique).
    let registry = Registry::new();
    let reg = registry.clone();
    let config = ConfigRendezvous::new(cle_autorite);
    let adresse_rv = demarrer(move |l| servir_authentifie(l, reg, config));

    Bancs {
        alice,
        rv: RendezvousClient::new(adresse_rv),
        registry,
    }
}

/// Chemin nominal : (ID, jeton) obtenus via nd-api, acceptés par le rendez-vous
/// de production, puis l'ID est résolvable par `lookup`.
#[test]
fn jeton_emis_par_nd_api_accepte_par_le_rendezvous_puis_resolvable() {
    let bancs = bancs();
    let cle_appareil = SigningKey::from_bytes(&[42u8; 32]);

    // 1. Alice obtient (ID, jeton) via nd-api : toute la chaîne d'émission.
    let alloue = bancs
        .alice
        .allocate_id(cle_appareil.verifying_key().to_bytes())
        .expect("allocation d'ID via nd-api");
    let id = NovaId(alloue.id);
    assert!(
        (100_000_000..1_000_000_000).contains(&alloue.id),
        "{}",
        alloue.id
    );

    // 2. Le rendez-vous de production accepte l'enregistrement authentifié.
    bancs
        .rv
        .register_authentifie(
            id,
            adresse_publiee(),
            &[1, 2, 3],
            &alloue.jeton_enregistrement,
            &cle_appareil,
        )
        .expect("enregistrement authentifié accepté par le rendez-vous");
    assert_eq!(bancs.registry.online_count(), 1);

    // 3. L'ID est résolvable : adresse et certificat publiés, tels quels.
    let pair = bancs.rv.lookup(id).expect("lookup");
    assert_eq!(pair.addr, adresse_publiee());
    assert_eq!(pair.cert_der, vec![1, 2, 3]);
}

/// Rejets exigés par la VÉRIF : jeton pour un autre ID, trame expirée, jeton
/// d'une fausse autorité — aucun n'inscrit quoi que ce soit ; le légitime
/// passe ensuite.
#[test]
fn jetons_pour_autre_id_expire_ou_fausse_autorite_rejetes() {
    let bancs = bancs();
    let cle_appareil = SigningKey::from_bytes(&[42u8; 32]);
    let alloue = bancs
        .alice
        .allocate_id(cle_appareil.verifying_key().to_bytes())
        .expect("allocation d'ID via nd-api");
    let id = NovaId(alloue.id);
    let jeton =
        JetonEnregistrement::from_bytes(&alloue.jeton_enregistrement).expect("jeton décodable");

    // a) Jeton légitime d'alice, présenté pour un AUTRE ID : le rendez-vous
    //    exige jeton.id == id — un jeton n'usurpe pas un autre ID.
    let autre_id = NovaId(alloue.id ^ 1);
    assert!(bancs
        .rv
        .register_authentifie(
            autre_id,
            adresse_publiee(),
            &[7],
            &alloue.jeton_enregistrement,
            &cle_appareil,
        )
        .is_err());

    // b) Trame EXPIRÉE : jeton et signature valides, mais horodatage hors de la
    //    fenêtre de tolérance (le TTL d'anti-rejeu de l'enregistrement). On
    //    forge la trame datée hors fenêtre et on l'envoie brute.
    let addr = adresse_publiee().to_string();
    for horodatage in [maintenant_unix() - 3_600, maintenant_unix() + 3_600] {
        let trame =
            trame_register_authentifie(alloue.id, &addr, &[7], horodatage, &jeton, &cle_appareil);
        assert!(
            envoyer_et_refuse(&bancs.rv, &trame),
            "trame expirée acceptée à tort"
        );
    }

    // c) Jeton d'une FAUSSE autorité (inconnue du rendez-vous) : refus.
    let fausse = Autorite::depuis_graine(&[99u8; 32]);
    let jeton_forge = fausse.emettre_jeton_enregistrement(alloue.id, &cle_appareil.verifying_key());
    assert!(bancs
        .rv
        .register_authentifie(
            id,
            adresse_publiee(),
            &[7],
            &jeton_forge.to_bytes(),
            &cle_appareil,
        )
        .is_err());

    // Aucun essai n'a rien inscrit ; l'ID reste introuvable.
    assert_eq!(bancs.registry.online_count(), 0);
    assert!(bancs.rv.lookup(id).is_err());

    // Le jeton légitime, lui, est bien accepté pour SON ID.
    bancs
        .rv
        .register_authentifie(
            id,
            adresse_publiee(),
            &[1, 2, 3],
            &alloue.jeton_enregistrement,
            &cle_appareil,
        )
        .expect("enregistrement légitime");
    assert_eq!(bancs.registry.online_count(), 1);
}

/// Envoie une trame brute à la façade du `rv` et renvoie `true` si le serveur
/// répond par le refus générique (`NotFound`, tag 2).
fn envoyer_et_refuse(rv: &RendezvousClient, trame: &[u8]) -> bool {
    let mut flux = TcpStream::connect(rv.server_addr()).expect("connexion");
    flux.write_all(&(trame.len() as u32).to_be_bytes())
        .expect("longueur");
    flux.write_all(trame).expect("trame");
    let mut longueur = [0u8; 4];
    flux.read_exact(&mut longueur).expect("longueur réponse");
    let mut reponse = vec![0u8; u32::from_be_bytes(longueur) as usize];
    flux.read_exact(&mut reponse).expect("réponse");
    reponse.first() == Some(&2)
}
