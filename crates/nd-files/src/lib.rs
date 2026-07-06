//! `nd-files` — transfert de fichiers (reprise, intégrité BLAKE3), gestionnaire
//! de fichiers (opérations d'écriture), presse-papiers partagé et impression
//! distante. Voir `../../plan-technique/09-fichiers-clipboard.md`.

use std::fs::{File, Metadata};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
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
///
/// Les opérations d'écriture (plan 09 « gestionnaire de fichiers ») ont des
/// implémentations par défaut « non implémenté » : les implémentations
/// existantes (lecture seule) restent valides telles quelles et ne redéfinissent
/// que ce qu'elles savent faire.
pub trait RemoteFs: Send {
    /// Liste le contenu d'un répertoire distant.
    fn list(&mut self, path: &str) -> Result<Vec<RemoteEntry>>;

    /// Crée le répertoire `path` (ainsi que ses parents manquants ; idempotent
    /// si le répertoire existe déjà).
    fn mkdir(&mut self, _path: &str) -> Result<()> {
        Err(NdError::NotImplemented("RemoteFs::mkdir"))
    }

    /// Supprime le fichier `path`. Erreur si le fichier n'existe pas ou si
    /// `path` désigne un répertoire.
    fn remove_file(&mut self, _path: &str) -> Result<()> {
        Err(NdError::NotImplemented("RemoteFs::remove_file"))
    }

    /// Supprime récursivement le répertoire `path` et tout son contenu.
    /// Erreur si le répertoire n'existe pas.
    fn remove_dir_all(&mut self, _path: &str) -> Result<()> {
        Err(NdError::NotImplemented("RemoteFs::remove_dir_all"))
    }

    /// Renomme (ou déplace) `from` vers `to`.
    fn rename(&mut self, _from: &str, _to: &str) -> Result<()> {
        Err(NdError::NotImplemented("RemoteFs::rename"))
    }

    /// Copie le fichier `from` vers `to` (destination écrasée si elle existe)
    /// et renvoie le nombre d'octets copiés.
    fn copy_file(&mut self, _from: &str, _to: &str) -> Result<u64> {
        Err(NdError::NotImplemented("RemoteFs::copy_file"))
    }

    /// Crée un fichier vide en `path`. Erreur si le chemin existe déjà (pas de
    /// troncature silencieuse d'un fichier existant).
    fn create_file(&mut self, _path: &str) -> Result<()> {
        Err(NdError::NotImplemented("RemoteFs::create_file"))
    }

    /// Indique si `path` existe (fichier ou répertoire).
    fn exists(&mut self, _path: &str) -> Result<bool> {
        Err(NdError::NotImplemented("RemoteFs::exists"))
    }

    /// Métadonnées de `path`, ou `None` s'il n'existe pas. Les autres erreurs
    /// (droits insuffisants…) sont remontées telles quelles.
    fn stat(&mut self, _path: &str) -> Result<Option<RemoteEntry>> {
        Err(NdError::NotImplemented("RemoteFs::stat"))
    }
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
///
/// Par défaut ([`LocalFs::new`]), les chemins sont utilisés tels quels, sans
/// restriction. [`LocalFs::jailed`] active une protection anti-traversée
/// **basique** qui borne toutes les opérations sous un répertoire racine.
#[derive(Debug, Default)]
pub struct LocalFs {
    /// Racine optionnelle (« jail »), sous forme canonique : quand elle est
    /// définie, tous les chemins sont résolus et bornés sous cette racine.
    root: Option<PathBuf>,
}

impl LocalFs {
    /// Crée un accès au système de fichiers local, **non borné** : les chemins
    /// reçus sont utilisés tels quels.
    pub fn new() -> Self {
        Self { root: None }
    }

