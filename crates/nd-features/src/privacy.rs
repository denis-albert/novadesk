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
//!
//! # Intégration — l'effet système N'EST PAS implémenté ici
//!
//! Ce module reste volontairement pur calcul. L'exécuteur d'actions vit côté
//! plateforme, et l'orchestrateur (`nd-core`) doit :
//! 1. vérifier [`crate::Capability::PrivacyMode`] via le
//!    [`crate::PermissionBroker`] avant toute transition ;
//! 2. dérouler `transition_to(...)` **dans l'ordre rendu**, en traduisant
//!    chaque [`PrivacyAction`] en appel plateforme :
//!    - `EnableBlackScreen`/`DisableBlackScreen` → nd-capture / fenêtre hôte.
//!      Volet **sans droits admin déjà rendu ici** :
//!      [`PrivacyState::render_screen_cache`] fournit le tampon opaque
//!      ([`ScreenCache`]) que la fenêtre hôte affiche en recouvrement de son
//!      propre bureau. TODO(nd-capture) : extinction de la **sortie physique**
//!      elle-même (`SetDisplayConfig`/DDC sous Windows), hors de portée sans
//!      privilèges — rien ici ;
//!    - `BlockLocalInput`/`UnblockLocalInput` → nd-input. **Exige des droits
//!      élevés** : un blocage robuste (y compris Ctrl+Alt+Suppr / bureau
//!      sécurisé) passe par `BlockInput` ou des hooks bas niveau
//!      `WH_KEYBOARD_LL`/`WH_MOUSE_LL` avec UIAccess/élévation.
//!      TODO(nd-input) : rien ici, faute de privilèges dans ce jet ;
//!    - `HideWallpaper`/`RestoreWallpaper` → intégration OS
//!      (TODO(intégration OS) : `SystemParametersInfo(SPI_SETDESKWALLPAPER)`
//!      avec sauvegarde/restauration ; rien ici) ;
//! 3. exécuter [`PrivacyState::release_actions`] **systématiquement** à la fin
//!    de session, y compris sur perte de lien (le poste ne doit jamais rester
//!    écran noir / entrées bloquées après le départ du contrôleur).

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

    /// Rend le **cache d'écran** de confidentialité à superposer : le tampon
    /// opaque que la fenêtre hôte doit afficher par-dessus le bureau réel tant
    /// que l'écran est masqué. Renvoie `None` (rien à recouvrir) si
    /// `black_screen` est faux.
    ///
    /// C'est le volet du masquage d'écran **réalisable sans droits
    /// administrateur** : recouvrir son propre bureau d'une fenêtre opaque, au
    /// lieu d'éteindre la sortie physique (DDC/CCD, réservé à l'OS — voir le
    /// TODO en tête de module). `largeur`/`hauteur` sont celles de l'écran à
    /// recouvrir.
    #[must_use]
    pub fn render_screen_cache(self, largeur: u32, hauteur: u32) -> Option<ScreenCache> {
        self.black_screen
            .then(|| ScreenCache::opaque_noir(largeur, hauteur))
    }
}

impl Default for PrivacyState {
    fn default() -> Self {
        // Défaut : rideau levé tant que le contrôleur ne demande rien.
        PrivacyState::off()
    }
}

/// Cache d'écran de confidentialité : image que la **fenêtre hôte** affiche
/// par-dessus le bureau réel du poste contrôlé pendant le masquage (volet du
/// rideau réalisable sans privilèges — cf.
/// [`PrivacyState::render_screen_cache`]).
///
/// Format : RGBA 8 bits par canal (`[R, G, B, A]` par pixel, lignes de haut en
/// bas), identique à [`crate::RgbaCanvas`] — directement téléversable en
/// texture par la couche d'affichage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCache {
    largeur: u32,
    hauteur: u32,
    pixels: Vec<u8>,
}

impl ScreenCache {
    /// Cache entièrement noir et **opaque** de `largeur × hauteur` pixels.
    fn opaque_noir(largeur: u32, hauteur: u32) -> Self {
        let mut pixels = vec![0u8; largeur as usize * hauteur as usize * 4];
        // R = G = B = 0 (déjà à zéro) ; seul l'alpha passe à 255 (opaque).
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
        ScreenCache {
            largeur,
            hauteur,
            pixels,
        }
    }

    /// Largeur du cache, en pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.largeur
    }

    /// Hauteur du cache, en pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.hauteur
    }

    /// Les octets RGBA bruts (`largeur × hauteur × 4`), lignes de haut en bas.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Le pixel `(x, y)` sous forme `[R, G, B, A]`, ou `None` hors du cache.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.largeur || y >= self.hauteur {
            return None;
        }
        let base = (y as usize * self.largeur as usize + x as usize) * 4;
        Some([
            self.pixels[base],
            self.pixels[base + 1],
            self.pixels[base + 2],
            self.pixels[base + 3],
        ])
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

    #[test]
    fn cache_ecran_noir_opaque_quand_masque() {
        let etat = PrivacyState {
            black_screen: true,
            block_local_input: false,
            disable_wallpaper: false,
        };
        let cache = etat
            .render_screen_cache(4, 3)
            .expect("écran masqué → un cache à afficher");
        assert_eq!((cache.width(), cache.height()), (4, 3));
        assert_eq!(cache.pixels().len(), 4 * 3 * 4);
        assert_eq!(cache.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(cache.pixel(3, 2), Some([0, 0, 0, 255]));
        assert_eq!(cache.pixel(4, 0), None); // hors cadre
                                             // Tout le tampon est noir opaque.
        assert!(cache.pixels().chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
    }

    #[test]
    fn pas_de_cache_sans_ecran_noir() {
        // Sans `black_screen`, la fenêtre hôte n'a rien à recouvrir, même si
        // d'autres mesures de confidentialité sont actives.
        let etat = PrivacyState {
            black_screen: false,
            block_local_input: true,
            disable_wallpaper: true,
        };
        assert!(etat.render_screen_cache(8, 8).is_none());
        assert!(PrivacyState::off().render_screen_cache(8, 8).is_none());
    }

    #[test]
    fn cache_ecran_dimensions_nulles_sans_panique() {
        let cache = PrivacyState::curtain().render_screen_cache(0, 0).unwrap();
        assert!(cache.pixels().is_empty());
        assert_eq!(cache.pixel(0, 0), None);
    }
}
