//! Invitations de session éphémères (type « QuickSupport ») : un code court
//! et lisible que l'utilisateur aidé communique au technicien, avec durée de
//! vie limitée et option usage unique.
//!
//! # Sécurité
//!
//! Les codes sont tirés du **CSPRNG du système** (`getrandom`, qui s'appuie
//! sur `ProcessPrng`/`BCryptGenRandom` sous Windows, `getrandom(2)` sous
//! Linux…) : 45 bits d'entropie par code (9 symboles × 5 bits), sans biais —
//! l'alphabet de 32 symboles se prête à un tirage exact par groupes de
//! 5 bits. Aucune source dérivée de l'horloge (voir plan 13, §invitations).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Alphabet des codes : 32 symboles sans caractères ambigus (ni `I`, `O`,
/// `0`, `1`), pour une dictée sans erreur au téléphone.
pub const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Nombre de symboles utiles d'un code (hors tirets), soit 9 × 5 = 45 bits.
pub const CODE_SYMBOLS: usize = 9;

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
/// Le code provient du CSPRNG du système (voir la section Sécurité du module).
///
/// # Panics
///
/// Si la source d'aléa du système est indisponible — situation pathologique
/// (OS cassé) où il serait dangereux de continuer avec des codes devinables.
#[must_use]
pub fn generate_invite(ttl_secs: u64, one_time: bool) -> SessionInvite {
    SessionInvite {
        code: random_code(),
        expires_unix: unix_now().saturating_add(ttl_secs),
        one_time,
    }
}

/// Tire un code `XXX-XXX-XXX` depuis le CSPRNG du système : 64 bits d'aléa
/// OS, consommés 5 bits par symbole (l'alphabet compte exactement 32 symboles,
/// le tirage est donc uniforme, sans biais de modulo).
fn random_code() -> String {
    let mut octets = [0u8; 8];
    getrandom::fill(&mut octets)
        .expect("source d'aléa du système indisponible : codes d'invitation impossibles");
    let mut alea = u64::from_le_bytes(octets);

    let mut code = String::with_capacity(CODE_SYMBOLS + 2);
    for i in 0..CODE_SYMBOLS {
        if i > 0 && i % 3 == 0 {
            code.push('-');
        }
        code.push(char::from(CODE_ALPHABET[(alea & 31) as usize]));
        alea >>= 5;
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
    fn csprng_pas_de_collision_sur_un_grand_tirage() {
        // 45 bits d'entropie : sur 2 000 tirages, la probabilité d'une
        // collision est ~5,7 × 10⁻⁸ — un doublon signale une régression du
        // tirage (retour à une graine d'horloge, biais…), pas un hasard.
        let mut vus = std::collections::HashSet::new();
        for _ in 0..2_000 {
            assert!(
                vus.insert(generate_invite(60, false).code),
                "collision : le tirage n'est plus cryptographique"
            );
        }
    }

    #[test]
    fn csprng_couvre_l_alphabet_sur_chaque_position() {
        // Chaque position du code doit voir passer une vraie diversité de
        // symboles. Sur 300 tirages uniformes parmi 32 symboles, observer
        // ≤ 8 symboles distincts à une position donnée est astronomiquement
        // improbable ; un tirage figé ou fortement biaisé échoue net.
        let mut par_position: Vec<std::collections::HashSet<u8>> = vec![Default::default(); 9];
        for _ in 0..300 {
            let code = generate_invite(60, false).code;
            for (position, octet) in code.bytes().filter(|o| *o != b'-').enumerate() {
                par_position[position].insert(octet);
            }
        }
        for (position, symboles) in par_position.iter().enumerate() {
            assert!(
                symboles.len() > 8,
                "position {position} : {} symboles distincts seulement",
                symboles.len()
            );
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
