//! API applicative NovaDesk — carnet d'adresses, RBAC, groupes/équipes,
//! partage d'appareils, mises à jour et distribution de configuration.
//!
//! La bibliothèque expose :
//! - le carnet d'adresses ([`AddressBook`]) et ses types ;
//! - les magasins métier : rôles ([`rbac`]), groupes ([`groups`]), partages
//!   ([`sharing`]), mises à jour ([`update`]), politiques ([`config`]) ;
//! - le protocole TCP ([`protocol`]) : trames `u32` BE + un octet de tag,
//!   au même format que `nd-signaling` — **tous** les magasins ci-dessus sont
//!   réellement appelables par un client via [`protocol::Request`] ;
//! - l'état assemblé et le serveur ([`services`]) ;
//! - la persistance légère ([`storage`]) : JSON pur Rust, écriture atomique
//!   (fichier temporaire + renommage).
//!
//! La vérification du jeton de session est volontairement minimale pour ce jet
//! (tout jeton non vide est accepté) — la validation croisée avec `nd-accounts`
//! viendra ensuite. Voir `../../plan-technique/11-backend-infrastructure.md`.
//!
//! Le binaire (`main.rs`) ne fait qu'assembler : écoute TCP + [`serve`].

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

/// Erreurs métier du carnet d'adresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Jeton de session vide ou absent.
    JetonInvalide,
    /// Alias de contact vide.
    AliasVide,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::JetonInvalide => write!(f, "jeton invalide ou absent"),
            ApiError::AliasVide => write!(f, "alias de contact vide"),
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

/// Table du carnet : jeton de session → contacts du compte.
pub type CarnetMap = HashMap<String, Vec<Contact>>;

/// Carnet d'adresses partagé, en mémoire (thread-safe, clonable).
#[derive(Clone, Default)]
pub struct AddressBook(Arc<Mutex<CarnetMap>>);

impl AddressBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute (ou met à jour l'alias d')un contact du compte identifié par `jeton`.
    ///
    /// # Errors
    /// `JetonInvalide` si le jeton est vide, `AliasVide` si l'alias est vide.
    pub fn add_contact(&self, jeton: &str, contact_id: u64, alias: &str) -> Result<(), ApiError> {
        verifier_jeton(jeton)?;
        if alias.trim().is_empty() {
            return Err(ApiError::AliasVide);
        }
        let mut carnet = self.0.lock().unwrap();
        let contacts = carnet.entry(jeton.to_string()).or_default();
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

    /// Liste les contacts du compte identifié par `jeton` (vide si aucun).
    ///
    /// # Errors
    /// `JetonInvalide` si le jeton est vide.
    pub fn list_contacts(&self, jeton: &str) -> Result<Vec<Contact>, ApiError> {
        verifier_jeton(jeton)?;
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(jeton)
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

/// Vérification minimale pour ce jet : tout jeton non vide est accepté.
/// (La validation auprès de `nd-accounts` viendra avec les comptes réels.)
pub(crate) fn verifier_jeton(jeton: &str) -> Result<(), ApiError> {
    if jeton.trim().is_empty() {
        Err(ApiError::JetonInvalide)
    } else {
        Ok(())
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
            .add_contact("jeton-a", 111_222_333, "PC bureau")
            .expect("add 1");
        carnet
            .add_contact("jeton-a", 444_555_666, "Portable")
            .expect("add 2");
        let contacts = carnet.list_contacts("jeton-a").expect("list");
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
    fn carnets_isoles_par_jeton() {
        let carnet = AddressBook::new();
        carnet.add_contact("jeton-a", 1, "A").expect("add a");
        carnet.add_contact("jeton-b", 2, "B").expect("add b");
        assert_eq!(carnet.list_contacts("jeton-a").expect("list a").len(), 1);
        assert_eq!(carnet.list_contacts("jeton-b").expect("list b").len(), 1);
        // Jeton jamais vu : carnet vide, pas d'erreur.
        assert!(carnet.list_contacts("jeton-c").expect("list c").is_empty());
    }

    #[test]
    fn meme_id_met_alias_a_jour() {
        let carnet = AddressBook::new();
        carnet
            .add_contact("jeton-a", 42, "Ancien nom")
            .expect("add");
        carnet
            .add_contact("jeton-a", 42, "Nouveau nom")
            .expect("maj");
        let contacts = carnet.list_contacts("jeton-a").expect("list");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].alias, "Nouveau nom");
    }

    #[test]
    fn jeton_vide_refuse() {
        let carnet = AddressBook::new();
        assert_eq!(carnet.add_contact("", 1, "X"), Err(ApiError::JetonInvalide));
        assert_eq!(carnet.list_contacts("  "), Err(ApiError::JetonInvalide));
        // Alias vide refusé aussi.
        assert_eq!(carnet.add_contact("jeton", 1, ""), Err(ApiError::AliasVide));
    }

    #[test]
    fn snapshot_puis_from_snapshot() {
        let carnet = AddressBook::new();
        carnet.add_contact("jeton-a", 7, "NAS").expect("add");
        let rejoue = AddressBook::from_snapshot(carnet.snapshot());
        assert_eq!(
            rejoue.list_contacts("jeton-a").expect("list"),
            carnet.list_contacts("jeton-a").expect("list")
        );
    }
}
