//! Relecture d'un enregistrement de session : ouvre un fichier `.mp4`
//! (produit par [`super::mp4::Mp4Muxer`]) **ou** une archive `.ndr` v2
//! (produite par [`super::IndexedRecorder`]), détecte le format, expose les
//! métadonnées (dimensions, cadence, durée) et restitue les échantillons
//! encodés **prêts à décoder** ([`EncodedSample`], H.264 Annex B).
//!
//! C'est le point d'entrée que nd-ffi / l'UI utilisent pour rejouer une
//! session : pour chaque [`EncodedSample`], construire un
//! [`nd_codec::EncodedChunk`] (`data`, `is_keyframe`, `timestamp_us` + un
//! `monitor`) et le passer au décodeur — voir le test de re-décodage réel
//! `tests/recording_mp4.rs`.
//!
//! Accès par temps : [`RecordingPlayer::sample_at`] rend l'**image-clé** la
//! plus proche avant (ou à) l'horodatage demandé — le point de départ correct
//! d'un décodage pour un « seek ».
//!
//! L'extraction ne fait que lire le conteneur ; le décodage est laissé à
//! l'appelant (nd-codec), qui construit le [`nd_codec::EncodedChunk`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use nd_proto::{NdError, Result};

use super::mp4::Mp4Reader;
use super::{EncodedSample, SessionReader, MAGIC, MAGIC_V2};

/// Format de conteneur détecté à l'ouverture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingFormat {
    /// MP4 ISO BMFF (piste vidéo AVC), lisible aussi par VLC/ffmpeg.
    Mp4,
    /// Archive interne NovaDesk `.ndr` v2 (conteneur indexé).
    Ndr,
}

/// Lecteur unifié interne : MP4 ou `.ndr`.
#[derive(Debug)]
enum LecteurRelecture<R: Read + Seek> {
    Mp4(Mp4Reader<R>),
    Ndr(SessionReader<R>),
}

/// Lecteur de relecture : ouvre un enregistrement (`.mp4` **ou** `.ndr`),
/// expose ses métadonnées et rend les échantillons prêts à décoder.
///
/// - [`RecordingPlayer::samples`] — tous les échantillons, dans l'ordre ;
/// - [`RecordingPlayer::sample_at`] — l'image-clé la plus proche ≤ `ts` (seek).
///
/// Les données rendues sont du H.264 Annex B ([`EncodedSample`]) : l'appelant
/// n'a qu'à les envelopper dans un [`nd_codec::EncodedChunk`] pour les décoder.
#[derive(Debug)]
pub struct RecordingPlayer<R: Read + Seek> {
    lecteur: LecteurRelecture<R>,
    format: RecordingFormat,
    width: u32,
    height: u32,
    fps: u32,
    duration_us: u64,
    frames: u64,
}

impl RecordingPlayer<File> {
    /// Ouvre l'enregistrement désigné par `chemin` (format auto-détecté).
    pub fn open_path(chemin: impl AsRef<Path>) -> Result<Self> {
        RecordingPlayer::open(File::open(chemin)?)
    }
}

impl<R: Read + Seek> RecordingPlayer<R> {
    /// Ouvre un enregistrement depuis une source `Read + Seek`, en détectant le
    /// format sur les premiers octets : magic `NDREC1`/`NDREC2` → `.ndr`, boîte
    /// `ftyp` → MP4. Charge et met en cache dimensions, cadence et durée.
    ///
    /// Un `.ndr` v1 (séquentiel, sans métadonnées) est refusé : ses dimensions
    /// et sa cadence sont inconnues, la relecture ne peut pas les annoncer
    /// (le convertir en MP4 ou le réenregistrer en v2).
    pub fn open(mut source: R) -> Result<Self> {
        let mut entete = [0u8; 8];
        source.seek(SeekFrom::Start(0))?;
        source
            .read_exact(&mut entete)
            .map_err(|_| NdError::Protocol("fichier trop court pour un enregistrement".into()))?;
        source.seek(SeekFrom::Start(0))?;

        let magic6 = &entete[..6];
        if magic6 == MAGIC_V2.as_slice() || magic6 == MAGIC.as_slice() {
            Self::depuis_ndr(source)
        } else if &entete[4..8] == b"ftyp" {
            Self::depuis_mp4(source)
        } else {
            Err(NdError::Protocol(
                "format non reconnu : ni MP4 (boîte ftyp) ni enregistrement NovaDesk (NDREC*)"
                    .into(),
            ))
        }
    }

    fn depuis_mp4(source: R) -> Result<Self> {
        let lecteur = Mp4Reader::new(source)?;
        Ok(RecordingPlayer {
            format: RecordingFormat::Mp4,
            width: lecteur.width(),
            height: lecteur.height(),
            fps: lecteur.fps(),
            duration_us: lecteur.duration_us(),
            frames: lecteur.frames(),
            lecteur: LecteurRelecture::Mp4(lecteur),
        })
    }

