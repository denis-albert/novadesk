//! Tests d'intégration TCP de `nd-api` : un serveur réel dans un thread, un
//! client TCP qui déroule un scénario réaliste bout en bout par le protocole
//! (trames `u32` BE) — **jetons applicatifs signés** et RBAC appliqué —, puis
//! la persistance : rouvrir le fichier d'état et retrouver tout ce qui a été
//! écrit (carnet, groupes, partages, rôles, ID alloués).

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use nd_api::auth::{JetonEnregistrement, SigningKey};
use nd_api::protocol::{read_frame, write_frame, Request, Response};
use nd_api::rbac::{Permission, Role};
use nd_api::services::{serve, Services};
use nd_api::sharing::Beneficiaire;
use nd_api::update::{ReleaseChannel, UpdateDecision, UpdateManifest, Version};
use nd_api::Contact;

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

/// Un échange complet : une connexion, une requête, une réponse.
fn aller_retour(adresse: SocketAddr, requete: &Request) -> Response {
    let mut flux = TcpStream::connect(adresse).expect("connexion");
    write_frame(&mut flux, &requete.to_bytes()).expect("écriture");
    Response::from_bytes(&read_frame(&mut flux).expect("lecture")).expect("réponse décodable")
}

/// Raccourci : requête dont on attend `Response::Ok`.
fn attendre_ok(adresse: SocketAddr, requete: &Request) {
    assert_eq!(aller_retour(adresse, requete), Response::Ok, "{requete:?}");
}

/// Raccourci : requête dont on attend un refus d'accès (RBAC).
fn attendre_acces_refuse(adresse: SocketAddr, requete: &Request) {
    assert_eq!(
        aller_retour(adresse, requete),
        Response::Erreur {
            message: "accès refusé".into()
        },
        "{requete:?}"
    );
}

/// Chemin d'état unique dans le répertoire temporaire du système.
fn chemin_etat(nom: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nd-api-int-{}-{nom}.json", std::process::id()))
}

