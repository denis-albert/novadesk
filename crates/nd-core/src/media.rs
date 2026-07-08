//! Câblage des briques « prêtes pour la session » (lot 05) dans la vraie boucle
//! média de [`crate::SessionEngine`] : audio système, transfert de fichiers,
//! presse-papiers, chat et bascule multi-écran — chacune gardée par une
//! [`Capability`](nd_features::Capability).
//!
//! # Canaux logiques et **ordre des nonces Noise**
//!
//! Le transport chiffré de session ([`crate::EncryptedTransport`]) porte **une
//! seule** session Noise par direction : les charges y sont chiffrées avec un
//! compteur de nonce qui **doit** être déchiffré dans le même ordre à l'autre
//! bout (snow `TransportState` ne tolère ni perte ni réordonnancement). Or le
//! transport QUIC fusionne deux files à la réception (flux fiable **vs**
//! datagrammes) : mélanger, **dans une même direction**, des envois fiables et
//! des datagrammes désynchronise le nonce (vérifié empiriquement — le premier
//! message arrivé dans le désordre casse le déchiffrement).
//!
//! Conséquence directe sur l'architecture : **chaque direction n'emploie qu'un
//! seul domaine d'ordonnancement**.
//!
//! * `contrôleur → hôte` : déjà **fiable** (canal `Input`). On y ajoute
//!   Fichiers, Presse-papiers, Chat et Bascule-moniteur, tous **fiables**,
//!   multiplexés (canal `Files` pour le transfert ; canal `Control` avec un
//!   petit en-tête de sous-type pour presse-papiers/chat/moniteur).
//! * `hôte → contrôleur` : la vidéo occupe historiquement le domaine
//!   **datagrammes**. Comme le plan de contrôle (réponses chat, presse-papiers,
//!   trames de contrôle du transfert) doit remonter **de façon fiable**, la
//!   direction `hôte → contrôleur` passe **intégralement en fiable** dès que les
//!   fonctions étendues sont actives : vidéo, audio et plan de contrôle sur le
//!   flux fiable ordonné. Le nonce reste synchronisé (un seul domaine). Les
//!   sessions **sans** fonction étendue gardent la vidéo en datagrammes+FEC
//!   (comportement historique inchangé).
//!
//! Le choix « audio non fiable » suggéré par le lot est donc arbitré en
//! **fiable** ici : c'est le prix de l'intégrité du nonce avec une session Noise
//! unique. En boucle locale (sans perte) le comportement est identique ; la voie
//! « média non fiable + plan de contrôle chiffré séparément » (deux sessions
//! Noise démultiplexées) est notée comme évolution production.

use std::path::PathBuf;

use nd_audio::AudioPacket;
use nd_proto::{ChannelKind, MonitorId};

/// Nombre de moniteurs distincts pré-cartographiés pour la réception vidéo
/// (couvre la bascule multi-écran sans reconstruire la carte des canaux).
pub(crate) const MONITEURS_MAX: u32 = 8;

/// Message de chat livré au consommateur via [`crate::SessionHandle::chat_rx`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// `true` si le message vient du **pair distant**, `false` s'il s'agit de
    /// l'écho local d'un message que ce poste vient d'émettre.
    pub from_remote: bool,
    /// Texte du message (UTF-8).
    pub text: String,
}

/// Catégorie d'un message reçu, déduite du canal logique — pilote le
/// démultiplexage du récepteur unique de chaque côté.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Categorie {
    /// Flux vidéo d'un moniteur (décodage → frames).
    Video(MonitorId),
    /// Flux audio (dépaquetage → lecture).
    Audio,
    /// Transfert de fichiers (trames [`nd_files::TransferSession`]).
    Fichiers,
    /// Entrées clavier/souris (injection côté hôte).
    Input,
    /// Plan de contrôle annexe (presse-papiers, chat, bascule moniteur).
    Controle,
}

impl Categorie {
    /// Catégorie d'un [`ChannelKind`].
    pub(crate) fn depuis_kind(kind: ChannelKind) -> Categorie {
        match kind {
            ChannelKind::Video(m) => Categorie::Video(m),
            ChannelKind::Audio => Categorie::Audio,
            ChannelKind::Files => Categorie::Fichiers,
            ChannelKind::Input => Categorie::Input,
            ChannelKind::Control => Categorie::Controle,
        }
    }
}

