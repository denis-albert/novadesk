//! Enregistrement de session : sérialisation d'une suite d'images encodées
//! (opaques pour ce module) vers un flux `Write`, et relecture depuis `Read`.
//!
//! Deux formats coexistent (entiers petit-boutistes dans les deux cas) :
//!
//! **v1 (`NDREC1`)** — flux séquentiel minimal :
//! - en-tête : magic `NDREC1` (6 octets) puis version `u16` ;
//! - puis, pour chaque image : `[u64 timestamp_us][u8 keyframe][u32 len][data]`.
//!
//! **v2 (`NDREC2`)** — conteneur indexé produit par [`IndexedRecorder`] :
//! - en-tête : magic `NDREC2` (6 octets) puis version `u16` ;
//! - métadonnées : `[u32 largeur][u32 hauteur][u32 fps][u64 date_debut_unix_ms]`
//!   `[u16 len_codec][codec UTF-8]` ;
//! - sections étiquetées :
//!   - image (`1`) : `[u64 timestamp_us][u8 keyframe][u32 len][data]` ;
//!   - index (`2`) : `[u32 n]` puis `n × [u64 timestamp_us][u64 offset]` — la
//!     table des images-clés, `offset` étant la position absolue de
//!     l'étiquette de la section image correspondante ;
//!   - fin (`3`) : `[u64 duree_us][u64 nb_images][u8 hachage_present]`
//!     `[32 octets BLAKE3 si présent]` — le hachage couvre **tous** les octets
//!     du fichier qui le précèdent ;
//! - pied (16 octets) : `[u64 offset_index][b"NDRECEND"]`, qui permet à un
//!   lecteur `Read + Seek` de retrouver l'index sans balayer les images.
//!
//! Le contenu des images est opaque : l'enregistreur ne décode rien, il
//! archive fidèlement ce que le codec lui donne (voir plan 13, §enregistrement).

use std::io::{Read, Seek, SeekFrom, Write};

use nd_proto::{NdError, Result};

/// Magic en tête d'un enregistrement NovaDesk v1 (séquentiel).
pub const MAGIC: &[u8; 6] = b"NDREC1";

/// Version du format séquentiel historique.
pub const VERSION: u16 = 1;

/// Magic en tête d'un enregistrement NovaDesk v2 (conteneur indexé).
pub const MAGIC_V2: &[u8; 6] = b"NDREC2";

/// Version du format indexé.
pub const VERSION_INDEXEE: u16 = 2;

// Étiquettes de section du format v2.
const SECTION_IMAGE: u8 = 1;
const SECTION_INDEX: u8 = 2;
const SECTION_FIN: u8 = 3;

/// Magic de fin de fichier v2 (dernier champ du pied).
const FIN_MAGIC: &[u8; 8] = b"NDRECEND";

/// Taille du pied v2 : `u64 offset_index` + magic de fin.
const TAILLE_PIED: u64 = 16;

/// Une image relue depuis un enregistrement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFrame {
    /// Horodatage de capture, en microsecondes depuis le début de la session.
    pub timestamp_us: u64,
    /// Vrai si l'image est une image clef (décodable sans les précédentes).
    pub keyframe: bool,
    /// Données encodées, opaques pour l'enregistreur.
    pub data: Vec<u8>,
}

/// Métadonnées d'un enregistrement v2 : ce qu'il faut pour rejouer sans
/// deviner (dimensions, codec, cadence) et pour dater la session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingMetadata {
    /// Largeur des images, en pixels.
    pub width: u32,
    /// Hauteur des images, en pixels.
    pub height: u32,
    /// Cadence nominale, en images par seconde.
    pub fps: u32,
    /// Nom du codec des images (`"nova-h264"`, `"vp9"`, …), opaque ici.
    pub codec: String,
    /// Date de début de session, en millisecondes Unix.
    pub start_unix_ms: u64,
}

impl RecordingMetadata {
    /// Sérialise les métadonnées (format décrit en tête de module).
    fn encode(&self) -> Result<Vec<u8>> {
        let nom = self.codec.as_bytes();
        let longueur = u16::try_from(nom.len())
            .map_err(|_| NdError::Protocol("nom de codec trop long (> 65 535 octets)".into()))?;
        let mut sortie = Vec::with_capacity(22 + nom.len());
        sortie.extend_from_slice(&self.width.to_le_bytes());
        sortie.extend_from_slice(&self.height.to_le_bytes());
        sortie.extend_from_slice(&self.fps.to_le_bytes());
        sortie.extend_from_slice(&self.start_unix_ms.to_le_bytes());
        sortie.extend_from_slice(&longueur.to_le_bytes());
        sortie.extend_from_slice(nom);
        Ok(sortie)
    }
}

/// Une entrée de la table des images-clés d'un enregistrement v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeEntry {
    /// Horodatage de l'image-clé, en microsecondes depuis le début.
    pub timestamp_us: u64,
    /// Position absolue (octets depuis le début du fichier) de l'étiquette de
    /// la section image correspondante.
    pub offset: u64,
}

/// Rapport rendu par [`SessionReader::validate`] après vérification complète.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationReport {
    /// Nombre d'images réellement présentes dans le conteneur.
    pub frames: u64,
    /// Nombre d'images-clés (taille de l'index).
    pub keyframes: u64,
    /// Durée de l'enregistrement (horodatage de la dernière image).
    pub duration_us: u64,
    /// Vrai si un hachage BLAKE3 était présent et a été vérifié.
    pub hash_verified: bool,
}

// ---------------------------------------------------------------------------
// Écriture v1 (inchangée)
// ---------------------------------------------------------------------------

/// Écrit un enregistrement de session v1 dans un flux quelconque
/// (`Vec<u8>` en mémoire, fichier, socket…). Format séquentiel sans index :
/// préférer [`IndexedRecorder`] quand la recherche ou l'intégrité importent.
#[derive(Debug)]
pub struct SessionRecorder<W: Write> {
    sortie: W,
    images: u64,
}

