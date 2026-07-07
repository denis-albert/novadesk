//! Autorité de signature NovaDesk — jetons et tickets signés (Ed25519).
//!
//! Ce module est la **source de vérité des formats signés** échangés entre les
//! serveurs NovaDesk (plan 11 — backend). Trois artefacts, tous signés par la
//! même **autorité de déploiement** (une paire Ed25519 par déploiement) :
//!
//! - [`JetonEnregistrement`] : émis par le service d'attribution d'ID
//!   ([`crate::allocation`]) à l'allocation d'un `NovaId`. Il **lie l'ID à la
//!   clé statique du client** ; le serveur de rendez-vous (`nd-rendezvous`)
//!   l'exige au `Register`, accompagné d'une signature fraîche du client
//!   (preuve de possession de la clé) — anti-squatting d'ID.
//! - [`TicketRelais`] : autorise une **paire d'IDs** à emprunter le relais
//!   (`nd-relay`) jusqu'à une date d'expiration. Le relais n'accepte plus
//!   aucun ticket non signé ou expiré.
//! - **Jeton applicatif** ([`Autorite::emettre_jeton_applicatif`] /
//!   [`verifier_jeton_applicatif`]) : porte le **compte agissant** des requêtes
//!   `nd-api` ; le RBAC s'applique à ce compte, jamais à un champ de requête.
//!
//! ## Répartition des clés
//! - clé **privée** : `nd-api` (attribution d'ID, émission des jetons pour ce
//!   jet) et, à terme, `nd-accounts` (lot 09) qui émettra les jetons
//!   applicatifs à la connexion ;
//! - clé **publique** seulement : `nd-rendezvous` et `nd-relay` (vérification).
//!
//! ## Point de jonction avec `nd-accounts` (lot 09)
//! `nd-api` vérifie les jetons applicatifs **localement** avec une simple clé
//! publique ([`crate::services::Services::en_verification_seule`]) : c'est le
//! contrat de jonction. Le service de comptes (`nd-accounts`, binaire
//! indépendant) émet aujourd'hui des jetons **JWS compact EdDSA**
//! (`en-tête.charge.signature` en base64url, claims `iss`/`sub`/`exp`…) et
//! publie sa clé publique par la requête `ClePubliqueJetons` — le câblage
//! (accepter ce format-là dans [`verifier_jeton_applicatif`], récupérer la
//! clé au démarrage) est l'étape d'intégration convenue, hors de ce lot. En
//! attendant, le format local `nda1.<hex(charge)>.<hex(signature)>` ci-dessous
//! rend l'autorisation réelle et testable de bout en bout.
//!
//! Chaque signature couvre un **contexte de domaine** distinct : une signature
//! émise pour un usage (ex. ticket de relais) est invérifiable dans un autre
//! (ex. jeton applicatif), même à charge utile identique.

use std::fmt;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Longueur (octets) d'une clé publique Ed25519.
pub const LG_CLE_PUBLIQUE: usize = 32;
/// Longueur (octets) d'une signature Ed25519.
pub const LG_SIGNATURE: usize = 64;

/// Contexte de domaine des jetons d'enregistrement d'ID.
const CONTEXTE_ENREGISTREMENT: &[u8] = b"novadesk-jeton-enregistrement-v1";
/// Contexte de domaine des tickets de relais.
const CONTEXTE_TICKET_RELAIS: &[u8] = b"novadesk-ticket-relais-v1";
/// Contexte de domaine des jetons applicatifs.
const CONTEXTE_JETON_APPLICATIF: &[u8] = b"novadesk-jeton-applicatif-v1";

/// Préfixe textuel des jetons applicatifs (version 1).
const PREFIXE_JETON: &str = "nda1";

/// Version courante du format binaire des tickets de relais.
const VERSION_TICKET: u8 = 1;

