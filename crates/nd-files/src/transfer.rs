//! Protocole de transfert de fichiers message par message (plan 09), bâti sur
//! le découpage en chunks + intégrité BLAKE3 du crate
//! ([`crate::plan_file_chunks_with`], [`crate::ChunkPlan`]).
//!
//! Indépendant du réseau, donc testable entièrement en mémoire : l'émetteur
//! ([`FileSender`]) produit la séquence de [`TransferMsg`] (`Start`, un `Chunk`
//! par bloc, `End`) et le récepteur ([`FileReceiver`]) les consomme un par un
//! via [`FileReceiver::accept`]. L'acheminement réel des trames (canal Files)
//! arrivera avec le plan 16.
//!
//! # Format de trame (sérialisation binaire, préfixe de longueur)
//!
//! ```text
//! trame  = [longueur du corps : u32 LE][corps]
//! corps  = [tag : u8][charge utile]
//!
//! Start (tag 1) : [long. nom : u32 LE][nom UTF-8][taille : u64 LE]
//!                 [taille de chunk : u32 LE][hash racine BLAKE3 : 32 octets]
//! Chunk (tag 2) : [index : u64 LE][données jusqu'à la fin du corps]
//! End   (tag 3) : charge utile vide
//! ```
//!
//! # Intégrité et reprise
//!
//! * Le hash racine du `Start` couvre exactement les octets transférés dans la
//!   session (tout le fichier quand l'offset de reprise vaut 0, le suffixe
//!   sinon) ; il est vérifié à la réception du `End`.
//! * Chaque chunk peut en plus être vérifié individuellement (BLAKE3) quand le
//!   récepteur connaît le [`ChunkPlan`] attendu
//!   ([`FileReceiver::with_expected_plan`]) : un chunk corrompu est alors
//!   rejeté immédiatement, sans être écrit, et peut être renvoyé.
//! * La reprise se fait sur frontière de chunk : les octets déjà présents dans
//!   la destination sont conservés tels quels (ils ne sont pas re-vérifiés par
//!   le hash racine, qui ne couvre que le suffixe reçu) ; une éventuelle queue
//!   partielle est tronquée au `Start`.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nd_proto::{NdError, Result};

use crate::{plan_file_chunks_with, verify_chunk, ChunkPlan, DEFAULT_CHUNK_SIZE};

// ---------------------------------------------------------------------------
// Messages du protocole
// ---------------------------------------------------------------------------

/// Tag binaire du message `Start` (0 est évité pour détecter les tampons nuls).
const TAG_START: u8 = 1;
/// Tag binaire du message `Chunk`.
const TAG_CHUNK: u8 = 2;
/// Tag binaire du message `End`.
const TAG_END: u8 = 3;

/// Message du protocole de transfert d'un fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferMsg {
    /// Ouverture du transfert : métadonnées du fichier et hash racine BLAKE3
    /// des octets qui vont suivre (suffixe seulement en cas de reprise).
    Start {
        /// Nom du fichier (informatif : la destination est choisie localement).
        name: String,
        /// Taille totale du fichier en octets.
        size: u64,
        /// Taille nominale d'un chunk (le dernier peut être partiel).
        chunk_size: u32,
        /// Hash BLAKE3 de l'ensemble des octets transférés dans la session.
        root_hash: [u8; 32],
    },
    /// Un bloc de données. `index` est absolu (offset = index × chunk_size).
    Chunk {
        /// Numéro absolu du chunk dans le fichier.
        index: u64,
        /// Contenu du chunk (`chunk_size` octets, sauf le dernier).
        data: Vec<u8>,
    },
    /// Clôture du transfert : déclenche la vérification du hash racine.
    End,
}

