//! Fédération d'identité OIDC / OAuth2 — flux **Authorization Code + PKCE**
//! côté client de confiance (plan 11), **complet et utilisable avec les
//! fournisseurs réels** (Google, Entra ID, Keycloak…).
//!
//! 1. [`generate_pkce`] tire un couple (vérificateur, défi) PKCE **S256**
//!    (RFC 7636) ;
//! 2. [`build_authorization_url`] construit l'URL d'autorisation à ouvrir dans
//!    le navigateur (`state` anti-CSRF, `nonce` anti-rejeu, défi PKCE) ;
//! 3. [`echanger_code`] échange le code d'autorisation contre les jetons au
//!    `token_endpoint` (POST HTTP(S) avec le vérificateur PKCE) ;
//! 4. [`validate_id_token`] vérifie l'ID token JWT : signature **d'abord** —
//!    **RS256** et **ES256** via les clés publiques **JWKS** du fournisseur
//!    (module [`crate::jwks`], sélection par `kid`, vecteurs RFC 7515
//!    vérifiés), **HS256** conservé pour les tests et le développement,
//!    `alg:none` toujours refusé — puis `iss`, `aud`, `exp` et `nonce`.
//!
//! [`FluxOidc`] orchestre le tout côté serveur : `demarrer()` crée une
//! transaction (state → vérificateur PKCE + nonce, expiration courte) et rend
//! l'URL d'autorisation ; `rappel(state, code)` consomme la transaction (usage
//! unique, anti-rejeu), échange le code, récupère/cache les JWKS
//! ([`crate::jwks::CacheJwks`], rafraîchissement forcé si le `kid` est inconnu
//! — rotation de clés) et valide l'ID token. Le rattachement du sujet fédéré
//! (`iss|sub`) à un compte local reste du ressort d'`AccountStore::link_oidc`
//! / `login_oidc` — voir `main.rs`, requêtes réseau `DemarrerOidc` /
//! `RappelOidc`.
//!
//! [`decoder_id_token_sans_signature`] reste réservé au débogage : ne jamais
//! faire confiance à des claims dont la signature n'a pas été vérifiée.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::jwks::{CacheJwks, Jwks};

type HmacSha256 = Hmac<Sha256>;

/// Taille de l'aléa du vérificateur PKCE, en octets. 32 octets → 43 caractères
/// base64url, dans la plage 43–128 imposée par la RFC 7636 §4.1.
const TAILLE_VERIFICATEUR: usize = 32;

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreurs de validation d'un ID token OIDC et du flux fédéré.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OidcError {
    /// Jeton mal découpé (≠ 3 parties non vides), base64url ou JSON invalide.
    JetonMalforme,
    /// Algorithme de signature non pris en charge (RS384, PS256…) ;
    /// `none` : toujours refusé (attaque classique).
    AlgorithmeNonSupporte(String),
    /// La signature (HS256, RS256 ou ES256) ne correspond pas au contenu.
    SignatureInvalide,
    /// Jeton HS256 sans clé partagée configurée, ou RS256/ES256 sans
    /// document JWKS fourni.
    CleManquante,
    /// Aucune clé du document JWKS ne correspond au `kid` (et à l'algorithme)
    /// de l'en-tête — même après rafraîchissement dans [`FluxOidc`].
    CleIntrouvable(String),
    /// Document JWKS illisible ou clé publique inutilisable.
    JwksInvalide(String),
    /// Échec réseau (récupération des JWKS, appel au token endpoint).
    Reseau(String),
    /// Le token endpoint a refusé l'échange code → jetons, ou sa réponse est
    /// inexploitable.
    EchangeCode(String),
    /// `state` inconnu, déjà consommé (anti-rejeu) ou expiré.
    TransactionInconnue,
    /// Champ obligatoire absent ou de mauvais type dans la charge utile.
    ChampManquant(&'static str),
    /// `iss` ne correspond pas à l'émetteur attendu.
    EmetteurInattendu,
    /// `aud` ne contient pas le client attendu.
    AudienceInattendue,
    /// Le jeton a expiré (`exp` ≤ maintenant).
    JetonExpire,
    /// `nonce` absent ou différent de celui envoyé dans l'URL d'autorisation.
    NonceInvalide,
}

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OidcError::JetonMalforme => write!(f, "ID token malformé"),
            OidcError::AlgorithmeNonSupporte(alg) => {
                write!(f, "algorithme de signature non pris en charge : {alg}")
            }
            OidcError::SignatureInvalide => write!(f, "signature de l'ID token invalide"),
            OidcError::CleManquante => {
                write!(f, "aucune clé configurée pour cet algorithme (HS256/JWKS)")
            }
            OidcError::CleIntrouvable(kid) => {
                write!(f, "aucune clé JWKS ne correspond au kid « {kid} »")
            }
            OidcError::JwksInvalide(message) => write!(f, "document JWKS invalide : {message}"),
            OidcError::Reseau(message) => write!(f, "échec réseau OIDC : {message}"),
            OidcError::EchangeCode(message) => {
                write!(f, "échange code → jetons refusé : {message}")
            }
            OidcError::TransactionInconnue => {
                write!(f, "transaction OIDC inconnue, expirée ou déjà utilisée")
            }
            OidcError::ChampManquant(champ) => {
                write!(f, "champ obligatoire absent ou invalide : {champ}")
            }
            OidcError::EmetteurInattendu => write!(f, "émetteur (iss) inattendu"),
            OidcError::AudienceInattendue => write!(f, "audience (aud) inattendue"),
            OidcError::JetonExpire => write!(f, "ID token expiré"),
            OidcError::NonceInvalide => write!(f, "nonce absent ou inattendu"),
        }
    }
}

impl std::error::Error for OidcError {}

// ---------------------------------------------------------------------------
// Configuration du fournisseur
// ---------------------------------------------------------------------------

/// Configuration d'un fournisseur d'identité OIDC (issue de sa découverte
/// `/.well-known/openid-configuration`, renseignée ici statiquement).
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Émetteur (`iss`) attendu dans les ID tokens, p. ex.
    /// `https://accounts.example.com`.
    pub issuer: String,
    /// URL du point d'autorisation (`authorization_endpoint`).
    pub authorization_endpoint: String,
    /// URL du point d'échange code → jetons (`token_endpoint`).
    pub token_endpoint: String,
    /// URL du document de clés publiques du fournisseur (`jwks_uri`).
    pub jwks_uri: String,
    /// Identifiant client de NovaDesk auprès du fournisseur (`client_id`,
    /// aussi l'audience `aud` attendue).
    pub client_id: String,
    /// URL de retour enregistrée auprès du fournisseur (`redirect_uri`).
    pub redirect_uri: String,
    /// Portées demandées ; vide → `openid` seul.
    pub scopes: Vec<String>,
}

// ---------------------------------------------------------------------------
// PKCE (RFC 7636, méthode S256)
// ---------------------------------------------------------------------------

