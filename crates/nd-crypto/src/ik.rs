//! Handshake Noise **IK** — accès non surveillé (plan 06).
//!
//! Contrairement au motif XX (première connexion : les identités statiques sont
//! échangées pendant le handshake), le motif IK suppose que l'initiateur connaît
//! DÉJÀ la clé publique statique du répondeur — typiquement épinglée lors d'une
//! première connexion XX (voir [`crate::pinning`]). C'est le mode « je me connecte
//! à MON PC par sa clé connue, sans interaction côté répondeur » : aucun SAS à
//! comparer, l'authentification du répondeur est structurelle au motif.
//!
//! Séquence IK (deux messages seulement, un aller-retour) :
//!
//! ```text
//! -> e, es, s, ss
//! <- e, ee, se
//! ```
//!
//! L'identité statique de l'initiateur est chiffrée dès le premier message vers la
//! clé statique du répondeur. Conséquence anti-MITM : si l'initiateur détient une
//! MAUVAISE clé publique (répondeur usurpé, clé changée), le répondeur légitime ne
//! peut pas authentifier le premier message et le handshake échoue immédiatement.

use nd_proto::{NdError, Result};
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::{crypto_err, derive_public_key, NoiseSession, PeerFingerprint, NOISE_MAX_MESSAGE_LEN};

/// Motif Noise pour l'accès non surveillé : IK (clé publique statique du répondeur
/// connue d'avance par l'initiateur), mêmes primitives que le motif XX du plan 06.
pub const NOISE_PATTERN_IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Taille d'une clé publique statique X25519 (octets).
const X25519_PUBLIC_LEN: usize = 32;

/// Construit les paramètres Noise du motif IK.
fn noise_params_ik() -> Result<NoiseParams> {
    NOISE_PATTERN_IK
        .parse()
        .map_err(|e| NdError::Crypto(format!("motif Noise IK invalide : {e}")))
}

/// Handshake Noise IK en cours, piloté par échange de messages.
///
/// Même contrat que [`crate::NoiseHandshake`] (motif XX) : l'appelant fait circuler
/// les messages produits par [`write_message`](Self::write_message) et consommés par
/// [`read_message`](Self::read_message) jusqu'à ce que
/// [`is_finished`](Self::is_finished) soit vrai des deux côtés, puis convertit en
/// session de transport via [`into_session`](Self::into_session) — la session
/// obtenue est une [`NoiseSession`] ordinaire (mêmes chiffrement et empreintes).
pub struct NoiseHandshakeIk {
    etat: HandshakeState,
    empreinte_locale: PeerFingerprint,
}

impl NoiseHandshakeIk {
    /// Prépare le handshake IK côté initiateur : `local_private` est la clé privée
    /// statique X25519 locale (32 octets), `remote_public` la clé publique statique
    /// X25519 du répondeur, connue d'avance (épinglée, voir [`crate::pinning`]).
    pub fn new_initiator(local_private: &[u8], remote_public: &[u8]) -> Result<Self> {
        if remote_public.len() != X25519_PUBLIC_LEN {
            return Err(NdError::Crypto(format!(
                "clé publique statique du répondeur invalide : {X25519_PUBLIC_LEN} octets attendus, {} reçus",
                remote_public.len()
            )));
        }
        let cle_publique = derive_public_key(local_private)?;
        let empreinte_locale = PeerFingerprint::from_public_key(&cle_publique);

        let etat = Builder::new(noise_params_ik()?)
            .local_private_key(local_private)
            .remote_public_key(remote_public)
            .build_initiator()
            .map_err(|e| crypto_err("initialisation du handshake Noise IK (initiateur)", e))?;

        Ok(Self {
            etat,
            empreinte_locale,
        })
    }