impl TransferMsg {
    /// Sérialise le message en une trame binaire autonome (préfixe de longueur
    /// `u32` LE + tag + charge utile). Voir le format en tête de module.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut corps = Vec::new();
        match self {
            TransferMsg::Start {
                name,
                size,
                chunk_size,
                root_hash,
            } => {
                corps.push(TAG_START);
                corps.extend_from_slice(&(name.len() as u32).to_le_bytes());
                corps.extend_from_slice(name.as_bytes());
                corps.extend_from_slice(&size.to_le_bytes());
                corps.extend_from_slice(&chunk_size.to_le_bytes());
                corps.extend_from_slice(root_hash);
            }
            TransferMsg::Chunk { index, data } => {
                corps.push(TAG_CHUNK);
                corps.extend_from_slice(&index.to_le_bytes());
                corps.extend_from_slice(data);
            }
            TransferMsg::End => corps.push(TAG_END),
        }
        let mut trame = Vec::with_capacity(4 + corps.len());
        trame.extend_from_slice(&(corps.len() as u32).to_le_bytes());
        trame.extend_from_slice(&corps);
        trame
    }

    /// Désérialise **une** trame depuis le début de `buf` et renvoie le message
    /// avec le nombre d'octets consommés (permet de parcourir un flux de trames
    /// concaténées). [`NdError::Protocol`] si la trame est tronquée, si le tag
    /// est inconnu ou si la charge utile est incohérente.
    pub fn from_bytes(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < 4 {
            return Err(NdError::Protocol(
                "trame tronquée : préfixe de longueur incomplet".into(),
            ));
        }
        let longueur = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let fin = 4usize
            .checked_add(longueur)
            .ok_or_else(|| NdError::Protocol("longueur de trame démesurée".into()))?;
        if buf.len() < fin {
            return Err(NdError::Protocol(format!(
                "trame tronquée : {longueur} octets annoncés, {} disponibles",
                buf.len() - 4
            )));
        }
        let corps = &buf[4..fin];
        let (&tag, mut charge) = corps
            .split_first()
            .ok_or_else(|| NdError::Protocol("trame vide (tag manquant)".into()))?;
        let msg = match tag {
            TAG_START => {
                let long_nom = lire_u32(&mut charge)? as usize;
                let nom = String::from_utf8(lire_octets(&mut charge, long_nom)?.to_vec())
                    .map_err(|_| NdError::Protocol("nom de fichier non UTF-8".into()))?;
                let size = lire_u64(&mut charge)?;
                let chunk_size = lire_u32(&mut charge)?;
                let root_hash = lire_tableau::<32>(&mut charge)?;
                if !charge.is_empty() {
                    return Err(NdError::Protocol(
                        "octets excédentaires après le message Start".into(),
                    ));
                }
                TransferMsg::Start {
                    name: nom,
                    size,
                    chunk_size,
                    root_hash,
                }
            }
            TAG_CHUNK => {
                let index = lire_u64(&mut charge)?;
                TransferMsg::Chunk {
                    index,
                    data: charge.to_vec(),
                }
            }
            TAG_END => {
                if !charge.is_empty() {
                    return Err(NdError::Protocol(
                        "octets excédentaires après le message End".into(),
                    ));
                }
                TransferMsg::End
            }
            t => {
                return Err(NdError::Protocol(format!(
                    "tag de message de transfert inconnu : {t}"
                )))
            }
        };
        Ok((msg, fin))
    }
}

/// Prélève `n` octets en tête de `charge` (avance le curseur).
fn lire_octets<'a>(charge: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if charge.len() < n {
        return Err(NdError::Protocol(format!(
            "trame tronquée : {n} octets attendus, {} restants",
            charge.len()
        )));
    }
    let (tete, reste) = charge.split_at(n);
    *charge = reste;
    Ok(tete)
}

/// Lit un `u32` little-endian en tête de `charge`.
fn lire_u32(charge: &mut &[u8]) -> Result<u32> {
    Ok(u32::from_le_bytes(lire_tableau::<4>(charge)?))
}

/// Lit un `u64` little-endian en tête de `charge`.
fn lire_u64(charge: &mut &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(lire_tableau::<8>(charge)?))
}

/// Lit un tableau de `N` octets en tête de `charge`.
fn lire_tableau<const N: usize>(charge: &mut &[u8]) -> Result<[u8; N]> {
    let octets = lire_octets(charge, N)?;
    let mut tableau = [0u8; N];
    tableau.copy_from_slice(octets);
    Ok(tableau)
}

// ---------------------------------------------------------------------------
// Émetteur
// ---------------------------------------------------------------------------

/// Étape courante de l'émetteur dans la séquence Start → Chunk… → End.
enum Etape {
    /// Le `Start` n'a pas encore été produit.
    Start,
    /// Les chunks sont en cours de production (puis le `End`).
    Chunks,
    /// Séquence terminée : plus aucun message.
    Fini,
}

/// Émetteur : lit un fichier source et produit la séquence de [`TransferMsg`]
/// (`Start`, un `Chunk` par bloc du plan, `End`) via [`Self::next_message`].
///
/// Le plan de chunks ([`ChunkPlan`]) est calculé à la construction ; chaque
/// chunk est relu au moment de l'envoi et re-vérifié contre son hash BLAKE3
/// planifié, ce qui détecte une modification du fichier pendant le transfert.
pub struct FileSender {
    /// Fichier source, ouvert pour toute la durée du transfert.
    fichier: File,
    /// Plan de chunks couvrant `[resume_offset, EOF)`.
    plan: ChunkPlan,
    /// Nom de fichier annoncé dans le `Start`.
    nom: String,
    /// Position dans `plan.chunks` du prochain chunk à produire.
    prochain: usize,
    /// Étape courante de la séquence.
    etape: Etape,
}

