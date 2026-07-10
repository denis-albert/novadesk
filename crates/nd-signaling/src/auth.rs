//! Enregistrement **authentifié** auprès du rendez-vous de production (plan 11).
//!
//! Le serveur de production (`server/nd-rendezvous`) refuse le `Register` nu
//! (tag 1) : l'enregistrement doit **prouver la possession de l'ID** — jeton
//! d'attribution signé par l'autorité du déploiement + signature Ed25519
//! fraîche du client. Ce module fournit, côté client, la trame
//! [`RegisterAuthentifie`] (tag [`TAG_REGISTER_AUTHENTIFIE`]) envoyée par
//! [`crate::RendezvousClient::register_authentifie`].
//!
//! # Source de vérité du format
//!
//! La **source de vérité** est `server/nd-rendezvous/src/lib.rs`
//! (`verifier_register_authentifie`, parseur, et `trame_register_authentifie`,
//! constructeur de référence) : c'est lui qui parse et vérifie la trame. On ne
//! dépend **pas** du crate serveur à l'exécution (graphe à l'envers) : on
//! duplique le format, aligné octet à octet — même démarche que
//! `nd-transport::ticket` pour les tickets de relais. Le test d'intégration
//! `tests/register_authentifie.rs` compare la trame à la référence serveur et
//! prouve son acceptation (et le refus d'une signature invalide) par la façade
//! réelle montée en process.
//!
//! # Trame `RegisterAuthentifie` (tag 8, gros-boutiste)
//!
//! ```text
//! [8][id u64 BE][addr u32 BE + UTF-8][cert u32 BE + octets][horodatage u64 BE]
//!    [jeton u32 BE + octets][signature 64 octets]
//! ```
//!
//! - `jeton` : jeton d'enregistrement émis par le service d'attribution d'ID
//!   (`nd-api`), qui lie l'ID à la clé statique du client. Il est **opaque**
//!   pour ce module (seul le serveur le vérifie) : le client le transmet tel
//!   qu'il l'a reçu ;
//! - `signature` : signature Ed25519 du client sur le **message canonique**
//!   [`message_enregistrement`] `(contexte, id, addr, cert, horodatage)` —
//!   exactement ce que le serveur vérifie. Elle prouve la **possession** de la
//!   clé liée à l'ID (un jeton observé sur le réseau ne suffit pas) et scelle
//!   l'adresse et le certificat publiés ;
//! - `horodatage` (secondes UNIX, [`maintenant_unix`]) : borne le rejeu à la
//!   fenêtre de tolérance du serveur (±300 s par défaut).
//!
//! # Pourquoi le heartbeat reste « nu »
//!
//! Le rendez-vous de production n'a **pas** de variante authentifiée du
//! heartbeat : sa façade ne filtre que l'enregistrement (tag 1 refusé, tag 8
//! vérifié) et **transmet tels quels** `Heartbeat`, `PublishCandidates`,
//! `Punch` et `PollPunch` au moteur (limite documentée sur son
//! `servir_authentifie`, coordination lot 05). Seul un `Register` peut lier ou
//! **remplacer** l'(adresse, certificat) d'un ID ; un tiers peut au pire
//! rafraîchir la présence d'un ID enregistré ou déposer des candidats de punch
//! à sa place (dérangement), l'épinglage de certificat de la couche transport
//! bornant l'impact. Après un
//! [`crate::RendezvousClient::register_authentifie`], les
//! [`crate::RendezvousClient::heartbeat`] et autres messages existants restent
//! donc les bons — inutile (et impossible) de les signer aujourd'hui.

use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::Signer;
pub use ed25519_dalek::{Signature, SigningKey, VerifyingKey};

use crate::{put_bytes, read_bytes, read_chaine, read_u64, read_u8};

/// Tag de la trame d'enregistrement authentifié — extension de la façade de
/// production (les tags 1..=7 appartiennent au protocole nu de ce crate).
/// Identique à `nd-rendezvous::TAG_REGISTER_AUTHENTIFIE`.
pub const TAG_REGISTER_AUTHENTIFIE: u8 = 8;

