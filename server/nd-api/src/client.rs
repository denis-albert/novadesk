//! Client TCP de l'API applicative — pendant de [`crate::services::serve`].
//!
//! [`ApiClient`] parle le protocole de [`crate::protocol`] (trames `u32` BE,
//! [`Request`]/[`Response`]) et offre au-dessus une **API synchrone
//! ergonomique** : une méthode par opération, des types métier en entrée et en
//! sortie (jamais d'octets bruts), et des erreurs françaises ([`ErreurClient`]).
//! Il est destiné à `nd-ffi`, qui l'expose à l'application.
//!
//! # Modèle de connexion
//!
//! Comme le serveur (un thread par connexion, **une** requête et **une**
//! réponse par connexion), chaque appel ouvre une connexion TCP neuve, envoie
//! sa trame de requête, lit la trame de réponse, puis referme.
//! [`ApiClient::connect`] ne fait donc que mémoriser l'adresse (résolue) et le
//! **jeton applicatif** du compte agissant : aucun flux persistant n'est tenu.
//! Le jeton est joint à chaque requête authentifiée ; seul
//! [`ApiClient::check_update`] s'en passe (requête volontairement anonyme).
//!
//! # Erreurs
//!
//! Toute méthode renvoie [`ErreurClient`] : échec réseau ([`ErreurClient::Io`]),
//! réponse illisible ou d'un type inattendu, ou **erreur métier du serveur**
//! ([`ErreurClient::Serveur`]) dont le message français (« accès refusé »,
//! « jeton invalide ou absent », « groupe inconnu »...) est transmis tel quel.

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};

use crate::groups::Group;
use crate::protocol::{read_frame, write_frame, Request, Response};
use crate::rbac::{Permission, Role};
use crate::sharing::Beneficiaire;
use crate::update::{ReleaseChannel, UpdateDecision, UpdateManifest, Version};
use crate::Contact;

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreur d'un appel de [`ApiClient`].
#[derive(Debug)]
pub enum ErreurClient {
    /// Échec réseau : connexion, écriture ou lecture de la trame.
    Io(io::Error),
    /// Réponse reçue mais indécodable (trame corrompue ou tag inconnu).
    ReponseIllisible,
    /// Réponse décodée mais d'un type incompatible avec la requête émise.
    ReponseInattendue,
    /// Erreur métier ou d'autorisation renvoyée par le serveur — le message
    /// français est celui produit par le serveur, transmis sans retouche.
    Serveur(String),
}

