//! Mux MP4 (ISO BMFF) pour l'enregistrement de session : produit un fichier
//! **réellement lisible** par les lecteurs standard (VLC, ffmpeg, lecteurs
//! natifs) à partir des unités H.264 fournies par `nd-codec`.
//!
//! Écrit en pur Rust, sans dépendance de mux : les boîtes ISO BMFF sont
//! construites à la main (voir plan 13, §enregistrement). Structure produite :
//!
//! ```text
//! ftyp                         (marque `isom`, compatible `avc1`/`iso2`/`mp41`)
//! mdat                         (échantillons AVCC : [u32 BE longueur][NAL]…)
//! moov
//!   mvhd                       (échelle film 1 000 = millisecondes)
//!   trak > tkhd                (dimensions réelles)
//!        > mdia > mdhd         (échelle média 90 000 Hz, durée réelle)
//!               > hdlr `vide`
//!               > minf > vmhd + dinf
//!                      > stbl > stsd > avc1 > avcC   (SPS/PPS réels du flux)
//!                             > stts                 (durées réelles des images)
//!                             > stss                 (index des images-clés)
//!                             > stsc + stsz + stco   (tables d'échantillons)
//! free `NDB3`                  (hachage BLAKE3 de tous les octets précédents)
//! ```
//!
//! Entrée : du H.264 au format **Annex B** (codes de départ `00 00 01` /
//! `00 00 00 01`), tel que produit par l'encodeur openh264 de `nd-codec`. Le
//! muxeur convertit chaque unité en AVCC (préfixes de longueur 4 octets),
//! déporte les SPS/PPS dans la boîte `avcC` (exigence du type d'échantillon
//! `avc1`) et impose que le **premier échantillon soit une image-clé** — sans
//! quoi le fichier ne serait pas décodable depuis le début.
//!
//! Intégrité : la dernière boîte du fichier est une boîte `free` (ignorée par
//! tous les lecteurs) marquée `NDB3` et contenant le BLAKE3 des octets qui la
//! précèdent ; [`Mp4Reader::validate`] le recalcule et le compare.
//!
//! Relecture : [`Mp4Reader`] rouvre le fichier, reconstruit la table des
//! échantillons (stsc/stco/stsz/stts/stss), vérifie la structure AVCC de
//! chaque échantillon puis peut restituer les unités au format Annex B
//! ([`Mp4Reader::sample_annexb`]) pour un décodeur réel — c'est la preuve de
//! rejouabilité utilisée par les tests et l'exemple `record_and_replay`.

use std::io::{Read, Seek, SeekFrom, Write};

use nd_codec::EncodedChunk;
use nd_proto::{NdError, Result};

use super::{EncodedSample, RecordedFrame, RecordingMetadata, SessionReader};

/// Échelle de temps de la piste vidéo (90 kHz, standard broadcast : les
/// horodatages microsecondes s'y convertissent sans dérive cumulée).
pub const MEDIA_TIMESCALE: u32 = 90_000;

/// Échelle de temps du film (millisecondes, pour `mvhd`).
const FILM_TIMESCALE: u32 = 1_000;

/// Marqueur du hachage BLAKE3 dans la boîte `free` finale.
const MARQUE_NDB3: &[u8; 4] = b"NDB3";

/// Taille de la boîte `free` d'intégrité : en-tête (8) + marque (4) + hachage (32).
const TAILLE_BOITE_HACHAGE: u64 = 44;

/// Écart entre l'époque MP4 (1904-01-01) et l'époque Unix (1970-01-01), en secondes.
const EPOQUE_MP4_VERS_UNIX: u64 = 2_082_844_800;

/// Plafond de lecture d'une boîte chargée en mémoire (`moov`, `ftyp`) : garde
/// contre une taille corrompue qui déclencherait une allocation absurde.
const PLAFOND_BOITE_MEMOIRE: u64 = 256 * 1024 * 1024;

/// Plafond d'échantillons acceptés d'une table `stsz` à taille fixe (forme
/// que [`Mp4Muxer`] ne produit pas) : ~465 h de vidéo à 60 i/s, garde contre
/// un compte falsifié qui ferait allouer des gigaoctets au lecteur.
const PLAFOND_ECHANTILLONS: usize = 100_000_000;

// ---------------------------------------------------------------------------
// Aides communes
// ---------------------------------------------------------------------------

/// Le nom de codec annoncé désigne-t-il du H.264 ? (seul format muxé ici)
fn codec_est_h264(nom: &str) -> bool {
    let nom = nom.to_ascii_lowercase();
    nom.contains("264") || nom.contains("avc")
}

/// Microsecondes → tics 90 kHz, à l'arrondi près (chaque horodatage est
/// converti individuellement : l'erreur d'arrondi ne se cumule jamais).
fn us_vers_90khz(us: u64) -> u64 {
    ((u128::from(us) * 9 + 50) / 100) as u64
}

/// Tics 90 kHz → microsecondes (troncature, écart ≤ 1 µs par horodatage).
fn tics_vers_us(tics: u64) -> u64 {
    (u128::from(tics) * 100 / 9) as u64
}

/// Découpe un flux H.264 Annex B en unités NAL (sans codes de départ).
///
/// Accepte les codes de départ à 3 (`00 00 01`) et 4 octets (`00 00 00 01`) ;
/// refuse un flux qui ne commence pas par un code de départ.
fn decouper_annexb(data: &[u8]) -> Result<Vec<&[u8]>> {
    let commence_par_code = data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]);
    if !commence_par_code {
        return Err(NdError::Protocol(
            "flux H.264 Annex B attendu (aucun code de départ 00 00 01 en tête)".into(),
        ));
    }
    // Positions de tous les motifs `00 00 01`.
    let mut debuts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            debuts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(debuts.len());
    for (rang, &debut) in debuts.iter().enumerate() {
        let charge_debut = debut + 3;
        // Fin de la NAL : début du code suivant, moins l'octet nul qui lui
        // appartient si c'est un code à 4 octets.
        let charge_fin = match debuts.get(rang + 1) {
            Some(&suivant) => {
                if suivant > charge_debut && data[suivant - 1] == 0 {
                    suivant - 1
                } else {
                    suivant
                }
            }
            None => data.len(),
        };
        if charge_fin > charge_debut {
            nals.push(&data[charge_debut..charge_fin]);
        }
    }
    if nals.is_empty() {
        return Err(NdError::Protocol(
            "flux Annex B sans aucune unité NAL exploitable".into(),
        ));
    }
    Ok(nals)
}

/// Type d'une unité NAL H.264 (5 bits de poids faible du premier octet).
fn type_nal(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |premier| premier & 0x1F)
}

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// Boîte MP4 simple : `[u32 BE taille][type 4 octets][corps]`.
fn boite(nom: &[u8; 4], corps: &[u8]) -> Result<Vec<u8>> {
    let taille = u32::try_from(corps.len() + 8)
        .map_err(|_| NdError::Protocol(format!("boîte {nom:?} trop grande pour le format")))?;
    let mut sortie = Vec::with_capacity(corps.len() + 8);
    sortie.extend_from_slice(&taille.to_be_bytes());
    sortie.extend_from_slice(nom);
    sortie.extend_from_slice(corps);
    Ok(sortie)
}

/// Boîte MP4 « pleine » : boîte + `[u8 version][u24 drapeaux]` en tête du corps.
fn boite_pleine(nom: &[u8; 4], version: u8, drapeaux: u32, corps: &[u8]) -> Result<Vec<u8>> {
    let mut plein = Vec::with_capacity(corps.len() + 4);
    plein.push(version);
    plein.extend_from_slice(&drapeaux.to_be_bytes()[1..]);
    plein.extend_from_slice(corps);
    boite(nom, &plein)
}

/// Matrice de transformation identité (tkhd/mvhd).
const MATRICE_IDENTITE: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

// ---------------------------------------------------------------------------
// Écriture : Mp4Muxer
// ---------------------------------------------------------------------------

/// Un échantillon écrit dans `mdat` (une image encodée).
#[derive(Debug, Clone, Copy)]
struct EchantillonEcrit {
    taille: u32,
    timestamp_us: u64,
    keyframe: bool,
}

/// Mux MP4 d'une piste vidéo H.264 : accepte des [`EncodedChunk`] (ou des
/// unités Annex B brutes) et produit un fichier `.mp4` standard, lisible par
/// n'importe quel lecteur.
///
/// La sortie doit être `Read + Write + Seek` (un [`std::fs::File`], un
/// `Cursor<Vec<u8>>`…) : la taille de `mdat` est corrigée à la clôture, et le
/// hachage BLAKE3 final relit le fichier écrit.
///
/// Contrat d'usage (vérifié) :
/// - le **premier** échantillon doit être une image-clé (fichier décodable
///   depuis le début — l'orchestrateur force une image-clé au démarrage de
///   l'enregistrement) ;
/// - horodatages croissants au sens large ;
/// - un seul jeu SPS/PPS par enregistrement (un changement de résolution en
///   cours de session doit ouvrir un nouveau fichier).
#[derive(Debug)]
pub struct Mp4Muxer<W: Read + Write + Seek> {
    sortie: W,
    metadata: RecordingMetadata,
    echantillons: Vec<EchantillonEcrit>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// Octets de charge utile écrits dans `mdat` jusqu'ici.
    charge_mdat: u64,
    /// Position absolue de l'en-tête `mdat` (juste après `ftyp`).
    offset_mdat: u64,
    dernier_ts: Option<u64>,
}