/// Sous-type d'un message multiplexé sur le canal `Control` **après** le
/// handshake (le handshake Noise, lui, a déjà libéré le canal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SousTypeControle {
    /// Message de chat (payload = texte UTF-8).
    Chat = 1,
    /// Synchro presse-papiers (payload = octets [`nd_files::ClipboardSync`]).
    PressePapiers = 2,
    /// Demande de bascule moniteur (payload = index u32 big-endian).
    BasculeMoniteur = 3,
}

impl SousTypeControle {
    /// Reconstruit le sous-type depuis l'octet de tête.
    pub(crate) fn depuis_octet(octet: u8) -> Option<SousTypeControle> {
        match octet {
            1 => Some(SousTypeControle::Chat),
            2 => Some(SousTypeControle::PressePapiers),
            3 => Some(SousTypeControle::BasculeMoniteur),
            _ => None,
        }
    }
}

/// Encadre un message de plan de contrôle : `[sous-type u8] ++ payload`.
pub(crate) fn encoder_controle(sous_type: SousTypeControle, payload: &[u8]) -> Vec<u8> {
    let mut trame = Vec::with_capacity(1 + payload.len());
    trame.push(sous_type as u8);
    trame.extend_from_slice(payload);
    trame
}

/// Décode un message de plan de contrôle en `(sous-type, payload)`.
pub(crate) fn decoder_controle(trame: &[u8]) -> Option<(SousTypeControle, &[u8])> {
    let (&tete, reste) = trame.split_first()?;
    Some((SousTypeControle::depuis_octet(tete)?, reste))
}

/// Encadre un paquet audio pour le canal `Audio` : `[timestamp_us u64 BE] ++ data`.
pub(crate) fn encoder_audio(paquet: &AudioPacket) -> Vec<u8> {
    let mut trame = Vec::with_capacity(8 + paquet.data.len());
    trame.extend_from_slice(&paquet.timestamp_us.to_be_bytes());
    trame.extend_from_slice(&paquet.data);
    trame
}

/// Décode un paquet audio reçu sur le canal `Audio`.
pub(crate) fn decoder_audio(trame: &[u8]) -> Option<AudioPacket> {
    let entete = trame.get(0..8)?;
    let timestamp_us = u64::from_be_bytes(entete.try_into().ok()?);
    Some(AudioPacket {
        timestamp_us,
        data: trame.get(8..)?.to_vec(),
    })
}

/// Commande adressée aux threads média d'une session vivante via
/// [`crate::SessionHandle`] (fichiers à envoyer, bascule moniteur, audio).
#[derive(Debug, Clone)]
pub(crate) enum CommandeMedia {
    /// Démarrer l'envoi d'une file de fichiers vers le pair.
    EnvoyerFichiers(Vec<PathBuf>),
    /// Demander au pair (hôte) de diffuser le moniteur d'index donné.
    BasculerMoniteur(u32),
    /// Activer/désactiver l'émission (hôte) ou la lecture (contrôleur) audio.
    AudioActif(bool),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_controle() {
        for st in [
            SousTypeControle::Chat,
            SousTypeControle::PressePapiers,
            SousTypeControle::BasculeMoniteur,
        ] {
            let trame = encoder_controle(st, b"charge utile");
            let (dec, payload) = decoder_controle(&trame).expect("décodage");
            assert_eq!(dec, st);
            assert_eq!(payload, b"charge utile");
        }
        assert!(decoder_controle(&[]).is_none());
        assert!(decoder_controle(&[99]).is_none());
    }

    #[test]
    fn roundtrip_audio() {
        let p = AudioPacket {
            data: vec![1, 2, 3, 4, 5],
            timestamp_us: 123_456_789,
        };
        let trame = encoder_audio(&p);
        let dec = decoder_audio(&trame).expect("décodage");
        assert_eq!(dec.timestamp_us, p.timestamp_us);
        assert_eq!(dec.data, p.data);
        assert!(decoder_audio(&[0, 1, 2]).is_none());
    }

    #[test]
    fn categorie_depuis_kind() {
        assert_eq!(Categorie::depuis_kind(ChannelKind::Audio), Categorie::Audio);
        assert_eq!(
            Categorie::depuis_kind(ChannelKind::Video(MonitorId(3))),
            Categorie::Video(MonitorId(3))
        );
        assert_eq!(
            Categorie::depuis_kind(ChannelKind::Control),
            Categorie::Controle
        );
    }
}
