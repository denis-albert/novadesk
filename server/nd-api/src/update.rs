//! Service de mises à jour NovaDesk — versions, canaux et manifestes **en mémoire**.
//!
//! Le serveur publie un [`UpdateManifest`] par canal ([`ReleaseChannel`]) ; les
//! clients interrogent [`UpdateService::check_update`] avec leur version courante
//! et reçoivent une [`UpdateDecision`] : à jour, mise à jour disponible, ou mise
//! à jour **forcée** si leur version est passée sous `min_supported`.
//! Le déploiement progressif ([`rollout_bucket`]) répartit les appareils dans
//! des seaux 0..100 par hachage déterministe : un appareil est servi dès que le
//! pourcentage de rollout atteint son seau, et le reste ensuite (monotone).
//! Voir `../../plan-technique/15-securite-operationnelle.md` et plan 11.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Version sémantique
// ---------------------------------------------------------------------------

/// Version sémantique `major.minor.patch` (ex. « 1.2.3 »).
///
/// L'ordre dérivé (`Ord`) est lexicographique sur (major, minor, patch), ce qui
/// correspond à l'ordre sémantique attendu : `1.2.3 < 1.10.0 < 2.0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Erreur d'analyse d'une chaîne de version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionParseError {
    /// La chaîne n'a pas exactement trois composantes séparées par des points.
    Format,
    /// Une composante n'est pas un entier décimal valide.
    Nombre,
}

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionParseError::Format => write!(f, "format attendu : « major.minor.patch »"),
            VersionParseError::Nombre => write!(f, "composante de version non numérique"),
        }
    }
}

impl std::error::Error for VersionParseError {}

impl FromStr for Version {
    type Err = VersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(VersionParseError::Format);
        };
        let nombre = |p: &str| p.parse::<u32>().map_err(|_| VersionParseError::Nombre);
        Ok(Version::new(nombre(major)?, nombre(minor)?, nombre(patch)?))
    }
}

// ---------------------------------------------------------------------------
// Canaux et manifestes
// ---------------------------------------------------------------------------

/// Canal de diffusion d'une mise à jour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseChannel {
    /// Canal stable, grand public.
    Stable,
    /// Pré-version pour testeurs volontaires.
    Beta,
    /// Builds quotidiens, très en avance (et très instables).
    Canary,
    /// Support long terme : correctifs de sécurité uniquement.
    Lts,
}

/// Manifeste de mise à jour publié pour un canal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifest {
    /// Canal auquel ce manifeste s'applique.
    pub channel: ReleaseChannel,
    /// Dernière version publiée sur le canal.
    pub latest: Version,
    /// Version minimale encore supportée : en dessous, mise à jour **forcée**.
    pub min_supported: Version,
    /// URL de téléchargement du paquet complet.
    pub url: String,
    /// Empreinte SHA-256 (hex) du paquet, vérifiée par le client avant installation.
    pub sha256: String,
    /// Version de départ d'un paquet delta, si un delta est disponible.
    pub delta_from: Option<Version>,
}

/// Décision rendue par [`UpdateService::check_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Le client est déjà à la dernière version (ou plus récent, ou canal vide).
    UpToDate,
    /// Une version plus récente existe ; la mise à jour reste optionnelle.
    UpdateAvailable(UpdateManifest),
    /// La version du client n'est plus supportée : mise à jour obligatoire.
    ForcedUpdate(UpdateManifest),
}

/// Registre des manifestes publiés, par canal (thread-safe, clonable).
#[derive(Clone, Default)]
pub struct UpdateService(Arc<Mutex<HashMap<ReleaseChannel, UpdateManifest>>>);

impl UpdateService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publie `manifest` sur `channel` (remplace le manifeste précédent du canal).
    pub fn publish(&self, channel: ReleaseChannel, manifest: UpdateManifest) {
        self.0.lock().unwrap().insert(channel, manifest);
    }

    /// Compare la version `current` d'un client au manifeste du canal.
    ///
    /// - Aucun manifeste publié, ou `current >= latest` → [`UpdateDecision::UpToDate`] ;
    /// - `current < min_supported` → [`UpdateDecision::ForcedUpdate`] ;
    /// - sinon (`min_supported <= current < latest`) → [`UpdateDecision::UpdateAvailable`].
    #[must_use]
    pub fn check_update(&self, channel: ReleaseChannel, current: Version) -> UpdateDecision {
        let registre = self.0.lock().unwrap();
        let Some(manifest) = registre.get(&channel) else {
            return UpdateDecision::UpToDate;
        };
        if current < manifest.min_supported {
            UpdateDecision::ForcedUpdate(manifest.clone())
        } else if current < manifest.latest {
            UpdateDecision::UpdateAvailable(manifest.clone())
        } else {
            UpdateDecision::UpToDate
        }
    }
}

