//! Protocole du **canal local service ↔ assistant** : un encadrement binaire
//! simple et robuste, porté par un tube nommé Windows (voir [`crate::tube`]) mais
//! **indépendant de la plateforme** ici pour rester testable partout.
//!
//! # Deux sens, deux jeux de messages
//!
//! ```text
//! service  ──►  assistant : [`MessageService`]   (configuration, entrées, région,
//!                                                  bascule moniteur, arrêt)
//! assistant ──► service   : [`MessageAssistant`] (trames capturées, événements de
//!                                                  capture, moniteurs, erreurs, prêt)
//! ```
//!
//! # Encadrement
//!
//! Chaque message est précédé de sa longueur : `[len u32 big-endian][charge]`. La
//! charge commence par un **octet d'étiquette** (le variant), suivi des champs. La
//! longueur est bornée ([`LEN_MAX`]) : une trame hostile ou corrompue ne peut pas
//! provoquer d'allocation démesurée (le tube est local et entre nos propres
//! processus, mais la robustesse est gratuite).
//!
//! Les types transportés sont **directement** ceux de `nd-capture` / `nd-input`
//! (pas de type parallèle) : le capteur côté service ([`crate::pont::CapteurAssistant`])
//! reconstruit un [`nd_capture::CapturedFrame`] prêt pour l'encodeur, et
//! [`appliquer_entree`] rejoue un [`EvenementEntree`] sur un injecteur réel.

use std::io::{self, Read, Write};

use nd_capture::{
    CaptureEvent, CapturedFrame, CursorState, FrameImage, MonitorInfo, PixelFormat, Rect,
};
use nd_input::MouseButton;
use nd_proto::MonitorId;

/// Taille maximale d'une charge de message (256 Mio) : borne de sûreté contre une
/// longueur corrompue. Une trame 8K BGRA plein cadre (~132 Mio) reste sous ce seuil.
pub const LEN_MAX: usize = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Événement d'entrée (service → assistant) : une action de l'injecteur
// ---------------------------------------------------------------------------

/// Une action d'entrée à rejouer côté assistant, image des méthodes de
/// [`nd_input::InputInjector`]. Sérialisée sur le canal, appliquée par
/// [`appliquer_entree`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EvenementEntree {
    /// Curseur en coordonnées absolues normalisées (0.0–1.0) sur un moniteur.
    SourisAbsolue { x: f64, y: f64, monitor: u32 },
    /// Déplacement relatif du curseur (mode jeu).
    SourisRelative { dx: f64, dy: f64 },
    /// Bouton de souris pressé/relâché.
    Bouton { bouton: MouseButton, enfonce: bool },
    /// Molette (défilement horizontal/vertical, unités haute résolution).
    Molette { dx: f64, dy: f64 },
    /// Touche par scancode physique.
    Touche { scancode: u32, enfonce: bool },
    /// Saisie d'un caractère Unicode.
    Unicode { caractere: char },
    /// Relâche toutes les touches/boutons (anti « stuck key »).
    ToutRelacher,
}

