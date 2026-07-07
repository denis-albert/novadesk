//! Négociation de codec entre pairs et échelle de débit adaptatif (ABR) — plan 03.
//!
//! Pendant la poignée de main (plan 05), chaque pair annonce le résultat de
//! [`available_encoders`] ; [`negotiate`] choisit ensuite le codec du flux vidéo.
//! En session, la couche transport (plan 04) alimente [`BitrateLadder`] avec ses
//! estimations réseau (bande passante, RTT, pertes) pour adapter débit, résolution
//! et cadence — dégradation gracieuse, sans oscillation (hystérésis).

use crate::{CodecCaps, CodecKind, EncoderConfig, VideoEncoder};

// ---------------------------------------------------------------------------
// 1. Inventaire des encodeurs disponibles
// ---------------------------------------------------------------------------

/// Inventaire des encodeurs réellement disponibles sur cette machine.
///
/// - Le repli logiciel openh264 (H.264) est **toujours** présent : il ne dépend
///   d'aucune ressource plateforme.
/// - L'encodeur plateforme (Windows : Media Foundation, H.264) n'est annoncé que si
///   sa création réussit effectivement ici via [`crate::create_hardware_encoder`] —
///   on n'annonce jamais au pair distant une capacité « sur le papier » qui
///   échouerait en session. Son champ [`CodecCaps::hardware`] reflète ses capacités
///   réelles (voir `mediafoundation` : le premier jet est un MFT logiciel).
///
/// L'ordre du vecteur n'a pas de signification : la sélection passe par
/// [`negotiate`], qui agrège toutes les entrées.
#[must_use]
pub fn available_encoders() -> Vec<CodecCaps> {
    let mut encodeurs = vec![crate::software::Openh264Encoder::capabilities()];

    #[cfg(windows)]
    if crate::create_hardware_encoder(CodecKind::H264).is_ok() {
        encodeurs.push(crate::mediafoundation::MediaFoundationEncoder::capabilities());
    }

    encodeurs
}

// ---------------------------------------------------------------------------
// 2. Négociation du codec
// ---------------------------------------------------------------------------

/// Ordre de préférence des codecs, du meilleur au moins bon (plan 03) :
/// AV1 (libre de redevances, meilleure compression) > H.265 > H.264 (socle
/// universel) > VP9 (repli secondaire).
const ORDRE_PREFERENCE: [CodecKind; 4] = [
    CodecKind::Av1,
    CodecKind::H265,
    CodecKind::H264,
    CodecKind::Vp9,
];

/// `Some(materiel)` si `kind` est supporté par au moins une entrée de `caps`
/// (`materiel` vaut vrai si au moins une de ces entrées est accélérée), `None` si
/// le codec n'est pas supporté du tout.
fn support(caps: &[CodecCaps], kind: CodecKind) -> Option<bool> {
    let mut trouve = false;
    let mut materiel = false;
    for c in caps {
        if c.kinds.contains(&kind) {
            trouve = true;
            materiel |= c.hardware;
        }
    }
    trouve.then_some(materiel)
}

/// Choisit le meilleur codec commun aux deux inventaires de capacités.
///
/// Règle documentée (plan 03) :
/// 1. Ne sont candidats que les codecs présents **des deux côtés**.
/// 2. Le **matériel prime** : chaque candidat reçoit un score 0..=2 (nombre de
///    côtés où il est accéléré). Pour du bureau à distance, la latence d'encodage
///    l'emporte sur le gain de compression — H.264 matériel des deux côtés bat un
///    AV1 purement logiciel.
/// 3. À score matériel égal, l'ordre de préférence tranche :
///    AV1 > H.265 > H.264 > VP9 (constante `ORDRE_PREFERENCE`).
///
/// Renvoie `None` si l'intersection est vide (aucun codec commun).
#[must_use]
pub fn negotiate(local: &[CodecCaps], remote: &[CodecCaps]) -> Option<CodecKind> {
    let mut meilleur: Option<(u8, CodecKind)> = None;
    for &kind in &ORDRE_PREFERENCE {
        let (Some(mat_local), Some(mat_distant)) = (support(local, kind), support(remote, kind))
        else {
            continue;
        };
        let score = u8::from(mat_local) + u8::from(mat_distant);
        // Strictement supérieur : à score égal, le mieux classé (rencontré en
        // premier) est conservé.
        if meilleur.is_none_or(|(s, _)| score > s) {
            meilleur = Some((score, kind));
        }
    }
    meilleur.map(|(_, kind)| kind)
}