// ---------------------------------------------------------------------------
// Déploiement progressif (rollout par pourcentage)
// ---------------------------------------------------------------------------

/// Hachage FNV-1a 64 bits : déterministe et stable entre exécutions/plateformes
/// (contrairement à `DefaultHasher`, dont l'algorithme n'est pas garanti).
fn fnv1a_64(donnees: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hachage = FNV_OFFSET;
    for &octet in donnees {
        hachage ^= u64::from(octet);
        hachage = hachage.wrapping_mul(FNV_PRIME);
    }
    hachage
}

/// L'appareil `device_id` fait-il partie de la tranche de rollout `percent` ?
///
/// Le hachage déterministe de l'ID place chaque appareil dans un seau 0..100 ;
/// l'appareil est servi si son seau est **strictement** inférieur à `percent`.
/// Ainsi `percent = 0` ne sert personne, `percent = 100` sert tout le monde, et
/// augmenter le pourcentage ne retire jamais un appareil déjà servi (monotone).
#[must_use]
pub fn rollout_bucket(device_id: &str, percent: u8) -> bool {
    let seau = fnv1a_64(device_id.as_bytes()) % 100;
    seau < u64::from(percent)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().expect("version de test valide")
    }

    fn manifeste_stable() -> UpdateManifest {
        UpdateManifest {
            channel: ReleaseChannel::Stable,
            latest: v("2.5.1"),
            min_supported: v("2.0.0"),
            url: "https://updates.novadesk.example/stable/2.5.1/novadesk-setup.exe".into(),
            sha256: "deadbeef".repeat(8),
            delta_from: Some(v("2.5.0")),
        }
    }

    #[test]
    fn version_parse_valide() {
        assert_eq!(v("1.2.3"), Version::new(1, 2, 3));
        assert_eq!(v("0.0.0"), Version::new(0, 0, 0));
        assert_eq!(v(" 10.20.30 "), Version::new(10, 20, 30)); // Espaces tolérés.
        assert_eq!(v("1.2.3").to_string(), "1.2.3"); // Aller-retour Display.
    }

    #[test]
    fn version_parse_invalide() {
        for mauvaise in [
            "", "1", "1.2", "1.2.3.4", "a.b.c", "1..3", "1.2.x", "-1.2.3",
        ] {
            assert!(
                mauvaise.parse::<Version>().is_err(),
                "« {mauvaise} » aurait dû être rejetée"
            );
        }
        assert_eq!("1.2".parse::<Version>(), Err(VersionParseError::Format));
        assert_eq!("1.2.x".parse::<Version>(), Err(VersionParseError::Nombre));
    }

    #[test]
    fn version_ordre_semantique() {
        assert!(v("1.2.3") < v("1.2.4")); // Patch.
        assert!(v("1.2.9") < v("1.10.0")); // Minor numérique, pas lexical.
        assert!(v("1.99.99") < v("2.0.0")); // Major domine.
        assert_eq!(v("3.1.4"), v("3.1.4"));
        assert_eq!(v("1.0.0").max(v("0.9.9")), v("1.0.0"));
    }

    #[test]
    fn check_update_a_jour() {
        let service = UpdateService::new();
        // Canal sans manifeste : à jour par défaut.
        assert_eq!(
            service.check_update(ReleaseChannel::Stable, v("1.0.0")),
            UpdateDecision::UpToDate
        );

        service.publish(ReleaseChannel::Stable, manifeste_stable());
        // Exactement la dernière version, ou plus récent (build local) : à jour.
        assert_eq!(
            service.check_update(ReleaseChannel::Stable, v("2.5.1")),
            UpdateDecision::UpToDate
        );
        assert_eq!(
            service.check_update(ReleaseChannel::Stable, v("3.0.0")),
            UpdateDecision::UpToDate
        );
    }

    #[test]
    fn check_update_disponible() {
        let service = UpdateService::new();
        service.publish(ReleaseChannel::Stable, manifeste_stable());
        // Supportée mais en retard : mise à jour optionnelle, manifeste renvoyé.
        match service.check_update(ReleaseChannel::Stable, v("2.3.0")) {
            UpdateDecision::UpdateAvailable(m) => {
                assert_eq!(m.latest, v("2.5.1"));
                assert_eq!(m.delta_from, Some(v("2.5.0")));
            }
            autre => panic!("UpdateAvailable attendu, obtenu {autre:?}"),
        }
        // Cas limite : exactement min_supported → disponible, pas forcée.
        assert!(matches!(
            service.check_update(ReleaseChannel::Stable, v("2.0.0")),
            UpdateDecision::UpdateAvailable(_)
        ));
    }

    #[test]
    fn check_update_forcee_sous_min_supported() {
        let service = UpdateService::new();
        service.publish(ReleaseChannel::Stable, manifeste_stable());
        match service.check_update(ReleaseChannel::Stable, v("1.9.9")) {
            UpdateDecision::ForcedUpdate(m) => assert_eq!(m.min_supported, v("2.0.0")),
            autre => panic!("ForcedUpdate attendu, obtenu {autre:?}"),
        }
    }

    #[test]
    fn canaux_independants_et_republication() {
        let service = UpdateService::new();
        service.publish(ReleaseChannel::Stable, manifeste_stable());
        // Les canaux sans manifeste (Beta, Canary, Lts) : à jour, même très en retard.
        for canal in [
            ReleaseChannel::Beta,
            ReleaseChannel::Canary,
            ReleaseChannel::Lts,
        ] {
            assert_eq!(
                service.check_update(canal, v("0.1.0")),
                UpdateDecision::UpToDate
            );
        }

        // Un manifeste publié sur Canary ne déborde pas sur Stable.
        let mut canary = manifeste_stable();
        canary.channel = ReleaseChannel::Canary;
        canary.latest = v("3.0.0");
        service.publish(ReleaseChannel::Canary, canary);
        assert!(matches!(
            service.check_update(ReleaseChannel::Canary, v("2.5.1")),
            UpdateDecision::UpdateAvailable(_)
        ));
        assert_eq!(
            service.check_update(ReleaseChannel::Stable, v("2.5.1")),
            UpdateDecision::UpToDate
        );

        // Republier sur Stable remplace le manifeste précédent.
        let mut plus_recent = manifeste_stable();
        plus_recent.latest = v("2.6.0");
        plus_recent.delta_from = None;
        service.publish(ReleaseChannel::Stable, plus_recent);
        match service.check_update(ReleaseChannel::Stable, v("2.5.1")) {
            UpdateDecision::UpdateAvailable(m) => {
                assert_eq!(m.latest, v("2.6.0"));
                assert_eq!(m.delta_from, None);
            }
            autre => panic!("manifeste republié attendu, obtenu {autre:?}"),
        }
    }

    #[test]
    fn rollout_deterministe_et_bornes() {
        // Même appareil, même pourcentage : toujours la même décision.
        for id in ["appareil-1", "appareil-2", "poste-caisse-42"] {
            for percent in [0, 1, 25, 50, 99, 100] {
                assert_eq!(rollout_bucket(id, percent), rollout_bucket(id, percent));
            }
            // Bornes : 0 % ne sert personne, 100 % sert tout le monde.
            assert!(!rollout_bucket(id, 0));
            assert!(rollout_bucket(id, 100));
        }
    }

    #[test]
    fn rollout_monotone_en_pourcentage() {
        // Un appareil servi à p % le reste à p' % > p (jamais de retour arrière).
        for i in 0..200 {
            let id = format!("appareil-{i}");
            let mut deja_servi = false;
            for percent in 0..=100 {
                let servi = rollout_bucket(&id, percent);
                assert!(!deja_servi || servi, "{id} retiré du rollout à {percent} %");
                deja_servi = servi;
            }
        }
    }

    #[test]
    fn rollout_reparti_sur_la_population() {
        // Sur 1000 appareils à 50 %, on attend une part proche de la moitié
        // (marge large : le hachage n'est pas parfaitement uniforme).
        let servis = (0..1000)
            .filter(|i| rollout_bucket(&format!("device-{i:04}"), 50))
            .count();
        assert!(
            (350..=650).contains(&servis),
            "répartition anormale : {servis}/1000 servis à 50 %"
        );
        // Et à 10 %, nettement moins.
        let servis_10 = (0..1000)
            .filter(|i| rollout_bucket(&format!("device-{i:04}"), 10))
            .count();
        assert!(
            (30..=250).contains(&servis_10),
            "répartition anormale : {servis_10}/1000 servis à 10 %"
        );
        assert!(servis_10 < servis);
    }
}
