//! `nd-files` — transfert de fichiers (reprise, intégrité BLAKE3), presse-papiers
//! partagé et impression distante. Voir `../../plan-technique/09-fichiers-clipboard.md`.

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use nd_proto::{NdError, Result};

#[cfg(windows)]
mod win;
/// Formats « riches » du presse-papiers Windows : images (`CF_DIB`) et listes
/// de fichiers (`CF_HDROP`). Séparé de `win` (texte) pour cloisonner le FFI.
#[cfg(windows)]
mod win_riche;
#[cfg(windows)]
pub use win::WindowsClipboard;

/// Protocole de transfert message par message (Start/Chunk/End) bâti sur le
/// découpage en chunks ci-dessous. Voir la documentation du module.
pub mod transfer;

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

/// Image bitmap échangée via le presse-papiers : pixels RGBA 8 bits par canal,
/// rangés ligne par ligne **du haut vers le bas** (orientation « top-down »),
/// soit exactement `width * height * 4` octets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRgba {
    /// Largeur en pixels (> 0).
    pub width: u32,
    /// Hauteur en pixels (> 0).
    pub height: u32,
    /// Pixels RGBA, 4 octets par pixel, lignes du haut vers le bas.
    pub rgba: Vec<u8>,
}

/// Presse-papiers partagé entre les deux machines.
///
/// Le texte est le format de base ; les images et les listes de fichiers
/// (plan 09 « presse-papiers riche ») ont des implémentations par défaut
/// « non implémenté » afin que chaque plateforme ne redéfinisse que ce
/// qu'elle sait faire.
pub trait Clipboard: Send {
    fn get_text(&self) -> Result<Option<String>>;
    fn set_text(&self, text: &str) -> Result<()>;

    /// Image du presse-papiers convertie en RGBA, ou `None` s'il n'en
    /// contient pas. Sous Windows : format `CF_DIB` (24/32 bits).
    fn get_image(&self) -> Result<Option<ImageRgba>> {
        Err(NdError::NotImplemented("Clipboard::get_image"))
    }

    /// Place `image` dans le presse-papiers (remplace le contenu courant).
    /// Sous Windows : DIB 32 bits (`CF_DIB`).
    fn set_image(&self, _image: &ImageRgba) -> Result<()> {
        Err(NdError::NotImplemented("Clipboard::set_image"))
    }

    /// Chemins des fichiers copiés (« Copier » dans l'explorateur) ; liste
    /// vide si le presse-papiers n'en contient pas. Sous Windows : `CF_HDROP`.
    fn get_files(&self) -> Result<Vec<PathBuf>> {
        Err(NdError::NotImplemented("Clipboard::get_files"))
    }
}

// ---------------------------------------------------------------------------
// Système de fichiers local derrière `RemoteFs`
// ---------------------------------------------------------------------------

/// Accès « distant » au système de fichiers **local** : première implémentation
/// de [`RemoteFs`], utilisée en boucle locale et côté machine contrôlée, en
/// attendant l'acheminement réseau des requêtes (plan 16).
#[derive(Debug, Default)]
pub struct LocalFs;

impl LocalFs {
    /// Crée un accès au système de fichiers local.
    pub fn new() -> Self {
        Self
    }
}

impl RemoteFs for LocalFs {
    fn list(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        let mut entries = Vec::new();
        for dir_entry in std::fs::read_dir(path)? {
            let dir_entry = dir_entry?;
            let meta = match dir_entry.metadata() {
                Ok(m) => m,
                // L'entrée a pu disparaître entre `read_dir` et `metadata`
                // (répertoire vivant) : on l'ignore plutôt que d'échouer.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let modified_epoch = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            entries.push(RemoteEntry {
                name: dir_entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
                modified_epoch,
            });
        }
        // Ordre déterministe pour l'affichage et les tests.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

/// Ouvre un accès au système de fichiers « distant ». À ce stade, il s'agit du
/// système de fichiers local ([`LocalFs`]) ; le transport réseau des requêtes
/// arrivera avec le protocole de transfert (plans 09/16).
pub fn open_remote_fs() -> Result<Box<dyn RemoteFs>> {
    Ok(Box::new(LocalFs::new()))
}

/// Ouvre le presse-papiers de la plateforme. Windows uniquement à ce stade.
pub fn open_clipboard() -> Result<Box<dyn Clipboard>> {
    #[cfg(windows)]
    {
        Ok(Box::new(WindowsClipboard::new()))
    }
    #[cfg(not(windows))]
    {
        Err(NdError::NotImplemented(
            "nd-files::open_clipboard (presse-papiers non-Windows, voir plan 09)",
        ))
    }
}

// ---------------------------------------------------------------------------
// Découpage en chunks + intégrité BLAKE3 (reprise de transfert)
// ---------------------------------------------------------------------------

/// Taille de chunk par défaut : 1 MiB (compromis débit / granularité de reprise).
pub const DEFAULT_CHUNK_SIZE: u32 = 1024 * 1024;

/// Description d'un chunk d'un fichier à transférer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkInfo {
    /// Numéro absolu du chunk dans le fichier (0 = premier chunk du fichier).
    pub index: u64,
    /// Offset du premier octet du chunk depuis le début du fichier.
    pub offset: u64,
    /// Longueur du chunk en octets (`chunk_size`, sauf pour le dernier chunk).
    pub len: u32,
    /// Hash BLAKE3 du contenu du chunk.
    pub hash: [u8; 32],
}

