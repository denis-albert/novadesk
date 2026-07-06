//! Annotation d'écran (« tableau blanc ») : traits dessinés par-dessus
//! l'image transmise, par le contrôleur ou le contrôlé, sans modifier le
//! bureau réel. Ce module fournit le modèle ([`Stroke`], [`AnnotationLayer`]),
//! sa sérialisation binaire pour le transport, et le **rendu** de la couche
//! en tampon RGBA ([`RgbaCanvas`], [`AnnotationLayer::render`]) prêt à être
//! superposé à l'image décodée (voir plan 13, §annotation).
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

// ---------------------------------------------------------------------------
// Rendu (rastérisation) de la couche en tampon RGBA
// ---------------------------------------------------------------------------

/// Tampon d'image RGBA 8 bits par canal — octets `[R, G, B, A]` par pixel,
/// lignes de haut en bas — destiné à être superposé à l'image décodée par
/// l'interface. Le tampon naît entièrement transparent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaCanvas {
    largeur: u32,
    hauteur: u32,
    pixels: Vec<u8>,
}

impl RgbaCanvas {
    /// Tampon transparent de `largeur × hauteur` pixels. L'appelant fournit
    /// des dimensions raisonnables (celles de l'écran superposé).
    #[must_use]
    pub fn new(largeur: u32, hauteur: u32) -> Self {
        RgbaCanvas {
            largeur,
            hauteur,
            pixels: vec![0; largeur as usize * hauteur as usize * 4],
        }
    }

    /// Largeur du tampon, en pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.largeur
    }

    /// Hauteur du tampon, en pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.hauteur
    }

    /// Les octets RGBA bruts (`largeur × hauteur × 4`), lignes de haut en bas.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Le pixel `(x, y)` sous forme `[R, G, B, A]`, ou `None` hors du tampon.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.largeur || y >= self.hauteur {
            return None;
        }
        let base = (y as usize * self.largeur as usize + x as usize) * 4;
        Some([
            self.pixels[base],
            self.pixels[base + 1],
            self.pixels[base + 2],
            self.pixels[base + 3],
        ])
    }

    /// Compose `couleur` (RGBA empaqueté `0xRRGGBBAA`, alpha direct) sur le
    /// pixel d'indice `indice` avec l'opérateur « source over ».
    fn composer(&mut self, indice: usize, couleur: u32) {
        let source = [
            (couleur >> 24) as u8,
            (couleur >> 16) as u8,
            (couleur >> 8) as u8,
            couleur as u8,
        ];
        if source[3] == 0 {
            return;
        }
        let base = indice * 4;
        let alpha_source = f32::from(source[3]) / 255.0;
        let alpha_fond = f32::from(self.pixels[base + 3]) / 255.0;
        let alpha_final = alpha_source + alpha_fond * (1.0 - alpha_source);
        let fond = &mut self.pixels[base..base + 3];
        for (canal, valeur_source) in fond.iter_mut().zip(source) {
            let valeur = (f32::from(valeur_source) * alpha_source
                + f32::from(*canal) * alpha_fond * (1.0 - alpha_source))
                / alpha_final;
            *canal = valeur.round() as u8;
        }
        self.pixels[base + 3] = (alpha_final * 255.0).round() as u8;
    }
}

impl AnnotationLayer {
    /// Rend la couche dans un tampon transparent de `largeur × hauteur`.
    /// Les coordonnées des traits sont interprétées en pixels du tampon
    /// (l'appelant convertit d'abord si son repère est normalisé).
    #[must_use]
    pub fn render(&self, largeur: u32, hauteur: u32) -> RgbaCanvas {
        let mut toile = RgbaCanvas::new(largeur, hauteur);
        self.render_into(&mut toile);
        toile
    }

    /// Rend la couche par-dessus le contenu existant de `toile`, dans l'ordre
    /// de dessin. Chaque trait est d'abord rastérisé dans un masque de
    /// couverture puis composé **une seule fois** par pixel : un trait
    /// semi-transparent ne fonce pas là où ses tampons internes se recouvrent.
    pub fn render_into(&self, toile: &mut RgbaCanvas) {
        let largeur = toile.largeur as usize;
        let hauteur = toile.hauteur as usize;
        if largeur == 0 || hauteur == 0 {
            return;
        }
        let mut masque = vec![false; largeur * hauteur];
        for (_, forme) in &self.traits {
            masque.fill(false);
            forme.rasteriser(&mut masque, largeur, hauteur);
            let couleur = forme.couleur();
            for (indice, couvert) in masque.iter().enumerate() {
                if *couvert {
                    toile.composer(indice, couleur);
                }
            }
        }
    }
}

