//! TOTP (RFC 6238) sur HMAC-SHA1 : période 30 s, codes à 6 chiffres, fenêtre
//! de tolérance ±1 pas. Second facteur (2FA) du service de comptes.
//!
//! Le secret est une clé brute de 20 octets (taille recommandée pour SHA-1)
//! tirée de l'aléa du système. La troncature dynamique et le calcul HOTP
//! suivent la RFC 4226 ; la dérivation du compteur depuis le temps Unix suit
//! la RFC 6238 (vecteurs de test de l'annexe B vérifiés plus bas).

use argon2::password_hash::rand_core::{OsRng, RngCore};
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// Période d'un pas TOTP, en secondes (valeur par défaut de la RFC 6238).
pub const PERIODE_S: u64 = 30;
/// Nombre de chiffres d'un code TOTP.
pub const CHIFFRES: u32 = 6;
/// Tolérance : ±1 pas autour du pas courant (dérive d'horloge, saisie lente).
const FENETRE_PAS: u64 = 1;
/// Taille du secret généré, en octets (RFC 4226 §4 : au moins 160 bits pour SHA-1).
const TAILLE_SECRET: usize = 20;

type HmacSha1 = Hmac<Sha1>;

/// Génère un secret TOTP de 20 octets via l'aléa du système.
#[must_use]
pub fn generate_totp_secret() -> Vec<u8> {
    let mut secret = vec![0u8; TAILLE_SECRET];
    OsRng.fill_bytes(&mut secret);
    secret
}

/// Code TOTP (6 chiffres, zéros de tête inclus) au temps Unix donné (secondes).
#[must_use]
pub fn totp_at(secret: &[u8], unix_time: u64) -> String {
    hotp(secret, unix_time / PERIODE_S)
}

/// Vérifie un code au temps Unix donné, avec une fenêtre de ±1 pas.
///
/// Un code malformé (longueur ≠ 6 ou caractère non numérique) est refusé
/// d'emblée. La comparaison des codes est en temps constant.
#[must_use]
pub fn verify_totp(secret: &[u8], code: &str, unix_time: u64) -> bool {
    if code.len() != CHIFFRES as usize || !code.bytes().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let pas_courant = unix_time / PERIODE_S;
    let debut = pas_courant.saturating_sub(FENETRE_PAS);
    let fin = pas_courant.saturating_add(FENETRE_PAS);
    (debut..=fin).any(|pas| egalite_temps_constant(&hotp(secret, pas), code))
}

/// HOTP (RFC 4226) : HMAC-SHA1(secret, compteur BE 8 octets) + troncature dynamique.
fn hotp(secret: &[u8], compteur: u64) -> String {
    let mut mac =
        HmacSha1::new_from_slice(secret).expect("HMAC-SHA1 accepte une clé de toute longueur");
    mac.update(&compteur.to_be_bytes());
    let tag = mac.finalize().into_bytes();
    // Troncature dynamique : les 4 bits de poids faible du dernier octet
    // donnent l'offset d'un mot de 31 bits dans le tag (RFC 4226 §5.3).
    let offset = (tag[tag.len() - 1] & 0x0f) as usize;
    let binaire = u32::from_be_bytes([
        tag[offset] & 0x7f,
        tag[offset + 1],
        tag[offset + 2],
        tag[offset + 3],
    ]);
    let code = binaire % 10u32.pow(CHIFFRES);
    let largeur = CHIFFRES as usize;
    format!("{code:0largeur$}")
}

/// Comparaison en temps constant (évite un oracle de temps sur le code).
fn egalite_temps_constant(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

// ---------------------------------------------------------------------------
// Tests (vecteurs officiels RFC 4226 / RFC 6238)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Secret des vecteurs de test RFC 4226 / RFC 6238 (SHA-1) :
    /// la chaîne ASCII "12345678901234567890".
    const SECRET_RFC: &[u8] = b"12345678901234567890";

    #[test]
    fn vecteurs_rfc_6238_sha1() {
        // Annexe B de la RFC 6238 (colonne SHA-1). Les codes de la RFC font
        // 8 chiffres ; en 6 chiffres on garde les 6 derniers, puisque
        // code_6 = binaire mod 10^6 = (binaire mod 10^8) mod 10^6.
        let vecteurs = [
            (59, "287082"),             // 1970-01-01 00:00:59, code RFC 94287082
            (1_111_111_109, "081804"),  // 2005-03-18 01:58:29, code RFC 07081804
            (1_111_111_111, "050471"),  // 2005-03-18 01:58:31, code RFC 14050471
            (1_234_567_890, "005924"),  // 2009-02-13 23:31:30, code RFC 89005924
            (2_000_000_000, "279037"),  // 2033-05-18 03:33:20, code RFC 69279037
            (20_000_000_000, "353130"), // 2603-10-11 11:33:20, code RFC 65353130
        ];
        for (t, attendu) in vecteurs {
            assert_eq!(totp_at(SECRET_RFC, t), attendu, "temps Unix {t}");
        }
    }

    #[test]
    fn vecteurs_rfc_4226_hotp() {
        // Annexe D de la RFC 4226 : compteurs 0..=9, codes 6 chiffres.
        let codes = [
            "755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583",
            "399871", "520489",
        ];
        for (compteur, attendu) in codes.iter().enumerate() {
            assert_eq!(hotp(SECRET_RFC, compteur as u64), *attendu);
        }
    }

    #[test]
    fn fenetre_de_tolerance_plus_moins_un_pas() {
        let t = 1_111_111_109; // milieu d'un pas
        let code = totp_at(SECRET_RFC, t);
        // Accepté au pas courant et aux pas adjacents (±1).
        assert!(verify_totp(SECRET_RFC, &code, t));
        assert!(verify_totp(SECRET_RFC, &code, t + PERIODE_S));
        assert!(verify_totp(SECRET_RFC, &code, t - PERIODE_S));
        // Refusé à ±2 pas.
        assert!(!verify_totp(SECRET_RFC, &code, t + 2 * PERIODE_S));
        assert!(!verify_totp(SECRET_RFC, &code, t - 2 * PERIODE_S));
    }

    #[test]
    fn codes_malformes_refuses() {
        let t = 59;
        assert!(!verify_totp(SECRET_RFC, "28708", t)); // trop court
        assert!(!verify_totp(SECRET_RFC, "2870820", t)); // trop long
        assert!(!verify_totp(SECRET_RFC, "28x082", t)); // non numérique
        assert!(!verify_totp(SECRET_RFC, "", t)); // vide
    }

    #[test]
    fn secrets_generes_aleatoires() {
        let s1 = generate_totp_secret();
        let s2 = generate_totp_secret();
        assert_eq!(s1.len(), TAILLE_SECRET);
        assert_ne!(s1, s2, "deux secrets consécutifs doivent différer");
    }

    #[test]
    fn debut_des_temps_sans_debordement() {
        // À t < période, la fenêtre basse sature à 0 au lieu de déborder.
        let code = totp_at(SECRET_RFC, 0);
        assert!(verify_totp(SECRET_RFC, &code, 0));
    }
}
