//! Synchronisation du presse-papiers de session (plan 09).
//!
//! [`ClipboardSync`] sérialise le contenu du presse-papiers local (texte, image
//! ou liste de fichiers) en octets à faire circuler sur un canal fiable, et
//! applique le contenu reçu sur le presse-papiers local. La couche session
//! (nd-core) n'a qu'à transporter ces octets : la capture et l'application
//! réutilisent le trait [`Clipboard`] du crate (implémentation Windows via
//! `CF_UNICODETEXT`/`CF_DIB`/`CF_HDROP` ; les autres OS restent en
//! [`NdError::NotImplemented`] tant qu'aucun back-end n'est fourni).
//!
//! Le back-end est injectable ([`ClipboardSync::with_backend`]), ce qui rend la
//! sérialisation et l'application testables entièrement en mémoire.
//!
//! # Format binaire du contenu
//!
//! ```text
//! contenu = [tag : u8][charge utile]
//!
//! Text  (tag 1) : [long. u32 LE][texte UTF-8]
//! Image (tag 2) : [largeur u32][hauteur u32][long. rgba u64][octets RGBA]
//! Files (tag 3) : [nombre u32]{ [long. u32][chemin UTF-8] }
//! ```

use std::path::PathBuf;

use nd_proto::{NdError, Result};

use crate::{Clipboard, ImageRgba};

/// Tag binaire du contenu texte.
const TAG_TEXT: u8 = 1;
/// Tag binaire du contenu image.
const TAG_IMAGE: u8 = 2;
/// Tag binaire du contenu « liste de fichiers ».
const TAG_FILES: u8 = 3;

/// Contenu de presse-papiers échangeable sur le canal de session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    /// Texte brut.
    Text(String),
    /// Image bitmap RGBA (voir [`ImageRgba`]).
    Image(ImageRgba),
    /// Liste de fichiers copiés (chemins). Sérialisés en UTF-8 (potentiellement
    /// avec perte pour un chemin non-UTF-8, cas rare et documenté).
    ///
    /// Ces chemins sont ceux de la machine **émettrice** : les appliquer tels
    /// quels ([`ClipboardSync::apply`]) colle des chemins qui n'existent pas sur
    /// le récepteur. Pour coller de **vrais** fichiers, le récepteur matérialise
    /// leur contenu localement — voir le module
    /// [`clipboard_files`](crate::clipboard_files) (manifeste nom + taille, puis
    /// téléchargement par tranches vers un dossier temporaire local).
    Files(Vec<PathBuf>),
}

impl ClipboardContent {
    /// Sérialise le contenu en octets autonomes (voir le format en tête de module).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ClipboardContent::Text(t) => {
                out.push(TAG_TEXT);
                out.extend_from_slice(&(t.len() as u32).to_le_bytes());
                out.extend_from_slice(t.as_bytes());
            }
            ClipboardContent::Image(img) => {
                out.push(TAG_IMAGE);
                out.extend_from_slice(&img.width.to_le_bytes());
                out.extend_from_slice(&img.height.to_le_bytes());
                out.extend_from_slice(&(img.rgba.len() as u64).to_le_bytes());
                out.extend_from_slice(&img.rgba);
            }
            ClipboardContent::Files(paths) => {
                out.push(TAG_FILES);
                out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
                for p in paths {
                    let s = p.to_string_lossy();
                    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
            }
        }
        out
    }

    /// Désérialise un contenu depuis `buf`, qui doit contenir exactement un
    /// message (aucun octet excédentaire). [`NdError::Protocol`] sinon.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let (&tag, mut reste) = buf
            .split_first()
            .ok_or_else(|| NdError::Protocol("contenu de presse-papiers vide".into()))?;
        let contenu = match tag {
            TAG_TEXT => {
                let n = lire_u32(&mut reste)? as usize;
                let texte = String::from_utf8(lire_octets(&mut reste, n)?.to_vec())
                    .map_err(|_| NdError::Protocol("texte de presse-papiers non UTF-8".into()))?;
                exiger_vide(reste)?;
                ClipboardContent::Text(texte)
            }
            TAG_IMAGE => {
                let width = lire_u32(&mut reste)?;
                let height = lire_u32(&mut reste)?;
                let long = lire_u64(&mut reste)? as usize;
                let rgba = lire_octets(&mut reste, long)?.to_vec();
                exiger_vide(reste)?;
                let attendu = (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|n| n.checked_mul(4));
                if width == 0 || height == 0 || attendu != Some(rgba.len()) {
                    return Err(NdError::Protocol(format!(
                        "image de presse-papiers incohérente : {width}x{height} pour {} octets",
                        rgba.len()
                    )));
                }
                ClipboardContent::Image(ImageRgba {
                    width,
                    height,
                    rgba,
                })
            }
            TAG_FILES => {
                let nombre = lire_u32(&mut reste)? as usize;
                let mut paths = Vec::with_capacity(nombre);
                for _ in 0..nombre {
                    let n = lire_u32(&mut reste)? as usize;
                    let s = String::from_utf8(lire_octets(&mut reste, n)?.to_vec())
                        .map_err(|_| NdError::Protocol("chemin de fichier non UTF-8".into()))?;
                    paths.push(PathBuf::from(s));
                }
                exiger_vide(reste)?;
                ClipboardContent::Files(paths)
            }
            t => {
                return Err(NdError::Protocol(format!(
                    "tag de contenu de presse-papiers inconnu : {t}"
                )))
            }
        };
        Ok(contenu)
    }
}