// ---------------------------------------------------------------------------
// 3. Échelle de débit adaptatif (ABR)
// ---------------------------------------------------------------------------

/// Estimation de l'état du réseau, fournie par la couche transport (plan 04).
#[derive(Debug, Clone, Copy)]
pub struct NetworkEstimate {
    /// Bande passante disponible estimée, en kbit/s.
    pub bandwidth_kbps: u32,
    /// Aller-retour réseau (RTT), en millisecondes.
    pub rtt_ms: u32,
    /// Taux de perte de paquets, dans [0, 1].
    pub loss: f32,
}

impl NetworkEstimate {
    /// Construit l'estimation depuis les champs d'un `PathEstimate` de
    /// nd-transport (`rtt_us`, `loss_ratio`, `estimated_bandwidth_kbps`) — champ à
    /// champ, pour que nd-core fasse le pont **sans** que nd-codec dépende de
    /// nd-transport. Le RTT est converti en millisecondes (saturé) et la perte
    /// ramenée dans [0, 1].
    #[must_use]
    pub fn from_path(rtt_us: u64, loss_ratio: f32, estimated_bandwidth_kbps: u32) -> Self {
        Self {
            bandwidth_kbps: estimated_bandwidth_kbps,
            rtt_ms: u32::try_from(rtt_us / 1_000).unwrap_or(u32::MAX),
            loss: if loss_ratio.is_finite() {
                loss_ratio.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }
}

/// Nature du contenu affiché, pilotant l'axe de dégradation (plan 03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentProfile {
    /// Bureautique/texte : la netteté prime — on dégrade d'abord la cadence (fps)
    /// et on préserve la résolution le plus longtemps possible.
    Text,
    /// Contenu animé (vidéo, jeu) : la fluidité prime — on dégrade d'abord la
    /// résolution et on préserve la cadence le plus longtemps possible.
    Video,
}

/// Part de la bande passante estimée réellement allouée à la vidéo (le reste
/// absorbe audio, canal de contrôle et retransmissions).
const MARGE_BUDGET_PCT: u32 = 80;

/// Marge d'hystérésis pour **monter** d'un palier : le budget doit couvrir ce
/// pourcentage du débit du palier visé (125 % = 25 % de marge au-dessus du strict
/// nécessaire), sinon on reste où l'on est.
const MARGE_MONTEE_PCT: u32 = 125;

/// Nombre d'évaluations consécutives favorables exigées avant de monter d'un
/// palier (une éclaircie isolée ne déclenche pas de remontée).
const SEUIL_MONTEE: u32 = 3;

/// Plancher du débit cible, en kbit/s (en dessous, l'image n'est plus exploitable).
const DEBIT_PLANCHER_KBPS: u32 = 100;

/// Un palier de dégradation, exprimé en pourcentages de la configuration de base.
/// L'axe dégradé dépend du profil : cadence pour [`ContentProfile::Text`],
/// résolution pour [`ContentProfile::Video`].
struct Palier {
    /// Débit cible, en % du débit de base.
    debit_pct: u32,
    /// Cadence en % (profil Texte) — dégradée en premier pour garder le texte net.
    fps_pct_texte: u32,
    /// Échelle de résolution en % par axe (profil Texte) — préservée le plus
    /// longtemps, réduite seulement aux paliers extrêmes.
    echelle_pct_texte: u32,
    /// Cadence en % (profil Vidéo) — préservée le plus longtemps pour la fluidité.
    fps_pct_video: u32,
    /// Échelle de résolution en % par axe (profil Vidéo) — dégradée en premier.
    echelle_pct_video: u32,
}

/// Paliers documentés du plan 03, du plein régime (0) au plancher (4) :
///
/// | Palier | Débit | Texte (fps / échelle) | Vidéo (fps / échelle) |
/// |-------:|------:|----------------------:|----------------------:|
/// |      0 | 100 % |         100 % / 100 % |         100 % / 100 % |
/// |      1 |  70 % |          75 % / 100 % |          100 % / 85 % |
/// |      2 |  45 % |          50 % / 100 % |          100 % / 70 % |
/// |      3 |  25 % |           30 % / 75 % |           75 % / 50 % |
/// |      4 |  12 % |           15 % / 50 % |           50 % / 35 % |
#[rustfmt::skip]
const PALIERS: [Palier; 5] = [
    Palier { debit_pct: 100, fps_pct_texte: 100, echelle_pct_texte: 100, fps_pct_video: 100, echelle_pct_video: 100 },
    Palier { debit_pct:  70, fps_pct_texte:  75, echelle_pct_texte: 100, fps_pct_video: 100, echelle_pct_video:  85 },
    Palier { debit_pct:  45, fps_pct_texte:  50, echelle_pct_texte: 100, fps_pct_video: 100, echelle_pct_video:  70 },
    Palier { debit_pct:  25, fps_pct_texte:  30, echelle_pct_texte:  75, fps_pct_video:  75, echelle_pct_video:  50 },
    Palier { debit_pct:  12, fps_pct_texte:  15, echelle_pct_texte:  50, fps_pct_video:  50, echelle_pct_video:  35 },
];

/// Met `dim` à l'échelle (`pct` % par axe) et l'arrondit à la valeur **paire**
/// inférieure (contrainte du sous-échantillonnage chroma 4:2:0), avec un plancher
/// de 16 px.
fn dimension_paire(dim: u32, pct: u32) -> u32 {
    ((dim.saturating_mul(pct) / 100) & !1).max(16)
}

/// Convertit l'estimation réseau en budget vidéo exploitable (kbit/s) :
///
/// 1. Marge de sécurité : seuls [`MARGE_BUDGET_PCT`] % de la bande passante estimée
///    sont alloués à la vidéo.
/// 2. Pénalité de perte : 5 points de budget en moins par % de perte, plafonnée à
///    50 % (au-delà de 10 % de perte, c'est la correction d'erreurs qui prime).
/// 3. Pénalité de latence : RTT ≤ 100 ms → plein budget ; ≤ 250 ms → 90 % ;
///    au-delà → 75 % (un RTT élevé ralentit la boucle de régulation, on encode
///    donc plus prudemment).
fn budget_video_kbps(estimation: NetworkEstimate) -> u32 {
    let brut = estimation.bandwidth_kbps.saturating_mul(MARGE_BUDGET_PCT) / 100;
    let penalite_perte_pct = (f64::from(estimation.loss.clamp(0.0, 1.0)) * 500.0).min(50.0);
    let apres_perte = (f64::from(brut) * (100.0 - penalite_perte_pct) / 100.0) as u32;
    let facteur_rtt_pct = match estimation.rtt_ms {
        0..=100 => 100,
        101..=250 => 90,
        _ => 75,
    };
    apres_perte.saturating_mul(facteur_rtt_pct) / 100
}

/// Échelle de débit adaptatif (ABR) du plan 03 : à partir des estimations réseau,
/// choisit un palier de dégradation et produit l'[`EncoderConfig`] cible que
/// l'appelant applique via [`VideoEncoder::configure`] /
/// [`VideoEncoder::set_target_bitrate`].
///
/// ## Dégradation gracieuse
///
/// La configuration de base (`base`) est le plein régime ; chaque palier en dérive
/// par pourcentages (voir la table de `PALIERS`). L'axe sacrifié dépend du
/// [`ContentProfile`] : cadence d'abord pour le texte (netteté), résolution
/// d'abord pour la vidéo (fluidité).
///
/// ## Anti-oscillation (hystérésis)
///
/// - **Descente immédiate**, éventuellement de plusieurs paliers : à la moindre
///   congestion, mieux vaut dégrader tout de suite que laisser gonfler files
///   d'attente et latence.
/// - **Montée prudente** : un seul palier à la fois, uniquement après
///   `SEUIL_MONTEE` évaluations consécutives dont le budget couvre
///   `MARGE_MONTEE_PCT` % du débit du palier visé. Une estimation qui oscille
///   autour du seuil ne provoque donc aucun yo-yo.
#[derive(Debug)]
pub struct BitrateLadder {
    /// Configuration plein régime (palier 0) dont dérivent tous les paliers.
    base: EncoderConfig,
    profil: ContentProfile,
    /// Indice du palier courant dans `PALIERS` (0 = plein régime).
    courant: usize,
    /// Évaluations consécutives favorables à une montée (hystérésis).
    votes_montee: u32,
}

impl BitrateLadder {
    /// Crée l'échelle sur la configuration « plein régime » `base`. L'échelle
    /// démarre optimiste (palier 0) ; le premier [`Self::update`] descend
    /// immédiatement si le réseau ne suit pas.
    #[must_use]
    pub fn new(base: EncoderConfig, profil: ContentProfile) -> Self {
        Self {
            base,
            profil,
            courant: 0,
            votes_montee: 0,
        }
    }

