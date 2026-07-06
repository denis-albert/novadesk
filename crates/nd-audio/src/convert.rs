//! Conversions PCM 100 % sûres : décodage des échantillons natifs vers `f32`,
//! mixage vers stéréo et rééchantillonnage linéaire vers la fréquence cible
//! (48 kHz pour Opus, voir plan 08) — et le chemin inverse pour la
//! restitution : stéréo → N voies ([`depuis_stereo`]) et `f32` → octets natifs
//! ([`f32_vers_octets`]).
//!
//! Ce module est indépendant de l'OS : il est testé partout, y compris hors
//! Windows, alors que la capture et la lecture WASAPI elles-mêmes sont
//! cantonnées aux modules Windows.

/// Format d'un échantillon PCM natif tel que fourni par le moteur audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatEchantillon {
    /// IEEE float 32 bits (format de mixage par défaut de WASAPI).
    F32,
    /// PCM entier signé 16 bits.
    I16,
    /// PCM entier signé 32 bits.
    I32,
}

impl FormatEchantillon {
    /// Taille d'un échantillon en octets.
    #[must_use]
    pub fn octets(self) -> usize {
        match self {
            FormatEchantillon::F32 | FormatEchantillon::I32 => 4,
            FormatEchantillon::I16 => 2,
        }
    }
}

/// Décode des octets PCM (petit-boutiste, entrelacés) vers des `f32` dans
/// `[-1, 1]`. Les octets excédentaires d'un échantillon incomplet sont ignorés.
#[must_use]
pub fn octets_vers_f32(octets: &[u8], format: FormatEchantillon) -> Vec<f32> {
    match format {
        FormatEchantillon::F32 => octets
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        FormatEchantillon::I16 => octets
            .chunks_exact(2)
            .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0)
            .collect(),
        FormatEchantillon::I32 => octets
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
            .collect(),
    }
}

/// Ramène un flux entrelacé à `canaux` voies vers du stéréo entrelacé.
///
/// Mono → duplication gauche/droite ; plus de deux voies → seules les deux
/// premières (avant gauche/droite dans l'ordre WAVE) sont conservées. Une
/// frame incomplète en fin de bloc est ignorée.
#[must_use]
pub fn vers_stereo(echantillons: &[f32], canaux: usize) -> Vec<f32> {
    match canaux {
        0 => Vec::new(),
        1 => echantillons.iter().flat_map(|&s| [s, s]).collect(),
        2 => echantillons.to_vec(),
        n => echantillons
            .chunks_exact(n)
            .flat_map(|frame| [frame[0], frame[1]])
            .collect(),
    }
}

/// Déploie un flux **stéréo entrelacé** vers `canaux` voies entrelacées
/// (chemin de restitution, inverse de [`vers_stereo`]).
///
/// Mono → moyenne gauche/droite ; plus de deux voies → gauche/droite sur les
/// deux premières (ordre WAVE), silence sur les autres. Une frame stéréo
/// incomplète en fin de bloc est ignorée.
#[must_use]
pub fn depuis_stereo(echantillons: &[f32], canaux: usize) -> Vec<f32> {
    match canaux {
        0 => Vec::new(),
        1 => echantillons
            .chunks_exact(2)
            .map(|f| (f[0] + f[1]) * 0.5)
            .collect(),
        2 => echantillons.to_vec(),
        n => {
            let frames = echantillons.len() / 2;
            let mut sortie = vec![0.0; frames * n];
            for (i, f) in echantillons.chunks_exact(2).enumerate() {
                sortie[i * n] = f[0];
                sortie[i * n + 1] = f[1];
            }
            sortie
        }
    }
}

/// Encode des `f32` (attendus dans `[-1, 1]`, saturés sinon) vers des octets
/// PCM petit-boutistes du format natif demandé (inverse de [`octets_vers_f32`]).
#[must_use]
pub fn f32_vers_octets(echantillons: &[f32], format: FormatEchantillon) -> Vec<u8> {
    let mut octets = Vec::with_capacity(echantillons.len() * format.octets());
    match format {
        FormatEchantillon::F32 => {
            for &v in echantillons {
                octets.extend_from_slice(&v.to_le_bytes());
            }
        }
        FormatEchantillon::I16 => {
            for &v in echantillons {
                let q = (f64::from(v.clamp(-1.0, 1.0)) * 32_767.0).round() as i16;
                octets.extend_from_slice(&q.to_le_bytes());
            }
        }
        FormatEchantillon::I32 => {
            for &v in echantillons {
                let q = (f64::from(v.clamp(-1.0, 1.0)) * 2_147_483_647.0).round() as i32;
                octets.extend_from_slice(&q.to_le_bytes());
            }
        }
    }
    octets
}