impl FileSender {
    /// Prépare l'envoi de `path` à partir de `resume_offset` (0 pour un
    /// transfert complet), avec la taille de chunk par défaut
    /// ([`DEFAULT_CHUNK_SIZE`]). Voir [`Self::with_chunk_size`].
    pub fn new(path: &Path, resume_offset: u64) -> Result<Self> {
        Self::with_chunk_size(path, resume_offset, DEFAULT_CHUNK_SIZE)
    }

    /// Prépare l'envoi de `path` : calcule le plan de chunks (hashs BLAKE3 et
    /// hash racine, voir [`plan_file_chunks_with`]) puis ouvre le fichier pour
    /// la relecture. `resume_offset` doit être aligné sur `chunk_size`.
    pub fn with_chunk_size(path: &Path, resume_offset: u64, chunk_size: u32) -> Result<Self> {
        let plan = plan_file_chunks_with(path, resume_offset, chunk_size)?;
        let nom = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| NdError::Protocol("chemin source sans nom de fichier".into()))?;
        let fichier = File::open(path)?;
        Ok(Self {
            fichier,
            plan,
            nom,
            prochain: 0,
            etape: Etape::Start,
        })
    }

    /// Plan de chunks calculé à la construction (hashs, hash racine, reprise).
    pub fn plan(&self) -> &ChunkPlan {
        &self.plan
    }

    /// Nom de fichier annoncé dans le message `Start`.
    pub fn file_name(&self) -> &str {
        &self.nom
    }

    /// Produit le prochain message de la séquence, ou `Ok(None)` quand elle est
    /// épuisée (après le `End`). [`NdError::Protocol`] si un chunk relu ne
    /// correspond plus à son hash planifié (fichier modifié en cours de route).
    pub fn next_message(&mut self) -> Result<Option<TransferMsg>> {
        match self.etape {
            Etape::Start => {
                self.etape = Etape::Chunks;
                Ok(Some(TransferMsg::Start {
                    name: self.nom.clone(),
                    size: self.plan.file_len,
                    chunk_size: self.plan.chunk_size,
                    root_hash: self.plan.root_hash,
                }))
            }
            Etape::Chunks => {
                let Some(info) = self.plan.chunks.get(self.prochain).copied() else {
                    self.etape = Etape::Fini;
                    return Ok(Some(TransferMsg::End));
                };
                let mut data = vec![0u8; info.len as usize];
                self.fichier.seek(SeekFrom::Start(info.offset))?;
                self.fichier.read_exact(&mut data)?;
                if !verify_chunk(&info, &data) {
                    return Err(NdError::Protocol(format!(
                        "chunk {} modifié depuis la planification (hash BLAKE3 divergent)",
                        info.index
                    )));
                }
                self.prochain += 1;
                Ok(Some(TransferMsg::Chunk {
                    index: info.index,
                    data,
                }))
            }
            Etape::Fini => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Récepteur
// ---------------------------------------------------------------------------

/// Statut observable d'un [`FileReceiver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    /// En attente du message `Start`.
    AwaitingStart,
    /// `Start` reçu, chunks en cours de réception.
    Receiving,
    /// `End` reçu et hash racine BLAKE3 vérifié : fichier complet.
    Complete,
}

/// Progression d'un transfert côté récepteur (octets déjà présents dans la
/// destination, reprise incluse, sur la taille totale annoncée).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferProgress {
    /// Octets déjà écrits (ou conservés par la reprise) dans la destination.
    pub bytes_done: u64,
    /// Taille totale du fichier annoncée par le `Start` (0 avant le `Start`).
    pub bytes_total: u64,
}

impl TransferProgress {
    /// Fraction accomplie dans `[0, 1]` (1 pour un fichier vide terminé).
    pub fn ratio(&self) -> f64 {
        if self.bytes_total == 0 {
            1.0
        } else {
            self.bytes_done as f64 / self.bytes_total as f64
        }
    }
}

/// État interne du récepteur.
enum Etat {
    /// Aucun `Start` reçu.
    Attente,
    /// Transfert en cours (boîte : le hachage BLAKE3 incrémental est gros).
    Reception(Box<Reception>),
    /// Transfert terminé et vérifié.
    Termine {
        /// Nom annoncé par le `Start`.
        nom: String,
        /// Taille totale du fichier reçu.
        taille: u64,
        /// Offset de reprise qui avait été retenu au `Start`.
        reprise: u64,
    },
}

