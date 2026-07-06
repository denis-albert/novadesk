//! Annotation d'écran (« tableau blanc ») : traits dessinés par-dessus
//! l'image transmise, par le contrôleur ou le contrôlé, sans modifier le
//! bureau réel. Ce module fournit le modèle ([`Stroke`], [`AnnotationLayer`])
//! et sa sérialisation binaire pour le transport (voir plan 13, §annotation).
//!
//! Format binaire (entiers et flottants petit-boutistes) :
//! - en-tête : magic `NDANN1` (6 octets) puis version `u16` ;
//! - `u32` nombre de traits, puis pour chaque trait :
//!   `[u64 id][u8 type][charge utile selon le type]`.
//!
//! Coordonnées : flottants normalisés ou pixels — le module n'impose rien,
//! l'émetteur et le récepteur partagent la même convention de repère.

use nd_proto::{NdError, Result};

/// Magic en tête d'une couche d'annotations sérialisée.
pub const MAGIC: &[u8; 6] = b"NDANN1";

/// Version courante du format d'annotations.
pub const VERSION: u16 = 1;

// Étiquettes de type des traits dans le flux binaire.
const TAG_LINE: u8 = 1;
const TAG_RECT: u8 = 2;
const TAG_ELLIPSE: u8 = 3;
const TAG_ARROW: u8 = 4;
const TAG_TEXT: u8 = 5;

/// Un trait dessiné par-dessus l'écran.
///
/// `color` est un RGBA empaqueté `0xRRGGBBAA` ; `width` est l'épaisseur du
/// tracé dans l'unité du repère partagé.
#[derive(Debug, Clone, PartialEq)]
pub enum Stroke {
    /// Tracé libre / polyligne reliant `points` dans l'ordre.
    Line {
        points: Vec<(f32, f32)>,
        color: u32,
        width: f32,
    },
    /// Rectangle défini par deux coins opposés.
    Rect {
        min: (f32, f32),
        max: (f32, f32),
        color: u32,
        width: f32,
    },
    /// Ellipse de centre `center` et de demi-axes `radii`.
    Ellipse {
        center: (f32, f32),
        radii: (f32, f32),
        color: u32,
        width: f32,
    },
    /// Flèche pointant de `from` vers `to`.
    Arrow {
        from: (f32, f32),
        to: (f32, f32),
        color: u32,
        width: f32,
    },
    /// Texte posé en `position` (`size` : hauteur de police, même unité).
    Text {
        position: (f32, f32),
        contenu: String,
        color: u32,
        size: f32,
    },
}

impl Stroke {
    /// Sérialise la charge utile du trait (étiquette comprise) dans `sortie`.
    fn encode(&self, sortie: &mut Vec<u8>) -> Result<()> {
        match self {
            Stroke::Line {
                points,
                color,
                width,
            } => {
                sortie.push(TAG_LINE);
                let nombre = u32::try_from(points.len()).map_err(|_| {
                    NdError::Protocol("polyligne trop longue pour le format (> u32)".into())
                })?;
                sortie.extend_from_slice(&nombre.to_le_bytes());
                for point in points {
                    ecrire_point(sortie, *point);
                }
                sortie.extend_from_slice(&color.to_le_bytes());
                sortie.extend_from_slice(&width.to_le_bytes());
            }
            Stroke::Rect {
                min,
                max,
                color,
                width,
            } => {
                sortie.push(TAG_RECT);
                ecrire_point(sortie, *min);
                ecrire_point(sortie, *max);
                sortie.extend_from_slice(&color.to_le_bytes());
                sortie.extend_from_slice(&width.to_le_bytes());
            }
            Stroke::Ellipse {
                center,
                radii,
                color,
                width,
            } => {
                sortie.push(TAG_ELLIPSE);
                ecrire_point(sortie, *center);
                ecrire_point(sortie, *radii);
                sortie.extend_from_slice(&color.to_le_bytes());
                sortie.extend_from_slice(&width.to_le_bytes());
            }
            Stroke::Arrow {
                from,
                to,
                color,
                width,
            } => {
                sortie.push(TAG_ARROW);
                ecrire_point(sortie, *from);
                ecrire_point(sortie, *to);
                sortie.extend_from_slice(&color.to_le_bytes());
                sortie.extend_from_slice(&width.to_le_bytes());
            }
            Stroke::Text {
                position,
                contenu,
                color,
                size,
            } => {
                sortie.push(TAG_TEXT);
                ecrire_point(sortie, *position);
                let longueur = u32::try_from(contenu.len()).map_err(|_| {
                    NdError::Protocol("texte trop long pour le format (> u32)".into())
                })?;
                sortie.extend_from_slice(&longueur.to_le_bytes());
                sortie.extend_from_slice(contenu.as_bytes());
                sortie.extend_from_slice(&color.to_le_bytes());
                sortie.extend_from_slice(&size.to_le_bytes());
            }
        }
        Ok(())
    }