    /// Crée un accès **borné** sous `root` (protection anti-traversée basique).
    ///
    /// `root` doit être un répertoire existant ; il est canonicalisé à la
    /// construction. Ensuite, pour chaque opération :
    ///
    /// * tout chemin contenant un composant `..` est refusé d'emblée
    ///   ([`NdError::Protocol`]), avant tout accès disque ;
    /// * un chemin relatif est ancré sous la racine ;
    /// * un chemin absolu est accepté seulement s'il reste sous la racine :
    ///   son ancêtre existant le plus profond est canonicalisé (ce qui résout
    ///   les liens symboliques) et doit être un descendant de la racine.
    ///
    /// Limites (protection « basique », documentée au plan 09) : la
    /// vérification n'est pas atomique vis-à-vis d'une modification
    /// concurrente de l'arborescence (TOCTOU) et ne couvre pas les liens
    /// physiques (hard links). Ce n'est pas une frontière de sécurité à elle
    /// seule ; le contrôle d'accès réel relève de la couche session.
    pub fn jailed(root: impl AsRef<Path>) -> Result<Self> {
        let canon = std::fs::canonicalize(root.as_ref())?;
        if !canon.is_dir() {
            return Err(NdError::Protocol(format!(
                "racine de confinement non répertoire : {}",
                canon.display()
            )));
        }
        Ok(Self { root: Some(canon) })
    }

    /// Résout `path` selon la configuration : identité si non borné, sinon
    /// ancrage sous la racine + vérification de confinement (voir
    /// [`LocalFs::jailed`]).
    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let Some(root) = &self.root else {
            return Ok(PathBuf::from(path));
        };
        let brut = Path::new(path);
        // 1. Refus des composants `..` avant tout accès disque : c'est le
        //    vecteur de traversée classique, inutile ici puisque les chemins
        //    relatifs sont déjà ancrés sous la racine.
        if brut.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(NdError::Protocol(format!(
                "chemin refusé (composant '..') : {path}"
            )));
        }
        // 2. Chemin relatif → ancré sous la racine ; absolu → vérifié tel quel.
        let candidat = if brut.is_absolute() {
            brut.to_path_buf()
        } else {
            root.join(brut)
        };
        // 3. Confinement : l'ancêtre existant le plus profond, une fois
        //    canonicalisé (liens symboliques résolus), doit rester sous la
        //    racine. Les composants restants (à créer) ne contiennent pas de
        //    `..` (refusés en 1) et ne peuvent donc pas remonter.
        let ancetre = candidat
            .ancestors()
            .find(|a| a.exists())
            .map(std::fs::canonicalize)
            .transpose()?
            .ok_or_else(|| NdError::Protocol(format!("chemin sans ancêtre existant : {path}")))?;
        if !ancetre.starts_with(root) {
            return Err(NdError::Protocol(format!(
                "chemin hors de la racine de confinement : {path}"
            )));
        }
        Ok(candidat)
    }
}