    /// Indice du palier courant (0 = plein régime, 4 = plancher). Exposé pour
    /// l'observabilité (journaux/tests) ; les appelants consomment plutôt
    /// [`Self::current_config`].
    #[must_use]
    pub fn palier(&self) -> usize {
        self.courant
    }

    /// Configuration d'encodage cible du palier courant. Les dimensions sont
    /// toujours paires (contrainte 4:2:0) et bornées à 16 px minimum ; la cadence
    /// vaut au moins 1 fps et le débit au moins `DEBIT_PLANCHER_KBPS`.
    #[must_use]
    pub fn current_config(&self) -> EncoderConfig {
        let palier = &PALIERS[self.courant];
        let (fps_pct, echelle_pct) = match self.profil {
            ContentProfile::Text => (palier.fps_pct_texte, palier.echelle_pct_texte),
            ContentProfile::Video => (palier.fps_pct_video, palier.echelle_pct_video),
        };
        EncoderConfig {
            kind: self.base.kind,
            width: dimension_paire(self.base.width, echelle_pct),
            height: dimension_paire(self.base.height, echelle_pct),
            target_bitrate_kbps: self.debit_palier_kbps(self.courant),
            max_fps: (self.base.max_fps.saturating_mul(fps_pct) / 100).max(1),
        }
    }

