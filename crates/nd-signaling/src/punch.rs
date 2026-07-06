//! **UDP hole punching** — ouverture d'un chemin direct à travers les NAT.
//!
//! # Théorie (plan 05)
//!
//! Un NAT ne laisse entrer un datagramme UDP que s'il correspond à un
//! *mapping* créé par un envoi sortant. Le hole punching exploite cela : les
//! deux pairs envoient **simultanément** des sondes vers les candidats de
//! l'autre (adresse locale + adresse réflexive découverte via [`crate::stun`]),
//! ce qui perce chacun son propre NAT ; dès que les mappings existent des deux
//! côtés, les sondes passent et le chemin est ouvert. La simultanéité est
//! coordonnée par le serveur de rendez-vous (échange de candidats + demande de
//! punch relayée, voir [`crate::RendezvousClient::request_punch`] et
//! [`crate::RendezvousClient::poll_punch`]).
//!
//! Le succès dépend du **type de NAT** (voir [`crate::nat`]) :
//!
//! - **Full cone** : le mapping accepte les datagrammes de n'importe quelle
//!   source → le punch réussit trivialement.
//! - **(Port) restricted cone** : le mapping n'accepte que les sources vers
//!   lesquelles on a déjà émis (IP, voire IP:port) → le punch réussit car les
//!   deux pairs émettent l'un vers l'autre.
//! - **Symétrique** : le NAT alloue un mapping *différent par destination* ;
//!   l'adresse réflexive vue par le serveur STUN n'est pas celle utilisée vers
//!   le pair → les candidats sont faux et le punch échoue en général (sauf
//!   allocation de ports prédictible, non tentée ici).
//!
//! **Cas d'échec** (symétrique des deux côtés, pare-feu strict, UDP bloqué) :
//! l'appelant doit se replier sur un **relais** (serveur `nd-relay`, plan 05) —
//! le trafic reste chiffré de bout en bout, le relais ne voit que des octets.
//!
//! # Protocole de sondage
//!
//! Datagrammes de [`PAQUET_LEN`] octets : préfixe magique `NDPUNCH1`, un octet
//! de type ([sonde](TYPE_SONDE) ou [accusé](TYPE_ACK)) et un octet de rôle
//! ([`PunchRole`]). Le rôle évite qu'un pair confonde ses propres sondes avec
//! celles du pair distant (réflexion, candidats erronés pointant sur soi).
//! Recevoir un paquet valide du rôle opposé prouve que le chemin entrant est
//! ouvert ; comme chaque pair émet aussi vers l'autre, le chemin est alors
//! considéré bidirectionnel et l'adresse **source observée** est retournée —
//! elle peut différer des candidats (NAT ayant réécrit le port).
//!
//! La couche transport qui récupère la socket doit tolérer les sondes
//! résiduelles du pair : [`est_paquet_punch`] permet de les filtrer.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use nd_proto::{NdError, Result};

/// Préfixe magique des datagrammes de punch (version 1).
const MAGIC: &[u8; 8] = b"NDPUNCH1";
/// Type de paquet : sonde (ouvre le mapping NAT et teste le chemin).
const TYPE_SONDE: u8 = 1;
/// Type de paquet : accusé (répond à une sonde, confirme le chemin retour).
const TYPE_ACK: u8 = 2;
/// Taille exacte d'un paquet de punch : magique + type + rôle.
const PAQUET_LEN: usize = MAGIC.len() + 2;
/// Intervalle entre deux salves de sondes vers tous les candidats.
const INTERVALLE_SONDES: Duration = Duration::from_millis(100);
/// Nombre d'accusés finaux émis après confirmation, pour aider le pair
/// distant à confirmer de son côté même si quelques datagrammes se perdent.
const ACKS_FINAUX: u32 = 3;

/// Durée totale de sondage par défaut avant repli (relais `nd-relay`).
pub const DEFAULT_PUNCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Rôle du pair dans le hole punching.
///
/// Purement symétrique dans l'algorithme (les deux côtés sondent et
/// répondent) ; le rôle est encodé dans chaque paquet pour qu'un pair ignore
/// les paquets portant son **propre** rôle (réflexion par le réseau, candidat
/// erroné pointant sur soi-même, deux pairs sur la même machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchRole {
    /// Pair appelant (contrôleur) : celui qui a déposé la demande de punch.
    Caller,
    /// Pair appelé (contrôlé) : celui qui a relevé la demande de punch.
    Callee,
}