impl<W: Read + Write + Seek> Mp4Muxer<W> {
    /// Ouvre le muxeur : vérifie les métadonnées (dimensions non nulles,
    /// cadence non nulle, codec H.264) puis écrit `ftyp` et l'en-tête `mdat`.
    pub fn new(mut sortie: W, metadata: RecordingMetadata) -> Result<Self> {
        if !codec_est_h264(&metadata.codec) {
            return Err(NdError::Protocol(format!(
                "mux MP4 : seul H.264 est géré, codec « {} » refusé",
                metadata.codec
            )));
        }
        if metadata.width == 0 || metadata.height == 0 {
            return Err(NdError::Protocol(
                "mux MP4 : dimensions nulles dans les métadonnées".into(),
            ));
        }
        if metadata.fps == 0 {
            return Err(NdError::Protocol(
                "mux MP4 : cadence (fps) nulle dans les métadonnées".into(),
            ));
        }

        // ftyp : marque majeure isom, compatible avec les lecteurs courants.
        let mut ftyp_corps = Vec::with_capacity(24);
        ftyp_corps.extend_from_slice(b"isom");
        ftyp_corps.extend_from_slice(&0x200u32.to_be_bytes());
        for marque in [b"isom", b"iso2", b"avc1", b"mp41"] {
            ftyp_corps.extend_from_slice(marque);
        }
        let ftyp = boite(b"ftyp", &ftyp_corps)?;
        sortie.write_all(&ftyp)?;
        let offset_mdat = ftyp.len() as u64;

        // mdat en forme longue (taille sur 64 bits, corrigée à la clôture) :
        // pas de plafond 4 Gio, et l'offset des échantillons est constant.
        sortie.write_all(&1u32.to_be_bytes())?;
        sortie.write_all(b"mdat")?;
        sortie.write_all(&16u64.to_be_bytes())?;

        Ok(Mp4Muxer {
            sortie,
            metadata,
            echantillons: Vec::new(),
            sps: None,
            pps: None,
            charge_mdat: 0,
            offset_mdat,
            dernier_ts: None,
        })
    }

    /// Ajoute une unité encodée telle que produite par `nd-codec`. Le champ
    /// `monitor` est ignoré : l'enregistrement est mono-piste (un fichier par
    /// moniteur enregistré).
    pub fn record_video_chunk(&mut self, chunk: &EncodedChunk) -> Result<()> {
        self.record(chunk.timestamp_us, chunk.is_keyframe, &chunk.data)
    }

    /// Ajoute une image H.264 Annex B. Les SPS/PPS rencontrés sont déportés
    /// vers la boîte `avcC` ; les autres NAL forment l'échantillon (AVCC).
    ///
    /// Une unité ne contenant **que** des SPS/PPS est absorbée sans créer
    /// d'échantillon (paramètres capturés, aucune image à dater).
    pub fn record(&mut self, timestamp_us: u64, keyframe: bool, annexb: &[u8]) -> Result<()> {
        if self
            .dernier_ts
            .is_some_and(|dernier| timestamp_us < dernier)
        {
            return Err(NdError::Protocol(format!(
                "horodatage décroissant : {timestamp_us} µs après {} µs",
                self.dernier_ts.unwrap_or(0)
            )));
        }
        let nals = decouper_annexb(annexb)?;

        // Sépare paramètres (SPS/PPS → avcC) et NAL d'image (→ mdat).
        let mut nals_image: Vec<&[u8]> = Vec::with_capacity(nals.len());
        for nal in nals {
            match type_nal(nal) {
                NAL_SPS => self.capturer_parametre(nal, true)?,
                NAL_PPS => self.capturer_parametre(nal, false)?,
                _ => nals_image.push(nal),
            }
        }
        if nals_image.is_empty() {
            // Paramètres seuls : rien à échantillonner.
            return Ok(());
        }
        if self.echantillons.is_empty() && !keyframe {
            return Err(NdError::Protocol(
                "le premier échantillon d'un MP4 doit être une image-clé \
                 (forcer une image-clé au démarrage de l'enregistrement)"
                    .into(),
            ));
        }

        let mut taille = 0u64;
        for nal in &nals_image {
            taille += 4 + nal.len() as u64;
        }
        let taille = u32::try_from(taille).map_err(|_| {
            NdError::Protocol("image trop grande pour un échantillon MP4 (> u32)".into())
        })?;
        for nal in &nals_image {
            // La longueur tient dans u32 : bornée par la taille totale ci-dessus.
            self.sortie.write_all(&(nal.len() as u32).to_be_bytes())?;
            self.sortie.write_all(nal)?;
        }
        self.charge_mdat += u64::from(taille);
        self.echantillons.push(EchantillonEcrit {
            taille,
            timestamp_us,
            keyframe,
        });
        self.dernier_ts = Some(timestamp_us);
        Ok(())
    }

    /// Capture un SPS ou un PPS. Refuse un changement de paramètres en cours
    /// d'enregistrement (une seule `avcC` par fichier) ; les répétitions à
    /// l'identique — chaque image-clé openh264 les rejoue — sont absorbées.
    fn capturer_parametre(&mut self, nal: &[u8], est_sps: bool) -> Result<()> {
        if est_sps && nal.len() < 4 {
            return Err(NdError::Protocol(
                "SPS H.264 tronqué (moins de 4 octets)".into(),
            ));
        }
        let (case, quoi) = if est_sps {
            (&mut self.sps, "SPS")
        } else {
            (&mut self.pps, "PPS")
        };
        match case {
            Some(connu) if connu.as_slice() != nal => Err(NdError::Protocol(format!(
                "changement de {quoi} en cours d'enregistrement non géré \
                 (ouvrir un nouveau fichier au changement de résolution)"
            ))),
            Some(_) => Ok(()),
            None => {
                *case = Some(nal.to_vec());
                Ok(())
            }
        }
    }

    /// Nombre d'échantillons (images) écrits jusqu'ici.
    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.echantillons.len() as u64
    }

    /// Nombre d'images-clés écrites jusqu'ici.
    #[must_use]
    pub fn keyframes_written(&self) -> u64 {
        self.echantillons.iter().filter(|e| e.keyframe).count() as u64
    }

    /// Métadonnées annoncées à l'ouverture.
    #[must_use]
    pub fn metadata(&self) -> &RecordingMetadata {
        &self.metadata
    }

    /// Clôt le fichier : corrige la taille de `mdat`, écrit `moov` (tables
    /// d'échantillons réelles), puis la boîte d'intégrité BLAKE3, et rend le
    /// flux sous-jacent. Refuse un enregistrement sans image ou sans SPS/PPS
    /// (le fichier ne serait pas décodable).
    pub fn finish(mut self) -> Result<W> {
        if self.echantillons.is_empty() {
            return Err(NdError::Protocol(
                "enregistrement MP4 vide : aucune image écrite".into(),
            ));
        }
        let (Some(sps), Some(pps)) = (self.sps.take(), self.pps.take()) else {
            return Err(NdError::Protocol(
                "SPS/PPS jamais rencontrés : le flux H.264 n'est pas décodable".into(),
            ));
        };

        // Corrige la taille réelle de mdat (en-tête long : 16 octets).
        self.sortie.seek(SeekFrom::Start(self.offset_mdat + 8))?;
        self.sortie
            .write_all(&(16 + self.charge_mdat).to_be_bytes())?;
        self.sortie.seek(SeekFrom::End(0))?;

        let moov = construire_moov(
            &self.metadata,
            &self.echantillons,
            &sps,
            &pps,
            self.offset_mdat + 16,
        )?;
        self.sortie.write_all(&moov)?;
        self.sortie.flush()?;

        // Boîte d'intégrité : BLAKE3 de tout ce qui précède, relu depuis la
        // sortie elle-même (les octets corrigés de mdat sont donc couverts).
        let taille_fichier = self.sortie.seek(SeekFrom::End(0))?;
        self.sortie.seek(SeekFrom::Start(0))?;
        let hachage = hacher_prefixe(&mut self.sortie, taille_fichier)?;
        let mut corps = Vec::with_capacity(36);
        corps.extend_from_slice(MARQUE_NDB3);
        corps.extend_from_slice(hachage.as_bytes());
        self.sortie.seek(SeekFrom::End(0))?;
        self.sortie.write_all(&boite(b"free", &corps)?)?;
        self.sortie.flush()?;
        Ok(self.sortie)
    }
}

/// BLAKE3 des `limite` premiers octets d'une source positionnée au début.
fn hacher_prefixe<R: Read>(source: &mut R, limite: u64) -> Result<blake3::Hash> {
    let mut hachoir = blake3::Hasher::new();
    let mut tampon = vec![0u8; 8192];
    let mut restant = limite;
    while restant > 0 {
        let pas = restant.min(8192) as usize;
        source.read_exact(&mut tampon[..pas])?;
        hachoir.update(&tampon[..pas]);
        restant -= pas as u64;
    }
    Ok(hachoir.finalize())
}