impl<W: Write> SessionRecorder<W> {
    /// Ouvre l'enregistreur et écrit immédiatement l'en-tête (magic + version).
    pub fn new(mut sortie: W) -> Result<Self> {
        sortie.write_all(MAGIC)?;
        sortie.write_all(&VERSION.to_le_bytes())?;
        Ok(SessionRecorder { sortie, images: 0 })
    }

    /// Ajoute une image encodée à l'enregistrement.
    pub fn record(&mut self, timestamp_us: u64, keyframe: bool, data: &[u8]) -> Result<()> {
        let longueur = u32::try_from(data.len()).map_err(|_| {
            NdError::Protocol("image trop grande pour le format (longueur > u32)".into())
        })?;
        self.sortie.write_all(&timestamp_us.to_le_bytes())?;
        self.sortie.write_all(&[u8::from(keyframe)])?;
        self.sortie.write_all(&longueur.to_le_bytes())?;
        self.sortie.write_all(data)?;
        self.images += 1;
        Ok(())
    }

    /// Nombre d'images écrites depuis l'ouverture.
    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.images
    }

    /// Vide les tampons du flux sous-jacent.
    pub fn flush(&mut self) -> Result<()> {
        self.sortie.flush()?;
        Ok(())
    }

    /// Termine l'enregistrement (flush) et rend le flux sous-jacent.
    pub fn finish(mut self) -> Result<W> {
        self.sortie.flush()?;
        Ok(self.sortie)
    }
}

// ---------------------------------------------------------------------------
// Écriture v2 (conteneur indexé)
// ---------------------------------------------------------------------------

/// Sortie comptée : suit la position absolue d'écriture (pour l'index) et
/// alimente, à la demande, le hachage BLAKE3 d'intégrité.
struct SortieComptee<W: Write> {
    interne: W,
    ecrits: u64,
    hachoir: Option<blake3::Hasher>,
}

impl<W: Write> SortieComptee<W> {
    fn new(interne: W, hacher: bool) -> Self {
        SortieComptee {
            interne,
            ecrits: 0,
            hachoir: hacher.then(blake3::Hasher::new),
        }
    }

    /// Écrit tout, met à jour la position et le hachage en cours.
    fn ecrire(&mut self, octets: &[u8]) -> Result<()> {
        self.interne.write_all(octets)?;
        self.ecrits += octets.len() as u64;
        if let Some(hachoir) = &mut self.hachoir {
            hachoir.update(octets);
        }
        Ok(())
    }

    /// Position absolue courante (octets écrits depuis l'ouverture).
    fn position(&self) -> u64 {
        self.ecrits
    }

    /// Fige le hachage : les octets écrits ensuite ne sont plus couverts.
    fn finaliser_hachage(&mut self) -> Option<blake3::Hash> {
        self.hachoir.take().map(|hachoir| hachoir.finalize())
    }
}

impl<W: Write + std::fmt::Debug> std::fmt::Debug for SortieComptee<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortieComptee")
            .field("interne", &self.interne)
            .field("ecrits", &self.ecrits)
            .field("hachage", &self.hachoir.is_some())
            .finish()
    }
}

/// Écrit un enregistrement v2 : métadonnées en tête, table des images-clés en
/// queue (pour [`SessionReader::seek_to_keyframe`]) et, en option, un hachage
/// BLAKE3 d'intégrité couvrant tout le conteneur.
///
/// Les horodatages doivent être croissants au sens large ; l'index est
/// construit au fil de l'eau et écrit par [`IndexedRecorder::finish`].
#[derive(Debug)]
pub struct IndexedRecorder<W: Write> {
    sortie: SortieComptee<W>,
    metadata: RecordingMetadata,
    index: Vec<KeyframeEntry>,
    images: u64,
    dernier_ts: Option<u64>,
}

impl<W: Write> IndexedRecorder<W> {
    /// Ouvre l'enregistreur : écrit l'en-tête et les métadonnées. Si
    /// `avec_hachage` est vrai, un hachage BLAKE3 de tout le conteneur sera
    /// écrit dans la section de fin (vérifiable par [`SessionReader::validate`]).
    pub fn new(sortie: W, metadata: RecordingMetadata, avec_hachage: bool) -> Result<Self> {
        let mut sortie = SortieComptee::new(sortie, avec_hachage);
        sortie.ecrire(MAGIC_V2)?;
        sortie.ecrire(&VERSION_INDEXEE.to_le_bytes())?;
        sortie.ecrire(&metadata.encode()?)?;
        Ok(IndexedRecorder {
            sortie,
            metadata,
            index: Vec::new(),
            images: 0,
            dernier_ts: None,
        })
    }

    /// Ajoute une image encodée. Les images-clés alimentent l'index.
    ///
    /// Refuse un horodatage strictement décroissant : la recherche par
    /// timestamp suppose un flux ordonné.
    pub fn record(&mut self, timestamp_us: u64, keyframe: bool, data: &[u8]) -> Result<()> {
        if self
            .dernier_ts
            .is_some_and(|dernier| timestamp_us < dernier)
        {
            return Err(NdError::Protocol(format!(
                "horodatage décroissant : {timestamp_us} µs après {} µs",
                self.dernier_ts.unwrap_or(0)
            )));
        }
        let longueur = u32::try_from(data.len()).map_err(|_| {
            NdError::Protocol("image trop grande pour le format (longueur > u32)".into())
        })?;
        if keyframe {
            self.index.push(KeyframeEntry {
                timestamp_us,
                offset: self.sortie.position(),
            });
        }
        self.sortie.ecrire(&[SECTION_IMAGE])?;
        self.sortie.ecrire(&timestamp_us.to_le_bytes())?;
        self.sortie.ecrire(&[u8::from(keyframe)])?;
        self.sortie.ecrire(&longueur.to_le_bytes())?;
        self.sortie.ecrire(data)?;
        self.images += 1;
        self.dernier_ts = Some(timestamp_us);
        Ok(())
    }

