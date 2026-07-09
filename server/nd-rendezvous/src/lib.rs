//! Façade d'authentification du serveur de rendez-vous NovaDesk (plan 11).
//!
//! Le moteur de signalisation (registre, présence, candidats de punch) vit
//! dans `nd-signaling` et n'est **pas modifié** : cette bibliothèque place
//! devant lui une façade qui applique la **preuve de possession d'ID** avant
//! tout enregistrement — n'importe qui ne peut plus `Register` n'importe quel
//! ID (anti-squatting).
//!
//! # Fonctionnement
//!
//! [`servir_authentifie`] démarre le moteur `nd_signaling::serve` sur une
//! écoute de **boucle locale interne**, puis accepte les clients sur l'écoute
//! publique. Chaque trame reçue (même tramage `u32` BE que `nd-signaling`)
//! est filtrée par son octet de tag :
//!
//! - **`Register` nu (tag 1) : refusé** systématiquement (réponse `NotFound`) ;
//! - **`RegisterAuthentifie` (tag [`TAG_REGISTER_AUTHENTIFIE`]) : vérifié**
//!   puis, si valide, traduit en `Register` interne transmis au moteur ;
//! - autres tags (`Lookup`, `Heartbeat`, candidats, punch) : transmis tels
//!   quels au moteur (mêmes réponses).
//!
//! # Trame `RegisterAuthentifie` (tag 8)
//!
//! ```text
//! [8][id u64 BE][addr u32 BE + UTF-8][cert u32 BE + octets][horodatage u64 BE]
//!    [jeton u32 BE + octets][signature 64 octets]
//! ```
//!
//! - `jeton` : [`JetonEnregistrement`] émis par le service d'attribution d'ID
//!   (`nd-api`), signé par l'**autorité** du déploiement — il lie `id` à la
//!   clé statique du client ;
//! - `signature` : signature Ed25519 **du client** sur le message canonique
//!   [`message_enregistrement`] `(contexte, id, addr, cert, horodatage)` —
//!   elle prouve la **possession** de la clé liée (un jeton observé sur le
//!   réseau ne suffit pas) et scelle l'adresse et le certificat publiés ;
//! - `horodatage` (secondes UNIX) : borne le rejeu à la fenêtre
//!   [`ConfigRendezvous::tolerance_horodatage`].
//!
//! # Coordination (lot 05 — client `nd-signaling`)
//!
//! Le client `nd_signaling::RendezvousClient::register` émet encore le tag 1,
//! désormais refusé par ce serveur : le lot 05 doit lui apprendre la trame
//! authentifiée ci-dessus (constructeur : [`trame_register_authentifie`] ;
//! client de référence : [`enregistrer_authentifie`]). Les messages
//! `Heartbeat`/`PublishCandidates`/`Punch`/`PollPunch` restent transmis sans
//! authentification (voir les limites documentées sur [`servir_authentifie`]).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use nd_api::auth::{maintenant_unix, JetonEnregistrement, Signature, SigningKey, VerifyingKey};
use nd_api::auth::{Signer, LG_SIGNATURE};
use nd_proto::NovaId;
use nd_signaling::Registry;

/// Tag de la trame d'enregistrement authentifié (extension de cette façade ;
/// les tags 1..=7 appartiennent au protocole `nd-signaling`).
pub const TAG_REGISTER_AUTHENTIFIE: u8 = 8;

/// Tag du `Register` nu de `nd-signaling`, désormais refusé.
const TAG_REGISTER_NU: u8 = 1;

/// Réponse `NotFound` du protocole `nd-signaling` (sert de refus générique).
const REPONSE_REFUS: [u8; 1] = [2];

/// Taille maximale d'une trame acceptée (identique à `nd-signaling`).
const TRAME_MAX: usize = 1 << 20;

/// Contexte de domaine de la signature d'enregistrement (preuve de possession).
const CONTEXTE_ENREGISTREMENT: &[u8] = b"novadesk-rendezvous-enregistrement-v1";

/// Tolérance par défaut sur l'horodatage d'un enregistrement (anti-rejeu).
pub const TOLERANCE_HORODATAGE_DEFAUT: Duration = Duration::from_secs(300);