/// Construit la boîte `moov` complète à partir des échantillons réels.
fn construire_moov(
    metadata: &RecordingMetadata,
    echantillons: &[EchantillonEcrit],
    sps: &[u8],
    pps: &[u8],
    offset_premier_echantillon: u64,
) -> Result<Vec<u8>> {
    // Horodatages en 90 kHz et durées (stts). La dernière image reçoit la
    // durée de l'avant-dernière (ou 1/fps s'il n'y en a qu'une).
    let tics: Vec<u64> = echantillons
        .iter()
        .map(|e| us_vers_90khz(e.timestamp_us))
        .collect();
    let mut deltas: Vec<u32> = Vec::with_capacity(tics.len());
    for paire in tics.windows(2) {
        let delta = u32::try_from(paire[1] - paire[0]).map_err(|_| {
            NdError::Protocol("écart d'horodatage entre deux images trop grand".into())
        })?;
        deltas.push(delta);
    }
    let derniere_duree = deltas
        .last()
        .copied()
        .unwrap_or(MEDIA_TIMESCALE / metadata.fps.max(1));
    deltas.push(derniere_duree);
    let duree_media: u64 = tics.last().copied().unwrap_or(0) - tics.first().copied().unwrap_or(0)
        + u64::from(derniere_duree);
    let duree_film = duree_media * u64::from(FILM_TIMESCALE) / u64::from(MEDIA_TIMESCALE);
    let creation_mp4 = metadata.start_unix_ms / 1000 + EPOQUE_MP4_VERS_UNIX;

    // --- mvhd (version 1 : durées 64 bits, aucun plafond de durée) ---------
    let mut mvhd = Vec::with_capacity(108);
    mvhd.extend_from_slice(&creation_mp4.to_be_bytes());
    mvhd.extend_from_slice(&creation_mp4.to_be_bytes());
    mvhd.extend_from_slice(&FILM_TIMESCALE.to_be_bytes());
    mvhd.extend_from_slice(&duree_film.to_be_bytes());
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // vitesse 1.0
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    mvhd.extend_from_slice(&[0u8; 10]); // réservé
    for valeur in MATRICE_IDENTITE {
        mvhd.extend_from_slice(&valeur.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0u8; 24]); // pré-défini
    mvhd.extend_from_slice(&2u32.to_be_bytes()); // prochain identifiant de piste
    let mvhd = boite_pleine(b"mvhd", 1, 0, &mvhd)?;

    // --- tkhd -------------------------------------------------------------
    let mut tkhd = Vec::with_capacity(92);
    tkhd.extend_from_slice(&creation_mp4.to_be_bytes());
    tkhd.extend_from_slice(&creation_mp4.to_be_bytes());
    tkhd.extend_from_slice(&1u32.to_be_bytes()); // piste 1
    tkhd.extend_from_slice(&[0u8; 4]); // réservé
    tkhd.extend_from_slice(&duree_film.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 8]); // réservé
    tkhd.extend_from_slice(&[0u8; 2]); // couche
    tkhd.extend_from_slice(&[0u8; 2]); // groupe alternatif
    tkhd.extend_from_slice(&[0u8; 2]); // volume (piste vidéo : 0)
    tkhd.extend_from_slice(&[0u8; 2]); // réservé
    for valeur in MATRICE_IDENTITE {
        tkhd.extend_from_slice(&valeur.to_be_bytes());
    }
    tkhd.extend_from_slice(&(metadata.width << 16).to_be_bytes()); // 16.16
    tkhd.extend_from_slice(&(metadata.height << 16).to_be_bytes());
    let tkhd = boite_pleine(b"tkhd", 1, 3, &tkhd)?; // activée + dans le film

    // --- mdhd -------------------------------------------------------------
    let mut mdhd = Vec::with_capacity(32);
    mdhd.extend_from_slice(&creation_mp4.to_be_bytes());
    mdhd.extend_from_slice(&creation_mp4.to_be_bytes());
    mdhd.extend_from_slice(&MEDIA_TIMESCALE.to_be_bytes());
    mdhd.extend_from_slice(&duree_media.to_be_bytes());
    mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // langue « und »
    mdhd.extend_from_slice(&[0u8; 2]);
    let mdhd = boite_pleine(b"mdhd", 1, 0, &mdhd)?;

    // --- hdlr -------------------------------------------------------------
    let mut hdlr = Vec::with_capacity(36);
    hdlr.extend_from_slice(&[0u8; 4]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"NovaDesk Video\0");
    let hdlr = boite_pleine(b"hdlr", 0, 0, &hdlr)?;

    // --- stbl -------------------------------------------------------------
    let stsd = construire_stsd(metadata, sps, pps)?;
    let stts = construire_stts(&deltas)?;
    let stss = construire_stss(echantillons)?;
    let stsc = {
        // Un seul « chunk » contenant tous les échantillons, contigus dans mdat.
        let mut corps = Vec::with_capacity(16);
        corps.extend_from_slice(&1u32.to_be_bytes()); // une entrée
        corps.extend_from_slice(&1u32.to_be_bytes()); // premier chunk : 1
        corps.extend_from_slice(&(echantillons.len() as u32).to_be_bytes());
        corps.extend_from_slice(&1u32.to_be_bytes()); // description 1
        boite_pleine(b"stsc", 0, 0, &corps)?
    };
    let stsz = {
        let mut corps = Vec::with_capacity(8 + echantillons.len() * 4);
        corps.extend_from_slice(&0u32.to_be_bytes()); // tailles individuelles
        corps.extend_from_slice(&(echantillons.len() as u32).to_be_bytes());
        for echantillon in echantillons {
            corps.extend_from_slice(&echantillon.taille.to_be_bytes());
        }
        boite_pleine(b"stsz", 0, 0, &corps)?
    };
    let stco = {
        let offset = u32::try_from(offset_premier_echantillon)
            .map_err(|_| NdError::Protocol("offset du premier échantillon hors u32".into()))?;
        let mut corps = Vec::with_capacity(8);
        corps.extend_from_slice(&1u32.to_be_bytes());
        corps.extend_from_slice(&offset.to_be_bytes());
        boite_pleine(b"stco", 0, 0, &corps)?
    };
    let stbl = boite(b"stbl", &[stsd, stts, stss, stsc, stsz, stco].concat())?;

    // --- minf / mdia / trak / moov -----------------------------------------
    let vmhd = boite_pleine(b"vmhd", 0, 1, &[0u8; 8])?;
    let dref_url = boite_pleine(b"url ", 0, 1, &[])?; // média dans ce fichier
    let dref = {
        let mut corps = Vec::with_capacity(4 + dref_url.len());
        corps.extend_from_slice(&1u32.to_be_bytes());
        corps.extend_from_slice(&dref_url);
        boite_pleine(b"dref", 0, 0, &corps)?
    };
    let dinf = boite(b"dinf", &dref)?;
    let minf = boite(b"minf", &[vmhd, dinf, stbl].concat())?;
    let mdia = boite(b"mdia", &[mdhd, hdlr, minf].concat())?;
    let trak = boite(b"trak", &[tkhd, mdia].concat())?;
    boite(b"moov", &[mvhd, trak].concat())
}

/// Boîte `stsd` : entrée `avc1` + `avcC` (SPS/PPS réels du flux).
fn construire_stsd(metadata: &RecordingMetadata, sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    let largeur = u16::try_from(metadata.width)
        .map_err(|_| NdError::Protocol("largeur vidéo hors u16 (avc1)".into()))?;
    let hauteur = u16::try_from(metadata.height)
        .map_err(|_| NdError::Protocol("hauteur vidéo hors u16 (avc1)".into()))?;
    let sps_len = u16::try_from(sps.len())
        .map_err(|_| NdError::Protocol("SPS trop long pour avcC".into()))?;
    let pps_len = u16::try_from(pps.len())
        .map_err(|_| NdError::Protocol("PPS trop long pour avcC".into()))?;

    let mut avcc = Vec::with_capacity(11 + sps.len() + pps.len());
    avcc.push(1); // version de configuration
    avcc.push(sps[1]); // profil (déjà vérifié : SPS ≥ 4 octets)
    avcc.push(sps[2]); // compatibilité de profil
    avcc.push(sps[3]); // niveau
    avcc.push(0xFC | 0x03); // longueurs de NAL sur 4 octets
    avcc.push(0xE0 | 0x01); // un SPS
    avcc.extend_from_slice(&sps_len.to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1); // un PPS
    avcc.extend_from_slice(&pps_len.to_be_bytes());
    avcc.extend_from_slice(pps);
    let avcc = boite(b"avcC", &avcc)?;

    let mut avc1 = Vec::with_capacity(78 + avcc.len());
    avc1.extend_from_slice(&[0u8; 6]); // réservé
    avc1.extend_from_slice(&1u16.to_be_bytes()); // référence de données 1
    avc1.extend_from_slice(&[0u8; 16]); // pré-défini / réservé
    avc1.extend_from_slice(&largeur.to_be_bytes());
    avc1.extend_from_slice(&hauteur.to_be_bytes());
    avc1.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
    avc1.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    avc1.extend_from_slice(&[0u8; 4]); // réservé
    avc1.extend_from_slice(&1u16.to_be_bytes()); // une image par échantillon
    let mut nom_compresseur = [0u8; 32];
    nom_compresseur[0] = 8;
    nom_compresseur[1..9].copy_from_slice(b"NovaDesk");
    avc1.extend_from_slice(&nom_compresseur);
    avc1.extend_from_slice(&0x0018u16.to_be_bytes()); // profondeur 24 bits
    avc1.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pré-défini
    avc1.extend_from_slice(&avcc);
    let avc1 = boite(b"avc1", &avc1)?;

    let mut corps = Vec::with_capacity(4 + avc1.len());
    corps.extend_from_slice(&1u32.to_be_bytes());
    corps.extend_from_slice(&avc1);
    boite_pleine(b"stsd", 0, 0, &corps)
}

/// Boîte `stts` : durées des échantillons, encodées par plages.
fn construire_stts(deltas: &[u32]) -> Result<Vec<u8>> {
    let mut plages: Vec<(u32, u32)> = Vec::new();
    for &delta in deltas {
        match plages.last_mut() {
            Some((compte, valeur)) if *valeur == delta => *compte += 1,
            _ => plages.push((1, delta)),
        }
    }
    let mut corps = Vec::with_capacity(4 + plages.len() * 8);
    corps.extend_from_slice(&(plages.len() as u32).to_be_bytes());
    for (compte, valeur) in plages {
        corps.extend_from_slice(&compte.to_be_bytes());
        corps.extend_from_slice(&valeur.to_be_bytes());
    }
    boite_pleine(b"stts", 0, 0, &corps)
}

/// Boîte `stss` : numéros (à partir de 1) des échantillons image-clé.
fn construire_stss(echantillons: &[EchantillonEcrit]) -> Result<Vec<u8>> {
    let cles: Vec<u32> = echantillons
        .iter()
        .enumerate()
        .filter(|(_, e)| e.keyframe)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    let mut corps = Vec::with_capacity(4 + cles.len() * 4);
    corps.extend_from_slice(&(cles.len() as u32).to_be_bytes());
    for numero in cles {
        corps.extend_from_slice(&numero.to_be_bytes());
    }
    boite_pleine(b"stss", 0, 0, &corps)
}

// ---------------------------------------------------------------------------
// Conversion .ndr → MP4
// ---------------------------------------------------------------------------

/// Convertit un enregistrement `.ndr` v2 (conteneur [`super::IndexedRecorder`])
/// en MP4 rejouable. Les métadonnées (dimensions, cadence, codec) viennent de
/// l'en-tête `.ndr` ; le codec doit être du H.264 et la première image une
/// image-clé (garanti si l'enregistrement a démarré sur une image-clé forcée).
///
/// Rend le flux de sortie clos (le MP4 complet, hachage BLAKE3 inclus).
pub fn ndr_to_mp4<R: Read, W: Read + Write + Seek>(
    ndr: &mut SessionReader<R>,
    sortie: W,
) -> Result<W> {
    let metadata = ndr.metadata().cloned().ok_or_else(|| {
        NdError::Protocol(
            "conversion MP4 : enregistrement v1 sans métadonnées (dimensions/cadence inconnues)"
                .into(),
        )
    })?;
    let mut muxeur = Mp4Muxer::new(sortie, metadata)?;
    while let Some(RecordedFrame {
        timestamp_us,
        keyframe,
        data,
    }) = ndr.next_frame()?
    {
        muxeur.record(timestamp_us, keyframe, &data)?;
    }
    muxeur.finish()
}

