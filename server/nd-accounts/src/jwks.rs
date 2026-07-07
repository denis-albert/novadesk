//! Clés publiques JWKS (RFC 7517) et vérification **asymétrique** des ID
//! tokens OIDC : **RS256** (RSASSA-PKCS1-v1_5 / SHA-256) et **ES256**
//! (ECDSA P-256 / SHA-256), les deux algorithmes des fournisseurs réels
//! (Google, Entra ID, Keycloak…). Complète le HS256 de [`crate::oidc`],
//! conservé pour les tests et le développement.
//!
//! - [`Jwks::depuis_json`] lit le document JWKS du fournisseur (endpoint
//!   `jwks_uri` de sa découverte) : clés `RSA` (`n`, `e`) et `EC` P-256
//!   (`x`, `y`), sélection par `kid`. Les clés d'un autre type, d'une autre
//!   courbe ou marquées `"use":"enc"` sont ignorées (tolérance RFC 7517).
//! - [`verifier_signature`] vérifie la signature d'une entrée de signature
//!   JWS (`<en-tête b64url>.<charge b64url>`) avec une clé du document —
//!   crates **RustCrypto pur Rust** (`rsa`, `p256`), vecteurs officiels de la
//!   RFC 7515 (annexes A.2 et A.3) vérifiés dans les tests.
//! - [`CacheJwks`] mémorise le document JWKS avec expiration, pour ne pas
//!   interroger le fournisseur à chaque connexion ; un `kid` inconnu déclenche
//!   un rafraîchissement forcé (rotation de clés) via [`CacheJwks::rafraichir`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use p256::ecdsa::{Signature as SignatureP256, VerifyingKey as VerifyingKeyP256};
use rsa::pkcs1v15::{Signature as SignatureRsa, VerifyingKey as VerifyingKeyRsa};
use rsa::signature::Verifier as _;
use rsa::{BigUint, RsaPublicKey};
use sha2::Sha256;

use crate::oidc::{decoder_base64url, OidcError};

/// Taille d'une coordonnée de point P-256, en octets.
const TAILLE_COORDONNEE_P256: usize = 32;

// ---------------------------------------------------------------------------
// Clés publiques
// ---------------------------------------------------------------------------

/// Clé publique de vérification issue d'un document JWKS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleJwk {
    /// Clé RSA (`kty:"RSA"`) : module et exposant publics, gros-boutistes.
    Rsa {
        /// Identifiant de clé (`kid`), s'il y en a un.
        kid: Option<String>,
        /// Module `n` (octets gros-boutistes).
        n: Vec<u8>,
        /// Exposant public `e` (octets gros-boutistes, typiquement 65537).
        e: Vec<u8>,
    },
    /// Clé ECDSA P-256 (`kty:"EC"`, `crv:"P-256"`) : coordonnées affines.
    P256 {
        /// Identifiant de clé (`kid`), s'il y en a un.
        kid: Option<String>,
        /// Coordonnée `x` du point public (32 octets).
        x: [u8; TAILLE_COORDONNEE_P256],
        /// Coordonnée `y` du point public (32 octets).
        y: [u8; TAILLE_COORDONNEE_P256],
    },
}

impl CleJwk {
    /// Identifiant `kid` de la clé, s'il y en a un.
    #[must_use]
    pub fn kid(&self) -> Option<&str> {
        match self {
            CleJwk::Rsa { kid, .. } | CleJwk::P256 { kid, .. } => kid.as_deref(),
        }
    }

    /// La clé peut-elle vérifier l'algorithme JWS donné ?
    fn compatible(&self, algorithme: &str) -> bool {
        matches!(
            (self, algorithme),
            (CleJwk::Rsa { .. }, "RS256") | (CleJwk::P256 { .. }, "ES256")
        )
    }
}

/// Jeu de clés publiques d'un fournisseur (document JWKS, RFC 7517).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Jwks {
    cles: Vec<CleJwk>,
}

impl Jwks {
    /// Lit un document JWKS (`{"keys":[...]}`). Les clés inutilisables pour la
    /// signature (autre `kty`, autre courbe, `"use":"enc"`) sont ignorées ;
    /// une clé de signature **malformée** (base64url ou taille invalide) rend
    /// tout le document invalide — mieux vaut échouer que vérifier de travers.
    ///
    /// # Errors
    /// [`OidcError::JwksInvalide`] si le JSON est illisible, sans tableau
    /// `keys`, ou si une clé de signature est malformée.
    pub fn depuis_json(texte: &str) -> Result<Self, OidcError> {
        let document: serde_json::Value = serde_json::from_str(texte)
            .map_err(|e| OidcError::JwksInvalide(format!("JSON illisible : {e}")))?;
        let liste = document
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| OidcError::JwksInvalide("tableau `keys` absent".into()))?;

