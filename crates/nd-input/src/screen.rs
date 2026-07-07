//! Cartographie **multi-écran** des coordonnées de souris absolues, **agnostique
//! de l'OS**.
//!
//! Le contrôleur envoie une position de souris normalisée `(fx, fy)` dans
//! `[0, 1]` **relative à un moniteur donné** ([`nd_proto::MonitorId`]). Chaque
//! backend d'injection (Windows `SendInput`, macOS `CGEventPost`, Linux XTEST)
//! doit projeter ce point vers l'espace de coordonnées de sa plateforme en
//! tenant compte du **rectangle du bon écran** dans le bureau virtuel (offsets
//! multi-moniteur, éventuellement négatifs pour un écran à gauche/au-dessus du
//! principal).
//!
//! Ce module isole ce calcul — purement arithmétique, sans le moindre appel
//! système — afin qu'il soit **testé sur toutes les plateformes** (y compris
//! Windows, où macOS/Linux ne compilent pas). Chaque backend se contente
//! d'énumérer ses moniteurs ([`MonitorRect`]) puis d'appeler [`point_absolu`] ;
//! Windows ajoute la conversion vers l'espace normalisé `0..=65535`
//! ([`pixel_vers_normalise_65535`]) attendu par `SendInput`.
//!
//! Voir `../../plan-technique/07-injection-entrees.md` §multi-écran et
//! `../../plan-technique/13-fonctionnalites-avancees.md`.

use nd_proto::MonitorId;

/// Rectangle d'un moniteur dans le **bureau virtuel**, en pixels (macOS : en
/// points de l'espace global — même arithmétique).
///
/// `x`/`y` peuvent être négatifs : un écran secondaire placé à gauche ou
/// au-dessus du principal a une origine négative dans le bureau virtuel.
/// L'ordre de la tranche de `MonitorRect` fixe la correspondance avec
/// [`MonitorId`] : `MonitorId(i)` = `i`-ième moniteur énuméré par le backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MonitorRect {
    /// Index d'énumération du moniteur (= `MonitorId.0`).
    pub id: u32,
    /// Abscisse du coin haut-gauche dans le bureau virtuel (peut être négative).
    pub x: i32,
    /// Ordonnée du coin haut-gauche dans le bureau virtuel (peut être négative).
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Sélectionne le moniteur cible par identifiant, avec **repli sûr** sur le
/// premier moniteur énuméré si l'identifiant est absent (jamais de panique :
/// un `MonitorId` périmé après un hotplug ne doit pas casser l'injection).
fn moniteur_cible(moniteurs: &[MonitorRect], monitor: MonitorId) -> Option<&MonitorRect> {
    moniteurs
        .iter()
        .find(|m| m.id == monitor.0)
        .or_else(|| moniteurs.first())
}

/// Projette un point normalisé `(fx, fy)` (`[0, 1]`, relatif au moniteur
/// `monitor`) en un **point absolu** `(x, y)` en pixels du bureau virtuel.
///
/// `fx`/`fy` sont bornés à `[0, 1]` (un point hors écran est ramené sur le
/// bord). L'échelle utilise `largeur - 1` / `hauteur - 1` de sorte que `0.0`
/// vise le premier pixel et `1.0` le dernier pixel du moniteur (bords exacts).
/// Renvoie `None` uniquement si la liste de moniteurs est vide.
pub(crate) fn point_absolu(
    moniteurs: &[MonitorRect],
    monitor: MonitorId,
    fx: f64,
    fy: f64,
) -> Option<(i32, i32)> {
    let m = moniteur_cible(moniteurs, monitor)?;
    // `width`/`height` valent au moins 1 pour un vrai moniteur ; `saturating_sub`
    // protège le cas dégénéré d'un rectangle de dimension nulle.
    let etendue_x = f64::from(m.width.saturating_sub(1));
    let etendue_y = f64::from(m.height.saturating_sub(1));
    let px = f64::from(m.x) + fx.clamp(0.0, 1.0) * etendue_x;
    let py = f64::from(m.y) + fy.clamp(0.0, 1.0) * etendue_y;
    Some((px.round() as i32, py.round() as i32))
}

/// Bornes du **bureau virtuel** (union de tous les moniteurs) : origine et
/// dimensions en pixels. Sous Windows, données par `GetSystemMetrics`
/// (`SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN`/
/// `SM_CYVIRTUALSCREEN`).
///
/// Spécifique à Windows (`SendInput` normalise sur le bureau virtuel) ; inutilisé
/// hors Windows où le point absolu suffit — d'où le `allow(dead_code)` ciblé.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BureauVirtuel {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Résolution de l'espace normalisé absolu de `SendInput` (0..=65535 inclus).
const PLEINE_ECHELLE: f64 = 65535.0;

