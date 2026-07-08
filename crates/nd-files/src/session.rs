//! Session de transfert pilotable par canal (plans 09/16), au-dessus du
//! protocole message par message [`crate::transfer`] (chunks BLAKE3 + reprise).
//!
//! [`TransferSession`] est une **machine à états orientée octets** : elle ne
//! touche jamais au réseau. nd-core n'a qu'à faire circuler sur un canal fiable
//! (canal `Files`) les octets produits par [`TransferSession::poll_outgoing`] et
//! à réinjecter les octets reçus dans [`TransferSession::handle_incoming`]. Tout
//! le reste — file de plusieurs fichiers, négociation de reprise, progression,
//! annulation, pause — est géré ici et reste testable entièrement en mémoire.
//!
//! # Deux rôles, une seule API de canal
//!
//! * [`TransferSession::send`] : côté émetteur, pilote une file de fichiers.
//! * [`TransferSession::receive`] : côté récepteur, écrit dans un répertoire.
//!
//! Les deux rôles exposent exactement la même surface (`poll_outgoing`,
//! `handle_incoming`, `pause`, `resume`, `cancel`, `progress`, `take_events`).
//!
//! # Protocole de session (au-dessus des trames [`TransferMsg`])
//!
//! Chaque fichier est négocié puis streamé séquentiellement :
//!
//! ```text
//! émetteur → Offer(seq, nom, taille, chunk)     « je veux envoyer ce fichier »
//! récepteur → Resume(seq, offset)               « reprends à cet offset »   (offset=0 si neuf)
//! émetteur → Data(Start) Data(Chunk)… Data(End) « le fichier, via TransferMsg »
//! …répété pour chaque fichier de la file…
//! émetteur → Done                               « toute la file est passée »
//! ```
//!
//! `Cancel` peut être émis à tout moment par l'un ou l'autre pair. L'`offset`
//! de reprise est déterminé par le **récepteur** à partir des octets déjà
//! présents dans la destination (frontière de chunk), d'où la reprise après
//! coupure sans coordination externe.
//!
//! # Cadre binaire de session
//!
//! ```text
//! trame  = [longueur du corps : u32 LE][corps]
//! corps  = [tag : u8][charge utile]
//!
//! Offer  (tag 1) : [seq u64][long. nom u32][nom UTF-8][taille u64][chunk u32]
//! Resume (tag 2) : [seq u64][offset u64]
//! Data   (tag 3) : [trame TransferMsg complète (cf. crate::transfer)]
//! Cancel (tag 4) : charge vide
//! Done   (tag 5) : charge vide
//! ```
//!
//! Le préfixe de longueur permet de réassembler un flux d'octets quelconque :
//! [`TransferSession::handle_incoming`] accepte des tranches arbitraires et
//! bufferise les trames partielles.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Instant;

use nd_proto::{NdError, Result};

use crate::transfer::{FileReceiver, FileSender, TransferMsg, TransferStatus};
use crate::DEFAULT_CHUNK_SIZE;

// ---------------------------------------------------------------------------
// Évènements et progression
// ---------------------------------------------------------------------------

/// Progression détaillée d'un fichier et de la session, portée par
/// [`TransferEvent::Progress`]. Le débit et l'ETA portent sur la **session**
/// entière (tous fichiers confondus) et ne comptent que les octets réellement
/// déplacés pendant cette session (hors reprise).
#[derive(Debug, Clone, PartialEq)]
pub struct TransferProgressInfo {
    /// Index (0-basé) du fichier courant dans la file.
    pub file_index: u64,
    /// Nom du fichier courant.
    pub file_name: String,
    /// Octets présents dans la destination pour le fichier courant (reprise incluse).
    pub file_bytes_done: u64,
    /// Taille totale du fichier courant.
    pub file_bytes_total: u64,
    /// Octets présents pour l'ensemble de la file (reprise incluse).
    pub session_bytes_done: u64,
    /// Taille totale connue de la file.
    pub session_bytes_total: u64,
    /// Débit instantané moyen de la session (octets/seconde).
    pub bytes_per_sec: f64,
    /// Temps estimé avant la fin de la session (secondes), si un débit existe.
    pub eta_secs: Option<f64>,
}

impl TransferProgressInfo {
    /// Fraction accomplie du fichier courant dans `[0, 1]`.
    pub fn file_ratio(&self) -> f64 {
        ratio(self.file_bytes_done, self.file_bytes_total)
    }

    /// Fraction accomplie de la session dans `[0, 1]`.
    pub fn session_ratio(&self) -> f64 {
        ratio(self.session_bytes_done, self.session_bytes_total)
    }

    /// Pourcentage accompli de la session dans `[0, 100]`.
    pub fn percent(&self) -> f64 {
        self.session_ratio() * 100.0
    }
}

/// Évènement observable d'une [`TransferSession`], à drainer via
/// [`TransferSession::take_events`]. Chaque rôle émet sa propre vue (l'émetteur
/// selon ce qu'il envoie, le récepteur selon ce qu'il écrit).
#[derive(Debug, Clone, PartialEq)]
pub enum TransferEvent {
    /// Un fichier commence : index dans la file, nom, taille, offset de reprise.
    FileStarted {
        index: u64,
        name: String,
        size: u64,
        resume_offset: u64,
    },
    /// Progression périodique (émise à chaque chunk).
    Progress(TransferProgressInfo),
    /// Un fichier est terminé (et vérifié BLAKE3 côté récepteur).
    FileCompleted { index: u64, name: String, size: u64 },
    /// Toute la file a été transférée.
    Finished,
    /// La session a été annulée (localement ou par le pair).
    Cancelled,
}