        let mut cles = Vec::new();
        for cle in liste {
            // Une clé de chiffrement (`use:"enc"`) ne vérifie pas de signature.
            let usage = cle.get("use").and_then(serde_json::Value::as_str);
            if usage.is_some_and(|u| u != "sig") {
                continue;
            }
            let kid = cle
                .get("kid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            match cle.get("kty").and_then(serde_json::Value::as_str) {
                Some("RSA") => cles.push(CleJwk::Rsa {
                    kid,
                    n: champ_base64url(cle, "n")?,
                    e: champ_base64url(cle, "e")?,
                }),
                Some("EC") => {
                    // Seule P-256 (ES256) est prise en charge ; les autres
                    // courbes sont ignorées, pas des erreurs.
                    if cle.get("crv").and_then(serde_json::Value::as_str) != Some("P-256") {
                        continue;
                    }
                    cles.push(CleJwk::P256 {
                        kid,
                        x: coordonnee_p256(cle, "x")?,
                        y: coordonnee_p256(cle, "y")?,
                    });
                }
                // `kty` inconnu (OKP…) : ignoré, comme le veut la RFC 7517.
                _ => continue,
            }
        }
        Ok(Self { cles })
    }

    /// Sélectionne la clé de vérification pour un algorithme et un `kid`.
    ///
    /// Sans `kid` dans l'en-tête du jeton, la clé n'est retenue que si le
    /// document n'en contient qu'**une seule** compatible (pratique répandue
    /// chez les fournisseurs à clé unique) — plusieurs candidates sans `kid`
    /// seraient un choix ambigu, donc un refus.
    #[must_use]
    pub fn trouver(&self, algorithme: &str, kid: Option<&str>) -> Option<&CleJwk> {
        match kid {
            Some(kid) => self
                .cles
                .iter()
                .find(|cle| cle.compatible(algorithme) && cle.kid() == Some(kid)),
            None => {
                let mut candidates = self.cles.iter().filter(|cle| cle.compatible(algorithme));
                let premiere = candidates.next()?;
                candidates.next().is_none().then_some(premiere)
            }
        }
    }

    /// Nombre de clés de signature utilisables du document.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cles.len()
    }

    /// Le document est-il vide de clés utilisables ?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cles.is_empty()
    }
}

/// Décode un champ base64url obligatoire d'une clé JWK.
fn champ_base64url(cle: &serde_json::Value, champ: &str) -> Result<Vec<u8>, OidcError> {
    cle.get(champ)
        .and_then(serde_json::Value::as_str)
        .and_then(decoder_base64url)
        .ok_or_else(|| OidcError::JwksInvalide(format!("champ `{champ}` absent ou invalide")))
}