/// Données vivantes d'un transfert en cours de réception.
struct Reception {
    /// Fichier de destination, ouvert en écriture.
    fichier: File,
    /// Nom annoncé par le `Start`.
    nom: String,
    /// Taille totale annoncée par le `Start`.
    taille: u64,
    /// Taille nominale d'un chunk.
    chunk_size: u32,
    /// Hash racine BLAKE3 annoncé, vérifié au `End`.
    root_hash: [u8; 32],
    /// Offset de reprise retenu au `Start` (octets pré-existants conservés).
    reprise: u64,
    /// Offset du prochain octet attendu (reprise incluse).
    offset: u64,
    /// Hash BLAKE3 cumulé des octets reçus dans cette session.
    hachage: blake3::Hasher,
}

/// Récepteur : écrit les [`TransferMsg`] dans un fichier de destination, avec
/// vérification BLAKE3 (chunk par chunk quand le plan attendu est connu, hash
/// racine dans tous les cas au `End`) et reprise sur les chunks déjà présents.
///
/// Aucune E/S n'est faite avant le premier [`Self::accept`] d'un `Start` : la
/// destination est alors ouverte (créée au besoin), l'offset de reprise est
/// déduit de sa longueur (arrondi à la frontière de chunk inférieure) et une
/// éventuelle queue partielle est tronquée. Un message invalide, hors séquence
/// ou corrompu est rejeté par une erreur **sans faire avancer l'état** : le
/// chunk fautif peut être renvoyé.
pub struct FileReceiver {
    /// Chemin du fichier de destination.
    dest: PathBuf,
    /// Plan attendu (transmis hors bande) pour la vérification par chunk.
    plan_attendu: Option<ChunkPlan>,
    /// État courant du transfert.
    etat: Etat,
}

impl FileReceiver {
    /// Prépare la réception vers `dest`. L'intégrité est garantie par le hash
    /// racine au `End` ; pour un rejet immédiat des chunks corrompus, voir
    /// [`Self::with_expected_plan`].
    pub fn new(dest: &Path) -> Self {
        Self {
            dest: dest.to_path_buf(),
            plan_attendu: None,
            etat: Etat::Attente,
        }
    }

    /// Prépare la réception vers `dest` en connaissant le [`ChunkPlan`] attendu
    /// (obtenu hors bande, p. ex. via le canal de contrôle au plan 16) : chaque
    /// chunk est alors vérifié individuellement contre son hash BLAKE3 et un
    /// chunk corrompu est rejeté immédiatement, sans être écrit.
    pub fn with_expected_plan(dest: &Path, plan: ChunkPlan) -> Self {
        Self {
            dest: dest.to_path_buf(),
            plan_attendu: Some(plan),
            etat: Etat::Attente,
        }
    }

    /// Statut courant du transfert.
    pub fn status(&self) -> TransferStatus {
        match self.etat {
            Etat::Attente => TransferStatus::AwaitingStart,
            Etat::Reception(_) => TransferStatus::Receiving,
            Etat::Termine { .. } => TransferStatus::Complete,
        }
    }

    /// Progression courante (octets présents / taille totale annoncée).
    pub fn progress(&self) -> TransferProgress {
        match &self.etat {
            Etat::Attente => TransferProgress {
                bytes_done: 0,
                bytes_total: 0,
            },
            Etat::Reception(r) => TransferProgress {
                bytes_done: r.offset,
                bytes_total: r.taille,
            },
            Etat::Termine { taille, .. } => TransferProgress {
                bytes_done: *taille,
                bytes_total: *taille,
            },
        }
    }

    /// Nom de fichier annoncé par le `Start` (une fois celui-ci reçu).
    pub fn file_name(&self) -> Option<&str> {
        match &self.etat {
            Etat::Attente => None,
            Etat::Reception(r) => Some(&r.nom),
            Etat::Termine { nom, .. } => Some(nom),
        }
    }

    /// Offset de reprise retenu au `Start` (octets pré-existants conservés),
    /// une fois le transfert commencé.
    pub fn resume_offset(&self) -> Option<u64> {
        match &self.etat {
            Etat::Attente => None,
            Etat::Reception(r) => Some(r.reprise),
            Etat::Termine { reprise, .. } => Some(*reprise),
        }
    }

    /// Consomme un message du protocole et renvoie le statut atteint.
    ///
    /// En cas d'erreur (message hors séquence, longueur ou index inattendus,
    /// hash BLAKE3 invalide…), l'état n'avance pas : un chunk rejeté peut être
    /// renvoyé tel quel. Le `End` vérifie que tous les octets sont là puis que
    /// le hash racine BLAKE3 correspond, et scelle le statut à `Complete`.
    pub fn accept(&mut self, msg: TransferMsg) -> Result<TransferStatus> {
        match msg {
            TransferMsg::Start {
                name,
                size,
                chunk_size,
                root_hash,
            } => self.accepter_start(name, size, chunk_size, root_hash),
            TransferMsg::Chunk { index, data } => self.accepter_chunk(index, &data),
            TransferMsg::End => self.accepter_end(),
        }
    }