/// Convertit un **point absolu** (pixels du bureau virtuel) vers les
/// coordonnées normalisées `0..=65535` attendues par `SendInput` en mode
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
///
/// Le coin haut-gauche du bureau virtuel correspond à `0`, le coin bas-droite à
/// `65535`. Le résultat est borné à `[0, 65535]` (un point hors bureau est
/// ramené sur le bord). L'échelle utilise `dimension - 1` pour faire coïncider
/// exactement les pixels extrêmes avec `0` et `65535`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pixel_vers_normalise_65535(px: i32, py: i32, bureau: BureauVirtuel) -> (i32, i32) {
    let norm = |valeur: i32, origine: i32, dimension: u32| -> i32 {
        let etendue = f64::from(dimension.saturating_sub(1)).max(1.0);
        let rel = f64::from(valeur - origine) / etendue;
        (rel.clamp(0.0, 1.0) * PLEINE_ECHELLE).round() as i32
    };
    (
        norm(px, bureau.x, bureau.width),
        norm(py, bureau.y, bureau.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deux 1920×1080 côte à côte : le centre de chaque écran tombe bien dans
    /// son propre rectangle (offset horizontal honoré).
    #[test]
    fn deux_ecrans_cote_a_cote() {
        let moniteurs = [
            MonitorRect {
                id: 0,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                id: 1,
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        // Centre de l'écran principal.
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(0), 0.5, 0.5),
            Some((960, 540))
        );
        // Centre de l'écran secondaire : décalé de 1920 px.
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 0.5, 0.5),
            Some((2880, 540))
        );
        // Coin haut-gauche du secondaire.
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 0.0, 0.0),
            Some((1920, 0))
        );
        // Coin bas-droite du secondaire (bords exacts).
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 1.0, 1.0),
            Some((3839, 1079))
        );
    }

    /// Écran secondaire à **gauche** du principal : origine négative honorée.
    #[test]
    fn ecran_secondaire_offset_negatif() {
        let moniteurs = [
            MonitorRect {
                id: 0,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                id: 1,
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 0.0, 0.0),
            Some((-1920, 0))
        );
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 1.0, 1.0),
            Some((-1, 1079))
        );
        // -1920 + 0.5·1919 = -960,5 → arrondi au plus loin de zéro = -961.
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 0.5, 0.5),
            Some((-961, 540))
        );
    }

    /// Écrans empilés verticalement (secondaire au-dessus, offset négatif en y).
    #[test]
    fn ecrans_empiles_verticalement() {
        let moniteurs = [
            MonitorRect {
                id: 0,
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
            MonitorRect {
                id: 1,
                x: 0,
                y: -1080,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 0.0, 0.0),
            Some((0, -1080))
        );
        // x : 0 + 0,5·1919 = 959,5 → 960 ; y : -1080 + 0,5·1079 = -540,5 → -541.
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 0.5, 0.5),
            Some((960, -541))
        );
    }

    /// Résolutions hétérogènes : le mapping suit les dimensions propres de
    /// chaque écran (pas celles du principal).
    #[test]
    fn resolutions_heterogenes() {
        let moniteurs = [
            MonitorRect {
                id: 0,
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
            },
            MonitorRect {
                id: 1,
                x: 3840,
                y: 0,
                width: 1280,
                height: 720,
            },
        ];
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(1), 1.0, 1.0),
            Some((3840 + 1279, 719))
        );
    }

    /// Identifiant de moniteur inconnu (hotplug périmé) : repli sûr sur le
    /// premier moniteur, jamais de panique ni de `None`.
    #[test]
    fn moniteur_inconnu_repli_sur_le_premier() {
        let moniteurs = [MonitorRect {
            id: 0,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(7), 0.5, 0.5),
            Some((960, 540))
        );
    }

    /// Liste vide : aucune cible possible (l'appelant gère l'erreur).
    #[test]
    fn liste_vide_donne_none() {
        assert_eq!(point_absolu(&[], MonitorId(0), 0.5, 0.5), None);
    }

    /// Les coordonnées hors `[0, 1]` sont ramenées sur le bord de l'écran visé.
    #[test]
    fn coordonnees_hors_bornes_saturees() {
        let moniteurs = [MonitorRect {
            id: 0,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(
            point_absolu(&moniteurs, MonitorId(0), -0.5, 2.0),
            Some((0, 1079))
        );
    }

    /// Normalisation 65535 sur un bureau à un seul écran : les bords tombent
    /// exactement sur `0` et `65535`, le centre au milieu.
    #[test]
    fn normalise_65535_ecran_unique() {
        let bureau = BureauVirtuel {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(pixel_vers_normalise_65535(0, 0, bureau), (0, 0));
        assert_eq!(
            pixel_vers_normalise_65535(1919, 1079, bureau),
            (65535, 65535)
        );
        let (cx, _) = pixel_vers_normalise_65535(960, 540, bureau);
        assert!((32_750..=32_785).contains(&cx), "centre normalisé : {cx}");
    }

    /// Normalisation 65535 sur un bureau virtuel à **origine négative** (écran
    /// secondaire à gauche) : le point le plus à gauche vaut `0`, le plus à
    /// droite `65535`, et l'origine du principal tombe au milieu de la plage.
    #[test]
    fn normalise_65535_bureau_origine_negative() {
        // Deux 1920 côte à côte, le secondaire à gauche : bureau [-1920, 1920).
        let bureau = BureauVirtuel {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        assert_eq!(pixel_vers_normalise_65535(-1920, 0, bureau).0, 0);
        assert_eq!(pixel_vers_normalise_65535(1919, 0, bureau).0, 65535);
        // Origine (0,0) du principal ≈ moitié droite de la plage.
        let (mx, _) = pixel_vers_normalise_65535(0, 0, bureau);
        assert!((32_760..=32_790).contains(&mx), "milieu normalisé : {mx}");
    }

    /// Chaîne complète Windows : point normalisé sur l'écran secondaire →
    /// pixels bureau virtuel → normalisé 65535. Le résultat vise bien la moitié
    /// droite de la plage (l'écran secondaire est à droite).
    #[test]
    fn chaine_complete_windows_ecran_secondaire() {
        let moniteurs = [
            MonitorRect {
                id: 0,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                id: 1,
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        let bureau = BureauVirtuel {
            x: 0,
            y: 0,
            width: 3840,
            height: 1080,
        };
        let (px, py) = point_absolu(&moniteurs, MonitorId(1), 0.5, 0.5).expect("point");
        let (nx, _ny) = pixel_vers_normalise_65535(px, py, bureau);
        // Centre de l'écran de droite : ~3/4 de la plage horizontale.
        assert!((48_000..=50_500).contains(&nx), "normalisé x = {nx}");
    }
}
