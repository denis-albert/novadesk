//! `nd-crypto` — session chiffrée de bout en bout.
//!
//! Le handshake s'appuie sur le Noise Protocol Framework (crate `snow`) avec le motif
//! `Noise_XX_25519_ChaChaPoly_BLAKE2s` : les identités statiques X25519 des deux pairs
//! sont échangées (chiffrées) pendant le handshake, et les clés éphémères garantissent
//! la confidentialité persistante (PFS). La protection anti-MITM repose sur la
//! comparaison d'un SAS (short authentication string) dérivé des empreintes des clés
//! publiques statiques. Modèle de menace et détails :
//! `../../plan-technique/06-securite-chiffrement.md`.

use nd_proto::{NdError, Result};
use snow::params::{DHChoice, HashChoice, NoiseParams};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{Builder, HandshakeState, TransportState};

/// Motif Noise retenu pour la première connexion (plan 06) : XX échange les identités
/// statiques pendant le handshake, X25519 pour le Diffie-Hellman, ChaCha20-Poly1305
/// pour l'AEAD et BLAKE2s pour le hachage.
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Taille maximale d'un message Noise (spécification Noise §3).
const NOISE_MAX_MESSAGE_LEN: usize = 65_535;

/// Taille du tag d'authentification AEAD (ChaCha20-Poly1305).
const AEAD_TAG_LEN: usize = 16;

/// Construit les paramètres Noise à partir du motif constant.
fn noise_params() -> Result<NoiseParams> {
    NOISE_PATTERN
        .parse()
        .map_err(|e| NdError::Crypto(format!("motif Noise invalide : {e}")))
}

/// Convertit une erreur `snow` en [`NdError::Crypto`] avec un contexte lisible.
fn crypto_err(contexte: &str, err: snow::Error) -> NdError {
    NdError::Crypto(format!("{contexte} : {err}"))
}

/// Empreinte de la clé publique d'un pair (hash 32 octets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerFingerprint(pub [u8; 32]);

impl PeerFingerprint {
    /// Calcule l'empreinte d'une clé publique statique X25519 : BLAKE2s-256
    /// (32 octets), la même primitive de hachage que le motif Noise retenu.
    #[must_use]
    pub fn from_public_key(public_key: &[u8]) -> Self {
        let mut hasher = DefaultResolver
            .resolve_hash(&HashChoice::Blake2s)
            .expect("BLAKE2s est toujours disponible dans le resolver par défaut de snow");
        hasher.input(public_key);
        let mut out = [0u8; 32];
        hasher.result(&mut out);
        PeerFingerprint(out)
    }

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

/// Paire de clés statiques X25519 identifiant un pair.
///
/// Générée une seule fois par machine puis stockée localement ; la clé privée doit être
/// protégée au repos (DPAPI/keychain, voir plan 06 §gestion des clés).
pub struct StaticKeypair {
    /// Clé privée X25519 (32 octets) — ne jamais journaliser ni transmettre.
    pub private: Vec<u8>,
    /// Clé publique X25519 (32 octets) — identité cryptographique publiable du pair.
    pub public: Vec<u8>,
}

/// Génère une nouvelle paire de clés statiques X25519 via le CSPRNG de `snow`.
pub fn generate_static_keypair() -> Result<StaticKeypair> {
    let paire = Builder::new(noise_params()?)
        .generate_keypair()
        .map_err(|e| crypto_err("génération de la paire de clés statique", e))?;
    Ok(StaticKeypair {
        private: paire.private,
        public: paire.public,
    })
}

/// Dérive la clé publique X25519 correspondant à une clé privée donnée.
fn derive_public_key(private_key: &[u8]) -> Result<Vec<u8>> {
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or_else(|| NdError::Crypto("X25519 indisponible dans le resolver snow".into()))?;
    if private_key.len() != dh.priv_len() {
        return Err(NdError::Crypto(format!(
            "clé privée statique invalide : {} octets attendus, {} reçus",
            dh.priv_len(),
            private_key.len()
        )));
    }
    dh.set(private_key);
    Ok(dh.pubkey().to_vec())
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

/// Handshake Noise XX en cours, piloté par échange de messages.
///
/// L'appelant (couche transport/`nd-core`) fait circuler les messages produits par
/// [`write_message`](Self::write_message) et consommés par
/// [`read_message`](Self::read_message) jusqu'à ce que
/// [`is_finished`](Self::is_finished) soit vrai des deux côtés, puis convertit en
/// session de transport via [`into_session`](Self::into_session).
///
/// Séquence XX : `-> e` / `<- e, ee, s, es` / `-> s, se` (trois messages).
pub struct NoiseHandshake {
    etat: HandshakeState,
    empreinte_locale: PeerFingerprint,
}

impl NoiseHandshake {
    /// Prépare un handshake Noise XX dans le rôle donné, avec la clé privée statique
    /// X25519 locale (32 octets, voir [`generate_static_keypair`]).
    pub fn new(role: HandshakeRole, static_private_key: &[u8]) -> Result<Self> {
        // L'empreinte locale est dérivée dès maintenant : la clé publique est
        // recalculée à partir de la clé privée (X25519 : pub = priv * base).
        let cle_publique = derive_public_key(static_private_key)?;
        let empreinte_locale = PeerFingerprint::from_public_key(&cle_publique);

        let builder = Builder::new(noise_params()?).local_private_key(static_private_key);
        let etat = match role {
            HandshakeRole::Initiator => builder.build_initiator(),
            HandshakeRole::Responder => builder.build_responder(),
        }
        .map_err(|e| crypto_err("initialisation du handshake Noise", e))?;

        Ok(Self {
            etat,
            empreinte_locale,
        })
    }

    /// Produit le prochain message de handshake à envoyer au pair, en y embarquant
    /// `payload` (chiffré dès que le handshake dispose d'une clé).
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut tampon = vec![0u8; NOISE_MAX_MESSAGE_LEN];
        let n = self
            .etat
            .write_message(payload, &mut tampon)
            .map_err(|e| crypto_err("écriture d'un message de handshake", e))?;
        tampon.truncate(n);
        Ok(tampon)
    }

    /// Consomme un message de handshake reçu du pair et retourne le payload embarqué.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let mut tampon = vec![0u8; NOISE_MAX_MESSAGE_LEN];
        let n = self
            .etat
            .read_message(message, &mut tampon)
            .map_err(|e| crypto_err("lecture d'un message de handshake", e))?;
        tampon.truncate(n);
        Ok(tampon)
    }

