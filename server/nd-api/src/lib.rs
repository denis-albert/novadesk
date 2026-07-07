//! API applicative NovaDesk — carnet d'adresses, RBAC, groupes/équipes,
//! partage d'appareils, attribution d'ID, mises à jour et configuration.
//!
//! La bibliothèque expose :
//! - le carnet d'adresses ([`AddressBook`]) et ses types ;
//! - les magasins métier : rôles ([`rbac`]), groupes ([`groups`]), partages
//!   ([`sharing`]), mises à jour ([`update`]), politiques ([`config`]),
//!   attribution d'ID ([`allocation`]) ;
//! - l'autorité de signature ([`auth`]) : jetons applicatifs, jetons
//!   d'enregistrement d'ID et tickets de relais (Ed25519) — formats partagés
//!   avec `nd-rendezvous` et `nd-relay` ;
//! - le protocole TCP ([`protocol`]) : trames `u32` BE + un octet de tag,
//!   au même format que `nd-signaling` — **tous** les magasins ci-dessus sont
//!   réellement appelables par un client via [`protocol::Request`] ;
//! - l'état assemblé et le serveur ([`services`]) ;
//! - la persistance légère ([`storage`]) : JSON pur Rust, écriture atomique
//!   (fichier temporaire + renommage).
//!
//! **Autorisation réelle** (plan 11) : chaque requête authentifiée porte un
//! jeton applicatif **signé** ([`auth`]) dont est dérivé le **compte
//! agissant** — jamais d'un champ de la requête — et le RBAC est appliqué
//! comme contrôle d'accès (voir la matrice dans [`services`]). La validation
//! croisée avec `nd-accounts` (émission des jetons à la connexion) est le
//! point de jonction du lot 09, documenté dans [`auth`].
//!
//! Le binaire (`main.rs`) ne fait qu'assembler : écoute TCP + [`serve`].

pub mod allocation;
pub mod auth;
pub mod config;
pub mod groups;
pub mod protocol;
pub mod rbac;
pub mod services;
pub mod sharing;
pub mod storage;
pub mod update;

pub use services::{serve, Services};

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreurs d'authentification et d'autorisation de l'API (messages stables,
/// renvoyés tels quels aux clients par le protocole).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Jeton absent, mal formé ou signé par une autre autorité.
    JetonInvalide,
    /// Jeton bien signé mais dont la date d'expiration est passée.
    JetonExpire,
    /// Compte authentifié mais dépourvu du rôle requis pour l'opération.
    AccesRefuse,
    /// Alias de contact vide.
    AliasVide,
    /// Nom de compte vide.
    CompteVide,
    /// Émission de jeton impossible : l'autorité est en vérification seule
    /// (la clé privée vit ailleurs — `nd-accounts`, lot 09).
    EmissionIndisponible,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::JetonInvalide => write!(f, "jeton invalide ou absent"),
            ApiError::JetonExpire => write!(f, "jeton expiré"),
            ApiError::AccesRefuse => write!(f, "accès refusé"),
            ApiError::AliasVide => write!(f, "alias de contact vide"),
            ApiError::CompteVide => write!(f, "nom de compte vide"),
            ApiError::EmissionIndisponible => {
                write!(f, "émission de jeton indisponible (vérification seule)")
            }
        }
    }
}

impl std::error::Error for ApiError {}

// ---------------------------------------------------------------------------
// Carnet d'adresses
// ---------------------------------------------------------------------------

/// Entrée du carnet d'adresses : ID NovaDesk + alias lisible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// ID NovaDesk du pair (voir `nd_proto::NovaId`).
    pub id: u64,
    /// Alias choisi par l'utilisateur (« PC bureau », ...).
    pub alias: String,
}

/// Table du carnet : compte → contacts du compte.
pub type CarnetMap = HashMap<String, Vec<Contact>>;

/// Carnet d'adresses partagé, en mémoire (thread-safe, clonable).
///
/// Le carnet est indexé par **compte** : au niveau du protocole, le compte
/// agissant est dérivé du jeton applicatif signé (voir [`services`]) — deux
/// jetons du même compte voient donc le même carnet.
#[derive(Clone, Default)]
pub struct AddressBook(Arc<Mutex<CarnetMap>>);