/// Tire un couple PKCE `(vérificateur, défi)` : vérificateur de 32 octets
/// d'aléa système (43 caractères base64url), défi = `base64url(SHA-256(vérificateur))`.
///
/// Le vérificateur reste secret côté client jusqu'à l'échange du code au
/// `token_endpoint` ; seul le défi part dans l'URL d'autorisation.
#[must_use]
pub fn generate_pkce() -> (String, String) {
    let mut octets = [0u8; TAILLE_VERIFICATEUR];
    OsRng.fill_bytes(&mut octets);
    let verificateur = encoder_base64url(&octets);
    let defi = pkce_challenge(&verificateur);
    (verificateur, defi)
}

/// Défi S256 d'un vérificateur donné : `base64url(SHA-256(ascii(vérificateur)))`,
/// sans bourrage (RFC 7636 §4.2).
#[must_use]
pub fn pkce_challenge(verificateur: &str) -> String {
    encoder_base64url(&Sha256::digest(verificateur.as_bytes()))
}

// ---------------------------------------------------------------------------
// URL d'autorisation
// ---------------------------------------------------------------------------

/// Construit l'URL d'autorisation du flux Authorization Code + PKCE :
/// `response_type=code`, identifiants du client, portées, `state` (anti-CSRF),
/// `nonce` (anti-rejeu, revérifié dans l'ID token) et défi PKCE S256.
/// Tous les paramètres sont encodés en pourcentage (RFC 3986).
#[must_use]
pub fn build_authorization_url(
    config: &OidcConfig,
    state: &str,
    nonce: &str,
    challenge: &str,
) -> String {
    let scope = if config.scopes.is_empty() {
        "openid".to_string()
    } else {
        config.scopes.join(" ")
    };
    // Un point d'autorisation peut déjà porter des paramètres fixes.
    let separateur = if config.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!(
        "{}{separateur}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        config.authorization_endpoint,
        encoder_composant_url(&config.client_id),
        encoder_composant_url(&config.redirect_uri),
        encoder_composant_url(&scope),
        encoder_composant_url(state),
        encoder_composant_url(nonce),
        encoder_composant_url(challenge),
    )
}

/// Encodage en pourcentage d'un composant de requête : seuls les caractères
/// non réservés de la RFC 3986 §2.3 passent tels quels.
fn encoder_composant_url(composant: &str) -> String {
    let mut sortie = String::with_capacity(composant.len());
    for octet in composant.bytes() {
        match octet {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                sortie.push(octet as char);
            }
            _ => {
                let _ = write!(sortie, "%{octet:02X}");
            }
        }
    }
    sortie
}

// ---------------------------------------------------------------------------
// Validation d'ID token (JWT)
// ---------------------------------------------------------------------------

/// Claims extraits d'un ID token validé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdTokenClaims {
    /// Émetteur (`iss`).
    pub emetteur: String,
    /// Sujet (`sub`) : identifiant stable de l'utilisateur chez le
    /// fournisseur. À combiner avec l'émetteur (`iss|sub`) pour former une clé
    /// globalement unique avant `AccountStore::link_oidc`.
    pub sujet: String,
    /// Audiences (`aud`) : chaîne unique ou tableau dans le JWT, toujours une
    /// liste ici.
    pub audiences: Vec<String>,
    /// Expiration (`exp`), temps Unix en secondes.
    pub expiration: u64,
    /// Nonce (`nonce`) renvoyé par le fournisseur, s'il y en a un.
    pub nonce: Option<String>,
    /// E-mail (`email`), si la portée `email` a été accordée — c'est lui qu'on
    /// mappe vers un compte local.
    pub email: Option<String>,
}

/// Attentes de validation d'un ID token.
#[derive(Debug, Clone)]
pub struct ValidationOptions<'a> {
    /// Émetteur attendu (`iss`), cf. [`OidcConfig::issuer`].
    pub emetteur: &'a str,
    /// Audience attendue (`aud`) : le `client_id` de NovaDesk.
    pub audience: &'a str,
    /// Nonce attendu (celui de l'URL d'autorisation). `None` = pas de contrôle.
    pub nonce: Option<&'a str>,
    /// Clé partagée pour la signature HS256 (tests, développement). Sans clé,
    /// tout jeton HS256 est refusé ([`OidcError::CleManquante`]).
    pub cle_hs256: Option<&'a [u8]>,
    /// Clés publiques JWKS du fournisseur pour RS256/ES256. Sans document,
    /// tout jeton asymétrique est refusé ([`OidcError::CleManquante`]).
    pub jwks: Option<&'a Jwks>,
}

/// Valide un ID token JWT à l'instant `unix_now` (secondes Unix) :
/// **signature d'abord** (RS256/ES256 via les clés JWKS — sélection par `kid`
/// — ou HS256 via la clé partagée ; `alg:none` toujours refusé), puis `iss` ==
/// émetteur attendu, `aud` contient le client, `exp` strictement dans le
/// futur, et `nonce` égal à celui attendu s'il y en a un.
///
/// # Errors
/// Voir [`OidcError`] — chaque contrôle a son erreur propre, dans l'ordre :
/// forme du jeton, algorithme, clé, signature, claims obligatoires, `iss`,
/// `aud`, `exp`, `nonce`.
pub fn validate_id_token(
    jeton: &str,
    options: &ValidationOptions<'_>,
    unix_now: u64,
) -> Result<IdTokenClaims, OidcError> {
    let (en_tete_brut, charge_brute, signature_brute) = decouper_jwt(jeton)?;

    // 1. Signature — avant toute confiance dans les claims.
    let en_tete = json_base64url(en_tete_brut)?;
    let algorithme = en_tete
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .ok_or(OidcError::JetonMalforme)?;
    match algorithme {
        "HS256" => {
            let cle = options.cle_hs256.ok_or(OidcError::CleManquante)?;
            verifier_hs256(en_tete_brut, charge_brute, signature_brute, cle)?;
        }
        "RS256" | "ES256" => {
            let jwks = options.jwks.ok_or(OidcError::CleManquante)?;
            let kid = en_tete.get("kid").and_then(serde_json::Value::as_str);
            let cle = jwks.trouver(algorithme, kid).ok_or_else(|| {
                OidcError::CleIntrouvable(kid.unwrap_or("(sans kid)").to_string())
            })?;
            let signature = decoder_base64url(signature_brute).ok_or(OidcError::JetonMalforme)?;
            let message = format!("{en_tete_brut}.{charge_brute}");
            crate::jwks::verifier_signature(cle, message.as_bytes(), &signature)?;
        }
        // `none` et tout le reste : refus systématique.
        autre => return Err(OidcError::AlgorithmeNonSupporte(autre.to_string())),
    }

    // 2. Claims.
    let charge = json_base64url(charge_brute)?;
    let claims = extraire_claims(&charge)?;
    if claims.emetteur != options.emetteur {
        return Err(OidcError::EmetteurInattendu);
    }
    if !claims.audiences.iter().any(|a| a == options.audience) {
        return Err(OidcError::AudienceInattendue);
    }
    // RFC 7519 §4.1.4 : le jeton n'est valable que *strictement avant* `exp`.
    if unix_now >= claims.expiration {
        return Err(OidcError::JetonExpire);
    }
    if let Some(attendu) = options.nonce {
        if claims.nonce.as_deref() != Some(attendu) {
            return Err(OidcError::NonceInvalide);
        }
    }
    Ok(claims)
}

