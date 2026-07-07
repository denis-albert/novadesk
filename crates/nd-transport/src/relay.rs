//! Repli **relais** (plan 05) : QUIC tunnelé dans le relais TCP aveugle
//! (`nd-relay`) quand le P2P direct échoue (NAT symétrique, UDP filtré).
//!
//! # Principe
//!
//! Le relais n'accepte que du TCP : chaque pair annonce un **ticket**
//! (trame `[u32 BE len][ticket]`), le relais apparie les deux porteurs du
//! même ticket puis fait transiter les octets **sans les inspecter**. Pour
//! conserver exactement la même pile au-dessus (QUIC : chiffrement TLS 1.3,
//! épinglage du certificat, canaux, datagrammes), on **tunnelle les
//! datagrammes QUIC dans le flux TCP** :
//!
//! ```text
//!   endpoint QUIC local (127.0.0.1)          relais TCP           pair distant
//!   ────────────────────────────────         ──────────           ────────────
//!   datagramme UDP → socket-pont → trame [u16 BE len][octets] → TCP → … miroir …
//! ```
//!
//! Chaque côté crée un **pont** local : une socket UDP loopback qui joue le
//! rôle du pair pour quinn. Les datagrammes sortants de l'endpoint sont
//! encapsulés en trames TCP ; les trames entrantes sont réémises en
//! datagrammes vers l'endpoint. QUIC (handshake compris) traverse ainsi le
//! relais de bout en bout : le relais ne voit que des octets TLS 1.3 chiffrés,
//! l'appelant **épingle le certificat de l'appelé** exactement comme sur le
//! chemin direct. La perte de datagrammes disparaît (TCP), au prix de la
//! latence du relais — c'est un chemin de secours, pas le chemin nominal.
//!
//! # Rôles et tickets
//!
//! Comme sur le chemin direct, l'**appelé** est le serveur QUIC
//! ([`accept_via_relay`], avec l'identité dont le certificat est publié au
//! rendez-vous) et l'**appelant** le client ([`connect_via_relay`]). Les deux
//! doivent présenter le **même ticket** au relais. Le ticket est **opaque**
//! pour cette couche ; le serveur `nd-relay` de production ne l'accepte que
//! **signé Ed25519** par l'autorité du déploiement (portée = paire d'IDs,
//! expiration) — c'est le courtier de session du lot 07 qui l'émet et le
//! remet aux deux pairs. Les tests ci-dessous utilisent un relais de test
//! protocole-compatible sans vérification de signature.
//!
//! # Fin de vie
//!
//! La fermeture de la connexion QUIC (chute du transport, délai
//! d'inactivité) coupe le TCP du relais ; la coupure du TCP (relais arrêté,
//! pair parti, quota atteint) ferme l'endpoint QUIC — chaque sens débranche
//! l'autre, les threads du pont se terminent.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nd_proto::{NdError, Result};
use quinn::{Endpoint, VarInt};

use crate::quic::{
    accept_sur_endpoint, client_config, connect_sur_endpoint, endpoint_sur_socket, ensure_provider,
    runtime, server_config, spawn_transport, ServerIdentity,
};
use crate::{QuicTransport, Transport};

/// Taille maximale d'un ticket acceptée par `nd-relay` (annonce plus grande
/// rejetée) — reflétée ici pour échouer tôt, côté client.
const TAILLE_TICKET_MAX: usize = 1024;

/// Taille maximale d'un datagramme tunnelé (borne du préfixe u16 ; les
/// datagrammes QUIC réels restent ≤ MTU, très en deçà).
const MAX_DATAGRAMME: usize = 65_535;

/// Période de scrutation du sens montant du pont : borne le délai de sortie
/// du thread quand le tunnel meurt sans trafic.
const PERIODE_SCRUTATION: Duration = Duration::from_millis(250);

/// Annonce le ticket au relais : trame `[u32 BE len][ticket]` (protocole
/// `nd-relay`).
fn annoncer_ticket(tcp: &mut TcpStream, ticket: &[u8]) -> Result<()> {
    if ticket.is_empty() || ticket.len() > TAILLE_TICKET_MAX {
        return Err(NdError::Transport(format!(
            "ticket de relais invalide ({} octets, maximum {TAILLE_TICKET_MAX})",
            ticket.len()
        )));
    }
    tcp.write_all(&(ticket.len() as u32).to_be_bytes())?;
    tcp.write_all(ticket)?;
    Ok(())
}

