//! Encodage **delta** : exploitation des régions modifiées (`CapturedFrame::dirty`).
//!
//! La capture (DXGI, plan 02) annonce les régions modifiées depuis la frame
//! précédente. Ce module fournit la logique **commune aux backends** pour en tirer
//! parti (plan 03 §optimisation desktop) :
//!
//! 1. **Saut d'encodage** ([`SuiviDelta::doit_sauter`]) : si aucune région n'a
//!    changé, l'encodeur émet une *trame de répétition* (chunk à données vides,
//!    voir [`crate::VideoDecoder`]) au lieu de reconvertir et ré-encoder un plein
//!    cadre — zéro octet utile sur le fil, coût CPU quasi nul.
//! 2. **Conversion partielle** ([`rects_pairs_bornes`]) : quand des régions sont
//!    annoncées, seule leur surface est reconvertie (BGRA → YUV) dans le canevas
//!    persistant du backend ; l'encodeur H.264 voit toujours un plein cadre
//!    cohérent (ses macroblocs statiques restent bon marché), mais la conversion
//!    couleur — poste dominant sur écran calme — devient proportionnelle à la
//!    surface réellement modifiée.
//! 3. **Image-clé adaptative** ([`SuiviDelta::keyframe_apres_repos`]) : après une
//!    longue période statique, un grand changement (bascule d'application, page
//!    entière) déclenche une image-clé — point de resynchronisation quasi gratuit
//!    (le plein cadre doit de toute façon être ré-encodé) qui borne la propagation
//!    d'erreurs accumulées pendant le repos.
//!
//! ## Pourquoi un mode **opt-in** ([`crate::VideoEncoder::set_delta_mode`])
//!
//! `dirty` vide est ambigu : chez DXGI cela signifie « rien n'a changé », mais les
//! sources synthétiques (tests, bancs, futurs capteurs macOS/Linux) laissent le
//! champ vide alors que le contenu change. De plus, le capteur DXGI actuel ne
//! rapporte que `GetFrameDirtyRects` — les régions *déplacées*
//! (`GetFrameMoveRects`, défilement) ne sont pas encore fusionnées dans `dirty`
//! (voir nd-capture). Le mode delta ne doit donc être activé que par un appelant
//! qui garantit que sa source renseigne **fidèlement** toutes les régions
//! modifiées. Par défaut, le comportement historique (plein cadre) est conservé.
//!
//! ## Limite ROI documentée
//!
//! Ni openh264 ni le MFT H.264 de Microsoft n'exposent d'encodage restreint à une
//! région d'intérêt (ROI) : l'encodeur travaille toujours plein cadre. Le gain
//! delta porte donc sur (a) le saut complet des trames inchangées et (b) la
//! conversion couleur partielle ; la détection de macroblocs statiques reste à la
//! charge de l'encodeur (mode `ScreenContentRealTime`, très efficace sur canevas
//! stable). Un vrai ROI matériel viendra avec NVENC (lot ultérieur, plan 03/16).

use nd_capture::{CapturedFrame, Rect};

/// Nombre de trames sautées consécutives à partir duquel un grand changement
/// déclenche une image-clé de resynchronisation (≈ 1 s de repos à 30 fps).
pub(crate) const SAUTS_AVANT_RESYNC: u32 = 30;

/// Part minimale de l'image (en %) que doit couvrir le changement pour déclencher
/// l'image-clé de resynchronisation après repos.
pub(crate) const PCT_AIRE_RESYNC: u64 = 50;

