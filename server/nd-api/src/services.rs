//! Services de l'API applicative : état métier assemblé + serveur TCP.
//!
//! [`Services`] regroupe tous les magasins — carnet d'adresses, rôles (RBAC),
//! groupes, partages, mises à jour, politiques de configuration — et, en mode
//! durable, la persistance JSON atomique (voir [`crate::storage`]). Deux
//! constructeurs :
//!
//! - [`Services::new`] : tout en mémoire (tests, démos, premier jet) ;
//! - [`Services::open`] : charge l'état depuis un fichier, puis réécrit le
//!   fichier après chaque mutation durable réussie.
//!
//! Manifestes de mise à jour et politiques de configuration ne sont **pas**
//! persistés : ce sont des données d'exploitation, republiées au démarrage
//! par l'outillage d'administration.
//!
//! Le serveur ([`serve`]) suit le modèle de `nd-signaling` : bloquant, un
//! thread par connexion, une requête et une réponse par connexion.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::PolicyStore;
use crate::groups::GroupStore;
use crate::protocol::{read_frame, write_frame, Request, Response};
use crate::rbac::RoleStore;
use crate::sharing::SharingStore;
use crate::storage::{EtatPersistant, Partage, Storage};
use crate::update::UpdateService;
use crate::{verifier_jeton, AddressBook};

/// État métier complet de l'API applicative (thread-safe, clonable).
#[derive(Clone)]
pub struct Services {
    /// Carnet d'adresses (durable).
    pub carnet: AddressBook,
    /// Attributions de rôles RBAC (durable).
    pub roles: RoleStore,
    /// Groupes/équipes (durable).
    pub groupes: GroupStore,
    /// Partages d'appareils (durable), résolus via `groupes`.
    pub partages: SharingStore,
    /// Manifestes de mise à jour (en mémoire).
    pub maj: UpdateService,
    /// Politiques de configuration (en mémoire).
    pub politiques: PolicyStore,
    /// Stockage fichier, `None` en mode mémoire pure.
    stockage: Option<Arc<Storage>>,
}

impl Services {
    /// État entièrement en mémoire (rien n'est écrit sur disque).
    #[must_use]
    pub fn new() -> Self {
        let groupes = GroupStore::new();
        Self {
            carnet: AddressBook::new(),
            roles: RoleStore::new(),
            partages: SharingStore::new(groupes.clone()),
            groupes,
            maj: UpdateService::new(),
            politiques: PolicyStore::new(),
            stockage: None,
        }
    }

    /// Ouvre (ou crée au premier démarrage) l'état durable stocké dans le
    /// fichier `chemin`, puis persiste après chaque mutation durable.
    ///
    /// # Errors
    /// Propage les erreurs de lecture du fichier d'état ; `InvalidData` si le
    /// JSON est illisible (voir [`Storage::charger`]).
    pub fn open(chemin: impl Into<PathBuf>) -> std::io::Result<Self> {
        let stockage = Storage::new(chemin);
        let etat = stockage.charger()?.unwrap_or_default();
        let groupes = GroupStore::from_snapshot(etat.dernier_id_groupe, etat.groupes);
        let partages = SharingStore::from_snapshot(
            groupes.clone(),
            etat.partages
                .into_iter()
                .map(|p| (p.appareil, p.beneficiaire, p.role))
                .collect(),
        );
        Ok(Self {
            carnet: AddressBook::from_snapshot(etat.carnet),
            roles: RoleStore::from_snapshot(etat.roles),
            groupes,
            partages,
            maj: UpdateService::new(),
            politiques: PolicyStore::new(),
            stockage: Some(Arc::new(stockage)),
        })
    }

    /// Écrit l'état durable sur disque (sans effet en mode mémoire).
    ///
    /// # Errors
    /// Propage les erreurs d'écriture atomique (voir [`Storage::sauvegarder`]).
    pub fn persister(&self) -> std::io::Result<()> {
        let Some(stockage) = &self.stockage else {
            return Ok(());
        };
        stockage.sauvegarder(&self.instantane())
    }

    /// Instantané de l'état durable (les magasins sont verrouillés l'un après
    /// l'autre : bonne image à l'échelle du magasin, suffisante pour ce jet).
    fn instantane(&self) -> EtatPersistant {
        let (dernier_id_groupe, groupes) = self.groupes.snapshot();
        EtatPersistant {
            carnet: self.carnet.snapshot(),
            roles: self.roles.snapshot(),
            dernier_id_groupe,
            groupes,
            partages: self
                .partages
                .snapshot()
                .into_iter()
                .map(|(appareil, beneficiaire, role)| Partage {
                    appareil,
                    beneficiaire,
                    role,
                })
                .collect(),
        }
    }
}

