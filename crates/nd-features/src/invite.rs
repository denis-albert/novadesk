//! Invitations de session éphémères (type « QuickSupport ») : un code court
//! et lisible que l'utilisateur aidé communique au technicien, avec durée de
//! vie limitée et option usage unique.
//!
//! # Sécurité — AVERTISSEMENT
//!
//! L'aléa des codes est dérivé de sources `std` (horloge nanoseconde, adresse
//! d'une variable de pile, compteur atomique) mélangées par SplitMix64. C'est
//! suffisant pour éviter les collisions accidentelles, mais ce n'est **PAS
//! cryptographique** : un attaquant capable d'estimer l'horloge peut réduire
//! l'espace de recherche. Un vrai CSPRNG (`OsRng` via `nd-crypto`) remplacera
//! [`random_code`] avant toute exposition réseau (voir plan 13, §invitations).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Alphabet des codes : 32 symboles sans caractères ambigus (ni `I`, `O`,
/// `0`, `1`), pour une dictée sans erreur au téléphone.
pub const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Nombre de symboles utiles d'un code (hors tirets), soit 9 × 5 = 45 bits.
pub const CODE_SYMBOLS: usize = 9;

/// Compteur global : garantit des graines distinctes même si deux codes sont
/// générés dans le même quantum d'horloge.
static COMPTEUR: AtomicU64 = AtomicU64::new(0);

/// Invitation de session éphémère.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInvite {
    /// Code lisible au format `XXX-XXX-XXX` (alphabet [`CODE_ALPHABET`]).
    pub code: String,
    /// Instant d'expiration (secondes Unix). Le code est valide
    /// **strictement avant** cet instant.
    pub expires_unix: u64,
    /// Si vrai, le code est consommé au premier échange réussi.
    pub one_time: bool,
}

/// Résultat d'une tentative d'échange d'un code d'invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemResult {
    /// Code accepté (et consommé s'il était à usage unique).
    Valid,
    /// Code connu mais expiré.
    Expired,
    /// Code inconnu du magasin.
    Unknown,
    /// Code à usage unique déjà consommé.
    AlreadyUsed,
}

/// Secondes Unix courantes (0 si l'horloge est antérieure à l'époque Unix,
/// cas pathologique qu'on ne fait pas remonter).
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Génère une invitation valable `ttl_secs` secondes à partir de maintenant.
///
/// Voir l'avertissement de sécurité du module : le code n'est pas
/// cryptographique à ce stade.
#[must_use]
pub fn generate_invite(ttl_secs: u64, one_time: bool) -> SessionInvite {
    SessionInvite {
        code: random_code(),
        expires_unix: unix_now().saturating_add(ttl_secs),
        one_time,
    }
}

/// Finaliseur SplitMix64 : diffuse chaque bit d'entrée sur toute la sortie.
fn melange64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Tire un code `XXX-XXX-XXX`. **Pas cryptographique** (voir doc du module) :
/// graine = nanos d'horloge ⊕ adresse de pile (ASLR) ⊕ compteur atomique,
/// puis un tour de SplitMix64 par symbole.
fn random_code() -> String {
    let marqueur = 0u8;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let adresse = std::ptr::addr_of!(marqueur) as usize as u64;
    let compteur = COMPTEUR.fetch_add(1, Ordering::Relaxed);
    let mut etat = nanos ^ adresse.rotate_left(32) ^ melange64(compteur);

    let mut code = String::with_capacity(CODE_SYMBOLS + 2);
    for i in 0..CODE_SYMBOLS {
        if i > 0 && i % 3 == 0 {
            code.push('-');
        }
        etat = melange64(etat);
        code.push(char::from(CODE_ALPHABET[(etat & 31) as usize]));
    }
    code
}

/// Entrée interne du magasin d'invitations.
#[derive(Debug)]
struct Entree {
    expires_unix: u64,
    one_time: bool,
    used: bool,
}

/// Magasin d'invitations en mémoire, côté machine contrôlée.
///
/// Précédence de [`InviteStore::redeem`] : `Unknown` > `AlreadyUsed` >
/// `Expired` > `Valid` — un code consommé reste « déjà utilisé » même une
/// fois expiré (message plus utile pour diagnostiquer un rejeu).
#[derive(Debug, Default)]
pub struct InviteStore {
    entrees: HashMap<String, Entree>,
}

impl InviteStore {
    /// Magasin vide.
    #[must_use]
    pub fn new() -> Self {
        InviteStore::default()
    }

    /// Enregistre une invitation (générée ici ou reçue d'ailleurs).
    /// Ré-enregistrer le même code réinitialise son état.
    pub fn register(&mut self, invite: &SessionInvite) {
        self.entrees.insert(
            invite.code.clone(),
            Entree {
                expires_unix: invite.expires_unix,
                one_time: invite.one_time,
                used: false,
            },
        );
    }

    /// Génère une invitation via [`generate_invite`], l'enregistre et la rend.
    pub fn issue(&mut self, ttl_secs: u64, one_time: bool) -> SessionInvite {
        let invite = generate_invite(ttl_secs, one_time);
        self.register(&invite);
        invite
    }