    /// Traite le `Start` : ouvre la destination, détermine l'offset de reprise
    /// à partir des octets déjà présents et tronque toute queue partielle.
    fn accepter_start(
        &mut self,
        nom: String,
        taille: u64,
        chunk_size: u32,
        root_hash: [u8; 32],
    ) -> Result<TransferStatus> {
        if !matches!(self.etat, Etat::Attente) {
            return Err(NdError::Protocol(
                "message Start inattendu : transfert déjà commencé ou terminé".into(),
            ));
        }
        if chunk_size == 0 {
            return Err(NdError::Protocol(
                "taille de chunk nulle dans le message Start".into(),
            ));
        }
        let fichier = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.dest)?;
        // Reprise sur frontière de chunk : les octets pré-existants alignés
        // sont conservés (non re-vérifiés : le hash racine ne couvre que le
        // suffixe) ; un contenu plus long que la taille annoncée est écarté.
        let existant = fichier.metadata()?.len();
        let reprise = if existant > taille {
            0
        } else {
            existant - existant % u64::from(chunk_size)
        };
        if let Some(plan) = &self.plan_attendu {
            if plan.chunk_size != chunk_size
                || plan.file_len != taille
                || plan.root_hash != root_hash
                || plan.resume_offset != reprise
            {
                return Err(NdError::Protocol(
                    "plan attendu incompatible avec le message Start \
                     (taille, taille de chunk, hash racine ou offset de reprise)"
                        .into(),
                ));
            }
        }
        fichier.set_len(reprise)?;
        self.etat = Etat::Reception(Box::new(Reception {
            fichier,
            nom,
            taille,
            chunk_size,
            root_hash,
            reprise,
            offset: reprise,
            hachage: blake3::Hasher::new(),
        }));
        Ok(TransferStatus::Receiving)
    }

    /// Traite un `Chunk` : contrôles de séquence et de longueur, vérification
    /// BLAKE3 (si plan attendu), écriture à l'offset et mise à jour du hash
    /// racine cumulé.
    fn accepter_chunk(&mut self, index: u64, data: &[u8]) -> Result<TransferStatus> {
        let Etat::Reception(r) = &mut self.etat else {
            return Err(NdError::Protocol(
                "message Chunk hors transfert (Start manquant ou transfert terminé)".into(),
            ));
        };
        if r.offset >= r.taille {
            return Err(NdError::Protocol(format!(
                "chunk {index} inattendu : tous les octets sont déjà reçus"
            )));
        }
        let attendu = r.offset / u64::from(r.chunk_size);
        if index != attendu {
            return Err(NdError::Protocol(format!(
                "chunk hors séquence : index {index} reçu, {attendu} attendu"
            )));
        }
        let voulu = (r.taille - r.offset).min(u64::from(r.chunk_size)) as usize;
        if data.len() != voulu {
            return Err(NdError::Protocol(format!(
                "longueur du chunk {index} inattendue : {} octets reçus, {voulu} attendus",
                data.len()
            )));
        }
        // Vérification BLAKE3 chunk par chunk quand le plan attendu est connu :
        // un chunk corrompu est rejeté sans être écrit ni compté, et pourra
        // être renvoyé (l'état n'a pas avancé).
        if let Some(plan) = &self.plan_attendu {
            let premier = r.reprise / u64::from(r.chunk_size);
            let info = plan.chunks.get((index - premier) as usize).ok_or_else(|| {
                NdError::Protocol(format!("chunk {index} absent du plan attendu"))
            })?;
            if !verify_chunk(info, data) {
                return Err(NdError::Protocol(format!(
                    "intégrité BLAKE3 invalide pour le chunk {index} : chunk rejeté"
                )));
            }
        }
        r.fichier.seek(SeekFrom::Start(r.offset))?;
        r.fichier.write_all(data)?;
        r.hachage.update(data);
        r.offset += voulu as u64;
        Ok(TransferStatus::Receiving)
    }

    /// Traite le `End` : exige que tous les octets soient reçus, vérifie le
    /// hash racine BLAKE3 de la session puis scelle le transfert.
    fn accepter_end(&mut self) -> Result<TransferStatus> {
        let Etat::Reception(r) = &mut self.etat else {
            return Err(NdError::Protocol(
                "message End hors transfert (Start manquant ou transfert déjà terminé)".into(),
            ));
        };
        if r.offset != r.taille {
            return Err(NdError::Protocol(format!(
                "End prématuré : {} octets reçus sur {}",
                r.offset, r.taille
            )));
        }
        let calcule = *r.hachage.finalize().as_bytes();
        if calcule != r.root_hash {
            return Err(NdError::Protocol(
                "hash racine BLAKE3 invalide : le contenu reçu ne correspond pas au Start".into(),
            ));
        }
        r.fichier.sync_all()?;
        let (nom, taille, reprise) = (r.nom.clone(), r.taille, r.reprise);
        self.etat = Etat::Termine {
            nom,
            taille,
            reprise,
        };
        Ok(TransferStatus::Complete)
    }
}

