//! VU-mètre et mesure de niveaux PCM `f32` — RMS, crête, dBFS, silence.
//!
//! Fonctions instantanées ([`rms`], [`peak`], [`dbfs`], [`est_silence`]) et
//! [`LevelMeter`] à lissage attaque/relâchement pour un affichage stable
//! (montée rapide sur les transitoires, redescente progressive, comme un
//! VU-mètre matériel).
//!
//! Comme `convert` et `mixing`, ce module est pur PCM : aucune FFI, aucun
//! `unsafe`, testé sur tous les OS (plan 08).

/// Plancher dBFS : valeur renvoyée pour un signal nul (évite `-inf` dans
/// l'interface) et borne basse de [`dbfs`].
pub const DBFS_PLANCHER: f32 = -120.0;

/// Niveau RMS (valeur efficace) d'un bloc, dans `[0, 1]` pour un signal
/// normalisé. Bloc vide → 0.
///
/// L'accumulation se fait en `f64` : la somme des carrés reste précise même
/// sur de longs blocs.
#[must_use]
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let somme: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (somme / samples.len() as f64).sqrt() as f32
}

/// Crête d'un bloc : plus grande amplitude absolue. Bloc vide → 0.
#[must_use]
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// Convertit un niveau linéaire (RMS ou crête, pleine échelle = 1.0) en dBFS.
///
/// `dbfs(1.0) == 0.0` ; toute valeur nulle, négative ou plus basse que le
/// plancher renvoie [`DBFS_PLANCHER`].
#[must_use]
pub fn dbfs(linear: f32) -> f32 {
    if linear <= 0.0 {
        return DBFS_PLANCHER;
    }
    (20.0 * linear.log10()).max(DBFS_PLANCHER)
}

/// Vrai si le niveau RMS du bloc est sous `seuil_dbfs` (ex. −60 dBFS) —
/// détection de silence simple, sans lissage.
#[must_use]
pub fn est_silence(samples: &[f32], seuil_dbfs: f32) -> bool {
    dbfs(rms(samples)) < seuil_dbfs
}

/// VU-mètre à lissage : suit le RMS et la crête bloc par bloc avec des
/// coefficients d'attaque (montée) et de relâchement (descente) distincts.
///
/// Les coefficients s'appliquent **par bloc** (typiquement des trames de
/// 20 ms, voir [`crate::TRAME_MS`]) : `1.0` = suivi instantané, `0.0` = gelé.
/// Une attaque rapide et un relâchement lent donnent l'aiguille classique :
/// réactive aux transitoires, redescente lisible.
#[derive(Debug, Clone)]
pub struct LevelMeter {
    attaque: f32,
    relachement: f32,
    seuil_silence_dbfs: f32,
    rms_lisse: f32,
    peak_lisse: f32,
}

impl Default for LevelMeter {
    fn default() -> Self {
        LevelMeter::new()
    }
}

impl LevelMeter {
    /// Attaque par défaut (montée rapide : 70 % de l'écart comblé par bloc).
    pub const ATTAQUE_DEFAUT: f32 = 0.7;
    /// Relâchement par défaut (descente douce : 15 % de l'écart par bloc,
    /// soit ≈ −3 dB toutes les 2 trames sur un signal coupé net).
    pub const RELACHEMENT_DEFAUT: f32 = 0.15;
    /// Seuil de silence par défaut, en dBFS.
    pub const SEUIL_SILENCE_DEFAUT_DBFS: f32 = -60.0;

    /// VU-mètre avec les coefficients par défaut.
    #[must_use]
    pub fn new() -> Self {
        LevelMeter::avec_coefficients(Self::ATTAQUE_DEFAUT, Self::RELACHEMENT_DEFAUT)
    }