/// Longueur (octets) d'une signature Ed25519.
pub const LG_SIGNATURE: usize = 64;

/// Contexte de domaine de la signature d'enregistrement, identique au serveur
/// (`nd-rendezvous::CONTEXTE_ENREGISTREMENT`) : il isole cette preuve de
/// possession des autres artefacts Ed25519 du déploiement (jetons, tickets).
const CONTEXTE_ENREGISTREMENT: &[u8] = b"novadesk-rendezvous-enregistrement-v1";

/// Secondes UNIX courantes (0 si l'horloge précède l'époque — improbable).
/// Sert d'horodatage anti-rejeu à la trame authentifiée.
#[must_use]
pub fn maintenant_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Message canonique signé par le client pour prouver la possession de la clé
/// liée à son ID : `(contexte, id, addr, cert, horodatage)`. Aligné octet à
/// octet sur `nd-rendezvous::message_enregistrement` (ce que le serveur
/// vérifie est exactement ce que le client signe).
#[must_use]
pub fn message_enregistrement(id: u64, addr: &str, cert: &[u8], horodatage: u64) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(CONTEXTE_ENREGISTREMENT.len() + 8 + 4 + addr.len() + 4 + cert.len() + 8);
    message.extend_from_slice(CONTEXTE_ENREGISTREMENT);
    message.extend_from_slice(&id.to_be_bytes());
    put_bytes(&mut message, addr.as_bytes());
    put_bytes(&mut message, cert);
    message.extend_from_slice(&horodatage.to_be_bytes());
    message
}

/// Trame d'enregistrement authentifié (tag 8) du rendez-vous de production :
/// coordonnées publiées, horodatage anti-rejeu, jeton d'attribution (opaque)
/// et signature de possession. Voir la disposition en tête de module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterAuthentifie {
    /// ID NovaDesk publié.
    pub id: u64,
    /// Adresse (UDP/QUIC) publiée, en textuel.
    pub addr: String,
    /// Certificat auto-signé (DER) publié.
    pub cert: Vec<u8>,
    /// Horodatage de la trame (secondes UNIX), couvert par la signature.
    pub horodatage: u64,
    /// Jeton d'enregistrement émis par l'autorité (opaque pour le client).
    pub jeton: Vec<u8>,
    /// Signature Ed25519 du client sur [`message_enregistrement`].
    pub signature: [u8; LG_SIGNATURE],
}

impl RegisterAuthentifie {
    /// Construit la trame et produit la signature de possession avec `cle`
    /// (la clé statique du client, celle que le jeton lie à `id`).
    #[must_use]
    pub fn signer(
        id: u64,
        addr: &str,
        cert: &[u8],
        horodatage: u64,
        jeton: &[u8],
        cle: &SigningKey,
    ) -> Self {
        let signature = cle
            .sign(&message_enregistrement(id, addr, cert, horodatage))
            .to_bytes();
        Self {
            id,
            addr: addr.to_owned(),
            cert: cert.to_vec(),
            horodatage,
            jeton: jeton.to_vec(),
            signature,
        }
    }

    /// Vérifie la signature de possession contre `cle_client` (la clé publique
    /// que le jeton lie à l'ID) — le même contrôle que le serveur
    /// (`verify_strict` sur [`message_enregistrement`]). Sert aux tests de
    /// symétrie ; en production c'est le serveur qui vérifie.
    #[must_use]
    pub fn verifier_possession(&self, cle_client: &VerifyingKey) -> bool {
        cle_client
            .verify_strict(
                &message_enregistrement(self.id, &self.addr, &self.cert, self.horodatage),
                &Signature::from_bytes(&self.signature),
            )
            .is_ok()
    }

