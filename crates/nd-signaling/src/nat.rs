//! Détection **best-effort** du type de NAT via deux serveurs STUN.
//!
//! # Principe (plan 05)
//!
//! Une même socket UDP interroge **deux** serveurs STUN distincts et compare
//! les adresses réflexives renvoyées :
//!
//! - aucune réponse → UDP filtré ([`NatType::Blocked`]) ;
//! - réflexive = adresse locale → pas de NAT ([`NatType::Open`]) ;
//! - deux réflexives **identiques** → le NAT alloue un mapping indépendant de
//!   la destination : NAT *cone*, hole punching possible ;
//! - deux réflexives **différentes** → mapping par destination :
//!   [`NatType::Symmetric`], l'adresse publiée au rendez-vous ne vaut rien
//!   pour le pair → prévoir le repli relais (`nd-relay`).
//!
//! # Limites (documentées, assumées)
//!
//! Avec de simples Binding Requests RFC 5389 on ne teste que le comportement
//! de **mapping**, pas celui de **filtrage** : distinguer *full cone* /
//! *restricted* / *port-restricted* exigerait un serveur coopératif répondant
//! depuis une autre IP/port (CHANGE-REQUEST, RFC 5780), rarement disponible.
//! [`detect_nat_type`] renvoie donc [`NatType::PortRestricted`] pour tout NAT
//! cone — l'hypothèse la **plus restrictive**, sûre pour décider du punch ;
//! [`NatType::FullCone`] et [`NatType::Restricted`] restent dans l'énumération
//! pour une future détection RFC 5780 ou une configuration manuelle. Autres
//! angles morts classiques : échantillon unique (un NAT peut changer de
//! politique sous charge), NAT multiples en cascade, mappings expirés entre
//! les deux requêtes, réponse d'un seul serveur (comparaison impossible → on
//! suppose un cone, prudence).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use crate::stun;

/// Timeout de lecture par tentative STUN pendant la détection.
const TIMEOUT_PAR_DEFAUT: Duration = Duration::from_secs(2);
/// Nombre de tentatives par serveur STUN pendant la détection.
const TENTATIVES_PAR_DEFAUT: u32 = 3;

/// Type de NAT derrière lequel se trouve ce pair (classification classique).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatType {
    /// Pas de NAT : l'adresse locale est directement joignable.
    Open,
    /// *Full cone* : le mapping accepte les datagrammes de n'importe quelle
    /// source externe. Punch trivial.
    FullCone,
    /// *Restricted cone* : le mapping n'accepte que les sources (IP) vers
    /// lesquelles le pair a déjà émis. Punch fiable.
    Restricted,
    /// *Port-restricted cone* : filtrage par IP **et** port source. Punch
    /// fiable entre deux cones ; fragile face à un NAT symétrique.
    PortRestricted,
    /// Symétrique : un mapping différent par destination — l'adresse
    /// réflexive vue par STUN n'est pas celle vue par le pair. Punch
    /// improbable → relais (`nd-relay`).
    Symmetric,
    /// UDP bloqué (pare-feu strict, réseau captif) : ni punch ni STUN.
    Blocked,
}

impl NatType {
    /// Heuristique : le hole punching a-t-il des chances raisonnables de
    /// réussir entre ce NAT et celui du pair ?
    ///
    /// Règles : UDP bloqué d'un côté → non ; deux NAT symétriques → non ;
    /// symétrique contre *port-restricted* → non (le port réflexif publié par
    /// le symétrique est faux, et le cone filtre précisément sur le port).
    /// Symétrique contre *full cone* ou *restricted* (filtrage IP seul) passe
    /// en général. Tout le reste → oui. En cas de « non » : relais `nd-relay`.
    #[must_use]
    pub fn punch_probable_avec(self, autre: NatType) -> bool {
        use NatType::{Blocked, PortRestricted, Symmetric};
        !matches!(
            (self, autre),
            (Blocked, _)
                | (_, Blocked)
                | (Symmetric, Symmetric)
                | (Symmetric, PortRestricted)
                | (PortRestricted, Symmetric)
        )
    }
}

impl std::fmt::Display for NatType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NatType::Open => "ouvert (pas de NAT)",
            NatType::FullCone => "full cone",
            NatType::Restricted => "cone restreint (IP)",
            NatType::PortRestricted => "cone restreint (IP:port)",
            NatType::Symmetric => "symétrique",
            NatType::Blocked => "bloqué (UDP filtré)",
        })
    }
}

/// Détecte (best-effort) le type de NAT en comparant les adresses réflexives
/// renvoyées par deux serveurs STUN **distincts** (idéalement d'opérateurs
/// différents), interrogés depuis la même socket UDP.
///
/// Ne renvoie jamais d'erreur : tout échec réseau dégrade la classification
/// ([`NatType::Blocked`] si aucun serveur ne répond). Voir la doc du module
/// pour les limites — notamment : tout NAT cone est rapporté
/// [`NatType::PortRestricted`] (hypothèse prudente).
#[must_use]
pub fn detect_nat_type(stun_a: SocketAddr, stun_b: SocketAddr) -> NatType {
    detecter(stun_a, stun_b, TIMEOUT_PAR_DEFAUT, TENTATIVES_PAR_DEFAUT)
}