impl AddressBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute (ou met à jour l'alias d')un contact du carnet de `compte`.
    ///
    /// # Errors
    /// `CompteVide` si le compte est vide, `AliasVide` si l'alias est vide.
    pub fn add_contact(&self, compte: &str, contact_id: u64, alias: &str) -> Result<(), ApiError> {
        if compte.trim().is_empty() {
            return Err(ApiError::CompteVide);
        }
        if alias.trim().is_empty() {
            return Err(ApiError::AliasVide);
        }
        let mut carnet = self.0.lock().unwrap();
        let contacts = carnet.entry(compte.to_string()).or_default();
        match contacts.iter_mut().find(|c| c.id == contact_id) {
            // Même ID déjà présent : on met l'alias à jour.
            Some(existant) => existant.alias = alias.to_string(),
            None => contacts.push(Contact {
                id: contact_id,
                alias: alias.to_string(),
            }),
        }
        Ok(())
    }

    /// Liste les contacts du carnet de `compte` (vide si aucun).
    ///
    /// # Errors
    /// `CompteVide` si le compte est vide.
    pub fn list_contacts(&self, compte: &str) -> Result<Vec<Contact>, ApiError> {
        if compte.trim().is_empty() {
            return Err(ApiError::CompteVide);
        }
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(compte)
            .cloned()
            .unwrap_or_default())
    }

    /// Instantané complet du carnet (pour la persistance, voir [`storage`]).
    #[must_use]
    pub fn snapshot(&self) -> CarnetMap {
        self.0.lock().unwrap().clone()
    }

    /// Reconstruit un carnet depuis un instantané persisté.
    #[must_use]
    pub fn from_snapshot(carnet: CarnetMap) -> Self {
        Self(Arc::new(Mutex::new(carnet)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_puis_list_contacts() {
        let carnet = AddressBook::new();
        carnet
            .add_contact("alice", 111_222_333, "PC bureau")
            .expect("add 1");
        carnet
            .add_contact("alice", 444_555_666, "Portable")
            .expect("add 2");
        let contacts = carnet.list_contacts("alice").expect("list");
        assert_eq!(
            contacts,
            vec![
                Contact {
                    id: 111_222_333,
                    alias: "PC bureau".into()
                },
                Contact {
                    id: 444_555_666,
                    alias: "Portable".into()
                },
            ]
        );
    }

    #[test]
    fn carnets_isoles_par_compte() {
        let carnet = AddressBook::new();
        carnet.add_contact("alice", 1, "A").expect("add a");
        carnet.add_contact("bob", 2, "B").expect("add b");
        assert_eq!(carnet.list_contacts("alice").expect("list a").len(), 1);
        assert_eq!(carnet.list_contacts("bob").expect("list b").len(), 1);
        // Compte jamais vu : carnet vide, pas d'erreur.
        assert!(carnet.list_contacts("carol").expect("list c").is_empty());
    }

    #[test]
    fn meme_id_met_alias_a_jour() {
        let carnet = AddressBook::new();
        carnet.add_contact("alice", 42, "Ancien nom").expect("add");
        carnet.add_contact("alice", 42, "Nouveau nom").expect("maj");
        let contacts = carnet.list_contacts("alice").expect("list");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].alias, "Nouveau nom");
    }

    #[test]
    fn compte_ou_alias_vide_refuse() {
        let carnet = AddressBook::new();
        assert_eq!(carnet.add_contact("", 1, "X"), Err(ApiError::CompteVide));
        assert_eq!(carnet.list_contacts("  "), Err(ApiError::CompteVide));
        // Alias vide refusé aussi.
        assert_eq!(carnet.add_contact("alice", 1, ""), Err(ApiError::AliasVide));
    }

    #[test]
    fn snapshot_puis_from_snapshot() {
        let carnet = AddressBook::new();
        carnet.add_contact("alice", 7, "NAS").expect("add");
        let rejoue = AddressBook::from_snapshot(carnet.snapshot());
        assert_eq!(
            rejoue.list_contacts("alice").expect("list"),
            carnet.list_contacts("alice").expect("list")
        );
    }
}