    /// Encode la trame telle que le serveur la parse (tag 8 inclus).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut trame = Vec::with_capacity(
            1 + 8
                + 4
                + self.addr.len()
                + 4
                + self.cert.len()
                + 8
                + 4
                + self.jeton.len()
                + LG_SIGNATURE,
        );
        trame.push(TAG_REGISTER_AUTHENTIFIE);
        trame.extend_from_slice(&self.id.to_be_bytes());
        put_bytes(&mut trame, self.addr.as_bytes());
        put_bytes(&mut trame, &self.cert);
        trame.extend_from_slice(&self.horodatage.to_be_bytes());
        put_bytes(&mut trame, &self.jeton);
        trame.extend_from_slice(&self.signature);
        trame
    }

    /// Décode une trame (symétrique de [`RegisterAuthentifie::to_bytes`]),
    /// avec la même rigueur que le parseur serveur : tag exact, adresse UTF-8,
    /// signature de 64 octets, **aucun octet excédentaire**. `None` si la
    /// trame est mal formée.
    #[must_use]
    pub fn from_bytes(trame: &[u8]) -> Option<Self> {
        let mut p = 0;
        if read_u8(trame, &mut p)? != TAG_REGISTER_AUTHENTIFIE {
            return None;
        }
        let id = read_u64(trame, &mut p)?;
        let addr = read_chaine(trame, &mut p)?;
        let cert = read_bytes(trame, &mut p)?;
        let horodatage = read_u64(trame, &mut p)?;
        let jeton = read_bytes(trame, &mut p)?;
        let signature: [u8; LG_SIGNATURE] = trame.get(p..p + LG_SIGNATURE)?.try_into().ok()?;
        p += LG_SIGNATURE;
        (p == trame.len()).then_some(Self {
            id,
            addr,
            cert,
            horodatage,
            jeton,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clé statique de test déterministe (côté client).
    fn cle_test() -> SigningKey {
        SigningKey::from_bytes(&[5u8; 32])
    }

    /// Trame de test aux champs discriminants (chaque champ a une valeur
    /// reconnaissable pour détecter toute inversion d'ordre).
    fn trame_test() -> RegisterAuthentifie {
        RegisterAuthentifie::signer(
            0x0102_0304_0506_0708,
            "203.0.113.7:45000",
            &[0xAA, 0xBB, 0xCC],
            1_753_000_000,
            &[0xE0, 0xE1, 0xE2, 0xE3, 0xE4],
            &cle_test(),
        )
    }

    /// Le message canonique est disposé comme celui que le serveur vérifie :
    /// `contexte || id BE || (u32 BE + addr) || (u32 BE + cert) || horodatage BE`
    /// (référence : `nd-rendezvous::message_enregistrement`).
    #[test]
    fn message_canonique_dispose_comme_le_serveur() {
        let attendu = {
            let mut m = Vec::new();
            m.extend_from_slice(b"novadesk-rendezvous-enregistrement-v1");
            m.extend_from_slice(&42u64.to_be_bytes());
            m.extend_from_slice(&4u32.to_be_bytes());
            m.extend_from_slice(b"a:80");
            m.extend_from_slice(&2u32.to_be_bytes());
            m.extend_from_slice(&[9, 9]);
            m.extend_from_slice(&7u64.to_be_bytes());
            m
        };
        assert_eq!(message_enregistrement(42, "a:80", &[9, 9], 7), attendu);
    }

    /// La trame encodée suit octet à octet la disposition que le parseur
    /// serveur attend (référence : `nd-rendezvous::verifier_register_authentifie`).
    #[test]
    fn trame_disposee_octet_a_octet_comme_le_parseur_serveur() {
        let trame = trame_test();
        let octets = trame.to_bytes();

        assert_eq!(octets[0], TAG_REGISTER_AUTHENTIFIE);
        assert_eq!(&octets[1..9], &0x0102_0304_0506_0708u64.to_be_bytes());
        // Adresse : longueur u32 BE + UTF-8.
        assert_eq!(&octets[9..13], &17u32.to_be_bytes());
        assert_eq!(&octets[13..30], b"203.0.113.7:45000");
        // Certificat : longueur u32 BE + octets.
        assert_eq!(&octets[30..34], &3u32.to_be_bytes());
        assert_eq!(&octets[34..37], &[0xAA, 0xBB, 0xCC]);
        // Horodatage u64 BE.
        assert_eq!(&octets[37..45], &1_753_000_000u64.to_be_bytes());
        // Jeton : longueur u32 BE + octets.
        assert_eq!(&octets[45..49], &5u32.to_be_bytes());
        assert_eq!(&octets[49..54], &[0xE0, 0xE1, 0xE2, 0xE3, 0xE4]);
        // Signature : exactement 64 octets, rien après.
        assert_eq!(octets.len(), 54 + LG_SIGNATURE);
        assert_eq!(&octets[54..], &trame.signature);
    }

    #[test]
    fn encodage_decodage_symetriques() {
        let trame = trame_test();
        let relue = RegisterAuthentifie::from_bytes(&trame.to_bytes()).expect("décodage");
        assert_eq!(relue, trame);
        // La signature relue prouve toujours la possession de la clé.
        assert!(relue.verifier_possession(&cle_test().verifying_key()));
    }

    /// La signature couvre (id, addr, cert, horodatage) : la bonne clé la
    /// vérifie, une autre clé non, et tout champ couvert altéré l'invalide.
    #[test]
    fn signature_prouve_la_possession_et_scelle_les_champs() {
        let trame = trame_test();
        assert!(trame.verifier_possession(&cle_test().verifying_key()));
        let autre_cle = SigningKey::from_bytes(&[6u8; 32]);
        assert!(!trame.verifier_possession(&autre_cle.verifying_key()));

        let alterations: [fn(&mut RegisterAuthentifie); 4] = [
            |t| t.id ^= 1,
            |t| t.addr.push('9'),
            |t| t.cert.push(0),
            |t| t.horodatage += 1,
        ];
        for alterer in alterations {
            let mut alteree = trame.clone();
            alterer(&mut alteree);
            assert!(!alteree.verifier_possession(&cle_test().verifying_key()));
        }
        // Le jeton, lui, n'est pas couvert par CETTE signature (il porte la
        // sienne, celle de l'autorité, vérifiée par le serveur).
        let mut jeton_change = trame.clone();
        jeton_change.jeton.push(0);
        assert!(jeton_change.verifier_possession(&cle_test().verifying_key()));
    }

    /// Mêmes refus que le parseur serveur : tag inconnu, troncature, octets
    /// excédentaires, adresse non UTF-8, trame vide.
    #[test]
    fn from_bytes_rejette_les_trames_mal_formees() {
        let valide = trame_test().to_bytes();
        assert!(RegisterAuthentifie::from_bytes(&valide).is_some());

        // Tag du `Register` nu (1) ou inconnu : refusé.
        let mut mauvais_tag = valide.clone();
        mauvais_tag[0] = 1;
        assert!(RegisterAuthentifie::from_bytes(&mauvais_tag).is_none());

        // Troncature à toutes les longueurs : jamais de panique, toujours None.
        for fin in 0..valide.len() {
            assert!(RegisterAuthentifie::from_bytes(&valide[..fin]).is_none());
        }

        // Octet excédentaire : refusé (le serveur exige `p == trame.len()`).
        let mut excedent = valide.clone();
        excedent.push(0);
        assert!(RegisterAuthentifie::from_bytes(&excedent).is_none());

        // Adresse non UTF-8 : refusée (octet 0xFF dans « a:80 » → invalide).
        let mut non_utf8 = valide;
        non_utf8[13] = 0xFF;
        // 0xFF seul n'est jamais de l'UTF-8 valide.
        assert!(RegisterAuthentifie::from_bytes(&non_utf8).is_none());

        assert!(RegisterAuthentifie::from_bytes(&[]).is_none());
    }
}
