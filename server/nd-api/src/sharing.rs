//! Partage d'appareils NovaDesk — **en mémoire**.
//!
//! Un appareil (id `u64`, voir `nd_proto::NovaId`) est partagé avec un
//! bénéficiaire — un compte nommé ou un groupe (voir `groups.rs`) — assorti
//! d'un rôle (voir `rbac.rs`). La résolution pour un compte passe aussi par
//! ses groupes ; si plusieurs sources donnent des rôles différents sur le même
//! appareil, le rôle **effectif** est le plus élevé (`Ord` sur `Role`).
//! Voir `../../plan-technique/11-backend-infrastructure.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::groups::GroupStore;
use crate::rbac::Role;

/// Bénéficiaire d'un partage : compte individuel ou groupe entier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Beneficiaire {
    /// Compte nommé (partage direct).
    Compte(String),
    /// Groupe par id : tous ses membres, présents et futurs, en bénéficient.
    Groupe(u64),
}

/// Table interne : id d'appareil → (bénéficiaire → rôle accordé).
type PartageMap = HashMap<u64, HashMap<Beneficiaire, Role>>;

/// Magasin de partages, en mémoire (thread-safe, clonable).
///
/// Tient une poignée sur le [`GroupStore`] pour résoudre l'appartenance aux
/// groupes au moment de la requête (un membre ajouté après le partage en
/// bénéficie donc aussi).
#[derive(Clone)]
pub struct SharingStore {
    groupes: GroupStore,
    partages: Arc<Mutex<PartageMap>>,
}

impl SharingStore {
    #[must_use]
    pub fn new(groupes: GroupStore) -> Self {
        Self {
            groupes,
            partages: Arc::new(Mutex::new(PartageMap::new())),
        }
    }

    /// Partage l'appareil `device_id` avec `avec` (compte ou groupe) au rôle
    /// `role`. Repartager avec le même bénéficiaire met le rôle à jour.
    pub fn share_device(&self, device_id: u64, avec: Beneficiaire, role: Role) {
        self.partages
            .lock()
            .unwrap()
            .entry(device_id)
            .or_default()
            .insert(avec, role);
    }

    /// Retire le partage de `device_id` accordé à `avec`. Renvoie `true` si un
    /// partage existait pour ce bénéficiaire.
    pub fn unshare_device(&self, device_id: u64, avec: &Beneficiaire) -> bool {
        let mut partages = self.partages.lock().unwrap();
        let Some(par_beneficiaire) = partages.get_mut(&device_id) else {
            return false;
        };
        let retire = par_beneficiaire.remove(avec).is_some();
        if par_beneficiaire.is_empty() {
            partages.remove(&device_id);
        }
        retire
    }

    /// Rôle effectif de `compte` sur `device_id` : maximum entre le partage
    /// direct et ceux hérités des groupes dont il est membre. `None` si
    /// l'appareil ne lui est pas partagé.
    #[must_use]
    pub fn effective_role(&self, compte: &str, device_id: u64) -> Option<Role> {
        let partages = self.partages.lock().unwrap();
        let par_beneficiaire = partages.get(&device_id)?;
        Self::role_pour_compte(&self.groupes, compte, par_beneficiaire)
    }

    /// Appareils partagés avec `compte` (directement ou via ses groupes), avec
    /// le rôle effectif pour chacun. Triés par id d'appareil (déterministe).
    #[must_use]
    pub fn devices_shared_with(&self, compte: &str) -> Vec<(u64, Role)> {
        let partages = self.partages.lock().unwrap();
        let mut resultat: Vec<(u64, Role)> = partages
            .iter()
            .filter_map(|(device_id, par_beneficiaire)| {
                Self::role_pour_compte(&self.groupes, compte, par_beneficiaire)
                    .map(|role| (*device_id, role))
            })
            .collect();
        resultat.sort_by_key(|(device_id, _)| *device_id);
        resultat
    }