/// Décode l'en-tête et la charge utile d'un JWT **sans vérifier la signature**.
///
/// Réservé au débogage et au point d'extension RS256/ES256 : l'appelant doit
/// vérifier la signature (clés JWKS du fournisseur) **avant** de faire
/// confiance aux claims retournés.
///
/// # Errors
/// [`OidcError::JetonMalforme`] si le découpage, le base64url ou le JSON échoue.
pub fn decoder_id_token_sans_signature(
    jeton: &str,
) -> Result<(serde_json::Value, serde_json::Value), OidcError> {
    let (en_tete, charge, _signature) = decouper_jwt(jeton)?;
    Ok((json_base64url(en_tete)?, json_base64url(charge)?))
}

/// Découpe un JWT compact en ses trois parties non vides.
fn decouper_jwt(jeton: &str) -> Result<(&str, &str, &str), OidcError> {
    let mut parties = jeton.split('.');
    match (
        parties.next(),
        parties.next(),
        parties.next(),
        parties.next(),
    ) {
        (Some(en_tete), Some(charge), Some(signature), None)
            if !en_tete.is_empty() && !charge.is_empty() && !signature.is_empty() =>
        {
            Ok((en_tete, charge, signature))
        }
        _ => Err(OidcError::JetonMalforme),
    }
}

/// Décode une partie base64url d'un JWT en valeur JSON.
fn json_base64url(partie: &str) -> Result<serde_json::Value, OidcError> {
    let octets = decoder_base64url(partie).ok_or(OidcError::JetonMalforme)?;
    serde_json::from_slice(&octets).map_err(|_| OidcError::JetonMalforme)
}

/// Vérifie la signature HS256 : `HMAC-SHA256(clé, "<en-tête>.<charge>")`,
/// comparée en temps constant (`Mac::verify_slice`).
fn verifier_hs256(
    en_tete: &str,
    charge: &str,
    signature_b64: &str,
    cle: &[u8],
) -> Result<(), OidcError> {
    let signature = decoder_base64url(signature_b64).ok_or(OidcError::JetonMalforme)?;
    let mut mac =
        HmacSha256::new_from_slice(cle).expect("HMAC-SHA256 accepte une clé de toute longueur");
    mac.update(en_tete.as_bytes());
    mac.update(b".");
    mac.update(charge.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| OidcError::SignatureInvalide)
}

/// Extrait les claims obligatoires (`iss`, `sub`, `aud`, `exp`) et optionnels
/// (`nonce`, `email`) de la charge utile. `aud` accepte chaîne ou tableau.
fn extraire_claims(charge: &serde_json::Value) -> Result<IdTokenClaims, OidcError> {
    let chaine = |champ: &'static str| -> Result<String, OidcError> {
        charge
            .get(champ)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or(OidcError::ChampManquant(champ))
    };
    let emetteur = chaine("iss")?;
    let sujet = chaine("sub")?;
    let audiences = match charge.get("aud") {
        Some(serde_json::Value::String(unique)) => vec![unique.clone()],
        Some(serde_json::Value::Array(tableau)) => {
            let liste: Vec<String> = tableau
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if liste.is_empty() {
                return Err(OidcError::ChampManquant("aud"));
            }
            liste
        }
        _ => return Err(OidcError::ChampManquant("aud")),
    };
    let expiration = charge
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .ok_or(OidcError::ChampManquant("exp"))?;
    let nonce = charge
        .get("nonce")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let email = charge
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(IdTokenClaims {
        emetteur,
        sujet,
        audiences,
        expiration,
        nonce,
        email,
    })
}

// ---------------------------------------------------------------------------
// Échange code → jetons (token endpoint)
// ---------------------------------------------------------------------------

/// Jetons obtenus au token endpoint (RFC 6749 §4.1.4 + OIDC Core §3.1.3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JetonsRecus {
    /// ID token JWT — à valider avec [`validate_id_token`] avant tout usage.
    pub id_token: String,
    /// Jeton d'accès aux API du fournisseur (userinfo…), s'il y en a un.
    pub access_token: Option<String>,
    /// Durée de vie du jeton d'accès, en secondes, si annoncée.
    pub expires_in: Option<u64>,
}

/// Échange un code d'autorisation contre les jetons au `token_endpoint` du
/// fournisseur : POST `application/x-www-form-urlencoded` avec
/// `grant_type=authorization_code`, le code, la `redirect_uri`, le
/// `client_id` et le **vérificateur PKCE** (client public, RFC 7636 §4.5).
///
/// # Errors
/// [`OidcError::Reseau`] si l'appel échoue (connexion, délai),
/// [`OidcError::EchangeCode`] si le fournisseur refuse (HTTP 4xx/5xx) ou si
/// sa réponse ne contient pas d'`id_token`.
pub fn echanger_code(
    agent: &ureq::Agent,
    config: &OidcConfig,
    code: &str,
    verificateur: &str,
) -> Result<JetonsRecus, OidcError> {
    let reponse = agent.post(&config.token_endpoint).send_form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("client_id", config.client_id.as_str()),
        ("code_verifier", verificateur),
    ]);
    let corps = match reponse {
        Ok(reponse) => reponse
            .into_string()
            .map_err(|e| OidcError::Reseau(format!("lecture de la réponse : {e}")))?,
        Err(ureq::Error::Status(statut, reponse)) => {
            let corps = reponse.into_string().unwrap_or_default();
            return Err(OidcError::EchangeCode(format!("HTTP {statut} : {corps}")));
        }
        Err(e) => return Err(OidcError::Reseau(format!("appel du token endpoint : {e}"))),
    };
    let document: serde_json::Value = serde_json::from_str(&corps)
        .map_err(|_| OidcError::EchangeCode("réponse du token endpoint illisible".into()))?;
    let id_token = document
        .get("id_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| OidcError::EchangeCode("réponse sans id_token".into()))?
        .to_string();
    Ok(JetonsRecus {
        id_token,
        access_token: document
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        expires_in: document
            .get("expires_in")
            .and_then(serde_json::Value::as_u64),
    })
}

// ---------------------------------------------------------------------------
// Orchestration du flux côté serveur
// ---------------------------------------------------------------------------

/// Réglages du flux OIDC serveur (voir [`FluxOidc`]).
#[derive(Debug, Clone)]
pub struct OptionsFlux {
    /// Clé partagée HS256 (tests, développement) ; `None` en production.
    pub cle_hs256: Option<Vec<u8>>,
    /// Lier automatiquement un sujet fédéré inconnu au compte local portant
    /// l'e-mail (vérifié par le fournisseur) du claim `email`. À n'activer
    /// que pour un fournisseur de confiance ; sinon, seuls les sujets déjà
    /// liés par `AccountStore::link_oidc` peuvent se connecter.
    pub lier_par_email: bool,
    /// Durée de vie du cache des JWKS.
    pub duree_cache_jwks: Duration,
    /// Délai maximal des appels HTTP au fournisseur.
    pub delai_http: Duration,
    /// Durée de vie d'une transaction (state) en attente, en secondes.
    pub duree_transaction_s: u64,
}

