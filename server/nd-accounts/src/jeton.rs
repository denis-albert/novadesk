//! Jetons applicatifs NovaDesk — JWS compact signé **Ed25519** (`alg:"EdDSA"`).
//!
//! Après authentification (mot de passe, 2FA ou OIDC), le service échange un
//! jeton de session opaque contre un **jeton applicatif signé**, vérifiable
//! **hors ligne** par les autres services (nd-api, lot 07) avec la seule clé
//! publique du service de comptes.
//!
//! ## Format du jeton (point de jonction avec nd-api)
//! JWS compact `en-tête.charge.signature` (parties en base64url sans
//! bourrage) :
//! - **en-tête** : `{"alg":"EdDSA","typ":"JWT"}` — tout autre `alg` (dont
//!   `none`) est refusé à la vérification ;
//! - **charge (claims)** :
//!   - `iss` : `"nd-accounts"` ([`EMETTEUR`]),
//!   - `sub` : e-mail du compte authentifié,
//!   - `roles` : tableau de rôles (aujourd'hui `["utilisateur"]`),
//!   - `plan` : plan de licence du compte (`"free"`, `"pro"`, `"entreprise"`),
//!   - `iat` / `exp` : émission et expiration, temps Unix en secondes
//!     (durée par défaut : [`DUREE_DEFAUT_S`]) ;
//! - **signature** : Ed25519 (64 octets) sur les octets ASCII de
//!   `"<en-tête b64url>.<charge b64url>"`.
//!
//! La clé de signature est **dérivée du secret serveur** (voir
//! [`crate::chiffre::deriver_cle`]) : stable d'un redémarrage à l'autre tant
//! que le secret ne change pas. nd-api récupère la clé publique (32 octets,
//! hexadécimal) via la requête réseau `ClePubliqueJetons` (voir `main.rs`) et
//! vérifie ensuite chaque jeton localement ([`verifier_jeton`] est
//! l'implémentation de référence de cette vérification).

use std::fmt;

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use crate::chiffre;
use crate::oidc::{decoder_base64url, encoder_base64url};
use crate::storage::{hex_vers_octets, octets_vers_hex};

/// Valeur du claim `iss` des jetons applicatifs.
pub const EMETTEUR: &str = "nd-accounts";
/// Durée de vie par défaut d'un jeton applicatif, en secondes.
pub const DUREE_DEFAUT_S: u64 = 3600;
/// Étiquette de dérivation de la graine Ed25519 depuis le secret serveur.
const CONTEXTE_GRAINE: &str = "nd-accounts/jetons-ed25519/v1";

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreurs de vérification d'un jeton applicatif.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurJeton {
    /// Jeton mal découpé, base64url ou JSON invalide.
    Malforme,
    /// `alg` différent d'`EdDSA` (y compris `none`, toujours refusé).
    AlgorithmeInattendu(String),
    /// La signature Ed25519 ne correspond pas au contenu.
    SignatureInvalide,
    /// Champ obligatoire absent ou de mauvais type dans la charge utile.
    ChampManquant(&'static str),
    /// `iss` différent de [`EMETTEUR`].
    EmetteurInattendu,
    /// Le jeton a expiré (`exp` ≤ maintenant).
    Expire,
    /// La clé publique fournie n'est pas une clé Ed25519 valide (32 octets
    /// hexadécimaux, point sur la courbe).
    ClePubliqueInvalide,
}

impl fmt::Display for ErreurJeton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurJeton::Malforme => write!(f, "jeton applicatif malformé"),
            ErreurJeton::AlgorithmeInattendu(alg) => {
                write!(f, "algorithme inattendu pour un jeton applicatif : {alg}")
            }
            ErreurJeton::SignatureInvalide => write!(f, "signature du jeton invalide"),
            ErreurJeton::ChampManquant(champ) => {
                write!(f, "champ obligatoire absent ou invalide : {champ}")
            }
            ErreurJeton::EmetteurInattendu => write!(f, "émetteur (iss) inattendu"),
            ErreurJeton::Expire => write!(f, "jeton applicatif expiré"),
            ErreurJeton::ClePubliqueInvalide => write!(f, "clé publique Ed25519 invalide"),
        }
    }
}

impl std::error::Error for ErreurJeton {}

