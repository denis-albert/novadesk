//! Sonde **plan 05** : connectivité par ID entre **deux processus** distincts,
//! via de **vraies adresses d'interface** (pas 127.0.0.1 quand la machine en a
//! une) — échange de candidats, hole punching UDP, QUIC sur la socket percée,
//! transfert de N messages avec échos.
//!
//! ```text
//! cargo run -p nd-signaling --example p2p_two_process
//! ```
//!
//! Le processus parent joue l'infrastructure : serveur de **rendez-vous** et
//! serveur **STUN** (RFC 5389, réponses réelles « source observée ») liés à
//! l'adresse d'interface retenue. Il lance ensuite deux processus enfants —
//! l'appelé (contrôlé) puis l'appelant (contrôleur) — qui ne partagent
//! **aucune mémoire** : tout passe par le réseau local de la machine.
//!
//! # Honnêteté
//!
//! Les deux pairs étant sur la même machine, aucun NAT n'est traversé : la
//! sonde prouve le **câblage complet** (candidats STUN réels → punch
//! simultané coordonné → QUIC quinn sur la socket percée → données), pas la
//! traversée d'un vrai NAT — laquelle dépend du type de NAT (voir
//! `nd_signaling::nat`). Sans interface réseau utilisable, la sonde retombe
//! sur 127.0.0.1 et le dit.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use nd_proto::{ChannelKind, NovaId, Reliability};
use nd_signaling::{
    await_p2p, establish_p2p, serve, ConnAttempt, P2pIncoming, Registry, RendezvousClient,
};
use nd_transport::{
    accept_quic_over_socket, connect_quic_over_socket, QuicTransport, ServerIdentity, Transport,
};

/// Nombre de messages transférés (appelant → appelé, avec écho retour).
const N_MESSAGES: u32 = 20;