/// Horodatage de modification (secondes epoch) extrait de métadonnées, si
/// disponible (aide partagée entre `list` et `stat`).
fn epoch_modification(meta: &Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

impl RemoteFs for LocalFs {
    fn list(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        let path = self.resolve(path)?;
        let mut entries = Vec::new();
        for dir_entry in std::fs::read_dir(&path)? {
            let dir_entry = dir_entry?;
            let meta = match dir_entry.metadata() {
                Ok(m) => m,
                // L'entrée a pu disparaître entre `read_dir` et `metadata`
                // (répertoire vivant) : on l'ignore plutôt que d'échouer.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            entries.push(RemoteEntry {
                name: dir_entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
                modified_epoch: epoch_modification(&meta),
            });
        }
        // Ordre déterministe pour l'affichage et les tests.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        let path = self.resolve(path)?;
        std::fs::create_dir_all(&path)?;
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        let path = self.resolve(path)?;
        std::fs::remove_file(&path)?;
        Ok(())
    }

    fn remove_dir_all(&mut self, path: &str) -> Result<()> {
        let path = self.resolve(path)?;
        std::fs::remove_dir_all(&path)?;
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let from = self.resolve(from)?;
        let to = self.resolve(to)?;
        std::fs::rename(&from, &to)?;
        Ok(())
    }

    fn copy_file(&mut self, from: &str, to: &str) -> Result<u64> {
        let from = self.resolve(from)?;
        let to = self.resolve(to)?;
        Ok(std::fs::copy(&from, &to)?)
    }

    fn create_file(&mut self, path: &str) -> Result<()> {
        let path = self.resolve(path)?;
        // `create_new` : échec si le chemin existe déjà, pour ne jamais
        // tronquer silencieusement un fichier existant.
        File::create_new(&path)?;
        Ok(())
    }

    fn exists(&mut self, path: &str) -> Result<bool> {
        let path = self.resolve(path)?;
        Ok(path.try_exists()?)
    }

    fn stat(&mut self, path: &str) -> Result<Option<RemoteEntry>> {
        let resolu = self.resolve(path)?;
        let meta = match std::fs::metadata(&resolu) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Nom = dernier composant du chemin (le chemin complet en secours,
        // pour les racines sans nom de fichier).
        let name = resolu
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        Ok(Some(RemoteEntry {
            name,
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified_epoch: epoch_modification(&meta),
        }))
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

    /// Rendu `&str` d'un chemin de test (les chemins temporaires sont UTF-8).
    fn s(p: &Path) -> &str {
        p.to_str().unwrap()
    }

    #[test]
    fn localfs_cycle_complet_operations() {
        // Cycle mkdir → create → copy → rename → stat → remove sur un
        // répertoire temporaire dédié, via un `LocalFs` non borné.
        let racine = chemin_temp("fsops_cycle");
        std::fs::create_dir_all(&racine).unwrap();
        let mut fs = LocalFs::new();

        // mkdir (avec parents) + exists.
        let dossier = racine.join("a").join("b");
        fs.mkdir(s(&dossier)).unwrap();
        assert!(fs.exists(s(&dossier)).unwrap());
        assert!(fs.stat(s(&dossier)).unwrap().unwrap().is_dir);

        // create_file : fichier vide, refuse d'écraser un existant.
        let source = dossier.join("source.bin");
        fs.create_file(s(&source)).unwrap();
        let vide = fs.stat(s(&source)).unwrap().unwrap();
        assert!(!vide.is_dir);
        assert_eq!(vide.size, 0);
        assert!(matches!(fs.create_file(s(&source)), Err(NdError::Io(_))));

        // Écriture du contenu (via std, comme le ferait `transfer` côté
        // récepteur), puis copy_file avec vérification du nombre d'octets.
        let contenu = motif(64 * 1024 + 21);
        std::fs::write(&source, &contenu).unwrap();
        let copie = dossier.join("copie.bin");
        let octets = fs.copy_file(s(&source), s(&copie)).unwrap();
        assert_eq!(octets, contenu.len() as u64);
        assert_eq!(std::fs::read(&copie).unwrap(), contenu);

        // rename : l'ancienne entrée disparaît, la nouvelle porte le contenu.
        let finale = dossier.join("finale.bin");
        fs.rename(s(&copie), s(&finale)).unwrap();
        assert!(!fs.exists(s(&copie)).unwrap());
        let entree = fs.stat(s(&finale)).unwrap().unwrap();
        assert_eq!(entree.name, "finale.bin");
        assert!(!entree.is_dir);
        assert_eq!(entree.size, contenu.len() as u64);
        assert!(entree.modified_epoch.is_some());

        // remove_file puis remove_dir_all : plus rien n'existe.
        fs.remove_file(s(&source)).unwrap();
        assert!(fs.stat(s(&source)).unwrap().is_none());
        fs.remove_dir_all(s(&racine.join("a"))).unwrap();
        assert!(!fs.exists(s(&dossier)).unwrap());

        let _ = std::fs::remove_dir_all(&racine);
    }

    #[test]
    fn localfs_erreurs_sur_chemins_inexistants() {
        let racine = chemin_temp("fsops_erreurs");
        std::fs::create_dir_all(&racine).unwrap();
        let mut fs = LocalFs::new();
        let absent = racine.join("nexiste_pas.bin");

        // Suppressions et copie depuis une source absente : erreurs d'E/S.
        assert!(matches!(fs.remove_file(s(&absent)), Err(NdError::Io(_))));
        assert!(matches!(fs.remove_dir_all(s(&absent)), Err(NdError::Io(_))));
        assert!(matches!(
            fs.copy_file(s(&absent), s(&racine.join("cible.bin"))),
            Err(NdError::Io(_))
        ));
        // stat d'un absent : `None` sans erreur ; exists : `false`.
        assert!(fs.stat(s(&absent)).unwrap().is_none());
        assert!(!fs.exists(s(&absent)).unwrap());

        let _ = std::fs::remove_dir_all(&racine);
    }

    #[test]
    fn localfs_jailed_borne_les_chemins() {
        let racine = chemin_temp("fsops_jail");
        std::fs::create_dir_all(&racine).unwrap();
        // Répertoire frère hors racine, pour les tentatives d'évasion absolues.
        let hors = chemin_temp("fsops_hors_jail");
        std::fs::create_dir_all(&hors).unwrap();

        let mut fs = LocalFs::jailed(&racine).unwrap();

        // Les chemins relatifs sont ancrés sous la racine.
        fs.mkdir("dedans").unwrap();
        fs.create_file("dedans/a.txt").unwrap();
        assert!(fs.exists("dedans/a.txt").unwrap());
        assert!(racine.join("dedans").join("a.txt").is_file());
        // Un chemin absolu SOUS la racine reste accepté.
        assert!(fs.exists(s(&racine.join("dedans"))).unwrap());

        // Tout composant `..` est refusé, même s'il resterait sous la racine.
        assert!(matches!(
            fs.exists("../evasion.txt"),
            Err(NdError::Protocol(_))
        ));
        assert!(matches!(
            fs.mkdir("dedans/../autre"),
            Err(NdError::Protocol(_))
        ));

        // Chemin absolu hors racine : refusé (création, et cible de rename).
        assert!(matches!(
            fs.create_file(s(&hors.join("evasion.txt"))),
            Err(NdError::Protocol(_))
        ));
        assert!(matches!(
            fs.rename("dedans/a.txt", s(&hors.join("a.txt"))),
            Err(NdError::Protocol(_))
        ));
        // La tentative de rename n'a pas déplacé le fichier.
        assert!(fs.exists("dedans/a.txt").unwrap());
        assert!(!hors.join("a.txt").exists());

        // Racine inexistante : la construction échoue proprement.
        assert!(LocalFs::jailed(chemin_temp("fsops_jail_inexistante")).is_err());

        let _ = std::fs::remove_dir_all(&racine);
        let _ = std::fs::remove_dir_all(&hors);
    }

    #[test]
    fn remotefs_operations_par_defaut_non_implementees() {
        // Une implémentation minimale (lecture seule) reste valide : les
        // opérations d'écriture répondent `NotImplemented` par défaut.
        struct LectureSeule;
        impl RemoteFs for LectureSeule {
            fn list(&mut self, _path: &str) -> Result<Vec<RemoteEntry>> {
                Ok(Vec::new())
            }
        }
        let mut fs = LectureSeule;
        assert!(fs.list("x").unwrap().is_empty());
        assert!(matches!(fs.mkdir("x"), Err(NdError::NotImplemented(_))));
        assert!(matches!(
            fs.remove_file("x"),
            Err(NdError::NotImplemented(_))
        ));
        assert!(matches!(
            fs.remove_dir_all("x"),
            Err(NdError::NotImplemented(_))
        ));
        assert!(matches!(
            fs.rename("x", "y"),
            Err(NdError::NotImplemented(_))
        ));
        assert!(matches!(
            fs.copy_file("x", "y"),
            Err(NdError::NotImplemented(_))
        ));
        assert!(matches!(
            fs.create_file("x"),
            Err(NdError::NotImplemented(_))
        ));
        assert!(matches!(fs.exists("x"), Err(NdError::NotImplemented(_))));
        assert!(matches!(fs.stat("x"), Err(NdError::NotImplemented(_))));
    }
}