    /// VU-mètre avec des coefficients personnalisés, ramenés dans `[0, 1]`.
    #[must_use]
    pub fn avec_coefficients(attaque: f32, relachement: f32) -> Self {
        LevelMeter {
            attaque: attaque.clamp(0.0, 1.0),
            relachement: relachement.clamp(0.0, 1.0),
            seuil_silence_dbfs: Self::SEUIL_SILENCE_DEFAUT_DBFS,
            rms_lisse: 0.0,
            peak_lisse: 0.0,
        }
    }

    /// Fixe le seuil de silence (dBFS) utilisé par [`LevelMeter::est_silence`].
    pub fn definir_seuil_silence(&mut self, seuil_dbfs: f32) {
        self.seuil_silence_dbfs = seuil_dbfs;
    }

    /// Intègre un bloc PCM : met à jour les niveaux lissés (attaque si le
    /// niveau monte, relâchement s'il descend). Un bloc vide relâche vers 0.
    pub fn traiter(&mut self, echantillons: &[f32]) {
        self.rms_lisse = self.lisser(self.rms_lisse, rms(echantillons));
        self.peak_lisse = self.lisser(self.peak_lisse, peak(echantillons));
    }

    fn lisser(&self, courant: f32, cible: f32) -> f32 {
        let coeff = if cible > courant {
            self.attaque
        } else {
            self.relachement
        };
        courant + coeff * (cible - courant)
    }

    /// Niveau RMS lissé, linéaire (`[0, 1]` pour un signal normalisé).
    #[must_use]
    pub fn rms(&self) -> f32 {
        self.rms_lisse
    }

    /// Crête lissée, linéaire.
    #[must_use]
    pub fn peak(&self) -> f32 {
        self.peak_lisse
    }

    /// Niveau RMS lissé en dBFS (0 = pleine échelle, plancher −120).
    #[must_use]
    pub fn rms_dbfs(&self) -> f32 {
        dbfs(self.rms_lisse)
    }

    /// Crête lissée en dBFS.
    #[must_use]
    pub fn peak_dbfs(&self) -> f32 {
        dbfs(self.peak_lisse)
    }

    /// Vrai si le niveau RMS lissé est sous le seuil de silence.
    #[must_use]
    pub fn est_silence(&self) -> bool {
        self.rms_dbfs() < self.seuil_silence_dbfs
    }