/// Délai d'E/S par connexion cliente (une trame, une réponse) : une connexion
/// muette ou au goutte-à-goutte ne retient ni thread ni socket indéfiniment.
const DELAI_ECHANGE: Duration = Duration::from_secs(30);

/// Configuration de la façade d'authentification.
#[derive(Clone, Copy, Debug)]
pub struct ConfigRendezvous {
    /// Clé publique de l'autorité du déploiement (celle de `nd-api`), qui a
    /// signé les jetons d'enregistrement.
    pub cle_autorite: VerifyingKey,
    /// Écart absolu maximal accepté entre l'horodatage du client et l'horloge
    /// du serveur.
    pub tolerance_horodatage: Duration,
}

impl ConfigRendezvous {
    /// Configuration avec la tolérance par défaut ([`TOLERANCE_HORODATAGE_DEFAUT`]).
    #[must_use]
    pub fn new(cle_autorite: VerifyingKey) -> Self {
        Self {
            cle_autorite,
            tolerance_horodatage: TOLERANCE_HORODATAGE_DEFAUT,
        }
    }
}

// ---------------------------------------------------------------------------
// Serveur (façade)
// ---------------------------------------------------------------------------

/// Sert le rendez-vous authentifié : moteur `nd-signaling` interne (boucle
/// locale), façade de vérification devant (bloquant, un thread par connexion).
///
/// **Appliqué** : preuve de possession d'ID à l'enregistrement (jeton
/// d'attribution + signature fraîche), `Register` nu refusé. **Encore
/// permissif** (coordination lot 05, le protocole `nd-signaling` n'a pas de
/// champ de signature) : `Heartbeat`, `PublishCandidates`, `Punch` et
/// `PollPunch` sont transmis sans authentification — un tiers ne peut ni
/// enregistrer ni **remplacer** l'adresse/le certificat d'un ID (seul un
/// `Register` signé le peut), mais il peut encore rafraîchir la présence d'un
/// ID enregistré ou déposer des candidats de punch à sa place ; l'épinglage
/// de certificat de la couche transport borne l'impact à du dérangement.
///
/// Chaque connexion cliente est bornée par un délai d'E/S ([`DELAI_ECHANGE`]) :
/// un client muet ne retient ni thread ni socket indéfiniment.
///
/// # Errors
/// Renvoie une erreur si la mise en place du moteur interne ou l'acceptation
/// d'une connexion échoue.
pub fn servir_authentifie(
    listener: TcpListener,
    registry: Registry,
    config: ConfigRendezvous,
) -> io::Result<()> {
    // Moteur nd-signaling interne : seul ce processus connaît son adresse de
    // boucle locale, et la façade ne lui transmet que des trames vérifiées.
    let moteur = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let adresse_moteur = moteur.local_addr()?;
    std::thread::spawn(move || {
        let _ = nd_signaling::serve(moteur, registry);
    });

    for stream in listener.incoming() {
        let stream = stream?;
        std::thread::spawn(move || {
            let _ = traiter_connexion(stream, adresse_moteur, &config);
        });
    }
    Ok(())
}

/// Traite une connexion cliente : une trame, un filtrage, une réponse.
/// L'échange complet est borné par [`DELAI_ECHANGE`] côté lecture et écriture.
fn traiter_connexion(
    mut client: TcpStream,
    adresse_moteur: SocketAddr,
    config: &ConfigRendezvous,
) -> io::Result<()> {
    client.set_read_timeout(Some(DELAI_ECHANGE))?;
    client.set_write_timeout(Some(DELAI_ECHANGE))?;
    let trame = lire_trame(&mut client)?;
    match trame.first().copied() {
        // Enregistrement nu : refusé, l'ID doit être prouvé.
        Some(TAG_REGISTER_NU) => ecrire_trame(&mut client, &REPONSE_REFUS),
        // Enregistrement authentifié : vérifié puis traduit pour le moteur.
        Some(TAG_REGISTER_AUTHENTIFIE) => match verifier_register_authentifie(&trame, config) {
            Some(trame_interne) => {
                let reponse = interroger_moteur(adresse_moteur, &trame_interne)?;
                ecrire_trame(&mut client, &reponse)
            }
            None => ecrire_trame(&mut client, &REPONSE_REFUS),
        },
        // Tout le reste (lookup, heartbeat, candidats, punch) : transmis.
        Some(_) => {
            let reponse = interroger_moteur(adresse_moteur, &trame)?;
            ecrire_trame(&mut client, &reponse)
        }
        // Trame vide : réponse de refus, comme le ferait le moteur.
        None => ecrire_trame(&mut client, &REPONSE_REFUS),
    }
}