// ---------------------------------------------------------------------------
// Lecture : Mp4Reader
// ---------------------------------------------------------------------------

/// Un échantillon de la table (position dans `mdat` + datation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InfoEchantillon {
    offset: u64,
    taille: u32,
    tics: u64,
    keyframe: bool,
}

/// Une image relue depuis un MP4 : données au format AVCC (préfixes de
/// longueur 4 octets), à convertir via [`Mp4Reader::sample_annexb`] pour un
/// décodeur Annex B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp4Sample {
    /// Horodatage de présentation, en microsecondes (converti du 90 kHz ;
    /// écart d'arrondi ≤ 1 µs par rapport à l'horodatage d'origine).
    pub timestamp_us: u64,
    /// Vrai si l'échantillon est une image-clé (table `stss`).
    pub keyframe: bool,
    /// Données AVCC : `[u32 BE longueur][NAL]…`.
    pub data: Vec<u8>,
}

/// Rapport de [`Mp4Reader::validate`] : ce que contient réellement le fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp4ValidationReport {
    /// Nombre d'images (échantillons) de la piste vidéo.
    pub frames: u64,
    /// Nombre d'images-clés (entrées `stss`).
    pub keyframes: u64,
    /// Durée de la piste, en microsecondes.
    pub duration_us: u64,
    /// Dimensions déclarées par l'entrée `avc1`.
    pub width: u32,
    /// Hauteur déclarée par l'entrée `avc1`.
    pub height: u32,
    /// Nombre total d'unités NAL parcourues dans `mdat`.
    pub nals: u64,
    /// Codec au format RFC 6381 (« avc1.PPCCNN », dérivé de l'`avcC` réelle).
    pub codec: String,
    /// Vrai si la boîte d'intégrité `NDB3` était présente et le BLAKE3 exact.
    pub hash_verified: bool,
}

/// Lecteur/validateur de MP4 produits par [`Mp4Muxer`] (une piste vidéo AVC).
///
/// À l'ouverture, tout `moov` est analysé et la table des échantillons
/// reconstruite (stsc/stco/stsz/stts/stss) ; la lecture des échantillons
/// ([`Mp4Reader::next_sample`]) va ensuite chercher les octets dans `mdat`.
#[derive(Debug)]
pub struct Mp4Reader<R: Read + Seek> {
    source: R,
    largeur: u32,
    hauteur: u32,
    duree_media: u64,
    profil_avcc: [u8; 3],
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    echantillons: Vec<InfoEchantillon>,
    /// Charge utile de `mdat` : (offset absolu, taille en octets).
    mdat: (u64, u64),
    /// Hachage BLAKE3 déclaré et position de la boîte qui le porte (fin de la
    /// zone couverte), si la boîte `free NDB3` est présente.
    hachage: Option<([u8; 32], u64)>,
    prochain: usize,
}

impl<R: Read + Seek> Mp4Reader<R> {
    /// Ouvre et analyse le fichier : boîtes de premier niveau, `moov`, table
    /// des échantillons. Refuse ce qui n'est pas un MP4 vidéo AVC exploitable.
    pub fn new(mut source: R) -> Result<Self> {
        let taille_fichier = source.seek(SeekFrom::End(0))?;
        source.seek(SeekFrom::Start(0))?;

        let mut ftyp_vu = false;
        let mut mdat: Option<(u64, u64)> = None;
        let mut moov: Option<Vec<u8>> = None;
        let mut hachage: Option<([u8; 32], u64)> = None;

        let mut position = 0u64;
        while position < taille_fichier {
            let (nom, charge_offset, charge_taille) =
                lire_entete_boite(&mut source, position, taille_fichier)?;
            match &nom {
                b"ftyp" => {
                    if position != 0 {
                        return Err(NdError::Protocol(
                            "boîte ftyp absente en tête de fichier".into(),
                        ));
                    }
                    ftyp_vu = true;
                }
                b"mdat" => {
                    if mdat.is_some() {
                        return Err(NdError::Protocol(
                            "plusieurs boîtes mdat : conteneur non géré".into(),
                        ));
                    }
                    mdat = Some((charge_offset, charge_taille));
                }
                b"moov" => {
                    if charge_taille > PLAFOND_BOITE_MEMOIRE {
                        return Err(NdError::Protocol(
                            "boîte moov démesurée : fichier corrompu ?".into(),
                        ));
                    }
                    let mut octets = vec![0u8; charge_taille as usize];
                    source.seek(SeekFrom::Start(charge_offset))?;
                    source.read_exact(&mut octets)?;
                    moov = Some(octets);
                }
                b"free" if charge_taille == TAILLE_BOITE_HACHAGE - 8 => {
                    let mut corps = [0u8; 36];
                    source.seek(SeekFrom::Start(charge_offset))?;
                    source.read_exact(&mut corps)?;
                    if &corps[..4] == MARQUE_NDB3 {
                        let mut h = [0u8; 32];
                        h.copy_from_slice(&corps[4..]);
                        // Le hachage couvre tout ce qui précède la boîte.
                        hachage = Some((h, position));
                    }
                }
                _ => {} // boîte inconnue ou sans intérêt : sautée
            }
            position = charge_offset + charge_taille;
            source.seek(SeekFrom::Start(position))?;
        }
        if !ftyp_vu {
            return Err(NdError::Protocol(
                "pas de boîte ftyp : ce fichier n'est pas un MP4".into(),
            ));
        }
        let mdat = mdat
            .ok_or_else(|| NdError::Protocol("pas de boîte mdat : aucune donnée vidéo".into()))?;
        let moov =
            moov.ok_or_else(|| NdError::Protocol("pas de boîte moov : MP4 sans index".into()))?;

        let piste = analyser_moov(&moov)?;
        Ok(Mp4Reader {
            source,
            largeur: piste.largeur,
            hauteur: piste.hauteur,
            duree_media: piste.duree_media,
            profil_avcc: piste.profil_avcc,
            sps: piste.sps,
            pps: piste.pps,
            echantillons: piste.echantillons,
            mdat,
            hachage,
            prochain: 0,
        })
    }

