//! RBAC NovaDesk — rôles, permissions dérivées et attributions **en mémoire**.
//!
//! Un rôle (`Role`) est attribué à un compte sur une ressource (appareil,
//! organisation, équipe...). Les permissions ne sont jamais stockées : elles
//! sont **dérivées** du rôle via [`Role::permissions`], ce qui garantit qu'un
//! rôle donné accorde toujours exactement le même jeu de permissions.
//! Voir `../../plan-technique/11-backend-infrastructure.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Permission élémentaire accordée sur une ressource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Voir l'écran distant.
    ViewScreen,
    /// Contrôler clavier/souris.
    ControlInput,
    /// Transférer des fichiers.
    TransferFiles,
    /// Gérer les appareils (ajout, retrait, renommage).
    ManageDevices,
    /// Gérer les membres (invitations, rôles).
    ManageMembers,
}

/// Rôle attribuable à un compte sur une ressource.
///
/// L'ordre dérivé (`Ord`) reflète la hiérarchie `Viewer < Operator < Admin` :
/// il sert à résoudre le rôle **effectif** quand plusieurs sources (partage
/// direct, groupes) attribuent des rôles différents — on garde le plus élevé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Lecture seule : voit l'écran, ne touche à rien.
    Viewer,
    /// Opérateur : voit, contrôle et transfère des fichiers.
    Operator,
    /// Administrateur : tout, y compris la gestion des appareils et membres.
    Admin,
}

impl Role {
    /// Permissions dérivées du rôle (jeu fixe, jamais stocké).
    #[must_use]
    pub const fn permissions(self) -> &'static [Permission] {
        match self {
            Role::Viewer => &[Permission::ViewScreen],
            Role::Operator => &[
                Permission::ViewScreen,
                Permission::ControlInput,
                Permission::TransferFiles,
            ],
            Role::Admin => &[
                Permission::ViewScreen,
                Permission::ControlInput,
                Permission::TransferFiles,
                Permission::ManageDevices,
                Permission::ManageMembers,
            ],
        }
    }

    /// Le rôle accorde-t-il cette permission ?
    #[must_use]
    pub fn allows(self, perm: Permission) -> bool {
        self.permissions().contains(&perm)
    }
}

/// Table interne : compte → (ressource → rôle attribué).
type AttributionMap = HashMap<String, HashMap<String, Role>>;

/// Attributions de rôles partagées, en mémoire (thread-safe, clonable).
#[derive(Clone, Default)]
pub struct RoleStore(Arc<Mutex<AttributionMap>>);

impl RoleStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribue `role` à `compte` sur `ressource` (écrase l'attribution précédente).
    pub fn assign_role(&self, compte: &str, ressource: &str, role: Role) {
        self.0
            .lock()
            .unwrap()
            .entry(compte.to_string())
            .or_default()
            .insert(ressource.to_string(), role);
    }

    /// Retire l'attribution de `compte` sur `ressource`. Renvoie `true` si une
    /// attribution existait.
    pub fn revoke_role(&self, compte: &str, ressource: &str) -> bool {
        let mut table = self.0.lock().unwrap();
        match table.get_mut(compte) {
            Some(par_ressource) => par_ressource.remove(ressource).is_some(),
            None => false,
        }
    }

    /// Rôle attribué à `compte` sur `ressource`, s'il existe.
    #[must_use]
    pub fn role_of(&self, compte: &str, ressource: &str) -> Option<Role> {
        self.0
            .lock()
            .unwrap()
            .get(compte)
            .and_then(|par_ressource| par_ressource.get(ressource))
            .copied()
    }

    /// Le compte possède-t-il `perm` sur `ressource` (via son rôle attribué) ?
    /// Sans attribution : aucune permission (refus par défaut).
    #[must_use]
    pub fn has_permission(&self, compte: &str, ressource: &str, perm: Permission) -> bool {
        self.role_of(compte, ressource)
            .is_some_and(|role| role.allows(perm))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_derivees_par_role() {
        // Viewer : lecture seule.
        assert_eq!(Role::Viewer.permissions(), &[Permission::ViewScreen]);
        assert!(Role::Viewer.allows(Permission::ViewScreen));
        assert!(!Role::Viewer.allows(Permission::ControlInput));
        assert!(!Role::Viewer.allows(Permission::ManageMembers));

        // Operator : voit, contrôle, transfère — mais ne gère rien.
        assert!(Role::Operator.allows(Permission::ViewScreen));
        assert!(Role::Operator.allows(Permission::ControlInput));
        assert!(Role::Operator.allows(Permission::TransferFiles));
        assert!(!Role::Operator.allows(Permission::ManageDevices));
        assert!(!Role::Operator.allows(Permission::ManageMembers));

        // Admin : toutes les permissions.
        for perm in [
            Permission::ViewScreen,
            Permission::ControlInput,
            Permission::TransferFiles,
            Permission::ManageDevices,
            Permission::ManageMembers,
        ] {
            assert!(Role::Admin.allows(perm), "Admin doit avoir {perm:?}");
        }
        assert_eq!(Role::Admin.permissions().len(), 5);
    }

    #[test]
    fn hierarchie_des_roles() {
        // L'ordre sert à la résolution du rôle effectif (le plus élevé gagne).
        assert!(Role::Viewer < Role::Operator);
        assert!(Role::Operator < Role::Admin);
        assert_eq!(Role::Viewer.max(Role::Admin), Role::Admin);
    }

    #[test]
    fn attribution_puis_verification() {
        let store = RoleStore::new();
        store.assign_role("alice", "org-1", Role::Admin);
        store.assign_role("bob", "org-1", Role::Viewer);

        assert_eq!(store.role_of("alice", "org-1"), Some(Role::Admin));
        assert!(store.has_permission("alice", "org-1", Permission::ManageMembers));
        assert!(store.has_permission("bob", "org-1", Permission::ViewScreen));
        assert!(!store.has_permission("bob", "org-1", Permission::ControlInput));
    }

    #[test]
    fn refus_par_defaut_sans_attribution() {
        let store = RoleStore::new();
        // Compte jamais vu, ou ressource jamais vue : aucune permission.
        assert_eq!(store.role_of("inconnu", "org-1"), None);
        assert!(!store.has_permission("inconnu", "org-1", Permission::ViewScreen));
        store.assign_role("alice", "org-1", Role::Admin);
        assert!(!store.has_permission("alice", "org-2", Permission::ViewScreen));
    }

    #[test]
    fn reattribution_ecrase_et_revocation() {
        let store = RoleStore::new();
        store.assign_role("carol", "org-1", Role::Admin);
        store.assign_role("carol", "org-1", Role::Viewer); // Rétrogradée.
        assert_eq!(store.role_of("carol", "org-1"), Some(Role::Viewer));
        assert!(!store.has_permission("carol", "org-1", Permission::ManageDevices));

        assert!(store.revoke_role("carol", "org-1"));
        assert_eq!(store.role_of("carol", "org-1"), None);
        // Deuxième révocation : rien à retirer.
        assert!(!store.revoke_role("carol", "org-1"));
    }
}