/// ID NovaDesk de l'appelant (contrôleur).
const ID_APPELANT: u64 = 123_456_789;
/// ID NovaDesk de l'appelé (contrôlé).
const ID_APPELE: u64 = 987_654_321;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => orchestrer(),
        Some("appele") => enfant_appele(&args),
        Some("appelant") => enfant_appelant(&args),
        Some(autre) => {
            eprintln!("rôle inconnu : {autre} (attendu : aucun, `appele` ou `appelant`)");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Parent : infrastructure (rendez-vous + STUN) et orchestration
// ---------------------------------------------------------------------------

/// IP d'une vraie interface de sortie (découverte par un `connect` UDP sans
/// trafic vers une adresse routable), ou `None` si la machine n'en a pas.
fn ip_interface_reelle() -> Option<IpAddr> {
    let temoin = UdpSocket::bind("0.0.0.0:0").ok()?;
    temoin.connect("8.8.8.8:53").ok()?;
    let ip = temoin.local_addr().ok()?.ip();
    (!ip.is_loopback()).then_some(ip)
}

/// Vérifie qu'un aller UDP local fonctionne sur cette IP (pare-feu, etc.).
fn udp_local_fonctionne(ip: IpAddr) -> bool {
    let Ok(a) = UdpSocket::bind((ip, 0)) else {
        return false;
    };
    let Ok(b) = UdpSocket::bind((ip, 0)) else {
        return false;
    };
    let Ok(cible) = b.local_addr() else {
        return false;
    };
    if a.send_to(b"sonde", cible).is_err() {
        return false;
    }
    let _ = b.set_read_timeout(Some(Duration::from_millis(500)));
    let mut tampon = [0u8; 16];
    b.recv_from(&mut tampon).is_ok()
}

/// Serveur STUN minimal (RFC 5389) : répond à chaque Binding Request par une
/// Binding Success Response portant la **source observée** en
/// XOR-MAPPED-ADDRESS — le comportement d'un vrai serveur, sur cette machine.
fn lancer_stun(ip: IpAddr) -> std::io::Result<SocketAddr> {
    const MAGIC_COOKIE: u32 = 0x2112_A442;
    let socket = UdpSocket::bind((ip, 0))?;
    let adresse = socket.local_addr()?;
    std::thread::spawn(move || {
        let mut tampon = [0u8; 1500];
        while let Ok((n, source)) = socket.recv_from(&mut tampon) {
            // Binding Request : en-tête de 20 octets minimum.
            if n < 20 || tampon[0] != 0x00 || tampon[1] != 0x01 {
                continue;
            }
            let SocketAddr::V4(vue) = source else {
                continue;
            };
            // Attribut XOR-MAPPED-ADDRESS (IPv4).
            let mut attrs = Vec::with_capacity(12);
            attrs.extend_from_slice(&0x0020u16.to_be_bytes());
            attrs.extend_from_slice(&8u16.to_be_bytes());
            attrs.push(0);
            attrs.push(0x01);
            attrs.extend_from_slice(&(vue.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
            attrs.extend_from_slice(&(u32::from(*vue.ip()) ^ MAGIC_COOKIE).to_be_bytes());
            // Binding Success Response, transaction ID recopié.
            let mut rep = Vec::with_capacity(20 + attrs.len());
            rep.extend_from_slice(&0x0101u16.to_be_bytes());
            rep.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
            rep.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
            rep.extend_from_slice(&tampon[8..20]);
            rep.extend_from_slice(&attrs);
            let _ = socket.send_to(&rep, source);
        }
    });
    Ok(adresse)
}

fn orchestrer() -> ExitCode {
    // 1. Adresse d'interface : la vraie si possible, loopback en dernier recours.
    let ip = match ip_interface_reelle() {
        Some(ip) if udp_local_fonctionne(ip) => {
            println!("[parent] interface réelle retenue : {ip}");
            ip
        }
        Some(ip) => {
            println!(
                "[parent] interface {ip} trouvée mais l'UDP local n'y circule pas \
                 (pare-feu ?) — repli sur 127.0.0.1"
            );
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
        None => {
            println!("[parent] aucune interface réseau utilisable — repli sur 127.0.0.1");
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
    };

    // 2. Infrastructure : rendez-vous + STUN sur cette IP.
    let listener = match TcpListener::bind((ip, 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[parent] bind du rendez-vous impossible : {e}");
            return ExitCode::FAILURE;
        }
    };
    let addr_rv = listener.local_addr().expect("adresse du rendez-vous");
    let registry = Registry::new();
    std::thread::spawn(move || {
        let _ = serve(listener, registry);
    });
    let addr_stun = match lancer_stun(ip) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[parent] bind du serveur STUN impossible : {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("[parent] rendez-vous : {addr_rv} ; STUN : {addr_stun} ; {N_MESSAGES} messages");

    // 3. Deux processus enfants (aucune mémoire partagée) : l'appelé d'abord
    //    (il doit publier ses candidats), l'appelant ensuite.
    let exe = std::env::current_exe().expect("chemin de l'exécutable");
    let lancer = |role: &str, id_local: u64, id_pair: u64| {
        Command::new(&exe)
            .args([
                role,
                &addr_rv.to_string(),
                &addr_stun.to_string(),
                &id_local.to_string(),
                &id_pair.to_string(),
                &N_MESSAGES.to_string(),
            ])
            .spawn()
    };
    let mut appele = match lancer("appele", ID_APPELE, ID_APPELANT) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[parent] lancement de l'appelé impossible : {e}");
            return ExitCode::FAILURE;
        }
    };
    std::thread::sleep(Duration::from_millis(300));
    let mut appelant = match lancer("appelant", ID_APPELANT, ID_APPELE) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[parent] lancement de l'appelant impossible : {e}");
            let _ = appele.kill();
            return ExitCode::FAILURE;
        }
    };

    // 4. Verdict : les deux enfants doivent sortir en succès (leurs attentes
    //    internes sont bornées, ils ne peuvent pas bloquer indéfiniment).
    let statut_appelant = appelant.wait().expect("attente de l'appelant");
    let statut_appele = appele.wait().expect("attente de l'appelé");
    let ok = statut_appelant.success() && statut_appele.success();
    println!(
        "[parent] verdict : {} (appelant {statut_appelant}, appelé {statut_appele})",
        if ok { "SONDE OK" } else { "ÉCHEC" }
    );
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Enfants : les deux pairs
// ---------------------------------------------------------------------------

/// Arguments communs des enfants : `<rôle> rv stun id_local id_pair n`.
struct ArgsEnfant {
    rv: RendezvousClient,
    stun: SocketAddr,
    id_local: NovaId,
    id_pair: NovaId,
    n: u32,
}

fn analyser_args(args: &[String]) -> Option<ArgsEnfant> {
    Some(ArgsEnfant {
        rv: RendezvousClient::new(args.get(2)?.parse().ok()?),
        stun: args.get(3)?.parse().ok()?,
        id_local: NovaId(args.get(4)?.parse().ok()?),
        id_pair: NovaId(args.get(5)?.parse().ok()?),
        n: args.get(6)?.parse().ok()?,
    })
}

/// Draine `poll_recv` jusqu'au prochain message ou à l'expiration.
fn attendre_message(transport: &mut QuicTransport, timeout: Duration) -> Option<Vec<u8>> {
    let debut = Instant::now();
    while debut.elapsed() < timeout {
        if let Some((_, data)) = transport.poll_recv().ok()? {
            return Some(data);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

/// Appelé (contrôlé) : registre + attente P2P, QUIC serveur sur la socket
/// percée, écho des N messages.
fn enfant_appele(args: &[String]) -> ExitCode {
    let Some(a) = analyser_args(args) else {
        eprintln!("[appelé] arguments invalides");
        return ExitCode::FAILURE;
    };
    let identite = match ServerIdentity::generate() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[appelé] identité : {e}");
            return ExitCode::FAILURE;
        }
    };
    // Pas d'écouteur direct dans ce scénario pur punch : adresse réservée.
    if let Err(e) = a.rv.register(
        a.id_local,
        "0.0.0.0:0".parse().expect("adresse réservée"),
        identite.cert_der(),
    ) {
        eprintln!("[appelé] register : {e}");
        return ExitCode::FAILURE;
    }

    let entrant = match await_p2p(&a.rv, a.id_local, &[a.stun], Duration::from_secs(20)) {
        Ok(P2pIncoming::Direct(chemin)) => chemin,
        Ok(P2pIncoming::RelayFallback { from, reason }) => {
            eprintln!("[appelé] punch échoué avec {from} : {reason} (repli relais attendu ici ?)");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("[appelé] await_p2p : {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "[appelé] punch confirmé : pair {} vu en {} (socket locale {})",
        entrant.from,
        entrant.peer_addr,
        entrant
            .socket
            .local_addr()
            .map_or_else(|_| "?".into(), |a| a.to_string())
    );

    let mut transport = match accept_quic_over_socket(entrant.socket, &identite) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[appelé] accept_over_socket : {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("[appelé] session QUIC établie sur la socket percée");

    let canal = transport.open_channel(ChannelKind::Control);
    let mut recus = 0u32;
    while recus < a.n {
        let Some(data) = attendre_message(&mut transport, Duration::from_secs(10)) else {
            eprintln!("[appelé] flux interrompu après {recus}/{} messages", a.n);
            return ExitCode::FAILURE;
        };
        if transport.send(canal, data, Reliability::Reliable).is_err() {
            eprintln!("[appelé] écho impossible après {recus}/{} messages", a.n);
            return ExitCode::FAILURE;
        }
        recus += 1;
    }

    // Confirmation « fin » de l'appelant : elle prouve que tous les échos lui
    // sont parvenus AVANT que ce processus ne sorte — sortir plus tôt
    // fermerait la connexion et détruirait le dernier écho encore en vol.
    match attendre_message(&mut transport, Duration::from_secs(10)) {
        Some(fin) if fin == b"fin" => {
            println!(
                "[appelé] {recus}/{} messages reçus et retournés en écho, fin confirmée",
                a.n
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("[appelé] confirmation de fin manquante après {recus} échos");
            ExitCode::FAILURE
        }
    }
}

/// Appelant (contrôleur) : établissement P2P par ID, QUIC client sur la
/// socket percée (certificat épinglé via lookup), N messages + échos.
fn enfant_appelant(args: &[String]) -> ExitCode {
    let Some(a) = analyser_args(args) else {
        eprintln!("[appelant] arguments invalides");
        return ExitCode::FAILURE;
    };

    // L'appelé publie ses candidats de son côté : on retente jusqu'à 15 s.
    let echeance = Instant::now() + Duration::from_secs(15);
    let chemin = loop {
        match establish_p2p(&a.rv, a.id_local, a.id_pair, &[a.stun]) {
            Ok(ConnAttempt::Direct(chemin)) => break chemin,
            Ok(ConnAttempt::RelayFallback { reason, .. }) => {
                if Instant::now() >= echeance {
                    eprintln!("[appelant] punch jamais établi : {reason}");
                    return ExitCode::FAILURE;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                if Instant::now() >= echeance {
                    eprintln!("[appelant] establish_p2p : {e}");
                    return ExitCode::FAILURE;
                }
                // L'appelé n'est peut-être pas encore enregistré.
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    };
    println!(
        "[appelant] punch confirmé : pair {} vu en {} (socket locale {})",
        a.id_pair,
        chemin.peer_addr,
        chemin
            .socket
            .local_addr()
            .map_or_else(|_| "?".into(), |a| a.to_string())
    );

    let mut transport =
        match connect_quic_over_socket(chemin.socket, chemin.peer_addr, &chemin.peer_cert_der) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[appelant] connect_over_socket : {e}");
                return ExitCode::FAILURE;
            }
        };
    println!("[appelant] session QUIC établie (certificat du pair épinglé)");

    let canal = transport.open_channel(ChannelKind::Control);
    let mut echos = 0u32;
    for i in 0..a.n {
        let message = format!("sonde-{i}").into_bytes();
        if transport
            .send(canal, message.clone(), Reliability::Reliable)
            .is_err()
        {
            eprintln!("[appelant] envoi interrompu à {i}/{}", a.n);
            return ExitCode::FAILURE;
        }
        let Some(echo) = attendre_message(&mut transport, Duration::from_secs(10)) else {
            eprintln!("[appelant] écho manquant après {echos}/{}", a.n);
            return ExitCode::FAILURE;
        };
        if echo != message {
            eprintln!("[appelant] écho corrompu au message {i}");
            return ExitCode::FAILURE;
        }
        echos += 1;
    }

    // Signale la fin à l'appelé (tous les échos sont arrivés) et laisse un
    // battement au flux pour partir avant la sortie du processus.
    let _ = transport.send(canal, b"fin".to_vec(), Reliability::Reliable);
    std::thread::sleep(Duration::from_millis(300));
    println!(
        "[appelant] {echos}/{} messages envoyés et échos vérifiés",
        a.n
    );
    ExitCode::SUCCESS
}