impl Stroke {
    /// Couleur RGBA empaquetée du trait.
    fn couleur(&self) -> u32 {
        match self {
            Stroke::Line { color, .. }
            | Stroke::Rect { color, .. }
            | Stroke::Ellipse { color, .. }
            | Stroke::Arrow { color, .. }
            | Stroke::Text { color, .. } => *color,
        }
    }

    /// Marque dans `masque` (dimensions `largeur × hauteur`) les pixels
    /// couverts par le trait. Les coordonnées non finies sont ignorées sans
    /// paniquer ; tout est borné au tampon.
    fn rasteriser(&self, masque: &mut [bool], largeur: usize, hauteur: usize) {
        match self {
            Stroke::Line { points, width, .. } => {
                let rayon = rayon_de_trait(*width);
                if let [seul] = points.as_slice() {
                    marquer_disque(masque, largeur, hauteur, seul.0, seul.1, rayon);
                }
                for paire in points.windows(2) {
                    tracer_segment(masque, largeur, hauteur, paire[0], paire[1], rayon);
                }
            }
            Stroke::Rect {
                min, max, width, ..
            } => {
                let rayon = rayon_de_trait(*width);
                let (x0, x1) = (min.0.min(max.0), min.0.max(max.0));
                let (y0, y1) = (min.1.min(max.1), min.1.max(max.1));
                for (de, vers) in [
                    ((x0, y0), (x1, y0)),
                    ((x1, y0), (x1, y1)),
                    ((x1, y1), (x0, y1)),
                    ((x0, y1), (x0, y0)),
                ] {
                    tracer_segment(masque, largeur, hauteur, de, vers, rayon);
                }
            }
            Stroke::Ellipse {
                center,
                radii,
                width,
                ..
            } => {
                let rayon = rayon_de_trait(*width);
                let (rx, ry) = (radii.0.abs(), radii.1.abs());
                if !(center.0.is_finite()
                    && center.1.is_finite()
                    && rx.is_finite()
                    && ry.is_finite())
                {
                    return;
                }
                // Polygone régulier : assez de côtés pour que la corde reste
                // sous le demi-pixel, borné pour les rayons hostiles.
                let cotes = ((std::f32::consts::TAU * rx.max(ry)).ceil() as usize).clamp(16, 2048);
                let mut precedent = (center.0 + rx, center.1);
                for i in 1..=cotes {
                    let angle = std::f32::consts::TAU * (i as f32) / (cotes as f32);
                    let courant = (center.0 + rx * angle.cos(), center.1 + ry * angle.sin());
                    tracer_segment(masque, largeur, hauteur, precedent, courant, rayon);
                    precedent = courant;
                }
            }
            Stroke::Arrow {
                from, to, width, ..
            } => {
                let rayon = rayon_de_trait(*width);
                tracer_segment(masque, largeur, hauteur, *from, *to, rayon);
                let (dx, dy) = (to.0 - from.0, to.1 - from.1);
                let longueur = (dx * dx + dy * dy).sqrt();
                if !longueur.is_finite() || longueur <= f32::EPSILON {
                    return;
                }
                let (ux, uy) = (dx / longueur, dy / longueur);
                // Tête : deux barbillons à ±30° du corps, ~4× l'épaisseur,
                // jamais plus longs que la flèche elle-même.
                let angle_tete = 30.0_f32.to_radians();
                let (cos_a, sin_a) = (angle_tete.cos(), angle_tete.sin());
                let longueur_tete = (4.0 * width.max(1.0)).clamp(6.0, longueur);
                for signe in [1.0f32, -1.0] {
                    let direction = (
                        ux * cos_a - uy * sin_a * signe,
                        ux * sin_a * signe + uy * cos_a,
                    );
                    let barbillon = (
                        to.0 - longueur_tete * direction.0,
                        to.1 - longueur_tete * direction.1,
                    );
                    tracer_segment(masque, largeur, hauteur, *to, barbillon, rayon);
                }
            }
            Stroke::Text {
                position,
                contenu,
                size,
                ..
            } => {
                // Le rendu des glyphes appartient à l'interface (polices
                // système) : ici, un soulignement matérialise l'emprise du
                // texte pour la superposition.
                let caracteres = contenu.chars().count();
                if caracteres == 0 {
                    return;
                }
                let emprise = 0.6 * size * caracteres as f32;
                let rayon = rayon_de_trait((size / 10.0).max(1.0));
                tracer_segment(
                    masque,
                    largeur,
                    hauteur,
                    *position,
                    (position.0 + emprise, position.1),
                    rayon,
                );
            }
        }
    }
}