#[test]
fn scenario_complet_par_le_protocole() {
    let services = Services::new().avec_compte_racine("racine");
    let racine = services.emettre_jeton("racine", UNE_HEURE).expect("jeton");
    let alice = services.emettre_jeton("alice", UNE_HEURE).expect("jeton");
    let bob = services.emettre_jeton("bob", UNE_HEURE).expect("jeton");
    let cle_autorite = services.cle_publique_autorite_hex();
    let adresse = demarrer_serveur(services);

    // 0. Alice provisionne son appareil : allocation d'un ID NovaDesk lié à sa
    //    clé statique, avec le jeton d'enregistrement pour le rendez-vous.
    let cle_appareil = SigningKey::from_bytes(&[21u8; 32]);
    let (appareil, jeton_enregistrement) = match aller_retour(
        adresse,
        &Request::AllocateId {
            jeton: alice.clone(),
            cle_client: cle_appareil.verifying_key().to_bytes(),
        },
    ) {
        Response::IdAlloue {
            id,
            jeton_enregistrement,
        } => (id, jeton_enregistrement),
        autre => panic!("IdAlloue attendu, obtenu {autre:?}"),
    };
    assert!(
        (100_000_000..1_000_000_000).contains(&appareil),
        "{appareil}"
    );
    let jeton_enr =
        JetonEnregistrement::from_bytes(&jeton_enregistrement).expect("jeton décodable");
    assert_eq!(jeton_enr.id, appareil);
    let cle = nd_api::auth::cle_publique_depuis_hex(&cle_autorite).expect("clé autorité");
    assert!(jeton_enr.verifier(&cle), "jeton d'enregistrement signé");

    // 1. Alice crée un groupe (elle en devient l'administratrice).
    let groupe = match aller_retour(
        adresse,
        &Request::CreateGroup {
            jeton: alice.clone(),
            nom: "Support".into(),
        },
    ) {
        Response::GroupeCree { id } => id,
        autre => panic!("GroupeCree attendu, obtenu {autre:?}"),
    };

    // 2. Ajouter un membre : bob (sans rôle sur le groupe) est refusé, la
    //    créatrice passe.
    attendre_acces_refuse(
        adresse,
        &Request::AddMember {
            jeton: bob.clone(),
            groupe,
            compte: "bob".into(),
        },
    );
    attendre_ok(
        adresse,
        &Request::AddMember {
            jeton: alice.clone(),
            groupe,
            compte: "alice".into(),
        },
    );

    // 3. Partager l'appareil au groupe (rôle Operator) : réservé au
    //    propriétaire de l'ID — bob est refusé, alice passe.
    attendre_acces_refuse(
        adresse,
        &Request::ShareDevice {
            jeton: bob.clone(),
            appareil,
            beneficiaire: Beneficiaire::Groupe(groupe),
            role: Role::Operator,
        },
    );
    attendre_ok(
        adresse,
        &Request::ShareDevice {
            jeton: alice.clone(),
            appareil,
            beneficiaire: Beneficiaire::Groupe(groupe),
            role: Role::Operator,
        },
    );

    // 4. Lister les appareils partagés : alice hérite du groupe (elle demande
    //    pour elle-même), bob (non membre) ne voit rien.
    assert_eq!(
        aller_retour(
            adresse,
            &Request::DevicesSharedWith {
                jeton: alice.clone(),
                compte: "alice".into(),
            },
        ),
        Response::Appareils(vec![(appareil, Role::Operator)])
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::DevicesSharedWith {
                jeton: bob.clone(),
                compte: "bob".into(),
            },
        ),
        Response::Appareils(Vec::new())
    );
    // ... et bob ne peut pas espionner les partages d'alice.
    attendre_acces_refuse(
        adresse,
        &Request::DevicesSharedWith {
            jeton: bob.clone(),
            compte: "alice".into(),
        },
    );

    // 5. Vérifier le rôle effectif (hérité de l'appartenance au groupe).
    assert_eq!(
        aller_retour(
            adresse,
            &Request::EffectiveRole {
                jeton: alice.clone(),
                compte: "alice".into(),
                appareil,
            },
        ),
        Response::RoleEffectif(Some(Role::Operator))
    );

    // Les groupes d'alice, avec leurs membres.
    match aller_retour(
        adresse,
        &Request::ListGroups {
            jeton: alice.clone(),
            compte: "alice".into(),
        },
    ) {
        Response::Groupes(groupes) => {
            assert_eq!(groupes.len(), 1);
            assert_eq!(groupes[0].id, groupe);
            assert_eq!(groupes[0].name, "Support");
            assert_eq!(groupes[0].members, vec!["alice".to_string()]);
        }
        autre => panic!("Groupes attendus, obtenu {autre:?}"),
    }

    // RBAC : alice ne peut pas s'auto-attribuer un rôle sur org-1 ; la racine
    // le fait, puis les permissions dérivées répondent.
    attendre_acces_refuse(
        adresse,
        &Request::AssignRole {
            jeton: alice.clone(),
            compte: "alice".into(),
            ressource: "org-1".into(),
            role: Role::Admin,
        },
    );
    attendre_ok(
        adresse,
        &Request::AssignRole {
            jeton: racine.clone(),
            compte: "alice".into(),
            ressource: "org-1".into(),
            role: Role::Admin,
        },
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::HasPermission {
                jeton: alice.clone(),
                compte: "alice".into(),
                ressource: "org-1".into(),
                permission: Permission::ManageMembers,
            },
        ),
        Response::Booleen(true)
    );
    // Refus par défaut : bob n'a aucune attribution.
    assert_eq!(
        aller_retour(
            adresse,
            &Request::HasPermission {
                jeton: bob.clone(),
                compte: "bob".into(),
                ressource: "org-1".into(),
                permission: Permission::ViewScreen,
            },
        ),
        Response::Booleen(false)
    );

    // Le carnet d'adresses est celui du compte agissant (dérivé du jeton).
    attendre_ok(
        adresse,
        &Request::AddContact {
            jeton: alice.clone(),
            id: 42,
            alias: "PC bureau".into(),
        },
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::ListContacts {
                jeton: alice.clone(),
            },
        ),
        Response::Contacts(vec![Contact {
            id: 42,
            alias: "PC bureau".into(),
        }])
    );
    assert_eq!(
        aller_retour(adresse, &Request::ListContacts { jeton: bob.clone() }),
        Response::Contacts(Vec::new())
    );

    // Mises à jour : publier un manifeste est une opération racine ; alice est
    // refusée. CheckUpdate reste anonyme (pas de jeton).
    let manifeste = UpdateManifest {
        channel: ReleaseChannel::Stable,
        latest: Version::new(2, 1, 0),
        min_supported: Version::new(1, 0, 0),
        url: "https://updates.novadesk.example/stable/2.1.0/novadesk-setup.exe".into(),
        sha256: "cafebabe".repeat(8),
        delta_from: Some(Version::new(2, 0, 0)),
    };
    attendre_acces_refuse(
        adresse,
        &Request::PublishManifest {
            jeton: alice.clone(),
            manifeste: manifeste.clone(),
        },
    );
    attendre_ok(
        adresse,
        &Request::PublishManifest {
            jeton: racine.clone(),
            manifeste: manifeste.clone(),
        },
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::CheckUpdate {
                canal: ReleaseChannel::Stable,
                version: Version::new(2, 0, 0),
            },
        ),
        Response::MiseAJour(UpdateDecision::UpdateAvailable(manifeste.clone()))
    );
    // Version passée sous min_supported : mise à jour forcée.
    assert_eq!(
        aller_retour(
            adresse,
            &Request::CheckUpdate {
                canal: ReleaseChannel::Stable,
                version: Version::new(0, 9, 0),
            },
        ),
        Response::MiseAJour(UpdateDecision::ForcedUpdate(manifeste))
    );

    // Configuration : la politique d'organisation est une opération de
    // gestion (bob est refusé, la racine passe) ; la lecture de configuration
    // effective est ouverte aux comptes authentifiés.
    attendre_acces_refuse(
        adresse,
        &Request::SetPolicy {
            jeton: bob.clone(),
            org: "acme".into(),
            cle: "require_2fa".into(),
            valeur: "true".into(),
        },
    );
    attendre_ok(
        adresse,
        &Request::SetPolicy {
            jeton: racine,
            org: "acme".into(),
            cle: "require_2fa".into(),
            valeur: "true".into(),
        },
    );
    match aller_retour(
        adresse,
        &Request::EffectiveConfig {
            jeton: alice,
            org: "acme".into(),
        },
    ) {
        Response::Config(paires) => {
            let valeur = |cle: &str| {
                paires
                    .iter()
                    .find(|(c, _)| c == cle)
                    .map(|(_, v)| v.as_str())
            };
            // Surcharge de l'org.
            assert_eq!(valeur("require_2fa"), Some("true"));
            // Défauts intégrés hérités.
            assert_eq!(valeur("allow_file_transfer"), Some("true"));
            assert_eq!(valeur("session_timeout_minutes"), Some("30"));
            // Paires triées par clé (réponse déterministe).
            let mut triees = paires.clone();
            triees.sort();
            assert_eq!(paires, triees);
        }
        autre => panic!("Config attendue, obtenu {autre:?}"),
    }

    // Jeton vide ou forgé : refusé par le protocole sur une requête
    // authentifiée (aucun jeton non signé n'est accepté).
    for mauvais in ["  ", "jeton-opaque", "nda1.00.00"] {
        assert_eq!(
            aller_retour(
                adresse,
                &Request::CreateGroup {
                    jeton: mauvais.into(),
                    nom: "X".into(),
                },
            ),
            Response::Erreur {
                message: "jeton invalide ou absent".into()
            }
        );
    }
}