/// Instantané de progression global, interrogeable sans drainer les évènements.
///
/// Côté émetteur, `files_total`/`bytes_total` couvrent toute la file connue
/// d'avance. Côté récepteur, ils ne couvrent que les fichiers déjà annoncés
/// (le récepteur découvre la file au fil des `Offer`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProgress {
    /// Nombre de fichiers de la file (connus à ce stade).
    pub files_total: u64,
    /// Nombre de fichiers entièrement transférés.
    pub files_completed: u64,
    /// Octets présents (reprise incluse) sur l'ensemble de la file.
    pub bytes_done: u64,
    /// Taille totale connue de la file.
    pub bytes_total: u64,
    /// Nom du fichier en cours, s'il y en a un.
    pub current_file: Option<String>,
}

impl SessionProgress {
    /// Fraction accomplie dans `[0, 1]`.
    pub fn ratio(&self) -> f64 {
        ratio(self.bytes_done, self.bytes_total)
    }

    /// Pourcentage accompli dans `[0, 100]`.
    pub fn percent(&self) -> f64 {
        self.ratio() * 100.0
    }
}

/// Fraction `done/total` bornée, avec la convention « vide = 1.0 » (cohérente
/// avec [`crate::transfer::TransferProgress::ratio`]).
fn ratio(done: u64, total: u64) -> f64 {
    if total == 0 {
        1.0
    } else {
        done as f64 / total as f64
    }
}

/// Débit moyen de session et ETA, calculés à partir des octets réellement
/// déplacés et du temps écoulé depuis le début de la session.
fn rate_eta(moved: u64, remaining: u64, started: Instant) -> (f64, Option<f64>) {
    let elapsed = started.elapsed().as_secs_f64();
    if elapsed <= 0.0 || moved == 0 {
        return (0.0, None);
    }
    let rate = moved as f64 / elapsed;
    let eta = if rate > 0.0 {
        Some(remaining as f64 / rate)
    } else {
        None
    };
    (rate, eta)
}

// ---------------------------------------------------------------------------
// Cadre binaire de session
// ---------------------------------------------------------------------------

const F_OFFER: u8 = 1;
const F_RESUME: u8 = 2;
const F_DATA: u8 = 3;
const F_CANCEL: u8 = 4;
const F_DONE: u8 = 5;

/// Trame du protocole de session (voir le cadre binaire en tête de module).
#[derive(Debug, Clone, PartialEq)]
enum Frame {
    Offer {
        seq: u64,
        name: String,
        size: u64,
        chunk_size: u32,
    },
    Resume {
        seq: u64,
        offset: u64,
    },
    Data(TransferMsg),
    Cancel,
    Done,
}

impl Frame {
    /// Sérialise la trame (préfixe de longueur `u32` LE + tag + charge).
    fn encode(&self) -> Vec<u8> {
        let mut corps = Vec::new();
        match self {
            Frame::Offer {
                seq,
                name,
                size,
                chunk_size,
            } => {
                corps.push(F_OFFER);
                corps.extend_from_slice(&seq.to_le_bytes());
                corps.extend_from_slice(&(name.len() as u32).to_le_bytes());
                corps.extend_from_slice(name.as_bytes());
                corps.extend_from_slice(&size.to_le_bytes());
                corps.extend_from_slice(&chunk_size.to_le_bytes());
            }
            Frame::Resume { seq, offset } => {
                corps.push(F_RESUME);
                corps.extend_from_slice(&seq.to_le_bytes());
                corps.extend_from_slice(&offset.to_le_bytes());
            }
            Frame::Data(msg) => {
                corps.push(F_DATA);
                // La trame TransferMsg est autonome (préfixe de longueur interne).
                corps.extend_from_slice(&msg.to_bytes());
            }
            Frame::Cancel => corps.push(F_CANCEL),
            Frame::Done => corps.push(F_DONE),
        }
        let mut trame = Vec::with_capacity(4 + corps.len());
        trame.extend_from_slice(&(corps.len() as u32).to_le_bytes());
        trame.extend_from_slice(&corps);
        trame
    }