/// Rejoue un [`EvenementEntree`] sur l'injecteur `injecteur` (côté assistant, dans
/// la session interactive). Propage l'erreur d'injection éventuelle.
///
/// # Errors
/// Erreur si l'injection sous-jacente échoue (voir [`nd_input::InputInjector`]).
pub fn appliquer_entree(
    injecteur: &dyn nd_input::InputInjector,
    evenement: EvenementEntree,
) -> nd_proto::Result<()> {
    match evenement {
        EvenementEntree::SourisAbsolue { x, y, monitor } => {
            injecteur.mouse_move_abs(x, y, MonitorId(monitor))
        }
        EvenementEntree::SourisRelative { dx, dy } => injecteur.mouse_move_rel(dx, dy),
        EvenementEntree::Bouton { bouton, enfonce } => injecteur.mouse_button(bouton, enfonce),
        EvenementEntree::Molette { dx, dy } => injecteur.scroll(dx, dy),
        EvenementEntree::Touche { scancode, enfonce } => injecteur.key(scancode, enfonce),
        EvenementEntree::Unicode { caractere } => injecteur.unicode(caractere),
        EvenementEntree::ToutRelacher => {
            injecteur.release_all();
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Message émis par le **service** vers l'**assistant**.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageService {
    /// (Re)configure la capture : moniteur, cadence cible, capture du curseur.
    Configurer {
        monitor: u32,
        fps: u32,
        curseur: bool,
    },
    /// Une entrée à injecter dans la session interactive.
    Entree(EvenementEntree),
    /// Restreint la capture à une sous-région (« cadre d'écran »), ou plein écran (`None`).
    DefinirRegion(Option<Rect>),
    /// Demande la diffusion d'un autre moniteur (index).
    BasculerMoniteur(u32),
    /// Fin de session : l'assistant doit s'arrêter proprement.
    Arret,
}

/// Message émis par l'**assistant** vers le **service**.
///
/// [`PartialEq`] est **manuel** : [`nd_capture::CapturedFrame`] (et ses champs
/// `CursorState`/`FrameImage`) ne l'implémentent pas — on compare champ à champ
/// (utile aux tests d'aller-retour).
#[derive(Debug, Clone)]
pub enum MessageAssistant {
    /// L'assistant est connecté et a démarré la capture (poignée de main).
    Pret,
    /// Une trame capturée du bureau interactif.
    Trame(Box<CapturedFrame>),
    /// Un événement hors flux (changement de résolution, bureau sécurisé).
    Evenement(CaptureEvent),
    /// La liste des moniteurs de l'hôte (énumérés côté session interactive).
    Moniteurs(Vec<MonitorInfo>),
    /// Une erreur non fatale rencontrée côté assistant (journalisée par le service).
    Erreur(String),
}

impl PartialEq for MessageAssistant {
    fn eq(&self, autre: &Self) -> bool {
        match (self, autre) {
            (MessageAssistant::Pret, MessageAssistant::Pret) => true,
            (MessageAssistant::Trame(a), MessageAssistant::Trame(b)) => trames_egales(a, b),
            (MessageAssistant::Evenement(a), MessageAssistant::Evenement(b)) => a == b,
            (MessageAssistant::Moniteurs(a), MessageAssistant::Moniteurs(b)) => a == b,
            (MessageAssistant::Erreur(a), MessageAssistant::Erreur(b)) => a == b,
            _ => false,
        }
    }
}

/// Égalité champ à champ de deux trames capturées (curseur et image compris).
fn trames_egales(a: &CapturedFrame, b: &CapturedFrame) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.monitor == b.monitor
        && a.format == b.format
        && a.dirty == b.dirty
        && a.timestamp_us == b.timestamp_us
        && curseurs_egaux(a.cursor, b.cursor)
        && images_egales(a.image.as_ref(), b.image.as_ref())
}

/// Égalité de deux états de curseur (`CursorState` n'est pas `PartialEq`).
fn curseurs_egaux(a: Option<CursorState>, b: Option<CursorState>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.visible == b.visible,
        _ => false,
    }
}

/// Égalité de deux images (`FrameImage` n'est pas `PartialEq`).
fn images_egales(a: Option<&FrameImage>, b: Option<&FrameImage>) -> bool {
    match (a, b) {
        (None, None) => true,
        (
            Some(FrameImage::Cpu {
                data: da,
                stride: sa,
            }),
            Some(FrameImage::Cpu {
                data: db,
                stride: sb,
            }),
        ) => da == db && sa == sb,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Encadrement générique (longueur préfixée)
// ---------------------------------------------------------------------------

/// Écrit une charge encadrée `[len u32 BE][charge]` puis vide le tampon.
fn ecrire_cadre<W: Write>(flux: &mut W, charge: &[u8]) -> io::Result<()> {
    let len = u32::try_from(charge.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message trop grand"))?;
    flux.write_all(&len.to_be_bytes())?;
    flux.write_all(charge)?;
    flux.flush()
}

/// Lit une charge encadrée. Un EOF **au début** de l'en-tête remonte en
/// [`io::ErrorKind::UnexpectedEof`] (déconnexion propre côté lecteur).
fn lire_cadre<R: Read>(flux: &mut R) -> io::Result<Vec<u8>> {
    let mut entete = [0u8; 4];
    flux.read_exact(&mut entete)?;
    let len = u32::from_be_bytes(entete) as usize;
    if len > LEN_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("longueur de message {len} au-delà de la borne {LEN_MAX}"),
        ));
    }
    let mut charge = vec![0u8; len];
    flux.read_exact(&mut charge)?;
    Ok(charge)
}

