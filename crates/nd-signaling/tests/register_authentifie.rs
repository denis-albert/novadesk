//! Test d'intégration **plan 11 (moitié cliente)** : l'enregistrement
//! authentifié construit par `nd-signaling` est prouvé contre le **vrai
//! serveur de production** `server/nd-rendezvous`, monté en process :
//!
//! 1. la trame du client est identique **octet à octet** à la référence
//!    serveur (`nd_rendezvous::trame_register_authentifie`) et se décode
//!    symétriquement des deux côtés ;
//! 2. [`RendezvousClient::register_authentifie`] est **accepté** par la façade
//!    authentifiée, puis l'ID est résolvable par `lookup` et vivant via le
//!    `heartbeat` nu (transmis sans signature, par conception serveur) ;
//! 3. une signature de possession **invalide** (autre clé), un jeton visant un
//!    autre ID, un jeton d'une fausse autorité et le `Register` **nu** sont
//!    **rejetés** — rien n'entre au registre.
//!
//! `nd-rendezvous` et `nd-api` ne sont que des dépendances de dev : à
//! l'exécution, le client duplique le format (voir `src/auth.rs`).

use std::net::{SocketAddr, TcpListener};

use nd_api::auth::{Autorite, JetonEnregistrement};
use nd_proto::NovaId;
use nd_rendezvous::{servir_authentifie, ConfigRendezvous};
use nd_signaling::auth::{maintenant_unix, RegisterAuthentifie, SigningKey};
use nd_signaling::{Registry, RendezvousClient};

/// Autorité de déploiement de test, déterministe.
fn autorite_test() -> Autorite {
    Autorite::depuis_graine(&[21u8; 32])
}

/// Démarre la façade authentifiée réelle sur un port éphémère et renvoie
/// (client pointé dessus, registre partagé, autorité émettrice des jetons).
fn demarrer_rendezvous_production() -> (RendezvousClient, Registry, Autorite) {
    let autorite = autorite_test();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind façade");
    let adresse = listener.local_addr().expect("adresse façade");
    let registry = Registry::new();
    let config = ConfigRendezvous::new(autorite.cle_publique());
    let reg = registry.clone();
    std::thread::spawn(move || {
        let _ = servir_authentifie(listener, reg, config);
    });
    (RendezvousClient::new(adresse), registry, autorite)
}

fn adresse_publiee() -> SocketAddr {
    "203.0.113.9:45000".parse().expect("adresse")
}

/// La trame construite par le client est identique **octet à octet** à celle
/// du constructeur de référence du serveur (mêmes tag, ordre de champs et
/// message signé — Ed25519 étant déterministe, jusqu'à la signature incluse),
/// et chaque côté décode ce que l'autre encode.
#[test]
fn trame_client_identique_a_la_reference_serveur() {
    let cle = SigningKey::from_bytes(&[2u8; 32]);
    let id = 0x0102_0304_0506_0708u64;
    let addr = "203.0.113.9:45000";
    let cert = [0xAA, 0xBB, 0xCC];
    let horodatage = 1_753_000_000u64;
    let jeton = autorite_test().emettre_jeton_enregistrement(id, &cle.verifying_key());

    let trame_client =
        RegisterAuthentifie::signer(id, addr, &cert, horodatage, &jeton.to_bytes(), &cle)
            .to_bytes();
    let trame_serveur =
        nd_rendezvous::trame_register_authentifie(id, addr, &cert, horodatage, &jeton, &cle);
    assert_eq!(
        trame_client, trame_serveur,
        "la trame cliente doit être l'exact octet-à-octet de la référence serveur"
    );

    // Décodage symétrique : la trame de référence serveur se relit côté client
    // avec tous ses champs, jeton opaque compris.
    let relue = RegisterAuthentifie::from_bytes(&trame_serveur).expect("décodage");
    assert_eq!(relue.id, id);
    assert_eq!(relue.addr, addr);
    assert_eq!(relue.cert, cert);
    assert_eq!(relue.horodatage, horodatage);
    assert_eq!(
        JetonEnregistrement::from_bytes(&relue.jeton).expect("jeton"),
        jeton
    );
    assert!(relue.verifier_possession(&cle.verifying_key()));

    // Et le message signé côté client est celui que le serveur vérifie.
    assert_eq!(
        nd_signaling::auth::message_enregistrement(id, addr, &cert, horodatage),
        nd_rendezvous::message_enregistrement(id, addr, &cert, horodatage)
    );
}

/// Chemin nominal contre le serveur réel : enregistrement authentifié accepté,
/// ID résolvable par `lookup`, présence entretenue par le heartbeat nu.
#[test]
fn enregistrement_authentifie_accepte_puis_resolvable() {
    let (client, registry, autorite) = demarrer_rendezvous_production();
    let cle = SigningKey::from_bytes(&[2u8; 32]);
    let id = NovaId(123_456_789);
    let jeton = autorite.emettre_jeton_enregistrement(id.as_u64(), &cle.verifying_key());

    client
        .register_authentifie(id, adresse_publiee(), &[1, 2, 3], &jeton.to_bytes(), &cle)
        .expect("enregistrement authentifié accepté");
    assert_eq!(registry.online_count(), 1);

    // L'ID est résolvable : adresse et certificat publiés, tels quels.
    let pair = client.lookup(id).expect("lookup");
    assert_eq!(pair.addr, adresse_publiee());
    assert_eq!(pair.cert_der, vec![1, 2, 3]);

    // Le heartbeat nu suffit (la façade de production le transmet sans
    // signature — limite documentée côté serveur, voir src/auth.rs).
    client.heartbeat(id).expect("heartbeat");

    // Ré-enregistrement authentifié (nouvelle trame, nouvel horodatage) : ok.
    client
        .register_authentifie(id, adresse_publiee(), &[1, 2, 3], &jeton.to_bytes(), &cle)
        .expect("ré-enregistrement");
    assert_eq!(registry.online_count(), 1);
}

