//! Persistance légère de l'API applicative — JSON atomique, pur Rust.
//!
//! L'état durable ([`EtatPersistant`]) est sérialisé en JSON (`serde_json`) et
//! écrit de façon **atomique** : écriture complète dans un fichier temporaire
//! voisin (`<chemin>.tmp`), synchronisation disque, puis renommage sur le
//! fichier final — un arrêt brutal laisse donc toujours un état complet et
//! valide, jamais un fichier tronqué. Les sauvegardes concurrentes (une par
//! connexion mutante, voir [`crate::services`]) sont sérialisées par un verrou
//! interne, le fichier temporaire étant partagé.
//!
//! Sont durables : le carnet d'adresses, les attributions de rôles, les
//! groupes et les partages d'appareils. Les manifestes de mise à jour et les
//! politiques de configuration restent en mémoire (données d'exploitation,
//! republiées au démarrage). Voir `../../plan-technique/11-backend-infrastructure.md`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::groups::Group;
use crate::rbac::{AttributionMap, Role};
use crate::sharing::Beneficiaire;
use crate::CarnetMap;

/// Un partage persisté, sous forme dépliée (appareil, bénéficiaire, rôle) :
/// [`Beneficiaire`] ne peut pas servir de clé d'objet JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partage {
    /// Appareil partagé (id NovaDesk).
    pub appareil: u64,
    /// Compte ou groupe bénéficiaire.
    pub beneficiaire: Beneficiaire,
    /// Rôle accordé à ce bénéficiaire.
    pub role: Role,
}

/// Instantané complet de l'état durable, tel qu'écrit sur disque.
///
/// Chaque champ porte `#[serde(default)]` : un fichier écrit par une version
/// antérieure (champ manquant) se charge avec la valeur vide correspondante.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtatPersistant {
    /// Carnet d'adresses : jeton de session → contacts.
    #[serde(default)]
    pub carnet: CarnetMap,
    /// Attributions RBAC : compte → (ressource → rôle).
    #[serde(default)]
    pub roles: AttributionMap,
    /// Dernier id de groupe attribué (pour ne jamais réutiliser un id).
    #[serde(default)]
    pub dernier_id_groupe: u64,
    /// Groupes, triés par id à l'écriture (fichier stable et lisible).
    #[serde(default)]
    pub groupes: Vec<Group>,
    /// Partages d'appareils, triés à l'écriture.
    #[serde(default)]
    pub partages: Vec<Partage>,
}

/// Stockage fichier : un chemin + un verrou d'écriture.
#[derive(Debug)]
pub struct Storage {
    chemin: PathBuf,
    /// Sérialise les sauvegardes concurrentes (fichier temporaire partagé).
    verrou: Mutex<()>,
}

impl Storage {
    /// Prépare un stockage sur `chemin` (le fichier peut ne pas encore exister).
    #[must_use]
    pub fn new(chemin: impl Into<PathBuf>) -> Self {
        Self {
            chemin: chemin.into(),
            verrou: Mutex::new(()),
        }
    }

    /// Chemin du fichier d'état.
    #[must_use]
    pub fn chemin(&self) -> &Path {
        &self.chemin
    }

    /// Charge l'état persisté. `Ok(None)` si le fichier n'existe pas encore
    /// (premier démarrage).
    ///
    /// # Errors
    /// Propage les erreurs de lecture ; `InvalidData` si le JSON est illisible.
    pub fn charger(&self) -> io::Result<Option<EtatPersistant>> {
        let donnees = match fs::read(&self.chemin) {
            Ok(donnees) => donnees,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        serde_json::from_slice(&donnees).map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("état persistant illisible : {e}"),
            )
        })
    }

    /// Écrit `etat` de façon atomique : fichier temporaire complet, `sync`,
    /// puis renommage sur le fichier final (remplace l'existant, y compris
    /// sous Windows).
    ///
    /// # Errors
    /// Propage les erreurs d'écriture ou de renommage.
    pub fn sauvegarder(&self, etat: &EtatPersistant) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(etat).map_err(io::Error::other)?;
        let _exclusif = self.verrou.lock().unwrap();
        if let Some(parent) = self.chemin.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporaire = self.chemin_temporaire();
        {
            let mut fichier = fs::File::create(&temporaire)?;
            fichier.write_all(&json)?;
            fichier.sync_all()?;
        }
        fs::rename(&temporaire, &self.chemin)
    }

    /// Chemin du fichier temporaire voisin (`<chemin>.tmp`).
    fn chemin_temporaire(&self) -> PathBuf {
        let mut nom = self.chemin.as_os_str().to_os_string();
        nom.push(".tmp");
        PathBuf::from(nom)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Contact;

    /// Chemin de test unique (répertoire temporaire du système).
    fn chemin_test(nom: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nd-api-storage-{}-{nom}.json", std::process::id()))
    }

    fn etat_exemple() -> EtatPersistant {
        let mut carnet = CarnetMap::new();
        carnet.insert(
            "jeton-a".into(),
            vec![Contact {
                id: 42,
                alias: "PC bureau".into(),
            }],
        );
        let mut roles = AttributionMap::new();
        roles
            .entry("alice".to_string())
            .or_default()
            .insert("org-1".to_string(), Role::Admin);
        EtatPersistant {
            carnet,
            roles,
            dernier_id_groupe: 2,
            groupes: vec![Group {
                id: 1,
                name: "Support".into(),
                members: vec!["alice".into()],
            }],
            partages: vec![Partage {
                appareil: 100,
                beneficiaire: Beneficiaire::Groupe(1),
                role: Role::Operator,
            }],
        }
    }

    #[test]
    fn sauvegarde_puis_chargement() {
        let chemin = chemin_test("aller-retour");
        let stockage = Storage::new(&chemin);
        let etat = etat_exemple();
        stockage.sauvegarder(&etat).expect("sauvegarde");
        // Écriture atomique : le fichier temporaire a été renommé, pas laissé.
        assert!(!stockage.chemin_temporaire().exists());
        assert_eq!(stockage.charger().expect("chargement"), Some(etat));
        let _ = fs::remove_file(&chemin);
    }

    #[test]
    fn fichier_absent_est_un_premier_demarrage() {
        let stockage = Storage::new(chemin_test("absent"));
        assert_eq!(stockage.charger().expect("chargement"), None);
    }

    #[test]
    fn fichier_corrompu_signale_invalid_data() {
        let chemin = chemin_test("corrompu");
        fs::write(&chemin, b"{ pas du json").expect("écriture");
        let erreur = Storage::new(&chemin)
            .charger()
            .expect_err("corruption détectée");
        assert_eq!(erreur.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_file(&chemin);
    }

    #[test]
    fn resauvegarde_remplace_l_etat() {
        let chemin = chemin_test("remplacement");
        let stockage = Storage::new(&chemin);
        stockage.sauvegarder(&etat_exemple()).expect("sauvegarde 1");
        // Deuxième sauvegarde : le renommage écrase le fichier existant.
        let vide = EtatPersistant::default();
        stockage.sauvegarder(&vide).expect("sauvegarde 2");
        assert_eq!(stockage.charger().expect("chargement"), Some(vide));
        let _ = fs::remove_file(&chemin);
    }
}
