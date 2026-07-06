//! Rideau de confidentialité — pendant une session distante, la machine
//! contrôlée peut masquer son écran physique (écran noir), bloquer les
//! entrées locales (clavier/souris du poste) et couper le fond d'écran.
//!
//! Comme pour [`crate::Permissions`], la règle est la défense en profondeur :
//! l'état est **appliqué côté machine contrôlée** ; l'UI du contrôleur ne fait
//! que le demander. Ce module ne touche pas au système : il modélise l'état
//! ([`PrivacyState`]) et calcule la liste **ordonnée** d'actions concrètes
//! ([`PrivacyAction`]) que la couche plateforme devra exécuter
//! (voir plan 13, §rideau de confidentialité).

/// État du rideau de confidentialité côté machine contrôlée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivacyState {
    /// L'écran physique du poste contrôlé est forcé au noir.
    pub black_screen: bool,
    /// Clavier et souris **locaux** du poste contrôlé sont ignorés.
    pub block_local_input: bool,
    /// Le fond d'écran est retiré (bande passante réduite, discrétion).
    pub disable_wallpaper: bool,
}

/// Action concrète à exécuter côté machine contrôlée pour appliquer une
/// transition d'état du rideau. La couche plateforme (`nd-capture`,
/// `nd-input`, intégration OS) traduit chaque action en appel système.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyAction {
    /// Forcer l'écran physique au noir.
    EnableBlackScreen,
    /// Rétablir l'affichage physique normal.
    DisableBlackScreen,
    /// Ignorer les entrées locales (clavier/souris du poste contrôlé).
    BlockLocalInput,
    /// Rétablir les entrées locales.
    UnblockLocalInput,
    /// Retirer le fond d'écran.
    HideWallpaper,
    /// Restaurer le fond d'écran d'origine.
    RestoreWallpaper,
}

impl PrivacyState {
    /// Rideau levé : aucune mesure de confidentialité active.
    #[must_use]
    pub fn off() -> Self {
        PrivacyState {
            black_screen: false,
            block_local_input: false,
            disable_wallpaper: false,
        }
    }

    /// Rideau complet : écran noir, entrées locales bloquées, fond d'écran coupé.
    #[must_use]
    pub fn curtain() -> Self {
        PrivacyState {
            black_screen: true,
            block_local_input: true,
            disable_wallpaper: true,
        }
    }

    /// Vrai si au moins une mesure de confidentialité est active.
    #[must_use]
    pub fn is_active(self) -> bool {
        self.black_screen || self.block_local_input || self.disable_wallpaper
    }

    /// Calcule les actions à déclencher côté contrôlé pour passer de `self`
    /// à `cible`. Ne renvoie que les actions correspondant à des drapeaux qui
    /// changent (idempotence : transition vers soi-même = aucune action).
    ///
    /// Politique d'application (ordre garanti par les tests) :
    /// 1. les **activations** sont émises avant les désactivations, pour que
    ///    le poste reste couvert au maximum pendant la transition ;
    /// 2. l'écran noir est la **première** protection posée et la **dernière**
    ///    retirée (le masquage prime sur le reste).
    #[must_use]
    pub fn transition_to(self, cible: PrivacyState) -> Vec<PrivacyAction> {
        let mut actions = Vec::new();
        // Activations : l'écran noir d'abord (masquage immédiat).
        if cible.black_screen && !self.black_screen {
            actions.push(PrivacyAction::EnableBlackScreen);
        }
        if cible.block_local_input && !self.block_local_input {
            actions.push(PrivacyAction::BlockLocalInput);
        }
        if cible.disable_wallpaper && !self.disable_wallpaper {
            actions.push(PrivacyAction::HideWallpaper);
        }
        // Désactivations : ordre inverse, l'écran noir est retiré en dernier.
        if !cible.disable_wallpaper && self.disable_wallpaper {
            actions.push(PrivacyAction::RestoreWallpaper);
        }
        if !cible.block_local_input && self.block_local_input {
            actions.push(PrivacyAction::UnblockLocalInput);
        }
        if !cible.black_screen && self.black_screen {
            actions.push(PrivacyAction::DisableBlackScreen);
        }
        actions
    }

    /// Actions de fin de session : tout rétablir, quel que soit l'état.
    ///
    /// À exécuter **systématiquement** à la déconnexion (y compris sur perte
    /// de lien) : le poste contrôlé ne doit jamais rester aveugle et sourd
    /// après le départ du contrôleur.
    #[must_use]
    pub fn release_actions(self) -> Vec<PrivacyAction> {
        self.transition_to(PrivacyState::off())
    }
}

impl Default for PrivacyState {
    fn default() -> Self {
        // Défaut : rideau levé tant que le contrôleur ne demande rien.
        PrivacyState::off()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructeurs_et_defaut() {
        assert!(!PrivacyState::off().is_active());
        assert!(PrivacyState::curtain().is_active());
        assert_eq!(PrivacyState::default(), PrivacyState::off());
        let rideau = PrivacyState::curtain();
        assert!(rideau.black_screen && rideau.block_local_input && rideau.disable_wallpaper);
    }

    #[test]
    fn activation_complete_ordonnee() {
        // Écran noir posé en premier, puis entrées, puis fond d'écran.
        assert_eq!(
            PrivacyState::off().transition_to(PrivacyState::curtain()),
            vec![
                PrivacyAction::EnableBlackScreen,
                PrivacyAction::BlockLocalInput,
                PrivacyAction::HideWallpaper,
            ]
        );
    }

    #[test]
    fn desactivation_complete_ordonnee() {
        // Ordre inverse : l'écran noir est retiré en dernier.
        assert_eq!(
            PrivacyState::curtain().transition_to(PrivacyState::off()),
            vec![
                PrivacyAction::RestoreWallpaper,
                PrivacyAction::UnblockLocalInput,
                PrivacyAction::DisableBlackScreen,
            ]
        );
    }

    #[test]
    fn transition_identite_sans_action() {
        assert!(PrivacyState::off()
            .transition_to(PrivacyState::off())
            .is_empty());
        assert!(PrivacyState::curtain()
            .transition_to(PrivacyState::curtain())
            .is_empty());
    }

    #[test]
    fn transition_partielle() {
        let cible = PrivacyState {
            black_screen: true,
            block_local_input: false,
            disable_wallpaper: false,
        };
        assert_eq!(
            PrivacyState::off().transition_to(cible),
            vec![PrivacyAction::EnableBlackScreen]
        );
        assert!(cible.is_active());
    }

    #[test]
    fn transition_mixte_active_avant_de_desactiver() {
        // Écran noir seul → blocage d'entrées seul : on pose la nouvelle
        // protection avant de retirer l'ancienne (rester couvert).
        let depuis = PrivacyState {
            black_screen: true,
            block_local_input: false,
            disable_wallpaper: false,
        };
        let vers = PrivacyState {
            black_screen: false,
            block_local_input: true,
            disable_wallpaper: false,
        };
        assert_eq!(
            depuis.transition_to(vers),
            vec![
                PrivacyAction::BlockLocalInput,
                PrivacyAction::DisableBlackScreen,
            ]
        );
    }

    #[test]
    fn fin_de_session_retablit_tout() {
        assert_eq!(
            PrivacyState::curtain().release_actions(),
            PrivacyState::curtain().transition_to(PrivacyState::off())
        );
        assert!(PrivacyState::off().release_actions().is_empty());
    }
}