/// Offset de reprise à demander à l'émetteur pour une destination donnée :
/// longueur du fichier existant arrondie à la frontière de chunk inférieure
/// (0 si la destination n'existe pas). À passer à [`FileSender::new`] /
/// [`FileSender::with_chunk_size`] ; si la destination est plus longue que la
/// source (fichier différent), la planification échouera et l'appelant doit
/// repartir de 0.
pub fn resume_offset(dest: &Path, chunk_size: u32) -> Result<u64> {
    if chunk_size == 0 {
        return Err(NdError::Protocol("taille de chunk nulle".into()));
    }
    let longueur = match std::fs::metadata(dest) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    Ok(longueur - longueur % u64::from(chunk_size))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_hash;
    use std::path::PathBuf;

    /// Chemin temporaire unique pour un test (évite les collisions entre tests
    /// parallèles et entre exécutions).
    fn chemin_temp(nom: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nd_files_transfer_{}_{nom}", std::process::id()))
    }

    /// Motif déterministe non trivial (chaque offset produit un octet distinct
    /// de ses voisins, sans période courte évidente).
    fn motif(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
            .collect()
    }

    /// Draine l'émetteur en une séquence complète de messages.
    fn sequence(emetteur: &mut FileSender) -> Vec<TransferMsg> {
        let mut msgs = Vec::new();
        while let Some(msg) = emetteur.next_message().unwrap() {
            msgs.push(msg);
        }
        msgs
    }

    #[test]
    fn round_trip_des_messages() {
        let start = TransferMsg::Start {
            name: "façade—é.bin".to_string(),
            size: 123_456_789,
            chunk_size: 65_536,
            root_hash: [0xAB; 32],
        };
        let chunk = TransferMsg::Chunk {
            index: 42,
            data: motif(1000),
        };
        let end = TransferMsg::End;

        // Chaque variante survit à l'aller-retour et consomme toute sa trame.
        for msg in [&start, &chunk, &end] {
            let octets = msg.to_bytes();
            let (relu, consomme) = TransferMsg::from_bytes(&octets).unwrap();
            assert_eq!(&relu, msg);
            assert_eq!(consomme, octets.len());
        }

        // Un flux de trames concaténées se parcourt trame par trame.
        let mut flux = Vec::new();
        for msg in [&start, &chunk, &end] {
            flux.extend_from_slice(&msg.to_bytes());
        }
        let mut position = 0;
        for attendu in [&start, &chunk, &end] {
            let (relu, consomme) = TransferMsg::from_bytes(&flux[position..]).unwrap();
            assert_eq!(&relu, attendu);
            position += consomme;
        }
        assert_eq!(position, flux.len());

        // Trames invalides : tronquées, tag inconnu, nom non UTF-8, excédent.
        assert!(TransferMsg::from_bytes(&[]).is_err());
        assert!(TransferMsg::from_bytes(&[10, 0, 0, 0, TAG_END]).is_err()); // corps annoncé absent
        assert!(TransferMsg::from_bytes(&[0, 0, 0, 0]).is_err()); // corps vide (tag manquant)
        assert!(TransferMsg::from_bytes(&[1, 0, 0, 0, 99]).is_err()); // tag inconnu
        assert!(TransferMsg::from_bytes(&[2, 0, 0, 0, TAG_END, 0]).is_err()); // excédent après End
        let mut nom_invalide = TransferMsg::Start {
            name: "ab".to_string(),
            size: 0,
            chunk_size: 1,
            root_hash: [0; 32],
        }
        .to_bytes();
        // Le nom commence après [longueur u32][tag][long. nom u32] = 9 octets.
        nom_invalide[9] = 0xFF;
        nom_invalide[10] = 0xFF;
        assert!(TransferMsg::from_bytes(&nom_invalide).is_err());
    }

    #[test]
    fn transfert_de_bout_en_bout_en_memoire() {
        const CHUNK: u32 = 8 * 1024;
        // 2 chunks pleins + 1 chunk partiel.
        let contenu = motif(2 * CHUNK as usize + 1234);
        let src = chemin_temp("e2e_src.bin");
        let dst = chemin_temp("e2e_dst.bin");
        std::fs::write(&src, &contenu).unwrap();
        let _ = std::fs::remove_file(&dst);

        let mut emetteur = FileSender::with_chunk_size(&src, 0, CHUNK).unwrap();
        assert_eq!(emetteur.plan().chunks.len(), 3);
        let mut recepteur = FileReceiver::new(&dst);
        assert_eq!(recepteur.status(), TransferStatus::AwaitingStart);

        // Chaque message passe par la sérialisation binaire (comme sur le fil).
        let mut nb_messages = 0;
        let mut statut = recepteur.status();
        while let Some(msg) = emetteur.next_message().unwrap() {
            let octets = msg.to_bytes();
            let (decode, consomme) = TransferMsg::from_bytes(&octets).unwrap();
            assert_eq!(consomme, octets.len());
            statut = recepteur.accept(decode).unwrap();
            nb_messages += 1;
        }
        assert_eq!(nb_messages, 1 + 3 + 1); // Start + 3 chunks + End
        assert_eq!(statut, TransferStatus::Complete);
        assert_eq!(recepteur.status(), TransferStatus::Complete);
        // Le nom annoncé au récepteur est celui du fichier source.
        assert_eq!(recepteur.file_name(), Some(emetteur.file_name()));
        let progression = recepteur.progress();
        assert_eq!(progression.bytes_done, contenu.len() as u64);
        assert_eq!(progression.bytes_total, contenu.len() as u64);
        assert!((progression.ratio() - 1.0).abs() < f64::EPSILON);

        // Le fichier reconstruit est octet pour octet identique à la source
        // et son hash BLAKE3 correspond au hash racine annoncé.
        let relu = std::fs::read(&dst).unwrap();
        assert_eq!(relu, contenu);
        assert_eq!(chunk_hash(&relu), emetteur.plan().root_hash);

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn chunk_corrompu_rejete() {
        const CHUNK: u32 = 4 * 1024;
        let contenu = motif(2 * CHUNK as usize);
        let src = chemin_temp("corrompu_src.bin");
        std::fs::write(&src, &contenu).unwrap();

        let mut emetteur = FileSender::with_chunk_size(&src, 0, CHUNK).unwrap();
        let plan = emetteur.plan().clone();
        let msgs = sequence(&mut emetteur);
        let TransferMsg::Chunk { index, data } = &msgs[1] else {
            panic!("le deuxième message doit être un Chunk");
        };
        let mut corrompu = data.clone();
        corrompu[100] ^= 0xFF;

        // --- Avec plan attendu : rejet immédiat du chunk corrompu (hash BLAKE3
        //     invalide), sans avancer, puis retransmission du chunk intact.
        let dst = chemin_temp("corrompu_dst_plan.bin");
        let _ = std::fs::remove_file(&dst);
        let mut recepteur = FileReceiver::with_expected_plan(&dst, plan);
        recepteur.accept(msgs[0].clone()).unwrap();
        let refus = recepteur.accept(TransferMsg::Chunk {
            index: *index,
            data: corrompu.clone(),
        });
        assert!(matches!(refus, Err(NdError::Protocol(_))));
        assert_eq!(recepteur.progress().bytes_done, 0); // rien n'a été écrit
        assert_eq!(recepteur.status(), TransferStatus::Receiving);
        // Retransmission du chunk intact, puis fin normale du transfert.
        for msg in &msgs[1..] {
            recepteur.accept(msg.clone()).unwrap();
        }
        assert_eq!(recepteur.status(), TransferStatus::Complete);
        assert_eq!(std::fs::read(&dst).unwrap(), contenu);
        let _ = std::fs::remove_file(&dst);

        // --- Sans plan attendu : la corruption est détectée au End par le
        //     hash racine BLAKE3, le transfert n'est jamais Complete.
        let dst = chemin_temp("corrompu_dst_racine.bin");
        let _ = std::fs::remove_file(&dst);
        let mut recepteur = FileReceiver::new(&dst);
        recepteur.accept(msgs[0].clone()).unwrap();
        recepteur
            .accept(TransferMsg::Chunk {
                index: *index,
                data: corrompu,
            })
            .unwrap(); // indétectable isolément sans plan…
        recepteur.accept(msgs[2].clone()).unwrap();
        let fin = recepteur.accept(TransferMsg::End);
        assert!(matches!(fin, Err(NdError::Protocol(_)))); // …mais rejeté au End
        assert_ne!(recepteur.status(), TransferStatus::Complete);
        let _ = std::fs::remove_file(&dst);

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn reprise_depuis_un_offset() {
        const CHUNK: u32 = 8 * 1024;
        // 3 chunks pleins + 1 chunk partiel.
        let contenu = motif(3 * CHUNK as usize + 517);
        let src = chemin_temp("reprise_src.bin");
        let dst = chemin_temp("reprise_dst.bin");
        std::fs::write(&src, &contenu).unwrap();

        // Destination partielle : 2 chunks complets déjà reçus + une queue
        // partielle (transfert interrompu en plein chunk) qui sera écartée.
        let mut partiel = contenu[..2 * CHUNK as usize].to_vec();
        partiel.extend_from_slice(&contenu[2 * CHUNK as usize..2 * CHUNK as usize + 123]);
        std::fs::write(&dst, &partiel).unwrap();

        // L'orchestrateur interroge la destination puis planifie la reprise.
        let reprise = resume_offset(&dst, CHUNK).unwrap();
        assert_eq!(reprise, 2 * u64::from(CHUNK));
        let mut emetteur = FileSender::with_chunk_size(&src, reprise, CHUNK).unwrap();
        assert_eq!(emetteur.plan().resume_offset, reprise);
        assert_eq!(emetteur.plan().chunks.len(), 2);
        // Le hash racine de la session ne couvre que le suffixe repris.
        assert_eq!(
            emetteur.plan().root_hash,
            chunk_hash(&contenu[reprise as usize..])
        );

        let mut recepteur = FileReceiver::new(&dst);
        let msgs = sequence(&mut emetteur);
        assert_eq!(msgs.len(), 1 + 2 + 1); // Start + 2 chunks restants + End
        assert!(matches!(msgs[1], TransferMsg::Chunk { index: 2, .. }));
        for msg in msgs {
            recepteur.accept(msg).unwrap();
        }
        assert_eq!(recepteur.status(), TransferStatus::Complete);
        assert_eq!(recepteur.resume_offset(), Some(reprise));
        let progression = recepteur.progress();
        assert_eq!(progression.bytes_done, contenu.len() as u64);
        assert_eq!(progression.bytes_total, contenu.len() as u64);

        // Fichier final octet pour octet identique à la source complète.
        let relu = std::fs::read(&dst).unwrap();
        assert_eq!(relu, contenu);
        assert_eq!(chunk_hash(&relu), chunk_hash(&contenu));

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn messages_hors_sequence_rejetes() {
        const CHUNK: u32 = 64;
        let contenu = motif(100); // 1 chunk plein + 1 chunk partiel
        let dst = chemin_temp("sequence_dst.bin");
        let _ = std::fs::remove_file(&dst);
        let start = TransferMsg::Start {
            name: "sequence.bin".to_string(),
            size: contenu.len() as u64,
            chunk_size: CHUNK,
            root_hash: chunk_hash(&contenu),
        };

        let mut recepteur = FileReceiver::new(&dst);
        // Avant Start : Chunk et End sont hors séquence.
        assert!(recepteur
            .accept(TransferMsg::Chunk {
                index: 0,
                data: contenu[..CHUNK as usize].to_vec(),
            })
            .is_err());
        assert!(recepteur.accept(TransferMsg::End).is_err());
        // Start à taille de chunk nulle : refusé.
        assert!(recepteur
            .accept(TransferMsg::Start {
                name: "zero.bin".to_string(),
                size: 1,
                chunk_size: 0,
                root_hash: [0; 32],
            })
            .is_err());

        recepteur.accept(start.clone()).unwrap();
        // Un second Start en cours de transfert est refusé.
        assert!(recepteur.accept(start).is_err());
        // Index hors séquence, longueur inattendue, End prématuré : refusés
        // sans faire avancer la progression.
        assert!(recepteur
            .accept(TransferMsg::Chunk {
                index: 5,
                data: contenu[..CHUNK as usize].to_vec(),
            })
            .is_err());
        assert!(recepteur
            .accept(TransferMsg::Chunk {
                index: 0,
                data: contenu[..10].to_vec(),
            })
            .is_err());
        assert!(recepteur.accept(TransferMsg::End).is_err());
        assert_eq!(recepteur.progress().bytes_done, 0);

        // La séquence correcte aboutit malgré les rejets intermédiaires.
        recepteur
            .accept(TransferMsg::Chunk {
                index: 0,
                data: contenu[..CHUNK as usize].to_vec(),
            })
            .unwrap();
        recepteur
            .accept(TransferMsg::Chunk {
                index: 1,
                data: contenu[CHUNK as usize..].to_vec(),
            })
            .unwrap();
        assert_eq!(
            recepteur.accept(TransferMsg::End).unwrap(),
            TransferStatus::Complete
        );
        // Tout message après la fin est refusé.
        assert!(recepteur.accept(TransferMsg::End).is_err());
        assert_eq!(std::fs::read(&dst).unwrap(), contenu);

        let _ = std::fs::remove_file(&dst);
    }
}