/// Rectangle borné à l'image et aligné sur la grille 2×2 du sous-échantillonnage
/// chroma 4:2:0 : `x`/`y` arrondis au pair inférieur, bords droit/bas au pair
/// supérieur (sans déborder — les dimensions d'image sont paires, contrat des
/// encodeurs H.264 de cette crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RectPair {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl RectPair {
    /// Rectangle couvrant l'image entière (conversion pleine).
    pub(crate) fn plein(largeur: usize, hauteur: usize) -> Self {
        Self {
            x: 0,
            y: 0,
            w: largeur,
            h: hauteur,
        }
    }

    /// Aire du rectangle, en pixels.
    pub(crate) fn aire(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// Normalise les régions modifiées pour la conversion partielle : chaque `Rect`
/// est borné à l'image `largeur`×`hauteur` (paires), aligné sur la grille 2×2
/// (4:2:0), et les rectangles vides après bornage sont éliminés.
///
/// Les recouvrements éventuels ne sont **pas** fusionnés : reconvertir deux fois
/// un pixel est idempotent, et les listes DXGI sont courtes — la fusion coûterait
/// plus qu'elle ne rapporte.
pub(crate) fn rects_pairs_bornes(dirty: &[Rect], largeur: u32, hauteur: u32) -> Vec<RectPair> {
    let (lw, lh) = (u64::from(largeur), u64::from(hauteur));
    let mut sortie = Vec::with_capacity(dirty.len());
    for r in dirty {
        // Coin haut-gauche : borné puis arrondi au pair inférieur.
        let x0 = u64::from(r.x).min(lw) & !1;
        let y0 = u64::from(r.y).min(lh) & !1;
        // Coin bas-droit : borné puis arrondi au pair supérieur (reste ≤ lw/lh,
        // qui sont paires).
        let x1 = (u64::from(r.x).saturating_add(u64::from(r.w)).min(lw) + 1) & !1;
        let y1 = (u64::from(r.y).saturating_add(u64::from(r.h)).min(lh) + 1) & !1;
        if x1 > x0 && y1 > y0 {
            sortie.push(RectPair {
                x: x0 as usize,
                y: y0 as usize,
                w: (x1 - x0) as usize,
                h: (y1 - y0) as usize,
            });
        }
    }
    sortie
}

/// Somme des aires d'une liste de rectangles normalisés, bornée à `aire_max`.
/// Les recouvrements sont comptés plusieurs fois : c'est une **borne haute**
/// suffisante pour l'heuristique d'image-clé (jamais pour de la facturation
/// d'octets).
pub(crate) fn aire_totale(rects: &[RectPair], aire_max: u64) -> u64 {
    rects
        .iter()
        .fold(0u64, |acc, r| acc.saturating_add(r.aire()))
        .min(aire_max)
}

/// État partagé du mode delta d'un encodeur : activation, comptage des trames
/// réellement encodées (une image-clé doit exister avant tout saut) et des sauts
/// consécutifs (heuristique d'image-clé après repos).
#[derive(Debug, Default)]
pub(crate) struct SuiviDelta {
    /// Mode delta activé par l'appelant ([`crate::VideoEncoder::set_delta_mode`]).
    actif: bool,
    /// Trames réellement passées à l'encodeur depuis le dernier `configure`.
    encodees: u64,
    /// Trames sautées consécutivement (remis à zéro à chaque encodage réel).
    sauts_consecutifs: u32,
}

impl SuiviDelta {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Active/désactive le mode delta (sans toucher aux compteurs).
    pub(crate) fn set_actif(&mut self, actif: bool) {
        self.actif = actif;
    }

    /// Vrai si le mode delta est activé.
    pub(crate) fn actif(&self) -> bool {
        self.actif
    }

    /// Remise à zéro des compteurs (à chaque `configure` : nouveau flux, le
    /// décodeur distant repart d'une image-clé). Le mode actif est conservé.
    pub(crate) fn reinitialiser(&mut self) {
        self.encodees = 0;
        self.sauts_consecutifs = 0;
    }

    /// Décide du **saut d'encodage** : vrai si le mode delta est actif, que le
    /// canevas du backend est à jour (`canevas_compatible`), qu'au moins une trame
    /// réelle a déjà été émise (le décodeur distant a de quoi répéter), qu'aucune
    /// image-clé n'est exigée et que la capture n'annonce **aucune** région
    /// modifiée. Fonctionne aussi pour les frames sans pixels (`image: None`,
    /// délai de capture écoulé sans changement).
    pub(crate) fn doit_sauter(
        &self,
        frame: &CapturedFrame,
        force_keyframe: bool,
        canevas_compatible: bool,
    ) -> bool {
        self.actif
            && canevas_compatible
            && !force_keyframe
            && self.encodees > 0
            && frame.dirty.is_empty()
    }

    /// Heuristique d'**image-clé après repos** : après [`SAUTS_AVANT_RESYNC`]
    /// sauts consécutifs, un changement couvrant au moins [`PCT_AIRE_RESYNC`] %
    /// de l'image déclenche une image-clé (voir doc de module).
    pub(crate) fn keyframe_apres_repos(&self, aire_modifiee: u64, aire_image: u64) -> bool {
        self.sauts_consecutifs >= SAUTS_AVANT_RESYNC
            && aire_modifiee.saturating_mul(100) >= aire_image.saturating_mul(PCT_AIRE_RESYNC)
    }

    /// À appeler après chaque trame sautée.
    pub(crate) fn note_saut(&mut self) {
        self.sauts_consecutifs = self.sauts_consecutifs.saturating_add(1);
    }

    /// À appeler après chaque trame réellement encodée.
    pub(crate) fn note_encodage(&mut self) {
        self.encodees = self.encodees.saturating_add(1);
        self.sauts_consecutifs = 0;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nd_capture::PixelFormat;
    use nd_proto::MonitorId;

    /// Frame minimale de test (sans pixels — seul `dirty` compte ici).
    fn frame(dirty: Vec<Rect>) -> CapturedFrame {
        CapturedFrame {
            width: 64,
            height: 64,
            monitor: MonitorId(0),
            format: PixelFormat::Bgra8,
            dirty,
            cursor: None,
            timestamp_us: 0,
            image: None,
        }
    }

    fn r(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    /// Bornage et alignement pair : coordonnées impaires étendues vers l'extérieur,
    /// rectangles débordants ramenés dans l'image, rectangles vides éliminés.
    #[test]
    fn rects_normalises_pairs_et_bornes() {
        // Rect impair 3,5 (13×9) → étendu à 2,4 (14×10).
        assert_eq!(
            rects_pairs_bornes(&[r(3, 5, 13, 9)], 64, 64),
            vec![RectPair {
                x: 2,
                y: 4,
                w: 14,
                h: 10
            }]
        );
        // Débordement à droite/en bas → borné à l'image.
        assert_eq!(
            rects_pairs_bornes(&[r(60, 62, 100, 100)], 64, 64),
            vec![RectPair {
                x: 60,
                y: 62,
                w: 4,
                h: 2
            }]
        );
        // Rect entièrement hors image ou de surface nulle → éliminé.
        assert!(rects_pairs_bornes(&[r(64, 0, 8, 8)], 64, 64).is_empty());
        assert!(rects_pairs_bornes(&[r(10, 10, 0, 8)], 64, 64).is_empty());
        // Débordement arithmétique (x + w > u32::MAX) → borné sans panique.
        assert_eq!(
            rects_pairs_bornes(&[r(2, 2, u32::MAX, u32::MAX)], 64, 64),
            vec![RectPair {
                x: 2,
                y: 2,
                w: 62,
                h: 62
            }]
        );
    }

    /// L'aire totale est une borne haute plafonnée à l'aire de l'image.
    #[test]
    fn aire_totale_bornee() {
        let rects = vec![RectPair::plein(64, 64), RectPair::plein(64, 64)];
        assert_eq!(aire_totale(&rects, 64 * 64), 64 * 64);
        assert_eq!(
            aire_totale(
                &[RectPair {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 6
                }],
                64 * 64
            ),
            24
        );
        assert_eq!(aire_totale(&[], 64 * 64), 0);
    }

    /// Machine à états du saut : inactif par défaut, exige un canevas à jour, une
    /// première trame réelle, pas d'image-clé forcée et un `dirty` vide.
    #[test]
    fn doit_sauter_exige_toutes_les_conditions() {
        let mut suivi = SuiviDelta::new();
        let statique = frame(Vec::new());
        let bouge = frame(vec![r(0, 0, 8, 8)]);

        // Inactif (défaut) : jamais de saut, même conditions réunies.
        suivi.note_encodage();
        assert!(!suivi.doit_sauter(&statique, false, true));

        suivi.set_actif(true);
        assert!(suivi.doit_sauter(&statique, false, true));
        // Chaque condition manquante interdit le saut.
        assert!(
            !suivi.doit_sauter(&statique, true, true),
            "image-clé forcée"
        );
        assert!(
            !suivi.doit_sauter(&statique, false, false),
            "canevas périmé"
        );
        assert!(!suivi.doit_sauter(&bouge, false, true), "régions modifiées");
        suivi.reinitialiser();
        assert!(
            !suivi.doit_sauter(&statique, false, true),
            "aucune trame encodée depuis configure"
        );
    }

    /// Image-clé après repos : déclenchée seulement après SAUTS_AVANT_RESYNC sauts
    /// consécutifs ET un changement couvrant PCT_AIRE_RESYNC % de l'image.
    #[test]
    fn keyframe_apres_repos_seuils() {
        let mut suivi = SuiviDelta::new();
        suivi.set_actif(true);
        suivi.note_encodage();

        let aire_image = 64 * 64u64;
        let grande = aire_image; // 100 %
        let petite = aire_image / 4; // 25 % < seuil de 50 %

        for _ in 0..SAUTS_AVANT_RESYNC - 1 {
            suivi.note_saut();
        }
        assert!(
            !suivi.keyframe_apres_repos(grande, aire_image),
            "un saut de moins que le seuil : pas de resynchronisation"
        );
        suivi.note_saut();
        assert!(suivi.keyframe_apres_repos(grande, aire_image));
        assert!(
            !suivi.keyframe_apres_repos(petite, aire_image),
            "changement trop petit : pas d'image-clé"
        );

        // Un encodage réel remet le compteur de sauts à zéro.
        suivi.note_encodage();
        assert!(!suivi.keyframe_apres_repos(grande, aire_image));
    }
}