    /// Tente de décoder **une** trame en tête de `buf`. Renvoie `Ok(None)` si les
    /// octets sont insuffisants (trame partielle : réessayer plus tard),
    /// `Ok(Some((trame, consommés)))` sinon. [`NdError::Protocol`] si une trame
    /// complète est malformée.
    fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let longueur = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let fin = 4usize
            .checked_add(longueur)
            .ok_or_else(|| NdError::Protocol("longueur de trame de session démesurée".into()))?;
        if buf.len() < fin {
            return Ok(None);
        }
        let corps = &buf[4..fin];
        let (&tag, mut charge) = corps
            .split_first()
            .ok_or_else(|| NdError::Protocol("trame de session vide (tag manquant)".into()))?;
        let frame = match tag {
            F_OFFER => {
                let seq = lire_u64(&mut charge)?;
                let long_nom = lire_u32(&mut charge)? as usize;
                let nom = String::from_utf8(lire_octets(&mut charge, long_nom)?.to_vec())
                    .map_err(|_| NdError::Protocol("nom de fichier non UTF-8".into()))?;
                let size = lire_u64(&mut charge)?;
                let chunk_size = lire_u32(&mut charge)?;
                exiger_vide(charge, "Offer")?;
                Frame::Offer {
                    seq,
                    name: nom,
                    size,
                    chunk_size,
                }
            }
            F_RESUME => {
                let seq = lire_u64(&mut charge)?;
                let offset = lire_u64(&mut charge)?;
                exiger_vide(charge, "Resume")?;
                Frame::Resume { seq, offset }
            }
            F_DATA => {
                let (msg, consomme) = TransferMsg::from_bytes(charge)?;
                if consomme != charge.len() {
                    return Err(NdError::Protocol(
                        "octets excédentaires après le TransferMsg encapsulé".into(),
                    ));
                }
                Frame::Data(msg)
            }
            F_CANCEL => {
                exiger_vide(charge, "Cancel")?;
                Frame::Cancel
            }
            F_DONE => {
                exiger_vide(charge, "Done")?;
                Frame::Done
            }
            t => {
                return Err(NdError::Protocol(format!(
                    "tag de trame de session inconnu : {t}"
                )))
            }
        };
        Ok(Some((frame, fin)))
    }
}

/// Exige que `charge` soit vidée (aucun octet excédentaire).
fn exiger_vide(charge: &[u8], quoi: &str) -> Result<()> {
    if charge.is_empty() {
        Ok(())
    } else {
        Err(NdError::Protocol(format!(
            "octets excédentaires après le message {quoi}"
        )))
    }
}

/// Prélève `n` octets en tête de `charge` (avance le curseur).
fn lire_octets<'a>(charge: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if charge.len() < n {
        return Err(NdError::Protocol(format!(
            "trame de session tronquée : {n} octets attendus, {} restants",
            charge.len()
        )));
    }
    let (tete, reste) = charge.split_at(n);
    *charge = reste;
    Ok(tete)
}

/// Lit un `u32` little-endian en tête de `charge`.
fn lire_u32(charge: &mut &[u8]) -> Result<u32> {
    let o = lire_octets(charge, 4)?;
    Ok(u32::from_le_bytes([o[0], o[1], o[2], o[3]]))
}

/// Lit un `u64` little-endian en tête de `charge`.
fn lire_u64(charge: &mut &[u8]) -> Result<u64> {
    let o = lire_octets(charge, 8)?;
    Ok(u64::from_le_bytes([
        o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7],
    ]))
}

// ---------------------------------------------------------------------------
// Machine à états
// ---------------------------------------------------------------------------

/// Cycle de vie commun aux deux rôles vis-à-vis de l'annulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// Session active.
    Active,
    /// Annulation demandée localement, trame `Cancel` pas encore émise.
    CancelPending,
    /// Session annulée (trame `Cancel` émise ou reçue).
    Cancelled,
}

/// Un fichier de la file d'émission.
struct Queued {
    path: PathBuf,
    name: String,
    size: u64,
    chunk_size: u32,
}

/// Étape de l'émetteur pour le fichier courant.
enum SenderStage {
    /// Il faut émettre l'`Offer` du fichier courant (ou `Done` si file épuisée).
    Offer,
    /// `Offer` émis, en attente du `Resume` du pair.
    AwaitResume,
    /// `Resume` reçu : streaming des `TransferMsg` du fichier courant.
    Stream(Box<FileSender>),
    /// `Done` émis : plus rien à envoyer.
    Finished,
}

/// État de l'émetteur : file de fichiers + position + compteurs de progression.
struct SenderSide {
    queue: Vec<Queued>,
    current: usize,
    stage: SenderStage,
    total_bytes: u64,
    /// Octets des fichiers déjà entièrement envoyés.
    done_bytes: u64,
    /// Offset de reprise du fichier courant (octets déjà présents, non renvoyés).
    cur_offset_base: u64,
    /// Octets du corps déjà envoyés pour le fichier courant.
    cur_sent: u64,
    /// Octets réellement déplacés sur toute la session (pour le débit).
    moved: u64,
}

impl SenderSide {
    fn new(queue: Vec<Queued>, total_bytes: u64) -> Self {
        Self {
            queue,
            current: 0,
            stage: SenderStage::Offer,
            total_bytes,
            done_bytes: 0,
            cur_offset_base: 0,
            cur_sent: 0,
            moved: 0,
        }
    }