/// Secondes UNIX courantes (0 si l'horloge précède l'époque — improbable).
#[must_use]
pub fn maintenant_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Décode une clé publique Ed25519 depuis 64 caractères hexadécimaux.
/// `None` si la chaîne est mal formée ou si le point n'est pas valide.
#[must_use]
pub fn cle_publique_depuis_hex(texte: &str) -> Option<VerifyingKey> {
    let octets = hex::decode(texte.trim()).ok()?;
    let tableau: [u8; LG_CLE_PUBLIQUE] = octets.try_into().ok()?;
    VerifyingKey::from_bytes(&tableau).ok()
}

// ---------------------------------------------------------------------------
// Autorité (clé privée d'émission)
// ---------------------------------------------------------------------------

/// Autorité de signature d'un déploiement NovaDesk (paire Ed25519).
///
/// Émet les trois artefacts signés du module. Les serveurs qui ne font que
/// vérifier (`nd-rendezvous`, `nd-relay`) ne reçoivent que
/// [`Autorite::cle_publique`].
#[derive(Clone)]
pub struct Autorite {
    cle: SigningKey,
}

impl fmt::Debug for Autorite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Jamais la clé privée dans les journaux.
        write!(f, "Autorite(cle_publique={})", self.cle_publique_hex())
    }
}

impl Autorite {
    /// Génère une autorité éphémère (graine tirée du générateur du système).
    ///
    /// # Errors
    /// Propage l'échec du générateur aléatoire du système.
    pub fn generer() -> io::Result<Self> {
        let mut graine = [0u8; 32];
        getrandom::fill(&mut graine).map_err(io::Error::other)?;
        Ok(Self::depuis_graine(&graine))
    }

    /// Reconstruit l'autorité depuis une graine de 32 octets (déterministe).
    #[must_use]
    pub fn depuis_graine(graine: &[u8; 32]) -> Self {
        Self {
            cle: SigningKey::from_bytes(graine),
        }
    }

    /// Charge la graine hexadécimale depuis `chemin`, ou la crée au premier
    /// démarrage (écriture atomique : fichier temporaire puis renommage).
    ///
    /// Le fichier contient 64 caractères hexadécimaux (la graine privée) : il
    /// doit rester dans un répertoire protégé par les permissions du système.
    ///
    /// # Errors
    /// Propage les erreurs d'E/S ; `InvalidData` si le fichier est illisible.
    pub fn charger_ou_creer(chemin: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(chemin) {
            Ok(texte) => {
                let octets = hex::decode(texte.trim()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "graine d'autorité illisible")
                })?;
                let graine: [u8; 32] = octets.try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "graine d'autorité illisible")
                })?;
                Ok(Self::depuis_graine(&graine))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let autorite = Self::generer()?;
                if let Some(parent) = chemin.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let temporaire = chemin.with_extension("tmp");
                std::fs::write(&temporaire, hex::encode(autorite.cle.to_bytes()))?;
                std::fs::rename(&temporaire, chemin)?;
                Ok(autorite)
            }
            Err(e) => Err(e),
        }
    }

    /// Clé publique de vérification (à distribuer à `nd-rendezvous`/`nd-relay`).
    #[must_use]
    pub fn cle_publique(&self) -> VerifyingKey {
        self.cle.verifying_key()
    }

    /// Clé publique en hexadécimal (64 caractères), pour la configuration.
    #[must_use]
    pub fn cle_publique_hex(&self) -> String {
        hex::encode(self.cle_publique().to_bytes())
    }

    /// Émet un jeton d'enregistrement liant `id` à la clé statique `cle_client`.
    #[must_use]
    pub fn emettre_jeton_enregistrement(
        &self,
        id: u64,
        cle_client: &VerifyingKey,
    ) -> JetonEnregistrement {
        let cle_client = cle_client.to_bytes();
        let signature = self
            .cle
            .sign(&message_enregistrement(id, &cle_client))
            .to_bytes();
        JetonEnregistrement {
            id,
            cle_client,
            signature,
        }
    }

    /// Émet un ticket autorisant la paire (`id_a`, `id_b`) à emprunter le
    /// relais jusqu'à `expire_le` (secondes UNIX). Le **même** ticket doit être
    /// remis aux deux pairs : le relais les apparie sur ses octets exacts.
    #[must_use]
    pub fn emettre_ticket_relais(&self, id_a: u64, id_b: u64, expire_le: u64) -> TicketRelais {
        let signature = self
            .cle
            .sign(&message_ticket(id_a, id_b, expire_le))
            .to_bytes();
        TicketRelais {
            id_a,
            id_b,
            expire_le,
            signature,
        }
    }

    /// Émet un jeton applicatif pour `compte`, expirant à `expire_le`
    /// (secondes UNIX). Format : `nda1.<hex(charge)>.<hex(signature)>`.
    #[must_use]
    pub fn emettre_jeton_applicatif(&self, compte: &str, expire_le: u64) -> String {
        let charge = charge_jeton(compte, expire_le);
        let signature = self.cle.sign(&message_jeton(&charge)).to_bytes();
        format!(
            "{PREFIXE_JETON}.{}.{}",
            hex::encode(&charge),
            hex::encode(signature)
        )
    }
}