    /// Décode un trait depuis `lecteur` (l'étiquette de type a déjà été lue).
    fn decode(etiquette: u8, lecteur: &mut Lecteur<'_>) -> Result<Stroke> {
        match etiquette {
            TAG_LINE => {
                let nombre = lecteur.u32()? as usize;
                // Garde-fou : la longueur annoncée doit tenir dans le flux
                // restant avant toute allocation (entrée hostile).
                if nombre.checked_mul(8).is_none_or(|o| o > lecteur.restant()) {
                    return Err(NdError::Protocol(
                        "polyligne annoncée plus longue que le flux".into(),
                    ));
                }
                let mut points = Vec::with_capacity(nombre);
                for _ in 0..nombre {
                    points.push(lecteur.point()?);
                }
                Ok(Stroke::Line {
                    points,
                    color: lecteur.u32()?,
                    width: lecteur.f32()?,
                })
            }
            TAG_RECT => Ok(Stroke::Rect {
                min: lecteur.point()?,
                max: lecteur.point()?,
                color: lecteur.u32()?,
                width: lecteur.f32()?,
            }),
            TAG_ELLIPSE => Ok(Stroke::Ellipse {
                center: lecteur.point()?,
                radii: lecteur.point()?,
                color: lecteur.u32()?,
                width: lecteur.f32()?,
            }),
            TAG_ARROW => Ok(Stroke::Arrow {
                from: lecteur.point()?,
                to: lecteur.point()?,
                color: lecteur.u32()?,
                width: lecteur.f32()?,
            }),
            TAG_TEXT => {
                let position = lecteur.point()?;
                let longueur = lecteur.u32()? as usize;
                if longueur > lecteur.restant() {
                    return Err(NdError::Protocol(
                        "texte annoncé plus long que le flux".into(),
                    ));
                }
                let contenu = String::from_utf8(lecteur.prendre(longueur)?.to_vec())
                    .map_err(|_| NdError::Protocol("texte d'annotation non UTF-8".into()))?;
                Ok(Stroke::Text {
                    position,
                    contenu,
                    color: lecteur.u32()?,
                    size: lecteur.f32()?,
                })
            }
            autre => Err(NdError::Protocol(format!(
                "type de trait inconnu : {autre}"
            ))),
        }
    }
}

/// Couche d'annotations : les traits, chacun sous un identifiant stable qui
/// permet la suppression ciblée (gomme) de part et d'autre du transport.
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationLayer {
    /// Paires `(id, trait)` dans l'ordre d'ajout (ordre de dessin).
    traits: Vec<(u64, Stroke)>,
    /// Prochain identifiant à attribuer (jamais réutilisé après suppression).
    prochain_id: u64,
}

impl AnnotationLayer {
    /// Couche vide ; les identifiants commencent à 1 (0 = « aucun trait »).
    #[must_use]
    pub fn new() -> Self {
        AnnotationLayer {
            traits: Vec::new(),
            prochain_id: 1,
        }
    }

    /// Ajoute un trait au-dessus des existants ; rend son identifiant.
    pub fn add(&mut self, stroke: Stroke) -> u64 {
        let id = self.prochain_id;
        self.prochain_id += 1;
        self.traits.push((id, stroke));
        id
    }

    /// Supprime le trait `id` ; rend vrai s'il existait.
    pub fn remove(&mut self, id: u64) -> bool {
        let avant = self.traits.len();
        self.traits.retain(|(i, _)| *i != id);
        self.traits.len() != avant
    }

    /// Efface tous les traits (les identifiants ne sont pas réutilisés).
    pub fn clear(&mut self) {
        self.traits.clear();
    }