/// Rééchantillonneur linéaire pour flux **stéréo entrelacé**, avec continuité
/// entre blocs : la dernière frame du bloc précédent et la position de lecture
/// fractionnaire sont conservées, si bien qu'un flux découpé en blocs
/// arbitraires produit la même sortie qu'un traitement d'un seul tenant.
///
/// L'interpolation linéaire suffit pour un premier jet (mix système souvent
/// déjà à 48 kHz) ; un filtre polyphase pourra la remplacer plus tard.
pub struct Reechantillonneur {
    frequence_source: u32,
    frequence_cible: u32,
    /// Position de lecture fractionnaire (en frames) dans le flux source
    /// virtuel « frame précédente + bloc courant ».
    position: f64,
    /// Dernière frame source du bloc précédent (gauche, droite).
    precedente: Option<[f32; 2]>,
}

impl Reechantillonneur {
    /// Crée un rééchantillonneur `frequence_source` → `frequence_cible` (Hz).
    #[must_use]
    pub fn new(frequence_source: u32, frequence_cible: u32) -> Self {
        Reechantillonneur {
            frequence_source,
            frequence_cible,
            position: 0.0,
            precedente: None,
        }
    }

    /// Rééchantillonne un bloc stéréo entrelacé. Si les fréquences sont
    /// égales, le bloc est renvoyé tel quel (copie).
    pub fn traiter(&mut self, entree: &[f32]) -> Vec<f32> {
        if self.frequence_source == self.frequence_cible {
            return entree.to_vec();
        }

        // Flux source virtuel : la frame conservée du bloc précédent (indice 0
        // le cas échéant) suivie des frames du bloc courant.
        let mut source: Vec<[f32; 2]> = Vec::with_capacity(entree.len() / 2 + 1);
        if let Some(frame) = self.precedente {
            source.push(frame);
        }
        source.extend(entree.chunks_exact(2).map(|c| [c[0], c[1]]));

        let total = source.len();
        if total < 2 {
            // Pas assez de frames pour interpoler : on mémorise et on attend.
            if let Some(&derniere) = source.last() {
                self.precedente = Some(derniere);
            }
            return Vec::new();
        }

        let pas = f64::from(self.frequence_source) / f64::from(self.frequence_cible);
        let mut sortie = Vec::with_capacity(entree.len() + entree.len() / 8 + 4);
        // On produit tant que l'interpolation dispose des frames i et i+1.
        while self.position < (total - 1) as f64 {
            let i = self.position as usize;
            let t = (self.position - i as f64) as f32;
            let a = source[i];
            let b = source[i + 1];
            sortie.push(a[0] + (b[0] - a[0]) * t);
            sortie.push(a[1] + (b[1] - a[1]) * t);
            self.position += pas;
        }

        // La dernière frame du bloc devient l'indice 0 du bloc suivant.
        self.precedente = Some(source[total - 1]);
        self.position -= (total - 1) as f64;
        sortie
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_i16_extremes() {
        let octets = [
            0x00, 0x80, // i16::MIN → -1.0
            0xFF, 0x7F, // i16::MAX → ~1.0
            0x00, 0x00, // 0 → 0.0
        ];
        let s = octets_vers_f32(&octets, FormatEchantillon::I16);
        assert_eq!(s.len(), 3);
        assert!((s[0] + 1.0).abs() < 1e-6);
        assert!((s[1] - 1.0).abs() < 1e-3);
        assert_eq!(s[2], 0.0);
    }

    #[test]
    fn decode_f32_identite() {
        let valeurs = [0.5f32, -0.25, 1.0];
        let mut octets = Vec::new();
        for v in valeurs {
            octets.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(octets_vers_f32(&octets, FormatEchantillon::F32), valeurs);
    }

    #[test]
    fn decode_i32_extremes() {
        let mut octets = Vec::new();
        octets.extend_from_slice(&i32::MIN.to_le_bytes());
        octets.extend_from_slice(&0i32.to_le_bytes());
        let s = octets_vers_f32(&octets, FormatEchantillon::I32);
        assert!((s[0] + 1.0).abs() < 1e-6);
        assert_eq!(s[1], 0.0);
    }

    #[test]
    fn mixage_mono_vers_stereo() {
        assert_eq!(vers_stereo(&[0.1, 0.2], 1), vec![0.1, 0.1, 0.2, 0.2]);
    }

    #[test]
    fn mixage_quad_vers_stereo() {
        // 5.1 tronqué : seules les deux premières voies sont conservées.
        let quad = [1.0, 2.0, 9.0, 9.0, 3.0, 4.0, 9.0, 9.0];
        assert_eq!(vers_stereo(&quad, 4), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn deploiement_stereo_vers_mono_moyenne() {
        let s = depuis_stereo(&[0.2, 0.4, -1.0, 1.0], 1);
        assert_eq!(s.len(), 2);
        assert!((s[0] - 0.3).abs() < 1e-6);
        assert!(s[1].abs() < 1e-6);
    }

    #[test]
    fn deploiement_stereo_identite() {
        let bloc = [0.1f32, -0.2, 0.3, -0.4];
        assert_eq!(depuis_stereo(&bloc, 2), bloc.to_vec());
    }

    #[test]
    fn deploiement_stereo_vers_six_voies() {
        // 5.1 : gauche/droite sur les deux premières voies, silence ailleurs.
        let s = depuis_stereo(&[0.1, 0.2, 0.3, 0.4], 6);
        assert_eq!(
            s,
            vec![0.1, 0.2, 0.0, 0.0, 0.0, 0.0, 0.3, 0.4, 0.0, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn encode_f32_aller_retour() {
        let valeurs = [0.5f32, -0.25, 1.0, 0.0];
        let octets = f32_vers_octets(&valeurs, FormatEchantillon::F32);
        assert_eq!(octets_vers_f32(&octets, FormatEchantillon::F32), valeurs);
    }

    #[test]
    fn encode_i16_extremes_et_saturation() {
        // -1 → -32767, +1 → +32767, 0 → 0 ; hors gamme → saturé.
        let octets = f32_vers_octets(&[-1.0, 1.0, 0.0, 2.0, -2.0], FormatEchantillon::I16);
        let attendu: Vec<u8> = [-32_767i16, 32_767, 0, 32_767, -32_767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(octets, attendu);
    }

    #[test]
    fn encode_i32_aller_retour_approche() {
        let valeurs = [0.5f32, -0.5, 0.0];
        let octets = f32_vers_octets(&valeurs, FormatEchantillon::I32);
        let relu = octets_vers_f32(&octets, FormatEchantillon::I32);
        for (a, b) in valeurs.iter().zip(&relu) {
            assert!((a - b).abs() < 1e-6, "aller-retour i32 : {a} vs {b}");
        }
    }

    #[test]
    fn reechantillonnage_identite() {
        let mut r = Reechantillonneur::new(48_000, 48_000);
        let bloc = [0.1f32, 0.2, 0.3, 0.4];
        assert_eq!(r.traiter(&bloc), bloc.to_vec());
    }

    #[test]
    fn reechantillonnage_44100_vers_48000_longueur() {
        let mut r = Reechantillonneur::new(44_100, 48_000);
        // 1 s de stéréo à 44,1 kHz, en blocs de 441 frames.
        let bloc = vec![0.5f32; 441 * 2];
        let mut total_frames = 0usize;
        for _ in 0..100 {
            total_frames += r.traiter(&bloc).len() / 2;
        }
        // ~48 000 frames attendues (à une frame de bord près).
        assert!(
            (47_990..=48_010).contains(&total_frames),
            "frames produites : {total_frames}"
        );
    }

    #[test]
    fn reechantillonnage_continuite_entre_blocs() {
        // Une rampe rééchantillonnée reste une rampe : pas de discontinuité
        // aux frontières de blocs.
        let rampe: Vec<f32> = (0..2 * 480).map(|i| (i / 2) as f32 / 480.0).collect();
        let mut r = Reechantillonneur::new(24_000, 48_000);
        let mut sortie = Vec::new();
        for bloc in rampe.chunks(2 * 60) {
            sortie.extend_from_slice(&r.traiter(bloc));
        }
        for paire in sortie.chunks_exact(2).collect::<Vec<_>>().windows(2) {
            let delta = paire[1][0] - paire[0][0];
            assert!(
                (0.0..=0.002).contains(&delta),
                "discontinuité détectée : delta = {delta}"
            );
        }
    }
}