// ---------------------------------------------------------------------------
// Jeton d'enregistrement d'ID (rendez-vous)
// ---------------------------------------------------------------------------

/// Attestation signée par l'autorité : « l'ID `id` appartient au porteur de la
/// clé statique `cle_client` ».
///
/// Émise à l'allocation ([`crate::allocation::AllocateurId`]), présentée au
/// serveur de rendez-vous à chaque `Register`. Le porteur doit en plus prouver
/// la **possession** de `cle_client` par une signature fraîche (voir
/// `nd-rendezvous`) : le jeton seul, observé sur le réseau, ne suffit pas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JetonEnregistrement {
    /// ID NovaDesk attribué.
    pub id: u64,
    /// Clé publique statique (Ed25519) du client, liée à l'ID.
    pub cle_client: [u8; LG_CLE_PUBLIQUE],
    /// Signature de l'autorité sur (contexte, id, clé client).
    signature: [u8; LG_SIGNATURE],
}

/// Taille du jeton d'enregistrement sérialisé : id + clé + signature.
pub const LG_JETON_ENREGISTREMENT: usize = 8 + LG_CLE_PUBLIQUE + LG_SIGNATURE;

/// Message canonique signé par l'autorité pour un jeton d'enregistrement.
fn message_enregistrement(id: u64, cle_client: &[u8; LG_CLE_PUBLIQUE]) -> Vec<u8> {
    let mut message = Vec::with_capacity(CONTEXTE_ENREGISTREMENT.len() + 8 + LG_CLE_PUBLIQUE);
    message.extend_from_slice(CONTEXTE_ENREGISTREMENT);
    message.extend_from_slice(&id.to_be_bytes());
    message.extend_from_slice(cle_client);
    message
}

impl JetonEnregistrement {
    /// Sérialise le jeton (format fixe de [`LG_JETON_ENREGISTREMENT`] octets :
    /// `id` u64 BE, clé client, signature).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LG_JETON_ENREGISTREMENT);
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&self.cle_client);
        out.extend_from_slice(&self.signature);
        out
    }

    /// Désérialise un jeton. `None` si la taille ne correspond pas.
    #[must_use]
    pub fn from_bytes(donnees: &[u8]) -> Option<Self> {
        if donnees.len() != LG_JETON_ENREGISTREMENT {
            return None;
        }
        Some(Self {
            id: u64::from_be_bytes(donnees[..8].try_into().ok()?),
            cle_client: donnees[8..8 + LG_CLE_PUBLIQUE].try_into().ok()?,
            signature: donnees[8 + LG_CLE_PUBLIQUE..].try_into().ok()?,
        })
    }

    /// Vérifie la signature de l'autorité. `false` si le jeton n'a pas été
    /// émis par elle (ou a été altéré).
    #[must_use]
    pub fn verifier(&self, autorite: &VerifyingKey) -> bool {
        autorite
            .verify_strict(
                &message_enregistrement(self.id, &self.cle_client),
                &Signature::from_bytes(&self.signature),
            )
            .is_ok()
    }

    /// Clé publique du client liée à l'ID, si elle est un point Ed25519 valide.
    #[must_use]
    pub fn cle_client(&self) -> Option<VerifyingKey> {
        VerifyingKey::from_bytes(&self.cle_client).ok()
    }
}

