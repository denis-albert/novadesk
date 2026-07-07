//! Licences et quotas NovaDesk — logique **en mémoire** (plan 11).
//!
//! Chaque compte a un plan commercial (`Free`, `Pro`, `Entreprise`) qui borne
//! le nombre de sessions de bureau à distance simultanées. Un compte sans plan
//! attribué est traité comme `Free`. La persistance (base de données) et la
//! facturation viendront plus tard ; tout est testable sans réseau.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Plan commercial d'un compte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Plan {
    /// Gratuit : 1 session simultanée.
    #[default]
    Free,
    /// Professionnel : 10 sessions simultanées.
    Pro,
    /// Entreprise : sessions illimitées.
    Entreprise,
}

impl Plan {
    /// Quota de sessions simultanées du plan (`None` = illimité).
    #[must_use]
    pub fn max_sessions(self) -> Option<u32> {
        match self {
            Plan::Free => Some(1),
            Plan::Pro => Some(10),
            Plan::Entreprise => None,
        }
    }

    /// Nom stable du plan (persistance, protocole, claims des jetons
    /// applicatifs) — voir [`Self::depuis_nom`].
    #[must_use]
    pub fn nom(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Pro => "pro",
            Plan::Entreprise => "entreprise",
        }
    }

    /// Plan désigné par son nom stable (`None` si inconnu).
    #[must_use]
    pub fn depuis_nom(nom: &str) -> Option<Self> {
        match nom {
            "free" => Some(Plan::Free),
            "pro" => Some(Plan::Pro),
            "entreprise" => Some(Plan::Entreprise),
            _ => None,
        }
    }
}

/// Licence d'un compte : plan en vigueur et sessions actuellement ouvertes.
#[derive(Debug, Clone, Default)]
pub struct License {
    /// Plan en vigueur.
    pub plan: Plan,
    /// Nombre de sessions actuellement ouvertes.
    pub sessions_actives: u32,
}

/// La licence a-t-elle encore une place de session disponible ?
fn sous_quota(licence: &License) -> bool {
    match licence.plan.max_sessions() {
        None => true, // illimité
        Some(max) => licence.sessions_actives < max,
    }
}

/// Magasin de licences en mémoire (thread-safe, clonable) : e-mail → licence.
#[derive(Clone, Default)]
pub struct LicenseStore {
    etat: Arc<Mutex<HashMap<String, License>>>,
}

impl LicenseStore {
    /// Magasin vide : tout compte inconnu est traité comme `Free`, 0 session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribue (ou change) le plan d'un compte. Les sessions déjà ouvertes
    /// sont conservées : une rétrogradation ne coupe pas les sessions en cours,
    /// elle empêche seulement d'en ouvrir de nouvelles au-delà du quota.
    pub fn assign_plan(&self, email: &str, plan: Plan) {
        self.etat
            .lock()
            .unwrap()
            .entry(email.to_string())
            .or_default()
            .plan = plan;
    }

    /// Licence courante d'un compte (plan `Free` et 0 session si inconnu).
    #[must_use]
    pub fn license_of(&self, email: &str) -> License {
        self.etat
            .lock()
            .unwrap()
            .get(email)
            .cloned()
            .unwrap_or_default()
    }

    /// Le compte peut-il ouvrir une session supplémentaire ? (lecture seule ;
    /// pour ouvrir réellement, passer par [`Self::session_started`]).
    #[must_use]
    pub fn can_start_session(&self, email: &str) -> bool {
        sous_quota(&self.license_of(email))
    }

    /// Tente d'ouvrir une session : vérifie le quota **et** incrémente le
    /// compteur sous le même verrou (pas de course entre vérification et
    /// ouverture). Renvoie `false` si le quota du plan est atteint.
    #[must_use = "le quota peut refuser l'ouverture de la session"]
    pub fn session_started(&self, email: &str) -> bool {
        let mut etat = self.etat.lock().unwrap();
        let licence = etat.entry(email.to_string()).or_default();
        if !sous_quota(licence) {
            return false;
        }
        licence.sessions_actives += 1;
        true
    }