/// Monte le pont UDP⇆TCP : renvoie l'adresse de la socket-pont (le « pair »
/// que voit l'endpoint quinn). Deux threads sont lancés :
///
/// * **montant** : datagrammes reçus de l'endpoint → trames TCP ;
/// * **descendant** : trames TCP → datagrammes vers l'endpoint.
///
/// Quand un sens meurt (TCP coupé, erreur socket), il ferme l'endpoint et
/// lève le drapeau d'arrêt : l'autre sens se termine à son tour (scrutation).
fn monter_pont(tcp: TcpStream, endpoint: &Endpoint) -> Result<SocketAddr> {
    let adresse_endpoint = endpoint
        .local_addr()
        .map_err(|e| NdError::Transport(format!("adresse de l'endpoint : {e}")))?;
    let pont = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    let adresse_pont = pont.local_addr()?;
    let arret = Arc::new(AtomicBool::new(false));

    // Sens montant : endpoint → relais.
    let mut tcp_montant = tcp.try_clone()?;
    let pont_montant = pont.try_clone()?;
    let arret_montant = Arc::clone(&arret);
    let endpoint_montant = endpoint.clone();
    std::thread::spawn(move || {
        let _ = pont_montant.set_read_timeout(Some(PERIODE_SCRUTATION));
        let mut tampon = [0u8; MAX_DATAGRAMME];
        while !arret_montant.load(Ordering::Relaxed) {
            match pont_montant.recv_from(&mut tampon) {
                Ok((n, _)) => {
                    // n ≤ 65 535 par construction du tampon : le préfixe u16 suffit.
                    let prefixe = (n as u16).to_be_bytes();
                    if tcp_montant.write_all(&prefixe).is_err()
                        || tcp_montant.write_all(&tampon[..n]).is_err()
                    {
                        break;
                    }
                }
                // Fenêtre de scrutation écoulée : on revérifie le drapeau.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                // ICMP remonté par Windows ou socket close : selon le cas on
                // continue (parasite) — le drapeau d'arrêt fait autorité.
                Err(_) => {}
            }
        }
        arret_montant.store(true, Ordering::Relaxed);
        endpoint_montant.close(VarInt::from_u32(0), b"tunnel relais coupe");
        let _ = tcp_montant.shutdown(Shutdown::Both);
    });

    // Sens descendant : relais → endpoint.
    let mut tcp_descendant = tcp;
    let arret_descendant = Arc::clone(&arret);
    let endpoint_descendant = endpoint.clone();
    std::thread::spawn(move || {
        let mut prefixe = [0u8; 2];
        let mut tampon = vec![0u8; MAX_DATAGRAMME];
        loop {
            if tcp_descendant.read_exact(&mut prefixe).is_err() {
                break;
            }
            let n = usize::from(u16::from_be_bytes(prefixe));
            if n == 0 {
                continue;
            }
            if tcp_descendant.read_exact(&mut tampon[..n]).is_err() {
                break;
            }
            if pont.send_to(&tampon[..n], adresse_endpoint).is_err() {
                break;
            }
        }
        arret_descendant.store(true, Ordering::Relaxed);
        endpoint_descendant.close(VarInt::from_u32(0), b"tunnel relais coupe");
    });

    Ok(adresse_pont)
}

/// À la fermeture de la connexion QUIC (quelle qu'en soit la cause), coupe le
/// TCP du relais : le relais voit la fin de session et libère la paire, les
/// threads du pont se débranchent.
fn couper_tcp_a_la_fermeture(transport: &QuicTransport, tcp: TcpStream) {
    let connexion = transport.connection();
    runtime().spawn(async move {
        let _ = connexion.closed().await;
        let _ = tcp.shutdown(Shutdown::Both);
    });
}

/// Se connecte au pair **via le relais** (repli quand le punch échoue) :
/// annonce `ticket` au relais TCP, tunnelle QUIC dedans et épingle
/// `server_cert_der` (le certificat de l'appelé, résolu via le rendez-vous —
/// le même que sur le chemin direct).
///
/// Bloque jusqu'au handshake QUIC, qui ne peut aboutir qu'une fois le pair
/// appelé présenté au relais avec le même ticket ([`accept_via_relay`]) ; en
/// son absence, échoue au délai d'inactivité QUIC.
///
/// Le ticket est opaque ici ; `nd-relay` exige un ticket **signé** émis par
/// le courtier de session (lot 07) et remis identique aux deux pairs.
///
/// # Errors
/// Erreur si le relais est injoignable, si le ticket est vide ou trop grand
/// (> 1024 octets), ou si le handshake QUIC échoue (pair absent, ticket
/// refusé par le relais, certificat non conforme à l'épinglage).
pub fn connect_via_relay(
    relay_addr: SocketAddr,
    ticket: &[u8],
    server_cert_der: &[u8],
) -> Result<Box<dyn Transport>> {
    Ok(Box::new(connect_quic_via_relay(
        relay_addr,
        ticket,
        server_cert_der,
    )?))
}