#[test]
fn persistance_rouverte_depuis_le_fichier() {
    let chemin = chemin_etat("persistance");
    let _ = std::fs::remove_file(&chemin);

    // Premier serveur : état durable, toutes les mutations par le protocole.
    let services = Services::open(&chemin)
        .expect("ouverture")
        .avec_compte_racine("racine");
    let racine = services.emettre_jeton("racine", UNE_HEURE).expect("jeton");
    let carol = services.emettre_jeton("carol", UNE_HEURE).expect("jeton");
    let adresse = demarrer_serveur(services);

    // Carol provisionne son appareil (ID alloué, lié à son compte).
    let cle_appareil = SigningKey::from_bytes(&[22u8; 32]);
    let appareil = match aller_retour(
        adresse,
        &Request::AllocateId {
            jeton: carol.clone(),
            cle_client: cle_appareil.verifying_key().to_bytes(),
        },
    ) {
        Response::IdAlloue { id, .. } => id,
        autre => panic!("IdAlloue attendu, obtenu {autre:?}"),
    };

    let groupe = match aller_retour(
        adresse,
        &Request::CreateGroup {
            jeton: carol.clone(),
            nom: "Infra".into(),
        },
    ) {
        Response::GroupeCree { id } => id,
        autre => panic!("GroupeCree attendu, obtenu {autre:?}"),
    };
    attendre_ok(
        adresse,
        &Request::AddMember {
            jeton: carol.clone(),
            groupe,
            compte: "carol".into(),
        },
    );
    attendre_ok(
        adresse,
        &Request::ShareDevice {
            jeton: carol.clone(),
            appareil,
            beneficiaire: Beneficiaire::Groupe(groupe),
            role: Role::Viewer,
        },
    );
    attendre_ok(
        adresse,
        &Request::AssignRole {
            jeton: racine,
            compte: "carol".into(),
            ressource: "org-2".into(),
            role: Role::Operator,
        },
    );
    attendre_ok(
        adresse,
        &Request::AddContact {
            jeton: carol,
            id: 1234,
            alias: "Baie serveur".into(),
        },
    );

    // `Ok` reçu = état écrit : le fichier existe.
    assert!(
        chemin.exists(),
        "fichier d'état attendu : {}",
        chemin.display()
    );

    // Réouverture depuis le fichier : tout l'état durable est là.
    let rouvert = Services::open(&chemin)
        .expect("réouverture")
        .avec_compte_racine("racine");
    assert_eq!(
        rouvert.carnet.list_contacts("carol").expect("carnet"),
        vec![Contact {
            id: 1234,
            alias: "Baie serveur".into(),
        }]
    );
    let infra = rouvert.groupes.get(groupe).expect("groupe persisté");
    assert_eq!(infra.name, "Infra");
    assert_eq!(infra.members, vec!["carol".to_string()]);
    assert_eq!(
        rouvert.partages.effective_role("carol", appareil),
        Some(Role::Viewer)
    );
    assert_eq!(
        rouvert.roles.role_of("carol", "org-2"),
        Some(Role::Operator)
    );
    // La créatrice reste administratrice de son groupe après réouverture.
    assert_eq!(
        rouvert.roles.role_of("carol", &format!("groupe:{groupe}")),
        Some(Role::Admin)
    );
    // Le compteur d'ids de groupe est lui aussi durable : pas de réutilisation.
    let nouveau = rouvert.groupes.create_group("Nouveau").expect("création");
    assert!(nouveau > groupe, "id réutilisé : {nouveau} <= {groupe}");
    // L'attribution d'ID est durable : propriétaire retrouvé, jamais réémis.
    assert!(rouvert.alloc.est_proprietaire(appareil, "carol"));
    let nouvel_id = rouvert
        .alloc
        .allouer("dave", &[9u8; 32])
        .expect("allocation");
    assert_ne!(nouvel_id, appareil, "ID réattribué après redémarrage");

    // Et un second serveur branché sur l'état rouvert répond par le protocole
    // (nouvelle autorité éphémère : on émet un jeton frais pour carol).
    let carol_bis = rouvert.emettre_jeton("carol", UNE_HEURE).expect("jeton");
    let adresse2 = demarrer_serveur(rouvert);
    assert_eq!(
        aller_retour(
            adresse2,
            &Request::DevicesSharedWith {
                jeton: carol_bis,
                compte: "carol".into(),
            },
        ),
        Response::Appareils(vec![(appareil, Role::Viewer)])
    );

    let _ = std::fs::remove_file(&chemin);
}