    /// Rôle le plus élevé accordé à `compte` parmi les bénéficiaires d'un
    /// appareil (partage direct ou appartenance à un groupe bénéficiaire).
    fn role_pour_compte(
        groupes: &GroupStore,
        compte: &str,
        par_beneficiaire: &HashMap<Beneficiaire, Role>,
    ) -> Option<Role> {
        par_beneficiaire
            .iter()
            .filter(|(beneficiaire, _)| match beneficiaire {
                Beneficiaire::Compte(c) => c == compte,
                Beneficiaire::Groupe(id) => groupes.is_member(*id, compte),
            })
            .map(|(_, role)| *role)
            .max()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Bénéficiaire « compte » de test.
    fn compte(nom: &str) -> Beneficiaire {
        Beneficiaire::Compte(nom.to_string())
    }

    #[test]
    fn partage_direct() {
        let store = SharingStore::new(GroupStore::new());
        store.share_device(100, compte("alice"), Role::Operator);
        store.share_device(200, compte("alice"), Role::Viewer);
        store.share_device(300, compte("bob"), Role::Admin);

        assert_eq!(
            store.devices_shared_with("alice"),
            vec![(100, Role::Operator), (200, Role::Viewer)]
        );
        assert_eq!(store.effective_role("alice", 100), Some(Role::Operator));
        assert_eq!(store.effective_role("alice", 300), None);
        assert!(store.devices_shared_with("inconnu").is_empty());
    }

    #[test]
    fn partage_via_groupe() {
        let groupes = GroupStore::new();
        let support = groupes.create_group("Support").expect("création");
        groupes.add_member(support, "alice").expect("ajout");

        let store = SharingStore::new(groupes.clone());
        store.share_device(100, Beneficiaire::Groupe(support), Role::Viewer);

        // Alice en bénéficie via son appartenance ; Bob non.
        assert_eq!(
            store.devices_shared_with("alice"),
            vec![(100, Role::Viewer)]
        );
        assert!(store.devices_shared_with("bob").is_empty());

        // Membre ajouté APRÈS le partage : il en bénéficie aussi (résolution
        // à la requête, pas à la création du partage).
        groupes.add_member(support, "bob").expect("ajout");
        assert_eq!(store.effective_role("bob", 100), Some(Role::Viewer));

        // Membre retiré du groupe : il perd l'accès.
        groupes.remove_member(support, "alice").expect("retrait");
        assert!(store.devices_shared_with("alice").is_empty());
    }

    #[test]
    fn role_effectif_le_plus_eleve() {
        let groupes = GroupStore::new();
        let equipe = groupes.create_group("Équipe").expect("création");
        groupes.add_member(equipe, "alice").expect("ajout");

        let store = SharingStore::new(groupes);
        // Deux sources sur le même appareil : direct Viewer + groupe Admin.
        store.share_device(100, compte("alice"), Role::Viewer);
        store.share_device(100, Beneficiaire::Groupe(equipe), Role::Admin);
        assert_eq!(store.effective_role("alice", 100), Some(Role::Admin));
        assert_eq!(store.devices_shared_with("alice"), vec![(100, Role::Admin)]);

        // L'inverse : direct Admin bat groupe Viewer.
        store.share_device(200, compte("alice"), Role::Admin);
        store.share_device(200, Beneficiaire::Groupe(equipe), Role::Viewer);
        assert_eq!(store.effective_role("alice", 200), Some(Role::Admin));
    }

    #[test]
    fn retrait_de_partage() {
        let groupes = GroupStore::new();
        let equipe = groupes.create_group("Équipe").expect("création");
        groupes.add_member(equipe, "alice").expect("ajout");

        let store = SharingStore::new(groupes);
        store.share_device(100, compte("alice"), Role::Viewer);
        store.share_device(100, Beneficiaire::Groupe(equipe), Role::Operator);

        // Retrait du partage de groupe : le partage direct subsiste.
        assert!(store.unshare_device(100, &Beneficiaire::Groupe(equipe)));
        assert_eq!(store.effective_role("alice", 100), Some(Role::Viewer));

        // Retrait du partage direct : plus aucun accès.
        assert!(store.unshare_device(100, &compte("alice")));
        assert_eq!(store.effective_role("alice", 100), None);
        assert!(store.devices_shared_with("alice").is_empty());

        // Retrait sans partage existant : `false`, pas de panique.
        assert!(!store.unshare_device(100, &compte("alice")));
        assert!(!store.unshare_device(999, &compte("alice")));
    }

    #[test]
    fn repartage_met_le_role_a_jour() {
        let store = SharingStore::new(GroupStore::new());
        store.share_device(100, compte("alice"), Role::Admin);
        store.share_device(100, compte("alice"), Role::Viewer); // Rétrogradée.
        assert_eq!(store.effective_role("alice", 100), Some(Role::Viewer));
        assert_eq!(store.devices_shared_with("alice").len(), 1);
    }
}