impl Default for OptionsFlux {
    fn default() -> Self {
        Self {
            cle_hs256: None,
            lier_par_email: false,
            duree_cache_jwks: Duration::from_secs(300),
            delai_http: Duration::from_secs(10),
            duree_transaction_s: 600,
        }
    }
}

/// Transaction d'autorisation en attente (état entre `demarrer` et `rappel`).
struct TransactionOidc {
    verificateur: String,
    nonce: String,
    expire_a: u64,
}

/// Flux OIDC côté serveur : transactions en attente (state → PKCE + nonce),
/// échange code → jetons et validation des ID tokens avec cache des JWKS.
/// Thread-safe ; à partager derrière un `Arc` entre threads de connexion.
pub struct FluxOidc {
    config: OidcConfig,
    options: OptionsFlux,
    agent: ureq::Agent,
    cache_jwks: CacheJwks,
    en_attente: Mutex<HashMap<String, TransactionOidc>>,
}

impl FluxOidc {
    /// Prépare le flux pour un fournisseur donné (rien n'est contacté ici).
    #[must_use]
    pub fn new(config: OidcConfig, options: OptionsFlux) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(options.delai_http)
            .build();
        let cache_jwks = CacheJwks::new(config.jwks_uri.clone(), options.duree_cache_jwks);
        Self {
            config,
            options,
            agent,
            cache_jwks,
            en_attente: Mutex::new(HashMap::new()),
        }
    }

    /// L'auto-liaison par e-mail vérifié est-elle activée ? (voir
    /// [`OptionsFlux::lier_par_email`] ; consulté par le serveur réseau).
    #[must_use]
    pub fn lier_par_email(&self) -> bool {
        self.options.lier_par_email
    }

    /// Démarre une autorisation : tire `state`, `nonce` et le couple PKCE,
    /// mémorise la transaction (usage unique, expirant) et rend l'URL
    /// d'autorisation à ouvrir dans le navigateur, avec le `state` que le
    /// client représentera au rappel.
    pub fn demarrer(&self, unix_now: u64) -> (String, String) {
        let state = aleatoire_base64url();
        let nonce = aleatoire_base64url();
        let (verificateur, defi) = generate_pkce();
        let url = build_authorization_url(&self.config, &state, &nonce, &defi);
        let mut en_attente = self.en_attente.lock().unwrap();
        // Purge des transactions expirées au passage (pas de fuite mémoire).
        en_attente.retain(|_, transaction| transaction.expire_a > unix_now);
        en_attente.insert(
            state.clone(),
            TransactionOidc {
                verificateur,
                nonce,
                expire_a: unix_now.saturating_add(self.options.duree_transaction_s),
            },
        );
        (url, state)
    }

    /// Rappel (redirection du fournisseur) : consomme la transaction `state`
    /// — usage unique, anti-rejeu —, échange le code au token endpoint (avec
    /// le vérificateur PKCE) puis valide l'ID token (JWKS, `iss`, `aud`,
    /// `exp`, `nonce`). Renvoie les claims vérifiés ; le rattachement à un
    /// compte local est du ressort de l'appelant.
    ///
    /// # Errors
    /// [`OidcError::TransactionInconnue`] si `state` est inconnu, expiré ou
    /// déjà utilisé ; sinon les erreurs d'[`echanger_code`] et de
    /// [`validate_id_token`].
    pub fn rappel(
        &self,
        state: &str,
        code: &str,
        unix_now: u64,
    ) -> Result<IdTokenClaims, OidcError> {
        let transaction = self
            .en_attente
            .lock()
            .unwrap()
            .remove(state)
            .ok_or(OidcError::TransactionInconnue)?;
        if transaction.expire_a <= unix_now {
            return Err(OidcError::TransactionInconnue);
        }
        let jetons = echanger_code(&self.agent, &self.config, code, &transaction.verificateur)?;
        self.valider(&jetons.id_token, Some(&transaction.nonce), unix_now)
    }

    /// Valide un ID token avec les JWKS du fournisseur (cache) ; un `kid`
    /// inconnu déclenche **un** rafraîchissement forcé (rotation de clés)
    /// avant le verdict définitif.
    ///
    /// # Errors
    /// Voir [`validate_id_token`] et [`crate::jwks::CacheJwks::obtenir`].
    pub fn valider(
        &self,
        id_token: &str,
        nonce: Option<&str>,
        unix_now: u64,
    ) -> Result<IdTokenClaims, OidcError> {
        let jwks = self.cache_jwks.obtenir(&self.agent)?;
        let resultat =
            validate_id_token(id_token, &self.options_validation(nonce, &jwks), unix_now);
        match resultat {
            Err(OidcError::CleIntrouvable(_)) => {
                let jwks = self.cache_jwks.rafraichir(&self.agent)?;
                validate_id_token(id_token, &self.options_validation(nonce, &jwks), unix_now)
            }
            autre => autre,
        }
    }

    /// Attentes de validation pour ce fournisseur.
    fn options_validation<'a>(
        &'a self,
        nonce: Option<&'a str>,
        jwks: &'a Jwks,
    ) -> ValidationOptions<'a> {
        ValidationOptions {
            emetteur: &self.config.issuer,
            audience: &self.config.client_id,
            nonce,
            cle_hs256: self.options.cle_hs256.as_deref(),
            jwks: Some(jwks),
        }
    }
}

/// 32 octets d'aléa système en base64url (43 caractères) — `state`, `nonce`.
fn aleatoire_base64url() -> String {
    let mut octets = [0u8; 32];
    OsRng.fill_bytes(&mut octets);
    encoder_base64url(&octets)
}

// ---------------------------------------------------------------------------
// Base64url sans bourrage (RFC 4648 §5) — impl locale, aucune crate
// ---------------------------------------------------------------------------

/// Alphabet base64url (RFC 4648 §5) : `-` et `_` au lieu de `+` et `/`.
const ALPHABET_B64URL: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode en base64url **sans bourrage** (`=` interdit dans les JWT et PKCE).
#[must_use]
pub fn encoder_base64url(octets: &[u8]) -> String {
    let mut sortie = String::with_capacity(octets.len().div_ceil(3) * 4);
    for bloc in octets.chunks(3) {
        let n = (u32::from(bloc[0]) << 16)
            | (u32::from(*bloc.get(1).unwrap_or(&0)) << 8)
            | u32::from(*bloc.get(2).unwrap_or(&0));
        let sextets = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        // 1 octet → 2 caractères, 2 octets → 3, 3 octets → 4.
        let garder = bloc.len() + 1;
        for &s in &sextets[..garder] {
            sortie.push(ALPHABET_B64URL[s as usize] as char);
        }
    }
    sortie
}

