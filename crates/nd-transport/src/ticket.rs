//! Ticket de relais **signé Ed25519** : format et vérification, côté transport.
//!
//! Le repli relais ([`crate::connect_via_relay`] / [`crate::accept_via_relay`])
//! transporte un ticket **opaque** que `nd-relay` de production n'accepte que
//! **signé** par l'autorité du déploiement. Ce module fournit, côté client, le
//! **format** et la **vérification** de ce ticket, pour que le pair rejette
//! localement (avant tout réseau) un ticket altéré ou expiré, et pour que les
//! deux pairs présentent au relais des octets identiques.
//!
//! # Source de vérité du format
//!
//! La **source de vérité** du format est `server/nd-api` (`auth.rs`,
//! `TicketRelais`, lot 07) : c'est l'autorité `nd-api` (clé privée) qui émet les
//! tickets en production et `nd-relay` (clé publique) qui les vérifie. On ne
//! dépend **pas** de `nd-api` ici (crate serveur, lourd et à l'envers du graphe
//! côté client) : on **duplique** la structure de vérification, alignée
//! octet-pour-octet. Toute évolution du format doit rester synchrone entre les
//! deux (voir le test `interop_format_nd_api`).
//!
//! # Format binaire (89 octets, gros-boutiste)
//!
//! ```text
//!   [version u8 = 1][id_a u64][id_b u64][expire_le u64][signature 64]
//! ```
//!
//! La **signature** couvre `contexte-de-domaine || version || id_a || id_b ||
//! expire_le` ; le contexte (`b"novadesk-ticket-relais-v1"`) isole cet usage des
//! autres artefacts signés de l'autorité (jetons d'enregistrement, jetons
//! applicatifs).
//!
//! # Portée et rejeu
//!
//! La portée est la **paire d'IDs** (`id_a`, `id_b`) et l'**expiration**
//! `expire_le`. Le relais reste un tuyau aveugle : il apparie les pairs sur les
//! octets exacts du ticket sans les associer aux IDs — la portée engage
//! l'émetteur (un ticket ne sert qu'à une paire donnée) et l'expiration borne la
//! fenêtre de rejeu.

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Longueur (octets) d'une clé publique Ed25519.
pub const LG_CLE_PUBLIQUE: usize = 32;
/// Longueur (octets) d'une signature Ed25519.
pub const LG_SIGNATURE: usize = 64;

/// Contexte de domaine des tickets de relais (identique à `nd-api`).
const CONTEXTE_TICKET_RELAIS: &[u8] = b"novadesk-ticket-relais-v1";
/// Version courante du format binaire des tickets de relais.
const VERSION_TICKET: u8 = 1;

/// Taille du ticket de relais sérialisé : version + ids + expiration + signature.
pub const LG_TICKET_RELAIS: usize = 1 + 8 + 8 + 8 + LG_SIGNATURE;

/// Secondes UNIX courantes (0 si l'horloge précède l'époque — improbable).
///
/// Utilitaire pour renseigner l'argument `maintenant` de [`TicketRelais::verifier`]
/// et des points d'entrée relais signés.
#[must_use]
pub fn maintenant_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reconstruit une clé publique de vérification depuis ses 32 octets.
///
/// `None` si les octets ne forment pas un point Ed25519 valide. Pratique pour
/// charger la clé de l'autorité depuis la configuration du déploiement.
#[must_use]
pub fn cle_publique_depuis_octets(octets: &[u8; LG_CLE_PUBLIQUE]) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(octets).ok()
}

/// Ticket signé autorisant une **paire d'IDs** à emprunter le relais jusqu'à une
/// date d'expiration.
///
/// Aligné octet-pour-octet sur `nd-api::auth::TicketRelais` (source de vérité du
/// format). La signature est privée : un ticket ne se construit que par
/// [`TicketRelais::signer`] (émetteur) ou [`TicketRelais::from_bytes`]
/// (désérialisation), et ne se valide que par [`TicketRelais::verifier`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRelais {
    /// Premier ID de la paire autorisée.
    pub id_a: u64,
    /// Second ID de la paire autorisée.
    pub id_b: u64,
    /// Expiration (secondes UNIX) : le ticket est refusé à partir de cet instant.
    pub expire_le: u64,
    /// Signature de l'autorité sur (contexte, version, ids, expiration).
    signature: [u8; LG_SIGNATURE],
}

