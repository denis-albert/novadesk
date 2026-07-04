//! `nd-files` — transfert de fichiers (reprise, intégrité BLAKE3), presse-papiers
//! partagé et impression distante. Voir `../../plan-technique/09-fichiers-clipboard.md`.

use nd_proto::{NdError, Result};

/// Entrée d'un listing de système de fichiers distant.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Horodatage de modification (secondes epoch), si disponible.
    pub modified_epoch: Option<u64>,
}

/// Accès au système de fichiers distant (côté machine contrôlée).
pub trait RemoteFs: Send {
    /// Liste le contenu d'un répertoire distant.
    fn list(&mut self, path: &str) -> Result<Vec<RemoteEntry>>;
}

/// Presse-papiers partagé entre les deux machines.
pub trait Clipboard: Send {
    fn get_text(&self) -> Result<Option<String>>;
    fn set_text(&self, text: &str) -> Result<()>;
}

/// Ouvre un accès au système de fichiers distant. Non implémenté à ce stade.
pub fn open_remote_fs() -> Result<Box<dyn RemoteFs>> {
    Err(NdError::NotImplemented(
        "nd-files::open_remote_fs (protocole de transfert à venir, voir plan 09/16)",
    ))
}