/// Synchro presse-papiers d'une session : capture le contenu local en octets et
/// applique le contenu reçu. Voir la documentation du module.
pub struct ClipboardSync {
    backend: Box<dyn Clipboard>,
}

impl ClipboardSync {
    /// Ouvre la synchro sur le presse-papiers de la plateforme (via
    /// [`crate::open_clipboard`]). Échoue avec [`NdError::NotImplemented`] sur
    /// les OS sans back-end presse-papiers à ce stade.
    pub fn new() -> Result<Self> {
        Ok(Self {
            backend: crate::open_clipboard()?,
        })
    }

    /// Construit la synchro sur un back-end fourni (tests en mémoire, ou
    /// implémentation de presse-papiers spécifique).
    pub fn with_backend(backend: Box<dyn Clipboard>) -> Self {
        Self { backend }
    }

    /// Capture le contenu courant du presse-papiers local à envoyer sur le
    /// canal. Ordre de préférence : texte, puis image, puis liste de fichiers.
    /// `Ok(None)` si le presse-papiers est vide ou d'un format non géré.
    pub fn capture(&self) -> Result<Option<ClipboardContent>> {
        if let Some(t) = self.backend.get_text()? {
            return Ok(Some(ClipboardContent::Text(t)));
        }
        if let Some(img) = self.backend.get_image()? {
            return Ok(Some(ClipboardContent::Image(img)));
        }
        let files = self.backend.get_files()?;
        if !files.is_empty() {
            return Ok(Some(ClipboardContent::Files(files)));
        }
        Ok(None)
    }

    /// Comme [`Self::capture`], mais renvoie directement les octets prêts pour
    /// le canal (`None` si rien à envoyer).
    pub fn capture_bytes(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.capture()?.map(|c| c.to_bytes()))
    }

    /// Applique sur le presse-papiers local un contenu reçu du pair.
    pub fn apply(&self, content: &ClipboardContent) -> Result<()> {
        match content {
            ClipboardContent::Text(t) => self.backend.set_text(t),
            ClipboardContent::Image(img) => self.backend.set_image(img),
            ClipboardContent::Files(paths) => self.backend.set_files(paths),
        }
    }

    /// Désérialise des octets reçus du canal puis applique le contenu.
    pub fn apply_bytes(&self, bytes: &[u8]) -> Result<()> {
        let content = ClipboardContent::from_bytes(bytes)?;
        self.apply(&content)
    }
}

/// Exige que `reste` soit vide (aucun octet excédentaire).
fn exiger_vide(reste: &[u8]) -> Result<()> {
    if reste.is_empty() {
        Ok(())
    } else {
        Err(NdError::Protocol(
            "octets excédentaires après le contenu de presse-papiers".into(),
        ))
    }
}

