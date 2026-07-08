//! Wake-on-LAN : construction et envoi du « paquet magique » — 6 octets de
//! synchronisation `0xFF` suivis de 16 répétitions de l'adresse MAC cible,
//! généralement diffusé en UDP vers l'adresse de broadcast, port 7 ou 9.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::str::FromStr;

use nd_proto::{NdError, Result};

/// Taille d'un paquet magique : 6 octets `0xFF` + 16 × 6 octets de MAC.
pub const MAGIC_PACKET_LEN: usize = 102;

/// Construit le paquet magique Wake-on-LAN pour l'adresse MAC donnée.
#[must_use]
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut paquet = [0xFF_u8; MAGIC_PACKET_LEN];
    // Les 6 premiers octets restent 0xFF (synchronisation) ; les 96 suivants
    // reçoivent 16 copies de la MAC.
    for repetition in paquet[6..].chunks_exact_mut(6) {
        repetition.copy_from_slice(&mac);
    }
    paquet
}

/// Envoie le paquet magique en UDP vers `broadcast` (par exemple
/// `255.255.255.255:9` ou l'adresse de diffusion du sous-réseau).
///
/// Le socket est éphémère et ouvert avec `SO_BROADCAST` : l'appel fonctionne
/// aussi bien vers une adresse de diffusion que vers une adresse unicast
/// (utile pour les tests et les relais WoL).
pub fn wake_on_lan(mac: [u8; 6], broadcast: SocketAddr) -> Result<()> {
    // Socket local de la même famille d'adresses que la destination.
    let locale: SocketAddr = match broadcast {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(locale)?;
    socket.set_broadcast(true)?;

    let paquet = magic_packet(mac);
    let envoyes = socket.send_to(&paquet, broadcast)?;
    if envoyes != paquet.len() {
        return Err(NdError::Transport(format!(
            "paquet magique tronqué : {envoyes}/{MAGIC_PACKET_LEN} octets envoyés"
        )));
    }
    Ok(())
}

/// Port UDP « discard » (RFC 863), cible habituelle du Wake-on-LAN.
pub const WOL_PORT_DISCARD: u16 = 9;

/// Port UDP « echo » (RFC 862), cible alternative parfois utilisée.
pub const WOL_PORT_ECHO: u16 = 7;

/// Adresse MAC (48 bits) d'une carte réseau, cible d'un réveil Wake-on-LAN.
///
/// Se construit depuis six octets ([`MacAddr::new`] / `From<[u8; 6]>`) ou se
/// **parse** depuis les deux écritures canoniques, séparées par `:` ou `-`
/// (`"01:23:45:67:89:AB"`, `"01-23-45-67-89-ab"`), via [`FromStr`] /
/// [`MacAddr::parse`]. L'affichage rend la forme minuscule séparée par `:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    /// Construit une adresse à partir de ses six octets (ordre réseau).
    #[must_use]
    pub fn new(octets: [u8; 6]) -> Self {
        MacAddr(octets)
    }

    /// Les six octets de l'adresse, dans l'ordre réseau.
    #[must_use]
    pub fn octets(self) -> [u8; 6] {
        self.0
    }

    /// Parse une adresse MAC écrite avec séparateurs `:` ou `-`.
    ///
    /// Simple alias lisible de [`str::parse`] (voir [`FromStr`]).
    pub fn parse(texte: &str) -> Result<Self> {
        texte.parse()
    }
}

