//! Services de l'API applicative : état métier assemblé + serveur TCP.
//!
//! [`Services`] regroupe tous les magasins — carnet d'adresses, rôles (RBAC),
//! groupes, partages, attribution d'ID, mises à jour, politiques — et, en mode
//! durable, la persistance JSON atomique (voir [`crate::storage`]). Deux
//! constructeurs :
//!
//! - [`Services::new`] : tout en mémoire (tests, démos, premier jet) ;
//! - [`Services::open`] : charge l'état depuis un fichier, puis réécrit le
//!   fichier après chaque mutation durable réussie.
//!
//! # Authentification et autorisation (plan 11)
//!
//! Toute requête non anonyme porte un **jeton applicatif signé** (voir
//! [`crate::auth`]). Le serveur en dérive le **compte agissant** — jamais d'un
//! champ de la requête — puis applique la matrice d'accès suivante :
//!
//! | Requête | Garde |
//! |---|---|
//! | `CheckUpdate` | anonyme |
//! | `AddContact`, `ListContacts` | authentifié (carnet du compte agissant) |
//! | `AllocateId` | authentifié (ID lié au compte agissant) |
//! | `CreateGroup` | authentifié ; le créateur reçoit `Admin` sur `groupe:<id>` |
//! | `AddMember` | racine, ou `ManageMembers` sur `groupe:<id>` |
//! | `AssignRole` | racine, ou `ManageMembers` sur la ressource visée |
//! | `SetPolicy` | racine, ou `ManageMembers` sur l'organisation visée |
//! | `ShareDevice` | racine, ou compte propriétaire de l'appareil (attribution) |
//! | `HasPermission`, `ListGroups`, `DevicesSharedWith`, `EffectiveRole` | compte visé lui-même, ou racine |
//! | `EffectiveConfig` | authentifié (les clients lisent leur configuration) |
//! | `PublishManifest` | racine |
//!
//! Le **compte racine** (opérateur du déploiement, voir
//! [`Services::avec_compte_racine`]) amorce le système : il attribue les
//! premiers rôles d'administration, qui se délèguent ensuite par le RBAC
//! lui-même. Sans compte racine configuré, aucune opération d'administration
//! n'est possible (fermé par défaut).
//!
//! **Émission des jetons** : pour ce jet, [`Services`] porte l'autorité
//! complète et peut émettre des jetons ([`Services::emettre_jeton`], outillage
//! et tests). En production, l'émission reviendra à `nd-accounts` (lot 09) à
//! la connexion de l'utilisateur ; `nd-api` passera alors en **vérification
//! seule** ([`Services::en_verification_seule`]) avec la seule clé publique.
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
use std::time::Duration;

use crate::allocation::AllocateurId;
use crate::auth::{self, Autorite, VerifyingKey};
use crate::config::PolicyStore;
use crate::groups::GroupStore;
use crate::protocol::{read_frame, write_frame, Request, Response};
use crate::rbac::{Permission, Role, RoleStore};
use crate::sharing::SharingStore;
use crate::storage::{EtatPersistant, Partage, Storage};
use crate::update::UpdateService;
use crate::{AddressBook, ApiError};

/// État métier complet de l'API applicative (thread-safe, clonable).
#[derive(Clone)]
pub struct Services {
    /// Carnet d'adresses (durable), indexé par compte.
    pub carnet: AddressBook,
    /// Attributions de rôles RBAC (durable).
    pub roles: RoleStore,
    /// Groupes/équipes (durable).
    pub groupes: GroupStore,
    /// Partages d'appareils (durable), résolus via `groupes`.
    pub partages: SharingStore,
    /// Attribution d'ID NovaDesk (durable).
    pub alloc: AllocateurId,
    /// Manifestes de mise à jour (en mémoire).
    pub maj: UpdateService,
    /// Politiques de configuration (en mémoire).
    pub politiques: PolicyStore,
    /// Autorité d'émission, `None` en vérification seule (lot 09 : la clé
    /// privée vit dans `nd-accounts`).
    emetteur: Option<Arc<Autorite>>,
    /// Clé publique vérifiant les jetons applicatifs entrants.
    verificateur: VerifyingKey,
    /// Compte opérateur du déploiement (toutes permissions), `None` = aucun.
    compte_racine: Option<String>,
    /// Stockage fichier, `None` en mode mémoire pure.
    stockage: Option<Arc<Storage>>,
}

