//! Mixage de flux PCM `f32` — 100 % indépendant de l'OS (plan 08).
//!
//! Mélange plusieurs flux entrelacés (ex. micro + audio système, ou plusieurs
//! participants) par addition pondérée par un gain, protégée par un écrêtage
//! doux (*soft-clip*) : parfaitement linéaire sous [`SEUIL_SOFT_CLIP`], puis
//! compression en `tanh` au-delà, bornée par ±1 — évite la distorsion dure
//! d'un simple `clamp`. Les flux de longueurs différentes sont complétés par
//! du silence.
//!
//! Comme `convert`, ce module est pur PCM : aucune FFI, aucun `unsafe`,
//! testé sur tous les OS.

/// Amplitude en dessous de laquelle l'écrêtage doux est parfaitement linéaire.
///
/// Au-delà, le signal est comprimé en douceur vers ±1 (voir [`soft_clip`]).
pub const SEUIL_SOFT_CLIP: f32 = 0.9;

/// Écrêtage doux d'un échantillon : identité sur `[-SEUIL, +SEUIL]`, puis
/// compression `tanh` de l'excédent, asymptote à ±1.
///
/// La transition est de classe C¹ (pente 1 au seuil) : pas de « marche »
/// audible au point de raccord.
#[must_use]
pub fn soft_clip(x: f32) -> f32 {
    let ampl = x.abs();
    if ampl <= SEUIL_SOFT_CLIP {
        x
    } else {
        // Marge disponible entre le seuil et la pleine échelle : l'excédent y
        // est replié par tanh (tanh'(0) = 1 ⇒ raccord de pente parfait).
        let marge = 1.0 - SEUIL_SOFT_CLIP;
        (SEUIL_SOFT_CLIP + marge * ((ampl - SEUIL_SOFT_CLIP) / marge).tanh()).copysign(x)
    }
}

/// Ajoute `src × gain` dans `dest`, échantillon par échantillon, avec écrêtage
/// doux du résultat.
///
/// - `src` plus court que `dest` : la fin de `dest` est laissée telle quelle
///   (équivalent d'un complément de `src` par du silence) ;
/// - `src` plus long : les échantillons excédentaires sont ignorés (`dest`
///   fixe la taille du bloc) ;
/// - `gain == 0.0` : aucun effet, `dest` est laissé strictement intact.
pub fn mix_into(dest: &mut [f32], src: &[f32], gain: f32) {
    if gain == 0.0 {
        return;
    }
    for (d, &s) in dest.iter_mut().zip(src) {
        *d = soft_clip(*d + s * gain);
    }
}

/// Mélange plusieurs flux PCM en un seul bloc.
///
/// La sortie a la longueur du flux le plus long, les flux plus courts étant
/// complétés par du silence. La somme est d'abord accumulée **linéairement**
/// (pas d'écrêtage intermédiaire qui fausserait le mélange de 3 flux et plus)
/// puis passée une seule fois au [`soft_clip`]. Un gain manquant dans `gains`
/// vaut 1.0 (gain plein) ; les gains excédentaires sont ignorés.
#[must_use]
pub fn mix(streams: &[&[f32]], gains: &[f32]) -> Vec<f32> {
    let longueur = streams.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut somme = vec![0.0f32; longueur];
    for (i, flux) in streams.iter().enumerate() {
        let gain = gains.get(i).copied().unwrap_or(1.0);
        if gain == 0.0 {
            continue;
        }
        for (d, &s) in somme.iter_mut().zip(flux.iter()) {
            *d += s * gain;
        }
    }
    for s in &mut somme {
        *s = soft_clip(*s);
    }
    somme
}

/// Une piste du [`Mixer`] : nom, gain et échantillons en attente de mixage.
#[derive(Debug, Clone)]
struct Piste {
    nom: String,
    gain: f32,
    tampon: Vec<f32>,
}

/// Mixeur à pistes nommées (ex. `"micro"`, `"systeme"`, ou un identifiant de
/// participant), chacune avec son gain propre.
///
/// Usage : déposer les blocs PCM de chaque source via [`Mixer::deposer`],
/// puis récupérer le bloc mélangé (et vider les tampons) via
/// [`Mixer::melanger`]. Les gains et les pistes survivent au mixage ; seuls
/// les échantillons en attente sont consommés.
#[derive(Debug, Clone, Default)]
pub struct Mixer {
    pistes: Vec<Piste>,
}

impl Mixer {
    /// Crée un mixeur sans piste.
    #[must_use]
    pub fn new() -> Self {
        Mixer { pistes: Vec::new() }
    }

    /// Position de la piste `nom`, en la créant (gain 1.0) si absente.
    fn piste_ou_cree(&mut self, nom: &str) -> usize {
        if let Some(i) = self.pistes.iter().position(|p| p.nom == nom) {
            return i;
        }
        self.pistes.push(Piste {
            nom: nom.to_owned(),
            gain: 1.0,
            tampon: Vec::new(),
        });
        self.pistes.len() - 1
    }

