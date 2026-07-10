//! Journalisation **fichier** du service, sous le répertoire machine
//! (`…\NovaDesk\service.log`).
//!
//! Un service en session 0 n'a pas de console : on journalise dans un fichier
//! sous ProgramData (append, horodaté). Le **journal d'événements Windows** est
//! l'alternative « intégrée » (visible dans l'observateur d'événements) ; il
//! exige d'enregistrer une source d'événements (clé de registre + catégories) et
//! reste une amélioration ultérieure — le fichier suffit au diagnostic du
//! squelette et ne dépend d'aucun enregistrement préalable.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Nom du fichier journal sous le répertoire machine.
const FICHIER_JOURNAL: &str = "service.log";

/// Ajoute une ligne horodatée au journal du service (best-effort : une erreur
/// d'écriture est silencieuse — journaliser ne doit jamais faire tomber le service).
pub fn journaliser(repertoire: &Path, message: &str) {
    let _ = ecrire(repertoire, message);
}

/// Journalise dans le répertoire machine par défaut (utilisé quand aucun
/// répertoire n'est encore résolu, ex. échec très précoce du service).
pub fn journaliser_defaut(message: &str) {
    journaliser(&crate::config::repertoire_service(), message);
}

/// Écriture effective (créée en append), remontée en `Result` pour rester testable.
fn ecrire(repertoire: &Path, message: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(repertoire)?;
    let mut fichier = OpenOptions::new()
        .create(true)
        .append(true)
        .open(repertoire.join(FICHIER_JOURNAL))?;
    writeln!(fichier, "[{}] {message}", horodatage_unix())
}

/// Horodatage Unix en secondes (0 si l'horloge précède l'époque). Sans dépendance
/// de date : le format lisible viendra avec le journal d'événements Windows.
fn horodatage_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn journalise_puis_relit_les_lignes() {
        let dir = TempDir::new().expect("répertoire temporaire");
        journaliser(dir.path(), "démarrage");
        journaliser(dir.path(), "arrêt");
        let contenu =
            std::fs::read_to_string(dir.path().join(FICHIER_JOURNAL)).expect("lecture du journal");
        assert!(contenu.contains("démarrage"));
        assert!(contenu.contains("arrêt"));
        assert_eq!(contenu.lines().count(), 2, "une ligne par appel");
    }
}