    fn depuis_ndr(source: R) -> Result<Self> {
        let mut lecteur = SessionReader::new(source)?;
        let metadata = lecteur.metadata().cloned().ok_or_else(|| {
            NdError::Protocol(
                "enregistrement .ndr v1 sans métadonnées : dimensions/cadence inconnues, \
                 relecture impossible (convertir en MP4 ou réenregistrer en v2)"
                    .into(),
            )
        })?;
        // Charge l'index et les compteurs de fin (durée, nombre d'images) sans
        // balayer les images ; laisse la lecture rembobinée au début.
        lecteur.load_index()?;
        Ok(RecordingPlayer {
            format: RecordingFormat::Ndr,
            width: metadata.width,
            height: metadata.height,
            fps: metadata.fps,
            duration_us: lecteur.duration_us().unwrap_or(0),
            frames: lecteur.declared_frames().unwrap_or(0),
            lecteur: LecteurRelecture::Ndr(lecteur),
        })
    }

    /// Format de conteneur détecté à l'ouverture.
    #[must_use]
    pub fn format(&self) -> RecordingFormat {
        self.format
    }

    /// Largeur des images, en pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Hauteur des images, en pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Cadence nominale, en images par seconde (dérivée des durées pour un MP4,
    /// lue dans les métadonnées pour un `.ndr`).
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Durée de l'enregistrement, en microsecondes.
    ///
    /// Note : pour un MP4, la durée inclut celle de la dernière image (somme
    /// des `stts`) ; pour un `.ndr`, c'est l'horodatage de la dernière image.
    #[must_use]
    pub fn duration_us(&self) -> u64 {
        self.duration_us
    }

    /// Nombre d'images de l'enregistrement.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// **Tous** les échantillons encodés, dans l'ordre de présentation, données
    /// au format H.264 Annex B prêtes à décoder ([`EncodedSample`]). Appelable
    /// plusieurs fois (rembobinage interne).
    pub fn samples(&mut self) -> Result<Vec<EncodedSample>> {
        match &mut self.lecteur {
            LecteurRelecture::Mp4(r) => r.samples(),
            LecteurRelecture::Ndr(r) => r.samples(),
        }
    }