// ---------------------------------------------------------------------------
// Claims
// ---------------------------------------------------------------------------

/// Claims d'un jeton applicatif vérifié.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimsApplicatifs {
    /// Émetteur (`iss`), toujours [`EMETTEUR`] après vérification.
    pub emetteur: String,
    /// Sujet (`sub`) : e-mail du compte authentifié.
    pub sujet: String,
    /// Rôles accordés (`roles`).
    pub roles: Vec<String>,
    /// Plan de licence (`plan`).
    pub plan: String,
    /// Émission (`iat`), temps Unix en secondes.
    pub emis_a: u64,
    /// Expiration (`exp`), temps Unix en secondes.
    pub expiration: u64,
}

// ---------------------------------------------------------------------------
// Émission
// ---------------------------------------------------------------------------

/// Émetteur de jetons applicatifs : clé Ed25519 dérivée du secret serveur
/// (même secret → même clé, les jetons survivent aux redémarrages).
pub struct EmetteurJetons {
    cle: SigningKey,
}

impl EmetteurJetons {
    /// Émetteur dont la clé est dérivée du secret serveur donné.
    #[must_use]
    pub fn depuis_secret(secret: &[u8]) -> Self {
        Self {
            cle: SigningKey::from_bytes(&chiffre::deriver_cle(secret, CONTEXTE_GRAINE)),
        }
    }

    /// Clé publique de vérification (32 octets, hexadécimal minuscule) —
    /// celle que nd-api utilise pour vérifier les jetons.
    #[must_use]
    pub fn cle_publique_hex(&self) -> String {
        octets_vers_hex(self.cle.verifying_key().as_bytes())
    }

