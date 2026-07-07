//! Connecteur **P2P par ID** — câble le rendez-vous, STUN et le hole punching
//! en un chemin de bout en bout (plan 05).
//!
//! # Vue d'ensemble
//!
//! ```text
//!  appelant (contrôleur)                 rendez-vous                appelé (contrôlé)
//!  ─────────────────────                 ───────────                ─────────────────
//!                                                       register + await_p2p :
//!                                            ◄─────────  socket UDP + STUN
//!                                            ◄─────────  publish_candidates
//!  establish_p2p :
//!  socket UDP + STUN
//!  request_punch  ────────────────────►  mémorise la demande,
//!                 ◄────────────────────  renvoie les candidats de la cible
//!                                            ◄─────────  poll_punch (relève)
//!  udp_hole_punch(Caller)  ◄═══ sondes UDP simultanées ═══►  udp_hole_punch(Callee)
//!  ConnAttempt::Direct                                        P2pIncoming::Direct
//!         │                                                          │
//!         ▼                                                          ▼
//!  nd-transport::connect_over_socket            nd-transport::accept_over_socket
//!  (QUIC client, certificat épinglé)            (QUIC serveur, même identité que
//!                                                le certificat publié au register)
//! ```
//!
//! Ce crate s'arrête au **socket UDP percé** : porter QUIC dessus est le rôle
//! de `nd-transport` (`connect_over_socket` / `accept_over_socket`), ce qui
//! garde `nd-signaling` sans dépendance transport. `nd-core` (lot 01) est le
//! chef d'orchestre : il appelle [`establish_p2p`] côté contrôleur,
//! [`await_p2p`] côté contrôlé, puis tend le socket à `nd-transport`.
//!
//! # Repli relais
//!
//! Quand le punch est impossible (pair sans candidats) ou échoue (timeout —
//! NAT symétrique des deux côtés, UDP filtré), le connecteur renvoie
//! [`ConnAttempt::RelayFallback`] / [`P2pIncoming::RelayFallback`] plutôt
//! qu'une erreur : l'appelant y trouve le certificat du pair (déjà résolu) et
//! le motif, et peut basculer sur `nd-transport::connect_via_relay` /
//! `accept_via_relay` avec un **ticket** remis par le courtier de session.
//! Le ticket est opaque pour cette couche ; sa **signature** (Ed25519,
//! vérifiée par `nd-relay`) est fournie par le lot 07 — le rendez-vous
//! non authentifié utilisé ici ne sait pas en émettre.
//!
//! # Honnêteté sur la couverture
//!
//! Tout ce chemin est exercé en boucle locale (tests) et via de vraies
//! adresses d'interface sur une même machine (sonde
//! `examples/p2p_two_process.rs`) : la traversée d'un **vrai NAT** dépend du
//! type de NAT (voir [`crate::punch`] et [`crate::nat`]) et n'est pas
//! testable sur une seule machine.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use nd_proto::{NdError, NovaId, Result};

use crate::punch::{self, PunchRole, DEFAULT_PUNCH_TIMEOUT};
use crate::{nat, stun, RendezvousClient};

/// Intervalle de relève des demandes de punch côté appelé ([`await_p2p`]).
/// Court devant [`crate::PUNCH_TTL`] et devant le timeout de punch de
/// l'appelant : la fenêtre de simultanéité est largement tenue.
const INTERVALLE_POLL: Duration = Duration::from_millis(150);

/// Timeout STUN par tentative pendant l'établissement (plus court que celui
/// du client STUN générique : un serveur STUN mort ne doit pas manger la
/// fenêtre de punch).
const STUN_TIMEOUT: Duration = Duration::from_millis(800);

/// Tentatives STUN par serveur pendant l'établissement.
const STUN_TENTATIVES: u32 = 2;

/// Chemin direct percé, prêt à porter QUIC (côté appelant).
#[derive(Debug)]
pub struct DirectPath {
    /// Socket UDP percée (mapping NAT ouvert des deux côtés). À passer telle
    /// quelle à `nd-transport::connect_over_socket` — le rebinder détruirait
    /// le mapping.
    pub socket: UdpSocket,
    /// Adresse du pair **confirmée par le punch** (source observée ; peut
    /// différer des candidats si le NAT a réécrit le port).
    pub peer_addr: SocketAddr,
    /// Certificat DER du pair (résolu via `lookup`), à épingler pour QUIC.
    pub peer_cert_der: Vec<u8>,
}