    /// Nombre d'images écrites depuis l'ouverture.
    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.images
    }

    /// Métadonnées annoncées à l'ouverture.
    #[must_use]
    pub fn metadata(&self) -> &RecordingMetadata {
        &self.metadata
    }

    /// Table des images-clés accumulée jusqu'ici.
    #[must_use]
    pub fn index(&self) -> &[KeyframeEntry] {
        &self.index
    }

    /// Vide les tampons du flux sous-jacent.
    pub fn flush(&mut self) -> Result<()> {
        self.sortie.interne.flush()?;
        Ok(())
    }

    /// Clôt le conteneur : écrit l'index, la section de fin (durée, nombre
    /// d'images, hachage éventuel) puis le pied, et rend le flux sous-jacent.
    pub fn finish(mut self) -> Result<W> {
        let offset_index = self.sortie.position();
        self.sortie.ecrire(&[SECTION_INDEX])?;
        let nombre = u32::try_from(self.index.len())
            .map_err(|_| NdError::Protocol("trop d'images-clés pour le format (> u32)".into()))?;
        self.sortie.ecrire(&nombre.to_le_bytes())?;
        for entree in &self.index {
            self.sortie.ecrire(&entree.timestamp_us.to_le_bytes())?;
            self.sortie.ecrire(&entree.offset.to_le_bytes())?;
        }

        self.sortie.ecrire(&[SECTION_FIN])?;
        let duree = self.dernier_ts.unwrap_or(0);
        self.sortie.ecrire(&duree.to_le_bytes())?;
        self.sortie.ecrire(&self.images.to_le_bytes())?;
        // Le hachage couvre tous les octets écrits jusqu'ici, drapeau compris.
        let hachage = {
            let present = u8::from(self.sortie.hachoir.is_some());
            self.sortie.ecrire(&[present])?;
            self.sortie.finaliser_hachage()
        };
        if let Some(hachage) = hachage {
            self.sortie.ecrire(hachage.as_bytes())?;
        }

        self.sortie.ecrire(&offset_index.to_le_bytes())?;
        self.sortie.ecrire(FIN_MAGIC)?;
        self.sortie.interne.flush()?;
        Ok(self.sortie.interne)
    }
}

// ---------------------------------------------------------------------------
// Lecture (v1 et v2)
// ---------------------------------------------------------------------------

/// Relit un enregistrement produit par [`SessionRecorder`] (v1) ou
/// [`IndexedRecorder`] (v2).
///
/// Utilisable via [`SessionReader::next_frame`] ou comme itérateur
/// d'`Item = Result<RecordedFrame>`. Sur une source `Read + Seek` et un
/// conteneur v2, offre en plus la recherche ([`SessionReader::seek_to_keyframe`])
/// et la validation complète ([`SessionReader::validate`]).
#[derive(Debug)]
pub struct SessionReader<R: Read> {
    source: R,
    version: u16,
    /// Position absolue de lecture (octets consommés depuis le début).
    position: u64,
    /// Position de la première section image (v2 : juste après les métadonnées).
    offset_premiere_image: u64,
    metadata: Option<RecordingMetadata>,
    index: Option<Vec<KeyframeEntry>>,
    offset_index: Option<u64>,
    duree_us: Option<u64>,
    images_declarees: Option<u64>,
    hachage: Option<[u8; 32]>,
    /// Position des 32 octets de hachage — fin de la zone qu'il couvre.
    position_hachage: Option<u64>,
    fin_atteinte: bool,
}

impl<R: Read> SessionReader<R> {
    /// Ouvre l'enregistrement : lit et valide l'en-tête (magic + version),
    /// puis, pour un conteneur v2, les métadonnées.
    pub fn new(mut source: R) -> Result<Self> {
        let mut magic = [0u8; 6];
        source.read_exact(&mut magic)?;
        let attendu = match &magic {
            m if m == MAGIC => VERSION,
            m if m == MAGIC_V2 => VERSION_INDEXEE,
            _ => {
                return Err(NdError::Protocol(
                    "magic NDREC1/NDREC2 absent : ce flux n'est pas un enregistrement NovaDesk"
                        .into(),
                ))
            }
        };
        let mut version = [0u8; 2];
        source.read_exact(&mut version)?;
        let version = u16::from_le_bytes(version);
        if version != attendu {
            return Err(NdError::Protocol(format!(
                "version d'enregistrement {version} non gérée (attendu {attendu})"
            )));
        }
        let mut lecteur = SessionReader {
            source,
            version,
            position: 8,
            offset_premiere_image: 8,
            metadata: None,
            index: None,
            offset_index: None,
            duree_us: None,
            images_declarees: None,
            hachage: None,
            position_hachage: None,
            fin_atteinte: false,
        };
        if version == VERSION_INDEXEE {
            let metadata = lecteur.lire_metadata()?;
            lecteur.metadata = Some(metadata);
            lecteur.offset_premiere_image = lecteur.position;
        }
        Ok(lecteur)
    }

    /// Version du format lue dans l'en-tête.
    #[must_use]
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Métadonnées de l'enregistrement (`None` pour un flux v1).
    #[must_use]
    pub fn metadata(&self) -> Option<&RecordingMetadata> {
        self.metadata.as_ref()
    }

    /// Table des images-clés, disponible après [`SessionReader::load_index`]
    /// ou après avoir lu le flux v2 jusqu'au bout.
    #[must_use]
    pub fn index(&self) -> Option<&[KeyframeEntry]> {
        self.index.as_deref()
    }

    /// Durée annoncée par la section de fin (horodatage de la dernière image),
    /// disponible dans les mêmes conditions que l'index.
    #[must_use]
    pub fn duration_us(&self) -> Option<u64> {
        self.duree_us
    }