    /// Largeur de la vidéo (entrée `avc1`).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.largeur
    }

    /// Hauteur de la vidéo (entrée `avc1`).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.hauteur
    }

    /// Nombre d'images de la piste vidéo.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.echantillons.len() as u64
    }

    /// Durée de la piste, en microsecondes.
    #[must_use]
    pub fn duration_us(&self) -> u64 {
        tics_vers_us(self.duree_media)
    }

    /// Codec au format RFC 6381 (« avc1.PPCCNN »), dérivé de l'`avcC` réelle.
    #[must_use]
    pub fn codec_rfc6381(&self) -> String {
        format!(
            "avc1.{:02X}{:02X}{:02X}",
            self.profil_avcc[0], self.profil_avcc[1], self.profil_avcc[2]
        )
    }

    /// SPS et PPS de l'`avcC`, au format Annex B (préfixés `00 00 00 01`) —
    /// à donner au décodeur avant (ou avec) la première image-clé.
    #[must_use]
    pub fn parameter_sets_annexb(&self) -> Vec<u8> {
        let mut sortie = Vec::new();
        for nal in self.sps.iter().chain(self.pps.iter()) {
            sortie.extend_from_slice(&[0, 0, 0, 1]);
            sortie.extend_from_slice(nal);
        }
        sortie
    }

    /// Échantillon suivant (données AVCC), ou `None` en fin de piste.
    pub fn next_sample(&mut self) -> Result<Option<Mp4Sample>> {
        if self.prochain >= self.echantillons.len() {
            return Ok(None);
        }
        let rang = self.prochain;
        self.prochain += 1;
        self.lire_echantillon(rang).map(Some)
    }

    /// Lit l'échantillon de rang donné (données AVCC) en allant chercher ses
    /// octets dans `mdat`, sans toucher au curseur de [`Mp4Reader::next_sample`].
    fn lire_echantillon(&mut self, rang: usize) -> Result<Mp4Sample> {
        let info = self.echantillons[rang];
        let mut data = vec![0u8; info.taille as usize];
        self.source.seek(SeekFrom::Start(info.offset))?;
        self.source.read_exact(&mut data)?;
        Ok(Mp4Sample {
            timestamp_us: tics_vers_us(info.tics),
            keyframe: info.keyframe,
            data,
        })
    }

    /// Cadence nominale, en images par seconde, dérivée de la durée média
    /// (tics 90 kHz) et du nombre d'images. `0` pour une piste vide.
    #[must_use]
    pub fn fps(&self) -> u32 {
        if self.duree_media == 0 || self.echantillons.is_empty() {
            return 0;
        }
        let frames = self.echantillons.len() as u128;
        ((frames * u128::from(MEDIA_TIMESCALE) + u128::from(self.duree_media) / 2)
            / u128::from(self.duree_media)) as u32
    }

    /// Extrait **tous** les échantillons de la piste, dans l'ordre, données
    /// converties en **H.264 Annex B** prêtes à décoder ([`EncodedSample`] :
    /// codes de départ `00 00 00 01`, SPS/PPS réinjectés en tête de chaque
    /// image-clé). Réutilise la table d'échantillons reconstruite à l'ouverture
    /// (stsc/stco/stsz/stts/stss) ; ne perturbe pas le curseur de
    /// [`Mp4Reader::next_sample`].
    pub fn samples(&mut self) -> Result<Vec<EncodedSample>> {
        let nombre = self.echantillons.len();
        let mut sortie = Vec::with_capacity(nombre);
        for rang in 0..nombre {
            let echantillon = self.lire_echantillon(rang)?;
            let data = self.sample_annexb(&echantillon)?;
            sortie.push(EncodedSample {
                timestamp_us: echantillon.timestamp_us,
                is_keyframe: echantillon.keyframe,
                data,
            });
        }
        Ok(sortie)
    }

    /// Image-clé la plus proche **avant** (ou à) `timestamp_us`, prête à
    /// décoder ([`EncodedSample`] Annex B) — point de départ correct d'un
    /// « seek ». Si `timestamp_us` précède la première image-clé, rend
    /// celle-ci ; `None` si la piste ne contient aucune image-clé.
    pub fn sample_at(&mut self, timestamp_us: u64) -> Result<Option<EncodedSample>> {
        let Some(rang) = self.rang_cle_pour(timestamp_us) else {
            return Ok(None);
        };
        let echantillon = self.lire_echantillon(rang)?;
        let data = self.sample_annexb(&echantillon)?;
        Ok(Some(EncodedSample {
            timestamp_us: echantillon.timestamp_us,
            is_keyframe: echantillon.keyframe,
            data,
        }))
    }

    /// Rang de l'image-clé à retenir pour un « seek » à `timestamp_us` : la
    /// dernière image-clé d'horodatage ≤ cible, ou la première image-clé si la
    /// cible les précède toutes. `None` s'il n'y a aucune image-clé. Les
    /// horodatages étant croissants, on s'arrête à la première clé au-delà.
    fn rang_cle_pour(&self, timestamp_us: u64) -> Option<usize> {
        let mut choisi = None;
        let mut premiere = None;
        for (rang, echantillon) in self.echantillons.iter().enumerate() {
            if !echantillon.keyframe {
                continue;
            }
            premiere.get_or_insert(rang);
            if tics_vers_us(echantillon.tics) <= timestamp_us {
                choisi = Some(rang);
            } else {
                break;
            }
        }
        choisi.or(premiere)
    }

    /// Rembobine la lecture des échantillons au début de la piste.
    pub fn rewind(&mut self) {
        self.prochain = 0;
    }

    /// Convertit un échantillon AVCC en unité Annex B prête à décoder : codes
    /// de départ `00 00 00 01`, précédés des SPS/PPS si c'est une image-clé
    /// (ils ont été déportés dans `avcC` au mux).
    pub fn sample_annexb(&self, sample: &Mp4Sample) -> Result<Vec<u8>> {
        let mut sortie = if sample.keyframe {
            self.parameter_sets_annexb()
        } else {
            Vec::new()
        };
        sortie.reserve(sample.data.len() + 8);
        parcourir_avcc(&sample.data, |nal| {
            sortie.extend_from_slice(&[0, 0, 0, 1]);
            sortie.extend_from_slice(nal);
        })?;
        Ok(sortie)
    }

    /// Vérification complète du conteneur :
    /// - chaque échantillon est bien dans les bornes de `mdat` ;
    /// - la structure AVCC de chaque échantillon est exacte (les préfixes de
    ///   longueur couvrent exactement l'échantillon, aucune NAL vide, bit
    ///   interdit à zéro) ;
    /// - la première image est une image-clé et `stss` est cohérente ;
    /// - l'`avcC` porte au moins un SPS et un PPS ;
    /// - le BLAKE3 de la boîte `NDB3` correspond au fichier, si présent.
    ///
    /// Laisse la lecture rembobinée au premier échantillon.
    pub fn validate(&mut self) -> Result<Mp4ValidationReport> {
        if self.sps.is_empty() || self.pps.is_empty() {
            return Err(NdError::Protocol(
                "avcC sans SPS ou sans PPS : vidéo non décodable".into(),
            ));
        }
        let (mdat_debut, mdat_taille) = self.mdat;
        let mut cles = 0u64;
        let mut nals = 0u64;
        let premier_est_cle = self.echantillons.first().is_some_and(|e| e.keyframe);
        if !self.echantillons.is_empty() && !premier_est_cle {
            return Err(NdError::Protocol(
                "le premier échantillon n'est pas une image-clé : flux non décodable du début"
                    .into(),
            ));
        }
        self.rewind();
        for rang in 0..self.echantillons.len() {
            let info = self.echantillons[rang];
            let fin = info.offset + u64::from(info.taille);
            if info.offset < mdat_debut || fin > mdat_debut + mdat_taille {
                return Err(NdError::Protocol(format!(
                    "échantillon {rang} hors des bornes de mdat"
                )));
            }
            let echantillon = self.next_sample()?.ok_or_else(|| {
                NdError::Protocol("table d'échantillons incohérente avec la lecture".into())
            })?;
            if echantillon.keyframe {
                cles += 1;
            }
            parcourir_avcc(&echantillon.data, |_| {
                nals += 1;
            })?;
        }
        // BLAKE3 : recalculé sur tous les octets couverts par la boîte NDB3.
        let hash_verified = if let Some((attendu, limite)) = self.hachage {
            self.source.seek(SeekFrom::Start(0))?;
            let calcule = hacher_prefixe(&mut self.source, limite)?;
            if calcule != blake3::Hash::from(attendu) {
                return Err(NdError::Protocol(
                    "hachage BLAKE3 invalide : enregistrement MP4 corrompu".into(),
                ));
            }
            true
        } else {
            false
        };
        self.rewind();
        Ok(Mp4ValidationReport {
            frames: self.frames(),
            keyframes: cles,
            duration_us: self.duration_us(),
            width: self.largeur,
            height: self.hauteur,
            nals,
            codec: self.codec_rfc6381(),
            hash_verified,
        })
    }
}

/// Parcourt les NAL d'un échantillon AVCC (`[u32 BE longueur][NAL]…`) et les
/// passe à `visite`. Erreur si les longueurs ne tombent pas exactement sur la
/// fin, si une NAL est vide, ou si le bit interdit d'une NAL est levé.
fn parcourir_avcc(data: &[u8], mut visite: impl FnMut(&[u8])) -> Result<()> {
    let mut position = 0usize;
    while position < data.len() {
        let Some(entete) = data.get(position..position + 4) else {
            return Err(NdError::Protocol(
                "échantillon AVCC tronqué (préfixe de longueur incomplet)".into(),
            ));
        };
        let longueur = u32::from_be_bytes(entete.try_into().expect("4 octets")) as usize;
        if longueur == 0 {
            return Err(NdError::Protocol("NAL vide dans un échantillon".into()));
        }
        let Some(nal) = data.get(position + 4..position + 4 + longueur) else {
            return Err(NdError::Protocol(
                "échantillon AVCC tronqué (NAL au-delà de l'échantillon)".into(),
            ));
        };
        if nal[0] & 0x80 != 0 {
            return Err(NdError::Protocol(
                "bit interdit levé en tête de NAL : flux corrompu".into(),
            ));
        }
        visite(nal);
        position += 4 + longueur;
    }
    Ok(())
}

/// Lit l'en-tête d'une boîte à `position` : rend (type, offset de la charge,
/// taille de la charge). Gère la forme longue (`taille == 1` + 64 bits) et la
/// forme « jusqu'à la fin du fichier » (`taille == 0`).
fn lire_entete_boite<R: Read + Seek>(
    source: &mut R,
    position: u64,
    taille_fichier: u64,
) -> Result<([u8; 4], u64, u64)> {
    source.seek(SeekFrom::Start(position))?;
    let mut entete = [0u8; 8];
    source.read_exact(&mut entete)?;
    let taille32 = u32::from_be_bytes(entete[..4].try_into().expect("4 octets"));
    let nom: [u8; 4] = entete[4..].try_into().expect("4 octets");
    let (taille_entete, taille_boite) = match taille32 {
        0 => (8u64, taille_fichier - position), // jusqu'à la fin du fichier
        1 => {
            let mut grand = [0u8; 8];
            source.read_exact(&mut grand)?;
            (16u64, u64::from_be_bytes(grand))
        }
        n => (8u64, u64::from(n)),
    };
    if taille_boite < taille_entete || position + taille_boite > taille_fichier {
        return Err(NdError::Protocol(format!(
            "boîte {} de taille invalide ou tronquée",
            String::from_utf8_lossy(&nom)
        )));
    }
    Ok((nom, position + taille_entete, taille_boite - taille_entete))
}

/// Ce que l'analyse de `moov` doit produire pour construire le lecteur.
struct PisteVideo {
    largeur: u32,
    hauteur: u32,
    duree_media: u64,
    profil_avcc: [u8; 3],
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    echantillons: Vec<InfoEchantillon>,
}

/// Curseur de boîtes sur une tranche mémoire (contenu d'une boîte parente).
struct CurseurBoites<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> CurseurBoites<'a> {
    fn new(data: &'a [u8]) -> Self {
        CurseurBoites { data, position: 0 }
    }

    /// Boîte suivante : (type, corps), ou `None` en fin de tranche.
    fn suivante(&mut self) -> Result<Option<([u8; 4], &'a [u8])>> {
        if self.position == self.data.len() {
            return Ok(None);
        }
        let reste = &self.data[self.position..];
        if reste.len() < 8 {
            return Err(NdError::Protocol("boîte tronquée dans moov".into()));
        }
        let taille32 = u32::from_be_bytes(reste[..4].try_into().expect("4 octets"));
        let nom: [u8; 4] = reste[4..8].try_into().expect("4 octets");
        let (entete, taille) = match taille32 {
            0 => (8usize, reste.len()),
            1 => {
                if reste.len() < 16 {
                    return Err(NdError::Protocol("boîte longue tronquée dans moov".into()));
                }
                let grand = u64::from_be_bytes(reste[8..16].try_into().expect("8 octets"));
                let grand = usize::try_from(grand)
                    .map_err(|_| NdError::Protocol("boîte démesurée dans moov".into()))?;
                (16usize, grand)
            }
            n => (8usize, n as usize),
        };
        if taille < entete || taille > reste.len() {
            return Err(NdError::Protocol(format!(
                "boîte {} de taille invalide dans moov",
                String::from_utf8_lossy(&nom)
            )));
        }
        self.position += taille;
        Ok(Some((nom, &reste[entete..taille])))
    }

    /// Cherche la première boîte nommée `nom` dans la tranche.
    fn trouver(data: &'a [u8], nom: &[u8; 4]) -> Result<Option<&'a [u8]>> {
        let mut curseur = CurseurBoites::new(data);
        while let Some((n, corps)) = curseur.suivante()? {
            if &n == nom {
                return Ok(Some(corps));
            }
        }
        Ok(None)
    }
}