impl PunchRole {
    /// Octet de rôle encodé dans les paquets.
    fn octet(self) -> u8 {
        match self {
            PunchRole::Caller => 0,
            PunchRole::Callee => 1,
        }
    }
}

/// Construit un paquet de punch : `NDPUNCH1` + type + rôle de l'émetteur.
fn construire_paquet(type_paquet: u8, role: PunchRole) -> [u8; PAQUET_LEN] {
    let mut p = [0u8; PAQUET_LEN];
    p[..MAGIC.len()].copy_from_slice(MAGIC);
    p[MAGIC.len()] = type_paquet;
    p[MAGIC.len() + 1] = role.octet();
    p
}

/// Vrai si le datagramme est un paquet de punch NovaDesk (toute version du
/// type/rôle). Sert à la couche transport pour filtrer les sondes résiduelles
/// qui arrivent après [`udp_hole_punch`].
#[must_use]
pub fn est_paquet_punch(donnees: &[u8]) -> bool {
    donnees.len() == PAQUET_LEN && donnees[..MAGIC.len()] == MAGIC[..]
}

/// Analyse un datagramme reçu pendant le punch.
///
/// Renvoie le type de paquet ([`TYPE_SONDE`] ou [`TYPE_ACK`]) si le paquet est
/// bien formé **et** émis par le rôle opposé ; `None` pour tout le reste
/// (datagramme parasite, écho de notre propre rôle, type inconnu).
fn analyser_paquet(donnees: &[u8], role_local: PunchRole) -> Option<u8> {
    if !est_paquet_punch(donnees) {
        return None;
    }
    let type_paquet = donnees[MAGIC.len()];
    let role_emetteur = donnees[MAGIC.len() + 1];
    if role_emetteur == role_local.octet() {
        return None;
    }
    matches!(type_paquet, TYPE_SONDE | TYPE_ACK).then_some(type_paquet)
}

/// Perce les NAT en UDP vers le pair distant et renvoie le chemin ouvert.
///
/// Émet des salves de sondes vers **tous** les `candidates` du pair (adresse
/// locale + adresse réflexive STUN, obtenus via le rendez-vous), répond aux
/// sondes entrantes par des accusés, et s'arrête dès qu'un paquet valide du
/// rôle opposé arrive : l'adresse source observée est le chemin ouvert.
/// Timeout par défaut : [`DEFAULT_PUNCH_TIMEOUT`] (voir
/// [`udp_hole_punch_with_timeout`] pour le régler).
///
/// La socket est rendue à l'appelant (timeout de lecture remis à `None`) avec
/// l'adresse confirmée du pair ; c'est sur ce couple que la couche transport
/// (QUIC, voir `nd-transport`) établit ensuite sa session. Les deux pairs
/// doivent lancer cet appel **en même temps** (coordination rendez-vous) avec
/// des rôles opposés.
///
/// # Errors
/// Échec si `candidates` est vide, sur erreur de socket, ou si aucun paquet
/// valide n'arrive avant le timeout — il faut alors se replier sur le relais
/// (`nd-relay`), cas typique des NAT symétriques (voir [`crate::nat`]).
pub fn udp_hole_punch(
    local: UdpSocket,
    candidates: &[SocketAddr],
    role: PunchRole,
) -> Result<(UdpSocket, SocketAddr)> {
    udp_hole_punch_with_timeout(local, candidates, role, DEFAULT_PUNCH_TIMEOUT)
}