impl Default for Services {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Serveur
// ---------------------------------------------------------------------------

/// Boucle de service (bloquante, un thread par connexion, une requête par connexion).
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
pub fn serve(listener: TcpListener, services: Services) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let services = services.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, &services);
        });
    }
    Ok(())
}

fn handle_conn(mut stream: TcpStream, services: &Services) -> std::io::Result<()> {
    let req_bytes = read_frame(&mut stream)?;
    let resp = match Request::from_bytes(&req_bytes) {
        Some(requete) => traiter_requete(services, requete),
        None => Response::Erreur {
            message: "requête invalide".into(),
        },
    };
    write_frame(&mut stream, &resp.to_bytes())
}

/// Traite une requête décodée et produit la réponse.
///
/// Les mutations durables (carnet, rôles, groupes, partages) sont persistées
/// avant de répondre `Ok` : un client qui a reçu `Ok` sait que l'état a été
/// écrit. En cas d'échec d'écriture, la mutation reste appliquée en mémoire et
/// l'erreur est signalée au client.
fn traiter_requete(services: &Services, requete: Request) -> Response {
    // Authentification minimale, comme le carnet : jeton non vide exigé pour
    // toute requête authentifiée. `CheckUpdate` est anonyme (voir `protocol`).
    if let Some(jeton) = requete.jeton() {
        if let Err(e) = verifier_jeton(jeton) {
            return erreur(&e);
        }
    }
    match requete {
        Request::AddContact { jeton, id, alias } => {
            match services.carnet.add_contact(&jeton, id, &alias) {
                Ok(()) => ok_persiste(services),
                Err(e) => erreur(&e),
            }
        }
        Request::ListContacts { jeton } => match services.carnet.list_contacts(&jeton) {
            Ok(contacts) => Response::Contacts(contacts),
            Err(e) => erreur(&e),
        },
        Request::AssignRole {
            compte,
            ressource,
            role,
            ..
        } => {
            services.roles.assign_role(&compte, &ressource, role);
            ok_persiste(services)
        }
        Request::HasPermission {
            compte,
            ressource,
            permission,
            ..
        } => Response::Booleen(
            services
                .roles
                .has_permission(&compte, &ressource, permission),
        ),
        Request::CreateGroup { nom, .. } => match services.groupes.create_group(&nom) {
            Ok(id) => match services.persister() {
                Ok(()) => Response::GroupeCree { id },
                Err(e) => erreur_persistance(&e),
            },
            Err(e) => erreur(&e),
        },
        Request::AddMember { groupe, compte, .. } => {
            match services.groupes.add_member(groupe, &compte) {
                Ok(()) => ok_persiste(services),
                Err(e) => erreur(&e),
            }
        }
        Request::ListGroups { compte, .. } => {
            Response::Groupes(services.groupes.groups_of(&compte))
        }
        Request::ShareDevice {
            appareil,
            beneficiaire,
            role,
            ..
        } => {
            services.partages.share_device(appareil, beneficiaire, role);
            ok_persiste(services)
        }
        Request::DevicesSharedWith { compte, .. } => {
            Response::Appareils(services.partages.devices_shared_with(&compte))
        }
        Request::EffectiveRole {
            compte, appareil, ..
        } => Response::RoleEffectif(services.partages.effective_role(&compte, appareil)),
        Request::CheckUpdate { canal, version } => {
            Response::MiseAJour(services.maj.check_update(canal, version))
        }
        Request::PublishManifest { manifeste, .. } => {
            // Donnée d'exploitation : non persistée (voir doc du module).
            services.maj.publish(manifeste.channel, manifeste);
            Response::Ok
        }
        Request::EffectiveConfig { org, .. } => {
            let mut paires: Vec<(String, String)> = services
                .politiques
                .effective_config(&org)
                .into_iter()
                .collect();
            // Tri par clé : réponse déterministe (l'ordre d'un HashMap ne l'est pas).
            paires.sort();
            Response::Config(paires)
        }
        Request::SetPolicy {
            org, cle, valeur, ..
        } => {
            // Donnée d'exploitation : non persistée (voir doc du module).
            services.politiques.set_policy(&org, &cle, &valeur);
            Response::Ok
        }
    }
}