/// Transmet une trame au moteur interne et renvoie sa réponse.
fn interroger_moteur(adresse_moteur: SocketAddr, trame: &[u8]) -> io::Result<Vec<u8>> {
    let mut moteur = TcpStream::connect(adresse_moteur)?;
    ecrire_trame(&mut moteur, trame)?;
    lire_trame(&mut moteur)
}

/// Vérifie une trame `RegisterAuthentifie` ; renvoie la trame `Register`
/// interne pour le moteur si tout est valide, `None` sinon (refus).
fn verifier_register_authentifie(trame: &[u8], config: &ConfigRendezvous) -> Option<Vec<u8>> {
    let mut p = 1; // Le tag a déjà été lu.
    let id = lire_u64(trame, &mut p)?;
    let addr = lire_octets(trame, &mut p)?;
    let addr = std::str::from_utf8(&addr).ok()?.to_string();
    let cert = lire_octets(trame, &mut p)?;
    let horodatage = lire_u64(trame, &mut p)?;
    let jeton = lire_octets(trame, &mut p)?;
    let signature: [u8; LG_SIGNATURE] = trame.get(p..p + LG_SIGNATURE)?.try_into().ok()?;
    p += LG_SIGNATURE;
    if p != trame.len() {
        return None; // Octets excédentaires : trame mal formée.
    }

    // 1. Fraîcheur : l'horodatage doit être dans la fenêtre de tolérance.
    let ecart = maintenant_unix().abs_diff(horodatage);
    if ecart > config.tolerance_horodatage.as_secs() {
        return None;
    }
    // 2. Le jeton d'enregistrement émane de l'autorité et vise bien cet ID.
    let jeton = JetonEnregistrement::from_bytes(&jeton)?;
    if jeton.id != id || !jeton.verifier(&config.cle_autorite) {
        return None;
    }
    // 3. Preuve de possession : la signature fraîche vient de la clé liée à
    //    l'ID et scelle l'adresse et le certificat publiés.
    let cle_client = jeton.cle_client()?;
    let message = message_enregistrement(id, &addr, &cert, horodatage);
    cle_client
        .verify_strict(&message, &Signature::from_bytes(&signature))
        .ok()?;

    // Tout est prouvé : trame `Register` du protocole nd-signaling.
    let mut interne = Vec::with_capacity(1 + 8 + 4 + addr.len() + 4 + cert.len());
    interne.push(TAG_REGISTER_NU);
    interne.extend_from_slice(&id.to_be_bytes());
    ajouter_octets(&mut interne, addr.as_bytes());
    ajouter_octets(&mut interne, &cert);
    Some(interne)
}

// ---------------------------------------------------------------------------
// Côté client (référence pour le lot 05, utilisé par les tests)
// ---------------------------------------------------------------------------

/// Message canonique signé par le client pour prouver la possession de la clé
/// liée à son ID : `(contexte, id, addr, cert, horodatage)`.
#[must_use]
pub fn message_enregistrement(id: u64, addr: &str, cert: &[u8], horodatage: u64) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(CONTEXTE_ENREGISTREMENT.len() + 8 + 4 + addr.len() + 4 + cert.len() + 8);
    message.extend_from_slice(CONTEXTE_ENREGISTREMENT);
    message.extend_from_slice(&id.to_be_bytes());
    ajouter_octets(&mut message, addr.as_bytes());
    ajouter_octets(&mut message, cert);
    message.extend_from_slice(&horodatage.to_be_bytes());
    message
}