/// Issue d'une tentative d'établissement P2P côté appelant ([`establish_p2p`]).
#[derive(Debug)]
pub enum ConnAttempt {
    /// Punch réussi : chemin direct prêt pour QUIC.
    Direct(DirectPath),
    /// Punch impossible ou échoué : se replier sur le relais
    /// (`nd-transport::connect_via_relay`) avec un ticket du courtier de
    /// session (signé — lot 07). Le certificat du pair, déjà résolu, reste
    /// nécessaire pour épingler la session QUIC tunnelée.
    RelayFallback {
        /// Certificat DER du pair (résolu via `lookup`).
        peer_cert_der: Vec<u8>,
        /// Motif du repli (diagnostic, journalisation).
        reason: String,
    },
}

/// Chemin direct percé, prêt à porter QUIC (côté appelé).
#[derive(Debug)]
pub struct IncomingPath {
    /// ID du pair appelant (celui de la demande de punch relevée).
    pub from: NovaId,
    /// Socket UDP percée. À passer telle quelle à
    /// `nd-transport::accept_over_socket` avec l'identité TLS **dont le
    /// certificat a été publié au `register`** (l'appelant l'épingle).
    pub socket: UdpSocket,
    /// Adresse de l'appelant confirmée par le punch.
    pub peer_addr: SocketAddr,
}

/// Issue d'une attente de connexion P2P côté appelé ([`await_p2p`]).
#[derive(Debug)]
pub enum P2pIncoming {
    /// Punch réussi : chemin direct prêt pour QUIC.
    Direct(IncomingPath),
    /// Demande relevée mais punch impossible/échoué : l'appelant basculera
    /// sur le relais — l'appelé doit s'y présenter aussi
    /// (`nd-transport::accept_via_relay`) avec le même ticket.
    RelayFallback {
        /// ID du pair appelant dont la demande a échoué.
        from: NovaId,
        /// Motif du repli (diagnostic, journalisation).
        reason: String,
    },
}

/// Établit un chemin P2P vers `peer_id` (côté **appelant**) : résout le pair,
/// découvre l'adresse réflexive (STUN), dépose la demande de punch au
/// rendez-vous et lance [`punch::udp_hole_punch`] (rôle
/// [`PunchRole::Caller`]) vers les candidats de la cible.
///
/// Renvoie [`ConnAttempt::Direct`] avec le socket percé, l'adresse confirmée
/// et le certificat du pair — prêts pour
/// `nd-transport::connect_over_socket` — ou [`ConnAttempt::RelayFallback`]
/// si le punch est impossible (pair sans candidats publiés : il n'est pas en
/// attente via [`await_p2p`]) ou échoue (timeout [`DEFAULT_PUNCH_TIMEOUT`]).
///
/// `stun_servers` : serveurs STUN interrogés **depuis le socket de punch**
/// (le premier qui répond fournit l'adresse réflexive). Une liste vide, ou
/// des serveurs muets, limitent les candidats à l'adresse locale — suffisant
/// en LAN/boucle locale, sans espoir à travers un NAT.
///
/// La cible doit être **en attente** ([`await_p2p`]) au moment de l'appel :
/// les deux punchs doivent être simultanés (fenêtre [`crate::PUNCH_TTL`]).
///
/// # Errors
/// Erreur si le pair est introuvable/hors-ligne, si aucun candidat local ne
/// peut être construit, ou en cas d'erreur réseau/protocole avec le
/// rendez-vous. L'échec du **punch** n'est pas une erreur : c'est
/// [`ConnAttempt::RelayFallback`].
pub fn establish_p2p(
    rv: &RendezvousClient,
    local_id: NovaId,
    peer_id: NovaId,
    stun_servers: &[SocketAddr],
) -> Result<ConnAttempt> {
    establish_p2p_with_timeout(rv, local_id, peer_id, stun_servers, DEFAULT_PUNCH_TIMEOUT)
}