/// Lecture d'entiers grands-boutistes dans une tranche, avec bornes vérifiées.
fn u32_be(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .map(|o| u32::from_be_bytes(o.try_into().expect("4 octets")))
        .ok_or_else(|| NdError::Protocol("tranche moov tronquée (u32)".into()))
}

fn u64_be(data: &[u8], offset: usize) -> Result<u64> {
    data.get(offset..offset + 8)
        .map(|o| u64::from_be_bytes(o.try_into().expect("8 octets")))
        .ok_or_else(|| NdError::Protocol("tranche moov tronquée (u64)".into()))
}

/// Corps d'une boîte pleine : (version, corps après version/drapeaux).
fn corps_boite_pleine(data: &[u8]) -> Result<(u8, &[u8])> {
    if data.len() < 4 {
        return Err(NdError::Protocol("boîte pleine tronquée".into()));
    }
    Ok((data[0], &data[4..]))
}

/// Analyse `moov` : retrouve la première piste vidéo (`hdlr` = `vide`) et
/// reconstruit sa table d'échantillons.
fn analyser_moov(moov: &[u8]) -> Result<PisteVideo> {
    let mut curseur = CurseurBoites::new(moov);
    while let Some((nom, corps)) = curseur.suivante()? {
        if &nom != b"trak" {
            continue;
        }
        let Some(mdia) = CurseurBoites::trouver(corps, b"mdia")? else {
            continue;
        };
        let Some(hdlr) = CurseurBoites::trouver(mdia, b"hdlr")? else {
            continue;
        };
        let (_, hdlr_corps) = corps_boite_pleine(hdlr)?;
        if hdlr_corps.get(4..8) != Some(b"vide".as_slice()) {
            continue; // pas une piste vidéo
        }
        return analyser_piste_video(mdia);
    }
    Err(NdError::Protocol(
        "aucune piste vidéo dans ce MP4 (hdlr « vide » introuvable)".into(),
    ))
}

/// Analyse la boîte `mdia` d'une piste vidéo : mdhd + stbl complètes.
fn analyser_piste_video(mdia: &[u8]) -> Result<PisteVideo> {
    let mdhd = CurseurBoites::trouver(mdia, b"mdhd")?
        .ok_or_else(|| NdError::Protocol("mdhd absente".into()))?;
    let (version, corps) = corps_boite_pleine(mdhd)?;
    let (echelle, duree) = if version == 1 {
        (u32_be(corps, 16)?, u64_be(corps, 20)?)
    } else {
        (u32_be(corps, 8)?, u64::from(u32_be(corps, 12)?))
    };
    if echelle == 0 {
        return Err(NdError::Protocol("échelle de temps média nulle".into()));
    }
    // Durée ramenée en tics 90 kHz quel que soit le timescale du fichier.
    let duree_media =
        (u128::from(duree) * u128::from(MEDIA_TIMESCALE) / u128::from(echelle)) as u64;

    let minf = CurseurBoites::trouver(mdia, b"minf")?
        .ok_or_else(|| NdError::Protocol("minf absente".into()))?;
    let stbl = CurseurBoites::trouver(minf, b"stbl")?
        .ok_or_else(|| NdError::Protocol("stbl absente".into()))?;

    // --- stsd → avc1 → avcC -------------------------------------------------
    let stsd = CurseurBoites::trouver(stbl, b"stsd")?
        .ok_or_else(|| NdError::Protocol("stsd absente".into()))?;
    let (_, stsd_corps) = corps_boite_pleine(stsd)?;
    if u32_be(stsd_corps, 0)? == 0 {
        return Err(NdError::Protocol("stsd sans entrée".into()));
    }
    let avc1 = CurseurBoites::trouver(&stsd_corps[4..], b"avc1")?
        .ok_or_else(|| NdError::Protocol("entrée avc1 absente : piste non AVC".into()))?;
    if avc1.len() < 78 {
        return Err(NdError::Protocol("entrée avc1 tronquée".into()));
    }
    let largeur = u32::from(u16::from_be_bytes(avc1[24..26].try_into().expect("2")));
    let hauteur = u32::from(u16::from_be_bytes(avc1[26..28].try_into().expect("2")));
    let avcc = CurseurBoites::trouver(&avc1[78..], b"avcC")?
        .ok_or_else(|| NdError::Protocol("boîte avcC absente".into()))?;
    let parametres = analyser_avcc(avcc)?;

    // --- tables d'échantillons ----------------------------------------------
    let tailles = lire_stsz(stbl)?;
    let tics = lire_stts(stbl, tailles.len())?;
    let cles = lire_stss(stbl)?;
    let offsets = lire_offsets(stbl, &tailles)?;

    let mut echantillons = Vec::with_capacity(tailles.len());
    for (rang, (&taille, (&offset, &tic))) in tailles
        .iter()
        .zip(offsets.iter().zip(tics.iter()))
        .enumerate()
    {
        let numero = rang as u32 + 1;
        echantillons.push(InfoEchantillon {
            offset,
            taille,
            tics: tic,
            keyframe: cles.as_ref().is_none_or(|liste| liste.contains(&numero)),
        });
    }
    Ok(PisteVideo {
        largeur,
        hauteur,
        duree_media,
        profil_avcc: parametres.profil,
        sps: parametres.sps,
        pps: parametres.pps,
        echantillons,
    })
}

/// Contenu utile d'une boîte `avcC` : identification du profil et jeux de
/// paramètres nécessaires au décodage.
struct ParametresAvcc {
    /// Profil, compatibilité, niveau (trois premiers octets après la version).
    profil: [u8; 3],
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

/// Analyse `avcC` : profil + SPS + PPS. Seuls les préfixes de longueur sur
/// 4 octets sont gérés (ce que produit [`Mp4Muxer`]).
fn analyser_avcc(avcc: &[u8]) -> Result<ParametresAvcc> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return Err(NdError::Protocol("avcC invalide".into()));
    }
    if avcc[4] & 0x03 != 0x03 {
        return Err(NdError::Protocol(
            "avcC avec préfixes de longueur ≠ 4 octets : non géré".into(),
        ));
    }
    let profil = [avcc[1], avcc[2], avcc[3]];
    let mut position = 5usize;
    let nb_sps = (avcc[position] & 0x1F) as usize;
    position += 1;
    let lire_jeu = |compte: usize, position: &mut usize| -> Result<Vec<Vec<u8>>> {
        let mut jeux = Vec::with_capacity(compte);
        for _ in 0..compte {
            let longueur = avcc
                .get(*position..*position + 2)
                .map(|o| u16::from_be_bytes(o.try_into().expect("2 octets")) as usize)
                .ok_or_else(|| NdError::Protocol("avcC tronquée".into()))?;
            *position += 2;
            let jeu = avcc
                .get(*position..*position + longueur)
                .ok_or_else(|| NdError::Protocol("avcC tronquée".into()))?;
            *position += longueur;
            jeux.push(jeu.to_vec());
        }
        Ok(jeux)
    };
    let sps = lire_jeu(nb_sps, &mut position)?;
    let nb_pps = *avcc
        .get(position)
        .ok_or_else(|| NdError::Protocol("avcC tronquée".into()))? as usize;
    position += 1;
    let pps = lire_jeu(nb_pps, &mut position)?;
    Ok(ParametresAvcc { profil, sps, pps })
}

/// Vérifie qu'un compte d'entrées annoncé tient dans le corps de sa boîte
/// (`corps.len() ≥ fixe + nombre × par_entree`) **avant** toute allocation :
/// un compte falsifié ne peut pas déclencher d'allocation démesurée.
fn verifier_compte(corps: &[u8], nombre: usize, fixe: usize, par_entree: usize) -> Result<()> {
    let requis = (nombre as u64) * (par_entree as u64) + fixe as u64;
    if requis > corps.len() as u64 {
        return Err(NdError::Protocol(
            "table d'échantillons annonçant plus d'entrées que sa boîte n'en contient".into(),
        ));
    }
    Ok(())
}

/// Table `stsz` : la taille de chaque échantillon.
fn lire_stsz(stbl: &[u8]) -> Result<Vec<u32>> {
    let stsz = CurseurBoites::trouver(stbl, b"stsz")?
        .ok_or_else(|| NdError::Protocol("stsz absente".into()))?;
    let (_, corps) = corps_boite_pleine(stsz)?;
    let taille_fixe = u32_be(corps, 0)?;
    let nombre = u32_be(corps, 4)? as usize;
    if taille_fixe == 0 {
        verifier_compte(corps, nombre, 8, 4)?;
    } else if nombre > PLAFOND_ECHANTILLONS {
        // Tailles constantes : la table est vide, le compte n'est donc pas
        // borné par la boîte — on le borne explicitement (garde
        // anti-corruption ; [`Mp4Muxer`] n'écrit jamais cette forme).
        return Err(NdError::Protocol(
            "stsz à taille fixe annonçant un compte démesuré".into(),
        ));
    }
    let mut tailles = Vec::with_capacity(nombre);
    for rang in 0..nombre {
        tailles.push(if taille_fixe != 0 {
            taille_fixe
        } else {
            u32_be(corps, 8 + rang * 4)?
        });
    }
    Ok(tailles)
}