    /// Nombre d'images annoncé par la section de fin.
    #[must_use]
    pub fn declared_frames(&self) -> Option<u64> {
        self.images_declarees
    }

    /// Hachage BLAKE3 annoncé par la section de fin, s'il y en a un.
    #[must_use]
    pub fn declared_hash(&self) -> Option<[u8; 32]> {
        self.hachage
    }

    /// Image suivante, ou `Ok(None)` en fin d'enregistrement (fin propre).
    ///
    /// Une fin de flux au milieu d'un enregistrement est signalée comme une
    /// erreur (enregistrement tronqué). Pour un conteneur v2, la lecture des
    /// sections finales renseigne au passage l'index, la durée et le hachage.
    pub fn next_frame(&mut self) -> Result<Option<RecordedFrame>> {
        if self.version == VERSION {
            self.next_frame_v1()
        } else {
            self.next_frame_v2()
        }
    }

    fn next_frame_v1(&mut self) -> Result<Option<RecordedFrame>> {
        let mut horodatage = [0u8; 8];
        if !lire_ou_fin(&mut self.source, &mut horodatage)? {
            return Ok(None);
        }
        self.position += 8;
        self.lire_corps_image(u64::from_le_bytes(horodatage))
            .map(Some)
    }

    fn next_frame_v2(&mut self) -> Result<Option<RecordedFrame>> {
        if self.fin_atteinte {
            return Ok(None);
        }
        let mut etiquette = [0u8; 1];
        if !lire_ou_fin(&mut self.source, &mut etiquette)? {
            return Err(NdError::Protocol(
                "enregistrement v2 tronqué : sections d'index et de fin absentes".into(),
            ));
        }
        self.position += 1;
        match etiquette[0] {
            SECTION_IMAGE => {
                let horodatage = self.lire_u64()?;
                self.lire_corps_image(horodatage).map(Some)
            }
            SECTION_INDEX => {
                self.offset_index = Some(self.position - 1);
                let entrees = self.lire_corps_index()?;
                self.index = Some(entrees);
                self.lire_fin()?;
                self.lire_pied()?;
                self.fin_atteinte = true;
                Ok(None)
            }
            autre => Err(NdError::Protocol(format!(
                "étiquette de section inconnue : {autre}"
            ))),
        }
    }

    // --- primitives de lecture comptée -----------------------------------

    fn lire_exact(&mut self, tampon: &mut [u8]) -> Result<()> {
        self.source.read_exact(tampon)?;
        self.position += tampon.len() as u64;
        Ok(())
    }

    fn lire_u8(&mut self) -> Result<u8> {
        let mut octets = [0u8; 1];
        self.lire_exact(&mut octets)?;
        Ok(octets[0])
    }

    fn lire_u16(&mut self) -> Result<u16> {
        let mut octets = [0u8; 2];
        self.lire_exact(&mut octets)?;
        Ok(u16::from_le_bytes(octets))
    }

    fn lire_u32(&mut self) -> Result<u32> {
        let mut octets = [0u8; 4];
        self.lire_exact(&mut octets)?;
        Ok(u32::from_le_bytes(octets))
    }

    fn lire_u64(&mut self) -> Result<u64> {
        let mut octets = [0u8; 8];
        self.lire_exact(&mut octets)?;
        Ok(u64::from_le_bytes(octets))
    }

    // --- morceaux du format ------------------------------------------------

    /// Corps d'une image (drapeau, longueur, données), l'horodatage étant lu.
    fn lire_corps_image(&mut self, timestamp_us: u64) -> Result<RecordedFrame> {
        let keyframe = match self.lire_u8()? {
            0 => false,
            1 => true,
            autre => {
                return Err(NdError::Protocol(format!(
                    "drapeau keyframe invalide : {autre}"
                )))
            }
        };
        let longueur = self.lire_u32()? as usize;
        let mut data = vec![0u8; longueur];
        self.lire_exact(&mut data)?;
        Ok(RecordedFrame {
            timestamp_us,
            keyframe,
            data,
        })
    }

    fn lire_metadata(&mut self) -> Result<RecordingMetadata> {
        let width = self.lire_u32()?;
        let height = self.lire_u32()?;
        let fps = self.lire_u32()?;
        let start_unix_ms = self.lire_u64()?;
        let longueur = self.lire_u16()? as usize;
        let mut nom = vec![0u8; longueur];
        self.lire_exact(&mut nom)?;
        let codec = String::from_utf8(nom)
            .map_err(|_| NdError::Protocol("nom de codec non UTF-8".into()))?;
        Ok(RecordingMetadata {
            width,
            height,
            fps,
            codec,
            start_unix_ms,
        })
    }

    /// Corps de la section index (l'étiquette a déjà été consommée).
    fn lire_corps_index(&mut self) -> Result<Vec<KeyframeEntry>> {
        let nombre = self.lire_u32()? as usize;
        let mut entrees = Vec::new();
        let mut precedente: Option<KeyframeEntry> = None;
        for _ in 0..nombre {
            let entree = KeyframeEntry {
                timestamp_us: self.lire_u64()?,
                offset: self.lire_u64()?,
            };
            if entree.offset < self.offset_premiere_image
                || self.offset_index.is_some_and(|oi| entree.offset >= oi)
            {
                return Err(NdError::Protocol(
                    "entrée d'index hors de la zone des images".into(),
                ));
            }
            if let Some(precedente) = precedente {
                if entree.timestamp_us < precedente.timestamp_us
                    || entree.offset <= precedente.offset
                {
                    return Err(NdError::Protocol("index désordonné".into()));
                }
            }
            precedente = Some(entree);
            entrees.push(entree);
        }
        Ok(entrees)
    }

