//! Chiffrement au repos et dérivation de clés du service de comptes (plan 11).
//!
//! Les secrets durables (secrets TOTP aujourd'hui, autres jetons sensibles
//! demain) ne touchent **jamais** le disque en clair : ils sont scellés par un
//! AEAD **ChaCha20-Poly1305** sous une clé dérivée du **secret serveur**
//! (voir `AccountStore::open` : variable d'environnement ou fichier de clé).
//!
//! Dérivation : `HMAC-SHA256(secret, contexte)` — chaque usage a son étiquette
//! de domaine (chiffrement du stockage, graine Ed25519 des jetons applicatifs…),
//! si bien qu'une clé compromise n'en révèle aucune autre et que le même
//! secret serveur redonne les mêmes clés à chaque démarrage.
//!
//! Enveloppe scellée : `nonce (12 octets d'aléa système) || chiffré+étiquette`.
//! Les **données associées** (AAD) lient le secret à son propriétaire (l'e-mail
//! du compte) : déplacer un blob chiffré d'un compte à un autre dans la base
//! rend le déchiffrement impossible.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Taille du nonce ChaCha20-Poly1305, en octets (RFC 8439).
pub const TAILLE_NONCE: usize = 12;

/// Étiquette de dérivation de la clé de chiffrement du stockage.
const CONTEXTE_STOCKAGE: &str = "nd-accounts/chiffrement-stockage/v1";

/// Dérive une clé de 32 octets du secret serveur pour un usage donné :
/// `HMAC-SHA256(secret, contexte)`. Déterministe (même secret + même contexte
/// → même clé) ; deux contextes distincts donnent des clés indépendantes.
#[must_use]
pub fn deriver_cle(secret: &[u8], contexte: &str) -> [u8; 32] {
    // Chemin qualifié : `Mac` et `aead::KeyInit` offrent tous deux
    // `new_from_slice`, il faut lever l'ambiguïté.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret)
        .expect("HMAC-SHA256 accepte une clé de toute longueur");
    mac.update(contexte.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Chiffreur AEAD des secrets au repos (clé dérivée du secret serveur).
/// Clonable : les clones partagent la même clé (elle est copiée, pas comptée).
#[derive(Clone)]
pub struct Chiffreur {
    cle: Key,
}

impl Chiffreur {
    /// Chiffreur dont la clé est dérivée du secret serveur
    /// (contexte [`CONTEXTE_STOCKAGE`]).
    #[must_use]
    pub fn depuis_secret(secret: &[u8]) -> Self {
        Self {
            cle: Key::from(deriver_cle(secret, CONTEXTE_STOCKAGE)),
        }
    }

    /// Scelle `clair` avec un nonce aléatoire : `nonce || chiffré+étiquette`.
    /// `aad` (données associées, p. ex. l'e-mail du compte) doit être identique
    /// au déchiffrement — il lie le blob à son propriétaire sans être stocké.
    #[must_use]
    pub fn chiffrer(&self, clair: &[u8], aad: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; TAILLE_NONCE];
        OsRng.fill_bytes(&mut nonce);
        let scelle = ChaCha20Poly1305::new(&self.cle)
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: clair, aad })
            .expect("le chiffrement ChaCha20-Poly1305 n'échoue qu'à l'allocation");
        let mut blob = Vec::with_capacity(TAILLE_NONCE + scelle.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&scelle);
        blob
    }

    /// Ouvre un blob scellé par [`Self::chiffrer`]. `None` si le blob est
    /// tronqué, altéré, chiffré sous une autre clé ou lié à d'autres données
    /// associées (l'étiquette Poly1305 fait foi, en temps constant).
    #[must_use]
    pub fn dechiffrer(&self, blob: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        if blob.len() < TAILLE_NONCE {
            return None;
        }
        let (nonce, scelle) = blob.split_at(TAILLE_NONCE);
        ChaCha20Poly1305::new(&self.cle)
            .decrypt(Nonce::from_slice(nonce), Payload { msg: scelle, aad })
            .ok()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_deterministe_et_separee_par_contexte() {
        let secret = b"secret-serveur-de-test";
        // Même secret + même contexte : même clé (stable entre démarrages).
        assert_eq!(deriver_cle(secret, "a"), deriver_cle(secret, "a"));
        // Contextes distincts : clés indépendantes.
        assert_ne!(deriver_cle(secret, "a"), deriver_cle(secret, "b"));
        // Secrets distincts : clés indépendantes.
        assert_ne!(deriver_cle(secret, "a"), deriver_cle(b"autre", "a"));
    }

    #[test]
    fn chiffrement_aller_retour() {
        let chiffreur = Chiffreur::depuis_secret(b"secret");
        let clair = b"secret TOTP de vingt octets!";
        let blob = chiffreur.chiffrer(clair, b"alice@example.com");
        // Le clair n'apparaît pas dans le blob scellé.
        assert!(!blob
            .windows(clair.len())
            .any(|fenetre| fenetre == clair.as_slice()));
        assert_eq!(
            chiffreur.dechiffrer(&blob, b"alice@example.com").as_deref(),
            Some(clair.as_slice())
        );
        // Deux scellés du même clair diffèrent (nonce aléatoire).
        assert_ne!(blob, chiffreur.chiffrer(clair, b"alice@example.com"));
    }

    #[test]
    fn dechiffrement_refuse_mauvaise_cle_ou_alteration() {
        let chiffreur = Chiffreur::depuis_secret(b"secret");
        let blob = chiffreur.chiffrer(b"donnee", b"aad");

        // Autre clé serveur : refus.
        assert_eq!(
            Chiffreur::depuis_secret(b"autre").dechiffrer(&blob, b"aad"),
            None
        );
        // Un octet altéré (dans le chiffré) : refus.
        let mut altere = blob.clone();
        *altere.last_mut().expect("blob non vide") ^= 1;
        assert_eq!(chiffreur.dechiffrer(&altere, b"aad"), None);
        // Blob tronqué (plus court qu'un nonce) : refus sans panique.
        assert_eq!(
            chiffreur.dechiffrer(&blob[..TAILLE_NONCE - 1], b"aad"),
            None
        );
        assert_eq!(chiffreur.dechiffrer(&[], b"aad"), None);
    }

    #[test]
    fn aad_lie_le_secret_a_son_proprietaire() {
        let chiffreur = Chiffreur::depuis_secret(b"secret");
        let blob = chiffreur.chiffrer(b"secret-totp", b"alice@example.com");
        // Le même blob « déplacé » vers un autre compte ne s'ouvre pas.
        assert_eq!(chiffreur.dechiffrer(&blob, b"eve@example.com"), None);
        assert!(chiffreur.dechiffrer(&blob, b"alice@example.com").is_some());
    }
}