    /// Intègre une estimation réseau et renvoie la configuration cible (voir la
    /// politique d'hystérésis dans la doc de la struct).
    pub fn update(&mut self, estimation: NetworkEstimate) -> EncoderConfig {
        let budget = budget_video_kbps(estimation);
        let cible = self.palier_pour_budget(budget);

        if cible > self.courant {
            // Descente immédiate (l'interactivité prime sur la qualité).
            self.courant = cible;
            self.votes_montee = 0;
        } else if cible < self.courant {
            // Montée prudente : un palier à la fois, avec marge d'hystérésis.
            let vise = self.courant - 1;
            let requis = self
                .debit_palier_kbps(vise)
                .saturating_mul(MARGE_MONTEE_PCT)
                / 100;
            if budget >= requis {
                self.votes_montee += 1;
                if self.votes_montee >= SEUIL_MONTEE {
                    self.courant = vise;
                    self.votes_montee = 0;
                }
            } else {
                // Le budget vise plus haut mais sans la marge : on ne bouge pas.
                self.votes_montee = 0;
            }
        } else {
            // Conditions stables au palier courant : le compteur repart de zéro.
            self.votes_montee = 0;
        }

        self.current_config()
    }

    /// Débit cible (kbit/s) du palier `palier`, borné par `DEBIT_PLANCHER_KBPS`.
    fn debit_palier_kbps(&self, palier: usize) -> u32 {
        (self
            .base
            .target_bitrate_kbps
            .saturating_mul(PALIERS[palier].debit_pct)
            / 100)
            .max(DEBIT_PLANCHER_KBPS)
    }