    /// Remet les niveaux lissés à zéro (changement de source, reprise).
    pub fn reinitialiser(&mut self) {
        self.rms_lisse = 0.0;
        self.peak_lisse = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sinusoïde d'amplitude `ampl`, `periodes` périodes de `n` échantillons
    /// (périodes entières ⇒ RMS théorique exact `ampl / √2`).
    fn sinusoide(ampl: f32, n: usize, periodes: usize) -> Vec<f32> {
        (0..n * periodes)
            .map(|i| ampl * (std::f32::consts::TAU * (i % n) as f32 / n as f32).sin())
            .collect()
    }

    #[test]
    fn silence_rms_zero_et_dbfs_plancher() {
        let silence = vec![0.0f32; 960];
        assert_eq!(rms(&silence), 0.0);
        assert_eq!(peak(&silence), 0.0);
        assert_eq!(dbfs(rms(&silence)), DBFS_PLANCHER);
        assert!(est_silence(&silence, -60.0));
        // Bloc vide : mêmes garanties.
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(peak(&[]), 0.0);
    }

    #[test]
    fn pleine_echelle_environ_zero_dbfs() {
        // Signal carré pleine échelle : RMS = crête = 1 → 0 dBFS exactement.
        let carre: Vec<f32> = (0..480)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        assert!((rms(&carre) - 1.0).abs() < 1e-6);
        assert!((peak(&carre) - 1.0).abs() < 1e-6);
        assert!(dbfs(rms(&carre)).abs() < 1e-4);
        assert!(!est_silence(&carre, -60.0));
    }

    #[test]
    fn sinus_amplitude_connue_rms_attendu() {
        // Sinus d'amplitude 0.5 sur des périodes entières : RMS = 0.5/√2.
        let sinus = sinusoide(0.5, 48, 10);
        let attendu = 0.5 / std::f32::consts::SQRT_2;
        assert!(
            (rms(&sinus) - attendu).abs() < 1e-4,
            "rms = {}",
            rms(&sinus)
        );
        assert!((peak(&sinus) - 0.5).abs() < 1e-6);
        // ≈ −9.03 dBFS (−3.01 dB de facteur de crête sous −6.02 dB).
        let db = dbfs(rms(&sinus));
        assert!((db + 9.03).abs() < 0.02, "dbfs = {db}");
    }

    #[test]
    fn dbfs_valeurs_de_reference() {
        assert!(dbfs(1.0).abs() < 1e-6);
        assert!((dbfs(0.5) + 6.020_6).abs() < 1e-3);
        assert!((dbfs(0.1) + 20.0).abs() < 1e-4);
        assert_eq!(dbfs(0.0), DBFS_PLANCHER);
        assert_eq!(dbfs(-0.3), DBFS_PLANCHER);
        // Une valeur infime est bornée au plancher, jamais en dessous.
        assert_eq!(dbfs(1e-30), DBFS_PLANCHER);
    }

    #[test]
    fn level_meter_attaque_puis_relachement() {
        let mut vu = LevelMeter::avec_coefficients(0.5, 0.1);
        let fort: Vec<f32> = vec![1.0; 480]; // RMS = crête = 1

        // Attaque : la moitié de l'écart est comblée au premier bloc.
        vu.traiter(&fort);
        assert!((vu.rms() - 0.5).abs() < 1e-6);
        assert!((vu.peak() - 0.5).abs() < 1e-6);
        // Convergence vers le niveau réel.
        for _ in 0..40 {
            vu.traiter(&fort);
        }
        assert!((vu.rms() - 1.0).abs() < 1e-4);
        assert!(vu.rms_dbfs().abs() < 0.01);

        // Relâchement : la descente est plus lente (10 % de l'écart par bloc).
        let silence = vec![0.0f32; 480];
        vu.traiter(&silence);
        assert!((vu.rms() - 0.9).abs() < 1e-3, "rms = {}", vu.rms());
        // Et progressive : encore bien au-dessus de zéro après 5 blocs.
        for _ in 0..4 {
            vu.traiter(&silence);
        }
        assert!(vu.rms() > 0.5);
    }

    #[test]
    fn level_meter_detection_de_silence_et_reset() {
        let mut vu = LevelMeter::new();
        assert!(vu.est_silence(), "un VU neuf est au silence");
        assert_eq!(vu.rms_dbfs(), DBFS_PLANCHER);

        vu.traiter(&sinusoide(0.5, 48, 10));
        assert!(!vu.est_silence());
        assert!(vu.peak_dbfs() > vu.rms_dbfs(), "crête au-dessus du RMS");

        // Seuil personnalisé : un signal à ≈ −9 dBFS passe sous un seuil à −3.
        vu.definir_seuil_silence(-3.0);
        for _ in 0..40 {
            vu.traiter(&sinusoide(0.5, 48, 10));
        }
        assert!(vu.est_silence());

        vu.reinitialiser();
        assert_eq!(vu.rms(), 0.0);
        assert_eq!(vu.peak(), 0.0);
    }

    #[test]
    fn level_meter_coefficients_bornes() {
        // Coefficients hors [0, 1] ramenés aux bornes : 1.0 = instantané.
        let mut vu = LevelMeter::avec_coefficients(5.0, -1.0);
        vu.traiter(&[1.0, -1.0]);
        assert!((vu.rms() - 1.0).abs() < 1e-6, "attaque bornée à 1.0");
        vu.traiter(&[0.0, 0.0]);
        assert!(
            (vu.rms() - 1.0).abs() < 1e-6,
            "relâchement borné à 0.0 : gelé"
        );
    }
}