/// Variante de [`udp_hole_punch`] avec un timeout global explicite.
///
/// # Errors
/// Voir [`udp_hole_punch`].
pub fn udp_hole_punch_with_timeout(
    local: UdpSocket,
    candidates: &[SocketAddr],
    role: PunchRole,
    timeout: Duration,
) -> Result<(UdpSocket, SocketAddr)> {
    if candidates.is_empty() {
        return Err(NdError::Protocol(
            "hole punching : aucun candidat à sonder".into(),
        ));
    }
    let sonde = construire_paquet(TYPE_SONDE, role);
    let ack = construire_paquet(TYPE_ACK, role);
    let debut = Instant::now();
    let mut tampon = [0u8; 64];

    // Boucle de salves : sonder tous les candidats, puis écouter jusqu'à la
    // salve suivante. Chaque salve rafraîchit les mappings NAT sortants.
    let confirmee: Option<SocketAddr> = 'punch: loop {
        let Some(restant_total) = timeout
            .checked_sub(debut.elapsed())
            .filter(|r| !r.is_zero())
        else {
            break 'punch None;
        };
        for candidat in candidates {
            // Un candidat injoignable (ICMP, route absente) ne condamne pas
            // les autres : l'erreur d'envoi est ignorée.
            let _ = local.send_to(&sonde, candidat);
        }
        // Fenêtre d'écoute jusqu'à la prochaine salve (bornée par l'échéance).
        let fin_fenetre = Instant::now() + INTERVALLE_SONDES.min(restant_total);
        loop {
            let Some(attente) = fin_fenetre
                .checked_duration_since(Instant::now())
                .filter(|r| !r.is_zero())
            else {
                break; // fenêtre écoulée : nouvelle salve
            };
            local.set_read_timeout(Some(attente))?;
            match local.recv_from(&mut tampon) {
                Ok((n, source)) => match analyser_paquet(&tampon[..n], role) {
                    // Sonde entrante : on accuse réception (le pair pourra
                    // confirmer) et le chemin est considéré ouvert.
                    Some(TYPE_SONDE) => {
                        let _ = local.send_to(&ack, source);
                        break 'punch Some(source);
                    }
                    // Accusé : notre sonde a fait l'aller, l'accusé le retour.
                    Some(_) => break 'punch Some(source),
                    // Datagramme parasite ou notre propre rôle : ignoré.
                    None => {}
                },
                // Fin de la fenêtre d'écoute : on repart en salve.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    break;
                }
                // Windows remonte les ICMP « port unreachable » (candidat
                // mort) comme des erreurs de lecture : on continue d'écouter.
                Err(_) => {}
            }
        }
    };

    match confirmee {
        Some(pair) => {
            // Accusés finaux : si le pair n'a pas encore confirmé, ces
            // paquets suffisent (recevoir un accusé valide confirme).
            for _ in 0..ACKS_FINAUX {
                let _ = local.send_to(&ack, pair);
            }
            local.set_read_timeout(None)?;
            Ok((local, pair))
        }
        None => Err(NdError::Protocol(format!(
            "hole punching : aucun chemin ouvert après {timeout:?} \
             (NAT symétrique ou UDP bloqué ?) — repli sur le relais nd-relay"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Socket UDP loopback éphémère + son adresse effective.
    fn socket_loopback() -> (UdpSocket, SocketAddr) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a = s.local_addr().unwrap();
        (s, a)
    }

    #[test]
    fn paquet_bien_forme_et_reconnu() {
        let p = construire_paquet(TYPE_SONDE, PunchRole::Caller);
        assert_eq!(p.len(), PAQUET_LEN);
        assert!(est_paquet_punch(&p));
        // Le récepteur (rôle opposé) accepte la sonde.
        assert_eq!(analyser_paquet(&p, PunchRole::Callee), Some(TYPE_SONDE));
        // L'émetteur (même rôle) ignore son propre écho.
        assert_eq!(analyser_paquet(&p, PunchRole::Caller), None);
    }

    #[test]
    fn analyse_rejette_les_paquets_invalides() {
        // Magique corrompu.
        let mut p = construire_paquet(TYPE_ACK, PunchRole::Caller);
        p[0] ^= 0xFF;
        assert!(!est_paquet_punch(&p));
        assert_eq!(analyser_paquet(&p, PunchRole::Callee), None);

        // Type de paquet inconnu.
        let mut p = construire_paquet(TYPE_SONDE, PunchRole::Caller);
        p[MAGIC.len()] = 42;
        assert_eq!(analyser_paquet(&p, PunchRole::Callee), None);

        // Longueur incorrecte (tronqué / rallongé).
        assert_eq!(
            analyser_paquet(&p[..PAQUET_LEN - 1], PunchRole::Callee),
            None
        );
        assert_eq!(
            analyser_paquet(&[0u8; PAQUET_LEN + 1], PunchRole::Callee),
            None
        );
        assert_eq!(analyser_paquet(&[], PunchRole::Callee), None);
    }

    /// Punch loopback : deux sockets locales se sondent mutuellement, chacune
    /// doit retourner l'adresse de l'autre (le chemin ouvert).
    #[test]
    fn punch_loopback_bidirectionnel() {
        let (sock_a, addr_a) = socket_loopback();
        let (sock_b, addr_b) = socket_loopback();

        let appelant =
            std::thread::spawn(move || udp_hole_punch(sock_a, &[addr_b], PunchRole::Caller));
        let appele =
            std::thread::spawn(move || udp_hole_punch(sock_b, &[addr_a], PunchRole::Callee));

        let (_sock, pair_vu_par_a) = appelant.join().unwrap().expect("punch appelant");
        let (_sock, pair_vu_par_b) = appele.join().unwrap().expect("punch appelé");
        assert_eq!(pair_vu_par_a, addr_b);
        assert_eq!(pair_vu_par_b, addr_a);
    }

    /// Avec plusieurs candidats dont des adresses mortes, le punch retourne
    /// celui qui répond réellement.
    #[test]
    fn punch_retourne_le_candidat_qui_repond() {
        let (sock_a, addr_a) = socket_loopback();
        let (sock_b, addr_b) = socket_loopback();

        // Candidats morts : sockets liées mais jamais lues ni répondues
        // (gardées vivantes pour que leurs ports ne soient pas réattribués).
        let (_garde_1, mort_1) = socket_loopback();
        let (_garde_2, mort_2) = socket_loopback();

        let candidats_pour_a = vec![mort_1, addr_b, mort_2];
        let appelant = std::thread::spawn(move || {
            udp_hole_punch(sock_a, &candidats_pour_a, PunchRole::Caller)
        });
        let appele = std::thread::spawn(move || {
            udp_hole_punch(sock_b, &[mort_2, addr_a], PunchRole::Callee)
        });

        let (_sock, pair_vu_par_a) = appelant.join().unwrap().expect("punch appelant");
        let (_sock, pair_vu_par_b) = appele.join().unwrap().expect("punch appelé");
        assert_eq!(pair_vu_par_a, addr_b, "seul le vrai candidat répond");
        assert_eq!(pair_vu_par_b, addr_a);
    }

    /// Sans pair en face, le punch échoue au timeout (repli relais attendu).
    #[test]
    fn punch_echoue_au_timeout_sans_pair() {
        let (sock, _) = socket_loopback();
        // Socket liée mais muette : personne ne sonde ni ne répond.
        let (_garde, mort) = socket_loopback();
        let debut = Instant::now();
        let resultat = udp_hole_punch_with_timeout(
            sock,
            &[mort],
            PunchRole::Caller,
            Duration::from_millis(250),
        );
        assert!(resultat.is_err());
        assert!(debut.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn punch_echoue_sans_candidats() {
        let (sock, _) = socket_loopback();
        assert!(udp_hole_punch(sock, &[], PunchRole::Caller).is_err());
    }

    /// La socket rendue reste utilisable après le punch (timeout remis à zéro,
    /// échange applicatif possible sur le chemin ouvert).
    #[test]
    fn socket_rendue_utilisable_apres_punch() {
        let (sock_a, addr_a) = socket_loopback();
        let (sock_b, addr_b) = socket_loopback();

        let appelant =
            std::thread::spawn(move || udp_hole_punch(sock_a, &[addr_b], PunchRole::Caller));
        let appele =
            std::thread::spawn(move || udp_hole_punch(sock_b, &[addr_a], PunchRole::Callee));
        let (sock_a, pair_a) = appelant.join().unwrap().unwrap();
        let (sock_b, _pair_b) = appele.join().unwrap().unwrap();
        assert_eq!(sock_a.read_timeout().unwrap(), None);

        // Échange applicatif sur le chemin ouvert, en filtrant les sondes
        // résiduelles du punch (rôle de la couche transport).
        sock_a.send_to(b"bonjour", pair_a).unwrap();
        sock_b
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut tampon = [0u8; 64];
        loop {
            let (n, _) = sock_b.recv_from(&mut tampon).unwrap();
            if est_paquet_punch(&tampon[..n]) {
                continue;
            }
            assert_eq!(&tampon[..n], b"bonjour");
            break;
        }
    }
}