    /// Image-clé la plus proche **avant** (ou à) `timestamp_us`, prête à
    /// décoder — point de départ correct d'un « seek ». Si `timestamp_us`
    /// précède la première image-clé, rend celle-ci ; `None` si
    /// l'enregistrement ne contient aucune image-clé.
    pub fn sample_at(&mut self, timestamp_us: u64) -> Result<Option<EncodedSample>> {
        match &mut self.lecteur {
            LecteurRelecture::Mp4(r) => r.sample_at(timestamp_us),
            LecteurRelecture::Ndr(r) => {
                if r.index().is_none_or(|entrees| entrees.is_empty()) {
                    return Ok(None);
                }
                r.seek_to_keyframe(timestamp_us)?;
                let image = r.next_frame()?.ok_or_else(|| {
                    NdError::Protocol("image-clé annoncée par l'index absente du flux".into())
                })?;
                Ok(Some(EncodedSample {
                    timestamp_us: image.timestamp_us,
                    is_keyframe: image.keyframe,
                    data: image.data,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::recording::mp4::Mp4Muxer;
    use crate::recording::{IndexedRecorder, RecordingMetadata, SessionRecorder};

    /// SPS factice plausible (profil Baseline 0x42, niveau 30).
    fn sps() -> Vec<u8> {
        vec![0x67, 0x42, 0xC0, 0x1E, 0xAB, 0xCD]
    }

    fn pps() -> Vec<u8> {
        vec![0x68, 0xCE, 0x3C, 0x80]
    }

    /// Unité Annex B d'image-clé : SPS + PPS + tranche IDR.
    fn unite_cle(remplissage: u8) -> Vec<u8> {
        let mut unite = Vec::new();
        for nal in [sps(), pps(), vec![0x65, remplissage, remplissage, 0x11]] {
            unite.extend_from_slice(&[0, 0, 0, 1]);
            unite.extend_from_slice(&nal);
        }
        unite
    }

    /// Unité Annex B d'image delta : une tranche non-IDR.
    fn unite_delta(remplissage: u8) -> Vec<u8> {
        let mut unite = vec![0, 0, 0, 1];
        unite.extend_from_slice(&[0x41, remplissage, 0x22]);
        unite
    }

    fn meta() -> RecordingMetadata {
        RecordingMetadata {
            width: 640,
            height: 360,
            fps: 25,
            codec: "nova-h264".into(),
            start_unix_ms: 1_750_000_000_000,
        }
    }

    /// 6 images à 25 i/s (40 000 µs), clés en 0 et 3 (donc ts 0 et 120 000).
    fn unites() -> Vec<(u64, bool, Vec<u8>)> {
        (0..6u64)
            .map(|i| {
                let cle = i % 3 == 0;
                let unite = if cle {
                    unite_cle(i as u8)
                } else {
                    unite_delta(i as u8)
                };
                (i * 40_000, cle, unite)
            })
            .collect()
    }

    fn mp4() -> Vec<u8> {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        for (ts, cle, unite) in unites() {
            muxeur.record(ts, cle, &unite).unwrap();
        }
        muxeur.finish().unwrap().into_inner()
    }

    fn ndr() -> Vec<u8> {
        let mut archive = IndexedRecorder::new(Vec::new(), meta(), true).unwrap();
        for (ts, cle, unite) in unites() {
            archive.record(ts, cle, &unite).unwrap();
        }
        archive.finish().unwrap()
    }

    #[test]
    fn ouverture_mp4_detecte_le_format_et_les_metadonnees() {
        let p = RecordingPlayer::open(Cursor::new(mp4())).unwrap();
        assert_eq!(p.format(), RecordingFormat::Mp4);
        assert_eq!((p.width(), p.height()), (640, 360));
        assert_eq!(p.fps(), 25);
        assert_eq!(p.frames(), 6);
        // MP4 : durée = somme des stts (inclut la dernière image).
        assert_eq!(p.duration_us(), 240_000);
    }

    #[test]
    fn ouverture_ndr_detecte_le_format_et_les_metadonnees() {
        let p = RecordingPlayer::open(Cursor::new(ndr())).unwrap();
        assert_eq!(p.format(), RecordingFormat::Ndr);
        assert_eq!((p.width(), p.height()), (640, 360));
        assert_eq!(p.fps(), 25);
        assert_eq!(p.frames(), 6);
        // .ndr : durée = horodatage de la dernière image (5 × 40 000).
        assert_eq!(p.duration_us(), 200_000);
    }

    #[test]
    fn samples_mp4_sont_en_annexb_et_complets() {
        let mut p = RecordingPlayer::open(Cursor::new(mp4())).unwrap();
        let echs = p.samples().unwrap();
        assert_eq!(echs.len(), 6);
        // Image-clé : commence par un code de départ et réinjecte le SPS.
        assert!(echs[0].is_keyframe);
        assert_eq!(&echs[0].data[..4], &[0, 0, 0, 1]);
        assert!(echs[0].data.windows(sps().len()).any(|f| f == sps()));
        // Delta : pas de réinjection de paramètres.
        assert!(!echs[1].is_keyframe);
        assert!(!echs[1].data.windows(sps().len()).any(|f| f == sps()));
        // Deux appels successifs rendent le même résultat.
        assert_eq!(p.samples().unwrap(), echs);
    }

    #[test]
    fn samples_ndr_restituent_les_octets_ecrits() {
        let mut p = RecordingPlayer::open(Cursor::new(ndr())).unwrap();
        let echs = p.samples().unwrap();
        assert_eq!(echs.len(), 6);
        // Le .ndr archive les unités telles quelles (déjà Annex B).
        assert_eq!(echs[0].data, unite_cle(0));
        assert_eq!(echs[1].data, unite_delta(1));
        assert_eq!(p.samples().unwrap(), echs);
    }

    #[test]
    fn sample_at_retombe_sur_la_bonne_image_cle() {
        for octets in [mp4(), ndr()] {
            let mut p = RecordingPlayer::open(Cursor::new(octets)).unwrap();
            // Clés à ts 0 et 120 000. Une cible entre les deux retombe sur 120 000.
            let s = p.sample_at(150_000).unwrap().unwrap();
            assert!(s.is_keyframe);
            assert_eq!(s.timestamp_us, 120_000);
            // Avant la première clé → première clé.
            assert_eq!(p.sample_at(0).unwrap().unwrap().timestamp_us, 0);
            // Au-delà de la fin → dernière clé.
            assert_eq!(
                p.sample_at(u64::MAX).unwrap().unwrap().timestamp_us,
                120_000
            );
            // samples() reste utilisable après des seeks.
            assert_eq!(p.samples().unwrap().len(), 6);
        }
    }

    #[test]
    fn format_inconnu_ou_trop_court_refuse() {
        assert!(RecordingPlayer::open(Cursor::new(b"abc".to_vec())).is_err());
        assert!(RecordingPlayer::open(Cursor::new(b"XXXXYYYY".to_vec())).is_err());
    }

    #[test]
    fn ndr_v1_sans_metadonnees_refuse() {
        let mut r = SessionRecorder::new(Vec::new()).unwrap();
        r.record(0, true, &unite_cle(0)).unwrap();
        let octets = r.finish().unwrap();
        assert!(RecordingPlayer::open(Cursor::new(octets)).is_err());
    }
}
