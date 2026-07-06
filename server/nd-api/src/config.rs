//! Distribution de configuration NovaDesk — politiques par organisation **en mémoire**.
//!
//! Une politique est une paire clé/valeur textuelle (ex. `allow_file_transfer=false`,
//! `require_2fa=true`) poussée du serveur vers les clients d'une organisation.
//! La résolution suit un héritage simple, du plus général au plus spécifique :
//!
//! 1. **défauts intégrés** ([`defauts_integres`]) — toujours présents ;
//! 2. **surcharges globales** ([`PolicyStore::set_global_policy`]) — tout le parc ;
//! 3. **surcharges d'organisation** ([`PolicyStore::set_policy`]) — priorité maximale.
//!
//! [`PolicyStore::effective_config`] fusionne les trois couches dans cet ordre.
//! Voir `../../plan-technique/15-securite-operationnelle.md` et plan 11.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Couche de configuration : clé de politique → valeur textuelle.
type CoucheConfig = HashMap<String, String>;

/// Défauts intégrés, appliqués à toute organisation sans surcharge.
///
/// Choix prudents : transfert de fichiers et presse-papiers autorisés (confort),
/// mais pas d'accès non surveillé ni de 2FA imposée par défaut — chaque
/// organisation durcit (ou assouplit) via ses surcharges.
fn defauts_integres() -> CoucheConfig {
    [
        ("allow_file_transfer", "true"),
        ("allow_clipboard_sync", "true"),
        ("allow_unattended_access", "false"),
        ("require_2fa", "false"),
        ("session_timeout_minutes", "30"),
    ]
    .into_iter()
    .map(|(cle, valeur)| (cle.to_string(), valeur.to_string()))
    .collect()
}

/// État interne : surcharges globales + surcharges par organisation.
#[derive(Default)]
struct PolicyState {
    /// Surcharges valant pour tout le parc (au-dessus des défauts intégrés).
    globales: CoucheConfig,
    /// Surcharges propres à chaque organisation (priorité maximale).
    par_org: HashMap<String, CoucheConfig>,
}

/// Magasin de politiques partagé, en mémoire (thread-safe, clonable).
#[derive(Clone, Default)]
pub struct PolicyStore(Arc<Mutex<PolicyState>>);

impl PolicyStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fixe (ou remplace) la politique `key = value` pour l'organisation `org`.
    pub fn set_policy(&self, org: &str, key: &str, value: &str) {
        self.0
            .lock()
            .unwrap()
            .par_org
            .entry(org.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }

    /// Fixe (ou remplace) une surcharge **globale** `key = value`, appliquée à
    /// toutes les organisations sauf surcharge propre.
    pub fn set_global_policy(&self, key: &str, value: &str) {
        self.0
            .lock()
            .unwrap()
            .globales
            .insert(key.to_string(), value.to_string());
    }

    /// Retire la surcharge `key` de l'organisation `org` (retour aux couches
    /// héritées). Renvoie `true` si une surcharge existait.
    pub fn unset_policy(&self, org: &str, key: &str) -> bool {
        let mut etat = self.0.lock().unwrap();
        match etat.par_org.get_mut(org) {
            Some(couche) => couche.remove(key).is_some(),
            None => false,
        }
    }

    /// Configuration **effective** de `org` : défauts intégrés, surchargés par
    /// les politiques globales, elles-mêmes surchargées par celles de l'org.
    #[must_use]
    pub fn effective_config(&self, org: &str) -> HashMap<String, String> {
        let etat = self.0.lock().unwrap();
        let mut effective = defauts_integres();
        effective.extend(etat.globales.clone());
        if let Some(couche_org) = etat.par_org.get(org) {
            effective.extend(couche_org.clone());
        }
        effective
    }

    /// Valeur effective d'une seule politique pour `org` (mêmes couches que
    /// [`Self::effective_config`]). `None` si la clé est inconnue de toutes les couches.
    #[must_use]
    pub fn policy_value(&self, org: &str, key: &str) -> Option<String> {
        let etat = self.0.lock().unwrap();
        etat.par_org
            .get(org)
            .and_then(|couche| couche.get(key))
            .or_else(|| etat.globales.get(key))
            .cloned()
            .or_else(|| defauts_integres().remove(key))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_inconnue_recoit_les_defauts() {
        let store = PolicyStore::new();
        let config = store.effective_config("org-jamais-vue");
        assert_eq!(config, defauts_integres());
        assert_eq!(
            config.get("allow_file_transfer").map(String::as_str),
            Some("true")
        );
        assert_eq!(config.get("require_2fa").map(String::as_str), Some("false"));
        // Clé hors du référentiel : absente, pas de valeur fantôme.
        assert!(!config.contains_key("politique_inexistante"));
        assert_eq!(store.policy_value("org-jamais-vue", "inconnue"), None);
    }

    #[test]
    fn surcharge_par_org_prime_sur_les_defauts() {
        let store = PolicyStore::new();
        store.set_policy("acme", "allow_file_transfer", "false");
        store.set_policy("acme", "require_2fa", "true");

        let config = store.effective_config("acme");
        // Surchargées.
        assert_eq!(
            config.get("allow_file_transfer").map(String::as_str),
            Some("false")
        );
        assert_eq!(config.get("require_2fa").map(String::as_str), Some("true"));
        // Non surchargée : le défaut reste.
        assert_eq!(
            config.get("session_timeout_minutes").map(String::as_str),
            Some("30")
        );
        // Les autres organisations ne voient rien.
        assert_eq!(
            store.policy_value("globex", "allow_file_transfer"),
            Some("true".to_string())
        );
    }

    #[test]
    fn heritage_global_puis_org() {
        let store = PolicyStore::new();
        // Durcissement du parc entier : 2FA partout, sessions courtes.
        store.set_global_policy("require_2fa", "true");
        store.set_global_policy("session_timeout_minutes", "10");
        // Mais « acme » a une dérogation sur la durée de session.
        store.set_policy("acme", "session_timeout_minutes", "60");

        // Org sans surcharge : global > défaut.
        let globex = store.effective_config("globex");
        assert_eq!(globex.get("require_2fa").map(String::as_str), Some("true"));
        assert_eq!(
            globex.get("session_timeout_minutes").map(String::as_str),
            Some("10")
        );

        // Org avec surcharge : org > global > défaut.
        let acme = store.effective_config("acme");
        assert_eq!(acme.get("require_2fa").map(String::as_str), Some("true"));
        assert_eq!(
            acme.get("session_timeout_minutes").map(String::as_str),
            Some("60")
        );
        assert_eq!(
            store.policy_value("acme", "session_timeout_minutes"),
            Some("60".to_string())
        );
    }

    #[test]
    fn remplacement_et_retrait_de_surcharge() {
        let store = PolicyStore::new();
        store.set_policy("acme", "allow_unattended_access", "true");
        assert_eq!(
            store.policy_value("acme", "allow_unattended_access"),
            Some("true".to_string())
        );

        // Remplacement : la dernière valeur gagne.
        store.set_policy("acme", "allow_unattended_access", "false");
        assert_eq!(
            store.policy_value("acme", "allow_unattended_access"),
            Some("false".to_string())
        );

        // Retrait : retour au défaut intégré (« false » aussi, mais hérité).
        assert!(store.unset_policy("acme", "allow_unattended_access"));
        assert!(!store.unset_policy("acme", "allow_unattended_access"));
        assert_eq!(
            store.effective_config("acme"),
            defauts_integres(),
            "après retrait, « acme » doit retomber sur les défauts purs"
        );
    }
}