    /// Palier le plus haut (indice le plus bas) dont le débit tient dans `budget` ;
    /// à défaut, le plancher (dernier palier).
    fn palier_pour_budget(&self, budget_kbps: u32) -> usize {
        (0..PALIERS.len())
            .find(|&palier| self.debit_palier_kbps(palier) <= budget_kbps)
            .unwrap_or(PALIERS.len() - 1)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Fabrique un `CodecCaps` de test.
    fn caps(hardware: bool, kinds: &[CodecKind]) -> CodecCaps {
        CodecCaps {
            hardware,
            kinds: kinds.to_vec(),
            max_width: 3840,
            max_height: 2160,
        }
    }

    /// Configuration plein régime de référence : 1080p60 à 8 Mbit/s.
    fn base_1080p60() -> EncoderConfig {
        EncoderConfig {
            kind: CodecKind::H264,
            width: 1920,
            height: 1080,
            target_bitrate_kbps: 8_000,
            max_fps: 60,
        }
    }

    fn estimation(bandwidth_kbps: u32, rtt_ms: u32, loss: f32) -> NetworkEstimate {
        NetworkEstimate {
            bandwidth_kbps,
            rtt_ms,
            loss,
        }
    }

    // ----- Inventaire -----

    /// L'inventaire annonce toujours au moins le repli logiciel openh264 (H.264),
    /// et chaque entrée annoncée est cohérente. Sur Windows, l'entrée Media
    /// Foundation n'apparaît que si sa création a réellement réussi.
    #[test]
    fn inventaire_annonce_toujours_h264_logiciel() {
        let inventaire = available_encoders();
        assert!(!inventaire.is_empty());
        assert!(!inventaire[0].hardware, "l'entrée 0 est le repli logiciel");
        for caps in &inventaire {
            assert!(caps.kinds.contains(&CodecKind::H264));
            assert!(caps.max_width > 0 && caps.max_height > 0);
        }
        #[cfg(windows)]
        assert_eq!(
            inventaire.len(),
            1 + usize::from(crate::create_hardware_encoder(CodecKind::H264).is_ok()),
            "l'entrée Media Foundation doit refléter la réussite de sa création"
        );
    }

    // ----- Négociation -----

    /// Intersection vide (codecs disjoints ou inventaire vide) → `None`.
    #[test]
    fn negociation_intersection_vide() {
        let h264 = [caps(false, &[CodecKind::H264])];
        let av1 = [caps(false, &[CodecKind::Av1])];
        assert_eq!(negotiate(&h264, &av1), None);
        assert_eq!(negotiate(&[], &h264), None);
        assert_eq!(negotiate(&h264, &[]), None);
        assert_eq!(negotiate(&[], &[]), None);
    }

    /// À conditions matérielles égales, l'ordre AV1 > H.265 > H.264 > VP9 tranche.
    #[test]
    fn negociation_respecte_la_preference() {
        let tous = [caps(
            false,
            &[
                CodecKind::H264,
                CodecKind::Av1,
                CodecKind::Vp9,
                CodecKind::H265,
            ],
        )];
        assert_eq!(negotiate(&tous, &tous), Some(CodecKind::Av1));

        let sans_av1 = [caps(
            false,
            &[CodecKind::H264, CodecKind::Vp9, CodecKind::H265],
        )];
        assert_eq!(negotiate(&tous, &sans_av1), Some(CodecKind::H265));

        let h264_vp9 = [caps(false, &[CodecKind::Vp9, CodecKind::H264])];
        assert_eq!(negotiate(&h264_vp9, &sans_av1), Some(CodecKind::H264));

        let vp9 = [caps(false, &[CodecKind::Vp9])];
        assert_eq!(negotiate(&vp9, &tous), Some(CodecKind::Vp9));

        // Même en tout-matériel, la préférence départage les scores égaux.
        let tous_hw = [caps(true, &[CodecKind::H264, CodecKind::Av1])];
        assert_eq!(negotiate(&tous_hw, &tous_hw), Some(CodecKind::Av1));
    }

    /// Un codec accéléré des deux côtés bat un codec mieux classé mais logiciel.
    #[test]
    fn negociation_materiel_deux_cotes_bat_la_preference() {
        let cote = [
            caps(true, &[CodecKind::H264]),
            caps(false, &[CodecKind::Av1]),
        ];
        assert_eq!(negotiate(&cote, &cote), Some(CodecKind::H264));
    }

    /// Le matériel d'un seul côté (score 1) bat aussi le tout-logiciel (score 0),
    /// et les entrées multiples d'un même inventaire sont bien agrégées.
    #[test]
    fn negociation_materiel_un_seul_cote_et_agregation() {
        let local = [
            caps(false, &[CodecKind::H264, CodecKind::Av1]),
            caps(true, &[CodecKind::H264]),
        ];
        let distant = [caps(false, &[CodecKind::H264, CodecKind::Av1])];
        assert_eq!(negotiate(&local, &distant), Some(CodecKind::H264));
    }

    // ----- ABR -----

    /// Le pont `PathEstimate` → `NetworkEstimate` convertit unités et bornes :
    /// µs → ms (saturé), perte ramenée dans [0, 1] (NaN → 0), bande passante 1:1.
    #[test]
    fn from_path_convertit_unites_et_bornes() {
        let e = NetworkEstimate::from_path(35_500, 0.02, 12_000);
        assert_eq!(e.rtt_ms, 35, "35 500 µs → 35 ms");
        assert_eq!(e.bandwidth_kbps, 12_000);
        assert!((e.loss - 0.02).abs() < f32::EPSILON);

        let sature = NetworkEstimate::from_path(u64::MAX, 7.5, u32::MAX);
        assert_eq!(sature.rtt_ms, u32::MAX, "RTT saturé sans panique");
        assert_eq!(sature.loss, 1.0, "perte bornée à 1");
        let nan = NetworkEstimate::from_path(0, f32::NAN, 100);
        assert_eq!(nan.loss, 0.0, "NaN neutralisé");
        assert_eq!(nan.rtt_ms, 0);
    }

    /// Bonne bande passante → palier 0 : la configuration cible est exactement la
    /// configuration de base (plein débit, plein fps, résolution native).
    #[test]
    fn abr_bonne_bande_passante_plein_regime() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Text);
        let cfg = abr.update(estimation(20_000, 20, 0.0));
        assert_eq!(abr.palier(), 0);
        assert_eq!(cfg, base_1080p60());
    }