/// Message canonique signé par l'autorité pour un ticket de relais.
///
/// Identique à `nd-api` : `contexte || version || id_a || id_b || expire_le`.
fn message_ticket(id_a: u64, id_b: u64, expire_le: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(CONTEXTE_TICKET_RELAIS.len() + 1 + 24);
    message.extend_from_slice(CONTEXTE_TICKET_RELAIS);
    message.push(VERSION_TICKET);
    message.extend_from_slice(&id_a.to_be_bytes());
    message.extend_from_slice(&id_b.to_be_bytes());
    message.extend_from_slice(&expire_le.to_be_bytes());
    message
}

/// Motif de rejet d'un ticket de relais (mêmes cas que `nd-api`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErreurTicket {
    /// Taille ou version inattendue.
    Malforme,
    /// Signature absente de l'autorité attendue (ou ticket altéré).
    SignatureInvalide,
    /// Date d'expiration atteinte ou dépassée.
    Expire,
}

impl fmt::Display for ErreurTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurTicket::Malforme => write!(f, "ticket de relais mal formé"),
            ErreurTicket::SignatureInvalide => write!(f, "signature de ticket de relais invalide"),
            ErreurTicket::Expire => write!(f, "ticket de relais expiré"),
        }
    }
}

impl std::error::Error for ErreurTicket {}

impl TicketRelais {
    /// Émet un ticket signé pour la paire (`id_a`, `id_b`) expirant à `expire_le`
    /// (secondes UNIX).
    ///
    /// En production, l'émetteur est l'autorité de `nd-api` (courtier de session,
    /// lot 07). Cette fonction rend le module **autoportant** — émetteur hors
    /// `nd-api`, tests, outils — sans changer le format : Ed25519 étant
    /// déterministe, les deux pairs qui signent la même portée obtiennent des
    /// octets identiques (le relais les apparie sur ces octets).
    #[must_use]
    pub fn signer(cle: &SigningKey, id_a: u64, id_b: u64, expire_le: u64) -> Self {
        let signature = cle.sign(&message_ticket(id_a, id_b, expire_le)).to_bytes();
        Self {
            id_a,
            id_b,
            expire_le,
            signature,
        }
    }