/// Construit la trame `RegisterAuthentifie` : l'ID, l'adresse et le certificat
/// publiés, l'horodatage, le jeton d'enregistrement, et la signature de
/// possession produite avec `cle` (la clé statique liée à l'ID).
#[must_use]
pub fn trame_register_authentifie(
    id: u64,
    addr: &str,
    cert: &[u8],
    horodatage: u64,
    jeton: &JetonEnregistrement,
    cle: &SigningKey,
) -> Vec<u8> {
    let signature = cle
        .sign(&message_enregistrement(id, addr, cert, horodatage))
        .to_bytes();
    let jeton = jeton.to_bytes();
    let mut trame =
        Vec::with_capacity(1 + 8 + 4 + addr.len() + 4 + cert.len() + 8 + 4 + jeton.len() + 64);
    trame.push(TAG_REGISTER_AUTHENTIFIE);
    trame.extend_from_slice(&id.to_be_bytes());
    ajouter_octets(&mut trame, addr.as_bytes());
    ajouter_octets(&mut trame, cert);
    trame.extend_from_slice(&horodatage.to_be_bytes());
    ajouter_octets(&mut trame, &jeton);
    trame.extend_from_slice(&signature);
    trame
}

/// Client de référence : enregistre `id` auprès du rendez-vous authentifié
/// `serveur`, en publiant `addr`/`cert`, avec le jeton d'attribution et la clé
/// statique du client. (Le lot 05 portera l'équivalent dans `nd-signaling`.)
///
/// # Errors
/// `PermissionDenied` si le serveur refuse l'enregistrement, sinon les erreurs
/// réseau/protocole.
pub fn enregistrer_authentifie(
    serveur: SocketAddr,
    id: NovaId,
    addr: SocketAddr,
    cert: &[u8],
    jeton: &JetonEnregistrement,
    cle: &SigningKey,
) -> io::Result<()> {
    let trame = trame_register_authentifie(
        id.as_u64(),
        &addr.to_string(),
        cert,
        maintenant_unix(),
        jeton,
        cle,
    );
    let mut flux = TcpStream::connect(serveur)?;
    ecrire_trame(&mut flux, &trame)?;
    let reponse = lire_trame(&mut flux)?;
    if reponse.first() == Some(&0) {
        Ok(()) // `Registered` du protocole nd-signaling.
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "enregistrement refusé par le rendez-vous",
        ))
    }
}

// ---------------------------------------------------------------------------
// Tramage et encodage élémentaires (mêmes formats que nd-signaling)
// ---------------------------------------------------------------------------

/// Écrit une trame : préfixe de longueur (`u32` BE) + charge utile.
fn ecrire_trame<W: Write>(w: &mut W, charge: &[u8]) -> io::Result<()> {
    w.write_all(&(charge.len() as u32).to_be_bytes())?;
    w.write_all(charge)
}

/// Lit une trame : préfixe de longueur (`u32` BE) + charge utile.
fn lire_trame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut longueur = [0u8; 4];
    r.read_exact(&mut longueur)?;
    let longueur = u32::from_be_bytes(longueur) as usize;
    if longueur > TRAME_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trame trop grande",
        ));
    }
    let mut charge = vec![0u8; longueur];
    r.read_exact(&mut charge)?;
    Ok(charge)
}

fn ajouter_octets(out: &mut Vec<u8>, octets: &[u8]) {
    out.extend_from_slice(&(octets.len() as u32).to_be_bytes());
    out.extend_from_slice(octets);
}

fn lire_u64(d: &[u8], p: &mut usize) -> Option<u64> {
    let valeur = u64::from_be_bytes(d.get(*p..*p + 8)?.try_into().ok()?);
    *p += 8;
    Some(valeur)
}