/// Table `stts` déroulée : l'horodatage (tics média) de chaque échantillon.
fn lire_stts(stbl: &[u8], attendu: usize) -> Result<Vec<u64>> {
    let stts = CurseurBoites::trouver(stbl, b"stts")?
        .ok_or_else(|| NdError::Protocol("stts absente".into()))?;
    let (_, corps) = corps_boite_pleine(stts)?;
    let entrees = u32_be(corps, 0)? as usize;
    verifier_compte(corps, entrees, 4, 8)?;
    let mut tics = Vec::with_capacity(attendu);
    let mut courant = 0u64;
    for rang in 0..entrees {
        let compte = u32_be(corps, 4 + rang * 8)?;
        let delta = u32_be(corps, 8 + rang * 8)?;
        // Un compte falsifié ne doit pas faire enfler `tics` au-delà de ce
        // que stsz annonce : on échoue avant d'allouer.
        if compte as usize > attendu - tics.len() {
            return Err(NdError::Protocol(format!(
                "stts annonce plus d'échantillons que stsz ({attendu})"
            )));
        }
        for _ in 0..compte {
            tics.push(courant);
            courant += u64::from(delta);
        }
    }
    if tics.len() != attendu {
        return Err(NdError::Protocol(format!(
            "stts annonce {} échantillons, stsz en annonce {attendu}",
            tics.len()
        )));
    }
    Ok(tics)
}

/// Table `stss` (numéros d'images-clés, à partir de 1), ou `None` si absente
/// (auquel cas la norme dit que **tous** les échantillons sont des clés).
fn lire_stss(stbl: &[u8]) -> Result<Option<Vec<u32>>> {
    let Some(stss) = CurseurBoites::trouver(stbl, b"stss")? else {
        return Ok(None);
    };
    let (_, corps) = corps_boite_pleine(stss)?;
    let nombre = u32_be(corps, 0)? as usize;
    verifier_compte(corps, nombre, 4, 4)?;
    let mut cles = Vec::with_capacity(nombre);
    for rang in 0..nombre {
        cles.push(u32_be(corps, 4 + rang * 4)?);
    }
    Ok(Some(cles))
}

/// Reconstruit l'offset absolu de chaque échantillon via stsc + stco/co64.
fn lire_offsets(stbl: &[u8], tailles: &[u32]) -> Result<Vec<u64>> {
    // stco (32 bits) ou co64 (64 bits) : offsets des « chunks ».
    let chunks: Vec<u64> = if let Some(stco) = CurseurBoites::trouver(stbl, b"stco")? {
        let (_, corps) = corps_boite_pleine(stco)?;
        let nombre = u32_be(corps, 0)? as usize;
        verifier_compte(corps, nombre, 4, 4)?;
        (0..nombre)
            .map(|rang| u32_be(corps, 4 + rang * 4).map(u64::from))
            .collect::<Result<_>>()?
    } else if let Some(co64) = CurseurBoites::trouver(stbl, b"co64")? {
        let (_, corps) = corps_boite_pleine(co64)?;
        let nombre = u32_be(corps, 0)? as usize;
        verifier_compte(corps, nombre, 4, 8)?;
        (0..nombre)
            .map(|rang| u64_be(corps, 4 + rang * 8))
            .collect::<Result<_>>()?
    } else {
        return Err(NdError::Protocol("stco/co64 absente".into()));
    };

    let stsc = CurseurBoites::trouver(stbl, b"stsc")?
        .ok_or_else(|| NdError::Protocol("stsc absente".into()))?;
    let (_, corps) = corps_boite_pleine(stsc)?;
    let entrees = u32_be(corps, 0)? as usize;

    let mut offsets = Vec::with_capacity(tailles.len());
    let mut rang_echantillon = 0usize;
    for rang in 0..entrees {
        let premier_chunk = u32_be(corps, 4 + rang * 12)? as usize;
        let par_chunk = u32_be(corps, 8 + rang * 12)? as usize;
        let fin_chunks = if rang + 1 < entrees {
            u32_be(corps, 4 + (rang + 1) * 12)? as usize
        } else {
            chunks.len() + 1
        };
        if premier_chunk == 0 || fin_chunks > chunks.len() + 1 || premier_chunk > fin_chunks {
            return Err(NdError::Protocol("table stsc incohérente".into()));
        }
        for chunk in premier_chunk..fin_chunks {
            let mut offset = chunks[chunk - 1];
            for _ in 0..par_chunk {
                let Some(&taille) = tailles.get(rang_echantillon) else {
                    return Err(NdError::Protocol(
                        "stsc annonce plus d'échantillons que stsz".into(),
                    ));
                };
                offsets.push(offset);
                offset += u64::from(taille);
                rang_echantillon += 1;
            }
        }
    }
    if rang_echantillon != tailles.len() {
        return Err(NdError::Protocol(format!(
            "stsc couvre {rang_echantillon} échantillons, stsz en annonce {}",
            tailles.len()
        )));
    }
    Ok(offsets)
}

