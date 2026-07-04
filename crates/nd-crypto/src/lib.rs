//! `nd-crypto` — session chiffrée de bout en bout.
//!
//! Le handshake s'appuiera sur le Noise Protocol Framework (crate `snow`) : Noise_XX
//! pour la première connexion, Noise_IK pour l'accès non-surveillé, X25519 +
//! AES-256-GCM / ChaCha20-Poly1305. La protection anti-MITM repose sur la comparaison
//! d'un SAS (short authentication string). Modèle de menace et détails :
//! `../../plan-technique/06-securite-chiffrement.md`.

use nd_proto::{NdError, Result};

/// Empreinte de la clé publique d'un pair (hash 32 octets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerFingerprint(pub [u8; 32]);

impl PeerFingerprint {
    /// Dérive un SAS numérique à 6 chiffres, comparé de visu par les deux utilisateurs
    /// pour détecter un homme-du-milieu (voir plan 06 §protection MITM).
    #[must_use]
    pub fn sas(&self) -> String {
        // Combine les 4 premiers octets en un entier, réduit modulo 1e6.
        let n = u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]);
        format!("{:06}", n % 1_000_000)
    }

    /// Représentation hexadécimale courte pour affichage/journalisation.
    #[must_use]
    pub fn short_hex(&self) -> String {
        self.0[..4].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Rôle dans le handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeRole {
    Initiator,
    Responder,
}

/// Session sécurisée établie entre deux pairs.
///
/// Fournit le chiffrement/déchiffrement AEAD des charges applicatives une fois le
/// handshake terminé. Le relais éventuel ne voit que le ciphertext (voir plan 05/06).
pub trait SecureSession: Send {
    /// Empreinte locale (à afficher pour vérification).
    fn local_fingerprint(&self) -> PeerFingerprint;
    /// Empreinte du pair distant, une fois le handshake terminé.
    fn remote_fingerprint(&self) -> Option<PeerFingerprint>;
    /// Chiffre une charge applicative.
    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>>;
    /// Déchiffre une charge reçue.
    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>>;
}

/// Démarre un handshake de session dans le rôle donné. Non implémenté à ce stade.
pub fn start_handshake(_role: HandshakeRole) -> Result<Box<dyn SecureSession>> {
    Err(NdError::NotImplemented(
        "nd-crypto::start_handshake (Noise/snow à venir, voir plan 06/16)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_fait_six_chiffres() {
        let fp = PeerFingerprint([0xAB; 32]);
        let sas = fp.sas();
        assert_eq!(sas.len(), 6);
        assert!(sas.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn short_hex_fait_huit_caracteres() {
        let fp = PeerFingerprint([0x0f; 32]);
        assert_eq!(fp.short_hex(), "0f0f0f0f");
    }
}