impl Services {
    /// État entièrement en mémoire (rien n'est écrit sur disque), avec une
    /// autorité de signature **éphémère** propre à cette instance.
    ///
    /// # Panics
    /// Si le générateur aléatoire du système est indisponible (le serveur ne
    /// peut pas fonctionner sans clés).
    #[must_use]
    pub fn new() -> Self {
        let autorite = Autorite::generer().expect("générateur aléatoire du système indisponible");
        let alloc = AllocateurId::new().expect("générateur aléatoire du système indisponible");
        let groupes = GroupStore::new();
        Self {
            carnet: AddressBook::new(),
            roles: RoleStore::new(),
            partages: SharingStore::new(groupes.clone()),
            groupes,
            alloc,
            maj: UpdateService::new(),
            politiques: PolicyStore::new(),
            verificateur: autorite.cle_publique(),
            emetteur: Some(Arc::new(autorite)),
            compte_racine: None,
            stockage: None,
        }
    }

    /// Ouvre (ou crée au premier démarrage) l'état durable stocké dans le
    /// fichier `chemin`, puis persiste après chaque mutation durable.
    /// L'autorité de signature est éphémère : voir [`Services::avec_autorite`]
    /// pour une autorité stable entre démarrages.
    ///
    /// # Errors
    /// Propage les erreurs de lecture du fichier d'état ; `InvalidData` si le
    /// JSON est illisible (voir [`Storage::charger`]).
    pub fn open(chemin: impl Into<PathBuf>) -> std::io::Result<Self> {
        let stockage = Storage::new(chemin);
        let etat = stockage.charger()?.unwrap_or_default();
        let autorite = Autorite::generer()?;
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
            alloc: AllocateurId::from_snapshot(
                etat.allocation_compteur,
                &etat.allocation_cle_hex,
                etat.ids_emis,
            )?,
            maj: UpdateService::new(),
            politiques: PolicyStore::new(),
            verificateur: autorite.cle_publique(),
            emetteur: Some(Arc::new(autorite)),
            compte_racine: None,
            stockage: Some(Arc::new(stockage)),
        })
    }

    /// Remplace l'autorité par une autorité complète (émission + vérification),
    /// typiquement chargée d'un fichier de graine ([`Autorite::charger_ou_creer`]).
    #[must_use]
    pub fn avec_autorite(mut self, autorite: Autorite) -> Self {
        self.verificateur = autorite.cle_publique();
        self.emetteur = Some(Arc::new(autorite));
        self
    }

    /// Passe en **vérification seule** : les jetons entrants sont vérifiés
    /// avec `cle_publique`, mais cette instance n'émet plus rien — mode de
    /// production une fois l'émission confiée à `nd-accounts` (lot 09).
    #[must_use]
    pub fn en_verification_seule(mut self, cle_publique: VerifyingKey) -> Self {
        self.verificateur = cle_publique;
        self.emetteur = None;
        self
    }

    /// Déclare le compte opérateur du déploiement (toutes permissions) qui
    /// amorce les attributions de rôles.
    #[must_use]
    pub fn avec_compte_racine(mut self, compte: &str) -> Self {
        self.compte_racine = Some(compte.to_string());
        self
    }

    /// Clé publique de l'autorité (hexadécimal), à distribuer aux serveurs
    /// vérificateurs (`nd-rendezvous`, `nd-relay`) et aux journaux.
    #[must_use]
    pub fn cle_publique_autorite_hex(&self) -> String {
        hex::encode(self.verificateur.to_bytes())
    }

    /// Émet un jeton applicatif signé pour `compte`, valable `duree` à partir
    /// de maintenant (outillage, tests ; en production l'émission revient à
    /// `nd-accounts`, lot 09).
    ///
    /// # Errors
    /// `EmissionIndisponible` en mode vérification seule, `CompteVide` si le
    /// compte est vide.
    pub fn emettre_jeton(&self, compte: &str, duree: Duration) -> Result<String, ApiError> {
        if compte.trim().is_empty() {
            return Err(ApiError::CompteVide);
        }
        let emetteur = self
            .emetteur
            .as_ref()
            .ok_or(ApiError::EmissionIndisponible)?;
        Ok(emetteur.emettre_jeton_applicatif(compte, auth::maintenant_unix() + duree.as_secs()))
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
        let (allocation_compteur, allocation_cle_hex, ids_emis) = self.alloc.snapshot();
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
            allocation_compteur,
            allocation_cle_hex,
            ids_emis,
        }
    }

    // -- Gardes d'autorisation (voir la matrice du module) -------------------

    /// Compte agissant porté par un jeton applicatif valide et non expiré.
    fn compte_du_jeton(&self, jeton: &str) -> Result<String, ApiError> {
        match auth::verifier_jeton_applicatif(jeton, &self.verificateur, auth::maintenant_unix()) {
            Ok(compte) => Ok(compte),
            Err(auth::ErreurJeton::Expire) => Err(ApiError::JetonExpire),
            Err(_) => Err(ApiError::JetonInvalide),
        }
    }

    /// L'acteur est-il le compte racine du déploiement ?
    fn est_racine(&self, acteur: &str) -> bool {
        self.compte_racine.as_deref() == Some(acteur)
    }

    /// Exige le compte racine.
    fn exiger_racine(&self, acteur: &str) -> Result<(), ApiError> {
        if self.est_racine(acteur) {
            Ok(())
        } else {
            Err(ApiError::AccesRefuse)
        }
    }

    /// Exige la permission `ManageMembers` sur `ressource` (ou racine).
    fn exiger_gestion(&self, acteur: &str, ressource: &str) -> Result<(), ApiError> {
        if self.est_racine(acteur)
            || self
                .roles
                .has_permission(acteur, ressource, Permission::ManageMembers)
        {
            Ok(())
        } else {
            Err(ApiError::AccesRefuse)
        }
    }

    /// Exige que l'acteur soit le compte visé par la requête (ou racine).
    fn exiger_compte_vise(&self, acteur: &str, vise: &str) -> Result<(), ApiError> {
        if acteur == vise || self.est_racine(acteur) {
            Ok(())
        } else {
            Err(ApiError::AccesRefuse)
        }
    }

    /// Exige que l'acteur soit le propriétaire enregistré de l'appareil
    /// (attribution d'ID) — ou racine.
    fn exiger_proprietaire(&self, acteur: &str, appareil: u64) -> Result<(), ApiError> {
        if self.est_racine(acteur) || self.alloc.est_proprietaire(appareil, acteur) {
            Ok(())
        } else {
            Err(ApiError::AccesRefuse)
        }
    }
}