// ---------------------------------------------------------------------------
// Ticket de relais
// ---------------------------------------------------------------------------

/// Ticket signé autorisant une paire d'IDs à emprunter le relais.
///
/// La **portée** est la paire (`id_a`, `id_b`) et l'**expiration** `expire_le` ;
/// le relais rejette tout ticket non signé par l'autorité ou expiré. Le relais
/// reste un tuyau aveugle : il n'associe pas les octets relayés aux IDs — la
/// portée engage l'émetteur (un ticket ne sert qu'à une paire donnée) et borne
/// la fenêtre de rejeu via l'expiration.
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

/// Taille du ticket de relais sérialisé : version + ids + expiration + signature.
pub const LG_TICKET_RELAIS: usize = 1 + 8 + 8 + 8 + LG_SIGNATURE;

/// Message canonique signé par l'autorité pour un ticket de relais.
fn message_ticket(id_a: u64, id_b: u64, expire_le: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(CONTEXTE_TICKET_RELAIS.len() + 1 + 24);
    message.extend_from_slice(CONTEXTE_TICKET_RELAIS);
    message.push(VERSION_TICKET);
    message.extend_from_slice(&id_a.to_be_bytes());
    message.extend_from_slice(&id_b.to_be_bytes());
    message.extend_from_slice(&expire_le.to_be_bytes());
    message
}

/// Motif de rejet d'un ticket de relais.
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
            ErreurTicket::Malforme => write!(f, "ticket mal formé"),
            ErreurTicket::SignatureInvalide => write!(f, "signature de ticket invalide"),
            ErreurTicket::Expire => write!(f, "ticket expiré"),
        }
    }
}

impl TicketRelais {
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

    /// Vérifie un ticket sérialisé : format, signature de l'autorité, puis
    /// expiration (`maintenant` en secondes UNIX). Renvoie le ticket décodé.
    ///
    /// # Errors
    /// [`ErreurTicket`] selon le premier contrôle en échec.
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
}

// ---------------------------------------------------------------------------
// Jeton applicatif (nd-api)
// ---------------------------------------------------------------------------

/// Motif de rejet d'un jeton applicatif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErreurJeton {
    /// Préfixe, hexadécimal ou charge utile illisibles.
    Malforme,
    /// Signature absente de l'autorité attendue (ou jeton altéré).
    SignatureInvalide,
    /// Date d'expiration atteinte ou dépassée.
    Expire,
}

/// Charge utile canonique d'un jeton applicatif :
/// `[version u8 = 1][longueur compte u32 BE][compte UTF-8][expire_le u64 BE]`.
fn charge_jeton(compte: &str, expire_le: u64) -> Vec<u8> {
    let mut charge = Vec::with_capacity(1 + 4 + compte.len() + 8);
    charge.push(1);
    charge.extend_from_slice(&(compte.len() as u32).to_be_bytes());
    charge.extend_from_slice(compte.as_bytes());
    charge.extend_from_slice(&expire_le.to_be_bytes());
    charge
}

/// Message signé d'un jeton applicatif (contexte de domaine + charge).
fn message_jeton(charge: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(CONTEXTE_JETON_APPLICATIF.len() + charge.len());
    message.extend_from_slice(CONTEXTE_JETON_APPLICATIF);
    message.extend_from_slice(charge);
    message
}

/// Décode la charge utile d'un jeton applicatif : (compte, expiration).
fn decoder_charge_jeton(charge: &[u8]) -> Option<(String, u64)> {
    if charge.first() != Some(&1) {
        return None;
    }
    let longueur = u32::from_be_bytes(charge.get(1..5)?.try_into().ok()?) as usize;
    let compte = String::from_utf8(charge.get(5..5 + longueur)?.to_vec()).ok()?;
    let reste = charge.get(5 + longueur..)?;
    if reste.len() != 8 || compte.trim().is_empty() {
        return None;
    }
    let expire_le = u64::from_be_bytes(reste.try_into().ok()?);
    Some((compte, expire_le))
}