/// Plan de transfert d'un fichier : chunks restant à envoyer + hash racine.
#[derive(Debug, Clone)]
pub struct ChunkPlan {
    /// Taille nominale d'un chunk (octets).
    pub chunk_size: u32,
    /// Offset de reprise à partir duquel les chunks ont été planifiés.
    pub resume_offset: u64,
    /// Longueur totale du fichier (octets).
    pub file_len: u64,
    /// Chunks couvrant `[resume_offset, file_len)`, dans l'ordre du fichier.
    pub chunks: Vec<ChunkInfo>,
    /// Hash BLAKE3 de l'ensemble des octets couverts par `chunks`
    /// (hash du fichier complet quand `resume_offset == 0`).
    pub root_hash: [u8; 32],
}

/// Hash BLAKE3 d'un bloc de données (aide partagée planification/vérification).
pub fn chunk_hash(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

/// Vérifie l'intégrité d'un chunk relu par rapport à son descripteur.
pub fn verify_chunk(info: &ChunkInfo, data: &[u8]) -> bool {
    data.len() == info.len as usize && chunk_hash(data) == info.hash
}

/// Produit le plan de chunks d'un fichier avec la taille par défaut
/// ([`DEFAULT_CHUNK_SIZE`]). Voir [`plan_file_chunks_with`].
pub fn plan_file_chunks(path: &Path, resume_offset: u64) -> Result<ChunkPlan> {
    plan_file_chunks_with(path, resume_offset, DEFAULT_CHUNK_SIZE)
}

/// Produit le plan de chunks d'un fichier : découpe `[resume_offset, EOF)` en
/// blocs de `chunk_size` octets alignés sur le début du fichier, hache chaque
/// bloc en BLAKE3 et calcule le hash racine de toute la plage couverte.
///
/// La reprise ne se fait que sur frontière de chunk : `resume_offset` doit être
/// un multiple de `chunk_size` et ≤ taille du fichier, sinon
/// [`NdError::Protocol`] est renvoyée.
pub fn plan_file_chunks_with(
    path: &Path,
    resume_offset: u64,
    chunk_size: u32,
) -> Result<ChunkPlan> {
    if chunk_size == 0 {
        return Err(NdError::Protocol("taille de chunk nulle".into()));
    }
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if resume_offset > file_len {
        return Err(NdError::Protocol(format!(
            "offset de reprise {resume_offset} au-delà de la fin du fichier ({file_len} octets)"
        )));
    }
    if !resume_offset.is_multiple_of(u64::from(chunk_size)) {
        return Err(NdError::Protocol(format!(
            "offset de reprise {resume_offset} non aligné sur la taille de chunk {chunk_size}"
        )));
    }
    file.seek(SeekFrom::Start(resume_offset))?;

    let mut chunks = Vec::new();
    let mut root = blake3::Hasher::new();
    let mut buf = vec![0u8; chunk_size as usize];
    let mut offset = resume_offset;
    while offset < file_len {
        // Dernier chunk possiblement partiel ; `want` ≤ chunk_size (u32) donc
        // les conversions vers usize/u32 sont sans perte.
        let want = (file_len - offset).min(u64::from(chunk_size)) as usize;
        file.read_exact(&mut buf[..want])?;
        let data = &buf[..want];
        root.update(data);
        chunks.push(ChunkInfo {
            index: offset / u64::from(chunk_size),
            offset,
            len: want as u32,
            hash: chunk_hash(data),
        });
        offset += want as u64;
    }

    Ok(ChunkPlan {
        chunk_size,
        resume_offset,
        file_len,
        chunks,
        root_hash: *root.finalize().as_bytes(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Chemin temporaire unique pour un test (évite les collisions entre tests
    /// parallèles et entre exécutions).
    fn chemin_temp(nom: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nd_files_test_{}_{nom}", std::process::id()))
    }

    /// Motif déterministe non trivial (chaque offset produit un octet distinct
    /// de ses voisins, sans période courte évidente).
    fn motif(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
            .collect()
    }

    #[test]
    fn blake3_vecteur_connu() {
        // Vecteur officiel BLAKE3 : hash de l'entrée vide.
        assert_eq!(
            blake3::hash(b"").to_hex().as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(chunk_hash(b""), *blake3::hash(b"").as_bytes());
    }

    #[test]
    fn plan_chunks_coherent_et_reconstitution() {
        const CHUNK: u32 = 64 * 1024;
        // 2 chunks pleins + 1 chunk partiel.
        let contenu = motif(2 * CHUNK as usize + 12_345);
        let path = chemin_temp("plan.bin");
        std::fs::write(&path, &contenu).unwrap();

        let plan = plan_file_chunks_with(&path, 0, CHUNK).unwrap();
        assert_eq!(plan.file_len, contenu.len() as u64);
        assert_eq!(plan.resume_offset, 0);
        assert_eq!(plan.chunks.len(), 3);
        // Le hash racine est le BLAKE3 du contenu complet.
        assert_eq!(plan.root_hash, chunk_hash(&contenu));

        // Chaque chunk : index/offset/len cohérents, hash du bon segment ;
        // reconstitution du contenu connu chunk par chunk.
        let mut reconstitue = vec![0u8; contenu.len()];
        for (i, c) in plan.chunks.iter().enumerate() {
            assert_eq!(c.index, i as u64);
            assert_eq!(c.offset, i as u64 * u64::from(CHUNK));
            let fin = c.offset as usize + c.len as usize;
            let segment = &contenu[c.offset as usize..fin];
            assert_eq!(c.hash, chunk_hash(segment));
            assert!(verify_chunk(c, segment));
            assert!(!verify_chunk(c, &segment[..segment.len() - 1]));
            reconstitue[c.offset as usize..fin].copy_from_slice(segment);
        }
        assert_eq!(reconstitue, contenu);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reprise_alignee_et_offsets_invalides() {
        const CHUNK: u32 = 32 * 1024;
        let contenu = motif(3 * CHUNK as usize + 7);
        let path = chemin_temp("reprise.bin");
        std::fs::write(&path, &contenu).unwrap();

        // Reprise après le premier chunk : indices absolus, hash racine du suffixe.
        let plan = plan_file_chunks_with(&path, u64::from(CHUNK), CHUNK).unwrap();
        assert_eq!(plan.chunks.len(), 3);
        assert_eq!(plan.chunks[0].index, 1);
        assert_eq!(plan.chunks[0].offset, u64::from(CHUNK));
        assert_eq!(plan.root_hash, chunk_hash(&contenu[CHUNK as usize..]));

        // Reprise en fin de fichier exacte : plan vide, hash racine du vide.
        let taille = contenu.len() as u64;
        let fin = plan_file_chunks_with(&path, taille - taille % u64::from(CHUNK), CHUNK);
        assert!(fin.is_ok());

        // Offset non aligné → erreur de protocole.
        assert!(matches!(
            plan_file_chunks_with(&path, 1, CHUNK),
            Err(NdError::Protocol(_))
        ));
        // Offset au-delà de la fin → erreur de protocole.
        assert!(matches!(
            plan_file_chunks_with(&path, u64::from(CHUNK) * 100, CHUNK),
            Err(NdError::Protocol(_))
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn localfs_liste_un_repertoire() {
        // Répertoire dédié pour un listing déterministe.
        let dir = chemin_temp("listing_dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.bin"), b"novadesk").unwrap();
        std::fs::write(dir.join("a.bin"), b"nd").unwrap();
        std::fs::create_dir_all(dir.join("sous_dossier")).unwrap();

        let mut fs = LocalFs::new();
        let entrees = fs.list(dir.to_str().unwrap()).unwrap();
        assert_eq!(entrees.len(), 3);
        // Tri par nom.
        assert_eq!(entrees[0].name, "a.bin");
        assert_eq!(entrees[0].size, 2);
        assert!(!entrees[0].is_dir);
        assert!(entrees[0].modified_epoch.is_some());
        assert_eq!(entrees[1].name, "b.bin");
        assert_eq!(entrees[1].size, 8);
        assert_eq!(entrees[2].name, "sous_dossier");
        assert!(entrees[2].is_dir);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