/// Charge malformée ⇒ erreur d'E/S homogène (jamais de panique sur entrée hostile).
fn corrompu(quoi: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("message {quoi} corrompu"),
    )
}

// ---------------------------------------------------------------------------
// Sérialisation : petites primitives sur Vec<u8> et curseur d'octets
// ---------------------------------------------------------------------------

/// Tampon d'écriture séquentiel (accumulateur de la charge d'un message).
#[derive(Default)]
struct Sortie {
    octets: Vec<u8>,
}

impl Sortie {
    fn u8(&mut self, v: u8) {
        self.octets.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.octets.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.octets.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.octets.extend_from_slice(&v.to_be_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.octets.extend_from_slice(&v.to_be_bytes());
    }
    fn bool(&mut self, v: bool) {
        self.octets.push(u8::from(v));
    }
    /// Chaîne longueur-préfixée `[len u16 BE][utf8]` (longueur plafonnée à `u16::MAX`).
    fn chaine(&mut self, v: &str) {
        let o = v.as_bytes();
        let len = u16::try_from(o.len()).unwrap_or(u16::MAX);
        self.octets.extend_from_slice(&len.to_be_bytes());
        self.octets.extend_from_slice(&o[..usize::from(len)]);
    }
    /// Bloc d'octets longueur-préfixé `[len u64 BE][octets]` (pixels d'une trame).
    fn bloc(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.octets.extend_from_slice(v);
    }
}

/// Curseur de lecture sur une charge reçue ; chaque lecture avance ou rend `None`.
struct Entree<'a> {
    reste: &'a [u8],
}