    /// Fixe le gain de la piste `nom` (créée avec un tampon vide si absente).
    pub fn definir_gain(&mut self, nom: &str, gain: f32) {
        let i = self.piste_ou_cree(nom);
        self.pistes[i].gain = gain;
    }

    /// Gain courant de la piste `nom`, ou `None` si elle n'existe pas.
    #[must_use]
    pub fn gain(&self, nom: &str) -> Option<f32> {
        self.pistes.iter().find(|p| p.nom == nom).map(|p| p.gain)
    }

    /// Ajoute un bloc d'échantillons à la piste `nom` (créée, gain 1.0, si
    /// absente). Les dépôts successifs s'enchaînent bout à bout.
    pub fn deposer(&mut self, nom: &str, echantillons: &[f32]) {
        let i = self.piste_ou_cree(nom);
        self.pistes[i].tampon.extend_from_slice(echantillons);
    }

    /// Mélange tous les échantillons en attente (somme linéaire pondérée par
    /// les gains puis [`soft_clip`], silence en complément des pistes courtes)
    /// et vide les tampons. Les pistes et leurs gains sont conservés.
    pub fn melanger(&mut self) -> Vec<f32> {
        let longueur = self
            .pistes
            .iter()
            .map(|p| p.tampon.len())
            .max()
            .unwrap_or(0);
        let mut somme = vec![0.0f32; longueur];
        for piste in &mut self.pistes {
            if piste.gain != 0.0 {
                for (d, &s) in somme.iter_mut().zip(piste.tampon.iter()) {
                    *d += s * piste.gain;
                }
            }
            piste.tampon.clear();
        }
        for s in &mut somme {
            *s = soft_clip(*s);
        }
        somme
    }