/// Cœur de [`detect_nat_type`] avec timeout/tentatives réglables (tests).
fn detecter(stun_a: SocketAddr, stun_b: SocketAddr, timeout: Duration, tentatives: u32) -> NatType {
    // Une seule socket pour les deux serveurs : indispensable, deux sockets
    // auraient deux mappings NAT distincts et la comparaison ne dirait rien.
    let non_specifiee: SocketAddr = match stun_a {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let Ok(socket) = UdpSocket::bind(non_specifiee) else {
        return NatType::Blocked;
    };
    let reflexive_a = stun::decouvrir_par_socket(&socket, stun_a, timeout, tentatives).ok();
    let reflexive_b = stun::decouvrir_par_socket(&socket, stun_b, timeout, tentatives).ok();
    let locale = adresse_locale_effective(&socket, stun_a);
    classifier(locale, reflexive_a, reflexive_b)
}

/// Adresse locale « effective » de la socket : IP de l'interface de sortie
/// vers `reference` (découverte par un `connect` UDP sans trafic sur une
/// socket témoin) + port réellement lié. Nécessaire car la socket est liée à
/// l'adresse non spécifiée (`0.0.0.0`), inutilisable pour la comparaison.
fn adresse_locale_effective(socket: &UdpSocket, reference: SocketAddr) -> Option<SocketAddr> {
    let port = socket.local_addr().ok()?.port();
    let non_specifiee: SocketAddr = match reference {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let temoin = UdpSocket::bind(non_specifiee).ok()?;
    temoin.connect(reference).ok()?;
    Some(SocketAddr::new(temoin.local_addr().ok()?.ip(), port))
}

/// Classification pure à partir des observations (testable sans réseau).
///
/// - Aucune réflexive → [`NatType::Blocked`].
/// - Deux réflexives différentes → [`NatType::Symmetric`].
/// - Réflexive(s) concordante(s) égale(s) à l'adresse locale →
///   [`NatType::Open`] ; sinon NAT cone, rapporté
///   [`NatType::PortRestricted`] (hypothèse prudente, voir doc du module).
///   Une seule réflexive disponible : la stabilité du mapping n'est pas
///   vérifiable, même hypothèse prudente.
fn classifier(
    locale: Option<SocketAddr>,
    reflexive_a: Option<SocketAddr>,
    reflexive_b: Option<SocketAddr>,
) -> NatType {
    match (reflexive_a, reflexive_b) {
        (None, None) => NatType::Blocked,
        (Some(a), Some(b)) if a != b => NatType::Symmetric,
        (Some(reflexive), _) | (None, Some(reflexive)) => {
            if locale == Some(reflexive) {
                NatType::Open
            } else {
                NatType::PortRestricted
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn adr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    // --- Classification pure sur adresses réflexives forgées -------------

    #[test]
    fn classification_ouvert_reflexive_egale_locale() {
        let locale = adr("192.0.2.10:5000");
        assert_eq!(
            classifier(Some(locale), Some(locale), Some(locale)),
            NatType::Open
        );
        // Un seul serveur a répondu, mais la réflexive est l'adresse locale.
        assert_eq!(classifier(Some(locale), Some(locale), None), NatType::Open);
        assert_eq!(classifier(Some(locale), None, Some(locale)), NatType::Open);
    }

    #[test]
    fn classification_cone_reflexives_identiques() {
        let locale = adr("192.168.1.10:5000");
        let publique = adr("203.0.113.7:41000");
        assert_eq!(
            classifier(Some(locale), Some(publique), Some(publique)),
            NatType::PortRestricted
        );
        // Une seule réponse : comparaison impossible, hypothèse cone prudente.
        assert_eq!(
            classifier(Some(locale), Some(publique), None),
            NatType::PortRestricted
        );
        // Adresse locale indéterminée : on ne peut pas conclure « ouvert ».
        assert_eq!(
            classifier(None, Some(publique), Some(publique)),
            NatType::PortRestricted
        );
    }

    #[test]
    fn classification_symetrique_reflexives_differentes() {
        let locale = adr("192.168.1.10:5000");
        // Même IP publique mais ports différents : mapping par destination.
        assert_eq!(
            classifier(
                Some(locale),
                Some(adr("203.0.113.7:41000")),
                Some(adr("203.0.113.7:41017")),
            ),
            NatType::Symmetric
        );
        // IP publiques différentes (multi-WAN) : symétrique aussi.
        assert_eq!(
            classifier(
                Some(locale),
                Some(adr("203.0.113.7:41000")),
                Some(adr("198.51.100.2:41000")),
            ),
            NatType::Symmetric
        );
    }

    #[test]
    fn classification_bloque_sans_reponse() {
        assert_eq!(
            classifier(Some(adr("192.168.1.10:5000")), None, None),
            NatType::Blocked
        );
        assert_eq!(classifier(None, None, None), NatType::Blocked);
    }

    // --- Heuristique de compatibilité punch -------------------------------

    #[test]
    fn compatibilite_punch_entre_types() {
        use NatType::{Blocked, FullCone, Open, PortRestricted, Restricted, Symmetric};
        // Cas favorables.
        assert!(Open.punch_probable_avec(Symmetric));
        assert!(FullCone.punch_probable_avec(Symmetric));
        assert!(Restricted.punch_probable_avec(Symmetric));
        assert!(PortRestricted.punch_probable_avec(PortRestricted));
        assert!(Open.punch_probable_avec(Open));
        // Cas défavorables → relais.
        assert!(!Symmetric.punch_probable_avec(Symmetric));
        assert!(!Symmetric.punch_probable_avec(PortRestricted));
        assert!(!PortRestricted.punch_probable_avec(Symmetric));
        assert!(!Blocked.punch_probable_avec(Open));
        assert!(!Open.punch_probable_avec(Blocked));
    }

    // --- Détection de bout en bout contre des serveurs STUN simulés -------

    /// Serveur STUN simulé en loopback : répond à toute Binding Request par
    /// une réponse XOR-MAPPED-ADDRESS. `reponse` : `None` = renvoyer la
    /// source observée (comportement d'un vrai serveur), `Some(a)` = adresse
    /// forgée (simule la vue publique d'un NAT).
    fn serveur_stun_simule(reponse: Option<SocketAddr>) -> SocketAddr {
        const MAGIC_COOKIE: u32 = 0x2112_A442;
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let adresse = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut tampon = [0u8; 1500];
            while let Ok((n, source)) = socket.recv_from(&mut tampon) {
                if n < 20 {
                    continue;
                }
                let SocketAddr::V4(vue) = reponse.unwrap_or(source) else {
                    continue;
                };
                // Attribut XOR-MAPPED-ADDRESS (IPv4).
                let mut attrs = Vec::new();
                attrs.extend_from_slice(&0x0020u16.to_be_bytes());
                attrs.extend_from_slice(&8u16.to_be_bytes());
                attrs.push(0);
                attrs.push(0x01); // famille IPv4
                attrs.extend_from_slice(&(vue.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
                attrs.extend_from_slice(&(u32::from(*vue.ip()) ^ MAGIC_COOKIE).to_be_bytes());
                // En-tête Binding Success Response + transaction ID recopié.
                let mut rep = Vec::with_capacity(20 + attrs.len());
                rep.extend_from_slice(&0x0101u16.to_be_bytes());
                rep.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
                rep.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
                rep.extend_from_slice(&tampon[8..20]);
                rep.extend_from_slice(&attrs);
                let _ = socket.send_to(&rep, source);
            }
        });
        adresse
    }

    #[test]
    fn detection_ouvert_en_loopback() {
        // Les deux serveurs renvoient la source observée : en loopback la
        // réflexive est l'adresse locale → Open.
        let a = serveur_stun_simule(None);
        let b = serveur_stun_simule(None);
        assert_eq!(detecter(a, b, Duration::from_millis(500), 3), NatType::Open);
    }

    #[test]
    fn detection_cone_reflexive_forgee_stable() {
        let publique = adr("203.0.113.9:44000");
        let a = serveur_stun_simule(Some(publique));
        let b = serveur_stun_simule(Some(publique));
        assert_eq!(
            detecter(a, b, Duration::from_millis(500), 3),
            NatType::PortRestricted
        );
    }

    #[test]
    fn detection_symetrique_reflexives_forgees_differentes() {
        let a = serveur_stun_simule(Some(adr("203.0.113.9:44000")));
        let b = serveur_stun_simule(Some(adr("203.0.113.9:44777")));
        assert_eq!(
            detecter(a, b, Duration::from_millis(500), 3),
            NatType::Symmetric
        );
    }

    #[test]
    fn detection_bloque_sans_serveur() {
        // Sockets liées mais muettes (gardées vivantes pour que leurs ports
        // ne soient pas réattribués) : aucun serveur ne répond.
        let garde_a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let garde_b = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(
            detecter(
                garde_a.local_addr().unwrap(),
                garde_b.local_addr().unwrap(),
                Duration::from_millis(100),
                1,
            ),
            NatType::Blocked
        );
    }

    /// Détection réelle contre deux serveurs STUN publics. Ignorée par défaut
    /// (dépend du réseau) : `cargo test -p nd-signaling -- --ignored`.
    #[test]
    #[ignore = "dépend du réseau : interroge des serveurs STUN publics"]
    fn detection_reelle() {
        use std::net::ToSocketAddrs;
        let resoudre = |hote: &str| {
            hote.to_socket_addrs()
                .expect("résolution DNS")
                .find(SocketAddr::is_ipv4)
                .expect("aucune adresse IPv4")
        };
        let a = resoudre("stun.l.google.com:19302");
        let b = resoudre("stun.cloudflare.com:3478");
        let nat = detect_nat_type(a, b);
        println!("type de NAT détecté : {nat}");
    }
}
