//! `nd-proto` — types partagés du projet NovaDesk : identité, canaux logiques,
//! versionnement de protocole et erreur commune.
//!
//! Ce crate ne dépend de rien (std uniquement) : il est au sommet du graphe de
//! dépendances et importé par presque tous les autres. Voir
//! `../../plan-technique/01-architecture-globale.md` et `04-transport-reseau.md`.

use std::fmt;

/// Version du protocole applicatif, négociée entre pairs au handshake.
///
/// La compatibilité est portée par le numéro majeur (voir
/// `plan-technique/15-deploiement-mise-a-jour.md` §négociation de version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// Version courante du protocole implémentée par ce binaire.
    pub const CURRENT: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };

    /// Deux pairs sont compatibles s'ils partagent le même numéro majeur.
    #[must_use]
    pub fn is_compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Identifiant NovaDesk d'un appareil (analogue à l'ID AnyDesk : 9–10 chiffres).
///
/// La génération, l'unicité et la protection anti-énumération sont traitées côté
/// serveur de rendez-vous — voir `plan-technique/05-connectivite-nat.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NovaId(pub u64);

impl NovaId {
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for NovaId {
    /// Affiche l'ID sur 9 chiffres groupés par 3, ex. `123 456 789`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = format!("{:09}", self.0);
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                f.write_str(" ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

/// Identifiant d'un moniteur côté machine contrôlée.
///
/// Partagé par la capture (`nd-capture`), l'injection (`nd-input`) et le multi-écran
/// (`plan-technique/13-fonctionnalites-avancees.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorId(pub u32);

/// Canal logique multiplexé sur la connexion QUIC.
///
/// Voir `plan-technique/04-transport-reseau.md` §canaux logiques.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    /// Contrôle de session, chat, négociation (fiable).
    Control,
    /// Flux vidéo d'un moniteur donné (non fiable + FEC).
    Video(MonitorId),
    /// Flux audio (non fiable + FEC).
    Audio,
    /// Entrées clavier/souris (fiable, priorité maximale).
    Input,
    /// Transfert de fichiers (fiable, débit).
    Files,
}

impl ChannelKind {
    /// Priorité d'ordonnancement : plus grand = servi en premier.
    ///
    /// L'input passe avant tout pour minimiser la latence perçue ; la vidéo passe
    /// avant les fichiers pour préserver la fluidité (voir plan 04).
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            ChannelKind::Input => 255,
            ChannelKind::Control => 200,
            ChannelKind::Audio => 150,
            ChannelKind::Video(_) => 100,
            ChannelKind::Files => 50,
        }
    }

    /// Fiabilité par défaut associée au type de canal.
    #[must_use]
    pub fn default_reliability(self) -> Reliability {
        match self {
            ChannelKind::Video(_) | ChannelKind::Audio => Reliability::UnreliableFec,
            ChannelKind::Control | ChannelKind::Input | ChannelKind::Files => Reliability::Reliable,
        }
    }
}

/// Niveau de fiabilité demandé pour un envoi sur un canal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliability {
    /// Flux fiable et ordonné (flux QUIC).
    Reliable,
    /// Datagramme non fiable, protégé par FEC (média temps réel).
    UnreliableFec,
}

/// Événement d'entrée transporté du contrôleur vers la machine contrôlée (canal
/// `Input`). Voir `plan-technique/07-injection-entrees.md`.
///
/// Sérialisation binaire compacte maison (grand-boutiste) — `nd-proto` reste sans
/// dépendance externe. Le mapping vers l'injection OS est dans `nd-core::apply_input`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Déplacement absolu, coordonnées normalisées 0.0–1.0 sur le moniteur.
    MouseMoveAbs { x: f64, y: f64, monitor: u32 },
    /// Déplacement relatif en pixels.
    MouseMoveRel { dx: f64, dy: f64 },
    /// Bouton souris (0=gauche, 1=droit, 2=milieu, 3=X1, 4=X2).
    MouseButton { button: u8, down: bool },
    /// Molette (crans ; positif = haut/droite).
    Scroll { dx: f64, dy: f64 },
    /// Touche par scancode physique.
    Key { scancode: u32, down: bool },
    /// Caractère Unicode (point de code).
    Unicode { codepoint: u32 },
}

