//! Tests d'intégration TCP de `nd-api` : un serveur réel dans un thread, un
//! client TCP qui déroule un scénario réaliste bout en bout par le protocole
//! (trames `u32` BE), puis la persistance : rouvrir le fichier d'état et
//! retrouver tout ce qui a été écrit.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;

use nd_api::protocol::{read_frame, write_frame, Request, Response};
use nd_api::rbac::{Permission, Role};
use nd_api::services::{serve, Services};
use nd_api::sharing::Beneficiaire;
use nd_api::update::{ReleaseChannel, UpdateDecision, UpdateManifest, Version};
use nd_api::Contact;

/// Jeton de session des tests (tout jeton non vide est accepté pour ce jet).
const JETON: &str = "jeton-integration";

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

/// Chemin d'état unique dans le répertoire temporaire du système.
fn chemin_etat(nom: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nd-api-int-{}-{nom}.json", std::process::id()))
}

#[test]
fn scenario_complet_par_le_protocole() {
    let adresse = demarrer_serveur(Services::new());
    let appareil = 555_000_111;

    // 1. Créer un groupe.
    let groupe = match aller_retour(
        adresse,
        &Request::CreateGroup {
            jeton: JETON.into(),
            nom: "Support".into(),
        },
    ) {
        Response::GroupeCree { id } => id,
        autre => panic!("GroupeCree attendu, obtenu {autre:?}"),
    };

    // 2. Ajouter un membre.
    attendre_ok(
        adresse,
        &Request::AddMember {
            jeton: JETON.into(),
            groupe,
            compte: "alice".into(),
        },
    );

    // 3. Partager un appareil au groupe (rôle Operator).
    attendre_ok(
        adresse,
        &Request::ShareDevice {
            jeton: JETON.into(),
            appareil,
            beneficiaire: Beneficiaire::Groupe(groupe),
            role: Role::Operator,
        },
    );

    // 4. Lister les appareils partagés d'un membre : alice hérite du groupe,
    //    bob (non membre) ne voit rien.
    assert_eq!(
        aller_retour(
            adresse,
            &Request::DevicesSharedWith {
                jeton: JETON.into(),
                compte: "alice".into(),
            },
        ),
        Response::Appareils(vec![(appareil, Role::Operator)])
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::DevicesSharedWith {
                jeton: JETON.into(),
                compte: "bob".into(),
            },
        ),
        Response::Appareils(Vec::new())
    );

    // 5. Vérifier le rôle effectif (hérité de l'appartenance au groupe).
    assert_eq!(
        aller_retour(
            adresse,
            &Request::EffectiveRole {
                jeton: JETON.into(),
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
            jeton: JETON.into(),
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

    // RBAC : attribution d'un rôle puis vérification des permissions dérivées.
    attendre_ok(
        adresse,
        &Request::AssignRole {
            jeton: JETON.into(),
            compte: "alice".into(),
            ressource: "org-1".into(),
            role: Role::Admin,
        },
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::HasPermission {
                jeton: JETON.into(),
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
                jeton: JETON.into(),
                compte: "bob".into(),
                ressource: "org-1".into(),
                permission: Permission::ViewScreen,
            },
        ),
        Response::Booleen(false)
    );

    // Le carnet d'adresses fonctionne toujours par le même protocole.
    attendre_ok(
        adresse,
        &Request::AddContact {
            jeton: JETON.into(),
            id: 42,
            alias: "PC bureau".into(),
        },
    );
    assert_eq!(
        aller_retour(
            adresse,
            &Request::ListContacts {
                jeton: JETON.into(),
            },
        ),
        Response::Contacts(vec![Contact {
            id: 42,
            alias: "PC bureau".into(),
        }])
    );

    // Mises à jour : publier un manifeste puis l'interroger (CheckUpdate est
    // anonyme : pas de jeton).
    let manifeste = UpdateManifest {
        channel: ReleaseChannel::Stable,
        latest: Version::new(2, 1, 0),
        min_supported: Version::new(1, 0, 0),
        url: "https://updates.novadesk.example/stable/2.1.0/novadesk-setup.exe".into(),
        sha256: "cafebabe".repeat(8),
        delta_from: Some(Version::new(2, 0, 0)),
    };
    attendre_ok(
        adresse,
        &Request::PublishManifest {
            jeton: JETON.into(),
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

    // Configuration : surcharge d'organisation puis configuration effective.
    attendre_ok(
        adresse,
        &Request::SetPolicy {
            jeton: JETON.into(),
            org: "acme".into(),
            cle: "require_2fa".into(),
            valeur: "true".into(),
        },
    );
    match aller_retour(
        adresse,
        &Request::EffectiveConfig {
            jeton: JETON.into(),
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

    // Jeton vide : refusé par le protocole sur une requête authentifiée.
    assert_eq!(
        aller_retour(
            adresse,
            &Request::CreateGroup {
                jeton: "  ".into(),
                nom: "X".into(),
            },
        ),
        Response::Erreur {
            message: "jeton invalide ou absent".into()
        }
    );
}

#[test]
fn persistance_rouverte_depuis_le_fichier() {
    let chemin = chemin_etat("persistance");
    let _ = std::fs::remove_file(&chemin);
    let appareil = 777_000_042;

    // Premier serveur : état durable, toutes les mutations par le protocole.
    let adresse = demarrer_serveur(Services::open(&chemin).expect("ouverture"));
    let groupe = match aller_retour(
        adresse,
        &Request::CreateGroup {
            jeton: JETON.into(),
            nom: "Infra".into(),
        },
    ) {
        Response::GroupeCree { id } => id,
        autre => panic!("GroupeCree attendu, obtenu {autre:?}"),
    };
    attendre_ok(
        adresse,
        &Request::AddMember {
            jeton: JETON.into(),
            groupe,
            compte: "carol".into(),
        },
    );
    attendre_ok(
        adresse,
        &Request::ShareDevice {
            jeton: JETON.into(),
            appareil,
            beneficiaire: Beneficiaire::Groupe(groupe),
            role: Role::Viewer,
        },
    );
    attendre_ok(
        adresse,
        &Request::AssignRole {
            jeton: JETON.into(),
            compte: "carol".into(),
            ressource: "org-2".into(),
            role: Role::Operator,
        },
    );
    attendre_ok(
        adresse,
        &Request::AddContact {
            jeton: JETON.into(),
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
    let rouvert = Services::open(&chemin).expect("réouverture");
    assert_eq!(
        rouvert.carnet.list_contacts(JETON).expect("carnet"),
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
    // Le compteur d'ids de groupe est lui aussi durable : pas de réutilisation.
    let nouveau = rouvert.groupes.create_group("Nouveau").expect("création");
    assert!(nouveau > groupe, "id réutilisé : {nouveau} <= {groupe}");

    // Et un second serveur branché sur l'état rouvert répond par le protocole.
    let adresse2 = demarrer_serveur(rouvert);
    assert_eq!(
        aller_retour(
            adresse2,
            &Request::DevicesSharedWith {
                jeton: JETON.into(),
                compte: "carol".into(),
            },
        ),
        Response::Appareils(vec![(appareil, Role::Viewer)])
    );

    let _ = std::fs::remove_file(&chemin);
}