/// Variante de [`establish_p2p`] avec un timeout de punch explicite.
///
/// # Errors
/// Voir [`establish_p2p`].
pub fn establish_p2p_with_timeout(
    rv: &RendezvousClient,
    local_id: NovaId,
    peer_id: NovaId,
    stun_servers: &[SocketAddr],
    punch_timeout: Duration,
) -> Result<ConnAttempt> {
    // 1. Résolution de l'ID : adresse publiée + certificat à épingler.
    let record = rv.lookup(peer_id)?;

    // 2. Socket de punch + candidats (adresse locale effective + réflexive STUN).
    let (socket, candidats_locaux) = preparer_socket_et_candidats(rv.server_addr(), stun_servers)?;

    // 3. Demande de punch : dépose nos candidats, récupère ceux de la cible.
    let candidats_cible = rv.request_punch(local_id, peer_id, &candidats_locaux)?;
    if candidats_cible.is_empty() {
        return Ok(ConnAttempt::RelayFallback {
            peer_cert_der: record.cert_der,
            reason: format!(
                "le pair {peer_id} n'a publié aucun candidat de punch \
                 (pas en attente via await_p2p ?)"
            ),
        });
    }

    // 4. Punch simultané (la cible relève la demande dans sa boucle de poll).
    match punch::udp_hole_punch_with_timeout(
        socket,
        &candidats_cible,
        PunchRole::Caller,
        punch_timeout,
    ) {
        Ok((socket, peer_addr)) => Ok(ConnAttempt::Direct(DirectPath {
            socket,
            peer_addr,
            peer_cert_der: record.cert_der,
        })),
        Err(e) => Ok(ConnAttempt::RelayFallback {
            peer_cert_der: record.cert_der,
            reason: e.to_string(),
        }),
    }
}

/// Attend une connexion P2P entrante (côté **appelé**, le pendant de
/// [`establish_p2p`]) : découvre l'adresse réflexive (STUN), publie les
/// candidats sous `local_id`, puis relève périodiquement les demandes de
/// punch ([`RendezvousClient::poll_punch`]) jusqu'à `wait_timeout`. À la
/// première demande, lance [`punch::udp_hole_punch`] (rôle
/// [`PunchRole::Callee`]) vers les candidats de l'appelant.
///
/// Renvoie [`P2pIncoming::Direct`] avec le socket percé — prêt pour
/// `nd-transport::accept_over_socket` avec l'identité TLS dont le certificat
/// a été publié au `register` — ou [`P2pIncoming::RelayFallback`] si le punch
/// échoue (l'appelant bascule sur le relais de son côté).
///
/// `local_id` doit être **déjà enregistré** ([`RendezvousClient::register`])
/// et maintenu en vie (heartbeat) : publier des candidats exige un ID en
/// ligne. Appeler en boucle (un socket neuf et des candidats frais sont
/// publiés à chaque appel) — typiquement dans le thread d'attente de
/// connexions du pair contrôlé. Si plusieurs demandes sont en attente, seule
/// la **première** est servie ; les autres appelants retomberont sur le
/// relais ou réessaieront (limitation documentée de ce premier jet).
///
/// # Errors
/// Erreur si aucune demande n'arrive avant `wait_timeout`, si l'ID n'est pas
/// (ou plus) enregistré, si aucun candidat local ne peut être construit, ou
/// en cas d'erreur réseau/protocole. L'échec du **punch** n'est pas une
/// erreur : c'est [`P2pIncoming::RelayFallback`].
pub fn await_p2p(
    rv: &RendezvousClient,
    local_id: NovaId,
    stun_servers: &[SocketAddr],
    wait_timeout: Duration,
) -> Result<P2pIncoming> {
    await_p2p_with_timeout(
        rv,
        local_id,
        stun_servers,
        wait_timeout,
        DEFAULT_PUNCH_TIMEOUT,
    )
}