    /// Produit la prochaine trame à émettre, ou `None` s'il faut attendre le pair.
    fn produce(&mut self, started: Instant, evs: &mut Vec<TransferEvent>) -> Result<Option<Frame>> {
        loop {
            match &mut self.stage {
                SenderStage::Offer => {
                    if self.current >= self.queue.len() {
                        self.stage = SenderStage::Finished;
                        evs.push(TransferEvent::Finished);
                        return Ok(Some(Frame::Done));
                    }
                    let q = &self.queue[self.current];
                    let frame = Frame::Offer {
                        seq: self.current as u64,
                        name: q.name.clone(),
                        size: q.size,
                        chunk_size: q.chunk_size,
                    };
                    self.stage = SenderStage::AwaitResume;
                    return Ok(Some(frame));
                }
                SenderStage::AwaitResume | SenderStage::Finished => return Ok(None),
                SenderStage::Stream(fs) => {
                    let msg = fs.next_message()?;
                    match msg {
                        Some(msg) => {
                            if let TransferMsg::Chunk { data, .. } = &msg {
                                let len = data.len() as u64;
                                self.cur_sent += len;
                                self.moved += len;
                                let q = &self.queue[self.current];
                                let file_done = self.cur_offset_base + self.cur_sent;
                                let session_done = self.done_bytes + file_done;
                                let remaining = self.total_bytes.saturating_sub(session_done);
                                let (rate, eta) = rate_eta(self.moved, remaining, started);
                                evs.push(TransferEvent::Progress(TransferProgressInfo {
                                    file_index: self.current as u64,
                                    file_name: q.name.clone(),
                                    file_bytes_done: file_done,
                                    file_bytes_total: q.size,
                                    session_bytes_done: session_done,
                                    session_bytes_total: self.total_bytes,
                                    bytes_per_sec: rate,
                                    eta_secs: eta,
                                }));
                            }
                            return Ok(Some(Frame::Data(msg)));
                        }
                        None => {
                            // Fichier courant entièrement streamé (le `End` a
                            // déjà été émis) : avancer dans la file.
                            let q = &self.queue[self.current];
                            let (idx, name, size) = (self.current as u64, q.name.clone(), q.size);
                            self.done_bytes += size;
                            self.current += 1;
                            self.cur_offset_base = 0;
                            self.cur_sent = 0;
                            self.stage = SenderStage::Offer;
                            evs.push(TransferEvent::FileCompleted {
                                index: idx,
                                name,
                                size,
                            });
                            // Boucle : produire l'`Offer` suivant (ou `Done`).
                        }
                    }
                }
            }
        }
    }

    /// Traite une trame reçue du pair (essentiellement `Resume`).
    fn handle(&mut self, frame: Frame, evs: &mut Vec<TransferEvent>) -> Result<()> {
        match frame {
            Frame::Resume { seq, offset } => {
                if !matches!(self.stage, SenderStage::AwaitResume) {
                    return Err(NdError::Protocol(
                        "Resume inattendu : aucun Offer en attente".into(),
                    ));
                }
                if seq != self.current as u64 {
                    return Err(NdError::Protocol(format!(
                        "Resume pour le fichier {seq}, {} attendu",
                        self.current
                    )));
                }
                let q = &self.queue[self.current];
                let fs = FileSender::with_chunk_size(&q.path, offset, q.chunk_size)?;
                self.cur_offset_base = offset;
                self.cur_sent = 0;
                evs.push(TransferEvent::FileStarted {
                    index: self.current as u64,
                    name: q.name.clone(),
                    size: q.size,
                    resume_offset: offset,
                });
                self.stage = SenderStage::Stream(Box::new(fs));
                Ok(())
            }
            Frame::Offer { .. } | Frame::Data(_) | Frame::Done => Err(NdError::Protocol(
                "trame inattendue côté émetteur (attendu Resume)".into(),
            )),
            // `Cancel` est intercepté au niveau session.
            Frame::Cancel => Ok(()),
        }
    }

    fn session_progress(&self) -> SessionProgress {
        let current_file = self.queue.get(self.current).map(|q| q.name.clone());
        let bytes_done = match self.stage {
            SenderStage::Finished => self.total_bytes,
            _ => self.done_bytes + self.cur_offset_base + self.cur_sent,
        };
        SessionProgress {
            files_total: self.queue.len() as u64,
            files_completed: self.current as u64,
            bytes_done,
            bytes_total: self.total_bytes,
            current_file,
        }
    }
}

/// Réception en cours pour un fichier donné.
struct CurrentRecv {
    index: u64,
    name: String,
    size: u64,
    recv: Box<FileReceiver>,
    /// Offset de reprise (octets déjà présents dans la destination).
    base: u64,
    /// Octets écrits pour ce fichier pendant cette session.
    written: u64,
}

/// État du récepteur : répertoire de destination + fichier courant + compteurs.
struct ReceiverSide {
    dest_dir: PathBuf,
    current: Option<CurrentRecv>,
    /// Trames à émettre vers le pair (réponses `Resume`).
    outbox: VecDeque<Frame>,
    done_bytes: u64,
    total_bytes: u64,
    moved: u64,
    completed: u64,
    /// Index de fichier attribué au prochain `Offer`.
    next_index: u64,
    finished: bool,
}

impl ReceiverSide {
    fn new(dest_dir: PathBuf) -> Self {
        Self {
            dest_dir,
            current: None,
            outbox: VecDeque::new(),
            done_bytes: 0,
            total_bytes: 0,
            moved: 0,
            completed: 0,
            next_index: 0,
            finished: false,
        }
    }

    /// Produit la prochaine réponse en attente (`Resume`), ou `None`.
    fn produce(&mut self) -> Option<Frame> {
        self.outbox.pop_front()
    }

