//! Test d'intégration du client de haut niveau ([`nd_api::ApiClient`]) contre un
//! serveur `nd-api` réel, dans un thread (état mémoire + compte racine).
//!
//! Même motif que `scenario_tcp.rs`, mais **entièrement piloté par le client
//! ergonomique** (jamais d'octets ni de `Request`/`Response` bruts) : un
//! scénario bout en bout — allouer un ID → créer un groupe → ajouter un membre
//! → partager → lister → rôle effectif — étendu au carnet, au RBAC, aux mises à
//! jour et à la configuration. Les gardes d'autorisation (RBAC) et les erreurs
//! métier remontent en [`nd_api::ErreurClient::Serveur`] avec leur message
//! français.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use nd_api::auth::{cle_publique_depuis_hex, JetonEnregistrement, SigningKey};
use nd_api::rbac::{Permission, Role};
use nd_api::sharing::Beneficiaire;
use nd_api::update::{ReleaseChannel, UpdateDecision, UpdateManifest, Version};
use nd_api::{serve, ApiClient, Contact, ErreurClient, Services};

/// Durée de vie des jetons de test.
const UNE_HEURE: Duration = Duration::from_secs(3600);

/// Démarre un serveur `nd-api` sur un port éphémère et renvoie son adresse.
fn demarrer_serveur(services: Services) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let adresse = listener.local_addr().expect("adresse locale");
    std::thread::spawn(move || {
        let _ = serve(listener, services);
    });
    adresse
}

/// Extrait le message d'une erreur serveur, ou échoue le test.
fn message_serveur(resultat: Result<(), ErreurClient>) -> String {
    match resultat {
        Err(ErreurClient::Serveur(message)) => message,
        autre => panic!("erreur serveur attendue, obtenu {autre:?}"),
    }
}