    /// Nombre de traits présents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.traits.len()
    }

    /// Vrai si la couche ne contient aucun trait.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.traits.is_empty()
    }

    /// Les traits, dans l'ordre de dessin, avec leurs identifiants.
    #[must_use]
    pub fn strokes(&self) -> &[(u64, Stroke)] {
        &self.traits
    }

    /// Sérialise la couche pour le transport (format décrit en tête de module).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut sortie = Vec::new();
        sortie.extend_from_slice(MAGIC);
        sortie.extend_from_slice(&VERSION.to_le_bytes());
        let nombre = u32::try_from(self.traits.len())
            .map_err(|_| NdError::Protocol("trop de traits pour le format (> u32)".into()))?;
        sortie.extend_from_slice(&nombre.to_le_bytes());
        for (id, stroke) in &self.traits {
            sortie.extend_from_slice(&id.to_le_bytes());
            stroke.encode(&mut sortie)?;
        }
        Ok(sortie)
    }

    /// Désérialise une couche produite par [`AnnotationLayer::to_bytes`].
    ///
    /// Refuse magic/version inconnus, flux tronqué et octets excédentaires.
    /// `prochain_id` repart au-dessus du plus grand identifiant lu : les ajouts
    /// ultérieurs ne peuvent pas entrer en collision.
    pub fn from_bytes(donnees: &[u8]) -> Result<AnnotationLayer> {
        let mut lecteur = Lecteur { donnees };
        if lecteur.prendre(MAGIC.len())? != MAGIC {
            return Err(NdError::Protocol(
                "magic NDANN1 absent : ce flux n'est pas une couche d'annotations".into(),
            ));
        }
        let version = lecteur.u16()?;
        if version != VERSION {
            return Err(NdError::Protocol(format!(
                "version d'annotations {version} non gérée (attendu {VERSION})"
            )));
        }
        let nombre = lecteur.u32()? as usize;
        let mut traits = Vec::new();
        let mut plus_grand_id = 0u64;
        for _ in 0..nombre {
            let id = lecteur.u64()?;
            let etiquette = lecteur.u8()?;
            traits.push((id, Stroke::decode(etiquette, &mut lecteur)?));
            plus_grand_id = plus_grand_id.max(id);
        }
        if !lecteur.donnees.is_empty() {
            return Err(NdError::Protocol(
                "octets excédentaires après la couche d'annotations".into(),
            ));
        }
        Ok(AnnotationLayer {
            traits,
            prochain_id: plus_grand_id.saturating_add(1).max(1),
        })
    }
}

impl Default for AnnotationLayer {
    fn default() -> Self {
        AnnotationLayer::new()
    }
}

/// Écrit un point `(x, y)` en deux `f32` petit-boutistes.
fn ecrire_point(sortie: &mut Vec<u8>, point: (f32, f32)) {
    sortie.extend_from_slice(&point.0.to_le_bytes());
    sortie.extend_from_slice(&point.1.to_le_bytes());
}

/// Curseur de lecture sur une tranche : toute lecture au-delà de la fin est
/// signalée comme flux tronqué, sans panique ni allocation démesurée.
struct Lecteur<'a> {
    donnees: &'a [u8],
}

impl<'a> Lecteur<'a> {
    /// Consomme `n` octets, ou signale un flux tronqué.
    fn prendre(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.donnees.len() < n {
            return Err(NdError::Protocol("couche d'annotations tronquée".into()));
        }
        let (tete, reste) = self.donnees.split_at(n);
        self.donnees = reste;
        Ok(tete)
    }