/// Comme [`connect_via_relay`], mais renvoie le type concret.
///
/// # Errors
/// Voir [`connect_via_relay`].
pub fn connect_quic_via_relay(
    relay_addr: SocketAddr,
    ticket: &[u8],
    server_cert_der: &[u8],
) -> Result<QuicTransport> {
    ensure_provider();
    let client_cfg = client_config(server_cert_der)?;
    let mut tcp = TcpStream::connect(relay_addr)?;
    annoncer_ticket(&mut tcp, ticket)?;
    let tcp_garde = tcp.try_clone()?;

    let resultat = runtime().block_on(async move {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let mut endpoint = endpoint_sur_socket(socket, None)?;
        endpoint.set_default_client_config(client_cfg);
        let pair_fictif = monter_pont(tcp, &endpoint)?;
        connect_sur_endpoint(endpoint, pair_fictif).await
    });
    let (endpoint, conn, send, recv) = match resultat {
        Ok(v) => v,
        Err(e) => {
            // Débranche le pont (sinon ses threads survivraient au tunnel mort).
            let _ = tcp_garde.shutdown(Shutdown::Both);
            return Err(e);
        }
    };
    let transport = spawn_transport(conn, Some(endpoint), send, recv);
    couper_tcp_a_la_fermeture(&transport, tcp_garde);
    Ok(transport)
}

/// Attend le pair **via le relais**, côté appelé : annonce `ticket`, tunnelle
/// QUIC et accepte une connexion en présentant `identity` — la même identité
/// que l'écouteur direct et les sockets percées, pour que l'épinglage de
/// l'appelant fonctionne à l'identique.
///
/// Bloque jusqu'au handshake (appeler depuis un thread dédié, comme
/// [`crate::Listener::accept`]) ; si le relais coupe (ticket refusé, quota),
/// l'endpoint est fermé et l'appel échoue.
///
/// # Errors
/// Erreur si le relais est injoignable, si le ticket est vide ou trop grand,
/// ou si le tunnel se ferme avant/pendant le handshake.
pub fn accept_via_relay(
    relay_addr: SocketAddr,
    ticket: &[u8],
    identity: &ServerIdentity,
) -> Result<Box<dyn Transport>> {
    Ok(Box::new(accept_quic_via_relay(
        relay_addr, ticket, identity,
    )?))
}

/// Comme [`accept_via_relay`], mais renvoie le type concret.
///
/// # Errors
/// Voir [`accept_via_relay`].
pub fn accept_quic_via_relay(
    relay_addr: SocketAddr,
    ticket: &[u8],
    identity: &ServerIdentity,
) -> Result<QuicTransport> {
    ensure_provider();
    let server_cfg = server_config(identity)?;
    let mut tcp = TcpStream::connect(relay_addr)?;
    annoncer_ticket(&mut tcp, ticket)?;
    let tcp_garde = tcp.try_clone()?;

    let resultat = runtime().block_on(async move {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let endpoint = endpoint_sur_socket(socket, Some(server_cfg))?;
        let _pair_fictif = monter_pont(tcp, &endpoint)?;
        accept_sur_endpoint(endpoint).await
    });
    let (endpoint, conn, send, recv) = match resultat {
        Ok(v) => v,
        Err(e) => {
            let _ = tcp_garde.shutdown(Shutdown::Both);
            return Err(e);
        }
    };
    let transport = spawn_transport(conn, Some(endpoint), send, recv);
    couper_tcp_a_la_fermeture(&transport, tcp_garde);
    Ok(transport)
}