/// Preuves invalides contre le serveur réel : signature d'une autre clé,
/// jeton visant un autre ID, jeton d'une fausse autorité, `Register` nu —
/// tous rejetés, le registre reste vide ; le légitime passe ensuite.
#[test]
fn signature_invalide_ou_jeton_usurpe_rejetes() {
    let (client, registry, autorite) = demarrer_rendezvous_production();
    let victime = SigningKey::from_bytes(&[2u8; 32]);
    let attaquant = SigningKey::from_bytes(&[3u8; 32]);
    let id = NovaId(111_111_111);
    let jeton = autorite.emettre_jeton_enregistrement(id.as_u64(), &victime.verifying_key());

    // a) Jeton légitime (observé sur le réseau) mais signé par une autre clé :
    //    la preuve de possession échoue.
    assert!(client
        .register_authentifie(id, adresse_publiee(), &[7], &jeton.to_bytes(), &attaquant)
        .is_err());

    // b) Jeton de l'attaquant (pour SON id) rejoué sur l'ID de la victime.
    let jeton_attaquant =
        autorite.emettre_jeton_enregistrement(222_222_222, &attaquant.verifying_key());
    assert!(client
        .register_authentifie(
            id,
            adresse_publiee(),
            &[7],
            &jeton_attaquant.to_bytes(),
            &attaquant,
        )
        .is_err());

    // c) Jeton « fait maison » d'une autorité étrangère au déploiement.
    let fausse_autorite = Autorite::depuis_graine(&[99u8; 32]);
    let jeton_forge =
        fausse_autorite.emettre_jeton_enregistrement(id.as_u64(), &attaquant.verifying_key());
    assert!(client
        .register_authentifie(
            id,
            adresse_publiee(),
            &[7],
            &jeton_forge.to_bytes(),
            &attaquant,
        )
        .is_err());

    // d) Le `Register` nu historique est refusé par la production.
    assert!(client.register(id, adresse_publiee(), &[7]).is_err());

    // Rien n'est entré au registre ; l'ID reste introuvable.
    assert_eq!(registry.online_count(), 0);
    assert!(client.lookup(id).is_err());

    // La victime, elle, s'enregistre toujours avec sa clé et son jeton.
    client
        .register_authentifie(id, adresse_publiee(), &[7], &jeton.to_bytes(), &victime)
        .expect("enregistrement légitime");
    assert_eq!(registry.online_count(), 1);
}

/// Un horodatage hors de la fenêtre de tolérance du serveur est rejeté même
/// avec un jeton et une signature valides : la méthode cliente date toujours
/// ses trames de « maintenant », mais une trame rejouée tardivement (ici
/// forgée à ±1 h) doit être refusée.
#[test]
fn horodatage_hors_tolerance_rejete() {
    let (client, registry, autorite) = demarrer_rendezvous_production();
    let cle = SigningKey::from_bytes(&[2u8; 32]);
    let id = 333_333_333u64;
    let jeton = autorite.emettre_jeton_enregistrement(id, &cle.verifying_key());
    let addr = adresse_publiee().to_string();

    for horodatage in [maintenant_unix() - 3_600, maintenant_unix() + 3_600] {
        let trame =
            RegisterAuthentifie::signer(id, &addr, &[7], horodatage, &jeton.to_bytes(), &cle);
        // Envoi brut de la trame datée hors fenêtre, via le client de
        // référence du serveur impossible ici : on passe par un round-trip
        // TCP minimal équivalent à celui du client.
        assert!(
            envoyer_et_refuse(&client, &trame.to_bytes()),
            "trame datée hors tolérance acceptée à tort"
        );
    }
    assert_eq!(registry.online_count(), 0);
}

/// Envoie une trame brute au serveur du `client` et renvoie `true` si le
/// serveur répond par le refus générique (`NotFound`, tag 2).
fn envoyer_et_refuse(client: &RendezvousClient, trame: &[u8]) -> bool {
    use std::io::{Read, Write};
    let mut flux = std::net::TcpStream::connect(client.server_addr()).expect("connexion");
    flux.write_all(&(trame.len() as u32).to_be_bytes())
        .expect("longueur");
    flux.write_all(trame).expect("trame");
    let mut longueur = [0u8; 4];
    flux.read_exact(&mut longueur).expect("longueur réponse");
    let mut reponse = vec![0u8; u32::from_be_bytes(longueur) as usize];
    flux.read_exact(&mut reponse).expect("réponse");
    reponse.first() == Some(&2)
}