impl InputEvent {
    /// Sérialise l'événement en binaire.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        match *self {
            InputEvent::MouseMoveAbs { x, y, monitor } => {
                out.push(0);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
                out.extend_from_slice(&monitor.to_be_bytes());
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                out.push(1);
                out.extend_from_slice(&dx.to_be_bytes());
                out.extend_from_slice(&dy.to_be_bytes());
            }
            InputEvent::MouseButton { button, down } => {
                out.push(2);
                out.push(button);
                out.push(u8::from(down));
            }
            InputEvent::Scroll { dx, dy } => {
                out.push(3);
                out.extend_from_slice(&dx.to_be_bytes());
                out.extend_from_slice(&dy.to_be_bytes());
            }
            InputEvent::Key { scancode, down } => {
                out.push(4);
                out.extend_from_slice(&scancode.to_be_bytes());
                out.push(u8::from(down));
            }
            InputEvent::Unicode { codepoint } => {
                out.push(5);
                out.extend_from_slice(&codepoint.to_be_bytes());
            }
        }
        out
    }

    /// Désérialise un événement depuis le format de [`InputEvent::to_bytes`].
    /// Renvoie `None` si les octets sont invalides ou incomplets.
    #[must_use]
    pub fn from_bytes(data: &[u8]) -> Option<InputEvent> {
        let (&tag, rest) = data.split_first()?;
        let f64_at = |o: usize| -> Option<f64> {
            Some(f64::from_be_bytes(rest.get(o..o + 8)?.try_into().ok()?))
        };
        let u32_at = |o: usize| -> Option<u32> {
            Some(u32::from_be_bytes(rest.get(o..o + 4)?.try_into().ok()?))
        };
        match tag {
            0 => Some(InputEvent::MouseMoveAbs {
                x: f64_at(0)?,
                y: f64_at(8)?,
                monitor: u32_at(16)?,
            }),
            1 => Some(InputEvent::MouseMoveRel {
                dx: f64_at(0)?,
                dy: f64_at(8)?,
            }),
            2 => Some(InputEvent::MouseButton {
                button: *rest.first()?,
                down: *rest.get(1)? != 0,
            }),
            3 => Some(InputEvent::Scroll {
                dx: f64_at(0)?,
                dy: f64_at(8)?,
            }),
            4 => Some(InputEvent::Key {
                scancode: u32_at(0)?,
                down: *rest.get(4)? != 0,
            }),
            5 => Some(InputEvent::Unicode {
                codepoint: u32_at(0)?,
            }),
            _ => None,
        }
    }
}

/// Erreur commune du projet. Chaque couche l'enrichit via ses variantes.
#[derive(Debug)]
pub enum NdError {
    /// Violation ou incompatibilité de protocole.
    Protocol(String),
    /// Erreur de la couche transport (voir `nd-transport`).
    Transport(String),
    /// Erreur cryptographique / de session sécurisée (voir `nd-crypto`).
    Crypto(String),
    /// Erreur de capture d'écran (voir `nd-capture`).
    Capture(String),
    /// Erreur d'encodage/décodage (voir `nd-codec`).
    Codec(String),
    /// Erreur d'injection d'entrées (voir `nd-input`).
    Input(String),
    /// Erreur d'entrée/sortie sous-jacente.
    Io(std::io::Error),
    /// Fonctionnalité pas encore implémentée à ce stade du projet.
    NotImplemented(&'static str),
}

impl fmt::Display for NdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NdError::Protocol(m) => write!(f, "protocole : {m}"),
            NdError::Transport(m) => write!(f, "transport : {m}"),
            NdError::Crypto(m) => write!(f, "crypto : {m}"),
            NdError::Capture(m) => write!(f, "capture : {m}"),
            NdError::Codec(m) => write!(f, "codec : {m}"),
            NdError::Input(m) => write!(f, "input : {m}"),
            NdError::Io(e) => write!(f, "io : {e}"),
            NdError::NotImplemented(what) => write!(f, "non implémenté : {what}"),
        }
    }
}

impl std::error::Error for NdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NdError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for NdError {
    fn from(e: std::io::Error) -> Self {
        NdError::Io(e)
    }
}

/// Alias de résultat commun à tout le projet.
pub type Result<T> = std::result::Result<T, NdError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_est_le_canal_le_plus_prioritaire() {
        assert!(ChannelKind::Input.priority() > ChannelKind::Control.priority());
        assert!(ChannelKind::Control.priority() > ChannelKind::Audio.priority());
        assert!(ChannelKind::Audio.priority() > ChannelKind::Video(MonitorId(0)).priority());
        assert!(ChannelKind::Video(MonitorId(0)).priority() > ChannelKind::Files.priority());
    }

    #[test]
    fn video_et_audio_sont_non_fiables() {
        assert_eq!(
            ChannelKind::Audio.default_reliability(),
            Reliability::UnreliableFec
        );
        assert_eq!(
            ChannelKind::Video(MonitorId(1)).default_reliability(),
            Reliability::UnreliableFec
        );
        assert_eq!(
            ChannelKind::Input.default_reliability(),
            Reliability::Reliable
        );
    }

    #[test]
    fn compatibilite_par_numero_majeur() {
        let a = ProtocolVersion { major: 1, minor: 3 };
        let b = ProtocolVersion { major: 1, minor: 7 };
        let c = ProtocolVersion { major: 2, minor: 0 };
        assert!(a.is_compatible_with(b));
        assert!(!a.is_compatible_with(c));
    }

    #[test]
    fn affichage_id_groupe_par_trois() {
        assert_eq!(NovaId(123_456_789).to_string(), "123 456 789");
        assert_eq!(NovaId(1_000).to_string(), "000 001 000");
    }

    #[test]
    fn input_event_roundtrip() {
        let events = [
            InputEvent::MouseMoveAbs {
                x: 0.25,
                y: 0.75,
                monitor: 1,
            },
            InputEvent::MouseMoveRel { dx: -3.5, dy: 12.0 },
            InputEvent::MouseButton {
                button: 2,
                down: true,
            },
            InputEvent::Scroll { dx: 0.0, dy: -1.0 },
            InputEvent::Key {
                scancode: 0x1E,
                down: false,
            },
            InputEvent::Unicode { codepoint: 0x41 },
        ];
        for ev in events {
            assert_eq!(InputEvent::from_bytes(&ev.to_bytes()), Some(ev));
        }
        assert_eq!(InputEvent::from_bytes(&[]), None);
        assert_eq!(InputEvent::from_bytes(&[99]), None);
    }
}