    /// Octets restant à lire.
    fn restant(&self) -> usize {
        self.donnees.len()
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.prendre(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let mut octets = [0u8; 2];
        octets.copy_from_slice(self.prendre(2)?);
        Ok(u16::from_le_bytes(octets))
    }

    fn u32(&mut self) -> Result<u32> {
        let mut octets = [0u8; 4];
        octets.copy_from_slice(self.prendre(4)?);
        Ok(u32::from_le_bytes(octets))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut octets = [0u8; 8];
        octets.copy_from_slice(self.prendre(8)?);
        Ok(u64::from_le_bytes(octets))
    }

    fn f32(&mut self) -> Result<f32> {
        let mut octets = [0u8; 4];
        octets.copy_from_slice(self.prendre(4)?);
        Ok(f32::from_le_bytes(octets))
    }

    fn point(&mut self) -> Result<(f32, f32)> {
        Ok((self.f32()?, self.f32()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une couche avec un trait de chaque type.
    fn couche_complete() -> AnnotationLayer {
        let mut couche = AnnotationLayer::new();
        couche.add(Stroke::Line {
            points: vec![(0.0, 0.0), (0.5, 0.25), (1.0, 1.0)],
            color: 0xFF00_00FF,
            width: 2.5,
        });
        couche.add(Stroke::Rect {
            min: (10.0, 20.0),
            max: (110.0, 220.0),
            color: 0x00FF_00FF,
            width: 1.0,
        });
        couche.add(Stroke::Ellipse {
            center: (64.0, 64.0),
            radii: (32.0, 16.0),
            color: 0x0000_FFFF,
            width: 3.0,
        });
        couche.add(Stroke::Arrow {
            from: (0.0, 0.0),
            to: (100.0, 50.0),
            color: 0xFFFF_00FF,
            width: 4.0,
        });
        couche.add(Stroke::Text {
            position: (5.0, 5.0),
            contenu: "Cliquez ici — été".into(),
            color: 0xFF00_FFFF,
            size: 14.0,
        });
        couche
    }

    fn trait_simple() -> Stroke {
        Stroke::Line {
            points: vec![(1.0, 2.0)],
            color: 0xFFFF_FFFF,
            width: 1.0,
        }
    }

    #[test]
    fn ajout_donne_des_ids_croissants_jamais_reutilises() {
        let mut couche = AnnotationLayer::new();
        assert!(couche.is_empty());
        let a = couche.add(trait_simple());
        let b = couche.add(trait_simple());
        let c = couche.add(trait_simple());
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(couche.len(), 3);

        // Gomme : suppression ciblée, idempotente.
        assert!(couche.remove(b));
        assert_eq!(couche.len(), 2);
        assert!(!couche.remove(b));
        assert!(!couche.remove(42));

        // Après effacement complet, les ids continuent de croître.
        couche.clear();
        assert!(couche.is_empty());
        assert_eq!(couche.add(trait_simple()), 4);
    }

    #[test]
    fn strokes_expose_l_ordre_de_dessin() {
        let couche = couche_complete();
        let ids: Vec<u64> = couche.strokes().iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn aller_retour_complet() {
        let couche = couche_complete();
        let octets = couche.to_bytes().unwrap();
        assert_eq!(&octets[..6], MAGIC);
        assert_eq!(u16::from_le_bytes([octets[6], octets[7]]), VERSION);

        let relue = AnnotationLayer::from_bytes(&octets).unwrap();
        assert_eq!(relue.strokes(), couche.strokes());
        assert_eq!(relue, couche);
    }

    #[test]
    fn aller_retour_couche_vide() {
        let octets = AnnotationLayer::new().to_bytes().unwrap();
        let relue = AnnotationLayer::from_bytes(&octets).unwrap();
        assert!(relue.is_empty());
    }

    #[test]
    fn apres_deserialisation_les_ids_repartent_au_dessus() {
        let couche = couche_complete();
        let mut relue = AnnotationLayer::from_bytes(&couche.to_bytes().unwrap()).unwrap();
        // 5 traits lus (ids 1..=5) : le prochain ajout doit recevoir 6.
        assert_eq!(relue.add(trait_simple()), 6);
    }

    #[test]
    fn magic_invalide_refuse() {
        assert!(AnnotationLayer::from_bytes(b"PASBON\x01\x00\x00\x00\x00\x00").is_err());
    }

    #[test]
    fn version_inconnue_refusee() {
        let mut octets = Vec::new();
        octets.extend_from_slice(MAGIC);
        octets.extend_from_slice(&99u16.to_le_bytes());
        octets.extend_from_slice(&0u32.to_le_bytes());
        assert!(AnnotationLayer::from_bytes(&octets).is_err());
    }

    #[test]
    fn troncature_detectee() {
        let octets = couche_complete().to_bytes().unwrap();
        for coupe in [octets.len() - 1, octets.len() - 5, 13, 7] {
            assert!(
                AnnotationLayer::from_bytes(&octets[..coupe]).is_err(),
                "troncature à {coupe} octets non détectée"
            );
        }
    }

    #[test]
    fn octets_excedentaires_refuses() {
        let mut octets = AnnotationLayer::new().to_bytes().unwrap();
        octets.push(0);
        assert!(AnnotationLayer::from_bytes(&octets).is_err());
    }

    #[test]
    fn type_de_trait_inconnu_refuse() {
        let mut octets = Vec::new();
        octets.extend_from_slice(MAGIC);
        octets.extend_from_slice(&VERSION.to_le_bytes());
        octets.extend_from_slice(&1u32.to_le_bytes());
        octets.extend_from_slice(&1u64.to_le_bytes()); // id
        octets.push(99); // étiquette inconnue
        assert!(AnnotationLayer::from_bytes(&octets).is_err());
    }

    #[test]
    fn polyligne_annoncee_trop_longue_refusee_sans_allocation() {
        let mut octets = Vec::new();
        octets.extend_from_slice(MAGIC);
        octets.extend_from_slice(&VERSION.to_le_bytes());
        octets.extend_from_slice(&1u32.to_le_bytes());
        octets.extend_from_slice(&1u64.to_le_bytes()); // id
        octets.push(1); // TAG_LINE
        octets.extend_from_slice(&u32::MAX.to_le_bytes()); // nombre de points hostile
        assert!(AnnotationLayer::from_bytes(&octets).is_err());
    }

    #[test]
    fn texte_non_utf8_refuse() {
        let mut couche = AnnotationLayer::new();
        couche.add(Stroke::Text {
            position: (0.0, 0.0),
            contenu: "ab".into(),
            color: 0,
            size: 12.0,
        });
        let mut octets = couche.to_bytes().unwrap();
        // Corrompt le premier octet du texte : en-tête 12 + id 8 + tag 1
        // + position 8 + longueur 4 = 33.
        octets[33] = 0xFF;
        octets[34] = 0xFE;
        assert!(AnnotationLayer::from_bytes(&octets).is_err());
    }
}