/// Vérifie un jeton applicatif et renvoie le **compte agissant** qu'il porte.
///
/// Contrôles, dans l'ordre : format (`nda1.<hex>.<hex>`), signature de
/// l'autorité, expiration (`maintenant` en secondes UNIX).
///
/// # Errors
/// [`ErreurJeton`] selon le premier contrôle en échec.
pub fn verifier_jeton_applicatif(
    jeton: &str,
    autorite: &VerifyingKey,
    maintenant: u64,
) -> Result<String, ErreurJeton> {
    let mut parties = jeton.trim().split('.');
    let (Some(prefixe), Some(charge_hex), Some(signature_hex), None) = (
        parties.next(),
        parties.next(),
        parties.next(),
        parties.next(),
    ) else {
        return Err(ErreurJeton::Malforme);
    };
    if prefixe != PREFIXE_JETON {
        return Err(ErreurJeton::Malforme);
    }
    let charge = hex::decode(charge_hex).map_err(|_| ErreurJeton::Malforme)?;
    let signature: [u8; LG_SIGNATURE] = hex::decode(signature_hex)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or(ErreurJeton::Malforme)?;
    let (compte, expire_le) = decoder_charge_jeton(&charge).ok_or(ErreurJeton::Malforme)?;
    autorite
        .verify_strict(&message_jeton(&charge), &Signature::from_bytes(&signature))
        .map_err(|_| ErreurJeton::SignatureInvalide)?;
    if expire_le <= maintenant {
        return Err(ErreurJeton::Expire);
    }
    Ok(compte)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn autorite_test() -> Autorite {
        Autorite::depuis_graine(&[7u8; 32])
    }

    fn autre_autorite() -> Autorite {
        Autorite::depuis_graine(&[8u8; 32])
    }

    #[test]
    fn jeton_enregistrement_aller_retour_et_verification() {
        let autorite = autorite_test();
        let client = SigningKey::from_bytes(&[1u8; 32]);
        let jeton = autorite.emettre_jeton_enregistrement(123_456_789, &client.verifying_key());

        let octets = jeton.to_bytes();
        assert_eq!(octets.len(), LG_JETON_ENREGISTREMENT);
        let relu = JetonEnregistrement::from_bytes(&octets).expect("décodage");
        assert_eq!(relu, jeton);
        assert!(relu.verifier(&autorite.cle_publique()));
        assert_eq!(
            relu.cle_client().expect("clé client").to_bytes(),
            client.verifying_key().to_bytes()
        );

        // Autre autorité : signature invérifiable.
        assert!(!relu.verifier(&autre_autorite().cle_publique()));
        // Jeton altéré (id changé) : refusé.
        let mut altere = octets.clone();
        altere[7] ^= 1;
        let altere = JetonEnregistrement::from_bytes(&altere).expect("décodage");
        assert!(!altere.verifier(&autorite.cle_publique()));
        // Taille inattendue : refusée au décodage.
        assert!(JetonEnregistrement::from_bytes(&octets[..octets.len() - 1]).is_none());
    }

    #[test]
    fn ticket_relais_verifie_signature_et_expiration() {
        let autorite = autorite_test();
        let cle = autorite.cle_publique();
        let ticket = autorite.emettre_ticket_relais(111, 222, 1_000);
        let octets = ticket.to_bytes();
        assert_eq!(octets.len(), LG_TICKET_RELAIS);

        // Valide avant l'expiration, décodé avec sa portée intacte.
        let relu = TicketRelais::verifier(&octets, &cle, 999).expect("ticket valide");
        assert_eq!((relu.id_a, relu.id_b, relu.expire_le), (111, 222, 1_000));
        // Expiré à l'instant exact et au-delà.
        assert_eq!(
            TicketRelais::verifier(&octets, &cle, 1_000),
            Err(ErreurTicket::Expire)
        );
        assert_eq!(
            TicketRelais::verifier(&octets, &cle, 2_000),
            Err(ErreurTicket::Expire)
        );
        // Autre autorité : signature invalide.
        assert_eq!(
            TicketRelais::verifier(&octets, &autre_autorite().cle_publique(), 999),
            Err(ErreurTicket::SignatureInvalide)
        );
        // Altération de la portée : signature invalide.
        let mut altere = octets.clone();
        altere[8] ^= 1;
        assert_eq!(
            TicketRelais::verifier(&altere, &cle, 999),
            Err(ErreurTicket::SignatureInvalide)
        );
        // Mal formé : tronqué ou version inconnue.
        assert_eq!(
            TicketRelais::verifier(&octets[..10], &cle, 999),
            Err(ErreurTicket::Malforme)
        );
        let mut version = octets;
        version[0] = 9;
        assert_eq!(
            TicketRelais::verifier(&version, &cle, 999),
            Err(ErreurTicket::Malforme)
        );
    }

    #[test]
    fn jeton_applicatif_porte_le_compte_et_expire() {
        let autorite = autorite_test();
        let cle = autorite.cle_publique();
        let jeton = autorite.emettre_jeton_applicatif("alice", 5_000);

        assert_eq!(
            verifier_jeton_applicatif(&jeton, &cle, 4_999),
            Ok("alice".to_string())
        );
        // Expiration inclusive : refusé dès l'instant `expire_le`.
        assert_eq!(
            verifier_jeton_applicatif(&jeton, &cle, 5_000),
            Err(ErreurJeton::Expire)
        );
        // Autre autorité : signature invalide.
        assert_eq!(
            verifier_jeton_applicatif(&jeton, &autre_autorite().cle_publique(), 0),
            Err(ErreurJeton::SignatureInvalide)
        );
    }

    #[test]
    fn jeton_applicatif_mal_forme_ou_altere_refuse() {
        let autorite = autorite_test();
        let cle = autorite.cle_publique();
        for mauvais in [
            "",
            "nda1",
            "nda1.zz.zz",
            "pas-un-jeton",
            "nda2.00.00",
            "nda1.00.00.00",
        ] {
            assert_eq!(
                verifier_jeton_applicatif(mauvais, &cle, 0),
                Err(ErreurJeton::Malforme),
                "{mauvais:?}"
            );
        }
        // Charge altérée (compte modifié) : la signature ne correspond plus.
        let jeton = autorite.emettre_jeton_applicatif("alice", 5_000);
        let mut parties: Vec<&str> = jeton.split('.').collect();
        let charge_bob = hex::encode(charge_jeton("bobby", 5_000));
        parties[1] = &charge_bob;
        let falsifie = parties.join(".");
        assert_eq!(
            verifier_jeton_applicatif(&falsifie, &cle, 0),
            Err(ErreurJeton::SignatureInvalide)
        );
    }

    #[test]
    fn autorite_persiste_sa_graine() {
        let chemin = std::env::temp_dir().join(format!(
            "nd-api-autorite-{}-persistance.hex",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&chemin);

        // Première ouverture : la graine est créée puis rechargée à l'identique.
        let premiere = Autorite::charger_ou_creer(&chemin).expect("création");
        let seconde = Autorite::charger_ou_creer(&chemin).expect("rechargement");
        assert_eq!(premiere.cle_publique_hex(), seconde.cle_publique_hex());

        // La clé publique se réimporte depuis l'hexadécimal.
        let cle = cle_publique_depuis_hex(&premiere.cle_publique_hex()).expect("clé publique");
        assert_eq!(cle, premiere.cle_publique());
        assert!(cle_publique_depuis_hex("pas-hexadecimal").is_none());
        assert!(cle_publique_depuis_hex("00ff").is_none());

        let _ = std::fs::remove_file(&chemin);
    }
}