fn lire_octets(d: &[u8], p: &mut usize) -> Option<Vec<u8>> {
    let longueur = u32::from_be_bytes(d.get(*p..*p + 4)?.try_into().ok()?) as usize;
    let octets = d.get(*p + 4..*p + 4 + longueur)?.to_vec();
    *p += 4 + longueur;
    Some(octets)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nd_api::auth::Autorite;
    use nd_signaling::RendezvousClient;

    /// Autorité de test déterministe (la même signe les jetons et configure
    /// la façade).
    fn autorite_test() -> Autorite {
        Autorite::depuis_graine(&[11u8; 32])
    }

    /// Démarre une façade authentifiée sur un port éphémère.
    fn demarrer(config: ConfigRendezvous) -> (SocketAddr, Registry) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind façade");
        let adresse = listener.local_addr().expect("adresse locale");
        let registry = Registry::new();
        let reg = registry.clone();
        std::thread::spawn(move || {
            let _ = servir_authentifie(listener, reg, config);
        });
        (adresse, registry)
    }

    fn demarrer_defaut() -> (SocketAddr, Registry, Autorite) {
        let autorite = autorite_test();
        let (adresse, registry) = demarrer(ConfigRendezvous::new(autorite.cle_publique()));
        (adresse, registry, autorite)
    }

    fn adresse_bidon() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    /// Envoie une trame brute à la façade et renvoie le premier octet de la
    /// réponse (tag).
    fn envoyer_trame_brute(serveur: SocketAddr, trame: &[u8]) -> u8 {
        let mut flux = TcpStream::connect(serveur).expect("connexion");
        ecrire_trame(&mut flux, trame).expect("écriture");
        let reponse = lire_trame(&mut flux).expect("lecture");
        reponse[0]
    }

    #[test]
    fn enregistrement_authentifie_accepte_puis_pass_through() {
        let (serveur, registry, autorite) = demarrer_defaut();
        let cle = SigningKey::from_bytes(&[2u8; 32]);
        let id = NovaId(123_456_789);
        let jeton = autorite.emettre_jeton_enregistrement(id.as_u64(), &cle.verifying_key());

        // Enregistrement prouvé : accepté, le pair est en ligne.
        enregistrer_authentifie(serveur, id, adresse_bidon(), &[1, 2, 3], &jeton, &cle)
            .expect("enregistrement authentifié");
        assert_eq!(registry.online_count(), 1);

        // Les autres opérations passent au moteur nd-signaling inchangées :
        // lookup, heartbeat, dépôt et lecture de candidats.
        let client = RendezvousClient::new(serveur);
        assert_eq!(client.lookup(id).expect("lookup").cert_der, vec![1, 2, 3]);
        client.heartbeat(id).expect("heartbeat");
        let candidats: Vec<SocketAddr> = vec!["192.168.1.10:7000".parse().unwrap()];
        client
            .publish_candidates(id, &candidats)
            .expect("candidats");
        assert_eq!(client.peer_candidates(id).expect("candidats"), candidats);
    }

    #[test]
    fn register_nu_refuse() {
        let (serveur, registry, _) = demarrer_defaut();
        // Le client historique nd-signaling (tag 1 sans preuve) est refusé.
        let client = RendezvousClient::new(serveur);
        assert!(client.register(NovaId(42), adresse_bidon(), &[9]).is_err());
        assert_eq!(registry.online_count(), 0);
        assert!(client.lookup(NovaId(42)).is_err());
    }

    #[test]
    fn enregistrement_de_l_id_d_autrui_refuse() {
        let (serveur, registry, autorite) = demarrer_defaut();
        let victime = SigningKey::from_bytes(&[2u8; 32]);
        let attaquant = SigningKey::from_bytes(&[3u8; 32]);
        let id_victime = NovaId(111_111_111);
        let jeton_victime =
            autorite.emettre_jeton_enregistrement(id_victime.as_u64(), &victime.verifying_key());

        // a) Jeton de la victime (observé sur le réseau) mais signature de
        //    l'attaquant : la preuve de possession échoue.
        assert!(enregistrer_authentifie(
            serveur,
            id_victime,
            adresse_bidon(),
            &[7],
            &jeton_victime,
            &attaquant,
        )
        .is_err());

        // b) Jeton légitime de l'attaquant pour SON id, rejoué sur l'id de la
        //    victime : l'ID du jeton ne correspond pas.
        let jeton_attaquant =
            autorite.emettre_jeton_enregistrement(222_222_222, &attaquant.verifying_key());
        assert!(enregistrer_authentifie(
            serveur,
            id_victime,
            adresse_bidon(),
            &[7],
            &jeton_attaquant,
            &attaquant,
        )
        .is_err());

        // c) Jeton « fait maison » signé par une autre autorité : refusé.
        let fausse_autorite = Autorite::depuis_graine(&[99u8; 32]);
        let jeton_forge = fausse_autorite
            .emettre_jeton_enregistrement(id_victime.as_u64(), &attaquant.verifying_key());
        assert!(enregistrer_authentifie(
            serveur,
            id_victime,
            adresse_bidon(),
            &[7],
            &jeton_forge,
            &attaquant,
        )
        .is_err());

        // Rien n'a été enregistré ; la victime, elle, s'enregistre toujours.
        assert_eq!(registry.online_count(), 0);
        enregistrer_authentifie(
            serveur,
            id_victime,
            adresse_bidon(),
            &[7],
            &jeton_victime,
            &victime,
        )
        .expect("enregistrement légitime");
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn horodatage_hors_tolerance_refuse() {
        let (serveur, registry, autorite) = demarrer_defaut();
        let cle = SigningKey::from_bytes(&[2u8; 32]);
        let id = 333_333_333u64;
        let jeton = autorite.emettre_jeton_enregistrement(id, &cle.verifying_key());
        let addr = adresse_bidon().to_string();

        // Trames signées valides mais datées hors fenêtre (passé et futur).
        for horodatage in [maintenant_unix() - 3_600, maintenant_unix() + 3_600] {
            let trame = trame_register_authentifie(id, &addr, &[7], horodatage, &jeton, &cle);
            assert_eq!(envoyer_trame_brute(serveur, &trame), 2, "refus attendu");
        }
        assert_eq!(registry.online_count(), 0);

        // La même trame datée de maintenant passe.
        let trame = trame_register_authentifie(id, &addr, &[7], maintenant_unix(), &jeton, &cle);
        assert_eq!(envoyer_trame_brute(serveur, &trame), 0, "acceptation");
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn adresse_ou_certificat_alteres_refuses() {
        let (serveur, registry, autorite) = demarrer_defaut();
        let cle = SigningKey::from_bytes(&[2u8; 32]);
        let id = 444_444_444u64;
        let jeton = autorite.emettre_jeton_enregistrement(id, &cle.verifying_key());

        // Signature produite pour un certificat, trame envoyée avec un autre :
        // la signature scelle (id, addr, cert, horodatage), donc refus.
        let horodatage = maintenant_unix();
        let signature = cle
            .sign(&message_enregistrement(
                id,
                "127.0.0.1:5000",
                &[7],
                horodatage,
            ))
            .to_bytes();
        let mut trame = Vec::new();
        trame.push(TAG_REGISTER_AUTHENTIFIE);
        trame.extend_from_slice(&id.to_be_bytes());
        ajouter_octets(&mut trame, b"127.0.0.1:5000");
        ajouter_octets(&mut trame, &[8, 8, 8]); // Certificat substitué.
        trame.extend_from_slice(&horodatage.to_be_bytes());
        ajouter_octets(&mut trame, &jeton.to_bytes());
        trame.extend_from_slice(&signature);
        assert_eq!(envoyer_trame_brute(serveur, &trame), 2, "refus attendu");
        assert_eq!(registry.online_count(), 0);
    }

    #[test]
    fn trame_authentifiee_malformee_refusee_sans_bloquer_le_service() {
        let (serveur, registry, autorite) = demarrer_defaut();
        let cle = SigningKey::from_bytes(&[2u8; 32]);
        let id = NovaId(555_555_555);
        let jeton = autorite.emettre_jeton_enregistrement(id.as_u64(), &cle.verifying_key());

        // Trame tag 8 tronquée, trame tag 8 avec octets excédentaires, trame
        // vide : refus propres.
        let valide = trame_register_authentifie(
            id.as_u64(),
            &adresse_bidon().to_string(),
            &[7],
            maintenant_unix(),
            &jeton,
            &cle,
        );
        assert_eq!(envoyer_trame_brute(serveur, &valide[..valide.len() - 1]), 2);
        let mut excedent = valide.clone();
        excedent.push(0);
        assert_eq!(envoyer_trame_brute(serveur, &excedent), 2);
        assert_eq!(envoyer_trame_brute(serveur, &[]), 2);
        assert_eq!(registry.online_count(), 0);

        // Le service reste opérationnel pour un enregistrement valide.
        enregistrer_authentifie(serveur, id, adresse_bidon(), &[7], &jeton, &cle)
            .expect("enregistrement légitime");
        assert_eq!(registry.online_count(), 1);
    }
}
