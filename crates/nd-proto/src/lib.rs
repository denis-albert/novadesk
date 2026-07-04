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
}