    /// Traite une trame reçue du pair (`Offer`, `Data`, `Done`).
    fn handle(
        &mut self,
        frame: Frame,
        started: Instant,
        evs: &mut Vec<TransferEvent>,
    ) -> Result<()> {
        match frame {
            Frame::Offer {
                seq,
                name,
                size,
                chunk_size,
            } => {
                let nom = assainir_nom(&name)?;
                let dest = self.dest_dir.join(&nom);
                let base = offset_reprise(&dest, size, chunk_size)?;
                let index = self.next_index;
                self.next_index += 1;
                self.total_bytes += size;
                evs.push(TransferEvent::FileStarted {
                    index,
                    name: nom.clone(),
                    size,
                    resume_offset: base,
                });
                self.current = Some(CurrentRecv {
                    index,
                    name: nom,
                    size,
                    recv: Box::new(FileReceiver::new(&dest)),
                    base,
                    written: 0,
                });
                self.outbox.push_back(Frame::Resume { seq, offset: base });
                Ok(())
            }
            Frame::Data(msg) => {
                let chunk_len = if let TransferMsg::Chunk { data, .. } = &msg {
                    data.len() as u64
                } else {
                    0
                };
                let cur = self
                    .current
                    .as_mut()
                    .ok_or_else(|| NdError::Protocol("Data hors fichier courant".into()))?;
                let status = cur.recv.accept(msg)?;
                if chunk_len > 0 {
                    cur.written += chunk_len;
                    self.moved += chunk_len;
                    let file_done = cur.base + cur.written;
                    let session_done = self.done_bytes + file_done;
                    let remaining = self.total_bytes.saturating_sub(session_done);
                    let (rate, eta) = rate_eta(self.moved, remaining, started);
                    evs.push(TransferEvent::Progress(TransferProgressInfo {
                        file_index: cur.index,
                        file_name: cur.name.clone(),
                        file_bytes_done: file_done,
                        file_bytes_total: cur.size,
                        session_bytes_done: session_done,
                        session_bytes_total: self.total_bytes,
                        bytes_per_sec: rate,
                        eta_secs: eta,
                    }));
                }
                if status == TransferStatus::Complete {
                    let (idx, name, size) = (cur.index, cur.name.clone(), cur.size);
                    self.done_bytes += size;
                    self.completed += 1;
                    self.current = None;
                    evs.push(TransferEvent::FileCompleted {
                        index: idx,
                        name,
                        size,
                    });
                }
                Ok(())
            }
            Frame::Done => {
                self.finished = true;
                evs.push(TransferEvent::Finished);
                Ok(())
            }
            Frame::Resume { .. } => Err(NdError::Protocol(
                "trame Resume inattendue côté récepteur".into(),
            )),
            // `Cancel` est intercepté au niveau session.
            Frame::Cancel => Ok(()),
        }
    }

    fn session_progress(&self) -> SessionProgress {
        let (bytes_done, current_file) = match &self.current {
            Some(c) => (self.done_bytes + c.base + c.written, Some(c.name.clone())),
            None => (self.done_bytes, None),
        };
        SessionProgress {
            files_total: self.next_index,
            files_completed: self.completed,
            bytes_done,
            bytes_total: self.total_bytes,
            current_file,
        }
    }
}

/// Rôle d'une session (boîté pour garder l'enum compact).
enum Role {
    Sender(Box<SenderSide>),
    Receiver(Box<ReceiverSide>),
}

/// Machine de transfert orientée canal. Voir la documentation du module pour le
/// protocole et la manière dont nd-core la pilote.
pub struct TransferSession {
    role: Role,
    lifecycle: Lifecycle,
    paused: bool,
    started: Instant,
    events: Vec<TransferEvent>,
    /// Tampon de réassemblage des octets entrants.
    rx: Vec<u8>,
}

impl TransferSession {
    /// Prépare l'émission d'une file de fichiers avec la taille de chunk par
    /// défaut ([`DEFAULT_CHUNK_SIZE`]). Voir [`Self::send_with_chunk_size`].
    pub fn send(files: Vec<PathBuf>) -> Result<Self> {
        Self::send_with_chunk_size(files, DEFAULT_CHUNK_SIZE)
    }

    /// Prépare l'émission d'une file de fichiers avec une taille de chunk
    /// donnée. Chaque chemin doit être un fichier existant au nom UTF-8 ; les
    /// tailles sont relevées à la construction pour la progression globale.
    pub fn send_with_chunk_size(files: Vec<PathBuf>, chunk_size: u32) -> Result<Self> {
        if chunk_size == 0 {
            return Err(NdError::Protocol("taille de chunk nulle".into()));
        }
        let mut queue = Vec::with_capacity(files.len());
        let mut total_bytes = 0u64;
        for path in files {
            let meta = std::fs::metadata(&path)?;
            if !meta.is_file() {
                return Err(NdError::Protocol(format!(
                    "la source n'est pas un fichier : {}",
                    path.display()
                )));
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| {
                    NdError::Protocol(format!(
                        "chemin source sans nom de fichier UTF-8 : {}",
                        path.display()
                    ))
                })?
                .to_string();
            total_bytes += meta.len();
            queue.push(Queued {
                path,
                name,
                size: meta.len(),
                chunk_size,
            });
        }
        Ok(Self::with_role(Role::Sender(Box::new(SenderSide::new(
            queue,
            total_bytes,
        )))))
    }

    /// Prépare la réception dans `dest_dir` (créé au besoin par les écritures).
    /// Les noms reçus sont réduits à leur dernier composant (anti-traversée).
    pub fn receive(dest_dir: impl Into<PathBuf>) -> Self {
        Self::with_role(Role::Receiver(Box::new(ReceiverSide::new(dest_dir.into()))))
    }