    /// Signale la fin d'une session : libère une place de quota. Sans effet si
    /// le compte est inconnu ou n'a aucune session ouverte (saturé à zéro).
    pub fn session_ended(&self, email: &str) {
        if let Some(licence) = self.etat.lock().unwrap().get_mut(email) {
            licence.sessions_actives = licence.sessions_actives.saturating_sub(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_des_plans() {
        assert_eq!(Plan::Free.max_sessions(), Some(1));
        assert_eq!(Plan::Pro.max_sessions(), Some(10));
        assert_eq!(Plan::Entreprise.max_sessions(), None);
        assert_eq!(Plan::default(), Plan::Free);
    }

    #[test]
    fn noms_de_plans_aller_retour() {
        for plan in [Plan::Free, Plan::Pro, Plan::Entreprise] {
            assert_eq!(Plan::depuis_nom(plan.nom()), Some(plan));
        }
        assert_eq!(Plan::depuis_nom("premium"), None);
        assert_eq!(Plan::depuis_nom(""), None);
    }

    #[test]
    fn free_bloque_la_deuxieme_session_simultanee() {
        let store = LicenseStore::new();
        // Compte inconnu : plan Free par défaut, 0 session.
        assert_eq!(store.license_of("a@example.com").plan, Plan::Free);
        assert!(store.can_start_session("a@example.com"));
        assert!(store.session_started("a@example.com"));
        // Quota Free (1) atteint : la 2e session simultanée est refusée.
        assert!(!store.can_start_session("a@example.com"));
        assert!(!store.session_started("a@example.com"));
        // La fin de session libère la place.
        store.session_ended("a@example.com");
        assert!(store.can_start_session("a@example.com"));
        assert!(store.session_started("a@example.com"));
    }

    #[test]
    fn pro_autorise_dix_sessions_simultanees() {
        let store = LicenseStore::new();
        store.assign_plan("b@example.com", Plan::Pro);
        for i in 0..10 {
            assert!(
                store.session_started("b@example.com"),
                "session {i} < quota"
            );
        }
        assert!(
            !store.session_started("b@example.com"),
            "11e session refusée"
        );
        store.session_ended("b@example.com");
        assert!(store.session_started("b@example.com"), "place libérée");
    }

    #[test]
    fn entreprise_illimite() {
        let store = LicenseStore::new();
        store.assign_plan("c@example.com", Plan::Entreprise);
        for _ in 0..100 {
            assert!(store.session_started("c@example.com"));
        }
        assert!(store.can_start_session("c@example.com"));
        assert_eq!(store.license_of("c@example.com").sessions_actives, 100);
    }

    #[test]
    fn changement_de_plan_conserve_les_sessions() {
        let store = LicenseStore::new();
        assert!(store.session_started("d@example.com")); // Free : 1/1
        assert!(!store.can_start_session("d@example.com"));
        // Montée en gamme : la session ouverte est conservée, une place s'ouvre.
        store.assign_plan("d@example.com", Plan::Pro);
        assert_eq!(store.license_of("d@example.com").sessions_actives, 1);
        assert!(store.can_start_session("d@example.com"));
        assert!(store.session_started("d@example.com")); // 2 actives
                                                         // Rétrogradation au-dessus du quota : plus de nouvelle session,
                                                         // mais les sessions en cours ne sont pas coupées.
        store.assign_plan("d@example.com", Plan::Free);
        assert!(!store.can_start_session("d@example.com"));
        assert_eq!(store.license_of("d@example.com").sessions_actives, 2);
    }

    #[test]
    fn session_ended_sans_session_est_sans_effet() {
        let store = LicenseStore::new();
        store.session_ended("e@example.com"); // compte inconnu : rien, pas de panique
        store.assign_plan("e@example.com", Plan::Free);
        store.session_ended("e@example.com"); // 0 session : saturé à zéro
        assert_eq!(store.license_of("e@example.com").sessions_actives, 0);
    }
}