/// Variante de [`await_p2p`] avec un timeout de punch explicite.
///
/// # Errors
/// Voir [`await_p2p`].
pub fn await_p2p_with_timeout(
    rv: &RendezvousClient,
    local_id: NovaId,
    stun_servers: &[SocketAddr],
    wait_timeout: Duration,
    punch_timeout: Duration,
) -> Result<P2pIncoming> {
    let (socket, candidats) = preparer_socket_et_candidats(rv.server_addr(), stun_servers)?;
    rv.publish_candidates(local_id, &candidats)?;

    let debut = Instant::now();
    let demande = loop {
        let mut demandes = rv.poll_punch(local_id)?;
        if !demandes.is_empty() {
            // Première demande servie ; les suivantes de la même relève sont
            // perdues (voir doc) — le rendez-vous a déjà vidé la file.
            break demandes.remove(0);
        }
        if debut.elapsed() >= wait_timeout {
            return Err(NdError::Protocol(format!(
                "aucune demande de punch pour {local_id} après {wait_timeout:?}"
            )));
        }
        std::thread::sleep(INTERVALLE_POLL);
    };

    if demande.candidates.is_empty() {
        return Ok(P2pIncoming::RelayFallback {
            from: demande.from,
            reason: "demande de punch sans candidats".into(),
        });
    }
    match punch::udp_hole_punch_with_timeout(
        socket,
        &demande.candidates,
        PunchRole::Callee,
        punch_timeout,
    ) {
        Ok((socket, peer_addr)) => Ok(P2pIncoming::Direct(IncomingPath {
            from: demande.from,
            socket,
            peer_addr,
        })),
        Err(e) => Ok(P2pIncoming::RelayFallback {
            from: demande.from,
            reason: e.to_string(),
        }),
    }
}

