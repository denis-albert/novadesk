//! Wake-on-LAN : construction et envoi du « paquet magique » — 6 octets de
//! synchronisation `0xFF` suivis de 16 répétitions de l'adresse MAC cible,
//! généralement diffusé en UDP vers l'adresse de broadcast, port 7 ou 9.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

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
}
