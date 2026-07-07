//! Contrôleur de débit adaptatif : la **boucle fermée** ABR → encodeur (plan 03/04).
//!
//! [`crate::BitrateLadder`] savait déjà choisir un palier, mais personne ne
//! l'appelait dans le chemin d'encodage. [`RateController`] ferme la boucle :
//! l'appelant (nd-core) lui transmet chaque estimation réseau de la couche
//! transport (`PathEstimate`, plan 04) convertie en [`NetworkEstimate`] (voir
//! [`NetworkEstimate::from_path`] — nd-codec ne dépend pas de nd-transport), et le
//! contrôleur applique le débit du palier à l'encodeur via
//! [`VideoEncoder::set_target_bitrate`] — désormais réel dans les deux backends.
//!
//! ## Périmètre appliqué / recommandé
//!
//! Seul le **débit** est appliqué à chaud : c'est le levier sans rupture de flux
//! (pas d'image-clé parasite, pas de reconfiguration). La cadence et l'échelle de
//! résolution du palier sont **renvoyées à l'appelant** dans l'[`EncoderConfig`]
//! cible : les appliquer exige d'agir en amont de l'encodeur (rythme de capture,
//! redimensionnement des frames) — hors de portée de nd-codec, qui reçoit des
//! frames déjà cadencées et dimensionnées.

use crate::{BitrateLadder, ContentProfile, EncoderConfig, NetworkEstimate, VideoEncoder};

/// Contrôleur ABR : intègre les estimations réseau (hystérésis de
/// [`BitrateLadder`] : descente immédiate, remontée prudente) et pilote le débit
/// de l'encodeur. Une instance par flux vidéo, à côté de l'encodeur.
#[derive(Debug)]
pub struct RateController {
    echelle: BitrateLadder,
    /// Dernier débit réellement transmis à l'encodeur (évite les appels redondants
    /// à chaque estimation quand le palier ne bouge pas).
    dernier_debit_kbps: Option<u32>,
}

impl RateController {
    /// Crée le contrôleur sur la configuration « plein régime » `base` (celle
    /// passée à [`VideoEncoder::configure`]) et le profil de contenu.
    #[must_use]
    pub fn new(base: EncoderConfig, profil: ContentProfile) -> Self {
        Self {
            echelle: BitrateLadder::new(base, profil),
            dernier_debit_kbps: None,
        }
    }

    /// Intègre une estimation réseau et applique le débit cible du palier retenu à
    /// `encodeur` (uniquement s'il a changé). Renvoie la configuration cible
    /// complète du palier — débit appliqué, cadence/résolution recommandées à
    /// l'appelant (voir doc de module).
    pub fn apply_network_estimate(
        &mut self,
        encodeur: &mut dyn VideoEncoder,
        estimation: NetworkEstimate,
    ) -> EncoderConfig {
        let cible = self.echelle.update(estimation);
        if self.dernier_debit_kbps != Some(cible.target_bitrate_kbps) {
            encodeur.set_target_bitrate(cible.target_bitrate_kbps);
            self.dernier_debit_kbps = Some(cible.target_bitrate_kbps);
        }
        cible
    }

    /// Indice du palier courant (0 = plein régime, 4 = plancher) — observabilité.
    #[must_use]
    pub fn palier(&self) -> usize {
        self.echelle.palier()
    }

    /// Configuration cible du palier courant (sans intégrer de nouvelle
    /// estimation).
    #[must_use]
    pub fn current_config(&self) -> EncoderConfig {
        self.echelle.current_config()
    }

    /// Dernier débit transmis à l'encodeur, s'il y en a un — observabilité/tests.
    #[must_use]
    pub fn last_applied_bitrate_kbps(&self) -> Option<u32> {
        self.dernier_debit_kbps
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CodecCaps, CodecKind, EncodedChunk};
    use nd_capture::CapturedFrame;
    use nd_proto::{NdError, Result};

    /// Encodeur témoin : enregistre les consignes de débit reçues.
    struct EncodeurTemoin {
        debits_recus: Vec<u32>,
    }

    impl EncodeurTemoin {
        fn new() -> Self {
            Self {
                debits_recus: Vec::new(),
            }
        }
    }

    impl VideoEncoder for EncodeurTemoin {
        fn capabilities() -> CodecCaps {
            CodecCaps {
                hardware: false,
                kinds: vec![CodecKind::H264],
                max_width: 3840,
                max_height: 2160,
            }
        }

        fn configure(&mut self, _cfg: EncoderConfig) -> Result<()> {
            Ok(())
        }

        fn encode(
            &mut self,
            _frame: &CapturedFrame,
            _force_keyframe: bool,
        ) -> Result<EncodedChunk> {
            Err(NdError::NotImplemented("encodeur témoin : pas d'encodage"))
        }

        fn set_target_bitrate(&mut self, kbps: u32) {
            self.debits_recus.push(kbps);
        }
    }

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

    /// La boucle est fermée : une congestion fait redescendre le débit transmis à
    /// l'encodeur, un retour au calme le fait remonter (après l'hystérésis), et
    /// aucune consigne redondante n'est envoyée quand le palier ne bouge pas.
    #[test]
    fn controleur_pilote_le_debit_de_l_encodeur() {
        let mut enc = EncodeurTemoin::new();
        let mut rc = RateController::new(base_1080p60(), ContentProfile::Text);

        // Réseau sain : palier 0, une seule consigne (8 000), pas de redite.
        for _ in 0..3 {
            let cfg = rc.apply_network_estimate(&mut enc, estimation(20_000, 20, 0.0));
            assert_eq!(cfg.target_bitrate_kbps, 8_000);
        }
        assert_eq!(enc.debits_recus, vec![8_000]);
        assert_eq!(rc.last_applied_bitrate_kbps(), Some(8_000));

        // Effondrement : descente immédiate (une seule nouvelle consigne).
        let cfg = rc.apply_network_estimate(&mut enc, estimation(1_000, 200, 0.05));
        assert!(rc.palier() > 0);
        assert!(cfg.target_bitrate_kbps < 8_000);
        assert_eq!(enc.debits_recus.len(), 2);
        assert_eq!(enc.debits_recus[1], cfg.target_bitrate_kbps);

        // Retour au calme : remontée prudente palier par palier — le débit
        // transmis à l'encodeur finit par revenir au plein régime.
        for _ in 0..20 {
            rc.apply_network_estimate(&mut enc, estimation(20_000, 20, 0.0));
        }
        assert_eq!(rc.palier(), 0);
        assert_eq!(enc.debits_recus.last(), Some(&8_000));
        // Chaque consigne envoyée est distincte de la précédente (pas de spam).
        for paire in enc.debits_recus.windows(2) {
            assert_ne!(paire[0], paire[1]);
        }
    }
}