    /// Section de fin : durée, nombre d'images, hachage éventuel.
    fn lire_fin(&mut self) -> Result<()> {
        let etiquette = self.lire_u8()?;
        if etiquette != SECTION_FIN {
            return Err(NdError::Protocol(format!(
                "section de fin attendue, étiquette {etiquette} trouvée"
            )));
        }
        self.duree_us = Some(self.lire_u64()?);
        self.images_declarees = Some(self.lire_u64()?);
        match self.lire_u8()? {
            0 => {
                self.hachage = None;
                self.position_hachage = None;
            }
            1 => {
                self.position_hachage = Some(self.position);
                let mut hachage = [0u8; 32];
                self.lire_exact(&mut hachage)?;
                self.hachage = Some(hachage);
            }
            autre => {
                return Err(NdError::Protocol(format!(
                    "drapeau de hachage invalide : {autre}"
                )))
            }
        }
        Ok(())
    }

    /// Pied de fichier : offset de l'index (recoupé) + magic de fin.
    fn lire_pied(&mut self) -> Result<()> {
        let offset = self.lire_u64()?;
        let mut magie = [0u8; 8];
        self.lire_exact(&mut magie)?;
        if &magie != FIN_MAGIC {
            return Err(NdError::Protocol(
                "pied de fichier absent ou corrompu".into(),
            ));
        }
        if Some(offset) != self.offset_index {
            return Err(NdError::Protocol(
                "offset d'index du pied incohérent avec la position réelle de l'index".into(),
            ));
        }
        Ok(())
    }
}

impl<R: Read + Seek> SessionReader<R> {
    /// Charge la table des images-clés d'un conteneur v2 en sautant
    /// directement au pied de fichier, sans balayer les images. Vérifie le
    /// pied, l'index et la section de fin, puis rembobine le lecteur au début
    /// des images.
    pub fn load_index(&mut self) -> Result<()> {
        if self.version != VERSION_INDEXEE {
            return Err(NdError::Protocol(
                "cet enregistrement (v1) n'a pas d'index : seule la lecture séquentielle est possible"
                    .into(),
            ));
        }
        let taille = self.source.seek(SeekFrom::End(0))?;
        // Minimum : index vide (5) + fin sans hachage (18) + pied (16).
        if taille < self.offset_premiere_image + 39 {
            return Err(NdError::Protocol(
                "enregistrement v2 tronqué : sections finales absentes".into(),
            ));
        }
        self.position = self.source.seek(SeekFrom::Start(taille - TAILLE_PIED))?;
        let offset_index = self.lire_u64()?;
        let mut magie = [0u8; 8];
        self.lire_exact(&mut magie)?;
        if &magie != FIN_MAGIC {
            return Err(NdError::Protocol(
                "pied de fichier absent ou corrompu".into(),
            ));
        }
        if offset_index < self.offset_premiere_image || offset_index >= taille - TAILLE_PIED {
            return Err(NdError::Protocol(
                "offset d'index hors des bornes du fichier".into(),
            ));
        }
        self.position = self.source.seek(SeekFrom::Start(offset_index))?;
        if self.lire_u8()? != SECTION_INDEX {
            return Err(NdError::Protocol(
                "pas de section d'index à l'offset annoncé par le pied".into(),
            ));
        }
        self.offset_index = Some(offset_index);
        let entrees = self.lire_corps_index()?;
        self.index = Some(entrees);
        self.lire_fin()?;
        if self.position != taille - TAILLE_PIED {
            return Err(NdError::Protocol(
                "octets excédentaires entre la section de fin et le pied".into(),
            ));
        }
        self.lire_pied()?;
        self.rembobiner()
    }

    /// Positionne le lecteur sur l'image-clé la plus proche **avant** (ou à)
    /// `timestamp_us`, et rend l'horodatage de cette image-clé. Si le
    /// timestamp précède la première image-clé, retombe sur celle-ci.
    /// Charge l'index à la première utilisation.
    ///
    /// L'appel suivant à [`SessionReader::next_frame`] rend l'image-clé
    /// elle-même, point de départ correct du décodage.
    pub fn seek_to_keyframe(&mut self, timestamp_us: u64) -> Result<u64> {
        if self.index.is_none() {
            self.load_index()?;
        }
        let index = self.index.as_deref().unwrap_or_default();
        let entree = index
            .iter()
            .rev()
            .find(|entree| entree.timestamp_us <= timestamp_us)
            .or_else(|| index.first())
            .copied()
            .ok_or_else(|| NdError::Protocol("aucune image-clé dans cet enregistrement".into()))?;
        self.position = self.source.seek(SeekFrom::Start(entree.offset))?;
        self.fin_atteinte = false;
        Ok(entree.timestamp_us)
    }