    /// Indique si le handshake est terminé (tous les messages du motif échangés).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.etat.is_handshake_finished()
    }

    /// Convertit le handshake terminé en session de transport AEAD.
    ///
    /// Échoue si le handshake n'est pas terminé.
    pub fn into_session(self) -> Result<NoiseSession> {
        if !self.is_finished() {
            return Err(NdError::Crypto(
                "handshake Noise inachevé : impossible de passer en mode transport".into(),
            ));
        }
        // Avec le motif XX, la clé publique statique du pair est connue à la fin du
        // handshake ; son empreinte sert à la vérification SAS côté utilisateur.
        let empreinte_distante = self
            .etat
            .get_remote_static()
            .map(PeerFingerprint::from_public_key);
        let transport = self
            .etat
            .into_transport_mode()
            .map_err(|e| crypto_err("passage en mode transport", e))?;
        Ok(NoiseSession {
            transport,
            empreinte_locale: self.empreinte_locale,
            empreinte_distante,
        })
    }
}

/// Session Noise établie : chiffrement AEAD (ChaCha20-Poly1305) des charges
/// applicatives via le `TransportState` de `snow` (nonces gérés par la bibliothèque).
pub struct NoiseSession {
    transport: TransportState,
    empreinte_locale: PeerFingerprint,
    empreinte_distante: Option<PeerFingerprint>,
}

impl SecureSession for NoiseSession {
    fn local_fingerprint(&self) -> PeerFingerprint {
        self.empreinte_locale
    }

    fn remote_fingerprint(&self) -> Option<PeerFingerprint> {
        self.empreinte_distante
    }

    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        // Le découpage des charges > 64 Kio - 16 relève de la couche appelante
        // (framing, voir plan 05) ; ici on refuse proprement via l'erreur de snow.
        let mut tampon = vec![0u8; plaintext.len() + AEAD_TAG_LEN];
        let n = self
            .transport
            .write_message(plaintext, &mut tampon)
            .map_err(|e| crypto_err("chiffrement d'une charge applicative", e))?;
        tampon.truncate(n);
        Ok(tampon)
    }

    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut tampon = vec![0u8; ciphertext.len()];
        let n = self
            .transport
            .read_message(ciphertext, &mut tampon)
            .map_err(|e| crypto_err("déchiffrement d'une charge reçue", e))?;
        tampon.truncate(n);
        Ok(tampon)
    }
}

