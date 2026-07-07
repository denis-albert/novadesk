//! Client **STUN** (RFC 5389) — découverte de l'adresse réflexive publique.
//!
//! Préalable au NAT traversal (voir `../../plan-technique/05-connectivite-nat.md`) :
//! le pair envoie une *Binding Request* à un serveur STUN public qui lui renvoie
//! l'adresse (IP:port) telle qu'il la voit, encodée dans l'attribut
//! **XOR-MAPPED-ADDRESS**. C'est cette adresse que le pair publie ensuite au
//! rendez-vous pour permettre le hole punching.
//!
//! Implémentation std pure : `UdpSocket` bloquant avec timeout de lecture,
//! transaction ID dérivé de l'horloge + compteur (pas de crate rng).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nd_proto::{NdError, Result};

/// Magic cookie STUN (RFC 5389 §6), fixe pour tous les messages.
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Type de message : Binding Request.
const BINDING_REQUEST: u16 = 0x0001;
/// Type de message : Binding Success Response.
const BINDING_SUCCESS: u16 = 0x0101;
/// Attribut XOR-MAPPED-ADDRESS (RFC 5389 §15.2).
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Attribut CHANGE-REQUEST (RFC 3489 §11.2.4, repris par RFC 5780 §7.2) :
/// demande au serveur de répondre depuis une autre IP et/ou un autre port —
/// c'est la brique du test de **filtrage** NAT (voir [`crate::nat`]).
const ATTR_CHANGE_REQUEST: u16 = 0x0003;
/// Drapeau CHANGE-REQUEST « changer d'adresse IP ».
const CHANGE_IP: u32 = 0x4;
/// Drapeau CHANGE-REQUEST « changer de port ».
const CHANGE_PORT: u32 = 0x2;
/// Taille de l'en-tête STUN (type + longueur + cookie + transaction ID).
const HEADER_LEN: usize = 20;
/// Famille d'adresse IPv4 dans un attribut d'adresse.
const FAMILY_IPV4: u8 = 0x01;
/// Famille d'adresse IPv6 dans un attribut d'adresse.
const FAMILY_IPV6: u8 = 0x02;

/// Erreur de protocole STUN avec un motif lisible.
fn erreur(motif: &str) -> NdError {
    NdError::Protocol(format!("réponse STUN invalide : {motif}"))
}

// ---------------------------------------------------------------------------
// Transaction ID
// ---------------------------------------------------------------------------