    /// Profil Texte à bande passante moyenne : la résolution native est préservée,
    /// la cadence et le débit baissent.
    #[test]
    fn abr_profil_texte_garde_la_resolution_et_baisse_le_fps() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Text);
        // Budget : 5 000 × 0,8 = 4 000 kbit/s → palier 2 (3 600 ≤ 4 000 < 5 600).
        let cfg = abr.update(estimation(5_000, 20, 0.0));
        assert_eq!(abr.palier(), 2);
        assert_eq!(
            (cfg.width, cfg.height),
            (1920, 1080),
            "résolution préservée"
        );
        assert_eq!(cfg.max_fps, 30, "cadence dégradée (50 % de 60)");
        assert_eq!(cfg.target_bitrate_kbps, 3_600, "45 % de 8 000");
    }

    /// Profil Vidéo au même palier : la cadence est préservée, la résolution baisse
    /// (dimensions paires).
    #[test]
    fn abr_profil_video_garde_le_fps_et_baisse_la_resolution() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Video);
        let cfg = abr.update(estimation(5_000, 20, 0.0));
        assert_eq!(abr.palier(), 2);
        assert_eq!(cfg.max_fps, 60, "cadence préservée");
        assert_eq!((cfg.width, cfg.height), (1344, 756), "70 % de 1920×1080");
        assert_eq!(cfg.target_bitrate_kbps, 3_600);
    }

    /// À bande passante égale, pertes et RTT élevés réduisent le palier.
    #[test]
    fn abr_perte_et_rtt_reduisent_le_palier() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Text);
        abr.update(estimation(12_000, 20, 0.0));
        assert_eq!(abr.palier(), 0, "12 Mbit/s sain → plein régime");

        // Budget : 9 600 × 0,5 (10 % de perte) × 0,75 (RTT 300 ms) = 3 600 → palier 2.
        abr.update(estimation(12_000, 300, 0.10));
        assert_eq!(abr.palier(), 2);
    }

    /// Une estimation qui oscille autour du seuil ne fait pas osciller le palier :
    /// la descente est immédiate, la remontée exige la marge d'hystérésis.
    #[test]
    fn abr_hysterese_empeche_le_yoyo() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Text);
        // Palier 0 exige 8 000 kbit/s de budget ; « moyen » n'en donne que 6 400.
        let moyen = estimation(8_000, 30, 0.0);
        // « bon » donne 8 400 : assez pour viser le palier 0, mais PAS la marge de
        // montée (8 000 × 125 % = 10 000).
        let bon = estimation(10_500, 30, 0.0);

        abr.update(moyen);
        assert_eq!(abr.palier(), 1, "descente immédiate");

        for _ in 0..6 {
            abr.update(bon);
            assert_eq!(abr.palier(), 1, "pas de remontée sans marge d'hystérésis");
            abr.update(moyen);
            assert_eq!(abr.palier(), 1, "pas de redescente : le palier tient");
        }

        // Conditions franchement bonnes ET stables : remontée après SEUIL_MONTEE
        // évaluations consécutives, pas avant.
        let excellent = estimation(16_000, 30, 0.0);
        abr.update(excellent);
        assert_eq!(abr.palier(), 1);
        abr.update(excellent);
        assert_eq!(abr.palier(), 1);
        abr.update(excellent);
        assert_eq!(abr.palier(), 0, "remontée au 3e vote consécutif");
    }

    /// Une rechute remet le compteur de montée à zéro : il faut des évaluations
    /// favorables CONSÉCUTIVES.
    #[test]
    fn abr_rechute_remet_le_compteur_de_montee_a_zero() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Text);
        let moyen = estimation(8_000, 30, 0.0);
        let excellent = estimation(16_000, 30, 0.0);

        abr.update(moyen);
        assert_eq!(abr.palier(), 1);

        abr.update(excellent);
        abr.update(excellent); // deux votes de montée…
        abr.update(moyen); // …annulés par la rechute
        abr.update(excellent);
        abr.update(excellent);
        assert_eq!(abr.palier(), 1, "2 votes après rechute : pas encore");
        abr.update(excellent);
        assert_eq!(abr.palier(), 0, "3 votes consécutifs : remontée");
    }

    /// La descente peut sauter plusieurs paliers d'un coup (jusqu'au plancher),
    /// mais la remontée se fait un palier à la fois.
    #[test]
    fn abr_montee_progressive_un_palier_a_la_fois() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Video);
        abr.update(estimation(500, 30, 0.0));
        assert_eq!(abr.palier(), 4, "effondrement → plancher direct");

        let excellent = estimation(16_000, 30, 0.0);
        for _ in 0..3 {
            abr.update(excellent);
        }
        assert_eq!(
            abr.palier(),
            3,
            "un seul palier gagné malgré l'excellent réseau"
        );
        for _ in 0..3 {
            abr.update(excellent);
        }
        assert_eq!(abr.palier(), 2, "puis le suivant, après 3 nouveaux votes");
    }

    /// Bande passante minuscule : plancher, avec une configuration encore valide.
    #[test]
    fn abr_plancher_bande_passante_minuscule() {
        let mut abr = BitrateLadder::new(base_1080p60(), ContentProfile::Text);
        let cfg = abr.update(estimation(50, 500, 0.30));
        assert_eq!(abr.palier(), 4);
        assert_eq!((cfg.width, cfg.height), (960, 540), "50 % de 1920×1080");
        assert_eq!(cfg.max_fps, 9, "15 % de 60");
        assert_eq!(cfg.target_bitrate_kbps, 960, "12 % de 8 000");
    }

    /// Quel que soit le palier ou le profil, la configuration produite reste
    /// exploitable : dimensions paires ≥ 16 px, cadence ≥ 1, débit ≥ plancher —
    /// même avec une base aux dimensions impaires.
    #[test]
    fn abr_configs_toujours_valides() {
        let base = EncoderConfig {
            kind: CodecKind::H264,
            width: 1917,
            height: 1077,
            target_bitrate_kbps: 400,
            max_fps: 5,
        };
        for profil in [ContentProfile::Text, ContentProfile::Video] {
            let mut abr = BitrateLadder::new(base, profil);
            for palier in 0..PALIERS.len() {
                abr.courant = palier;
                let cfg = abr.current_config();
                assert_eq!(cfg.width % 2, 0, "largeur paire (palier {palier})");
                assert_eq!(cfg.height % 2, 0, "hauteur paire (palier {palier})");
                assert!(cfg.width >= 16 && cfg.height >= 16);
                assert!(cfg.max_fps >= 1);
                assert!(cfg.target_bitrate_kbps >= DEBIT_PLANCHER_KBPS);
                assert_eq!(cfg.kind, base.kind);
            }
        }
    }
}