impl Default for Services {
    fn default() -> Self {
        Self::new()
    }
}

/// Nom de ressource RBAC d'un groupe (portée des droits de gestion du groupe).
fn ressource_groupe(id: u64) -> String {
    format!("groupe:{id}")
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
/// Le compte agissant est **dérivé du jeton signé** (voir [`crate::auth`]),
/// puis la garde d'autorisation de l'opération est vérifiée (matrice du
/// module) avant tout effet. Les mutations durables sont persistées avant de
/// répondre `Ok` : un client qui a reçu `Ok` sait que l'état a été écrit. En
/// cas d'échec d'écriture, la mutation reste appliquée en mémoire et l'erreur
/// est signalée au client.
fn traiter_requete(services: &Services, requete: Request) -> Response {
    // Seule requête anonyme : un client pas encore connecté doit pouvoir se
    // mettre à jour.
    if let Request::CheckUpdate { canal, version } = requete {
        return Response::MiseAJour(services.maj.check_update(canal, version));
    }
    // Authentification : le compte agissant vient du jeton, pas de la requête.
    let acteur = match requete.jeton() {
        Some(jeton) => match services.compte_du_jeton(jeton) {
            Ok(compte) => compte,
            Err(e) => return erreur(&e),
        },
        // Défensif : toutes les requêtes restantes portent un jeton.
        None => return erreur(&ApiError::JetonInvalide),
    };
    match requete {
        Request::AddContact { id, alias, .. } => {
            match services.carnet.add_contact(&acteur, id, &alias) {
                Ok(()) => ok_persiste(services),
                Err(e) => erreur(&e),
            }
        }
        Request::ListContacts { .. } => match services.carnet.list_contacts(&acteur) {
            Ok(contacts) => Response::Contacts(contacts),
            Err(e) => erreur(&e),
        },
        Request::AssignRole {
            compte,
            ressource,
            role,
            ..
        } => {
            if let Err(e) = services.exiger_gestion(&acteur, &ressource) {
                return erreur(&e);
            }
            services.roles.assign_role(&compte, &ressource, role);
            ok_persiste(services)
        }
        Request::HasPermission {
            compte,
            ressource,
            permission,
            ..
        } => {
            if let Err(e) = services.exiger_compte_vise(&acteur, &compte) {
                return erreur(&e);
            }
            Response::Booleen(
                services
                    .roles
                    .has_permission(&compte, &ressource, permission),
            )
        }
        Request::CreateGroup { nom, .. } => match services.groupes.create_group(&nom) {
            Ok(id) => {
                // Le créateur administre son groupe (délégable via AssignRole).
                services
                    .roles
                    .assign_role(&acteur, &ressource_groupe(id), Role::Admin);
                match services.persister() {
                    Ok(()) => Response::GroupeCree { id },
                    Err(e) => erreur_persistance(&e),
                }
            }
            Err(e) => erreur(&e),
        },
        Request::AddMember { groupe, compte, .. } => {
            if let Err(e) = services.exiger_gestion(&acteur, &ressource_groupe(groupe)) {
                return erreur(&e);
            }
            match services.groupes.add_member(groupe, &compte) {
                Ok(()) => ok_persiste(services),
                Err(e) => erreur(&e),
            }
        }
        Request::ListGroups { compte, .. } => {
            if let Err(e) = services.exiger_compte_vise(&acteur, &compte) {
                return erreur(&e);
            }
            Response::Groupes(services.groupes.groups_of(&compte))
        }
        Request::ShareDevice {
            appareil,
            beneficiaire,
            role,
            ..
        } => {
            if let Err(e) = services.exiger_proprietaire(&acteur, appareil) {
                return erreur(&e);
            }
            services.partages.share_device(appareil, beneficiaire, role);
            ok_persiste(services)
        }
        Request::DevicesSharedWith { compte, .. } => {
            if let Err(e) = services.exiger_compte_vise(&acteur, &compte) {
                return erreur(&e);
            }
            Response::Appareils(services.partages.devices_shared_with(&compte))
        }
        Request::EffectiveRole {
            compte, appareil, ..
        } => {
            if let Err(e) = services.exiger_compte_vise(&acteur, &compte) {
                return erreur(&e);
            }
            Response::RoleEffectif(services.partages.effective_role(&compte, appareil))
        }
        // Traitée en tête de fonction (anonyme) ; inatteignable ici.
        Request::CheckUpdate { canal, version } => {
            Response::MiseAJour(services.maj.check_update(canal, version))
        }
        Request::PublishManifest { manifeste, .. } => {
            if let Err(e) = services.exiger_racine(&acteur) {
                return erreur(&e);
            }
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
            if let Err(e) = services.exiger_gestion(&acteur, &org) {
                return erreur(&e);
            }
            // Donnée d'exploitation : non persistée (voir doc du module).
            services.politiques.set_policy(&org, &cle, &valeur);
            Response::Ok
        }
        Request::AllocateId { cle_client, .. } => {
            // La clé statique doit être un point Ed25519 valide : elle scelle
            // le jeton d'enregistrement exigé par le rendez-vous.
            let Ok(cle) = auth::VerifyingKey::from_bytes(&cle_client) else {
                return Response::Erreur {
                    message: "clé statique du client invalide".into(),
                };
            };
            let Some(emetteur) = &services.emetteur else {
                // Lot 09 : en vérification seule, l'attribution sera portée
                // par le détenteur de la clé privée (nd-accounts).
                return erreur(&ApiError::EmissionIndisponible);
            };
            match services.alloc.allouer(&acteur, &cle_client) {
                Ok(id) => {
                    let jeton = emetteur.emettre_jeton_enregistrement(id, &cle);
                    match services.persister() {
                        Ok(()) => Response::IdAlloue {
                            id,
                            jeton_enregistrement: jeton.to_bytes(),
                        },
                        Err(e) => erreur_persistance(&e),
                    }
                }
                Err(e) => erreur(&e),
            }
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
    use crate::auth::JetonEnregistrement;
    use crate::rbac::{Permission, Role};
    use crate::sharing::Beneficiaire;
    use crate::update::{ReleaseChannel, Version};
    use std::net::SocketAddr;

    /// Une heure : durée de vie des jetons de test.
    const UNE_HEURE: Duration = Duration::from_secs(3600);

    /// Services mémoire avec compte racine, plus trois jetons prêts à l'emploi.
    fn services_racine() -> (Services, String, String, String) {
        let services = Services::new().avec_compte_racine("racine");
        let racine = services.emettre_jeton("racine", UNE_HEURE).expect("racine");
        let alice = services.emettre_jeton("alice", UNE_HEURE).expect("alice");
        let bob = services.emettre_jeton("bob", UNE_HEURE).expect("bob");
        (services, racine, alice, bob)
    }

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
        let (services, _, alice, _) = services_racine();
        let adresse = demarrer(services);

        let ajout = aller_retour(
            adresse,
            &Request::AddContact {
                jeton: alice.clone(),
                id: 99,
                alias: "Serveur salon".into(),
            },
        );
        assert_eq!(ajout, Response::Ok);

        match aller_retour(adresse, &Request::ListContacts { jeton: alice }) {
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
    fn jeton_invalide_refuse_sur_les_requetes_authentifiees() {
        let services = Services::new();
        // Vide, opaque, mal formé : tous refusés avant d'atteindre le métier.
        for mauvais in ["", "  ", "jeton-opaque", "nda1.zz.zz"] {
            let requetes = vec![
                Request::AssignRole {
                    jeton: mauvais.into(),
                    compte: "alice".into(),
                    ressource: "org-1".into(),
                    role: Role::Admin,
                },
                Request::HasPermission {
                    jeton: mauvais.into(),
                    compte: "alice".into(),
                    ressource: "org-1".into(),
                    permission: Permission::ViewScreen,
                },
                Request::CreateGroup {
                    jeton: mauvais.into(),
                    nom: "Support".into(),
                },
                Request::ShareDevice {
                    jeton: mauvais.into(),
                    appareil: 1,
                    beneficiaire: Beneficiaire::Compte("alice".into()),
                    role: Role::Viewer,
                },
                Request::EffectiveConfig {
                    jeton: mauvais.into(),
                    org: "acme".into(),
                },
                Request::AllocateId {
                    jeton: mauvais.into(),
                    cle_client: [0u8; 32],
                },
            ];
            for requete in requetes {
                assert_eq!(
                    traiter_requete(&services, requete),
                    Response::Erreur {
                        message: "jeton invalide ou absent".into()
                    }
                );
            }
        }
        // Jeton signé par une AUTRE autorité : refusé aussi.
        let autre = Services::new();
        let etranger = autre.emettre_jeton("alice", UNE_HEURE).expect("jeton");
        assert_eq!(
            traiter_requete(&services, Request::ListContacts { jeton: etranger },),
            Response::Erreur {
                message: "jeton invalide ou absent".into()
            }
        );
        // Jeton expiré (durée nulle : déjà expiré à l'émission) : refusé.
        let expire = services
            .emettre_jeton("alice", Duration::ZERO)
            .expect("jeton");
        assert_eq!(
            traiter_requete(&services, Request::ListContacts { jeton: expire }),
            Response::Erreur {
                message: "jeton expiré".into()
            }
        );
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
    fn compte_agissant_derive_du_jeton_pas_de_la_requete() {
        let (services, _, alice, bob) = services_racine();
        // Alice remplit SON carnet ; un second jeton d'alice voit le même
        // carnet ; le carnet de bob reste vide.
        assert_eq!(
            traiter_requete(
                &services,
                Request::AddContact {
                    jeton: alice,
                    id: 7,
                    alias: "NAS".into(),
                },
            ),
            Response::Ok
        );
        let alice_bis = services.emettre_jeton("alice", UNE_HEURE).expect("jeton");
        match traiter_requete(&services, Request::ListContacts { jeton: alice_bis }) {
            Response::Contacts(contacts) => assert_eq!(contacts.len(), 1),
            autre => panic!("Contacts attendus, obtenu {autre:?}"),
        }
        match traiter_requete(&services, Request::ListContacts { jeton: bob }) {
            Response::Contacts(contacts) => assert!(contacts.is_empty()),
            autre => panic!("Contacts attendus, obtenu {autre:?}"),
        }
    }

    #[test]
    fn rbac_operations_d_administration_refusees_sans_role() {
        let (services, racine, alice, bob) = services_racine();
        let refuse = Response::Erreur {
            message: "accès refusé".into(),
        };

        // Sans rôle : AssignRole, SetPolicy et PublishManifest sont refusés.
        assert_eq!(
            traiter_requete(
                &services,
                Request::AssignRole {
                    jeton: alice.clone(),
                    compte: "alice".into(),
                    ressource: "org-1".into(),
                    role: Role::Admin,
                },
            ),
            refuse
        );
        assert_eq!(
            traiter_requete(
                &services,
                Request::SetPolicy {
                    jeton: alice.clone(),
                    org: "acme".into(),
                    cle: "require_2fa".into(),
                    valeur: "true".into(),
                },
            ),
            refuse
        );
        assert_eq!(
            traiter_requete(
                &services,
                Request::PublishManifest {
                    jeton: alice.clone(),
                    manifeste: crate::update::UpdateManifest {
                        channel: ReleaseChannel::Stable,
                        latest: Version::new(1, 0, 0),
                        min_supported: Version::new(1, 0, 0),
                        url: "https://updates.novadesk.example/x".into(),
                        sha256: "00".repeat(32),
                        delta_from: None,
                    },
                },
            ),
            refuse
        );

        // La racine attribue Admin sur acme à alice : alice gère alors acme...
        assert_eq!(
            traiter_requete(
                &services,
                Request::AssignRole {
                    jeton: racine,
                    compte: "alice".into(),
                    ressource: "acme".into(),
                    role: Role::Admin,
                },
            ),
            Response::Ok
        );
        assert_eq!(
            traiter_requete(
                &services,
                Request::SetPolicy {
                    jeton: alice.clone(),
                    org: "acme".into(),
                    cle: "require_2fa".into(),
                    valeur: "true".into(),
                },
            ),
            Response::Ok
        );
        // ... et peut déléguer sur acme, mais toujours pas ailleurs.
        assert_eq!(
            traiter_requete(
                &services,
                Request::AssignRole {
                    jeton: alice.clone(),
                    compte: "bob".into(),
                    ressource: "acme".into(),
                    role: Role::Viewer,
                },
            ),
            Response::Ok
        );
        assert_eq!(
            traiter_requete(
                &services,
                Request::AssignRole {
                    jeton: alice,
                    compte: "bob".into(),
                    ressource: "org-2".into(),
                    role: Role::Admin,
                },
            ),
            refuse
        );
        // Un Viewer sur acme ne gère rien (ManageMembers requis).
        assert_eq!(
            traiter_requete(
                &services,
                Request::SetPolicy {
                    jeton: bob,
                    org: "acme".into(),
                    cle: "x".into(),
                    valeur: "y".into(),
                },
            ),
            refuse
        );
    }

    #[test]
    fn createur_de_groupe_le_gere_les_autres_non() {
        let (services, _, alice, bob) = services_racine();
        let groupe = match traiter_requete(
            &services,
            Request::CreateGroup {
                jeton: alice.clone(),
                nom: "Support".into(),
            },
        ) {
            Response::GroupeCree { id } => id,
            autre => panic!("GroupeCree attendu, obtenu {autre:?}"),
        };
        // Créatrice : peut ajouter un membre.
        assert_eq!(
            traiter_requete(
                &services,
                Request::AddMember {
                    jeton: alice,
                    groupe,
                    compte: "carol".into(),
                },
            ),
            Response::Ok
        );
        // Tiers sans rôle sur le groupe : refusé.
        assert_eq!(
            traiter_requete(
                &services,
                Request::AddMember {
                    jeton: bob,
                    groupe,
                    compte: "bob".into(),
                },
            ),
            Response::Erreur {
                message: "accès refusé".into()
            }
        );
    }

    #[test]
    fn lectures_de_comptes_reservees_au_compte_vise_ou_racine() {
        let (services, racine, alice, bob) = services_racine();
        let refuse = Response::Erreur {
            message: "accès refusé".into(),
        };
        // Bob ne lit pas les données d'alice...
        for requete in [
            Request::HasPermission {
                jeton: bob.clone(),
                compte: "alice".into(),
                ressource: "org-1".into(),
                permission: Permission::ViewScreen,
            },
            Request::ListGroups {
                jeton: bob.clone(),
                compte: "alice".into(),
            },
            Request::DevicesSharedWith {
                jeton: bob.clone(),
                compte: "alice".into(),
            },
            Request::EffectiveRole {
                jeton: bob,
                compte: "alice".into(),
                appareil: 1,
            },
        ] {
            assert_eq!(traiter_requete(&services, requete), refuse);
        }
        // ... alice lit les siennes, la racine lit tout.
        assert_eq!(
            traiter_requete(
                &services,
                Request::ListGroups {
                    jeton: alice,
                    compte: "alice".into(),
                },
            ),
            Response::Groupes(Vec::new())
        );
        assert_eq!(
            traiter_requete(
                &services,
                Request::DevicesSharedWith {
                    jeton: racine,
                    compte: "alice".into(),
                },
            ),
            Response::Appareils(Vec::new())
        );
    }

    #[test]
    fn allocation_d_id_liee_au_compte_et_partage_reserve_au_proprietaire() {
        let (services, _, alice, bob) = services_racine();
        let cle_client = crate::auth::SigningKey::from_bytes(&[5u8; 32]);

        // Alice alloue un ID : 9 chiffres + jeton d'enregistrement vérifiable.
        let (id, jeton_octets) = match traiter_requete(
            &services,
            Request::AllocateId {
                jeton: alice.clone(),
                cle_client: cle_client.verifying_key().to_bytes(),
            },
        ) {
            Response::IdAlloue {
                id,
                jeton_enregistrement,
            } => (id, jeton_enregistrement),
            autre => panic!("IdAlloue attendu, obtenu {autre:?}"),
        };
        assert!((100_000_000..1_000_000_000).contains(&id), "{id}");
        let jeton_enr = JetonEnregistrement::from_bytes(&jeton_octets).expect("jeton décodable");
        assert_eq!(jeton_enr.id, id);
        assert_eq!(jeton_enr.cle_client, cle_client.verifying_key().to_bytes());
        assert!(jeton_enr.verifier(&services.verificateur));
        assert!(services.alloc.est_proprietaire(id, "alice"));

        // Une seconde allocation donne un autre ID (jamais réattribué).
        match traiter_requete(
            &services,
            Request::AllocateId {
                jeton: alice.clone(),
                cle_client: cle_client.verifying_key().to_bytes(),
            },
        ) {
            Response::IdAlloue { id: second, .. } => assert_ne!(second, id),
            autre => panic!("IdAlloue attendu, obtenu {autre:?}"),
        }

        // Clé statique invalide (octets qui ne se décompressent pas en point
        // Ed25519 — vérifié : `[2; 32]` n'est pas sur la courbe) : refusée.
        assert_eq!(
            traiter_requete(
                &services,
                Request::AllocateId {
                    jeton: alice.clone(),
                    cle_client: [2u8; 32],
                },
            ),
            Response::Erreur {
                message: "clé statique du client invalide".into()
            }
        );

        // Le partage de l'appareil est réservé à sa propriétaire.
        assert_eq!(
            traiter_requete(
                &services,
                Request::ShareDevice {
                    jeton: bob,
                    appareil: id,
                    beneficiaire: Beneficiaire::Compte("bob".into()),
                    role: Role::Viewer,
                },
            ),
            Response::Erreur {
                message: "accès refusé".into()
            }
        );
        assert_eq!(
            traiter_requete(
                &services,
                Request::ShareDevice {
                    jeton: alice,
                    appareil: id,
                    beneficiaire: Beneficiaire::Compte("bob".into()),
                    role: Role::Viewer,
                },
            ),
            Response::Ok
        );
    }

    #[test]
    fn erreur_metier_transmise_au_client() {
        let (services, racine, _, _) = services_racine();
        let adresse = demarrer(services);
        // Groupe inexistant : l'erreur métier traverse le protocole (la racine
        // passe la garde d'accès, le métier refuse ensuite).
        assert_eq!(
            aller_retour(
                adresse,
                &Request::AddMember {
                    jeton: racine,
                    groupe: 999,
                    compte: "alice".into(),
                },
            ),
            Response::Erreur {
                message: "groupe inconnu".into()
            }
        );
    }

    #[test]
    fn verification_seule_refuse_l_emission_mais_verifie() {
        let services = Services::new();
        let jeton = services.emettre_jeton("alice", UNE_HEURE).expect("jeton");
        // Une instance en vérification seule (même clé publique) accepte le
        // jeton émis ailleurs — point de jonction nd-accounts (lot 09)...
        let verifieur = Services::new()
            .en_verification_seule(services.verificateur)
            .avec_compte_racine("racine");
        match traiter_requete(
            &verifieur,
            Request::ListContacts {
                jeton: jeton.clone(),
            },
        ) {
            Response::Contacts(contacts) => assert!(contacts.is_empty()),
            autre => panic!("Contacts attendus, obtenu {autre:?}"),
        }
        // ... mais n'émet plus ni jetons ni IDs.
        assert_eq!(
            verifieur.emettre_jeton("bob", UNE_HEURE),
            Err(ApiError::EmissionIndisponible)
        );
        assert_eq!(
            traiter_requete(
                &verifieur,
                Request::AllocateId {
                    jeton,
                    cle_client: crate::auth::SigningKey::from_bytes(&[6u8; 32])
                        .verifying_key()
                        .to_bytes(),
                },
            ),
            Response::Erreur {
                message: "émission de jeton indisponible (vérification seule)".into()
            }
        );
    }
}