impl fmt::Display for ErreurClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurClient::Io(e) => write!(f, "erreur réseau : {e}"),
            ErreurClient::ReponseIllisible => write!(f, "réponse du serveur illisible"),
            ErreurClient::ReponseInattendue => write!(f, "réponse inattendue du serveur"),
            ErreurClient::Serveur(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ErreurClient {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ErreurClient::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ErreurClient {
    fn from(e: io::Error) -> Self {
        ErreurClient::Io(e)
    }
}

/// Convertit une réponse non conforme en erreur : un [`Response::Erreur`]
/// devient [`ErreurClient::Serveur`] (message transmis), toute autre réponse
/// [`ErreurClient::ReponseInattendue`].
fn erreur_reponse(reponse: Response) -> ErreurClient {
    match reponse {
        Response::Erreur { message } => ErreurClient::Serveur(message),
        _ => ErreurClient::ReponseInattendue,
    }
}

// ---------------------------------------------------------------------------
// Résultat d'allocation d'ID
// ---------------------------------------------------------------------------

/// Résultat d'une allocation d'ID (voir [`ApiClient::allocate_id`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdAlloue {
    /// ID NovaDesk alloué (9 chiffres), lié au compte du jeton.
    pub id: u64,
    /// Jeton d'enregistrement signé et sérialisé (voir
    /// [`crate::auth::JetonEnregistrement`]), à présenter au rendez-vous.
    pub jeton_enregistrement: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Client synchrone de l'API applicative NovaDesk.
///
/// Construit avec [`ApiClient::connect`] (adresse + jeton applicatif signé),
/// puis chaque méthode déroule un aller-retour requête/réponse par le
/// protocole. Clonable (état léger : adresse + jeton) et utilisable depuis
/// plusieurs fils, chaque appel étant indépendant.
#[derive(Clone)]
pub struct ApiClient {
    /// Adresse du serveur, résolue à la construction.
    adresse: SocketAddr,
    /// Jeton applicatif du compte agissant (secret porteur).
    jeton: String,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Le jeton est un secret porteur : jamais dans les journaux.
        f.debug_struct("ApiClient")
            .field("adresse", &self.adresse)
            .field("jeton", &"<masqué>")
            .finish()
    }
}

impl ApiClient {
    /// Prépare un client vers `adresse`, authentifié par `jeton` (jeton
    /// applicatif signé du compte agissant).
    ///
    /// L'adresse est résolue immédiatement, mais aucune connexion n'est ouverte
    /// (le protocole est sans état : une connexion par requête).
    ///
    /// # Errors
    /// [`ErreurClient::Io`] si l'adresse est irrésoluble ou introuvable.
    pub fn connect(
        adresse: impl ToSocketAddrs,
        jeton: impl Into<String>,
    ) -> Result<Self, ErreurClient> {
        let adresse = adresse
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "adresse introuvable"))?;
        Ok(Self {
            adresse,
            jeton: jeton.into(),
        })
    }

    // -- Carnet d'adresses ---------------------------------------------------

    /// Ajoute (ou renomme) un contact du carnet du compte agissant.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (jeton, alias vide...).
    pub fn add_contact(&self, id: u64, alias: &str) -> Result<(), ErreurClient> {
        self.attendre_ok(&Request::AddContact {
            jeton: self.jeton.clone(),
            id,
            alias: alias.to_string(),
        })
    }

    /// Liste les contacts du carnet du compte agissant.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur.
    pub fn list_contacts(&self) -> Result<Vec<Contact>, ErreurClient> {
        match self.echanger(&Request::ListContacts {
            jeton: self.jeton.clone(),
        })? {
            Response::Contacts(contacts) => Ok(contacts),
            autre => Err(erreur_reponse(autre)),
        }
    }

    // -- Groupes / équipes ---------------------------------------------------

    /// Crée un groupe vide et renvoie son id (le créateur en devient
    /// administrateur).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (jeton, nom vide...).
    pub fn create_group(&self, nom: &str) -> Result<u64, ErreurClient> {
        match self.echanger(&Request::CreateGroup {
            jeton: self.jeton.clone(),
            nom: nom.to_string(),
        })? {
            Response::GroupeCree { id } => Ok(id),
            autre => Err(erreur_reponse(autre)),
        }
    }

    /// Ajoute `compte` au groupe `groupe` (idempotent).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé, groupe
    /// inconnu...).
    pub fn add_member(&self, groupe: u64, compte: &str) -> Result<(), ErreurClient> {
        self.attendre_ok(&Request::AddMember {
            jeton: self.jeton.clone(),
            groupe,
            compte: compte.to_string(),
        })
    }

    /// Groupes dont `compte` est membre (réservé au compte lui-même ou à la
    /// racine), avec leurs membres, triés par id.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn list_groups(&self, compte: &str) -> Result<Vec<Group>, ErreurClient> {
        match self.echanger(&Request::ListGroups {
            jeton: self.jeton.clone(),
            compte: compte.to_string(),
        })? {
            Response::Groupes(groupes) => Ok(groupes),
            autre => Err(erreur_reponse(autre)),
        }
    }

    // -- Partage d'appareils -------------------------------------------------

    /// Partage `appareil` avec `beneficiaire` au rôle `role` (réservé au
    /// propriétaire enregistré de l'ID, ou à la racine). Repartager met le rôle
    /// à jour.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn share_device(
        &self,
        appareil: u64,
        beneficiaire: Beneficiaire,
        role: Role,
    ) -> Result<(), ErreurClient> {
        self.attendre_ok(&Request::ShareDevice {
            jeton: self.jeton.clone(),
            appareil,
            beneficiaire,
            role,
        })
    }

    /// Appareils partagés avec `compte` (directement ou via ses groupes), avec
    /// le rôle effectif de chacun, triés par id. Réservé au compte lui-même ou
    /// à la racine.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn devices_shared_with(&self, compte: &str) -> Result<Vec<(u64, Role)>, ErreurClient> {
        match self.echanger(&Request::DevicesSharedWith {
            jeton: self.jeton.clone(),
            compte: compte.to_string(),
        })? {
            Response::Appareils(appareils) => Ok(appareils),
            autre => Err(erreur_reponse(autre)),
        }
    }

    // -- Rôles (RBAC) --------------------------------------------------------

    /// Attribue `role` à `compte` sur `ressource` (racine, ou `ManageMembers`
    /// sur la ressource visée).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn assign_role(
        &self,
        compte: &str,
        ressource: &str,
        role: Role,
    ) -> Result<(), ErreurClient> {
        self.attendre_ok(&Request::AssignRole {
            jeton: self.jeton.clone(),
            compte: compte.to_string(),
            ressource: ressource.to_string(),
            role,
        })
    }

    /// Rôle effectif de `compte` sur `appareil` (`None` si l'appareil ne lui est
    /// pas partagé). Réservé au compte lui-même ou à la racine.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn effective_role(
        &self,
        compte: &str,
        appareil: u64,
    ) -> Result<Option<Role>, ErreurClient> {
        match self.echanger(&Request::EffectiveRole {
            jeton: self.jeton.clone(),
            compte: compte.to_string(),
            appareil,
        })? {
            Response::RoleEffectif(role) => Ok(role),
            autre => Err(erreur_reponse(autre)),
        }
    }

    /// `compte` possède-t-il `permission` sur `ressource` (via son rôle
    /// attribué) ? Réservé au compte lui-même ou à la racine.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn has_permission(
        &self,
        compte: &str,
        ressource: &str,
        permission: Permission,
    ) -> Result<bool, ErreurClient> {
        match self.echanger(&Request::HasPermission {
            jeton: self.jeton.clone(),
            compte: compte.to_string(),
            ressource: ressource.to_string(),
            permission,
        })? {
            Response::Booleen(valeur) => Ok(valeur),
            autre => Err(erreur_reponse(autre)),
        }
    }

    // -- Mises à jour --------------------------------------------------------

    /// Interroge le canal `canal` avec la version courante `version`.
    /// **Anonyme** : n'envoie pas le jeton (un client pas encore connecté doit
    /// pouvoir se mettre à jour).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou réponse inattendue.
    pub fn check_update(
        &self,
        canal: ReleaseChannel,
        version: Version,
    ) -> Result<UpdateDecision, ErreurClient> {
        match self.echanger(&Request::CheckUpdate { canal, version })? {
            Response::MiseAJour(decision) => Ok(decision),
            autre => Err(erreur_reponse(autre)),
        }
    }

    /// Publie (ou remplace) le manifeste de son canal (opération racine).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn publish_manifest(&self, manifeste: UpdateManifest) -> Result<(), ErreurClient> {
        self.attendre_ok(&Request::PublishManifest {
            jeton: self.jeton.clone(),
            manifeste,
        })
    }

    // -- Configuration -------------------------------------------------------

    /// Configuration effective de l'organisation `org` (paires triées par clé).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur.
    pub fn effective_config(&self, org: &str) -> Result<Vec<(String, String)>, ErreurClient> {
        match self.echanger(&Request::EffectiveConfig {
            jeton: self.jeton.clone(),
            org: org.to_string(),
        })? {
            Response::Config(paires) => Ok(paires),
            autre => Err(erreur_reponse(autre)),
        }
    }

    /// Fixe (ou remplace) la politique `cle = valeur` de `org` (racine, ou
    /// `ManageMembers` sur l'organisation).
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (accès refusé...).
    pub fn set_policy(&self, org: &str, cle: &str, valeur: &str) -> Result<(), ErreurClient> {
        self.attendre_ok(&Request::SetPolicy {
            jeton: self.jeton.clone(),
            org: org.to_string(),
            cle: cle.to_string(),
            valeur: valeur.to_string(),
        })
    }

    // -- Attribution d'ID ----------------------------------------------------

    /// Alloue un ID NovaDesk lié au compte du jeton et à la clé statique
    /// `cle_client` (32 octets : clé publique Ed25519 du client). Renvoie l'ID
    /// et son jeton d'enregistrement signé.
    ///
    /// # Errors
    /// [`ErreurClient`] : réseau, ou erreur serveur (clé statique invalide,
    /// émission indisponible...).
    pub fn allocate_id(&self, cle_client: [u8; 32]) -> Result<IdAlloue, ErreurClient> {
        match self.echanger(&Request::AllocateId {
            jeton: self.jeton.clone(),
            cle_client,
        })? {
            Response::IdAlloue {
                id,
                jeton_enregistrement,
            } => Ok(IdAlloue {
                id,
                jeton_enregistrement,
            }),
            autre => Err(erreur_reponse(autre)),
        }
    }

    // -- Transport (privé) ---------------------------------------------------

    /// Ouvre une connexion, envoie la requête, lit et décode la réponse.
    fn echanger(&self, requete: &Request) -> Result<Response, ErreurClient> {
        let mut flux = TcpStream::connect(self.adresse)?;
        write_frame(&mut flux, &requete.to_bytes())?;
        let octets = read_frame(&mut flux)?;
        Response::from_bytes(&octets).ok_or(ErreurClient::ReponseIllisible)
    }

    /// Échange dont on attend [`Response::Ok`] ; toute autre réponse est une
    /// erreur (métier ou inattendue).
    fn attendre_ok(&self, requete: &Request) -> Result<(), ErreurClient> {
        match self.echanger(requete)? {
            Response::Ok => Ok(()),
            autre => Err(erreur_reponse(autre)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (unitaires, hermétiques : pas de réseau)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_resout_l_adresse_et_garde_le_jeton() {
        let client = ApiClient::connect("127.0.0.1:9300", "jeton").expect("adresse valide");
        assert_eq!(
            client.adresse,
            "127.0.0.1:9300".parse().expect("SocketAddr")
        );
        assert_eq!(client.jeton, "jeton");
    }

    #[test]
    fn connect_adresse_invalide_refusee() {
        // Chaîne sans port : irrésoluble, sans le moindre accès réseau.
        assert!(matches!(
            ApiClient::connect("pas-une-adresse", "jeton"),
            Err(ErreurClient::Io(_))
        ));
    }

    #[test]
    fn debug_masque_le_jeton() {
        let client = ApiClient::connect("127.0.0.1:9300", "secret-porteur").expect("client");
        let rendu = format!("{client:?}");
        assert!(rendu.contains("<masqué>"), "{rendu}");
        assert!(!rendu.contains("secret-porteur"), "{rendu}");
    }

    #[test]
    fn erreur_reponse_transmet_le_message_serveur() {
        let e = erreur_reponse(Response::Erreur {
            message: "accès refusé".into(),
        });
        assert!(matches!(&e, ErreurClient::Serveur(m) if m == "accès refusé"));
        assert_eq!(e.to_string(), "accès refusé");
        // Réponse d'un autre type : inattendue.
        assert!(matches!(
            erreur_reponse(Response::Ok),
            ErreurClient::ReponseInattendue
        ));
    }

    #[test]
    fn affichage_des_erreurs_en_francais() {
        assert_eq!(
            ErreurClient::ReponseIllisible.to_string(),
            "réponse du serveur illisible"
        );
        assert_eq!(
            ErreurClient::ReponseInattendue.to_string(),
            "réponse inattendue du serveur"
        );
    }
}