/// Prélève `n` octets en tête de `charge`.
fn lire_octets<'a>(charge: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if charge.len() < n {
        return Err(NdError::Protocol(format!(
            "contenu de presse-papiers tronqué : {n} octets attendus, {} restants",
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Presse-papiers factice en mémoire (texte, image, fichiers), pour tester
    /// capture/application sans toucher au presse-papiers du système.
    #[derive(Default)]
    struct MockClip {
        text: RefCell<Option<String>>,
        image: RefCell<Option<ImageRgba>>,
        files: RefCell<Vec<PathBuf>>,
    }

    impl Clipboard for MockClip {
        fn get_text(&self) -> Result<Option<String>> {
            Ok(self.text.borrow().clone())
        }
        fn set_text(&self, text: &str) -> Result<()> {
            *self.text.borrow_mut() = Some(text.to_string());
            Ok(())
        }
        fn get_image(&self) -> Result<Option<ImageRgba>> {
            Ok(self.image.borrow().clone())
        }
        fn set_image(&self, image: &ImageRgba) -> Result<()> {
            *self.image.borrow_mut() = Some(image.clone());
            Ok(())
        }
        fn get_files(&self) -> Result<Vec<PathBuf>> {
            Ok(self.files.borrow().clone())
        }
        fn set_files(&self, paths: &[PathBuf]) -> Result<()> {
            *self.files.borrow_mut() = paths.to_vec();
            Ok(())
        }
    }

    fn image_test() -> ImageRgba {
        let (w, h) = (4u32, 3u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 17) as u8, (y * 23) as u8, (x + y) as u8, 200]);
            }
        }
        ImageRgba {
            width: w,
            height: h,
            rgba,
        }
    }

    #[test]
    fn serialisation_aller_retour() {
        let contenus = [
            ClipboardContent::Text("bonjour éàü₿".to_string()),
            ClipboardContent::Image(image_test()),
            ClipboardContent::Files(vec![
                PathBuf::from("C:/tmp/a.txt"),
                PathBuf::from("C:/tmp/dossier/b.bin"),
            ]),
        ];
        for c in &contenus {
            let octets = c.to_bytes();
            let relu = ClipboardContent::from_bytes(&octets).unwrap();
            assert_eq!(&relu, c);
        }
    }

    #[test]
    fn deserialisation_invalide_rejetee() {
        assert!(ClipboardContent::from_bytes(&[]).is_err()); // vide
        assert!(ClipboardContent::from_bytes(&[99]).is_err()); // tag inconnu
        assert!(ClipboardContent::from_bytes(&[TAG_TEXT, 5, 0, 0, 0, b'a']).is_err()); // texte tronqué
                                                                                       // Octets excédentaires après un texte complet.
        let mut trop = ClipboardContent::Text("ab".to_string()).to_bytes();
        trop.push(0);
        assert!(ClipboardContent::from_bytes(&trop).is_err());
        // Image aux dimensions incohérentes avec la longueur RGBA.
        let mut img = ImageRgba {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
        };
        img.rgba.truncate(8);
        assert!(ClipboardContent::from_bytes(&ClipboardContent::Image(img).to_bytes()).is_err());
    }

    #[test]
    fn capture_puis_application_via_backend() {
        // Pair « distant » : texte dans son presse-papiers.
        let distant = ClipboardSync::with_backend(Box::new(MockClip::default()));
        distant
            .apply(&ClipboardContent::Text("copié".to_string()))
            .unwrap();
        let octets = distant
            .capture_bytes()
            .unwrap()
            .expect("un contenu à envoyer");

        // Pair « local » : applique les octets reçus, son presse-papiers reçoit le texte.
        let local_backend = MockClip::default();
        let local = ClipboardSync::with_backend(Box::new(local_backend));
        local.apply_bytes(&octets).unwrap();
        assert_eq!(
            local.capture().unwrap(),
            Some(ClipboardContent::Text("copié".to_string()))
        );
    }

    #[test]
    fn capture_ordre_texte_image_fichiers() {
        let mock = MockClip::default();
        // Aucun contenu → None.
        let sync = ClipboardSync::with_backend(Box::new(mock));
        assert_eq!(sync.capture().unwrap(), None);

        // Image seule → capturée comme image.
        let sync = ClipboardSync::with_backend(Box::new(MockClip::default()));
        sync.apply(&ClipboardContent::Image(image_test())).unwrap();
        assert_eq!(
            sync.capture().unwrap(),
            Some(ClipboardContent::Image(image_test()))
        );

        // Liste de fichiers → aller-retour complet (capture puis application).
        let sync = ClipboardSync::with_backend(Box::new(MockClip::default()));
        let fichiers = ClipboardContent::Files(vec![PathBuf::from("C:/x/y.dat")]);
        sync.apply(&fichiers).unwrap();
        assert_eq!(sync.capture().unwrap(), Some(fichiers));
    }
}