    /// Vérification d'intégrité complète du conteneur v2 :
    /// - pied, index et section de fin bien formés (via [`SessionReader::load_index`]) ;
    /// - toutes les images lisibles, horodatages croissants ;
    /// - table des images-clés exactement conforme aux images du flux ;
    /// - nombre d'images et durée conformes à la section de fin ;
    /// - hachage BLAKE3 recalculé et comparé, s'il est présent.
    ///
    /// Laisse le lecteur rembobiné au début des images.
    pub fn validate(&mut self) -> Result<ValidationReport> {
        self.load_index()?;
        let index_attendu = self.index.clone().unwrap_or_default();
        let (Some(offset_index), Some(images_annoncees), Some(duree_annoncee)) =
            (self.offset_index, self.images_declarees, self.duree_us)
        else {
            return Err(NdError::Protocol(
                "état interne incohérent après le chargement de l'index".into(),
            ));
        };

        // Balayage séquentiel intégral des images.
        let mut images = 0u64;
        let mut cles = Vec::new();
        let mut dernier: Option<u64> = None;
        loop {
            let offset_etiquette = self.position;
            let etiquette = self.lire_u8()?;
            match etiquette {
                SECTION_IMAGE => {
                    let horodatage = self.lire_u64()?;
                    let image = self.lire_corps_image(horodatage)?;
                    if dernier.is_some_and(|dernier| image.timestamp_us < dernier) {
                        return Err(NdError::Protocol(
                            "horodatages non croissants dans l'enregistrement".into(),
                        ));
                    }
                    if image.keyframe {
                        cles.push(KeyframeEntry {
                            timestamp_us: image.timestamp_us,
                            offset: offset_etiquette,
                        });
                    }
                    dernier = Some(image.timestamp_us);
                    images += 1;
                }
                SECTION_INDEX => {
                    if offset_etiquette != offset_index {
                        return Err(NdError::Protocol(
                            "la section d'index n'est pas à l'offset annoncé par le pied".into(),
                        ));
                    }
                    break;
                }
                autre => {
                    return Err(NdError::Protocol(format!(
                        "étiquette de section inconnue : {autre}"
                    )))
                }
            }
        }
        if cles != index_attendu {
            return Err(NdError::Protocol(
                "index incohérent : la table ne correspond pas aux images-clés du flux".into(),
            ));
        }
        if images != images_annoncees {
            return Err(NdError::Protocol(format!(
                "nombre d'images incohérent : {images} lues, {images_annoncees} annoncées"
            )));
        }
        if dernier.unwrap_or(0) != duree_annoncee {
            return Err(NdError::Protocol(
                "durée annoncée incohérente avec la dernière image".into(),
            ));
        }

        // Hachage BLAKE3 : recalculé sur tous les octets qu'il couvre.
        let hash_verified = if let Some(attendu) = self.hachage {
            let limite = self.position_hachage.ok_or_else(|| {
                NdError::Protocol("état interne incohérent : position du hachage inconnue".into())
            })?;
            self.source.seek(SeekFrom::Start(0))?;
            let mut hachoir = blake3::Hasher::new();
            let mut tampon = vec![0u8; 8192];
            let mut restant = limite;
            while restant > 0 {
                let pas = restant.min(8192) as usize;
                self.source.read_exact(&mut tampon[..pas])?;
                hachoir.update(&tampon[..pas]);
                restant -= pas as u64;
            }
            if hachoir.finalize() != blake3::Hash::from(attendu) {
                return Err(NdError::Protocol(
                    "hachage BLAKE3 invalide : enregistrement corrompu".into(),
                ));
            }
            true
        } else {
            false
        };

        self.rembobiner()?;
        Ok(ValidationReport {
            frames: images,
            keyframes: index_attendu.len() as u64,
            duration_us: duree_annoncee,
            hash_verified,
        })
    }

    /// Repositionne le lecteur au tout début des images.
    fn rembobiner(&mut self) -> Result<()> {
        self.position = self
            .source
            .seek(SeekFrom::Start(self.offset_premiere_image))?;
        self.fin_atteinte = false;
        Ok(())
    }
}

impl<R: Read> Iterator for SessionReader<R> {
    type Item = Result<RecordedFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_frame().transpose()
    }
}