    /// Nombre d'échantillons en attente sur la piste la plus remplie
    /// (longueur du prochain bloc renvoyé par [`Mixer::melanger`]).
    #[must_use]
    pub fn en_attente(&self) -> usize {
        self.pistes
            .iter()
            .map(|p| p.tampon.len())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sinusoïde d'amplitude `ampl`, une période sur `n` échantillons,
    /// répétée `periodes` fois (le pic exact ±ampl est atteint si n % 4 == 0).
    fn sinusoide(ampl: f32, n: usize, periodes: usize) -> Vec<f32> {
        (0..n * periodes)
            .map(|i| ampl * (std::f32::consts::TAU * (i % n) as f32 / n as f32).sin())
            .collect()
    }

    fn crete(echantillons: &[f32]) -> f32 {
        echantillons.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
    }

    #[test]
    fn soft_clip_lineaire_sous_le_seuil() {
        for &x in &[0.0, 0.25, -0.5, 0.9, -0.9] {
            assert_eq!(soft_clip(x), x, "identité attendue pour {x}");
        }
    }

    #[test]
    fn soft_clip_borne_et_croissant_au_dela_du_seuil() {
        // Jamais au-delà de ±1, même pour des sommes énormes (en f32, tanh
        // sature : l'asymptote ±1 peut être atteinte, jamais dépassée).
        assert!(soft_clip(10.0) <= 1.0);
        assert!(soft_clip(-10.0) >= -1.0);
        // Strictement sous ±1 tant que tanh n'a pas saturé.
        assert!(soft_clip(1.5) < 1.0);
        assert!(soft_clip(-1.5) > -1.0);
        // Doux : 1.0 est comprimé mais reste au-dessus du seuil.
        let y = soft_clip(1.0);
        assert!(y > SEUIL_SOFT_CLIP && y < 1.0, "y = {y}");
        // Monotone croissant (pas de repli qui inverserait la forme d'onde).
        assert!(soft_clip(1.5) > soft_clip(1.2));
        // Raccord continu au seuil.
        assert!((soft_clip(0.900_1) - 0.900_1).abs() < 1e-4);
    }

    #[test]
    fn mix_into_deux_sinusoides_amplitude_attendue() {
        // Deux sinus en phase de 0.4 → sinus de 0.8, zone linéaire :
        // amplitude exacte, aucun dépassement.
        let a = sinusoide(0.4, 48, 10);
        let mut dest = a.clone();
        mix_into(&mut dest, &a, 1.0);
        let pic = crete(&dest);
        assert!((pic - 0.8).abs() < 1e-5, "pic = {pic}");
        for (d, s) in dest.iter().zip(a.iter()) {
            assert!((d - 2.0 * s).abs() < 1e-6);
        }
    }

    #[test]
    fn mix_into_soft_clip_sans_depassement() {
        // Deux sinus pleine échelle : la somme (jusqu'à 2.0) doit être
        // repliée sous ±1 sans jamais dépasser.
        let a = sinusoide(1.0, 48, 4);
        let mut dest = a.clone();
        mix_into(&mut dest, &a, 1.0);
        let pic = crete(&dest);
        assert!(pic <= 1.0, "pic = {pic}");
        assert!(
            pic > SEUIL_SOFT_CLIP,
            "le pic doit approcher 1, pic = {pic}"
        );
    }

    #[test]
    fn mix_into_gain_nul_et_gain_plein() {
        let src = [0.5f32, -0.5, 0.95];
        // Gain nul : dest strictement intact, même au-dessus du seuil.
        let mut dest = [0.1f32, 0.2, 0.95];
        let avant = dest;
        mix_into(&mut dest, &src, 0.0);
        assert_eq!(dest, avant);
        // Gain plein sur un dest silencieux : copie exacte (zone linéaire).
        let mut silence = [0.0f32; 3];
        mix_into(&mut silence, &[0.5, -0.5, 0.25], 1.0);
        assert_eq!(silence, [0.5, -0.5, 0.25]);
        // Demi-gain.
        let mut moitie = [0.0f32; 2];
        mix_into(&mut moitie, &[0.8, -0.4], 0.5);
        assert_eq!(moitie, [0.4, -0.2]);
    }

    #[test]
    fn mix_into_longueurs_differentes() {
        // src plus court : la fin de dest est inchangée.
        let mut dest = [0.1f32, 0.1, 0.1, 0.1];
        mix_into(&mut dest, &[0.2, 0.2], 1.0);
        let attendu = [0.3f32, 0.3, 0.1, 0.1];
        for (d, a) in dest.iter().zip(attendu.iter()) {
            assert!((d - a).abs() < 1e-6);
        }
        // src plus long : l'excédent est ignoré.
        let mut court = [0.0f32; 2];
        mix_into(&mut court, &[0.1, 0.2, 0.3, 0.4], 1.0);
        assert_eq!(court.len(), 2);
        assert!((court[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mix_longueurs_differentes_completees_par_du_silence() {
        let long = [0.1f32, 0.2, 0.3, 0.4];
        let court = [0.5f32, 0.5];
        let sortie = mix(&[&long, &court], &[1.0, 1.0]);
        assert_eq!(sortie.len(), 4);
        assert!((sortie[0] - 0.6).abs() < 1e-6);
        assert!((sortie[1] - 0.7).abs() < 1e-6);
        // Au-delà du flux court : uniquement le flux long (silence ajouté).
        assert!((sortie[2] - 0.3).abs() < 1e-6);
        assert!((sortie[3] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn mix_gains_et_flux_vides() {
        assert!(mix(&[], &[]).is_empty());
        // Gain manquant = 1.0 ; gain nul = piste muette.
        let a = [0.2f32, 0.2];
        let b = [0.4f32, 0.4];
        let sortie = mix(&[&a, &b], &[0.0]);
        // a muet (gain 0), b à gain plein (gain manquant).
        assert!((sortie[0] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn mix_trois_flux_ecretage_unique_en_fin() {
        // 3 × 0.5 = 1.5 → une seule passe de soft-clip sur la somme.
        let flux = [0.5f32; 8];
        let sortie = mix(&[&flux, &flux, &flux], &[1.0, 1.0, 1.0]);
        let attendu = soft_clip(1.5);
        for s in &sortie {
            assert!((s - attendu).abs() < 1e-6);
        }
    }

    #[test]
    fn mixer_pistes_nommees_gains_et_tampons() {
        let mut mixer = Mixer::new();
        assert_eq!(mixer.gain("micro"), None);

        mixer.definir_gain("micro", 0.5);
        mixer.deposer("micro", &[0.4, 0.4]);
        mixer.deposer("systeme", &[0.1, 0.1, 0.1]); // créée à gain 1.0
        assert_eq!(mixer.gain("micro"), Some(0.5));
        assert_eq!(mixer.gain("systeme"), Some(1.0));
        assert_eq!(mixer.en_attente(), 3);

        let bloc = mixer.melanger();
        assert_eq!(bloc.len(), 3);
        assert!((bloc[0] - 0.3).abs() < 1e-6); // 0.4×0.5 + 0.1×1.0
        assert!((bloc[2] - 0.1).abs() < 1e-6); // micro complété par du silence

        // Les tampons sont vidés, les pistes et gains conservés.
        assert_eq!(mixer.en_attente(), 0);
        assert!(mixer.melanger().is_empty());
        assert_eq!(mixer.gain("micro"), Some(0.5));

        // Dépôts successifs mis bout à bout.
        mixer.deposer("micro", &[1.0]);
        mixer.deposer("micro", &[1.0]);
        assert_eq!(mixer.en_attente(), 2);
    }
}