    /// Sérialise le ticket (format fixe de [`LG_TICKET_RELAIS`] octets :
    /// version, `id_a` u64 BE, `id_b` u64 BE, `expire_le` u64 BE, signature).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LG_TICKET_RELAIS);
        out.push(VERSION_TICKET);
        out.extend_from_slice(&self.id_a.to_be_bytes());
        out.extend_from_slice(&self.id_b.to_be_bytes());
        out.extend_from_slice(&self.expire_le.to_be_bytes());
        out.extend_from_slice(&self.signature);
        out
    }

    /// Désérialise un ticket. `None` si la taille ou la version diffèrent.
    #[must_use]
    pub fn from_bytes(donnees: &[u8]) -> Option<Self> {
        if donnees.len() != LG_TICKET_RELAIS || donnees[0] != VERSION_TICKET {
            return None;
        }
        Some(Self {
            id_a: u64::from_be_bytes(donnees[1..9].try_into().ok()?),
            id_b: u64::from_be_bytes(donnees[9..17].try_into().ok()?),
            expire_le: u64::from_be_bytes(donnees[17..25].try_into().ok()?),
            signature: donnees[25..].try_into().ok()?,
        })
    }

    /// Vérifie un ticket sérialisé : **format**, **signature** de l'autorité,
    /// puis **expiration** (`maintenant` en secondes UNIX, comparaison
    /// inclusive). Renvoie le ticket décodé (sa portée) en cas de succès.
    ///
    /// C'est l'unique porte d'entrée de validation, alignée sur
    /// `nd-api::auth::TicketRelais::verifier`.
    ///
    /// # Errors
    /// [`ErreurTicket`] selon le premier contrôle en échec (`Malforme`,
    /// `SignatureInvalide`, puis `Expire`).
    pub fn verifier(
        donnees: &[u8],
        autorite: &VerifyingKey,
        maintenant: u64,
    ) -> Result<Self, ErreurTicket> {
        let ticket = Self::from_bytes(donnees).ok_or(ErreurTicket::Malforme)?;
        autorite
            .verify_strict(
                &message_ticket(ticket.id_a, ticket.id_b, ticket.expire_le),
                &Signature::from_bytes(&ticket.signature),
            )
            .map_err(|_| ErreurTicket::SignatureInvalide)?;
        if ticket.expire_le <= maintenant {
            return Err(ErreurTicket::Expire);
        }
        Ok(ticket)
    }

    /// Portée du ticket : la paire d'IDs autorisée.
    #[must_use]
    pub fn portee(&self) -> (u64, u64) {
        (self.id_a, self.id_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn autorite() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn autre_autorite() -> SigningKey {
        SigningKey::from_bytes(&[8u8; 32])
    }

    #[test]
    fn ticket_valide_altere_expire() {
        let cle = autorite();
        let publique = cle.verifying_key();
        let ticket = TicketRelais::signer(&cle, 111, 222, 1_000);
        let octets = ticket.to_bytes();
        assert_eq!(octets.len(), LG_TICKET_RELAIS);

        // Valide avant l'expiration, décodé avec sa portée intacte.
        let relu = TicketRelais::verifier(&octets, &publique, 999).expect("ticket valide");
        assert_eq!((relu.id_a, relu.id_b, relu.expire_le), (111, 222, 1_000));
        assert_eq!(relu.portee(), (111, 222));

        // Expiré à l'instant exact (inclusif) et au-delà.
        assert_eq!(
            TicketRelais::verifier(&octets, &publique, 1_000),
            Err(ErreurTicket::Expire)
        );
        assert_eq!(
            TicketRelais::verifier(&octets, &publique, 2_000),
            Err(ErreurTicket::Expire)
        );

        // Autre autorité : signature invérifiable.
        assert_eq!(
            TicketRelais::verifier(&octets, &autre_autorite().verifying_key(), 999),
            Err(ErreurTicket::SignatureInvalide)
        );

        // Altération de la portée : la signature ne correspond plus.
        let mut altere = octets.clone();
        altere[8] ^= 1;
        assert_eq!(
            TicketRelais::verifier(&altere, &publique, 999),
            Err(ErreurTicket::SignatureInvalide)
        );

        // Mal formé : tronqué, ou version inconnue.
        assert_eq!(
            TicketRelais::verifier(&octets[..10], &publique, 999),
            Err(ErreurTicket::Malforme)
        );
        let mut mauvaise_version = octets;
        mauvaise_version[0] = 9;
        assert_eq!(
            TicketRelais::verifier(&mauvaise_version, &publique, 999),
            Err(ErreurTicket::Malforme)
        );
    }

    #[test]
    fn aller_retour_bytes() {
        let cle = autorite();
        let ticket = TicketRelais::signer(&cle, 42, 99, 12_345);
        let relu = TicketRelais::from_bytes(&ticket.to_bytes()).expect("décodage");
        assert_eq!(relu, ticket);
        assert!(TicketRelais::from_bytes(&[0u8; 10]).is_none());
    }

    /// Garde-fou anti-divergence : le format binaire et le message signé doivent
    /// rester identiques à `nd-api::auth` (mêmes constantes, même disposition).
    #[test]
    fn interop_format_nd_api() {
        assert_eq!(LG_TICKET_RELAIS, 89);
        assert_eq!(CONTEXTE_TICKET_RELAIS, b"novadesk-ticket-relais-v1");
        assert_eq!(VERSION_TICKET, 1);

        // Disposition octet-pour-octet de la sérialisation.
        let cle = autorite();
        let octets =
            TicketRelais::signer(&cle, 0x0102_0304_0506_0708, 0x1122_3344_5566_7788, 7).to_bytes();
        assert_eq!(octets[0], VERSION_TICKET);
        assert_eq!(&octets[1..9], &0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(&octets[9..17], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(&octets[17..25], &7u64.to_be_bytes());
        assert_eq!(octets[25..].len(), LG_SIGNATURE);

        // Message signé : contexte || version || id_a || id_b || expire_le.
        let attendu = {
            let mut m = Vec::new();
            m.extend_from_slice(b"novadesk-ticket-relais-v1");
            m.push(1);
            m.extend_from_slice(&1u64.to_be_bytes());
            m.extend_from_slice(&2u64.to_be_bytes());
            m.extend_from_slice(&3u64.to_be_bytes());
            m
        };
        assert_eq!(message_ticket(1, 2, 3), attendu);
    }
}