#[test]
fn scenario_client_bout_en_bout() {
    // État mémoire + compte racine ; jetons signés pour trois comptes.
    let services = Services::new().avec_compte_racine("racine");
    let jeton_racine = services
        .emettre_jeton("racine", UNE_HEURE)
        .expect("jeton racine");
    let jeton_alice = services
        .emettre_jeton("alice", UNE_HEURE)
        .expect("jeton alice");
    let jeton_bob = services.emettre_jeton("bob", UNE_HEURE).expect("jeton bob");
    let cle_autorite = services.cle_publique_autorite_hex();
    let adresse = demarrer_serveur(services);

    // Trois clients de haut niveau, un par compte (même adresse).
    let alice = ApiClient::connect(adresse, jeton_alice).expect("client alice");
    let bob = ApiClient::connect(adresse, jeton_bob).expect("client bob");
    let racine = ApiClient::connect(adresse, jeton_racine).expect("client racine");

    // 1. Alice provisionne son appareil : allocation d'un ID lié à sa clé
    //    statique, avec le jeton d'enregistrement destiné au rendez-vous.
    let cle_appareil = SigningKey::from_bytes(&[21u8; 32]);
    let alloue = alice
        .allocate_id(cle_appareil.verifying_key().to_bytes())
        .expect("allocation d'ID");
    let appareil = alloue.id;
    assert!(
        (100_000_000..1_000_000_000).contains(&appareil),
        "{appareil}"
    );
    // Le jeton d'enregistrement est signé par l'autorité et lie bien l'ID.
    let jeton_enr =
        JetonEnregistrement::from_bytes(&alloue.jeton_enregistrement).expect("jeton décodable");
    assert_eq!(jeton_enr.id, appareil);
    let cle = cle_publique_depuis_hex(&cle_autorite).expect("clé autorité");
    assert!(jeton_enr.verifier(&cle), "jeton d'enregistrement signé");

    // 2. Alice crée un groupe (elle en devient l'administratrice)...
    let groupe = alice.create_group("Support").expect("création du groupe");
    // ... bob (sans rôle sur le groupe) ne peut pas y ajouter de membre.
    assert_eq!(
        message_serveur(bob.add_member(groupe, "bob")),
        "accès refusé"
    );
    // ... la créatrice, si.
    alice.add_member(groupe, "alice").expect("ajout de membre");

    // 3. Partage de l'appareil au groupe (Operator) : réservé à la
    //    propriétaire de l'ID — bob est refusé, alice passe.
    assert_eq!(
        message_serveur(bob.share_device(appareil, Beneficiaire::Groupe(groupe), Role::Operator)),
        "accès refusé"
    );
    alice
        .share_device(appareil, Beneficiaire::Groupe(groupe), Role::Operator)
        .expect("partage de l'appareil");

    // 4. Lister : alice hérite du partage via le groupe ; bob ne voit rien.
    assert_eq!(
        alice.devices_shared_with("alice").expect("partages alice"),
        vec![(appareil, Role::Operator)]
    );
    assert!(bob
        .devices_shared_with("bob")
        .expect("partages bob")
        .is_empty());
    // Bob ne peut pas espionner les partages d'alice.
    assert_eq!(
        message_serveur(bob.devices_shared_with("alice").map(|_| ())),
        "accès refusé"
    );

    // 5. Rôle effectif hérité de l'appartenance au groupe.
    assert_eq!(
        alice
            .effective_role("alice", appareil)
            .expect("rôle effectif"),
        Some(Role::Operator)
    );

    // Les groupes d'alice, avec leurs membres.
    let groupes = alice.list_groups("alice").expect("groupes d'alice");
    assert_eq!(groupes.len(), 1);
    assert_eq!(groupes[0].id, groupe);
    assert_eq!(groupes[0].name, "Support");
    assert_eq!(groupes[0].members, vec!["alice".to_string()]);

    // Carnet d'adresses : celui du compte agissant (dérivé du jeton).
    alice
        .add_contact(42, "PC bureau")
        .expect("ajout de contact");
    assert_eq!(
        alice.list_contacts().expect("contacts d'alice"),
        vec![Contact {
            id: 42,
            alias: "PC bureau".into(),
        }]
    );
    assert!(bob.list_contacts().expect("contacts de bob").is_empty());

    // RBAC : alice ne peut pas s'auto-attribuer un rôle sur org-1 ; la racine
    // le fait, puis les permissions dérivées répondent.
    assert_eq!(
        message_serveur(alice.assign_role("alice", "org-1", Role::Admin)),
        "accès refusé"
    );
    racine
        .assign_role("alice", "org-1", Role::Admin)
        .expect("attribution du rôle");
    assert!(alice
        .has_permission("alice", "org-1", Permission::ManageMembers)
        .expect("permission d'alice"));
    // Refus par défaut : bob n'a aucune attribution.
    assert!(!bob
        .has_permission("bob", "org-1", Permission::ViewScreen)
        .expect("permission de bob"));

    // Mises à jour : publier un manifeste est une opération racine ; alice est
    // refusée. La vérification est anonyme (le jeton n'est pas envoyé).
    let manifeste = UpdateManifest {
        channel: ReleaseChannel::Stable,
        latest: Version::new(2, 1, 0),
        min_supported: Version::new(1, 0, 0),
        url: "https://updates.novadesk.example/stable/2.1.0/novadesk-setup.exe".into(),
        sha256: "cafebabe".repeat(8),
        delta_from: Some(Version::new(2, 0, 0)),
    };
    assert_eq!(
        message_serveur(alice.publish_manifest(manifeste.clone())),
        "accès refusé"
    );
    racine
        .publish_manifest(manifeste.clone())
        .expect("publication du manifeste");
    assert_eq!(
        alice
            .check_update(ReleaseChannel::Stable, Version::new(2, 0, 0))
            .expect("vérification de mise à jour"),
        UpdateDecision::UpdateAvailable(manifeste)
    );

    // Configuration : politique posée par la racine, lue par alice ; défauts
    // intégrés hérités, paires triées par clé.
    racine
        .set_policy("acme", "require_2fa", "true")
        .expect("politique d'organisation");
    let config = alice.effective_config("acme").expect("config d'acme");
    let valeur = |cle: &str| {
        config
            .iter()
            .find(|(c, _)| c == cle)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(valeur("require_2fa"), Some("true")); // Surcharge de l'org.
    assert_eq!(valeur("allow_file_transfer"), Some("true")); // Défaut intégré.
    assert_eq!(valeur("session_timeout_minutes"), Some("30"));
    let mut triees = config.clone();
    triees.sort();
    assert_eq!(config, triees, "paires triées par clé");
}

#[test]
fn jeton_invalide_remonte_en_erreur_serveur() {
    // Serveur avec compte racine, mais on se connecte avec un jeton bidon.
    let services = Services::new().avec_compte_racine("racine");
    let adresse = demarrer_serveur(services);
    let imposteur = ApiClient::connect(adresse, "nda1.zz.zz").expect("client");

    // Toute requête authentifiée est refusée avant d'atteindre le métier.
    assert_eq!(
        message_serveur(alice_liste(&imposteur)),
        "jeton invalide ou absent"
    );
    assert_eq!(
        message_serveur(imposteur.create_group("X").map(|_| ())),
        "jeton invalide ou absent"
    );
    // CheckUpdate reste anonyme : pas de jeton, pas de refus (canal vide).
    assert_eq!(
        imposteur
            .check_update(ReleaseChannel::Stable, Version::new(1, 0, 0))
            .expect("check update anonyme"),
        UpdateDecision::UpToDate
    );
}

/// Petite aide : `list_contacts` réduit à `Result<(), _>` pour le comparateur.
fn alice_liste(client: &ApiClient) -> Result<(), ErreurClient> {
    client.list_contacts().map(|_| ())
}
