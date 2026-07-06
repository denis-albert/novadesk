//! Enregistrement de session : sérialisation d'une suite d'images encodées
//! (opaques pour ce module) vers un flux `Write`, et relecture depuis `Read`.
//!
//! Format binaire (entiers petit-boutistes) :
//! - en-tête : magic `NDREC1` (6 octets) puis version `u16` ;
//! - puis, pour chaque image : `[u64 timestamp_us][u8 keyframe][u32 len][data]`.
//!
//! Le contenu des images est opaque : l'enregistreur ne décode rien, il
//! archive fidèlement ce que le codec lui donne (voir plan 13, §enregistrement).

use std::io::{Read, Write};

use nd_proto::{NdError, Result};

/// Magic en tête d'un enregistrement NovaDesk.
pub const MAGIC: &[u8; 6] = b"NDREC1";

/// Version courante du format d'enregistrement.
pub const VERSION: u16 = 1;

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

/// Écrit un enregistrement de session dans un flux quelconque
/// (`Vec<u8>` en mémoire, fichier, socket…).
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

/// Relit un enregistrement produit par [`SessionRecorder`].
///
/// Utilisable via [`SessionReader::next_frame`] ou comme itérateur
/// d'`Item = Result<RecordedFrame>`.
#[derive(Debug)]
pub struct SessionReader<R: Read> {
    source: R,
    version: u16,
}

impl<R: Read> SessionReader<R> {
    /// Ouvre l'enregistrement : lit et valide l'en-tête (magic + version).
    pub fn new(mut source: R) -> Result<Self> {
        let mut magic = [0u8; 6];
        source.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(NdError::Protocol(
                "magic NDREC1 absent : ce flux n'est pas un enregistrement NovaDesk".into(),
            ));
        }
        let mut version = [0u8; 2];
        source.read_exact(&mut version)?;
        let version = u16::from_le_bytes(version);
        if version != VERSION {
            return Err(NdError::Protocol(format!(
                "version d'enregistrement {version} non gérée (attendu {VERSION})"
            )));
        }
        Ok(SessionReader { source, version })
    }

    /// Version du format lue dans l'en-tête.
    #[must_use]
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Image suivante, ou `Ok(None)` en fin d'enregistrement (fin propre).
    ///
    /// Une fin de flux au milieu d'un enregistrement est signalée comme
    /// [`NdError::Protocol`] (enregistrement tronqué).
    pub fn next_frame(&mut self) -> Result<Option<RecordedFrame>> {
        let mut horodatage = [0u8; 8];
        if !lire_ou_fin(&mut self.source, &mut horodatage)? {
            return Ok(None);
        }
        let mut drapeau = [0u8; 1];
        self.source.read_exact(&mut drapeau)?;
        let keyframe = match drapeau[0] {
            0 => false,
            1 => true,
            autre => {
                return Err(NdError::Protocol(format!(
                    "drapeau keyframe invalide : {autre}"
                )))
            }
        };
        let mut longueur = [0u8; 4];
        self.source.read_exact(&mut longueur)?;
        let mut data = vec![0u8; u32::from_le_bytes(longueur) as usize];
        self.source.read_exact(&mut data)?;
        Ok(Some(RecordedFrame {
            timestamp_us: u64::from_le_bytes(horodatage),
            keyframe,
            data,
        }))
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