impl<'a> Entree<'a> {
    fn new(octets: &'a [u8]) -> Self {
        Entree { reste: octets }
    }
    fn prendre(&mut self, n: usize) -> Option<&'a [u8]> {
        let (t, r) = self.reste.split_at_checked(n)?;
        self.reste = r;
        Some(t)
    }
    fn u8(&mut self) -> Option<u8> {
        self.prendre(1).map(|t| t[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.prendre(4)
            .map(|t| u32::from_be_bytes(t.try_into().ok().unwrap()))
    }
    fn i32(&mut self) -> Option<i32> {
        self.prendre(4)
            .map(|t| i32::from_be_bytes(t.try_into().ok().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.prendre(8)
            .map(|t| u64::from_be_bytes(t.try_into().ok().unwrap()))
    }
    fn f64(&mut self) -> Option<f64> {
        self.prendre(8)
            .map(|t| f64::from_be_bytes(t.try_into().ok().unwrap()))
    }
    fn bool(&mut self) -> Option<bool> {
        self.u8().map(|v| v != 0)
    }
    fn chaine(&mut self) -> Option<String> {
        let len = usize::from(u16::from_be_bytes(self.prendre(2)?.try_into().ok()?));
        String::from_utf8(self.prendre(len)?.to_vec()).ok()
    }
    fn bloc(&mut self) -> Option<Vec<u8>> {
        let len = usize::try_from(self.u64()?).ok()?;
        Some(self.prendre(len)?.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Codage des types nd-capture / nd-input
// ---------------------------------------------------------------------------

fn code_format(f: PixelFormat) -> u8 {
    match f {
        PixelFormat::Bgra8 => 0,
        PixelFormat::Rgba8 => 1,
        PixelFormat::Nv12 => 2,
    }
}

fn format_depuis_code(c: u8) -> Option<PixelFormat> {
    match c {
        0 => Some(PixelFormat::Bgra8),
        1 => Some(PixelFormat::Rgba8),
        2 => Some(PixelFormat::Nv12),
        _ => None,
    }
}

fn code_bouton(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

fn bouton_depuis_code(c: u8) -> Option<MouseButton> {
    match c {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Right),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::X1),
        4 => Some(MouseButton::X2),
        _ => None,
    }
}

fn ecrire_rect(s: &mut Sortie, r: Rect) {
    s.u32(r.x);
    s.u32(r.y);
    s.u32(r.w);
    s.u32(r.h);
}

fn lire_rect(e: &mut Entree<'_>) -> Option<Rect> {
    Some(Rect {
        x: e.u32()?,
        y: e.u32()?,
        w: e.u32()?,
        h: e.u32()?,
    })
}

fn ecrire_trame(s: &mut Sortie, f: &CapturedFrame) {
    s.u32(f.width);
    s.u32(f.height);
    s.u32(f.monitor.0);
    s.u8(code_format(f.format));
    s.u32(u32::try_from(f.dirty.len()).unwrap_or(u32::MAX));
    for r in f.dirty.iter().take(u32::MAX as usize) {
        ecrire_rect(s, *r);
    }
    match f.cursor {
        Some(c) => {
            s.bool(true);
            s.i32(c.x);
            s.i32(c.y);
            s.bool(c.visible);
        }
        None => s.bool(false),
    }
    s.u64(f.timestamp_us);
    match &f.image {
        Some(FrameImage::Cpu { data, stride }) => {
            s.bool(true);
            s.u64(*stride as u64);
            s.bloc(data);
        }
        None => s.bool(false),
    }
}

fn lire_trame(e: &mut Entree<'_>) -> Option<CapturedFrame> {
    let width = e.u32()?;
    let height = e.u32()?;
    let monitor = MonitorId(e.u32()?);
    let format = format_depuis_code(e.u8()?)?;
    let n = e.u32()? as usize;
    let mut dirty = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        dirty.push(lire_rect(e)?);
    }
    let cursor = if e.bool()? {
        Some(CursorState {
            x: e.i32()?,
            y: e.i32()?,
            visible: e.bool()?,
        })
    } else {
        None
    };
    let timestamp_us = e.u64()?;
    let image = if e.bool()? {
        let stride = usize::try_from(e.u64()?).ok()?;
        let data = e.bloc()?;
        Some(FrameImage::Cpu { data, stride })
    } else {
        None
    };
    Some(CapturedFrame {
        width,
        height,
        monitor,
        format,
        dirty,
        cursor,
        timestamp_us,
        image,
    })
}

fn ecrire_moniteur(s: &mut Sortie, m: &MonitorInfo) {
    s.u32(m.id.0);
    s.chaine(&m.name);
    s.u32(m.width);
    s.u32(m.height);
    s.i32(m.x);
    s.i32(m.y);
    s.bool(m.is_primary);
}

fn lire_moniteur(e: &mut Entree<'_>) -> Option<MonitorInfo> {
    Some(MonitorInfo {
        id: MonitorId(e.u32()?),
        name: e.chaine()?,
        width: e.u32()?,
        height: e.u32()?,
        x: e.i32()?,
        y: e.i32()?,
        is_primary: e.bool()?,
    })
}

fn ecrire_evenement_entree(s: &mut Sortie, ev: EvenementEntree) {
    match ev {
        EvenementEntree::SourisAbsolue { x, y, monitor } => {
            s.u8(1);
            s.f64(x);
            s.f64(y);
            s.u32(monitor);
        }
        EvenementEntree::SourisRelative { dx, dy } => {
            s.u8(2);
            s.f64(dx);
            s.f64(dy);
        }
        EvenementEntree::Bouton { bouton, enfonce } => {
            s.u8(3);
            s.u8(code_bouton(bouton));
            s.bool(enfonce);
        }
        EvenementEntree::Molette { dx, dy } => {
            s.u8(4);
            s.f64(dx);
            s.f64(dy);
        }
        EvenementEntree::Touche { scancode, enfonce } => {
            s.u8(5);
            s.u32(scancode);
            s.bool(enfonce);
        }
        EvenementEntree::Unicode { caractere } => {
            s.u8(6);
            s.u32(caractere as u32);
        }
        EvenementEntree::ToutRelacher => s.u8(7),
    }
}

fn lire_evenement_entree(e: &mut Entree<'_>) -> Option<EvenementEntree> {
    match e.u8()? {
        1 => Some(EvenementEntree::SourisAbsolue {
            x: e.f64()?,
            y: e.f64()?,
            monitor: e.u32()?,
        }),
        2 => Some(EvenementEntree::SourisRelative {
            dx: e.f64()?,
            dy: e.f64()?,
        }),
        3 => Some(EvenementEntree::Bouton {
            bouton: bouton_depuis_code(e.u8()?)?,
            enfonce: e.bool()?,
        }),
        4 => Some(EvenementEntree::Molette {
            dx: e.f64()?,
            dy: e.f64()?,
        }),
        5 => Some(EvenementEntree::Touche {
            scancode: e.u32()?,
            enfonce: e.bool()?,
        }),
        6 => Some(EvenementEntree::Unicode {
            caractere: char::from_u32(e.u32()?)?,
        }),
        7 => Some(EvenementEntree::ToutRelacher),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// API publique : écriture / lecture des deux jeux de messages
// ---------------------------------------------------------------------------

/// Sérialise un [`MessageService`] et l'écrit (encadré) sur `flux`.
///
/// # Errors
/// Erreur d'E/S d'écriture, ou message trop grand pour l'encadrement u32.
pub fn ecrire_service<W: Write>(flux: &mut W, msg: &MessageService) -> io::Result<()> {
    let mut s = Sortie::default();
    match msg {
        MessageService::Configurer {
            monitor,
            fps,
            curseur,
        } => {
            s.u8(1);
            s.u32(*monitor);
            s.u32(*fps);
            s.bool(*curseur);
        }
        MessageService::Entree(ev) => {
            s.u8(2);
            ecrire_evenement_entree(&mut s, *ev);
        }
        MessageService::DefinirRegion(region) => {
            s.u8(3);
            match region {
                Some(r) => {
                    s.bool(true);
                    ecrire_rect(&mut s, *r);
                }
                None => s.bool(false),
            }
        }
        MessageService::BasculerMoniteur(index) => {
            s.u8(4);
            s.u32(*index);
        }
        MessageService::Arret => s.u8(5),
    }
    ecrire_cadre(flux, &s.octets)
}

/// Lit et décode un [`MessageService`] depuis `flux` (bloquant).
///
/// # Errors
/// [`io::ErrorKind::UnexpectedEof`] à la déconnexion, [`io::ErrorKind::InvalidData`]
/// sur trame corrompue, ou toute erreur d'E/S de lecture.
pub fn lire_service<R: Read>(flux: &mut R) -> io::Result<MessageService> {
    let charge = lire_cadre(flux)?;
    decoder_service(&charge).ok_or_else(|| corrompu("service"))
}

fn decoder_service(charge: &[u8]) -> Option<MessageService> {
    let mut e = Entree::new(charge);
    match e.u8()? {
        1 => Some(MessageService::Configurer {
            monitor: e.u32()?,
            fps: e.u32()?,
            curseur: e.bool()?,
        }),
        2 => Some(MessageService::Entree(lire_evenement_entree(&mut e)?)),
        3 => {
            let region = if e.bool()? {
                Some(lire_rect(&mut e)?)
            } else {
                None
            };
            Some(MessageService::DefinirRegion(region))
        }
        4 => Some(MessageService::BasculerMoniteur(e.u32()?)),
        5 => Some(MessageService::Arret),
        _ => None,
    }
}

/// Sérialise un [`MessageAssistant`] et l'écrit (encadré) sur `flux`.
///
/// # Errors
/// Erreur d'E/S d'écriture, ou message trop grand pour l'encadrement u32.
pub fn ecrire_assistant<W: Write>(flux: &mut W, msg: &MessageAssistant) -> io::Result<()> {
    let mut s = Sortie::default();
    match msg {
        MessageAssistant::Pret => s.u8(1),
        MessageAssistant::Trame(f) => {
            s.u8(2);
            ecrire_trame(&mut s, f);
        }
        MessageAssistant::Evenement(ev) => {
            s.u8(3);
            s.u8(match ev {
                CaptureEvent::ResolutionChanged => 1,
                CaptureEvent::SecureDesktop => 2,
            });
        }
        MessageAssistant::Moniteurs(liste) => {
            s.u8(4);
            s.u32(u32::try_from(liste.len()).unwrap_or(u32::MAX));
            for m in liste {
                ecrire_moniteur(&mut s, m);
            }
        }
        MessageAssistant::Erreur(txt) => {
            s.u8(5);
            s.chaine(txt);
        }
    }
    ecrire_cadre(flux, &s.octets)
}

/// Lit et décode un [`MessageAssistant`] depuis `flux` (bloquant).
///
/// # Errors
/// [`io::ErrorKind::UnexpectedEof`] à la déconnexion, [`io::ErrorKind::InvalidData`]
/// sur trame corrompue, ou toute erreur d'E/S de lecture.
pub fn lire_assistant<R: Read>(flux: &mut R) -> io::Result<MessageAssistant> {
    let charge = lire_cadre(flux)?;
    decoder_assistant(&charge).ok_or_else(|| corrompu("assistant"))
}

fn decoder_assistant(charge: &[u8]) -> Option<MessageAssistant> {
    let mut e = Entree::new(charge);
    match e.u8()? {
        1 => Some(MessageAssistant::Pret),
        2 => Some(MessageAssistant::Trame(Box::new(lire_trame(&mut e)?))),
        3 => Some(MessageAssistant::Evenement(match e.u8()? {
            1 => CaptureEvent::ResolutionChanged,
            2 => CaptureEvent::SecureDesktop,
            _ => return None,
        })),
        4 => {
            let n = e.u32()? as usize;
            let mut liste = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                liste.push(lire_moniteur(&mut e)?);
            }
            Some(MessageAssistant::Moniteurs(liste))
        }
        5 => Some(MessageAssistant::Erreur(e.chaine()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une trame synthétique BGRA plein cadre, avec curseur et régions modifiées.
    fn trame_test() -> CapturedFrame {
        CapturedFrame {
            width: 4,
            height: 2,
            monitor: MonitorId(1),
            format: PixelFormat::Bgra8,
            dirty: vec![
                Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2,
                },
                Rect {
                    x: 2,
                    y: 0,
                    w: 2,
                    h: 1,
                },
            ],
            cursor: Some(CursorState {
                x: 3,
                y: 1,
                visible: true,
            }),
            timestamp_us: 987_654_321,
            image: Some(FrameImage::Cpu {
                data: (0..(4 * 2 * 4) as u8).collect(),
                stride: 4 * 4,
            }),
        }
    }

    #[test]
    fn roundtrip_messages_service() {
        let messages = [
            MessageService::Configurer {
                monitor: 2,
                fps: 60,
                curseur: true,
            },
            MessageService::Entree(EvenementEntree::SourisAbsolue {
                x: 0.25,
                y: 0.75,
                monitor: 1,
            }),
            MessageService::Entree(EvenementEntree::Bouton {
                bouton: MouseButton::Right,
                enfonce: true,
            }),
            MessageService::Entree(EvenementEntree::Molette { dx: -1.0, dy: 3.5 }),
            MessageService::Entree(EvenementEntree::Touche {
                scancode: 0x1E,
                enfonce: false,
            }),
            MessageService::Entree(EvenementEntree::Unicode { caractere: 'é' }),
            MessageService::Entree(EvenementEntree::ToutRelacher),
            MessageService::DefinirRegion(Some(Rect {
                x: 10,
                y: 20,
                w: 640,
                h: 480,
            })),
            MessageService::DefinirRegion(None),
            MessageService::BasculerMoniteur(3),
            MessageService::Arret,
        ];
        // Écrit tous les messages en flux, puis les relit dans l'ordre.
        let mut tampon: Vec<u8> = Vec::new();
        for m in &messages {
            ecrire_service(&mut tampon, m).expect("écriture");
        }
        let mut curseur = std::io::Cursor::new(tampon);
        for attendu in &messages {
            let lu = lire_service(&mut curseur).expect("lecture");
            assert_eq!(&lu, attendu);
        }
        // Plus rien à lire ⇒ EOF propre.
        assert_eq!(
            lire_service(&mut curseur).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn roundtrip_messages_assistant() {
        let messages = vec![
            MessageAssistant::Pret,
            MessageAssistant::Trame(Box::new(trame_test())),
            MessageAssistant::Evenement(CaptureEvent::ResolutionChanged),
            MessageAssistant::Evenement(CaptureEvent::SecureDesktop),
            MessageAssistant::Moniteurs(vec![
                MonitorInfo {
                    id: MonitorId(0),
                    name: r"\\.\DISPLAY1".to_owned(),
                    width: 1920,
                    height: 1080,
                    x: 0,
                    y: 0,
                    is_primary: true,
                },
                MonitorInfo {
                    id: MonitorId(1),
                    name: r"\\.\DISPLAY2".to_owned(),
                    width: 2560,
                    height: 1440,
                    x: 1920,
                    y: -120,
                    is_primary: false,
                },
            ]),
            MessageAssistant::Erreur("capture momentanément indisponible".to_owned()),
        ];
        let mut tampon: Vec<u8> = Vec::new();
        for m in &messages {
            ecrire_assistant(&mut tampon, m).expect("écriture");
        }
        let mut curseur = std::io::Cursor::new(tampon);
        for attendu in &messages {
            let lu = lire_assistant(&mut curseur).expect("lecture");
            assert_eq!(&lu, attendu);
        }
        assert_eq!(
            lire_assistant(&mut curseur).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn trame_preserve_pixels_et_regions() {
        let trame = trame_test();
        let mut tampon: Vec<u8> = Vec::new();
        ecrire_assistant(
            &mut tampon,
            &MessageAssistant::Trame(Box::new(trame.clone())),
        )
        .expect("écriture");
        let mut curseur = std::io::Cursor::new(tampon);
        let MessageAssistant::Trame(lu) = lire_assistant(&mut curseur).expect("lecture") else {
            panic!("trame attendue");
        };
        assert_eq!(lu.width, trame.width);
        assert_eq!(lu.height, trame.height);
        assert_eq!(lu.monitor, trame.monitor);
        assert_eq!(lu.dirty, trame.dirty);
        assert_eq!(lu.timestamp_us, trame.timestamp_us);
        let (
            Some(FrameImage::Cpu {
                data: a,
                stride: sa,
            }),
            Some(FrameImage::Cpu {
                data: b,
                stride: sb,
            }),
        ) = (&lu.image, &trame.image)
        else {
            panic!("images CPU attendues");
        };
        assert_eq!(a, b, "pixels identiques après aller-retour");
        assert_eq!(sa, sb, "stride préservé");
    }

    #[test]
    fn trames_corrompues_ne_paniquent_pas() {
        // En-tête annonçant une longueur énorme : rejeté sans allouer.
        let mut enorme = (u32::MAX).to_be_bytes().to_vec();
        enorme.push(0);
        assert_eq!(
            lire_service(&mut std::io::Cursor::new(enorme))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        // Charge présente mais étiquette inconnue ⇒ InvalidData.
        let mut tampon = Vec::new();
        ecrire_cadre(&mut tampon, &[99]).expect("cadre");
        assert_eq!(
            lire_service(&mut std::io::Cursor::new(tampon))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        // Charge tronquée (étiquette Configurer sans ses champs) ⇒ InvalidData.
        let mut tampon = Vec::new();
        ecrire_cadre(&mut tampon, &[1, 0, 0]).expect("cadre");
        assert_eq!(
            lire_service(&mut std::io::Cursor::new(tampon))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