/// Décode une coordonnée P-256 en exactement 32 octets : complétée à gauche
/// si l'encodage a omis des zéros de tête, débarrassée d'un zéro de tête
/// surnuméraire, refusée au-delà.
fn coordonnee_p256(
    cle: &serde_json::Value,
    champ: &str,
) -> Result<[u8; TAILLE_COORDONNEE_P256], OidcError> {
    let octets = champ_base64url(cle, champ)?;
    let mut coordonnee = [0u8; TAILLE_COORDONNEE_P256];
    match octets.len() {
        len if len <= TAILLE_COORDONNEE_P256 => {
            coordonnee[TAILLE_COORDONNEE_P256 - len..].copy_from_slice(&octets);
            Ok(coordonnee)
        }
        len if len == TAILLE_COORDONNEE_P256 + 1 && octets[0] == 0 => {
            coordonnee.copy_from_slice(&octets[1..]);
            Ok(coordonnee)
        }
        len => Err(OidcError::JwksInvalide(format!(
            "coordonnée `{champ}` de {len} octets (32 attendus)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Vérification de signature
// ---------------------------------------------------------------------------

/// Vérifie la signature d'une entrée de signature JWS (`message` =
/// `<en-tête b64url>.<charge b64url>` en ASCII) avec une clé JWKS :
/// RSASSA-PKCS1-v1_5/SHA-256 pour une clé RSA, ECDSA P-256/SHA-256 (signature
/// brute `r || s` de 64 octets, format JWS) pour une clé EC.
///
/// # Errors
/// [`OidcError::JwksInvalide`] si la clé publique elle-même est inutilisable
/// (module RSA invalide, point hors courbe), [`OidcError::SignatureInvalide`]
/// si la signature ne correspond pas au message.
pub fn verifier_signature(cle: &CleJwk, message: &[u8], signature: &[u8]) -> Result<(), OidcError> {
    match cle {
        CleJwk::Rsa { n, e, .. } => {
            let publique = RsaPublicKey::new(BigUint::from_bytes_be(n), BigUint::from_bytes_be(e))
                .map_err(|e| OidcError::JwksInvalide(format!("clé RSA inutilisable : {e}")))?;
            let signature =
                SignatureRsa::try_from(signature).map_err(|_| OidcError::SignatureInvalide)?;
            VerifyingKeyRsa::<Sha256>::new(publique)
                .verify(message, &signature)
                .map_err(|_| OidcError::SignatureInvalide)
        }
        CleJwk::P256 { x, y, .. } => {
            let point = p256::EncodedPoint::from_affine_coordinates(x.into(), y.into(), false);
            let publique = VerifyingKeyP256::from_encoded_point(&point)
                .map_err(|e| OidcError::JwksInvalide(format!("point P-256 inutilisable : {e}")))?;
            let signature =
                SignatureP256::from_slice(signature).map_err(|_| OidcError::SignatureInvalide)?;
            publique
                .verify(message, &signature)
                .map_err(|_| OidcError::SignatureInvalide)
        }
    }
}

// ---------------------------------------------------------------------------
// Cache des JWKS (récupération HTTP + expiration)
// ---------------------------------------------------------------------------

/// Cache du document JWKS d'un fournisseur : la première demande le récupère
/// par HTTP(S), les suivantes le réutilisent jusqu'à expiration. Thread-safe ;
/// [`Self::rafraichir`] force une récupération (rotation de clés : `kid`
/// inconnu du document en cache).
pub struct CacheJwks {
    url: String,
    duree_vie: Duration,
    etat: Mutex<Option<(Arc<Jwks>, Instant)>>,
}

impl CacheJwks {
    /// Cache pointant l'endpoint JWKS donné ; rien n'est récupéré ici.
    #[must_use]
    pub fn new(url: String, duree_vie: Duration) -> Self {
        Self {
            url,
            duree_vie,
            etat: Mutex::new(None),
        }
    }

    /// Document en cache s'il est encore frais, sinon récupération HTTP.
    ///
    /// # Errors
    /// [`OidcError::Reseau`] si la récupération échoue,
    /// [`OidcError::JwksInvalide`] si le document est illisible.
    pub fn obtenir(&self, agent: &ureq::Agent) -> Result<Arc<Jwks>, OidcError> {
        if let Some((jwks, expire_a)) = self.etat.lock().unwrap().as_ref() {
            if Instant::now() < *expire_a {
                return Ok(Arc::clone(jwks));
            }
        }
        self.rafraichir(agent)
    }

    /// Récupération forcée (le cache, frais ou non, est remplacé).
    ///
    /// # Errors
    /// Voir [`Self::obtenir`].
    pub fn rafraichir(&self, agent: &ureq::Agent) -> Result<Arc<Jwks>, OidcError> {
        let corps = agent
            .get(&self.url)
            .call()
            .map_err(|e| OidcError::Reseau(format!("récupération des JWKS : {e}")))?
            .into_string()
            .map_err(|e| OidcError::Reseau(format!("lecture des JWKS : {e}")))?;
        let jwks = Arc::new(Jwks::depuis_json(&corps)?);
        *self.etat.lock().unwrap() = Some((Arc::clone(&jwks), Instant::now() + self.duree_vie));
        Ok(jwks)
    }
}

// ---------------------------------------------------------------------------
// Fournisseur d'identité simulé (partagé par les tests du crate)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_idp {
    //! Fournisseur OIDC **simulé** pour les tests : clés de signature des
    //! vecteurs officiels de la RFC 7515 (annexe A.2 : RSA-2048 ; annexe
    //! A.3 : P-256) et petit serveur HTTP local (std pur) servant le document
    //! JWKS et le token endpoint (échange code → jetons).

    use std::io::{BufRead, BufReader, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rsa::signature::{SignatureEncoding as _, Signer as _};
    use rsa::traits::PrivateKeyParts as _;
    use sha2::Sha256;

    use super::*;
    use crate::oidc::encoder_base64url;

    // -- Vecteur RFC 7515, annexe A.2 (RSASSA-PKCS1-v1_5 / SHA-256) ----------

    /// Module `n` de la clé RSA-2048 de l'annexe A.2 (base64url).
    pub const RFC7515_A2_N: &str = "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ";
    /// Exposant public `e` (65537).
    pub const RFC7515_A2_E: &str = "AQAB";
    /// Exposant privé `d`.
    pub const RFC7515_A2_D: &str = "Eq5xpGnNCivDflJsRQBXHx1hdR1k6Ulwe2JZD50LpXyWPEAeP88vLNO97IjlA7_GQ5sLKMgvfTeXZx9SE-7YwVol2NXOoAJe46sui395IW_GO-pWJ1O0BkTGoVEn2bKVRUCgu-GjBVaYLU6f3l9kJfFNS3E0QbVdxzubSu3Mkqzjkn439X0M_V51gfpRLI9JYanrC4D4qAdGcopV_0ZHHzQlBjudU2QvXt4ehNYTCBr6XCLQUShb1juUO1ZdiYoFaFQT5Tw8bGUl_x_jTj3ccPDVZFD9pIuhLhBOneufuBiB4cS98l2SR_RQyGWSeWjnczT0QU91p1DhOVRuOopznQ";
    /// Premier facteur premier `p`.
    pub const RFC7515_A2_P: &str = "4BzEEOtIpmVdVEZNCqS7baC4crd0pqnRH_5IB3jw3bcxGn6QLvnEtfdUdiYrqBdss1l58BQ3KhooKeQTa9AB0Hw_Py5PJdTJNPY8cQn7ouZ2KKDcmnPGBY5t7yLc1QlQ5xHdwW1VhvKn-nXqhJTBgIPgtldC-KDV5z-y2XDwGUc";
    /// Second facteur premier `q`.
    pub const RFC7515_A2_Q: &str = "uQPEfgmVtjL0Uyyx88GZFF1fOunH3-7cepKmtH4pxhtCoHqpWmT8YAmZxaewHgHAjLYsp1ZSe7zFYHj7C6ul7TjeLQeZD_YwD66t62wDmpe_HlB-TnBA-njbglfIsRLtXlnDzQkv5dTltRJ11BKBBypeeF6689rjcJIDEz9RWdc";
    /// Entrée de signature de l'exemple A.2 : `<en-tête>.<charge>` (l'en-tête
    /// est `{"alg":"RS256"}`, la charge celle de l'annexe A.1).
    pub const RFC7515_A2_ENTREE: &str = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ";
    /// Signature attendue de l'exemple A.2 (base64url).
    pub const RFC7515_A2_SIGNATURE: &str = "cC4hiUPoj9Eetdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Qc_lF5YKt_O8W2Fp5jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb1L07Qe7K0GarZRmB_eSN9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6IeYI2w9QOYEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypGlUPQGe77Rw";

    // -- Vecteur RFC 7515, annexe A.3 (ECDSA P-256 / SHA-256) ----------------

    /// Coordonnée `x` de la clé P-256 de l'annexe A.3 (base64url).
    pub const RFC7515_A3_X: &str = "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU";
    /// Coordonnée `y`.
    pub const RFC7515_A3_Y: &str = "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0";
    /// Scalaire privé `d`.
    pub const RFC7515_A3_D: &str = "jpsQnnGQmL-YBIffH1136cspYG6-0iY7X1fCE9-E9LI";
    /// JWS compact complet de l'annexe A.3 (en-tête `{"alg":"ES256"}`).
    pub const RFC7515_A3_JWS: &str = "eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.DtEhU3ljbEg8L38VWAfUAqOyKAM6-Xx-F4GawxaepmXFCgfTjDxw5djxLa8ISlSApmWQxfKTUJqPP3-Kg6NU1Q";

    /// `kid` des clés publiées par le fournisseur simulé.
    pub const KID_RSA: &str = "cle-rsa-test";
    /// `kid` de la clé P-256 du fournisseur simulé.
    pub const KID_P256: &str = "cle-p256-test";

    fn b64(texte: &str) -> Vec<u8> {
        decoder_base64url(texte).expect("constante base64url des vecteurs RFC")
    }

    /// Clé privée RSA de l'annexe A.2, prête à signer en RS256.
    pub fn cle_privee_rs256() -> rsa::pkcs1v15::SigningKey<Sha256> {
        let privee = rsa::RsaPrivateKey::from_components(
            BigUint::from_bytes_be(&b64(RFC7515_A2_N)),
            BigUint::from_bytes_be(&b64(RFC7515_A2_E)),
            BigUint::from_bytes_be(&b64(RFC7515_A2_D)),
            vec![
                BigUint::from_bytes_be(&b64(RFC7515_A2_P)),
                BigUint::from_bytes_be(&b64(RFC7515_A2_Q)),
            ],
        )
        .expect("clé RSA de la RFC 7515 A.2 cohérente");
        assert_eq!(privee.primes().len(), 2, "deux facteurs premiers");
        rsa::pkcs1v15::SigningKey::<Sha256>::new(privee)
    }

    /// Clé privée P-256 de l'annexe A.3, prête à signer en ES256.
    pub fn cle_privee_es256() -> p256::ecdsa::SigningKey {
        p256::ecdsa::SigningKey::from_slice(&b64(RFC7515_A3_D))
            .expect("scalaire P-256 de la RFC 7515 A.3 valide")
    }

    /// Forge un JWT signé **RS256** (clé A.2) avec l'en-tête `kid` donné.
    pub fn signer_rs256(charge: &serde_json::Value, kid: Option<&str>) -> String {
        signer(charge, "RS256", kid, |message| {
            cle_privee_rs256().sign(message).to_vec()
        })
    }

    /// Forge un JWT signé **ES256** (clé A.3, signature brute `r || s`).
    pub fn signer_es256(charge: &serde_json::Value, kid: Option<&str>) -> String {
        signer(charge, "ES256", kid, |message| {
            let signature: p256::ecdsa::Signature = cle_privee_es256().sign(message);
            signature.to_bytes().to_vec()
        })
    }

    fn signer(
        charge: &serde_json::Value,
        algorithme: &str,
        kid: Option<&str>,
        signe: impl Fn(&[u8]) -> Vec<u8>,
    ) -> String {
        let en_tete = match kid {
            Some(kid) => format!(r#"{{"alg":"{algorithme}","typ":"JWT","kid":"{kid}"}}"#),
            None => format!(r#"{{"alg":"{algorithme}","typ":"JWT"}}"#),
        };
        let message = format!(
            "{}.{}",
            encoder_base64url(en_tete.as_bytes()),
            encoder_base64url(charge.to_string().as_bytes())
        );
        let signature = encoder_base64url(&signe(message.as_bytes()));
        format!("{message}.{signature}")
    }

    /// Document JWKS publiant les clés publiques A.2 (RSA) et A.3 (P-256)
    /// sous les `kid` donnés.
    pub fn document_jwks(kid_rsa: &str, kid_p256: &str) -> String {
        serde_json::json!({
            "keys": [
                { "kty": "RSA", "kid": kid_rsa, "use": "sig", "alg": "RS256",
                  "n": RFC7515_A2_N, "e": RFC7515_A2_E },
                { "kty": "EC", "kid": kid_p256, "use": "sig", "crv": "P-256",
                  "x": RFC7515_A3_X, "y": RFC7515_A3_Y },
            ]
        })
        .to_string()
    }

    // -- Serveur HTTP local ---------------------------------------------------

    /// Fournisseur simulé : sert `GET /jwks` et `POST /token` sur une adresse
    /// locale. Les réponses sont modifiables en cours de test (rotation de
    /// clés, jeton adapté au nonce) ; les corps reçus au token endpoint sont
    /// conservés pour vérifier l'échange PKCE.
    pub struct FournisseurSimule {
        adresse: SocketAddr,
        /// Corps servi par `GET /jwks`.
        pub reponse_jwks: Arc<Mutex<String>>,
        /// Corps servi par `POST /token` (JSON, doit contenir `id_token`).
        pub reponse_jetons: Arc<Mutex<String>>,
        /// Si vrai, `POST /token` répond `400 invalid_grant`.
        pub echec_jetons: Arc<AtomicBool>,
        /// Nombre de récupérations du document JWKS.
        pub acces_jwks: Arc<AtomicUsize>,
        /// Corps (formulaires URL-encodés) reçus au token endpoint.
        pub corps_recus: Arc<Mutex<Vec<String>>>,
    }

    impl FournisseurSimule {
        /// Démarre le fournisseur sur un port éphémère local.
        pub fn demarrer(jwks_initial: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind du fournisseur simulé");
            let adresse = listener.local_addr().expect("adresse locale");
            let fournisseur = Self {
                adresse,
                reponse_jwks: Arc::new(Mutex::new(jwks_initial)),
                reponse_jetons: Arc::new(Mutex::new(String::from("{}"))),
                echec_jetons: Arc::new(AtomicBool::new(false)),
                acces_jwks: Arc::new(AtomicUsize::new(0)),
                corps_recus: Arc::new(Mutex::new(Vec::new())),
            };
            let (jwks, jetons, echec, acces, corps_recus) = (
                Arc::clone(&fournisseur.reponse_jwks),
                Arc::clone(&fournisseur.reponse_jetons),
                Arc::clone(&fournisseur.echec_jetons),
                Arc::clone(&fournisseur.acces_jwks),
                Arc::clone(&fournisseur.corps_recus),
            );
            std::thread::spawn(move || {
                for flux in listener.incoming() {
                    let Ok(mut flux) = flux else { continue };
                    let Some((ligne, corps)) = lire_requete(&mut flux) else {
                        continue;
                    };
                    if ligne.starts_with("GET /jwks") {
                        acces.fetch_add(1, Ordering::SeqCst);
                        repondre(&mut flux, "200 OK", &jwks.lock().unwrap());
                    } else if ligne.starts_with("POST /token") {
                        corps_recus.lock().unwrap().push(corps.clone());
                        if echec.load(Ordering::SeqCst) {
                            repondre(&mut flux, "400 Bad Request", r#"{"error":"invalid_grant"}"#);
                        } else if corps.contains("grant_type=authorization_code")
                            && corps.contains("code_verifier=")
                        {
                            repondre(&mut flux, "200 OK", &jetons.lock().unwrap());
                        } else {
                            repondre(
                                &mut flux,
                                "400 Bad Request",
                                r#"{"error":"invalid_request"}"#,
                            );
                        }
                    } else {
                        repondre(&mut flux, "404 Not Found", r#"{"error":"not_found"}"#);
                    }
                }
            });
            fournisseur
        }

        /// URL du document JWKS.
        pub fn jwks_uri(&self) -> String {
            format!("http://{}/jwks", self.adresse)
        }

        /// URL du token endpoint (échange code → jetons).
        pub fn token_endpoint(&self) -> String {
            format!("http://{}/token", self.adresse)
        }
    }

    /// Lit une requête HTTP/1.1 : ligne de requête + en-têtes (dont
    /// `Content-Length`) + corps éventuel.
    fn lire_requete(flux: &mut TcpStream) -> Option<(String, String)> {
        let mut lecteur = BufReader::new(flux.try_clone().ok()?);
        let mut ligne_requete = String::new();
        lecteur.read_line(&mut ligne_requete).ok()?;
        let mut longueur = 0usize;
        loop {
            let mut ligne = String::new();
            if lecteur.read_line(&mut ligne).ok()? == 0 {
                break;
            }
            if ligne == "\r\n" || ligne == "\n" {
                break;
            }
            if let Some(valeur) = ligne.to_ascii_lowercase().strip_prefix("content-length:") {
                longueur = valeur.trim().parse().ok()?;
            }
        }
        let mut corps = vec![0u8; longueur];
        if longueur > 0 {
            lecteur.read_exact(&mut corps).ok()?;
        }
        Some((ligne_requete, String::from_utf8_lossy(&corps).into_owned()))
    }

    /// Écrit une réponse HTTP/1.1 JSON puis ferme la connexion.
    fn repondre(flux: &mut TcpStream, statut: &str, corps: &str) {
        let _ = write!(
            flux,
            "HTTP/1.1 {statut}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
            corps.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::test_idp::*;
    use super::*;
    use crate::oidc::encoder_base64url;
    use rsa::signature::{SignatureEncoding as _, Signer as _};

    /// Clé publique RSA de l'annexe A.2, au format [`CleJwk`].
    fn cle_publique_a2() -> CleJwk {
        CleJwk::Rsa {
            kid: None,
            n: decoder_base64url(RFC7515_A2_N).expect("n"),
            e: decoder_base64url(RFC7515_A2_E).expect("e"),
        }
    }

    /// Clé publique P-256 de l'annexe A.3, au format [`CleJwk`].
    fn cle_publique_a3() -> CleJwk {
        let x = decoder_base64url(RFC7515_A3_X).expect("x");
        let y = decoder_base64url(RFC7515_A3_Y).expect("y");
        CleJwk::P256 {
            kid: None,
            x: x.try_into().expect("32 octets"),
            y: y.try_into().expect("32 octets"),
        }
    }

    #[test]
    fn rs256_vecteur_officiel_rfc_7515_a2() {
        // Vérification de la signature exacte de l'annexe A.2 avec la clé
        // publique (n, e) : le vecteur officiel doit passer…
        let signature = decoder_base64url(RFC7515_A2_SIGNATURE).expect("signature A.2");
        verifier_signature(&cle_publique_a2(), RFC7515_A2_ENTREE.as_bytes(), &signature)
            .expect("vecteur RS256 de la RFC 7515 accepté");

        // … un message altéré doit échouer…
        let mut altere = RFC7515_A2_ENTREE.as_bytes().to_vec();
        *altere.last_mut().expect("entrée non vide") ^= 1;
        assert_eq!(
            verifier_signature(&cle_publique_a2(), &altere, &signature),
            Err(OidcError::SignatureInvalide)
        );

        // … et la signature RSASSA-PKCS1-v1_5 étant déterministe, signer la
        // même entrée avec la clé privée A.2 redonne exactement le vecteur.
        let recalculee = cle_privee_rs256()
            .sign(RFC7515_A2_ENTREE.as_bytes())
            .to_vec();
        assert_eq!(recalculee, signature, "signature déterministe identique");
    }

    #[test]
    fn es256_vecteur_officiel_rfc_7515_a3() {
        // Le JWS compact complet de l'annexe A.3 se vérifie avec (x, y).
        let (message, signature) = RFC7515_A3_JWS
            .rsplit_once('.')
            .expect("JWS compact à trois parties");
        let signature = decoder_base64url(signature).expect("signature A.3");
        assert_eq!(signature.len(), 64, "ES256 : signature brute r || s");
        verifier_signature(&cle_publique_a3(), message.as_bytes(), &signature)
            .expect("vecteur ES256 de la RFC 7515 accepté");

        // Message altéré : refus.
        let mut altere = message.as_bytes().to_vec();
        altere[0] ^= 1;
        assert_eq!(
            verifier_signature(&cle_publique_a3(), &altere, &signature),
            Err(OidcError::SignatureInvalide)
        );

        // Cohérence du vecteur : le scalaire privé `d` engendre bien (x, y).
        let derivee = cle_privee_es256().verifying_key().to_encoded_point(false);
        assert_eq!(derivee.x().expect("x").as_slice(), {
            let CleJwk::P256 { x, .. } = cle_publique_a3() else {
                unreachable!()
            };
            x.to_vec()
        });
    }

    #[test]
    fn jwks_analyse_et_selection_par_kid() {
        let jwks = Jwks::depuis_json(&document_jwks(KID_RSA, KID_P256)).expect("document valide");
        assert_eq!(jwks.len(), 2);
        assert!(!jwks.is_empty());

        // Sélection par kid et par algorithme compatible.
        assert!(matches!(
            jwks.trouver("RS256", Some(KID_RSA)),
            Some(CleJwk::Rsa { .. })
        ));
        assert!(matches!(
            jwks.trouver("ES256", Some(KID_P256)),
            Some(CleJwk::P256 { .. })
        ));
        // kid inconnu, ou kid connu mais algorithme incompatible : rien.
        assert!(jwks.trouver("RS256", Some("kid-inconnu")).is_none());
        assert!(jwks.trouver("RS256", Some(KID_P256)).is_none());
        // Sans kid : une seule clé compatible par algorithme ici, acceptée.
        assert!(jwks.trouver("RS256", None).is_some());
        assert!(jwks.trouver("ES256", None).is_some());
    }

    #[test]
    fn jwks_sans_kid_ambigu_refuse() {
        let deux_rsa = serde_json::json!({
            "keys": [
                { "kty": "RSA", "kid": "a", "n": RFC7515_A2_N, "e": RFC7515_A2_E },
                { "kty": "RSA", "kid": "b", "n": RFC7515_A2_N, "e": RFC7515_A2_E },
            ]
        })
        .to_string();
        let jwks = Jwks::depuis_json(&deux_rsa).expect("document valide");
        // Deux candidates et pas de kid dans l'en-tête : choix ambigu, refus.
        assert!(jwks.trouver("RS256", None).is_none());
        assert!(jwks.trouver("RS256", Some("b")).is_some());
    }

    #[test]
    fn jwks_tolerance_et_rejets() {
        // Clés ignorées : usage chiffrement, kty inconnu, courbe inconnue.
        let document = serde_json::json!({
            "keys": [
                { "kty": "RSA", "kid": "enc", "use": "enc", "n": RFC7515_A2_N, "e": RFC7515_A2_E },
                { "kty": "OKP", "kid": "okp", "crv": "Ed25519", "x": RFC7515_A3_X },
                { "kty": "EC", "kid": "p384", "crv": "P-384", "x": RFC7515_A3_X, "y": RFC7515_A3_Y },
                { "kty": "RSA", "kid": "sig", "use": "sig", "n": RFC7515_A2_N, "e": RFC7515_A2_E },
            ]
        })
        .to_string();
        let jwks = Jwks::depuis_json(&document).expect("document valide");
        assert_eq!(jwks.len(), 1, "seule la clé de signature RSA est retenue");

        // Documents invalides : JSON illisible, `keys` absent, champ malformé.
        assert!(matches!(
            Jwks::depuis_json("pas du json"),
            Err(OidcError::JwksInvalide(_))
        ));
        assert!(matches!(
            Jwks::depuis_json(r#"{"cles":[]}"#),
            Err(OidcError::JwksInvalide(_))
        ));
        assert!(matches!(
            Jwks::depuis_json(r#"{"keys":[{"kty":"RSA","n":"%%%","e":"AQAB"}]}"#),
            Err(OidcError::JwksInvalide(_))
        ));
        // Coordonnée EC trop longue : refus.
        let x_long = encoder_base64url(&[1u8; 34]);
        let document = format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","x":"{x_long}","y":"{RFC7515_A3_Y}"}}]}}"#
        );
        assert!(matches!(
            Jwks::depuis_json(&document),
            Err(OidcError::JwksInvalide(_))
        ));
        // Un document vide de clés reste lisible (le refus viendra du kid).
        assert!(Jwks::depuis_json(r#"{"keys":[]}"#)
            .expect("document vide lisible")
            .is_empty());
    }

    #[test]
    fn coordonnee_p256_courte_completee_a_gauche() {
        // Une coordonnée encodée sur 31 octets (zéro de tête omis) est
        // complétée à gauche — au niveau de l'analyse du document.
        let x_court = encoder_base64url(&[7u8; 31]);
        let document = format!(
            r#"{{"keys":[{{"kty":"EC","kid":"c","crv":"P-256","x":"{x_court}","y":"{RFC7515_A3_Y}"}}]}}"#
        );
        let jwks = Jwks::depuis_json(&document).expect("document valide");
        let Some(CleJwk::P256 { x, .. }) = jwks.trouver("ES256", Some("c")) else {
            panic!("clé P-256 attendue");
        };
        assert_eq!(x[0], 0, "complétée par un zéro de tête");
        assert_eq!(&x[1..], &[7u8; 31]);
    }

    #[test]
    fn cache_jwks_expiration_et_rafraichissement() {
        let idp = FournisseurSimule::demarrer(document_jwks(KID_RSA, KID_P256));
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(5))
            .build();

        // Cache frais : une seule récupération pour deux demandes.
        let cache = CacheJwks::new(idp.jwks_uri(), std::time::Duration::from_secs(300));
        let premier = cache.obtenir(&agent).expect("récupération");
        assert_eq!(premier.len(), 2);
        let second = cache.obtenir(&agent).expect("cache");
        assert_eq!(second.len(), 2);
        assert_eq!(idp.acces_jwks.load(Ordering::SeqCst), 1, "servi du cache");

        // Rafraîchissement forcé : nouvelle récupération, nouveau contenu.
        *idp.reponse_jwks.lock().unwrap() = document_jwks("kid-tourne", KID_P256);
        let rafraichi = cache.rafraichir(&agent).expect("rafraîchissement");
        assert!(rafraichi.trouver("RS256", Some("kid-tourne")).is_some());
        assert_eq!(idp.acces_jwks.load(Ordering::SeqCst), 2);

        // Durée de vie nulle : chaque demande repart au fournisseur.
        let cache = CacheJwks::new(idp.jwks_uri(), std::time::Duration::ZERO);
        cache.obtenir(&agent).expect("récupération 1");
        cache.obtenir(&agent).expect("récupération 2");
        assert_eq!(idp.acces_jwks.load(Ordering::SeqCst), 4);

        // Endpoint injoignable : erreur réseau propre.
        let injoignable = CacheJwks::new(
            "http://127.0.0.1:1/jwks".into(),
            std::time::Duration::from_secs(1),
        );
        assert!(matches!(
            injoignable.obtenir(&agent),
            Err(OidcError::Reseau(_))
        ));
    }
}