/// Décode du base64url **sans bourrage** ; `None` si un caractère est hors
/// alphabet (y compris `=`) ou si la longueur est impossible (reste 1 mod 4).
#[must_use]
pub fn decoder_base64url(texte: &str) -> Option<Vec<u8>> {
    fn valeur(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let octets = texte.as_bytes();
    let mut sortie = Vec::with_capacity(octets.len() * 3 / 4);
    for bloc in octets.chunks(4) {
        let mut n: u32 = 0;
        for &c in bloc {
            n = (n << 6) | valeur(c)?;
        }
        match bloc.len() {
            4 => sortie.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            3 => sortie.extend_from_slice(&[(n >> 10) as u8, (n >> 2) as u8]),
            2 => sortie.push((n >> 4) as u8),
            _ => return None, // reste de 1 caractère : longueur impossible
        }
    }
    Some(sortie)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Clé partagée HS256 des jetons forgés.
    const CLE: &[u8] = b"cle-de-test-hs256-novadesk";

    /// Forge un JWT signé HS256 à partir d'une charge utile JSON.
    fn forger_jeton(charge: &serde_json::Value, cle: &[u8]) -> String {
        forger_jeton_alg(r#"{"alg":"HS256","typ":"JWT"}"#, charge, cle)
    }

    /// Forge un JWT avec un en-tête arbitraire (tests d'algorithme).
    fn forger_jeton_alg(en_tete: &str, charge: &serde_json::Value, cle: &[u8]) -> String {
        let en_tete_b64 = encoder_base64url(en_tete.as_bytes());
        let charge_b64 = encoder_base64url(charge.to_string().as_bytes());
        let mut mac = HmacSha256::new_from_slice(cle).expect("clé HMAC");
        mac.update(en_tete_b64.as_bytes());
        mac.update(b".");
        mac.update(charge_b64.as_bytes());
        let signature = encoder_base64url(&mac.finalize().into_bytes());
        format!("{en_tete_b64}.{charge_b64}.{signature}")
    }

    /// Charge utile valide de référence (expire à `maintenant + 300 s`).
    fn charge_valide(maintenant: u64) -> serde_json::Value {
        json!({
            "iss": "https://idp.example.com",
            "sub": "sub-42",
            "aud": "novadesk-client",
            "exp": maintenant + 300,
            "nonce": "n-1",
            "email": "alice@example.com",
        })
    }

    fn options() -> ValidationOptions<'static> {
        ValidationOptions {
            emetteur: "https://idp.example.com",
            audience: "novadesk-client",
            nonce: Some("n-1"),
            cle_hs256: Some(CLE),
            jwks: None,
        }
    }

    // -- Base64url ----------------------------------------------------------

    #[test]
    fn base64url_vecteurs_rfc_4648() {
        // Vecteurs de la RFC 4648 §10, sans bourrage.
        let vecteurs: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (octets, attendu) in vecteurs {
            assert_eq!(encoder_base64url(octets), attendu);
            assert_eq!(decoder_base64url(attendu).as_deref(), Some(octets));
        }
        // Alphabet URL : 0xFB 0xFF → `-` et `_`, pas `+` ni `/`.
        assert_eq!(encoder_base64url(&[0xFB, 0xFF]), "-_8");
        assert_eq!(decoder_base64url("-_8"), Some(vec![0xFB, 0xFF]));
    }

    #[test]
    fn base64url_aller_retour_binaire() {
        let tous: Vec<u8> = (0..=255).collect();
        assert_eq!(decoder_base64url(&encoder_base64url(&tous)), Some(tous));
    }

    #[test]
    fn base64url_invalide_refuse() {
        assert_eq!(decoder_base64url("Zg=="), None, "bourrage interdit");
        assert_eq!(decoder_base64url("a"), None, "reste de 1 caractère");
        assert_eq!(decoder_base64url("ab$d"), None, "caractère hors alphabet");
        assert_eq!(decoder_base64url("a+b/"), None, "alphabet standard refusé");
    }

    // -- PKCE ----------------------------------------------------------------

    #[test]
    fn pkce_vecteur_rfc_7636() {
        // Annexe B de la RFC 7636 : vérificateur et défi S256 officiels.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_defi_est_sha256_du_verificateur() {
        let (verificateur, defi) = generate_pkce();
        // 32 octets → 43 caractères base64url (plage RFC : 43–128).
        assert_eq!(verificateur.len(), 43);
        assert!(verificateur
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        // défi = base64url(SHA-256(vérificateur)), recalculé indépendamment.
        assert_eq!(
            defi,
            encoder_base64url(&Sha256::digest(verificateur.as_bytes()))
        );
        assert_eq!(defi, pkce_challenge(&verificateur));
    }

    #[test]
    fn pkce_couples_aleatoires() {
        let (v1, d1) = generate_pkce();
        let (v2, d2) = generate_pkce();
        assert_ne!(v1, v2, "deux vérificateurs consécutifs diffèrent");
        assert_ne!(d1, d2);
    }

    // -- URL d'autorisation ---------------------------------------------------

    #[test]
    fn url_autorisation_complete_et_encodee() {
        let config = OidcConfig {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: "https://idp.example.com/token".into(),
            jwks_uri: "https://idp.example.com/jwks".into(),
            client_id: "novadesk client".into(), // espace : à encoder
            redirect_uri: "https://app.novadesk.example/cb?src=oidc".into(),
            scopes: vec!["openid".into(), "email".into()],
        };
        let url = build_authorization_url(&config, "etat-1", "nonce-1", "defi-1");
        assert!(url.starts_with("https://idp.example.com/authorize?response_type=code&"));
        assert!(url.contains("client_id=novadesk%20client"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fapp.novadesk.example%2Fcb%3Fsrc%3Doidc"));
        assert!(url.contains("scope=openid%20email"));
        assert!(url.contains("state=etat-1"));
        assert!(url.contains("nonce=nonce-1"));
        assert!(url.contains("code_challenge=defi-1"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn url_autorisation_scope_par_defaut_et_endpoint_avec_parametres() {
        let config = OidcConfig {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize?tenant=nd".into(),
            token_endpoint: "https://idp.example.com/token".into(),
            jwks_uri: "https://idp.example.com/jwks".into(),
            client_id: "c".into(),
            redirect_uri: "https://cb".into(),
            scopes: vec![],
        };
        let url = build_authorization_url(&config, "s", "n", "d");
        assert!(url.contains("scope=openid"));
        // Le point d'autorisation portait déjà `?` : on enchaîne avec `&`.
        assert!(url.starts_with("https://idp.example.com/authorize?tenant=nd&response_type=code"));
    }

    // -- Validation d'ID token -------------------------------------------------

    #[test]
    fn id_token_valide_accepte() {
        let maintenant = 1_700_000_000;
        let jeton = forger_jeton(&charge_valide(maintenant), CLE);
        let claims =
            validate_id_token(&jeton, &options(), maintenant).expect("jeton valide accepté");
        assert_eq!(claims.emetteur, "https://idp.example.com");
        assert_eq!(claims.sujet, "sub-42");
        assert_eq!(claims.audiences, vec!["novadesk-client"]);
        assert_eq!(claims.expiration, maintenant + 300);
        assert_eq!(claims.nonce.as_deref(), Some("n-1"));
        assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn id_token_expire_refuse() {
        let maintenant = 1_700_000_000;
        let jeton = forger_jeton(&charge_valide(maintenant), CLE);
        // Toujours valide une seconde avant `exp`, expiré à `exp` pile.
        assert!(validate_id_token(&jeton, &options(), maintenant + 299).is_ok());
        assert_eq!(
            validate_id_token(&jeton, &options(), maintenant + 300),
            Err(OidcError::JetonExpire)
        );
        assert_eq!(
            validate_id_token(&jeton, &options(), maintenant + 10_000),
            Err(OidcError::JetonExpire)
        );
    }

    #[test]
    fn id_token_emetteur_ou_audience_inattendus() {
        let maintenant = 1_700_000_000;
        let mut charge = charge_valide(maintenant);
        charge["iss"] = json!("https://pirate.example.com");
        assert_eq!(
            validate_id_token(&forger_jeton(&charge, CLE), &options(), maintenant),
            Err(OidcError::EmetteurInattendu)
        );

        let mut charge = charge_valide(maintenant);
        charge["aud"] = json!("autre-client");
        assert_eq!(
            validate_id_token(&forger_jeton(&charge, CLE), &options(), maintenant),
            Err(OidcError::AudienceInattendue)
        );
    }

    #[test]
    fn id_token_audience_en_tableau_acceptee() {
        let maintenant = 1_700_000_000;
        let mut charge = charge_valide(maintenant);
        charge["aud"] = json!(["autre-client", "novadesk-client"]);
        let claims = validate_id_token(&forger_jeton(&charge, CLE), &options(), maintenant)
            .expect("audience présente dans le tableau");
        assert_eq!(claims.audiences.len(), 2);
    }

    #[test]
    fn id_token_nonce_incorrect_ou_absent_refuse() {
        let maintenant = 1_700_000_000;
        let mut charge = charge_valide(maintenant);
        charge["nonce"] = json!("autre-nonce");
        assert_eq!(
            validate_id_token(&forger_jeton(&charge, CLE), &options(), maintenant),
            Err(OidcError::NonceInvalide)
        );

        let mut charge = charge_valide(maintenant);
        charge.as_object_mut().expect("objet").remove("nonce");
        assert_eq!(
            validate_id_token(&forger_jeton(&charge, CLE), &options(), maintenant),
            Err(OidcError::NonceInvalide)
        );

        // Sans nonce attendu, le contrôle est désactivé.
        let mut sans_controle = options();
        sans_controle.nonce = None;
        assert!(validate_id_token(&forger_jeton(&charge, CLE), &sans_controle, maintenant).is_ok());
    }

    #[test]
    fn id_token_signature_invalide_refusee() {
        let maintenant = 1_700_000_000;
        // Signé avec une autre clé.
        let jeton = forger_jeton(&charge_valide(maintenant), b"mauvaise-cle");
        assert_eq!(
            validate_id_token(&jeton, &options(), maintenant),
            Err(OidcError::SignatureInvalide)
        );

        // Charge utile falsifiée après signature : le `sub` est remplacé.
        let jeton = forger_jeton(&charge_valide(maintenant), CLE);
        let (en_tete, _charge, signature) = {
            let mut parties = jeton.split('.');
            (
                parties.next().expect("en-tête").to_string(),
                parties.next().expect("charge").to_string(),
                parties.next().expect("signature").to_string(),
            )
        };
        let mut falsifiee = charge_valide(maintenant);
        falsifiee["sub"] = json!("sub-pirate");
        let charge_falsifiee = encoder_base64url(falsifiee.to_string().as_bytes());
        let trafique = format!("{en_tete}.{charge_falsifiee}.{signature}");
        assert_eq!(
            validate_id_token(&trafique, &options(), maintenant),
            Err(OidcError::SignatureInvalide)
        );
    }

    #[test]
    fn id_token_algorithmes_non_supportes_ou_sans_cle_refuses() {
        let maintenant = 1_700_000_000;
        let charge = charge_valide(maintenant);
        // RS256 est désormais pris en charge — mais sans document JWKS
        // configuré, un jeton asymétrique est refusé net (pas de repli HS256,
        // qui serait l'attaque par confusion d'algorithme classique).
        let jeton = forger_jeton_alg(r#"{"alg":"RS256","typ":"JWT"}"#, &charge, CLE);
        assert_eq!(
            validate_id_token(&jeton, &options(), maintenant),
            Err(OidcError::CleManquante)
        );
        // Algorithmes réellement non pris en charge : refus explicite.
        for alg in ["PS256", "RS384", "HS512"] {
            let jeton =
                forger_jeton_alg(&format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#), &charge, CLE);
            assert_eq!(
                validate_id_token(&jeton, &options(), maintenant),
                Err(OidcError::AlgorithmeNonSupporte(alg.into())),
                "alg : {alg}"
            );
        }
        // `alg: none` : toujours refusé (attaque classique).
        let jeton = forger_jeton_alg(r#"{"alg":"none","typ":"JWT"}"#, &charge, CLE);
        assert_eq!(
            validate_id_token(&jeton, &options(), maintenant),
            Err(OidcError::AlgorithmeNonSupporte("none".into()))
        );
    }

    #[test]
    fn id_token_hs256_sans_cle_refuse() {
        let maintenant = 1_700_000_000;
        let jeton = forger_jeton(&charge_valide(maintenant), CLE);
        let mut sans_cle = options();
        sans_cle.cle_hs256 = None;
        assert_eq!(
            validate_id_token(&jeton, &sans_cle, maintenant),
            Err(OidcError::CleManquante)
        );
    }

    #[test]
    fn id_token_malforme_refuse() {
        let maintenant = 1_700_000_000;
        for jeton in [
            "",
            "abc",
            "a.b",
            "a.b.c.d",
            "..",
            "%%%.a.b",                   // base64url invalide
            "eyJhbGciOiJIUzI1NiJ9..sig", // charge vide
        ] {
            assert_eq!(
                validate_id_token(jeton, &options(), maintenant),
                Err(OidcError::JetonMalforme),
                "jeton : {jeton:?}"
            );
        }
    }

    #[test]
    fn id_token_champs_obligatoires_manquants() {
        let maintenant = 1_700_000_000;
        for champ in ["iss", "sub", "aud", "exp"] {
            let mut charge = charge_valide(maintenant);
            charge.as_object_mut().expect("objet").remove(champ);
            assert_eq!(
                validate_id_token(&forger_jeton(&charge, CLE), &options(), maintenant),
                Err(OidcError::ChampManquant(champ)),
                "champ retiré : {champ}"
            );
        }
    }

    #[test]
    fn decodage_sans_signature_expose_les_parties() {
        let maintenant = 1_700_000_000;
        let jeton = forger_jeton(&charge_valide(maintenant), CLE);
        let (en_tete, charge) = decoder_id_token_sans_signature(&jeton).expect("jeton bien formé");
        assert_eq!(en_tete["alg"], "HS256");
        assert_eq!(charge["sub"], "sub-42");
        assert!(decoder_id_token_sans_signature("pas.un").is_err());
    }

    // -- Validation RS256 / ES256 via JWKS -------------------------------------

    use crate::jwks::test_idp;

    /// Attentes de validation adossées à un document JWKS.
    fn options_jwks(jwks: &Jwks) -> ValidationOptions<'_> {
        ValidationOptions {
            emetteur: "https://idp.example.com",
            audience: "novadesk-client",
            nonce: Some("n-1"),
            cle_hs256: None,
            jwks: Some(jwks),
        }
    }

    /// Document JWKS de test (clés RFC 7515 A.2 et A.3).
    fn jwks_test() -> Jwks {
        Jwks::depuis_json(&test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ))
        .expect("document JWKS de test")
    }

    #[test]
    fn id_token_rs256_valide_via_jwks() {
        let maintenant = 1_700_000_000;
        let jwks = jwks_test();
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), Some(test_idp::KID_RSA));
        let claims = validate_id_token(&jeton, &options_jwks(&jwks), maintenant)
            .expect("ID token RS256 valide accepté");
        assert_eq!(claims.sujet, "sub-42");
        assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
        // Les autres contrôles restent actifs après la signature asymétrique.
        assert_eq!(
            validate_id_token(&jeton, &options_jwks(&jwks), maintenant + 300),
            Err(OidcError::JetonExpire)
        );
    }

    #[test]
    fn id_token_es256_valide_via_jwks() {
        let maintenant = 1_700_000_000;
        let jwks = jwks_test();
        let jeton = test_idp::signer_es256(&charge_valide(maintenant), Some(test_idp::KID_P256));
        let claims = validate_id_token(&jeton, &options_jwks(&jwks), maintenant)
            .expect("ID token ES256 valide accepté");
        assert_eq!(claims.sujet, "sub-42");
        assert_eq!(claims.emetteur, "https://idp.example.com");
    }

    #[test]
    fn id_token_rs256_falsifie_refuse() {
        let maintenant = 1_700_000_000;
        let jwks = jwks_test();
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), Some(test_idp::KID_RSA));

        // Charge utile remplacée après signature : le `sub` est usurpé.
        let (en_tete, reste) = jeton.split_once('.').expect("en-tête");
        let (_charge, signature) = reste.rsplit_once('.').expect("signature");
        let mut falsifiee = charge_valide(maintenant);
        falsifiee["sub"] = json!("sub-pirate");
        let charge_falsifiee = encoder_base64url(falsifiee.to_string().as_bytes());
        assert_eq!(
            validate_id_token(
                &format!("{en_tete}.{charge_falsifiee}.{signature}"),
                &options_jwks(&jwks),
                maintenant
            ),
            Err(OidcError::SignatureInvalide)
        );

        // Même chose pour ES256 : un octet de signature altéré suffit.
        let jeton = test_idp::signer_es256(&charge_valide(maintenant), Some(test_idp::KID_P256));
        let (message, signature) = jeton.rsplit_once('.').expect("signature");
        let mut octets = decoder_base64url(signature).expect("base64url");
        octets[0] ^= 1;
        assert_eq!(
            validate_id_token(
                &format!("{message}.{}", encoder_base64url(&octets)),
                &options_jwks(&jwks),
                maintenant
            ),
            Err(OidcError::SignatureInvalide)
        );
    }

    #[test]
    fn id_token_kid_inconnu_refuse() {
        let maintenant = 1_700_000_000;
        let jwks = jwks_test();
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), Some("kid-fantome"));
        assert_eq!(
            validate_id_token(&jeton, &options_jwks(&jwks), maintenant),
            Err(OidcError::CleIntrouvable("kid-fantome".into()))
        );
    }

    #[test]
    fn id_token_sans_kid_selon_l_ambiguite() {
        let maintenant = 1_700_000_000;
        // Une seule clé RSA au document : un jeton sans `kid` est accepté.
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), None);
        let jwks = jwks_test();
        assert!(validate_id_token(&jeton, &options_jwks(&jwks), maintenant).is_ok());

        // Deux clés RSA candidates : choix ambigu, refus.
        let deux = serde_json::json!({
            "keys": [
                { "kty": "RSA", "kid": "a", "n": test_idp::RFC7515_A2_N, "e": test_idp::RFC7515_A2_E },
                { "kty": "RSA", "kid": "b", "n": test_idp::RFC7515_A2_N, "e": test_idp::RFC7515_A2_E },
            ]
        })
        .to_string();
        let jwks = Jwks::depuis_json(&deux).expect("document");
        assert_eq!(
            validate_id_token(&jeton, &options_jwks(&jwks), maintenant),
            Err(OidcError::CleIntrouvable("(sans kid)".into()))
        );
    }

    // -- Échange code → jetons et flux complet ---------------------------------

    /// Valeur d'un paramètre dans une URL ou un formulaire URL-encodé
    /// (les valeurs base64url de nos tests n'ont pas d'échappement).
    fn parametre(texte: &str, nom: &str) -> String {
        let marqueur = format!("&{nom}=");
        let debut = texte.find(&marqueur).expect("paramètre présent") + marqueur.len();
        texte[debut..]
            .split('&')
            .next()
            .expect("valeur du paramètre")
            .to_string()
    }

    /// Configuration pointant le fournisseur simulé.
    fn config_simulee(idp: &test_idp::FournisseurSimule) -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: idp.token_endpoint(),
            jwks_uri: idp.jwks_uri(),
            client_id: "novadesk-client".into(),
            redirect_uri: "http://127.0.0.1/rappel".into(),
            scopes: vec!["openid".into(), "email".into()],
        }
    }

    #[test]
    fn echange_code_contre_jetons() {
        let maintenant = 1_700_000_000;
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let id_token = test_idp::signer_rs256(&charge_valide(maintenant), Some(test_idp::KID_RSA));
        *idp.reponse_jetons.lock().unwrap() = json!({
            "id_token": id_token,
            "access_token": "jeton-acces-1",
            "token_type": "Bearer",
            "expires_in": 3600,
        })
        .to_string();

        let config = config_simulee(&idp);
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(5))
            .build();
        let jetons = echanger_code(&agent, &config, "code-42", "verificateur-pkce-de-test")
            .expect("échange réussi");
        assert_eq!(jetons.id_token, id_token);
        assert_eq!(jetons.access_token.as_deref(), Some("jeton-acces-1"));
        assert_eq!(jetons.expires_in, Some(3600));

        // Le POST contenait bien le code, le vérificateur PKCE et le client.
        let corps = idp
            .corps_recus
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("corps");
        assert!(corps.contains("grant_type=authorization_code"));
        assert_eq!(parametre(&corps, "code"), "code-42");
        assert_eq!(
            parametre(&corps, "code_verifier"),
            "verificateur-pkce-de-test"
        );
        assert_eq!(parametre(&corps, "client_id"), "novadesk-client");
    }

    #[test]
    fn echange_code_refus_et_reponses_inexploitables() {
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let config = config_simulee(&idp);
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(5))
            .build();

        // Refus HTTP 400 du fournisseur (code périmé, PKCE faux…).
        idp.echec_jetons
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let erreur = echanger_code(&agent, &config, "code", "verificateur").expect_err("refus");
        assert!(
            matches!(&erreur, OidcError::EchangeCode(m) if m.contains("400")),
            "erreur : {erreur:?}"
        );
        idp.echec_jetons
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Réponse 200 sans id_token : inexploitable.
        *idp.reponse_jetons.lock().unwrap() = r#"{"access_token":"seul"}"#.into();
        assert!(matches!(
            echanger_code(&agent, &config, "code", "verificateur"),
            Err(OidcError::EchangeCode(_))
        ));

        // Fournisseur injoignable : erreur réseau.
        let mut inaccessible = config.clone();
        inaccessible.token_endpoint = "http://127.0.0.1:1/token".into();
        assert!(matches!(
            echanger_code(&agent, &inaccessible, "code", "verificateur"),
            Err(OidcError::Reseau(_))
        ));
    }

    #[test]
    fn flux_complet_demarrage_echange_validation() {
        let maintenant = 1_700_000_000;
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let flux = FluxOidc::new(config_simulee(&idp), OptionsFlux::default());

        // Démarrage : l'URL porte state, nonce et défi PKCE S256.
        let (url, state) = flux.demarrer(maintenant);
        assert_eq!(parametre(&url, "state"), state);
        assert!(url.contains("code_challenge_method=S256"));
        let nonce = parametre(&url, "nonce");
        let defi = parametre(&url, "code_challenge");

        // Le fournisseur émettra un ID token RS256 portant ce nonce.
        let mut charge = charge_valide(maintenant);
        charge["nonce"] = json!(nonce);
        *idp.reponse_jetons.lock().unwrap() = json!({
            "id_token": test_idp::signer_rs256(&charge, Some(test_idp::KID_RSA)),
        })
        .to_string();

        // Rappel : échange + JWKS + validation → claims vérifiés.
        let claims = flux
            .rappel(&state, "code-autorisation-1", maintenant + 1)
            .expect("flux complet accepté");
        assert_eq!(claims.sujet, "sub-42");
        assert_eq!(claims.nonce.as_deref(), Some(nonce.as_str()));

        // PKCE de bout en bout : le vérificateur reçu par le fournisseur
        // correspond au défi S256 publié dans l'URL d'autorisation.
        let corps = idp
            .corps_recus
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("corps");
        let verificateur = parametre(&corps, "code_verifier");
        assert_eq!(pkce_challenge(&verificateur), defi);

        // Anti-rejeu : le même state ne sert qu'une fois.
        assert_eq!(
            flux.rappel(&state, "code-autorisation-1", maintenant + 2),
            Err(OidcError::TransactionInconnue)
        );
    }

    #[test]
    fn flux_rappel_state_inconnu_ou_expire_refuse() {
        let maintenant = 1_700_000_000;
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let flux = FluxOidc::new(config_simulee(&idp), OptionsFlux::default());

        // State jamais émis : refus sans le moindre appel réseau.
        assert_eq!(
            flux.rappel("state-fantome", "code", maintenant),
            Err(OidcError::TransactionInconnue)
        );
        assert!(idp.corps_recus.lock().unwrap().is_empty());

        // Transaction expirée : refusée elle aussi.
        let (_url, state) = flux.demarrer(maintenant);
        let apres = maintenant + OptionsFlux::default().duree_transaction_s;
        assert_eq!(
            flux.rappel(&state, "code", apres),
            Err(OidcError::TransactionInconnue)
        );
    }

    #[test]
    fn flux_nonce_falsifie_refuse() {
        let maintenant = 1_700_000_000;
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let flux = FluxOidc::new(config_simulee(&idp), OptionsFlux::default());
        let (_url, state) = flux.demarrer(maintenant);

        // ID token signé mais portant le nonce d'une **autre** transaction :
        // rejeu inter-transactions refusé.
        let mut charge = charge_valide(maintenant);
        charge["nonce"] = json!("nonce-d-une-autre-session");
        *idp.reponse_jetons.lock().unwrap() = json!({
            "id_token": test_idp::signer_rs256(&charge, Some(test_idp::KID_RSA)),
        })
        .to_string();
        assert_eq!(
            flux.rappel(&state, "code", maintenant),
            Err(OidcError::NonceInvalide)
        );
    }

    #[test]
    fn flux_rotation_de_cles_rafraichit_les_jwks() {
        let maintenant = 1_700_000_000;
        // Le fournisseur publie d'abord un document avec l'ancien kid.
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            "kid-ancien",
            test_idp::KID_P256,
        ));
        let flux = FluxOidc::new(config_simulee(&idp), OptionsFlux::default());

        // Premier jeton : réchauffe le cache avec l'ancien document.
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), Some("kid-ancien"));
        flux.valider(&jeton, Some("n-1"), maintenant)
            .expect("ancien kid accepté");
        assert_eq!(idp.acces_jwks.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Rotation chez le fournisseur : nouveau kid publié, jeton signé avec.
        *idp.reponse_jwks.lock().unwrap() =
            test_idp::document_jwks("kid-nouveau", test_idp::KID_P256);
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), Some("kid-nouveau"));
        // Le cache est périmé sans le savoir : kid introuvable → un
        // rafraîchissement forcé → validation réussie.
        flux.valider(&jeton, Some("n-1"), maintenant)
            .expect("rotation suivie après rafraîchissement");
        assert_eq!(idp.acces_jwks.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Un kid réellement inconnu reste refusé (après un rafraîchissement).
        let jeton = test_idp::signer_rs256(&charge_valide(maintenant), Some("kid-fantome"));
        assert_eq!(
            flux.valider(&jeton, Some("n-1"), maintenant),
            Err(OidcError::CleIntrouvable("kid-fantome".into()))
        );
    }

    #[test]
    fn flux_hs256_conserve_pour_le_developpement() {
        let maintenant = 1_700_000_000;
        let idp = test_idp::FournisseurSimule::demarrer(test_idp::document_jwks(
            test_idp::KID_RSA,
            test_idp::KID_P256,
        ));
        let options = OptionsFlux {
            cle_hs256: Some(CLE.to_vec()),
            ..OptionsFlux::default()
        };
        let flux = FluxOidc::new(config_simulee(&idp), options);
        // Un jeton HS256 signé avec la clé partagée passe par le même chemin.
        let jeton = forger_jeton(&charge_valide(maintenant), CLE);
        let claims = flux
            .valider(&jeton, Some("n-1"), maintenant)
            .expect("HS256 accepté en développement");
        assert_eq!(claims.sujet, "sub-42");
    }
}