    /// Émet un jeton applicatif signé pour le compte donné (voir le format
    /// documenté en tête de module).
    #[must_use]
    pub fn emettre(
        &self,
        sujet: &str,
        roles: &[&str],
        plan: &str,
        unix_now: u64,
        duree_s: u64,
    ) -> String {
        let en_tete = encoder_base64url(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let charge = serde_json::json!({
            "iss": EMETTEUR,
            "sub": sujet,
            "roles": roles,
            "plan": plan,
            "iat": unix_now,
            "exp": unix_now.saturating_add(duree_s),
        });
        let charge = encoder_base64url(charge.to_string().as_bytes());
        let message = format!("{en_tete}.{charge}");
        let signature = self.cle.sign(message.as_bytes());
        format!("{message}.{}", encoder_base64url(&signature.to_bytes()))
    }
}

// ---------------------------------------------------------------------------
// Vérification (implémentation de référence pour nd-api)
// ---------------------------------------------------------------------------

/// Vérifie un jeton applicatif à l'instant `unix_now` : **signature Ed25519
/// d'abord** (`verify_strict`), puis `alg == "EdDSA"`, `iss`, `exp`. C'est la
/// vérification que nd-api (lot 07) effectue avec la clé publique publiée.
///
/// # Errors
/// Voir [`ErreurJeton`] — forme du jeton, algorithme, signature, claims.
pub fn verifier_jeton(
    jeton: &str,
    cle_publique_hex: &str,
    unix_now: u64,
) -> Result<ClaimsApplicatifs, ErreurJeton> {
    let cle = hex_vers_octets(cle_publique_hex)
        .and_then(|octets| <[u8; 32]>::try_from(octets).ok())
        .ok_or(ErreurJeton::ClePubliqueInvalide)?;
    let cle = VerifyingKey::from_bytes(&cle).map_err(|_| ErreurJeton::ClePubliqueInvalide)?;

    let mut parties = jeton.split('.');
    let (Some(en_tete), Some(charge), Some(signature), None) = (
        parties.next(),
        parties.next(),
        parties.next(),
        parties.next(),
    ) else {
        return Err(ErreurJeton::Malforme);
    };
    if en_tete.is_empty() || charge.is_empty() || signature.is_empty() {
        return Err(ErreurJeton::Malforme);
    }

    // 1. Algorithme : EdDSA obligatoire (`none` et compagnie refusés).
    let en_tete_json: serde_json::Value = decoder_base64url(en_tete)
        .and_then(|octets| serde_json::from_slice(&octets).ok())
        .ok_or(ErreurJeton::Malforme)?;
    let algorithme = en_tete_json
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .ok_or(ErreurJeton::Malforme)?;
    if algorithme != "EdDSA" {
        return Err(ErreurJeton::AlgorithmeInattendu(algorithme.to_string()));
    }

    // 2. Signature — avant toute confiance dans les claims.
    let signature = decoder_base64url(signature)
        .and_then(|octets| Signature::from_slice(&octets).ok())
        .ok_or(ErreurJeton::Malforme)?;
    let message = format!("{en_tete}.{charge}");
    cle.verify_strict(message.as_bytes(), &signature)
        .map_err(|_| ErreurJeton::SignatureInvalide)?;

    // 3. Claims.
    let charge_json: serde_json::Value = decoder_base64url(charge)
        .and_then(|octets| serde_json::from_slice(&octets).ok())
        .ok_or(ErreurJeton::Malforme)?;
    let chaine = |champ: &'static str| -> Result<String, ErreurJeton> {
        charge_json
            .get(champ)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or(ErreurJeton::ChampManquant(champ))
    };
    let entier = |champ: &'static str| -> Result<u64, ErreurJeton> {
        charge_json
            .get(champ)
            .and_then(serde_json::Value::as_u64)
            .ok_or(ErreurJeton::ChampManquant(champ))
    };
    let claims = ClaimsApplicatifs {
        emetteur: chaine("iss")?,
        sujet: chaine("sub")?,
        roles: charge_json
            .get("roles")
            .and_then(serde_json::Value::as_array)
            .map(|roles| {
                roles
                    .iter()
                    .filter_map(|role| role.as_str().map(str::to_owned))
                    .collect()
            })
            .ok_or(ErreurJeton::ChampManquant("roles"))?,
        plan: chaine("plan")?,
        emis_a: entier("iat")?,
        expiration: entier("exp")?,
    };
    if claims.emetteur != EMETTEUR {
        return Err(ErreurJeton::EmetteurInattendu);
    }
    // RFC 7519 §4.1.4 : valable strictement avant `exp`.
    if unix_now >= claims.expiration {
        return Err(ErreurJeton::Expire);
    }
    Ok(claims)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MAINTENANT: u64 = 1_700_000_000;

    fn emetteur() -> EmetteurJetons {
        EmetteurJetons::depuis_secret(b"secret-serveur-de-test")
    }

    #[test]
    fn emission_puis_verification() {
        let emetteur = emetteur();
        let jeton = emetteur.emettre(
            "alice@example.com",
            &["utilisateur"],
            "pro",
            MAINTENANT,
            3600,
        );
        let claims = verifier_jeton(&jeton, &emetteur.cle_publique_hex(), MAINTENANT)
            .expect("jeton valide accepté");
        assert_eq!(claims.emetteur, EMETTEUR);
        assert_eq!(claims.sujet, "alice@example.com");
        assert_eq!(claims.roles, vec!["utilisateur"]);
        assert_eq!(claims.plan, "pro");
        assert_eq!(claims.emis_a, MAINTENANT);
        assert_eq!(claims.expiration, MAINTENANT + 3600);
    }

    #[test]
    fn cle_stable_pour_un_meme_secret() {
        // Même secret serveur : même clé publique (les jetons survivent aux
        // redémarrages) ; autre secret : autre clé.
        let a = EmetteurJetons::depuis_secret(b"secret-a");
        let b = EmetteurJetons::depuis_secret(b"secret-a");
        let c = EmetteurJetons::depuis_secret(b"secret-c");
        assert_eq!(a.cle_publique_hex(), b.cle_publique_hex());
        assert_ne!(a.cle_publique_hex(), c.cle_publique_hex());
        assert_eq!(a.cle_publique_hex().len(), 64, "32 octets en hexadécimal");

        // Un jeton émis par `a` se vérifie avec la clé de `b`… pas de `c`.
        let jeton = a.emettre("x@y", &["utilisateur"], "free", MAINTENANT, 60);
        assert!(verifier_jeton(&jeton, &b.cle_publique_hex(), MAINTENANT).is_ok());
        assert_eq!(
            verifier_jeton(&jeton, &c.cle_publique_hex(), MAINTENANT),
            Err(ErreurJeton::SignatureInvalide)
        );
    }

    #[test]
    fn jeton_expire_refuse() {
        let emetteur = emetteur();
        let jeton = emetteur.emettre("x@y", &["utilisateur"], "free", MAINTENANT, 300);
        let cle = emetteur.cle_publique_hex();
        // Valide une seconde avant `exp`, expiré à `exp` pile.
        assert!(verifier_jeton(&jeton, &cle, MAINTENANT + 299).is_ok());
        assert_eq!(
            verifier_jeton(&jeton, &cle, MAINTENANT + 300),
            Err(ErreurJeton::Expire)
        );
    }

    #[test]
    fn jeton_falsifie_refuse() {
        let emetteur = emetteur();
        let cle = emetteur.cle_publique_hex();
        let jeton = emetteur.emettre("x@y", &["utilisateur"], "free", MAINTENANT, 300);

        // Charge utile remplacée (élévation de `plan`) : signature invalide.
        let (en_tete, reste) = jeton.split_once('.').expect("en-tête");
        let (_charge, signature) = reste.rsplit_once('.').expect("signature");
        let falsifiee = encoder_base64url(
            serde_json::json!({
                "iss": EMETTEUR, "sub": "x@y", "roles": ["utilisateur"],
                "plan": "entreprise", "iat": MAINTENANT, "exp": MAINTENANT + 300,
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(
            verifier_jeton(
                &format!("{en_tete}.{falsifiee}.{signature}"),
                &cle,
                MAINTENANT
            ),
            Err(ErreurJeton::SignatureInvalide)
        );
    }

    #[test]
    fn algorithme_non_eddsa_refuse() {
        let emetteur = emetteur();
        let cle = emetteur.cle_publique_hex();
        let jeton = emetteur.emettre("x@y", &["utilisateur"], "free", MAINTENANT, 300);
        let (_, reste) = jeton.split_once('.').expect("en-tête");

        // `alg: none` (attaque classique) et tout autre algorithme : refus
        // avant même de regarder la signature.
        for alg in ["none", "HS256", "RS256"] {
            let en_tete = encoder_base64url(format!(r#"{{"alg":"{alg}"}}"#).as_bytes());
            assert_eq!(
                verifier_jeton(&format!("{en_tete}.{reste}"), &cle, MAINTENANT),
                Err(ErreurJeton::AlgorithmeInattendu(alg.to_string())),
                "alg : {alg}"
            );
        }
    }

    #[test]
    fn jeton_ou_cle_malformes_refuses() {
        let emetteur = emetteur();
        let cle = emetteur.cle_publique_hex();
        for jeton in ["", "a.b", "a.b.c.d", "..", "%%.a.b"] {
            assert_eq!(
                verifier_jeton(jeton, &cle, MAINTENANT),
                Err(ErreurJeton::Malforme),
                "jeton : {jeton:?}"
            );
        }
        // Clé publique inutilisable : hexadécimal invalide ou mauvaise taille.
        let jeton = emetteur.emettre("x@y", &["utilisateur"], "free", MAINTENANT, 300);
        for mauvaise in ["", "zz", "abcd"] {
            assert_eq!(
                verifier_jeton(&jeton, mauvaise, MAINTENANT),
                Err(ErreurJeton::ClePubliqueInvalide)
            );
        }
    }

    #[test]
    fn emetteur_inattendu_refuse() {
        // Un jeton signé par la bonne clé mais avec un `iss` étranger est
        // refusé (le champ est contrôlé après la signature).
        let emetteur = emetteur();
        let en_tete = encoder_base64url(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let charge = encoder_base64url(
            serde_json::json!({
                "iss": "autre-service", "sub": "x@y", "roles": ["utilisateur"],
                "plan": "free", "iat": MAINTENANT, "exp": MAINTENANT + 300,
            })
            .to_string()
            .as_bytes(),
        );
        let message = format!("{en_tete}.{charge}");
        let signature = encoder_base64url(&emetteur.cle.sign(message.as_bytes()).to_bytes());
        assert_eq!(
            verifier_jeton(
                &format!("{message}.{signature}"),
                &emetteur.cle_publique_hex(),
                MAINTENANT
            ),
            Err(ErreurJeton::EmetteurInattendu)
        );
    }
}