    fn with_role(role: Role) -> Self {
        Self {
            role,
            lifecycle: Lifecycle::Active,
            paused: false,
            started: Instant::now(),
            events: Vec::new(),
            rx: Vec::new(),
        }
    }

    /// Produit la prochaine trame d'octets à envoyer sur le canal, ou `None`
    /// s'il n'y a rien à émettre pour l'instant (attente du pair, pause ou
    /// session terminée). nd-core appelle en boucle jusqu'à `None`.
    pub fn poll_outgoing(&mut self) -> Result<Option<Vec<u8>>> {
        match self.lifecycle {
            Lifecycle::CancelPending => {
                self.lifecycle = Lifecycle::Cancelled;
                self.events.push(TransferEvent::Cancelled);
                return Ok(Some(Frame::Cancel.encode()));
            }
            Lifecycle::Cancelled => return Ok(None),
            Lifecycle::Active => {}
        }
        if self.paused {
            return Ok(None);
        }
        let started = self.started;
        let mut evs = Vec::new();
        let frame = match &mut self.role {
            Role::Sender(s) => s.produce(started, &mut evs)?,
            Role::Receiver(r) => r.produce(),
        };
        self.events.append(&mut evs);
        Ok(frame.map(|f| f.encode()))
    }

    /// Réinjecte des octets reçus du canal. La tranche peut contenir zéro, une
    /// ou plusieurs trames (ou une trame partielle, bufferisée jusqu'à la
    /// suite). [`NdError::Protocol`] si une trame complète est malformée ou hors
    /// séquence.
    pub fn handle_incoming(&mut self, bytes: &[u8]) -> Result<()> {
        if !matches!(self.lifecycle, Lifecycle::Active) {
            return Ok(());
        }
        self.rx.extend_from_slice(bytes);
        let started = self.started;
        loop {
            let Some((frame, consomme)) = Frame::decode(&self.rx)? else {
                break;
            };
            self.rx.drain(..consomme);
            if matches!(frame, Frame::Cancel) {
                self.lifecycle = Lifecycle::Cancelled;
                self.events.push(TransferEvent::Cancelled);
                self.rx.clear();
                break;
            }
            let mut evs = Vec::new();
            match &mut self.role {
                Role::Sender(s) => s.handle(frame, &mut evs)?,
                Role::Receiver(r) => r.handle(frame, started, &mut evs)?,
            }
            self.events.append(&mut evs);
        }
        Ok(())
    }

    /// Met la session en pause : `poll_outgoing` ne produira plus de trame (hors
    /// `Cancel`) jusqu'à [`Self::resume`]. Les trames déjà en vol côté canal ne
    /// sont pas rappelées.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Reprend une session mise en pause.
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Indique si la session est en pause.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Demande l'annulation : la prochaine `poll_outgoing` émettra une trame
    /// `Cancel` (même en pause) et la session passera à l'état annulé.
    pub fn cancel(&mut self) {
        if matches!(self.lifecycle, Lifecycle::Active) {
            self.lifecycle = Lifecycle::CancelPending;
        }
    }

    /// Indique si la session est terminée (file transférée, `Done` traité, ou
    /// annulation effective).
    pub fn is_finished(&self) -> bool {
        if matches!(self.lifecycle, Lifecycle::Cancelled) {
            return true;
        }
        match &self.role {
            Role::Sender(s) => matches!(s.stage, SenderStage::Finished),
            Role::Receiver(r) => r.finished,
        }
    }

    /// Indique si la session a été annulée.
    pub fn is_cancelled(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Cancelled)
    }

    /// Instantané de progression global (sans drainer les évènements).
    pub fn progress(&self) -> SessionProgress {
        match &self.role {
            Role::Sender(s) => s.session_progress(),
            Role::Receiver(r) => r.session_progress(),
        }
    }

    /// Retire et renvoie les évènements accumulés depuis le dernier appel.
    pub fn take_events(&mut self) -> Vec<TransferEvent> {
        std::mem::take(&mut self.events)
    }

    /// Évènements accumulés, sans les retirer.
    pub fn events(&self) -> &[TransferEvent] {
        &self.events
    }
}

/// Réduit un nom reçu à son dernier composant de chemin (protection basique
/// anti-traversée) et refuse les noms vides ou `.`/`..`.
fn assainir_nom(name: &str) -> Result<String> {
    let comp = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| NdError::Protocol(format!("nom de fichier reçu invalide : {name}")))?;
    if comp.is_empty() || comp == "." || comp == ".." {
        return Err(NdError::Protocol(format!(
            "nom de fichier reçu invalide : {name}"
        )));
    }
    Ok(comp.to_string())
}