/// Rayon du pinceau pour une épaisseur de trait donnée (au moins un
/// demi-pixel, pour qu'un trait fin marque quand même).
fn rayon_de_trait(width: f32) -> f32 {
    if width.is_finite() {
        (width * 0.5).max(0.5)
    } else {
        0.5
    }
}

/// Trace le segment `de → vers` dans le masque en tamponnant un disque de
/// `rayon` tous les demi-pixels (aucune lacune, même en diagonale fine). Le
/// segment est d'abord coupé au tampon élargi du rayon : le coût reste borné
/// par la taille de la toile, même pour des coordonnées lointaines.
fn tracer_segment(
    masque: &mut [bool],
    largeur: usize,
    hauteur: usize,
    de: (f32, f32),
    vers: (f32, f32),
    rayon: f32,
) {
    if !(de.0.is_finite() && de.1.is_finite() && vers.0.is_finite() && vers.1.is_finite()) {
        return;
    }
    // Un pinceau plus large que la toile la couvre de toute façon : borner le
    // rayon garde la marge de coupe (et donc le parcours) proportionnelle.
    let rayon = rayon.min((largeur + hauteur) as f32 + 1.0);
    let Some((p, q)) = couper_segment(de, vers, largeur, hauteur, rayon) else {
        return;
    };
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let longueur = (dx * dx + dy * dy).sqrt();
    let pas = ((longueur * 2.0).ceil() as usize).max(1);
    for i in 0..=pas {
        let t = i as f32 / pas as f32;
        marquer_disque(masque, largeur, hauteur, p.0 + t * dx, p.1 + t * dy, rayon);
    }
}

/// Coupe le segment `p → q` au rectangle `[-marge, largeur + marge] ×
/// [-marge, hauteur + marge]` (algorithme de Liang-Barsky). `None` si le
/// segment passe entièrement à côté.
fn couper_segment(
    p: (f32, f32),
    q: (f32, f32),
    largeur: usize,
    hauteur: usize,
    marge: f32,
) -> Option<((f32, f32), (f32, f32))> {
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    let bornes = [
        (-dx, p.0 + marge),                 // x >= -marge
        (dx, largeur as f32 + marge - p.0), // x <= largeur + marge
        (-dy, p.1 + marge),                 // y >= -marge
        (dy, hauteur as f32 + marge - p.1), // y <= hauteur + marge
    ];
    for (pente, distance) in bornes {
        if pente == 0.0 {
            if distance < 0.0 {
                return None; // parallèle à la borne, entièrement dehors
            }
            continue;
        }
        let t = distance / pente;
        if pente < 0.0 {
            if t > t1 {
                return None;
            }
            if t > t0 {
                t0 = t;
            }
        } else {
            if t < t0 {
                return None;
            }
            if t < t1 {
                t1 = t;
            }
        }
    }
    Some((
        (p.0 + t0 * dx, p.1 + t0 * dy),
        (p.0 + t1 * dx, p.1 + t1 * dy),
    ))
}