// ---------------------------------------------------------------------------
// Tests (NAL synthétiques : la structure du conteneur, sans codec réel —
// l'encodage/décodage réel est couvert par tests/recording_mp4.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::super::IndexedRecorder;
    use super::*;
    use nd_proto::MonitorId;

    /// SPS factice plausible (profil Baseline 0x42, niveau 30).
    fn sps() -> Vec<u8> {
        vec![0x67, 0x42, 0xC0, 0x1E, 0xAB, 0xCD]
    }

    /// PPS factice.
    fn pps() -> Vec<u8> {
        vec![0x68, 0xCE, 0x3C, 0x80]
    }

    /// Unité Annex B d'image-clé : SPS + PPS + tranche IDR (codes 4 octets).
    fn unite_cle(remplissage: u8) -> Vec<u8> {
        let mut unite = Vec::new();
        for nal in [sps(), pps(), vec![0x65, remplissage, remplissage, 0x11]] {
            unite.extend_from_slice(&[0, 0, 0, 1]);
            unite.extend_from_slice(&nal);
        }
        unite
    }

    /// Unité Annex B d'image delta : une tranche non-IDR (code 3 octets).
    fn unite_delta(remplissage: u8) -> Vec<u8> {
        let mut unite = vec![0, 0, 1];
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

    /// Mux de référence : 6 images à 25 i/s (40 000 µs), clés en 1 et 4.
    fn mp4_de_test() -> Vec<u8> {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        for i in 0..6u64 {
            let cle = i % 3 == 0;
            let unite = if cle {
                unite_cle(i as u8)
            } else {
                unite_delta(i as u8)
            };
            muxeur.record(i * 40_000, cle, &unite).unwrap();
        }
        assert_eq!(muxeur.frames_written(), 6);
        assert_eq!(muxeur.keyframes_written(), 2);
        muxeur.finish().unwrap().into_inner()
    }

    #[test]
    fn mux_puis_validation_complete() {
        let octets = mp4_de_test();
        // Le fichier commence bien par une boîte ftyp de marque isom.
        assert_eq!(&octets[4..8], b"ftyp");
        assert_eq!(&octets[8..12], b"isom");

        let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
        let rapport = lecteur.validate().unwrap();
        assert_eq!(rapport.frames, 6);
        assert_eq!(rapport.keyframes, 2);
        assert_eq!(rapport.duration_us, 240_000); // 6 images à 40 ms
        assert_eq!(rapport.width, 640);
        assert_eq!(rapport.height, 360);
        // 2 clés × 1 NAL (SPS/PPS déportés) + 4 deltas × 1 NAL.
        assert_eq!(rapport.nals, 6);
        assert_eq!(rapport.codec, "avc1.42C01E");
        assert!(rapport.hash_verified);
    }

    #[test]
    fn relecture_restitue_les_nal_exactes() {
        let mut lecteur = Mp4Reader::new(Cursor::new(mp4_de_test())).unwrap();
        // Image 1 : clé — la tranche IDR d'origine, SPS/PPS déportés.
        let premiere = lecteur.next_sample().unwrap().unwrap();
        assert!(premiere.keyframe);
        assert_eq!(premiere.timestamp_us, 0);
        assert_eq!(
            premiere.data,
            [&[0, 0, 0, 4][..], &[0x65, 0, 0, 0x11]].concat()
        );

        // Conversion Annex B : SPS + PPS réinjectés avant l'IDR.
        let annexb = lecteur.sample_annexb(&premiere).unwrap();
        assert_eq!(annexb, unite_cle(0));

        // Image 2 : delta, sans réinjection de paramètres.
        let deuxieme = lecteur.next_sample().unwrap().unwrap();
        assert!(!deuxieme.keyframe);
        assert_eq!(deuxieme.timestamp_us, 40_000);
        assert_eq!(lecteur.sample_annexb(&deuxieme).unwrap(), {
            let mut attendu = vec![0, 0, 0, 1];
            attendu.extend_from_slice(&[0x41, 1, 0x22]);
            attendu
        });

        // Les 6 images sont relisibles, puis fin propre ; rewind rejoue tout.
        assert!(lecteur.next_sample().unwrap().is_some());
        assert!(lecteur.next_sample().unwrap().is_some());
        assert!(lecteur.next_sample().unwrap().is_some());
        assert!(lecteur.next_sample().unwrap().is_some());
        assert!(lecteur.next_sample().unwrap().is_none());
        lecteur.rewind();
        assert_eq!(lecteur.next_sample().unwrap().unwrap().timestamp_us, 0);
    }

    #[test]
    fn samples_rend_de_l_annexb_pret_a_decoder() {
        let mut lecteur = Mp4Reader::new(Cursor::new(mp4_de_test())).unwrap();
        let echs = lecteur.samples().unwrap();
        assert_eq!(echs.len(), 6);
        // Image-clé : Annex B avec SPS/PPS réinjectés (== unité d'origine).
        assert!(echs[0].is_keyframe);
        assert_eq!(echs[0].timestamp_us, 0);
        assert_eq!(echs[0].data, unite_cle(0));
        // Delta : juste la tranche, sans paramètres réinjectés.
        assert!(!echs[1].is_keyframe);
        assert_eq!(echs[1].timestamp_us, 40_000);
        assert_eq!(echs[1].data, {
            let mut unite = vec![0, 0, 0, 1];
            unite.extend_from_slice(&[0x41, 1, 0x22]);
            unite
        });
        // N'affecte pas le curseur de next_sample, et reste idempotent.
        assert_eq!(lecteur.next_sample().unwrap().unwrap().timestamp_us, 0);
        assert_eq!(lecteur.samples().unwrap(), echs);
    }

    #[test]
    fn sample_at_choisit_l_image_cle_precedente() {
        let mut lecteur = Mp4Reader::new(Cursor::new(mp4_de_test())).unwrap();
        // Images-clés à ts 0 et 120 000.
        let s = lecteur.sample_at(130_000).unwrap().unwrap();
        assert!(s.is_keyframe);
        assert_eq!(s.timestamp_us, 120_000);
        assert_eq!(s.data, unite_cle(3));
        // Exactement sur une clé, avant tout, entre deux, après tout.
        assert_eq!(
            lecteur.sample_at(120_000).unwrap().unwrap().timestamp_us,
            120_000
        );
        assert_eq!(lecteur.sample_at(0).unwrap().unwrap().timestamp_us, 0);
        assert_eq!(lecteur.sample_at(50_000).unwrap().unwrap().timestamp_us, 0);
        assert_eq!(
            lecteur.sample_at(u64::MAX).unwrap().unwrap().timestamp_us,
            120_000
        );
    }

    #[test]
    fn fps_derive_des_durees_stts() {
        let lecteur = Mp4Reader::new(Cursor::new(mp4_de_test())).unwrap();
        assert_eq!(lecteur.fps(), 25);
    }

    #[test]
    fn record_video_chunk_equivaut_a_record() {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        muxeur
            .record_video_chunk(&EncodedChunk {
                data: unite_cle(9),
                is_keyframe: true,
                monitor: MonitorId(0),
                timestamp_us: 0,
            })
            .unwrap();
        muxeur
            .record_video_chunk(&EncodedChunk {
                data: unite_delta(9),
                is_keyframe: false,
                monitor: MonitorId(0),
                timestamp_us: 40_000,
            })
            .unwrap();
        let octets = muxeur.finish().unwrap().into_inner();
        let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
        assert_eq!(lecteur.validate().unwrap().frames, 2);
    }

    #[test]
    fn premier_echantillon_non_cle_refuse() {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        assert!(muxeur.record(0, false, &unite_delta(1)).is_err());
    }

    #[test]
    fn parametres_seuls_absorbes_sans_echantillon() {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        // SPS/PPS seuls (aucune tranche) : capturés, aucun échantillon créé.
        let mut parametres = Vec::new();
        for nal in [sps(), pps()] {
            parametres.extend_from_slice(&[0, 0, 0, 1]);
            parametres.extend_from_slice(&nal);
        }
        muxeur.record(0, false, &parametres).unwrap();
        assert_eq!(muxeur.frames_written(), 0);
        // L'image-clé suivante (IDR seul) devient le premier échantillon.
        let mut idr = vec![0, 0, 0, 1];
        idr.extend_from_slice(&[0x65, 0x00, 0x11]);
        muxeur.record(0, true, &idr).unwrap();
        let octets = muxeur.finish().unwrap().into_inner();
        let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
        assert_eq!(lecteur.validate().unwrap().frames, 1);
    }

    #[test]
    fn horodatage_decroissant_refuse() {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        muxeur.record(10_000, true, &unite_cle(1)).unwrap();
        assert!(muxeur.record(5_000, false, &unite_delta(1)).is_err());
        // Égalité permise (rafale de sous-images).
        muxeur.record(10_000, false, &unite_delta(2)).unwrap();
    }

    #[test]
    fn flux_non_annexb_refuse() {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        assert!(muxeur.record(0, true, b"pas un flux H.264").is_err());
        assert!(muxeur.record(0, true, &[]).is_err());
    }

    #[test]
    fn changement_de_sps_refuse() {
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        muxeur.record(0, true, &unite_cle(1)).unwrap();
        // Même structure mais SPS différent : refusé (une seule avcC).
        let mut autre = Vec::new();
        for nal in [vec![0x67, 0x64, 0x00, 0x28], pps()] {
            autre.extend_from_slice(&[0, 0, 0, 1]);
            autre.extend_from_slice(&nal);
        }
        autre.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x33]);
        assert!(muxeur.record(40_000, true, &autre).is_err());
    }

    #[test]
    fn finish_sans_image_ou_sans_parametres_refuse() {
        // Aucune image.
        let muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        assert!(muxeur.finish().is_err());
        // Une image-clé sans SPS/PPS : non décodable, refusé à la clôture.
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        let mut idr = vec![0, 0, 0, 1];
        idr.extend_from_slice(&[0x65, 0x00, 0x11]);
        muxeur.record(0, true, &idr).unwrap();
        assert!(muxeur.finish().is_err());
    }

    #[test]
    fn metadonnees_invalides_refusees() {
        let vp9 = RecordingMetadata {
            codec: "vp9".into(),
            ..meta()
        };
        assert!(Mp4Muxer::new(Cursor::new(Vec::new()), vp9).is_err());
        let fps_nul = RecordingMetadata { fps: 0, ..meta() };
        assert!(Mp4Muxer::new(Cursor::new(Vec::new()), fps_nul).is_err());
        let plate = RecordingMetadata {
            height: 0,
            ..meta()
        };
        assert!(Mp4Muxer::new(Cursor::new(Vec::new()), plate).is_err());
    }

    #[test]
    fn hachage_detecte_une_corruption_de_donnees() {
        let octets = mp4_de_test();
        // Corrompt un octet de la charge utile d'une NAL (après l'en-tête
        // mdat long : ftyp 32 + en-tête 16 + préfixe 4 + 1 → dans l'IDR).
        let mut corrompu = octets.clone();
        corrompu[32 + 16 + 4 + 1] ^= 0xFF;
        let mut lecteur = Mp4Reader::new(Cursor::new(corrompu)).unwrap();
        assert!(lecteur.validate().is_err());

        // Corrompt le hachage lui-même (32 derniers octets du fichier).
        let mut hachage_faux = octets;
        let dernier = hachage_faux.len() - 1;
        hachage_faux[dernier] ^= 0xFF;
        let mut lecteur = Mp4Reader::new(Cursor::new(hachage_faux)).unwrap();
        assert!(lecteur.validate().is_err());
    }

    #[test]
    fn sans_boite_ndb3_le_mp4_reste_valide_sans_verification() {
        // Retire la boîte `free NDB3` finale : le MP4 reste un MP4 complet,
        // simplement sans intégrité vérifiable (fichier d'un autre outil).
        let mut octets = mp4_de_test();
        octets.truncate(octets.len() - TAILLE_BOITE_HACHAGE as usize);
        let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
        let rapport = lecteur.validate().unwrap();
        assert_eq!(rapport.frames, 6);
        assert!(!rapport.hash_verified);
    }

    #[test]
    fn fichiers_non_mp4_refuses() {
        assert!(Mp4Reader::new(Cursor::new(b"NDREC2\x02\x00".to_vec())).is_err());
        assert!(Mp4Reader::new(Cursor::new(Vec::new())).is_err());
        // Taille de boîte au-delà du fichier : tronqué.
        let mut tronque = mp4_de_test();
        tronque.truncate(40);
        assert!(Mp4Reader::new(Cursor::new(tronque)).is_err());
    }

    #[test]
    fn compte_stsz_falsifie_rejete_sans_allocation_demesuree() {
        // Gonfle le compte d'échantillons de stsz à u32::MAX : le lecteur
        // doit refuser net (la table dépasse sa boîte), sans tenter
        // d'allouer des gigaoctets.
        let mut octets = mp4_de_test();
        let position = octets
            .windows(4)
            .position(|fenetre| fenetre == b"stsz")
            .expect("boîte stsz présente");
        // Après le type : version/drapeaux (4) + taille fixe (4) → compte.
        octets[position + 12..position + 16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Mp4Reader::new(Cursor::new(octets)).is_err());
    }

    #[test]
    fn codes_de_depart_3_et_4_octets_acceptes() {
        // L'unité clé mélange codes 4 octets (unite_cle) et 3 octets (delta).
        let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), meta()).unwrap();
        let mut mixte = unite_cle(5);
        mixte.extend_from_slice(&unite_delta(5)); // code 3 octets à la suite
        muxeur.record(0, true, &mixte).unwrap();
        let octets = muxeur.finish().unwrap().into_inner();
        let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
        let rapport = lecteur.validate().unwrap();
        assert_eq!(rapport.frames, 1);
        assert_eq!(rapport.nals, 2); // IDR + delta dans le même échantillon
    }

    #[test]
    fn conversion_ndr_vers_mp4() {
        // Archive .ndr v2 (hachage activé) avec les mêmes unités Annex B.
        let mut archive = IndexedRecorder::new(Vec::new(), meta(), true).unwrap();
        for i in 0..5u64 {
            let cle = i % 2 == 0;
            let unite = if cle {
                unite_cle(i as u8)
            } else {
                unite_delta(i as u8)
            };
            archive.record(i * 40_000, cle, &unite).unwrap();
        }
        let ndr = archive.finish().unwrap();

        let mut lecteur_ndr = SessionReader::new(Cursor::new(ndr)).unwrap();
        let mp4 = ndr_to_mp4(&mut lecteur_ndr, Cursor::new(Vec::new()))
            .unwrap()
            .into_inner();
        let mut lecteur = Mp4Reader::new(Cursor::new(mp4)).unwrap();
        let rapport = lecteur.validate().unwrap();
        assert_eq!(rapport.frames, 5);
        assert_eq!(rapport.keyframes, 3);
        assert!(rapport.hash_verified);
    }

    #[test]
    fn conversion_ndr_v1_sans_metadonnees_refusee() {
        let mut v1 = super::super::SessionRecorder::new(Vec::new()).unwrap();
        v1.record(0, true, &unite_cle(0)).unwrap();
        let octets = v1.finish().unwrap();
        let mut lecteur_ndr = SessionReader::new(Cursor::new(octets)).unwrap();
        assert!(ndr_to_mp4(&mut lecteur_ndr, Cursor::new(Vec::new())).is_err());
    }

    #[test]
    fn conversion_us_90khz_sans_derive() {
        // Multiples de 100 µs : conversion exacte dans les deux sens.
        for us in [0u64, 10_000, 40_000, 1_000_000, 3_600_000_000] {
            assert_eq!(tics_vers_us(us_vers_90khz(us)), us);
        }
        // Cas général : écart d'arrondi ≤ 1 µs, jamais cumulé.
        for us in [33_333u64, 16_667, 999_999_999] {
            assert!(tics_vers_us(us_vers_90khz(us)).abs_diff(us) <= 1);
        }
    }
}