/// Offset de reprise pour une destination : longueur existante arrondie à la
/// frontière de chunk inférieure, ou 0 si la destination est absente ou plus
/// longue que la taille annoncée (fichier différent → on repart de zéro). Cohérent
/// avec la logique interne de [`FileReceiver`].
fn offset_reprise(dest: &Path, size: u64, chunk_size: u32) -> Result<u64> {
    if chunk_size == 0 {
        return Err(NdError::Protocol("taille de chunk nulle".into()));
    }
    let existant = match std::fs::metadata(dest) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    if existant > size {
        Ok(0)
    } else {
        Ok(existant - existant % u64::from(chunk_size))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_hash;

    /// Répertoire temporaire unique pour un test (isolé entre exécutions).
    fn dir_temp(nom: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nd_files_session_{}_{nom}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Motif déterministe non trivial.
    fn motif(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
            .collect()
    }

    /// Fait circuler les octets entre deux sessions jusqu'à quiescence.
    fn pump(a: &mut TransferSession, b: &mut TransferSession) -> Result<()> {
        loop {
            let mut avance = false;
            while let Some(bytes) = a.poll_outgoing()? {
                b.handle_incoming(&bytes)?;
                avance = true;
            }
            while let Some(bytes) = b.poll_outgoing()? {
                a.handle_incoming(&bytes)?;
                avance = true;
            }
            if !avance {
                break;
            }
        }
        Ok(())
    }

    #[test]
    fn frame_aller_retour_et_flux() {
        let frames = [
            Frame::Offer {
                seq: 3,
                name: "é—à.bin".to_string(),
                size: 9_000,
                chunk_size: 4096,
            },
            Frame::Resume {
                seq: 3,
                offset: 8192,
            },
            Frame::Data(TransferMsg::Chunk {
                index: 7,
                data: motif(500),
            }),
            Frame::Cancel,
            Frame::Done,
        ];
        // Aller-retour individuel, consommation exacte.
        for f in &frames {
            let octets = f.encode();
            let (relu, n) = Frame::decode(&octets).unwrap().unwrap();
            assert_eq!(&relu, f);
            assert_eq!(n, octets.len());
        }
        // Flux concaténé : décodage trame par trame.
        let mut flux = Vec::new();
        for f in &frames {
            flux.extend_from_slice(&f.encode());
        }
        let mut pos = 0;
        for attendu in &frames {
            let (relu, n) = Frame::decode(&flux[pos..]).unwrap().unwrap();
            assert_eq!(&relu, attendu);
            pos += n;
        }
        assert_eq!(pos, flux.len());
        // Trame partielle : Ok(None) tant que le corps est incomplet.
        let octets = frames[0].encode();
        assert!(Frame::decode(&octets[..3]).unwrap().is_none());
        assert!(Frame::decode(&octets[..octets.len() - 1])
            .unwrap()
            .is_none());
        // Tag inconnu sur une trame complète : erreur.
        assert!(Frame::decode(&[1, 0, 0, 0, 99]).is_err());
    }

    #[test]
    fn transfert_multi_fichiers_en_memoire() {
        let src = dir_temp("multi_src");
        let dst = dir_temp("multi_dst");
        let contenus = [
            ("a.bin", motif(10_000)),
            ("b.bin", motif(1)),
            ("c.bin", motif(70_000)),
        ];
        let mut chemins = Vec::new();
        let mut total = 0u64;
        for (nom, data) in &contenus {
            let p = src.join(nom);
            std::fs::write(&p, data).unwrap();
            total += data.len() as u64;
            chemins.push(p);
        }

        let mut emetteur = TransferSession::send_with_chunk_size(chemins, 4096).unwrap();
        let mut recepteur = TransferSession::receive(dst.clone());
        pump(&mut emetteur, &mut recepteur).unwrap();

        assert!(emetteur.is_finished());
        assert!(recepteur.is_finished());
        // Contenus reconstruits à l'identique.
        for (nom, data) in &contenus {
            assert_eq!(&std::fs::read(dst.join(nom)).unwrap(), data);
        }
        // Progression finale : tout est là.
        let p = recepteur.progress();
        assert_eq!(p.bytes_done, total);
        assert_eq!(p.bytes_total, total);
        assert_eq!(p.files_completed, 3);
        // Évènements côté récepteur : 3 débuts, 3 fins, 1 clôture.
        let evs = recepteur.take_events();
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, TransferEvent::FileStarted { .. }))
                .count(),
            3
        );
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, TransferEvent::FileCompleted { .. }))
                .count(),
            3
        );
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, TransferEvent::Finished))
                .count(),
            1
        );

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn gros_fichier_progression_croissante() {
        let src = dir_temp("gros_src");
        let dst = dir_temp("gros_dst");
        let contenu = motif(1_000_000); // ~1 Mio
        let p = src.join("gros.bin");
        std::fs::write(&p, &contenu).unwrap();

        let mut emetteur = TransferSession::send_with_chunk_size(vec![p], 16 * 1024).unwrap();
        let mut recepteur = TransferSession::receive(dst.clone());
        pump(&mut emetteur, &mut recepteur).unwrap();

        let relu = std::fs::read(dst.join("gros.bin")).unwrap();
        assert_eq!(chunk_hash(&relu), chunk_hash(&contenu));

        // La progression du récepteur est monotone et atteint la taille totale.
        let evs = recepteur.take_events();
        let mut precedent = 0u64;
        let mut vu_progres = false;
        for e in &evs {
            if let TransferEvent::Progress(info) = e {
                assert!(info.session_bytes_done >= precedent);
                precedent = info.session_bytes_done;
                assert_eq!(info.session_bytes_total, contenu.len() as u64);
                assert!(info.bytes_per_sec >= 0.0);
                vu_progres = true;
            }
        }
        assert!(vu_progres);
        assert_eq!(precedent, contenu.len() as u64);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn reprise_apres_coupure_simulee() {
        const CHUNK: u32 = 1024;
        let src = dir_temp("reprise_src");
        let dst = dir_temp("reprise_dst");
        let contenu = motif(40 * CHUNK as usize + 321);
        let p = src.join("data.bin");
        std::fs::write(&p, &contenu).unwrap();

        // --- Première session : on ne délivre qu'une partie des trames, puis on
        //     « coupe » en laissant tomber les deux sessions.
        {
            let mut emetteur =
                TransferSession::send_with_chunk_size(vec![p.clone()], CHUNK).unwrap();
            let mut recepteur = TransferSession::receive(dst.clone());
            // Poignée de main Offer/Resume.
            let offer = emetteur.poll_outgoing().unwrap().unwrap();
            recepteur.handle_incoming(&offer).unwrap();
            let resume = recepteur.poll_outgoing().unwrap().unwrap();
            emetteur.handle_incoming(&resume).unwrap();
            // Délivre seulement 6 trames de données (Start + quelques chunks).
            for _ in 0..6 {
                if let Some(bytes) = emetteur.poll_outgoing().unwrap() {
                    recepteur.handle_incoming(&bytes).unwrap();
                }
            }
            // Coupure : quelque chose a bien été écrit, mais pas tout.
            let ecrit = std::fs::metadata(dst.join("data.bin")).unwrap().len();
            assert!(ecrit > 0 && ecrit < contenu.len() as u64);
        }

        // --- Seconde session, mêmes fichier et destination : la reprise repart
        //     de l'offset déduit des octets déjà présents.
        let mut emetteur2 = TransferSession::send_with_chunk_size(vec![p], CHUNK).unwrap();
        let mut recepteur2 = TransferSession::receive(dst.clone());
        pump(&mut emetteur2, &mut recepteur2).unwrap();

        // Le récepteur a bien annoncé une reprise non nulle.
        let evs = recepteur2.take_events();
        let reprise = evs.iter().find_map(|e| match e {
            TransferEvent::FileStarted { resume_offset, .. } => Some(*resume_offset),
            _ => None,
        });
        assert_eq!(reprise, Some(reprise.unwrap()));
        assert!(reprise.unwrap() > 0);

        // Fichier final identique à la source.
        assert_eq!(std::fs::read(dst.join("data.bin")).unwrap(), contenu);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn annulation_propagee() {
        let src = dir_temp("annul_src");
        let dst = dir_temp("annul_dst");
        let contenu = motif(100_000);
        let p = src.join("f.bin");
        std::fs::write(&p, &contenu).unwrap();

        let mut emetteur = TransferSession::send_with_chunk_size(vec![p], 4096).unwrap();
        let mut recepteur = TransferSession::receive(dst.clone());
        // Poignée de main + un peu de données.
        let offer = emetteur.poll_outgoing().unwrap().unwrap();
        recepteur.handle_incoming(&offer).unwrap();
        let resume = recepteur.poll_outgoing().unwrap().unwrap();
        emetteur.handle_incoming(&resume).unwrap();
        let bytes = emetteur.poll_outgoing().unwrap().unwrap();
        recepteur.handle_incoming(&bytes).unwrap();

        // Annulation côté émetteur : la trame Cancel est émise même sans plus rien d'autre.
        emetteur.cancel();
        let cancel = emetteur.poll_outgoing().unwrap().unwrap();
        assert!(emetteur.is_cancelled());
        recepteur.handle_incoming(&cancel).unwrap();
        assert!(recepteur.is_cancelled());
        assert!(recepteur.is_finished());
        // Plus aucune trame de part et d'autre.
        assert!(emetteur.poll_outgoing().unwrap().is_none());
        assert!(recepteur.poll_outgoing().unwrap().is_none());
        assert!(recepteur
            .take_events()
            .iter()
            .any(|e| matches!(e, TransferEvent::Cancelled)));

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn pause_suspend_puis_reprend() {
        let src = dir_temp("pause_src");
        let dst = dir_temp("pause_dst");
        let contenu = motif(50_000);
        let p = src.join("f.bin");
        std::fs::write(&p, &contenu).unwrap();

        let mut emetteur = TransferSession::send_with_chunk_size(vec![p], 4096).unwrap();
        let mut recepteur = TransferSession::receive(dst.clone());

        // En pause dès le départ : l'émetteur ne produit rien.
        emetteur.pause();
        assert!(emetteur.is_paused());
        assert!(emetteur.poll_outgoing().unwrap().is_none());

        // Reprise : le transfert se déroule normalement jusqu'au bout.
        emetteur.resume();
        assert!(!emetteur.is_paused());
        pump(&mut emetteur, &mut recepteur).unwrap();
        assert!(recepteur.is_finished());
        assert_eq!(std::fs::read(dst.join("f.bin")).unwrap(), contenu);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn nom_avec_traversee_est_assaini() {
        assert_eq!(assainir_nom("simple.bin").unwrap(), "simple.bin");
        assert_eq!(assainir_nom("../../evasion.bin").unwrap(), "evasion.bin");
        assert_eq!(assainir_nom("a/b/c.bin").unwrap(), "c.bin");
        assert!(assainir_nom("..").is_err());
        assert!(assainir_nom("").is_err());
    }
}
