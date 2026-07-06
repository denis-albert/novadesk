//! Groupes/équipes NovaDesk — gestion **en mémoire**.
//!
//! Un groupe rassemble des comptes (identifiés par leur nom) sous un id
//! numérique attribué à la création. Sert de cible de partage d'appareils
//! (voir `sharing.rs`) et, à terme, d'unité d'attribution de rôles.
//! Voir `../../plan-technique/11-backend-infrastructure.md`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Erreurs métier des groupes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupError {
    /// Nom de groupe vide.
    NomVide,
    /// Nom de compte vide.
    CompteVide,
    /// Aucun groupe avec cet id.
    GroupeInconnu,
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GroupError::NomVide => write!(f, "nom de groupe vide"),
            GroupError::CompteVide => write!(f, "nom de compte vide"),
            GroupError::GroupeInconnu => write!(f, "groupe inconnu"),
        }
    }
}

impl std::error::Error for GroupError {}

/// Groupe (équipe) : id attribué à la création, nom lisible, membres (comptes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Id du groupe, attribué par le [`GroupStore`] (croissant, jamais réutilisé).
    pub id: u64,
    /// Nom lisible (« Support », « Équipe infra », ...).
    pub name: String,
    /// Comptes membres, dans l'ordre d'ajout, sans doublon.
    pub members: Vec<String>,
}

/// État interne : compteur d'ids + groupes par id.
#[derive(Default)]
struct GroupesInner {
    /// Dernier id attribué (0 = aucun ; les ids commencent à 1).
    dernier_id: u64,
    groupes: HashMap<u64, Group>,
}

/// Magasin de groupes partagé, en mémoire (thread-safe, clonable).
#[derive(Clone, Default)]
pub struct GroupStore(Arc<Mutex<GroupesInner>>);

impl GroupStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Crée un groupe vide et renvoie son id.
    ///
    /// # Errors
    /// `NomVide` si le nom est vide.
    pub fn create_group(&self, name: &str) -> Result<u64, GroupError> {
        if name.trim().is_empty() {
            return Err(GroupError::NomVide);
        }
        let mut inner = self.0.lock().unwrap();
        inner.dernier_id += 1;
        let id = inner.dernier_id;
        inner.groupes.insert(
            id,
            Group {
                id,
                name: name.to_string(),
                members: Vec::new(),
            },
        );
        Ok(id)
    }

    /// Ajoute `compte` au groupe `group_id` (sans effet s'il est déjà membre).
    ///
    /// # Errors
    /// `GroupeInconnu` si l'id n'existe pas, `CompteVide` si le compte est vide.
    pub fn add_member(&self, group_id: u64, compte: &str) -> Result<(), GroupError> {
        if compte.trim().is_empty() {
            return Err(GroupError::CompteVide);
        }
        let mut inner = self.0.lock().unwrap();
        let groupe = inner
            .groupes
            .get_mut(&group_id)
            .ok_or(GroupError::GroupeInconnu)?;
        if !groupe.members.iter().any(|m| m == compte) {
            groupe.members.push(compte.to_string());
        }
        Ok(())
    }

    /// Retire `compte` du groupe `group_id` (sans effet s'il n'était pas membre).
    ///
    /// # Errors
    /// `GroupeInconnu` si l'id n'existe pas.
    pub fn remove_member(&self, group_id: u64, compte: &str) -> Result<(), GroupError> {
        let mut inner = self.0.lock().unwrap();
        let groupe = inner
            .groupes
            .get_mut(&group_id)
            .ok_or(GroupError::GroupeInconnu)?;
        groupe.members.retain(|m| m != compte);
        Ok(())
    }

    /// Le compte est-il membre du groupe ? (`false` si le groupe n'existe pas.)
    #[must_use]
    pub fn is_member(&self, group_id: u64, compte: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .groupes
            .get(&group_id)
            .is_some_and(|g| g.members.iter().any(|m| m == compte))
    }

    /// Copie du groupe `group_id`, s'il existe.
    #[must_use]
    pub fn get(&self, group_id: u64) -> Option<Group> {
        self.0.lock().unwrap().groupes.get(&group_id).cloned()
    }

    /// Groupes dont `compte` est membre, triés par id (déterministe).
    #[must_use]
    pub fn groups_of(&self, compte: &str) -> Vec<Group> {
        let inner = self.0.lock().unwrap();
        let mut groupes: Vec<Group> = inner
            .groupes
            .values()
            .filter(|g| g.members.iter().any(|m| m == compte))
            .cloned()
            .collect();
        groupes.sort_by_key(|g| g.id);
        groupes
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_puis_ajout_de_membres() {
        let store = GroupStore::new();
        let support = store.create_group("Support").expect("création");
        let infra = store.create_group("Infra").expect("création");
        assert_ne!(support, infra, "ids distincts");

        store.add_member(support, "alice").expect("ajout alice");
        store.add_member(support, "bob").expect("ajout bob");
        store
            .add_member(support, "alice")
            .expect("ajout idempotent");

        let groupe = store.get(support).expect("groupe existant");
        assert_eq!(groupe.name, "Support");
        assert_eq!(groupe.members, vec!["alice".to_string(), "bob".to_string()]);
        assert!(store.is_member(support, "alice"));
        assert!(!store.is_member(infra, "alice"));
    }

    #[test]
    fn retrait_de_membre() {
        let store = GroupStore::new();
        let id = store.create_group("Équipe").expect("création");
        store.add_member(id, "alice").expect("ajout");
        store.add_member(id, "bob").expect("ajout");

        store.remove_member(id, "alice").expect("retrait");
        assert!(!store.is_member(id, "alice"));
        assert!(store.is_member(id, "bob"));
        // Retrait d'un non-membre : sans effet, pas d'erreur.
        store
            .remove_member(id, "alice")
            .expect("retrait idempotent");
    }

    #[test]
    fn groupes_d_un_compte() {
        let store = GroupStore::new();
        let a = store.create_group("A").expect("création");
        let b = store.create_group("B").expect("création");
        let c = store.create_group("C").expect("création");
        store.add_member(a, "alice").expect("ajout");
        store.add_member(c, "alice").expect("ajout");
        store.add_member(b, "bob").expect("ajout");

        let groupes = store.groups_of("alice");
        assert_eq!(
            groupes.iter().map(|g| g.id).collect::<Vec<_>>(),
            vec![a, c],
            "triés par id"
        );
        assert!(store.groups_of("inconnu").is_empty());
    }

    #[test]
    fn erreurs_metier() {
        let store = GroupStore::new();
        assert_eq!(store.create_group("  "), Err(GroupError::NomVide));
        assert_eq!(
            store.add_member(999, "alice"),
            Err(GroupError::GroupeInconnu)
        );
        assert_eq!(
            store.remove_member(999, "alice"),
            Err(GroupError::GroupeInconnu)
        );
        let id = store.create_group("Ok").expect("création");
        assert_eq!(store.add_member(id, " "), Err(GroupError::CompteVide));
        assert!(store.get(999).is_none());
    }
}