/// Marque les pixels dont le **centre** est à `rayon` ou moins de `(cx, cy)`.
fn marquer_disque(
    masque: &mut [bool],
    largeur: usize,
    hauteur: usize,
    cx: f32,
    cy: f32,
    rayon: f32,
) {
    if !(cx.is_finite() && cy.is_finite() && rayon.is_finite()) {
        return;
    }
    let rayon = rayon.max(0.5);
    // Boîte englobante, bornée au tampon (les conversions f32 → usize
    // saturent : une boîte entièrement dehors donne des bornes vides).
    let x0 = (cx - rayon).floor().max(0.0) as usize;
    let y0 = (cy - rayon).floor().max(0.0) as usize;
    let x1 = (((cx + rayon).ceil() + 1.0).max(0.0) as usize).min(largeur);
    let y1 = (((cy + rayon).ceil() + 1.0).max(0.0) as usize).min(hauteur);
    let rayon_carre = rayon * rayon;
    for y in y0..y1 {
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= rayon_carre {
                masque[y * largeur + x] = true;
            }
        }
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

#[cfg(test)]
mod tests_rendu {
    use super::*;

    const ROUGE: u32 = 0xFF00_00FF;
    const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

    fn couche_avec(forme: Stroke) -> AnnotationLayer {
        let mut couche = AnnotationLayer::new();
        couche.add(forme);
        couche
    }

    #[test]
    fn ligne_horizontale_marque_sa_rangee() {
        let toile = couche_avec(Stroke::Line {
            points: vec![(2.5, 10.5), (20.5, 10.5)],
            color: ROUGE,
            width: 1.0,
        })
        .render(32, 32);
        assert_eq!(toile.pixel(10, 10), Some([255, 0, 0, 255]));
        assert_eq!(toile.pixel(2, 10), Some([255, 0, 0, 255])); // départ
        assert_eq!(toile.pixel(20, 10), Some([255, 0, 0, 255])); // arrivée
        assert_eq!(toile.pixel(10, 9), Some(TRANSPARENT)); // rangée au-dessus
        assert_eq!(toile.pixel(10, 11), Some(TRANSPARENT)); // rangée en dessous
        assert_eq!(toile.pixel(0, 10), Some(TRANSPARENT)); // avant le départ
    }

    #[test]
    fn epaisseur_couvre_plusieurs_rangees() {
        let toile = couche_avec(Stroke::Line {
            points: vec![(2.5, 10.5), (20.5, 10.5)],
            color: ROUGE,
            width: 5.0,
        })
        .render(32, 32);
        // Rayon 2,5 : les rangées à ±2 pixels sont couvertes, pas ±3.
        assert_eq!(toile.pixel(10, 8).unwrap()[3], 255);
        assert_eq!(toile.pixel(10, 12).unwrap()[3], 255);
        assert_eq!(toile.pixel(10, 7), Some(TRANSPARENT));
        assert_eq!(toile.pixel(10, 13), Some(TRANSPARENT));
    }

    #[test]
    fn rectangle_contour_sans_interieur() {
        let toile = couche_avec(Stroke::Rect {
            min: (2.5, 3.5),
            max: (10.5, 8.5),
            color: ROUGE,
            width: 1.0,
        })
        .render(16, 16);
        assert_eq!(toile.pixel(6, 3).unwrap()[3], 255); // bord haut
        assert_eq!(toile.pixel(6, 8).unwrap()[3], 255); // bord bas
        assert_eq!(toile.pixel(2, 5).unwrap()[3], 255); // bord gauche
        assert_eq!(toile.pixel(10, 5).unwrap()[3], 255); // bord droit
        assert_eq!(toile.pixel(6, 5), Some(TRANSPARENT)); // intérieur vide
    }

    #[test]
    fn ellipse_passe_par_ses_extremes() {
        let toile = couche_avec(Stroke::Ellipse {
            center: (16.5, 16.5),
            radii: (8.0, 5.0),
            color: ROUGE,
            width: 1.0,
        })
        .render(32, 32);
        assert_eq!(toile.pixel(24, 16).unwrap()[3], 255); // centre + rx
        assert_eq!(toile.pixel(8, 16).unwrap()[3], 255); // centre − rx
        assert_eq!(toile.pixel(16, 21).unwrap()[3], 255); // centre + ry
        assert_eq!(toile.pixel(16, 11).unwrap()[3], 255); // centre − ry
        assert_eq!(toile.pixel(16, 16), Some(TRANSPARENT)); // centre vide
    }

    #[test]
    fn fleche_corps_et_barbillons() {
        let toile = couche_avec(Stroke::Arrow {
            from: (2.5, 10.5),
            to: (20.5, 10.5),
            color: ROUGE,
            width: 2.0,
        })
        .render(32, 32);
        assert_eq!(toile.pixel(10, 10).unwrap()[3], 255); // corps
                                                          // Barbillons à ±30°, longueur 8 : extrémités vers (13,6) et (13,14).
        assert_eq!(toile.pixel(13, 6).unwrap()[3], 255);
        assert_eq!(toile.pixel(13, 14).unwrap()[3], 255);
        assert_eq!(toile.pixel(2, 5), Some(TRANSPARENT)); // loin de la tête
    }

    #[test]
    fn texte_rend_un_soulignement() {
        let toile = couche_avec(Stroke::Text {
            position: (5.5, 20.5),
            contenu: "ab".into(),
            color: ROUGE,
            size: 10.0,
        })
        .render(32, 32);
        // Emprise : 0,6 × 10 × 2 = 12 pixels à partir de la position.
        assert_eq!(toile.pixel(10, 20).unwrap()[3], 255);
        assert_eq!(toile.pixel(17, 20).unwrap()[3], 255);
        assert_eq!(toile.pixel(25, 20), Some(TRANSPARENT));
        assert_eq!(toile.pixel(10, 10), Some(TRANSPARENT));
    }

    #[test]
    fn trait_semi_transparent_compose_une_seule_fois() {
        let toile = couche_avec(Stroke::Line {
            points: vec![(2.5, 10.5), (20.5, 10.5)],
            color: 0xFF00_0080, // rouge, alpha 128
            width: 6.0,
        })
        .render(32, 32);
        // Les tampons internes se recouvrent, mais chaque pixel n'est composé
        // qu'une fois : l'alpha reste exactement celui du trait.
        assert_eq!(toile.pixel(10, 10), Some([255, 0, 0, 128]));
    }

    #[test]
    fn les_traits_se_composent_dans_l_ordre() {
        let mut couche = AnnotationLayer::new();
        couche.add(Stroke::Line {
            points: vec![(2.5, 10.5), (20.5, 10.5)],
            color: 0xFF00_00FF, // rouge opaque
            width: 3.0,
        });
        couche.add(Stroke::Line {
            points: vec![(2.5, 10.5), (20.5, 10.5)],
            color: 0x00FF_0080, // vert semi-transparent par-dessus
            width: 3.0,
        });
        let toile = couche.render(32, 32);
        let pixel = toile.pixel(10, 10).unwrap();
        assert_eq!(pixel[3], 255); // le fond opaque le reste
        assert!(pixel[1] > 100); // le vert s'est déposé
        assert!(pixel[0] > 50 && pixel[0] < 200); // le rouge transparaît
    }

    #[test]
    fn point_isole_marque() {
        let toile = couche_avec(Stroke::Line {
            points: vec![(8.5, 8.5)],
            color: ROUGE,
            width: 3.0,
        })
        .render(16, 16);
        assert_eq!(toile.pixel(8, 8).unwrap()[3], 255);
        assert_eq!(toile.pixel(8, 11), Some(TRANSPARENT));
    }

    #[test]
    fn coordonnees_hostiles_sans_panique() {
        let mut couche = AnnotationLayer::new();
        // Segment gigantesque et très épais : coupé puis borné à la toile.
        couche.add(Stroke::Line {
            points: vec![(-1e9, -1e9), (1e9, 1e9)],
            color: ROUGE,
            width: 1e9,
        });
        // Coordonnées non finies : ignorées sans paniquer.
        couche.add(Stroke::Line {
            points: vec![(f32::NAN, 0.0), (10.0, f32::INFINITY)],
            color: ROUGE,
            width: f32::NAN,
        });
        couche.add(Stroke::Ellipse {
            center: (16.0, 16.0),
            radii: (f32::INFINITY, 4.0),
            color: ROUGE,
            width: 1.0,
        });
        let toile = couche.render(16, 16);
        // Le premier trait couvre la diagonale (pinceau borné à la toile).
        assert!(toile.pixel(8, 8).unwrap()[3] > 0);
    }

    #[test]
    fn toile_vide_et_couche_vide_sans_panique() {
        let toile = couche_avec(Stroke::Rect {
            min: (0.0, 0.0),
            max: (4.0, 4.0),
            color: ROUGE,
            width: 1.0,
        })
        .render(0, 0);
        assert!(toile.pixels().is_empty());
        assert_eq!(toile.pixel(0, 0), None);

        let vide = AnnotationLayer::new().render(4, 4);
        assert!(vide.pixels().iter().all(|octet| *octet == 0));
    }

    #[test]
    fn render_into_preserve_le_fond() {
        let mut toile = RgbaCanvas::new(8, 8);
        couche_avec(Stroke::Line {
            points: vec![(0.5, 1.5), (6.5, 1.5)],
            color: ROUGE,
            width: 1.0,
        })
        .render_into(&mut toile);
        // Deuxième passe par-dessus la première, ailleurs.
        couche_avec(Stroke::Line {
            points: vec![(0.5, 5.5), (6.5, 5.5)],
            color: 0x00FF_00FF,
            width: 1.0,
        })
        .render_into(&mut toile);
        assert_eq!(toile.pixel(3, 1), Some([255, 0, 0, 255]));
        assert_eq!(toile.pixel(3, 5), Some([0, 255, 0, 255]));
    }
}