    /// Prépare le handshake IK côté répondeur avec sa clé privée statique X25519
    /// (32 octets). Le répondeur n'a rien à connaître d'avance : c'est SA clé
    /// publique que l'initiateur doit détenir.
    pub fn new_responder(local_private: &[u8]) -> Result<Self> {
        let cle_publique = derive_public_key(local_private)?;
        let empreinte_locale = PeerFingerprint::from_public_key(&cle_publique);

        let etat = Builder::new(noise_params_ik()?)
            .local_private_key(local_private)
            .build_responder()
            .map_err(|e| crypto_err("initialisation du handshake Noise IK (répondeur)", e))?;

        Ok(Self {
            etat,
            empreinte_locale,
        })
    }

    /// Produit le prochain message de handshake à envoyer au pair, en y embarquant
    /// `payload` (chiffré dès le premier message côté initiateur : IK dispose d'une
    /// clé dès `es`).
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut tampon = vec![0u8; NOISE_MAX_MESSAGE_LEN];
        let n = self
            .etat
            .write_message(payload, &mut tampon)
            .map_err(|e| crypto_err("écriture d'un message de handshake IK", e))?;
        tampon.truncate(n);
        Ok(tampon)
    }

    /// Consomme un message de handshake reçu du pair et retourne le payload embarqué.
    ///
    /// Côté répondeur, la lecture du PREMIER message échoue déjà si l'initiateur a
    /// utilisé une mauvaise clé publique du répondeur (authentification `es`/`ss`).
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let mut tampon = vec![0u8; NOISE_MAX_MESSAGE_LEN];
        let n = self
            .etat
            .read_message(message, &mut tampon)
            .map_err(|e| crypto_err("lecture d'un message de handshake IK", e))?;
        tampon.truncate(n);
        Ok(tampon)
    }

    /// Indique si le handshake est terminé (les deux messages du motif échangés).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.etat.is_handshake_finished()
    }

    /// Convertit le handshake terminé en session de transport AEAD ([`NoiseSession`]).
    ///
    /// Échoue si le handshake n'est pas terminé. L'empreinte distante est toujours
    /// disponible : côté initiateur c'est celle de la clé fournie à
    /// [`new_initiator`](Self::new_initiator), côté répondeur celle de la clé
    /// statique reçue (chiffrée) dans le premier message.
    pub fn into_session(self) -> Result<NoiseSession> {
        if !self.is_finished() {
            return Err(NdError::Crypto(
                "handshake Noise IK inachevé : impossible de passer en mode transport".into(),
            ));
        }
        let empreinte_distante = self
            .etat
            .get_remote_static()
            .map(PeerFingerprint::from_public_key);
        let transport = self
            .etat
            .into_transport_mode()
            .map_err(|e| crypto_err("passage en mode transport (IK)", e))?;
        Ok(NoiseSession {
            transport,
            empreinte_locale: self.empreinte_locale,
            empreinte_distante,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{generate_static_keypair, SecureSession, StaticKeypair};

    /// Déroule le handshake IK complet en mémoire (l'initiateur connaît la clé
    /// publique du répondeur) et retourne les deux sessions et les paires de clés.
    fn etablit_sessions_ik() -> (NoiseSession, NoiseSession, StaticKeypair, StaticKeypair) {
        let cles_init = generate_static_keypair().expect("génération clés initiateur");
        let cles_rep = generate_static_keypair().expect("génération clés répondeur");

        let mut initiateur = NoiseHandshakeIk::new_initiator(&cles_init.private, &cles_rep.public)
            .expect("initiateur IK");
        let mut repondeur =
            NoiseHandshakeIk::new_responder(&cles_rep.private).expect("répondeur IK");

        // Motif IK, deux messages : -> e, es, s, ss | <- e, ee, se.
        let m1 = initiateur.write_message(&[]).expect("écriture m1");
        repondeur.read_message(&m1).expect("lecture m1");
        let m2 = repondeur.write_message(&[]).expect("écriture m2");
        initiateur.read_message(&m2).expect("lecture m2");

        assert!(initiateur.is_finished(), "handshake initiateur terminé");
        assert!(repondeur.is_finished(), "handshake répondeur terminé");

        let session_init = initiateur.into_session().expect("session initiateur");
        let session_rep = repondeur.into_session().expect("session répondeur");
        (session_init, session_rep, cles_init, cles_rep)
    }

    #[test]
    fn handshake_ik_complet_chiffrement_bidirectionnel_et_empreintes() {
        let (mut session_init, mut session_rep, cles_init, cles_rep) = etablit_sessions_ik();

        // Chiffrement initiateur -> répondeur.
        let clair_aller = b"NovaDesk : acces non surveille, trame de test";
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
        let clair_retour = b"NovaDesk : accuse de reception IK";
        let chiffre_retour = session_rep
            .encrypt(clair_retour)
            .expect("chiffrement retour");
        assert_eq!(
            session_init
                .decrypt(&chiffre_retour)
                .expect("déchiffrement retour"),
            clair_retour
        );

        // Empreintes cohérentes : chaque pair voit bien l'empreinte de la clé
        // publique statique de l'autre ; côté initiateur, l'empreinte distante est
        // exactement celle de la clé connue d'avance.
        let fp_init = PeerFingerprint::from_public_key(&cles_init.public);
        let fp_rep = PeerFingerprint::from_public_key(&cles_rep.public);
        assert_eq!(session_init.local_fingerprint(), fp_init);
        assert_eq!(session_rep.local_fingerprint(), fp_rep);
        assert_eq!(session_init.remote_fingerprint(), Some(fp_rep));
        assert_eq!(session_rep.remote_fingerprint(), Some(fp_init));
    }

    #[test]
    fn handshake_ik_echoue_avec_une_mauvaise_cle_publique_du_repondeur() {
        let cles_init = generate_static_keypair().expect("génération clés initiateur");
        let cles_rep = generate_static_keypair().expect("génération clés répondeur");
        // L'initiateur croit se connecter au répondeur mais détient la clé publique
        // d'une AUTRE machine (usurpation, clé changée...).
        let mauvaise_cle = generate_static_keypair().expect("génération mauvaise clé");

        let mut initiateur =
            NoiseHandshakeIk::new_initiator(&cles_init.private, &mauvaise_cle.public)
                .expect("initiateur IK");
        let mut repondeur =
            NoiseHandshakeIk::new_responder(&cles_rep.private).expect("répondeur IK");

        // L'initiateur produit son premier message sans erreur (il ne peut pas
        // savoir localement que la clé est mauvaise)...
        let m1 = initiateur.write_message(&[]).expect("écriture m1");
        // ... mais le répondeur légitime ne peut pas l'authentifier : échec immédiat.
        assert!(matches!(
            repondeur.read_message(&m1),
            Err(NdError::Crypto(_))
        ));
    }

    #[test]
    fn new_initiator_refuse_des_cles_de_mauvaise_taille() {
        let cles = generate_static_keypair().expect("génération clés");
        // Clé publique distante trop courte.
        assert!(matches!(
            NoiseHandshakeIk::new_initiator(&cles.private, &[0u8; 16]),
            Err(NdError::Crypto(_))
        ));
        // Clé privée locale trop courte.
        assert!(matches!(
            NoiseHandshakeIk::new_initiator(&[0u8; 16], &cles.public),
            Err(NdError::Crypto(_))
        ));
    }

    #[test]
    fn new_responder_refuse_une_cle_privee_de_mauvaise_taille() {
        assert!(matches!(
            NoiseHandshakeIk::new_responder(&[0u8; 16]),
            Err(NdError::Crypto(_))
        ));
    }

    #[test]
    fn into_session_refuse_un_handshake_ik_inacheve() {
        let cles_init = generate_static_keypair().expect("génération clés initiateur");
        let cles_rep = generate_static_keypair().expect("génération clés répondeur");
        let handshake = NoiseHandshakeIk::new_initiator(&cles_init.private, &cles_rep.public)
            .expect("initiateur IK");
        assert!(!handshake.is_finished());
        assert!(matches!(handshake.into_session(), Err(NdError::Crypto(_))));
    }
}