/// Remplit `tampon` entièrement et renvoie `Ok(true)`, ou `Ok(false)` si le
/// flux se termine proprement **avant le premier octet**. Une fin de flux au
/// milieu du tampon est une erreur (enregistrement tronqué).
fn lire_ou_fin<R: Read>(source: &mut R, tampon: &mut [u8]) -> Result<bool> {
    let mut lus = 0;
    while lus < tampon.len() {
        match source.read(&mut tampon[lus..]) {
            Ok(0) if lus == 0 => return Ok(false),
            Ok(0) => return Err(NdError::Protocol("enregistrement tronqué".into())),
            Ok(n) => lus += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Jeu d'essai : timestamps croissants, keyframes alternées, tailles
    /// variées (dont une image vide).
    fn images_de_test() -> Vec<(u64, bool, Vec<u8>)> {
        (0..8u64)
            .map(|i| {
                let taille = (i % 5) as usize * 7;
                (i * 16_667, i % 3 == 0, vec![i as u8 + 1; taille])
            })
            .collect()
    }

    #[test]
    fn aller_retour_exact() {
        let images = images_de_test();

        let mut enregistreur = SessionRecorder::new(Vec::new()).unwrap();
        for (ts, kf, data) in &images {
            enregistreur.record(*ts, *kf, data).unwrap();
        }
        assert_eq!(enregistreur.frames_written(), images.len() as u64);
        let octets = enregistreur.finish().unwrap();

        // En-tête : magic puis version.
        assert_eq!(&octets[..6], MAGIC);
        assert_eq!(u16::from_le_bytes([octets[6], octets[7]]), VERSION);

        let mut lecteur = SessionReader::new(octets.as_slice()).unwrap();
        assert_eq!(lecteur.version(), VERSION);
        for (ts, kf, data) in &images {
            let image = lecteur.next_frame().unwrap().expect("image manquante");
            assert_eq!(image.timestamp_us, *ts);
            assert_eq!(image.keyframe, *kf);
            assert_eq!(&image.data, data);
        }
        // Fin propre, et fin stable si on insiste.
        assert!(lecteur.next_frame().unwrap().is_none());
        assert!(lecteur.next_frame().unwrap().is_none());
    }

    #[test]
    fn iterateur_equivalent_a_next_frame() {
        let images = images_de_test();
        let mut enregistreur = SessionRecorder::new(Vec::new()).unwrap();
        for (ts, kf, data) in &images {
            enregistreur.record(*ts, *kf, data).unwrap();
        }
        let octets = enregistreur.finish().unwrap();

        let lecteur = SessionReader::new(octets.as_slice()).unwrap();
        let relues: Vec<RecordedFrame> = lecteur.map(|image| image.unwrap()).collect();
        assert_eq!(relues.len(), images.len());
        for (relue, (ts, kf, data)) in relues.iter().zip(&images) {
            assert_eq!(relue.timestamp_us, *ts);
            assert_eq!(relue.keyframe, *kf);
            assert_eq!(&relue.data, data);
        }
    }

    #[test]
    fn enregistrement_vide_relu_vide() {
        let octets = SessionRecorder::new(Vec::new()).unwrap().finish().unwrap();
        let mut lecteur = SessionReader::new(octets.as_slice()).unwrap();
        assert!(lecteur.next_frame().unwrap().is_none());
    }

    #[test]
    fn magic_invalide_refuse() {
        assert!(SessionReader::new(&b"PASBON\x01\x00"[..]).is_err());
    }

    #[test]
    fn version_inconnue_refusee() {
        let mut octets = Vec::new();
        octets.extend_from_slice(MAGIC);
        octets.extend_from_slice(&99u16.to_le_bytes());
        assert!(SessionReader::new(octets.as_slice()).is_err());
    }

    #[test]
    fn troncature_detectee() {
        let mut enregistreur = SessionRecorder::new(Vec::new()).unwrap();
        enregistreur.record(42, true, b"abcdef").unwrap();
        let mut octets = enregistreur.finish().unwrap();
        octets.truncate(octets.len() - 3);

        let mut lecteur = SessionReader::new(octets.as_slice()).unwrap();
        assert!(lecteur.next_frame().is_err());
    }

    #[test]
    fn drapeau_keyframe_invalide_refuse() {
        let mut enregistreur = SessionRecorder::new(Vec::new()).unwrap();
        enregistreur.record(1, false, b"x").unwrap();
        let mut octets = enregistreur.finish().unwrap();
        // Corrompt l'octet keyframe (en-tête 8 octets + timestamp 8 octets).
        octets[8 + 8] = 7;

        let mut lecteur = SessionReader::new(octets.as_slice()).unwrap();
        assert!(lecteur.next_frame().is_err());
    }
}

#[cfg(test)]
mod tests_v2 {
    use super::*;
    use std::io::Cursor;

    fn meta_de_test() -> RecordingMetadata {
        RecordingMetadata {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: "nova-h264".into(),
            start_unix_ms: 1_750_000_000_000,
        }
    }

    /// Position de la première section image : en-tête (8) + métadonnées
    /// fixes (22) + nom du codec.
    fn offset_premiere_image() -> usize {
        8 + 22 + meta_de_test().codec.len()
    }

    /// Neuf images (10 000 µs d'écart), images-clés à 0, 40 000 et 80 000 µs.
    fn enregistrement(avec_hachage: bool) -> Vec<u8> {
        let mut enregistreur =
            IndexedRecorder::new(Vec::new(), meta_de_test(), avec_hachage).unwrap();
        for i in 0..9u64 {
            enregistreur
                .record(i * 10_000, i % 4 == 0, &[i as u8; 5])
                .unwrap();
        }
        assert_eq!(enregistreur.frames_written(), 9);
        assert_eq!(enregistreur.index().len(), 3);
        enregistreur.finish().unwrap()
    }

    #[test]
    fn v2_aller_retour_sequentiel() {
        let octets = enregistrement(true);
        assert_eq!(&octets[..6], MAGIC_V2);
        assert_eq!(&octets[octets.len() - 8..], FIN_MAGIC);

        // Une simple tranche (Read sans Seek) suffit pour la lecture séquentielle.
        let mut lecteur = SessionReader::new(octets.as_slice()).unwrap();
        assert_eq!(lecteur.version(), VERSION_INDEXEE);
        assert_eq!(lecteur.metadata(), Some(&meta_de_test()));
        for i in 0..9u64 {
            let image = lecteur.next_frame().unwrap().expect("image manquante");
            assert_eq!(image.timestamp_us, i * 10_000);
            assert_eq!(image.keyframe, i % 4 == 0);
            assert_eq!(image.data, vec![i as u8; 5]);
        }
        // Fin propre et stable ; les sections finales ont été absorbées.
        assert!(lecteur.next_frame().unwrap().is_none());
        assert!(lecteur.next_frame().unwrap().is_none());
        assert_eq!(lecteur.duration_us(), Some(80_000));
        assert_eq!(lecteur.declared_frames(), Some(9));
        assert_eq!(lecteur.index().map(<[KeyframeEntry]>::len), Some(3));
        assert!(lecteur.declared_hash().is_some());
    }

    #[test]
    fn v2_seek_retombe_sur_la_bonne_image_cle() {
        let mut lecteur = SessionReader::new(Cursor::new(enregistrement(true))).unwrap();

        // 55 000 µs : l'image-clé précédente est à 40 000 µs.
        assert_eq!(lecteur.seek_to_keyframe(55_000).unwrap(), 40_000);
        let image = lecteur.next_frame().unwrap().unwrap();
        assert!(image.keyframe);
        assert_eq!(image.timestamp_us, 40_000);
        // La lecture reprend bien le fil après l'image-clé.
        assert_eq!(lecteur.next_frame().unwrap().unwrap().timestamp_us, 50_000);

        // Timestamp exact d'une image-clé, et au-delà de la fin.
        assert_eq!(lecteur.seek_to_keyframe(80_000).unwrap(), 80_000);
        assert_eq!(lecteur.seek_to_keyframe(u64::MAX).unwrap(), 80_000);
        assert_eq!(lecteur.seek_to_keyframe(0).unwrap(), 0);
    }

    #[test]
    fn v2_seek_avant_la_premiere_cle_retombe_sur_celle_ci() {
        let mut enregistreur = IndexedRecorder::new(Vec::new(), meta_de_test(), false).unwrap();
        enregistreur.record(5_000, true, b"cle").unwrap();
        enregistreur.record(6_000, false, b"delta").unwrap();
        let octets = enregistreur.finish().unwrap();

        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        assert_eq!(lecteur.seek_to_keyframe(0).unwrap(), 5_000);
        assert_eq!(lecteur.next_frame().unwrap().unwrap().timestamp_us, 5_000);
    }

    #[test]
    fn v2_lecture_apres_seek_va_jusqu_au_bout() {
        let mut lecteur = SessionReader::new(Cursor::new(enregistrement(true))).unwrap();
        lecteur.seek_to_keyframe(80_000).unwrap();
        // 80 000 µs = image 8 : c'est la dernière, puis fin propre.
        assert_eq!(lecteur.next_frame().unwrap().unwrap().timestamp_us, 80_000);
        assert!(lecteur.next_frame().unwrap().is_none());
    }

    #[test]
    fn v2_validate_conteneur_sain() {
        let mut lecteur = SessionReader::new(Cursor::new(enregistrement(true))).unwrap();
        let rapport = lecteur.validate().unwrap();
        assert_eq!(
            rapport,
            ValidationReport {
                frames: 9,
                keyframes: 3,
                duration_us: 80_000,
                hash_verified: true,
            }
        );
        // Après validation, le lecteur est rembobiné et lisible.
        assert_eq!(lecteur.next_frame().unwrap().unwrap().timestamp_us, 0);

        // Sans hachage : mêmes chiffres, hachage simplement absent.
        let mut lecteur = SessionReader::new(Cursor::new(enregistrement(false))).unwrap();
        assert!(!lecteur.validate().unwrap().hash_verified);
    }

    #[test]
    fn v2_hachage_detecte_une_donnee_corrompue() {
        let mut octets = enregistrement(true);
        // Corrompt un octet de données de la première image (structure intacte).
        let premier_octet_donnees = offset_premiere_image() + 1 + 8 + 1 + 4;
        octets[premier_octet_donnees] ^= 0xFF;

        // La lecture séquentielle ne voit rien (les données sont opaques)…
        let lecteur = SessionReader::new(octets.as_slice()).unwrap();
        assert_eq!(lecteur.count(), 9);
        // … mais la vérification d'intégrité, si.
        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        assert!(lecteur.validate().is_err());
    }

    #[test]
    fn v2_sans_hachage_la_corruption_des_donnees_est_invisible() {
        // Limite documentée : sans hachage, seule la structure est vérifiable.
        let mut octets = enregistrement(false);
        let premier_octet_donnees = offset_premiere_image() + 1 + 8 + 1 + 4;
        octets[premier_octet_donnees] ^= 0xFF;
        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        assert!(lecteur.validate().is_ok());
    }

    #[test]
    fn v2_drapeau_keyframe_falsifie_detecte_sans_hachage() {
        let mut octets = enregistrement(false);
        // Bascule le drapeau keyframe de la première image : l'index ne
        // correspond plus aux images-clés réelles.
        let octet_drapeau = offset_premiere_image() + 1 + 8;
        assert_eq!(octets[octet_drapeau], 1);
        octets[octet_drapeau] = 0;
        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        assert!(lecteur.validate().is_err());
    }

    #[test]
    fn v2_index_falsifie_detecte() {
        let mut octets = enregistrement(true);
        // Retrouve l'index via le pied, puis altère l'horodatage de la
        // première entrée (0 → 1).
        let pied = octets.len() - 16;
        let offset_index = u64::from_le_bytes(octets[pied..pied + 8].try_into().unwrap()) as usize;
        octets[offset_index + 5] = 1;
        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        assert!(lecteur.validate().is_err());
    }

    #[test]
    fn v2_troncature_et_pied_corrompu_detectes() {
        let octets = enregistrement(true);

        let mut tronque = octets.clone();
        tronque.truncate(tronque.len() - 5);
        let mut lecteur = SessionReader::new(Cursor::new(tronque)).unwrap();
        assert!(lecteur.load_index().is_err());

        let mut pied_corrompu = octets;
        let dernier = pied_corrompu.len() - 1;
        pied_corrompu[dernier] ^= 0xFF;
        let mut lecteur = SessionReader::new(Cursor::new(pied_corrompu)).unwrap();
        assert!(lecteur.load_index().is_err());
    }

    #[test]
    fn v2_horodatage_decroissant_refuse_a_l_ecriture() {
        let mut enregistreur = IndexedRecorder::new(Vec::new(), meta_de_test(), false).unwrap();
        enregistreur.record(10_000, true, b"a").unwrap();
        assert!(enregistreur.record(5_000, false, b"b").is_err());
        // Un horodatage égal reste permis (rafale de sous-images).
        enregistreur.record(10_000, false, b"c").unwrap();
    }

    #[test]
    fn v2_enregistrement_vide() {
        let octets = IndexedRecorder::new(Vec::new(), meta_de_test(), true)
            .unwrap()
            .finish()
            .unwrap();
        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        let rapport = lecteur.validate().unwrap();
        assert_eq!(rapport.frames, 0);
        assert_eq!(rapport.keyframes, 0);
        assert_eq!(rapport.duration_us, 0);
        assert!(rapport.hash_verified);
        assert!(lecteur.next_frame().unwrap().is_none());
        // Sans image-clé, la recherche est impossible et le dit clairement.
        assert!(lecteur.seek_to_keyframe(0).is_err());
    }

    #[test]
    fn v1_sans_index_le_seek_est_refuse() {
        let mut enregistreur = SessionRecorder::new(Vec::new()).unwrap();
        enregistreur.record(0, true, b"x").unwrap();
        let octets = enregistreur.finish().unwrap();
        let mut lecteur = SessionReader::new(Cursor::new(octets)).unwrap();
        assert!(lecteur.load_index().is_err());
        assert!(lecteur.seek_to_keyframe(0).is_err());
    }

    #[test]
    fn v2_version_inattendue_refusee() {
        let mut octets = Vec::new();
        octets.extend_from_slice(MAGIC_V2);
        octets.extend_from_slice(&1u16.to_le_bytes());
        assert!(SessionReader::new(octets.as_slice()).is_err());
    }
}