/// Mélange SplitMix64 : diffusion rapide et déterministe des bits d'une graine.
fn splitmix64(graine: u64) -> u64 {
    let mut z = graine.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Génère un transaction ID de 12 octets, unique par requête.
///
/// Sans crate rng : on mélange l'horloge (nanosecondes), un compteur atomique
/// (unicité intra-processus) et le PID (unicité inter-processus) via SplitMix64.
/// Suffisant pour corréler requête/réponse ; pas une garantie cryptographique.
fn nouveau_transaction_id() -> [u8; 12] {
    static COMPTEUR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let tour = COMPTEUR.fetch_add(1, Ordering::Relaxed);
    let a = splitmix64(nanos ^ u64::from(std::process::id()));
    let b = splitmix64(a ^ tour);
    let mut id = [0u8; 12];
    id[..8].copy_from_slice(&a.to_be_bytes());
    id[8..].copy_from_slice(&b.to_be_bytes()[..4]);
    id
}

// ---------------------------------------------------------------------------
// Encodage / décodage des messages
// ---------------------------------------------------------------------------

/// Construit une Binding Request STUN : en-tête de 20 octets, zéro attribut.
fn construire_binding_request(transaction_id: &[u8; 12]) -> [u8; HEADER_LEN] {
    let mut req = [0u8; HEADER_LEN];
    req[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // req[2..4] : longueur des attributs = 0 (déjà à zéro).
    req[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    req[8..20].copy_from_slice(transaction_id);
    req
}

/// Construit une Binding Request portant un attribut CHANGE-REQUEST : le
/// serveur (s'il implémente RFC 3489/5780) répondra depuis une autre IP
/// et/ou un autre port selon les drapeaux.
fn construire_binding_request_change(
    transaction_id: &[u8; 12],
    change_ip: bool,
    change_port: bool,
) -> Vec<u8> {
    let mut drapeaux = 0u32;
    if change_ip {
        drapeaux |= CHANGE_IP;
    }
    if change_port {
        drapeaux |= CHANGE_PORT;
    }
    let mut req = Vec::with_capacity(HEADER_LEN + 8);
    req.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Longueur des attributs : CHANGE-REQUEST = 4 octets d'en-tête + 4 de valeur.
    req.extend_from_slice(&8u16.to_be_bytes());
    req.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    req.extend_from_slice(transaction_id);
    req.extend_from_slice(&ATTR_CHANGE_REQUEST.to_be_bytes());
    req.extend_from_slice(&4u16.to_be_bytes());
    req.extend_from_slice(&drapeaux.to_be_bytes());
    req
}

/// Analyse une Binding Success Response et extrait le XOR-MAPPED-ADDRESS.
///
/// Vérifie type de message, cohérence de longueur, magic cookie et transaction
/// ID, puis parcourt les attributs TLV (alignés sur 4 octets).
fn analyser_binding_response(donnees: &[u8], transaction_id: &[u8; 12]) -> Result<SocketAddr> {
    if donnees.len() < HEADER_LEN {
        return Err(erreur("en-tête tronqué"));
    }
    let type_msg = u16::from_be_bytes([donnees[0], donnees[1]]);
    if type_msg != BINDING_SUCCESS {
        return Err(erreur(&format!(
            "type de message inattendu 0x{type_msg:04x}"
        )));
    }
    let longueur = usize::from(u16::from_be_bytes([donnees[2], donnees[3]]));
    if longueur % 4 != 0 || HEADER_LEN + longueur > donnees.len() {
        return Err(erreur("longueur d'attributs incohérente"));
    }
    let cookie = u32::from_be_bytes([donnees[4], donnees[5], donnees[6], donnees[7]]);
    if cookie != MAGIC_COOKIE {
        return Err(erreur("magic cookie invalide"));
    }
    if &donnees[8..HEADER_LEN] != transaction_id {
        return Err(erreur("transaction ID inattendu"));
    }

    // Parcours des attributs : type (u16) + longueur (u16) + valeur, chaque
    // attribut étant complété à un multiple de 4 octets.
    let attributs = &donnees[HEADER_LEN..HEADER_LEN + longueur];
    let mut p = 0;
    while p + 4 <= attributs.len() {
        let type_attr = u16::from_be_bytes([attributs[p], attributs[p + 1]]);
        let long_attr = usize::from(u16::from_be_bytes([attributs[p + 2], attributs[p + 3]]));
        p += 4;
        let valeur = attributs
            .get(p..p + long_attr)
            .ok_or_else(|| erreur("attribut tronqué"))?;
        if type_attr == ATTR_XOR_MAPPED_ADDRESS {
            return decoder_xor_mapped_address(valeur, transaction_id);
        }
        p += long_attr.next_multiple_of(4);
    }
    Err(erreur("attribut XOR-MAPPED-ADDRESS absent"))
}

/// Décode la valeur d'un attribut XOR-MAPPED-ADDRESS (IPv4 ou IPv6).
///
/// Le port est XORé avec les 16 bits de poids fort du magic cookie ; l'adresse
/// IPv4 avec le cookie entier ; l'adresse IPv6 avec cookie ‖ transaction ID.
fn decoder_xor_mapped_address(valeur: &[u8], transaction_id: &[u8; 12]) -> Result<SocketAddr> {
    if valeur.len() < 8 {
        return Err(erreur("XOR-MAPPED-ADDRESS tronqué"));
    }
    let famille = valeur[1];
    let port = u16::from_be_bytes([valeur[2], valeur[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
    match famille {
        FAMILY_IPV4 => {
            if valeur.len() != 8 {
                return Err(erreur("longueur IPv4 invalide"));
            }
            let brut = u32::from_be_bytes([valeur[4], valeur[5], valeur[6], valeur[7]]);
            let ip = Ipv4Addr::from(brut ^ MAGIC_COOKIE);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        FAMILY_IPV6 => {
            if valeur.len() != 20 {
                return Err(erreur("longueur IPv6 invalide"));
            }
            // Masque de dé-XOR : magic cookie (4 octets) suivi du transaction ID.
            let mut masque = [0u8; 16];
            masque[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            masque[4..].copy_from_slice(transaction_id);
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&valeur[4..20]);
            for (o, m) in octets.iter_mut().zip(masque) {
                *o ^= m;
            }
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(octets),
                port,
                0,
                0,
            )))
        }
        _ => Err(erreur("famille d'adresse inconnue")),
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Client STUN minimal : une Binding Request, quelques retransmissions.
pub struct StunClient {
    /// Adresse du serveur STUN interrogé.
    server: SocketAddr,
    /// Timeout de lecture par tentative.
    timeout: Duration,
    /// Nombre de tentatives (envoi + attente) avant abandon.
    tentatives: u32,
}

impl StunClient {
    /// Client avec les réglages par défaut : timeout 2 s, 3 tentatives.
    #[must_use]
    pub fn new(server: SocketAddr) -> Self {
        Self {
            server,
            timeout: Duration::from_secs(2),
            tentatives: 3,
        }
    }

    /// Remplace le timeout de lecture par tentative.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Interroge le serveur et renvoie l'adresse publique (IP:port) vue par lui.
    ///
    /// # Errors
    /// Renvoie une erreur si l'envoi UDP échoue, si aucune réponse valide
    /// n'arrive dans les délais, ou si la réponse est malformée.
    pub fn discover(&self) -> Result<SocketAddr> {
        // Socket éphémère de la même famille que le serveur cible.
        let locale: SocketAddr = match self.server {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind(locale)?;
        decouvrir_par_socket(&socket, self.server, self.timeout, self.tentatives)
    }
}

/// Transaction Binding complète sur une socket **fournie** : envoi de la
/// requête, retransmissions, extraction du XOR-MAPPED-ADDRESS.
///
/// Utilisé par [`StunClient::discover`] (socket éphémère) et par la détection
/// du type de NAT ([`crate::nat`]), qui doit interroger **deux** serveurs
/// depuis la **même** socket pour comparer les mappings. Laisse le timeout de
/// lecture de la socket positionné à `timeout`.
pub(crate) fn decouvrir_par_socket(
    socket: &UdpSocket,
    serveur: SocketAddr,
    timeout: Duration,
    tentatives: u32,
) -> Result<SocketAddr> {
    socket.set_read_timeout(Some(timeout))?;
    let transaction_id = nouveau_transaction_id();
    let requete = construire_binding_request(&transaction_id);
    let mut tampon = [0u8; 1500];
    let mut derniere = erreur("aucune réponse du serveur STUN");
    for _ in 0..tentatives {
        socket.send_to(&requete, serveur)?;
        match socket.recv_from(&mut tampon) {
            Ok((n, _)) => match analyser_binding_response(&tampon[..n], &transaction_id) {
                Ok(adresse) => return Ok(adresse),
                // Datagramme parasite ou réponse invalide : on retente.
                Err(e) => derniere = e,
            },
            // Timeout (ou ICMP « port unreachable » remonté par Windows) :
            // on retransmet, conformément à l'esprit de RFC 5389 §7.2.1.
            Err(e) => derniere = e.into(),
        }
    }
    Err(derniere)
}

/// Transaction Binding avec attribut **CHANGE-REQUEST** sur une socket
/// fournie (test de filtrage NAT, RFC 3489/5780 — voir [`crate::nat`]) : la
/// réponse attendue provient d'une **autre** adresse que `serveur`, c'est tout
/// l'objet du test. Renvoie `(adresse mappée, source de la réponse)` — c'est à
/// l'appelant de vérifier que la source a bien changé.
///
/// Les datagrammes dont le transaction ID ne correspond pas (réponses
/// tardives d'une transaction précédente sur la même socket, parasites) sont
/// ignorés sans consommer la tentative. Laisse le timeout de lecture de la
/// socket positionné.
pub(crate) fn decouvrir_change_request(
    socket: &UdpSocket,
    serveur: SocketAddr,
    change_ip: bool,
    change_port: bool,
    timeout: Duration,
    tentatives: u32,
) -> Result<(SocketAddr, SocketAddr)> {
    let transaction_id = nouveau_transaction_id();
    let requete = construire_binding_request_change(&transaction_id, change_ip, change_port);
    let mut tampon = [0u8; 1500];
    let mut derniere = erreur("aucune réponse au CHANGE-REQUEST");
    for _ in 0..tentatives {
        socket.send_to(&requete, serveur)?;
        // Fenêtre d'écoute de la tentative : bornée par `timeout` global.
        let echeance = Instant::now() + timeout;
        loop {
            let Some(restant) = echeance
                .checked_duration_since(Instant::now())
                .filter(|r| !r.is_zero())
            else {
                break; // fenêtre écoulée : retransmission
            };
            socket.set_read_timeout(Some(restant))?;
            match socket.recv_from(&mut tampon) {
                Ok((n, source)) => {
                    match analyser_binding_response(&tampon[..n], &transaction_id) {
                        Ok(mappee) => return Ok((mappee, source)),
                        // Parasite ou transaction périmée : on continue d'écouter.
                        Err(e) => derniere = e,
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    break;
                }
                // ICMP « port unreachable » remonté par Windows : on continue.
                Err(_) => {}
            }
        }
    }
    Err(derniere)
}

/// Découvre l'adresse réflexive publique via le serveur STUN donné.
///
/// Raccourci : `StunClient::new(stun_server).discover()`.
///
/// # Errors
/// Voir [`StunClient::discover`].
pub fn discover_public_addr(stun_server: SocketAddr) -> Result<SocketAddr> {
    StunClient::new(stun_server).discover()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Transaction ID de test, reconnaissable.
    const TID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    /// Forge une Binding Success Response contenant les attributs fournis
    /// (déjà encodés TLV + padding).
    fn forger_reponse(tid: &[u8; 12], attributs: &[u8]) -> Vec<u8> {
        let mut r = Vec::with_capacity(HEADER_LEN + attributs.len());
        r.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        r.extend_from_slice(&(attributs.len() as u16).to_be_bytes());
        r.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        r.extend_from_slice(tid);
        r.extend_from_slice(attributs);
        r
    }

    /// Encode un attribut XOR-MAPPED-ADDRESS IPv4 pour l'adresse donnée.
    fn attribut_xor_ipv4(ip: Ipv4Addr, port: u16) -> Vec<u8> {
        let mut a = Vec::new();
        a.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        a.extend_from_slice(&8u16.to_be_bytes());
        a.push(0);
        a.push(FAMILY_IPV4);
        a.extend_from_slice(&(port ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
        a.extend_from_slice(&(u32::from(ip) ^ MAGIC_COOKIE).to_be_bytes());
        a
    }

    #[test]
    fn requete_binding_bien_formee() {
        let req = construire_binding_request(&TID);
        assert_eq!(req.len(), HEADER_LEN);
        // Type : Binding Request.
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        // Longueur des attributs : 0.
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0);
        // Magic cookie.
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            MAGIC_COOKIE
        );
        // Transaction ID recopié tel quel.
        assert_eq!(&req[8..20], &TID);
    }

    #[test]
    fn transaction_ids_distincts() {
        assert_ne!(nouveau_transaction_id(), nouveau_transaction_id());
    }

    #[test]
    fn requete_change_request_bien_formee() {
        let req = construire_binding_request_change(&TID, true, false);
        assert_eq!(req.len(), HEADER_LEN + 8);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        // Longueur des attributs : 8 (en-tête TLV + valeur u32).
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 8);
        // Attribut CHANGE-REQUEST avec le seul drapeau « change IP ».
        assert_eq!(u16::from_be_bytes([req[20], req[21]]), ATTR_CHANGE_REQUEST);
        assert_eq!(u16::from_be_bytes([req[22], req[23]]), 4);
        let drapeaux = u32::from_be_bytes([req[24], req[25], req[26], req[27]]);
        assert_eq!(drapeaux, CHANGE_IP);

        // Les deux drapeaux combinés.
        let req = construire_binding_request_change(&TID, true, true);
        let drapeaux = u32::from_be_bytes([req[24], req[25], req[26], req[27]]);
        assert_eq!(drapeaux, CHANGE_IP | CHANGE_PORT);

        // Aucun drapeau : l'attribut reste présent, valeur nulle.
        let req = construire_binding_request_change(&TID, false, false);
        let drapeaux = u32::from_be_bytes([req[24], req[25], req[26], req[27]]);
        assert_eq!(drapeaux, 0);
    }

    #[test]
    fn parse_xor_mapped_address_ipv4() {
        let attendu = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 54321));
        let reponse = forger_reponse(
            &TID,
            &attribut_xor_ipv4(Ipv4Addr::new(203, 0, 113, 7), 54321),
        );
        assert_eq!(analyser_binding_response(&reponse, &TID).unwrap(), attendu);
    }

    #[test]
    fn parse_ignore_attributs_inconnus_avec_padding() {
        // Attribut SOFTWARE (0x8022) de 6 octets → complété à 8, puis l'adresse.
        let mut attrs = Vec::new();
        attrs.extend_from_slice(&0x8022u16.to_be_bytes());
        attrs.extend_from_slice(&6u16.to_be_bytes());
        attrs.extend_from_slice(b"NovaDk\0\0");
        attrs.extend_from_slice(&attribut_xor_ipv4(Ipv4Addr::new(198, 51, 100, 2), 443));
        let reponse = forger_reponse(&TID, &attrs);
        assert_eq!(
            analyser_binding_response(&reponse, &TID).unwrap(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 2), 443))
        );
    }

    #[test]
    fn parse_xor_mapped_address_ipv6() {
        let ip = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42);
        let port = 6000;
        let mut masque = [0u8; 16];
        masque[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        masque[4..].copy_from_slice(&TID);
        let mut xored = ip.octets();
        for (o, m) in xored.iter_mut().zip(masque) {
            *o ^= m;
        }
        let mut attrs = Vec::new();
        attrs.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attrs.extend_from_slice(&20u16.to_be_bytes());
        attrs.push(0);
        attrs.push(FAMILY_IPV6);
        attrs.extend_from_slice(&(port ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
        attrs.extend_from_slice(&xored);
        let reponse = forger_reponse(&TID, &attrs);
        assert_eq!(
            analyser_binding_response(&reponse, &TID).unwrap(),
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0))
        );
    }

    #[test]
    fn rejet_reponses_malformees() {
        let valide = forger_reponse(&TID, &attribut_xor_ipv4(Ipv4Addr::new(203, 0, 113, 7), 80));

        // Trop courte pour contenir l'en-tête.
        assert!(analyser_binding_response(&[0u8; 10], &TID).is_err());

        // Type inattendu (Binding Error Response 0x0111).
        let mut r = valide.clone();
        r[0..2].copy_from_slice(&0x0111u16.to_be_bytes());
        assert!(analyser_binding_response(&r, &TID).is_err());

        // Magic cookie corrompu.
        let mut r = valide.clone();
        r[4] ^= 0xFF;
        assert!(analyser_binding_response(&r, &TID).is_err());

        // Transaction ID différent.
        let autre_tid = [9u8; 12];
        assert!(analyser_binding_response(&valide, &autre_tid).is_err());

        // Longueur annoncée dépassant le datagramme.
        let mut r = valide.clone();
        r[2..4].copy_from_slice(&200u16.to_be_bytes());
        assert!(analyser_binding_response(&r, &TID).is_err());

        // Attribut tronqué (valeur coupée).
        let mut r = valide.clone();
        r.truncate(HEADER_LEN + 6);
        r[2..4].copy_from_slice(&4u16.to_be_bytes());
        assert!(analyser_binding_response(&r, &TID).is_err());

        // Aucun attribut XOR-MAPPED-ADDRESS.
        let r = forger_reponse(&TID, &[]);
        assert!(analyser_binding_response(&r, &TID).is_err());

        // Famille d'adresse inconnue.
        let mut r = valide;
        r[HEADER_LEN + 5] = 0x03;
        assert!(analyser_binding_response(&r, &TID).is_err());
    }

    /// Test réseau réel : interroge un serveur STUN public et affiche l'adresse
    /// publique découverte. Ignoré par défaut (dépend du réseau) ; lancer avec
    /// `cargo test -p nd-signaling -- --ignored`.
    #[test]
    #[ignore = "dépend du réseau : interroge stun.l.google.com"]
    fn decouverte_adresse_publique_reelle() {
        use std::net::ToSocketAddrs;
        let serveur = "stun.l.google.com:19302"
            .to_socket_addrs()
            .expect("résolution DNS de stun.l.google.com")
            .find(SocketAddr::is_ipv4)
            .expect("aucune adresse IPv4 pour le serveur STUN");
        let publique = discover_public_addr(serveur).expect("découverte STUN");
        println!("adresse publique découverte via {serveur} : {publique}");
    }
}