// ---------------------------------------------------------------------------
// Tests — relais de test protocole-compatible (`[u32 len][ticket]` + tuyau
// aveugle), sans vérification de signature (celle du vrai nd-relay, lot 07).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nd_proto::{ChannelKind, Reliability};
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Instant;

    /// Mini-relais aveugle : apparie deux connexions sur les octets exacts du
    /// ticket, puis copie les octets dans les deux sens sans les regarder.
    /// Réplique le protocole d'annonce de `nd-relay` (sans tickets signés).
    fn mini_relais() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind relais");
        let adresse = listener.local_addr().expect("adresse relais");
        let en_attente: Arc<Mutex<HashMap<Vec<u8>, TcpStream>>> = Arc::default();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let table = Arc::clone(&en_attente);
                thread::spawn(move || {
                    // Annonce : [u32 BE len][ticket].
                    let mut prefixe = [0u8; 4];
                    if stream.read_exact(&mut prefixe).is_err() {
                        return;
                    }
                    let longueur = u32::from_be_bytes(prefixe) as usize;
                    if longueur == 0 || longueur > 1024 {
                        return;
                    }
                    let mut ticket = vec![0u8; longueur];
                    if stream.read_exact(&mut ticket).is_err() {
                        return;
                    }
                    let paire = {
                        let mut table = table.lock().unwrap();
                        match table.remove(&ticket) {
                            None => {
                                table.insert(ticket, stream);
                                return; // premier pair : en attente
                            }
                            Some(premier) => premier,
                        }
                    };
                    // Second pair : tuyau aveugle bidirectionnel.
                    let mut aller_src = stream.try_clone().expect("clone");
                    let mut aller_dst = paire.try_clone().expect("clone");
                    let mut retour_src = paire;
                    let mut retour_dst = stream;
                    thread::spawn(move || {
                        let _ = std::io::copy(&mut aller_src, &mut aller_dst);
                        let _ = aller_dst.shutdown(Shutdown::Both);
                    });
                    let _ = std::io::copy(&mut retour_src, &mut retour_dst);
                    let _ = retour_dst.shutdown(Shutdown::Both);
                });
            }
        });
        adresse
    }

    /// Draine `poll_recv` jusqu'au prochain message ou à l'expiration.
    fn attendre_message(
        transport: &mut QuicTransport,
        timeout: Duration,
    ) -> Option<(crate::ChannelHandle, Vec<u8>)> {
        let debut = Instant::now();
        while debut.elapsed() < timeout {
            if let Some(message) = transport.poll_recv().expect("poll_recv") {
                return Some(message);
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    /// Chemin de repli complet : QUIC (handshake + épinglage + canaux)
    /// traverse le relais aveugle dans les deux sens.
    #[test]
    fn quic_traverse_le_relais_de_bout_en_bout() {
        let relais = mini_relais();
        let identite = ServerIdentity::generate().expect("identité");
        let cert = identite.cert_der().to_vec();
        let ticket = b"ticket-paire-42".to_vec();

        let ticket_appele = ticket.clone();
        let appele = thread::spawn(move || {
            accept_quic_via_relay(relais, &ticket_appele, &identite).expect("accept_via_relay")
        });
        let mut appelant =
            connect_quic_via_relay(relais, &ticket, &cert).expect("connect_via_relay");
        let mut appele = appele.join().expect("thread appelé");
        assert!(appelant.is_connected());
        assert!(appele.is_connected());

        // Aller : plusieurs messages fiables.
        let h = appelant.open_channel(ChannelKind::Control);
        for i in 0..5u8 {
            appelant
                .send(
                    h,
                    vec![b'r', b'e', b'l', b'a', b'i', i],
                    Reliability::Reliable,
                )
                .expect("send appelant");
        }
        for i in 0..5u8 {
            let (_, data) =
                attendre_message(&mut appele, Duration::from_secs(5)).expect("message aller");
            assert_eq!(data, vec![b'r', b'e', b'l', b'a', b'i', i]);
        }

        // Retour.
        let h = appele.open_channel(ChannelKind::Control);
        appele
            .send(h, b"bien recu".to_vec(), Reliability::Reliable)
            .expect("send appelé");
        let (_, data) =
            attendre_message(&mut appelant, Duration::from_secs(5)).expect("message retour");
        assert_eq!(data, b"bien recu");
    }

    /// La chute d'un côté ferme la session de l'autre (le pont propage la
    /// fin de vie à travers le relais).
    #[test]
    fn fermeture_propagee_a_travers_le_relais() {
        let relais = mini_relais();
        let identite = ServerIdentity::generate().expect("identité");
        let cert = identite.cert_der().to_vec();
        let ticket = b"ticket-fin-de-vie".to_vec();

        let ticket_appele = ticket.clone();
        let appele = thread::spawn(move || {
            accept_quic_via_relay(relais, &ticket_appele, &identite).expect("accept_via_relay")
        });
        let appelant = connect_quic_via_relay(relais, &ticket, &cert).expect("connect_via_relay");
        let appele = appele.join().expect("thread appelé");

        let (tx, rx) = std::sync::mpsc::channel();
        appele.on_disconnect(move |raison| {
            let _ = tx.send(raison);
        });
        drop(appelant);
        let raison = rx
            .recv_timeout(Duration::from_secs(15))
            .expect("coupure vue à travers le relais");
        assert!(!raison.is_empty());
        assert!(!appele.is_connected());
    }

    /// Un ticket hors bornes est refusé côté client, avant tout réseau.
    #[test]
    fn ticket_hors_bornes_refuse() {
        let relais = mini_relais();
        let identite = ServerIdentity::generate().expect("identité");
        assert!(connect_quic_via_relay(relais, &[], identite.cert_der()).is_err());
        let trop_grand = vec![0u8; 1025];
        assert!(connect_quic_via_relay(relais, &trop_grand, identite.cert_der()).is_err());
    }
}