    /// Tente d'échanger `code` à l'instant `now_unix` (secondes Unix).
    ///
    /// Un code est valide **strictement avant** son expiration
    /// (`now_unix < expires_unix`). Un échange réussi marque le code comme
    /// consommé ; pour un code à usage unique, tout échange ultérieur rend
    /// [`RedeemResult::AlreadyUsed`].
    pub fn redeem(&mut self, code: &str, now_unix: u64) -> RedeemResult {
        let Some(entree) = self.entrees.get_mut(code) else {
            return RedeemResult::Unknown;
        };
        if entree.one_time && entree.used {
            return RedeemResult::AlreadyUsed;
        }
        if now_unix >= entree.expires_unix {
            return RedeemResult::Expired;
        }
        entree.used = true;
        RedeemResult::Valid
    }

    /// Supprime les codes expirés à `now_unix` ; rend le nombre supprimé.
    pub fn purge_expired(&mut self, now_unix: u64) -> usize {
        let avant = self.entrees.len();
        self.entrees.retain(|_, e| now_unix < e.expires_unix);
        avant - self.entrees.len()
    }

    /// Nombre d'invitations enregistrées (consommées ou non).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entrees.len()
    }

    /// Vrai si aucun code n'est enregistré.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entrees.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invitation fabriquée de toutes pièces (tests déterministes).
    fn invitation(code: &str, expires_unix: u64, one_time: bool) -> SessionInvite {
        SessionInvite {
            code: code.into(),
            expires_unix,
            one_time,
        }
    }

    #[test]
    fn format_du_code() {
        let invite = generate_invite(300, true);
        assert!(invite.one_time);
        // `XXX-XXX-XXX` : 11 caractères, tirets aux positions 3 et 7.
        let octets = invite.code.as_bytes();
        assert_eq!(octets.len(), 11);
        for (i, o) in octets.iter().enumerate() {
            if i == 3 || i == 7 {
                assert_eq!(*o, b'-');
            } else {
                assert!(CODE_ALPHABET.contains(o), "symbole hors alphabet : {o}");
            }
        }
    }

    #[test]
    fn expiration_derivee_du_ttl() {
        let avant = unix_now();
        let invite = generate_invite(3600, false);
        let apres = unix_now();
        assert!(invite.expires_unix >= avant + 3600);
        assert!(invite.expires_unix <= apres + 3600);
        assert!(!invite.one_time);
    }

    #[test]
    fn codes_distincts() {
        let mut vus = std::collections::HashSet::new();
        for _ in 0..100 {
            assert!(vus.insert(generate_invite(60, true).code), "code en double");
        }
    }

    #[test]
    fn echange_valide_puis_reutilisable_si_multi_usage() {
        let mut magasin = InviteStore::new();
        magasin.register(&invitation("AAA-BBB-CCC", 1_000, false));
        assert_eq!(magasin.redeem("AAA-BBB-CCC", 500), RedeemResult::Valid);
        // Multi-usage : rejouable tant que non expiré.
        assert_eq!(magasin.redeem("AAA-BBB-CCC", 600), RedeemResult::Valid);
    }

    #[test]
    fn usage_unique_consomme() {
        let mut magasin = InviteStore::new();
        magasin.register(&invitation("AAA-BBB-CCC", 1_000, true));
        assert_eq!(magasin.redeem("AAA-BBB-CCC", 500), RedeemResult::Valid);
        assert_eq!(
            magasin.redeem("AAA-BBB-CCC", 501),
            RedeemResult::AlreadyUsed
        );
        // Même expiré, il reste « déjà utilisé » (précédence documentée).
        assert_eq!(
            magasin.redeem("AAA-BBB-CCC", 2_000),
            RedeemResult::AlreadyUsed
        );
    }

    #[test]
    fn expiration_stricte() {
        let mut magasin = InviteStore::new();
        magasin.register(&invitation("AAA-BBB-CCC", 1_000, true));
        // Pile à l'expiration : refusé (validité strictement avant).
        assert_eq!(magasin.redeem("AAA-BBB-CCC", 1_000), RedeemResult::Expired);
        assert_eq!(magasin.redeem("AAA-BBB-CCC", 5_000), RedeemResult::Expired);
        // L'échec n'a pas consommé le code : valide juste avant l'expiration.
        assert_eq!(magasin.redeem("AAA-BBB-CCC", 999), RedeemResult::Valid);
    }

    #[test]
    fn code_inconnu() {
        let mut magasin = InviteStore::new();
        assert_eq!(magasin.redeem("ZZZ-ZZZ-ZZZ", 0), RedeemResult::Unknown);
    }

    #[test]
    fn issue_enregistre_et_purge_nettoie() {
        let mut magasin = InviteStore::new();
        assert!(magasin.is_empty());
        let invite = magasin.issue(60, true);
        assert_eq!(magasin.len(), 1);
        assert_eq!(
            magasin.redeem(&invite.code, invite.expires_unix - 1),
            RedeemResult::Valid
        );
        // Purge après expiration : le magasin se vide.
        assert_eq!(magasin.purge_expired(invite.expires_unix), 1);
        assert!(magasin.is_empty());
        assert_eq!(
            magasin.redeem(&invite.code, invite.expires_unix - 1),
            RedeemResult::Unknown
        );
    }
}