/// Prépare le socket de punch et ses candidats : bind sur l'adresse non
/// spécifiée (famille de `reference`), candidat local = IP de l'interface de
/// sortie vers `reference` + port lié, candidat réflexif = première réponse
/// STUN obtenue **depuis ce socket** (indispensable : l'adresse réflexive
/// doit correspondre au mapping NAT du socket qui va percer).
fn preparer_socket_et_candidats(
    reference: SocketAddr,
    stun_servers: &[SocketAddr],
) -> Result<(UdpSocket, Vec<SocketAddr>)> {
    let non_specifiee: SocketAddr = match reference {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(non_specifiee)?;

    let mut candidats = Vec::with_capacity(2);
    if let Some(locale) = nat::adresse_locale_effective(&socket, reference) {
        candidats.push(locale);
    }
    // Le premier serveur STUN qui répond suffit (les suivants sont du secours).
    for serveur in stun_servers {
        match stun::decouvrir_par_socket(&socket, *serveur, STUN_TIMEOUT, STUN_TENTATIVES) {
            Ok(reflexive) => {
                if !candidats.contains(&reflexive) {
                    candidats.push(reflexive);
                }
                break;
            }
            Err(_) => continue,
        }
    }
    // La découverte STUN laisse un timeout de lecture : on le retire, le
    // punch (puis QUIC) gèrent les leurs.
    socket.set_read_timeout(None)?;

    if candidats.is_empty() {
        return Err(NdError::Protocol(
            "aucun candidat de punch constructible (adresse locale indéterminée \
             et aucun serveur STUN n'a répondu)"
                .into(),
        ));
    }
    Ok((socket, candidats))
}

// ---------------------------------------------------------------------------
// Tests (boucle locale — voir la doc du module pour les limites NAT)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{serve, Registry};
    use std::net::TcpListener;

    /// Démarre un rendez-vous éphémère et rend un client par pair.
    fn rendezvous_de_test() -> (RendezvousClient, RendezvousClient) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let registry = Registry::new();
        std::thread::spawn(move || {
            let _ = serve(listener, registry);
        });
        (RendezvousClient::new(addr), RendezvousClient::new(addr))
    }

    fn adresse_bidon() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    /// Chaîne complète en boucle locale : annonce, demande, punch simultané,
    /// puis échange applicatif sur les sockets percées.
    #[test]
    fn etablissement_p2p_de_bout_en_bout() {
        let (rv_appelant, rv_appele) = rendezvous_de_test();
        let appelant = NovaId(1001);
        let appele = NovaId(2002);
        rv_appele
            .register(appele, adresse_bidon(), &[7, 7])
            .unwrap();

        let cote_appele = std::thread::spawn(move || {
            await_p2p(&rv_appele, appele, &[], Duration::from_secs(10)).expect("await_p2p")
        });

        // L'appelant réessaie tant que l'appelé n'a pas publié ses candidats
        // (la publication arrive au début d'await_p2p, d'un autre thread).
        let tentative = loop {
            match establish_p2p(&rv_appelant, appelant, appele, &[]).expect("establish_p2p") {
                ConnAttempt::RelayFallback { reason, .. } if reason.contains("aucun candidat") => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                autre => break autre,
            }
        };
        let ConnAttempt::Direct(direct) = tentative else {
            panic!("punch attendu en boucle locale");
        };
        assert_eq!(direct.peer_cert_der, vec![7, 7], "certificat via lookup");

        let entrant = cote_appele.join().unwrap();
        let P2pIncoming::Direct(entrant) = entrant else {
            panic!("punch attendu côté appelé");
        };
        assert_eq!(entrant.from, appelant);

        // Les adresses confirmées se croisent : chaque pair voit le port du
        // socket de l'autre. (Les sockets étant liées à 0.0.0.0, seule la
        // comparaison des ports a un sens — l'IP locale effective est
        // 127.0.0.1, celle observée par le punch.)
        assert_eq!(
            direct.peer_addr.port(),
            entrant.socket.local_addr().unwrap().port()
        );
        assert_eq!(
            entrant.peer_addr.port(),
            direct.socket.local_addr().unwrap().port()
        );
        assert!(direct.peer_addr.ip().is_loopback());
        assert!(entrant.peer_addr.ip().is_loopback());

        // Échange applicatif sur le chemin percé (sondes résiduelles filtrées).
        direct.socket.send_to(b"salut", direct.peer_addr).unwrap();
        entrant
            .socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut tampon = [0u8; 64];
        loop {
            let (n, _) = entrant.socket.recv_from(&mut tampon).unwrap();
            if punch::est_paquet_punch(&tampon[..n]) {
                continue;
            }
            assert_eq!(&tampon[..n], b"salut");
            break;
        }
    }

    /// L'établissement passe aussi par un serveur STUN (simulé) : le candidat
    /// réflexif forgé est mort, le candidat local sauve le punch — preuve que
    /// plusieurs candidats sont réellement sondés.
    #[test]
    fn etablissement_avec_candidat_stun_mort_et_local_vivant() {
        let (rv_appelant, rv_appele) = rendezvous_de_test();
        let appelant = NovaId(1);
        let appele = NovaId(2);
        rv_appele.register(appele, adresse_bidon(), &[1]).unwrap();

        // STUN simulé qui forge une « vue publique » injoignable : le
        // connecteur publie [locale, réflexive forgée].
        let stun_forge = serveur_stun_forge("203.0.113.44:41000".parse().unwrap());

        let cote_appele = std::thread::spawn(move || {
            await_p2p(&rv_appele, appele, &[stun_forge], Duration::from_secs(10))
                .expect("await_p2p")
        });
        let tentative = loop {
            match establish_p2p(&rv_appelant, appelant, appele, &[stun_forge]).expect("establish") {
                ConnAttempt::RelayFallback { reason, .. } if reason.contains("aucun candidat") => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                autre => break autre,
            }
        };
        assert!(
            matches!(tentative, ConnAttempt::Direct(_)),
            "le candidat local doit sauver le punch"
        );
        assert!(matches!(
            cote_appele.join().unwrap(),
            P2pIncoming::Direct(_)
        ));
    }

    /// Serveur STUN simulé qui répond toujours la même adresse forgée
    /// (l'équivalent test de `nat::tests::serveur_stun_simule(Some(..))`,
    /// recopié ici car les utilitaires de test ne traversent pas les modules).
    fn serveur_stun_forge(vue: SocketAddr) -> SocketAddr {
        const MAGIC_COOKIE: u32 = 0x2112_A442;
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let adresse = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut tampon = [0u8; 1500];
            while let Ok((n, source)) = socket.recv_from(&mut tampon) {
                if n < 20 {
                    continue;
                }
                let SocketAddr::V4(vue) = vue else { continue };
                let mut attrs = Vec::new();
                attrs.extend_from_slice(&0x0020u16.to_be_bytes());
                attrs.extend_from_slice(&8u16.to_be_bytes());
                attrs.push(0);
                attrs.push(0x01);
                attrs.extend_from_slice(&(vue.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
                attrs.extend_from_slice(&(u32::from(*vue.ip()) ^ MAGIC_COOKIE).to_be_bytes());
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
    fn pair_hors_ligne_est_une_erreur() {
        let (rv, _) = rendezvous_de_test();
        assert!(establish_p2p(&rv, NovaId(1), NovaId(404), &[]).is_err());
    }

    #[test]
    fn cible_sans_candidats_repli_relais() {
        let (rv, rv2) = rendezvous_de_test();
        let cible = NovaId(9);
        rv2.register(cible, adresse_bidon(), &[3, 3]).unwrap();
        // La cible est en ligne mais n'attend pas (aucun candidat publié).
        match establish_p2p(&rv, NovaId(1), cible, &[]).unwrap() {
            ConnAttempt::RelayFallback {
                peer_cert_der,
                reason,
            } => {
                assert_eq!(
                    peer_cert_der,
                    vec![3, 3],
                    "certificat conservé pour le relais"
                );
                assert!(reason.contains("aucun candidat"), "motif : {reason}");
            }
            ConnAttempt::Direct(_) => panic!("punch impossible sans candidats"),
        }
    }

    #[test]
    fn punch_qui_echoue_repli_relais() {
        let (rv, rv2) = rendezvous_de_test();
        let cible = NovaId(11);
        rv2.register(cible, adresse_bidon(), &[5]).unwrap();
        // Candidat publié mais mort : socket liée jamais lue (gardée vivante
        // pour que son port ne soit pas réattribué).
        let morte = UdpSocket::bind("127.0.0.1:0").unwrap();
        rv2.publish_candidates(cible, &[morte.local_addr().unwrap()])
            .unwrap();

        match establish_p2p_with_timeout(&rv, NovaId(1), cible, &[], Duration::from_millis(250))
            .unwrap()
        {
            ConnAttempt::RelayFallback {
                peer_cert_der,
                reason,
            } => {
                assert_eq!(peer_cert_der, vec![5]);
                assert!(reason.contains("hole punching"), "motif : {reason}");
            }
            ConnAttempt::Direct(_) => panic!("le punch ne peut pas réussir"),
        }
    }

    #[test]
    fn await_sans_demande_expire() {
        let (rv, _) = rendezvous_de_test();
        let id = NovaId(21);
        rv.register(id, adresse_bidon(), &[1]).unwrap();
        let debut = Instant::now();
        assert!(await_p2p(&rv, id, &[], Duration::from_millis(300)).is_err());
        assert!(debut.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn await_exige_un_id_enregistre() {
        let (rv, _) = rendezvous_de_test();
        // publish_candidates échoue : ID jamais enregistré.
        assert!(await_p2p(&rv, NovaId(31), &[], Duration::from_millis(200)).is_err());
    }

    #[test]
    fn demande_sans_candidats_repli_relais_cote_appele() {
        let (rv_appelant, rv_appele) = rendezvous_de_test();
        let appele = NovaId(41);
        rv_appele.register(appele, adresse_bidon(), &[1]).unwrap();
        // L'appelé publie (via await dans un thread) puis l'appelant dépose
        // une demande **sans candidats** : punch impossible côté appelé.
        let cote_appele = std::thread::spawn(move || {
            await_p2p(&rv_appele, appele, &[], Duration::from_secs(5)).expect("await_p2p")
        });
        // Attend que les candidats de l'appelé soient publiés.
        loop {
            if !rv_appelant.peer_candidates(appele).unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        rv_appelant.request_punch(NovaId(42), appele, &[]).unwrap();
        match cote_appele.join().unwrap() {
            P2pIncoming::RelayFallback { from, reason } => {
                assert_eq!(from, NovaId(42));
                assert!(reason.contains("sans candidats"), "motif : {reason}");
            }
            P2pIncoming::Direct(_) => panic!("punch impossible sans candidats"),
        }
    }
}