impl From<[u8; 6]> for MacAddr {
    fn from(octets: [u8; 6]) -> Self {
        MacAddr(octets)
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl FromStr for MacAddr {
    type Err = NdError;

    fn from_str(texte: &str) -> Result<Self> {
        let texte = texte.trim();
        // Séparateur accepté : ':' ou '-'. On exige un seul style et six groupes.
        let separateur = if texte.contains(':') {
            ':'
        } else if texte.contains('-') {
            '-'
        } else {
            return Err(NdError::Protocol(format!(
                "adresse MAC « {texte} » : séparateur ':' ou '-' attendu"
            )));
        };

        let mut octets = [0u8; 6];
        let mut lus = 0usize;
        for (indice, groupe) in texte.split(separateur).enumerate() {
            if indice >= 6 {
                return Err(NdError::Protocol(format!(
                    "adresse MAC « {texte} » : plus de six octets"
                )));
            }
            // Exactement deux chiffres hexadécimaux (rejette '+', '-', vide…).
            if groupe.len() != 2 || !groupe.bytes().all(|o| o.is_ascii_hexdigit()) {
                return Err(NdError::Protocol(format!(
                    "adresse MAC « {texte} » : « {groupe} » n'est pas un octet hexadécimal"
                )));
            }
            octets[indice] = u8::from_str_radix(groupe, 16).expect("deux chiffres hex validés");
            lus = indice + 1;
        }
        if lus != 6 {
            return Err(NdError::Protocol(format!(
                "adresse MAC « {texte} » : {lus} octet(s) au lieu de six"
            )));
        }
        Ok(MacAddr(octets))
    }
}

/// Adresse de **diffusion limitée** (`255.255.255.255`) sur `port`, cible par
/// défaut d'un Wake-on-LAN : le paquet ne franchit pas le routeur mais atteint
/// tout le sous-réseau local. Combiner avec [`WOL_PORT_DISCARD`] ou
/// [`WOL_PORT_ECHO`].
#[must_use]
pub fn limited_broadcast(port: u16) -> SocketAddr {
    (Ipv4Addr::BROADCAST, port).into()
}

/// Envoie le paquet magique Wake-on-LAN vers `broadcast` pour réveiller `mac`.
///
/// Enrobage ergonomique de [`wake_on_lan`] acceptant une [`MacAddr`] typée (ou
/// tout `[u8; 6]`). Le socket UDP éphémère est ouvert avec `SO_BROADCAST` ;
/// pour la cible usuelle, composer avec [`limited_broadcast`] :
/// `send_wol(mac, limited_broadcast(WOL_PORT_DISCARD))`.
pub fn send_wol(mac: impl Into<MacAddr>, broadcast: SocketAddr) -> Result<()> {
    wake_on_lan(mac.into().octets(), broadcast)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn structure_du_paquet_magique() {
        let mac = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB];
        let paquet = magic_packet(mac);

        // 102 octets : 6 de synchronisation + 16 répétitions de 6 octets.
        assert_eq!(paquet.len(), MAGIC_PACKET_LEN);
        assert!(paquet[..6].iter().all(|&octet| octet == 0xFF));
        let repetitions: Vec<&[u8]> = paquet[6..].chunks_exact(6).collect();
        assert_eq!(repetitions.len(), 16);
        for repetition in repetitions {
            assert_eq!(repetition, mac);
        }
    }

    #[test]
    fn paquet_magique_sans_reste() {
        // 6 + 16 × 6 = 102 : aucune place pour un « reste » après la 16e copie.
        let paquet = magic_packet([0u8; 6]);
        assert!(paquet[6..].chunks_exact(6).remainder().is_empty());
    }

    #[test]
    fn envoi_local_recu_intact() {
        // On « réveille » un récepteur local : aucune diffusion réelle requise.
        let recepteur = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        recepteur
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let destination = recepteur.local_addr().unwrap();

        let mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x42];
        wake_on_lan(mac, destination).unwrap();

        let mut tampon = [0u8; 256];
        let (recus, _) = recepteur.recv_from(&mut tampon).unwrap();
        assert_eq!(&tampon[..recus], &magic_packet(mac)[..]);
    }

    #[test]
    fn parse_mac_deux_formats() {
        let attendu = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB];
        assert_eq!(
            MacAddr::parse("01:23:45:67:89:AB").unwrap().octets(),
            attendu
        );
        assert_eq!(
            MacAddr::parse("01-23-45-67-89-ab").unwrap().octets(),
            attendu
        );
        // Insensible à la casse, espaces de bord tolérés.
        assert_eq!(
            "  0a:0B:0c:0D:0e:0F  ".parse::<MacAddr>().unwrap().octets(),
            [0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F]
        );
    }

    #[test]
    fn affichage_mac_est_reparsable() {
        let mac = MacAddr::new([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x42]);
        assert_eq!(mac.to_string(), "de:ad:be:ef:00:42");
        assert_eq!(MacAddr::parse(&mac.to_string()).unwrap(), mac);
    }

    #[test]
    fn parse_mac_refuse_les_entrees_invalides() {
        for mauvais in [
            "",
            "01:23:45:67:89",       // cinq octets
            "01:23:45:67:89:AB:CD", // sept octets
            "0123456789AB",         // sans séparateur
            "01:23:45:67:89:GG",    // non hexadécimal
            "1:23:45:67:89:AB",     // groupe d'un seul chiffre
            "01:23:45:67:89:+A",    // signe parasite dans un groupe
            "01-23:45-67:89-AB",    // séparateurs mélangés
        ] {
            assert!(
                mauvais.parse::<MacAddr>().is_err(),
                "« {mauvais} » aurait dû être refusé"
            );
        }
    }

    #[test]
    fn limited_broadcast_cible_255() {
        assert_eq!(
            limited_broadcast(WOL_PORT_DISCARD),
            "255.255.255.255:9".parse().unwrap()
        );
        assert_eq!(limited_broadcast(WOL_PORT_ECHO).port(), 7);
    }

    #[test]
    fn send_wol_via_mac_recu_intact() {
        // Réveil vers un récepteur local : pas de diffusion réelle nécessaire.
        let recepteur = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        recepteur
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let destination = recepteur.local_addr().unwrap();

        let mac = MacAddr::parse("de:ad:be:ef:00:42").unwrap();
        send_wol(mac, destination).unwrap();

        let mut tampon = [0u8; 256];
        let (recus, _) = recepteur.recv_from(&mut tampon).unwrap();
        assert_eq!(&tampon[..recus], &magic_packet(mac.octets())[..]);
    }
}