/// Démarre un handshake Noise XX dans le rôle donné avec la clé privée statique locale.
///
/// Façade au-dessus de [`NoiseHandshake::new`], point d'entrée stable pour `nd-core`
/// (voir plan 06/16).
pub fn start_handshake(role: HandshakeRole, static_private_key: &[u8]) -> Result<NoiseHandshake> {
    NoiseHandshake::new(role, static_private_key)
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

    /// Déroule le handshake XX complet en mémoire et retourne les deux sessions
    /// ainsi que les paires de clés statiques utilisées.
    fn etablit_sessions() -> (NoiseSession, NoiseSession, StaticKeypair, StaticKeypair) {
        let cles_init = generate_static_keypair().expect("génération clés initiateur");
        let cles_rep = generate_static_keypair().expect("génération clés répondeur");

        let mut initiateur =
            NoiseHandshake::new(HandshakeRole::Initiator, &cles_init.private).expect("initiateur");
        let mut repondeur =
            NoiseHandshake::new(HandshakeRole::Responder, &cles_rep.private).expect("répondeur");

        // Motif XX, trois messages : -> e | <- e, ee, s, es | -> s, se.
        let m1 = initiateur.write_message(&[]).expect("écriture m1");
        repondeur.read_message(&m1).expect("lecture m1");
        let m2 = repondeur.write_message(&[]).expect("écriture m2");
        initiateur.read_message(&m2).expect("lecture m2");
        let m3 = initiateur.write_message(&[]).expect("écriture m3");
        repondeur.read_message(&m3).expect("lecture m3");

        assert!(initiateur.is_finished(), "handshake initiateur terminé");
        assert!(repondeur.is_finished(), "handshake répondeur terminé");

        let session_init = initiateur.into_session().expect("session initiateur");
        let session_rep = repondeur.into_session().expect("session répondeur");
        (session_init, session_rep, cles_init, cles_rep)
    }

    #[test]
    fn handshake_xx_complet_chiffrement_bidirectionnel_et_empreintes() {
        let (mut session_init, mut session_rep, cles_init, cles_rep) = etablit_sessions();

        // Chiffrement initiateur -> répondeur.
        let clair_aller = b"NovaDesk : trame video chiffree de test";
        let chiffre_aller = session_init
            .encrypt(clair_aller)
            .expect("chiffrement aller");
        assert_ne!(
            &chiffre_aller[..clair_aller.len().min(chiffre_aller.len())],
            &clair_aller[..],
            "le ciphertext ne doit pas contenir le clair tel quel"
        );
        assert_eq!(
            session_rep
                .decrypt(&chiffre_aller)
                .expect("déchiffrement aller"),
            clair_aller
        );

        // Chiffrement répondeur -> initiateur.
        let clair_retour = b"NovaDesk : accuse de reception";
        let chiffre_retour = session_rep
            .encrypt(clair_retour)
            .expect("chiffrement retour");
        assert_eq!(
            session_init
                .decrypt(&chiffre_retour)
                .expect("déchiffrement retour"),
            clair_retour
        );

        // Empreintes : chacun voit l'empreinte de la clé publique statique de l'autre,
        // et sa propre empreinte correspond à sa propre clé publique.
        let fp_init = PeerFingerprint::from_public_key(&cles_init.public);
        let fp_rep = PeerFingerprint::from_public_key(&cles_rep.public);
        assert_eq!(session_init.local_fingerprint(), fp_init);
        assert_eq!(session_rep.local_fingerprint(), fp_rep);
        assert_eq!(session_init.remote_fingerprint(), Some(fp_rep));
        assert_eq!(session_rep.remote_fingerprint(), Some(fp_init));

        // SAS : ce que l'initiateur affiche localement est bien ce que le répondeur
        // voit pour lui (et réciproquement) — la comparaison de visu est cohérente.
        assert_eq!(
            session_init.local_fingerprint().sas(),
            session_rep
                .remote_fingerprint()
                .expect("empreinte distante")
                .sas()
        );
        assert_eq!(
            session_rep.local_fingerprint().sas(),
            session_init
                .remote_fingerprint()
                .expect("empreinte distante")
                .sas()
        );
    }

    #[test]
    fn dechiffrement_echoue_si_ciphertext_altere() {
        let (mut session_init, mut session_rep, _, _) = etablit_sessions();

        let mut chiffre = session_init
            .encrypt(b"charge sensible")
            .expect("chiffrement");
        // Altère un octet : le tag Poly1305 doit rejeter le message.
        chiffre[0] ^= 0xFF;
        assert!(matches!(
            session_rep.decrypt(&chiffre),
            Err(NdError::Crypto(_))
        ));
    }

    #[test]
    fn into_session_refuse_un_handshake_inacheve() {
        let cles = generate_static_keypair().expect("génération clés");
        let handshake =
            NoiseHandshake::new(HandshakeRole::Initiator, &cles.private).expect("initiateur");
        assert!(!handshake.is_finished());
        assert!(matches!(handshake.into_session(), Err(NdError::Crypto(_))));
    }

    #[test]
    fn new_refuse_une_cle_privee_de_mauvaise_taille() {
        assert!(matches!(
            NoiseHandshake::new(HandshakeRole::Initiator, &[0u8; 16]),
            Err(NdError::Crypto(_))
        ));
    }
}