/// Réponse d'erreur à partir d'une erreur métier ou d'E/S.
fn erreur(e: &dyn std::fmt::Display) -> Response {
    Response::Erreur {
        message: e.to_string(),
    }
}

/// Persiste l'état durable puis répond `Ok` — ou signale l'échec d'écriture.
fn ok_persiste(services: &Services) -> Response {
    match services.persister() {
        Ok(()) => Response::Ok,
        Err(e) => erreur_persistance(&e),
    }
}

fn erreur_persistance(e: &std::io::Error) -> Response {
    Response::Erreur {
        message: format!("échec de persistance : {e}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac::{Permission, Role};
    use crate::sharing::Beneficiaire;
    use crate::update::{ReleaseChannel, Version};
    use std::net::SocketAddr;

    /// Démarre un serveur sur un port éphémère et renvoie son adresse.
    fn demarrer(services: Services) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let adresse = listener.local_addr().expect("adresse locale");
        std::thread::spawn(move || {
            let _ = serve(listener, services);
        });
        adresse
    }

    fn aller_retour(adresse: SocketAddr, requete: &Request) -> Response {
        let mut flux = TcpStream::connect(adresse).expect("connexion");
        write_frame(&mut flux, &requete.to_bytes()).expect("écriture");
        Response::from_bytes(&read_frame(&mut flux).expect("lecture")).expect("réponse")
    }

    #[test]
    fn serveur_tcp_add_puis_list() {
        let adresse = demarrer(Services::new());

        let ajout = aller_retour(
            adresse,
            &Request::AddContact {
                jeton: "jeton-tcp".into(),
                id: 99,
                alias: "Serveur salon".into(),
            },
        );
        assert_eq!(ajout, Response::Ok);

        match aller_retour(
            adresse,
            &Request::ListContacts {
                jeton: "jeton-tcp".into(),
            },
        ) {
            Response::Contacts(contacts) => {
                assert_eq!(contacts.len(), 1);
                assert_eq!(contacts[0].id, 99);
                assert_eq!(contacts[0].alias, "Serveur salon");
            }
            autre => panic!("list TCP : contacts attendus, obtenu {autre:?}"),
        }
    }

    #[test]
    fn requete_invalide_renvoie_erreur() {
        let adresse = demarrer(Services::new());
        let mut flux = TcpStream::connect(adresse).expect("connexion");
        // Tag inconnu : le serveur répond proprement au lieu de couper.
        write_frame(&mut flux, &[250]).expect("écriture");
        match Response::from_bytes(&read_frame(&mut flux).expect("lecture")) {
            Some(Response::Erreur { message }) => assert_eq!(message, "requête invalide"),
            autre => panic!("Erreur attendue, obtenu {autre:?}"),
        }
    }

    #[test]
    fn jeton_vide_refuse_sur_les_requetes_authentifiees() {
        let services = Services::new();
        let vides = vec![
            Request::AssignRole {
                jeton: "  ".into(),
                compte: "alice".into(),
                ressource: "org-1".into(),
                role: Role::Admin,
            },
            Request::HasPermission {
                jeton: String::new(),
                compte: "alice".into(),
                ressource: "org-1".into(),
                permission: Permission::ViewScreen,
            },
            Request::CreateGroup {
                jeton: String::new(),
                nom: "Support".into(),
            },
            Request::ShareDevice {
                jeton: String::new(),
                appareil: 1,
                beneficiaire: Beneficiaire::Compte("alice".into()),
                role: Role::Viewer,
            },
            Request::EffectiveConfig {
                jeton: String::new(),
                org: "acme".into(),
            },
        ];
        for requete in vides {
            assert_eq!(
                traiter_requete(&services, requete),
                Response::Erreur {
                    message: "jeton invalide ou absent".into()
                }
            );
        }
        // CheckUpdate reste anonyme : pas de jeton, pas de refus.
        assert_eq!(
            traiter_requete(
                &services,
                Request::CheckUpdate {
                    canal: ReleaseChannel::Stable,
                    version: Version::new(1, 0, 0),
                },
            ),
            Response::MiseAJour(crate::update::UpdateDecision::UpToDate)
        );
    }

    #[test]
    fn erreur_metier_transmise_au_client() {
        let adresse = demarrer(Services::new());
        // Groupe inexistant : l'erreur métier traverse le protocole.
        assert_eq!(
            aller_retour(
                adresse,
                &Request::AddMember {
                    jeton: "jeton".into(),
                    groupe: 999,
                    compte: "alice".into(),
                },
            ),
            Response::Erreur {
                message: "groupe inconnu".into()
            }
        );
    }
}
