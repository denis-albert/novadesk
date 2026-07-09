//! Tunnel TCP **de session** : redirection de port relayée à travers le canal
//! `Control` chiffré de la session NovaDesk (voir [`crate::media`]).
//!
//! # Portée (best-effort)
//!
//! Un tunnel expose, côté **contrôleur**, un écouteur TCP local
//! ([`SessionHandle::open_tunnel`](crate::SessionHandle::open_tunnel)). Chaque
//! connexion locale acceptée est reliée, **à travers le canal fiable de la
//! session** (multiplexé sur `Control`, sous-type
//! [`SousTypeControle::Tunnel`](crate::media)), à une connexion TCP que l'**hôte**
//! ouvre vers la cible demandée — comme le tunnel TCP d'AnyDesk relie un port
//! local à un service du réseau distant.
//!
//! Le relais octet-à-octet réutilise
//! [`nd_features::pipe_bidirectional_stats`] : de chaque côté, le vrai flux TCP
//! (client accepté côté contrôleur, connexion vers la cible côté hôte) est ponté
//! à un tube local ; l'autre extrémité du tube est **pompée** vers/depuis la
//! session. Les octets relayés dans chaque sens et le nombre de connexions sont
//! comptés dans un [`TunnelStats`] par session (cumulé sur tous les flux), lu via
//! [`TunnelHandle::stats`].
//!
//! Limites assumées (best-effort, hors périmètre de ce jet) :
//! - la fenêtre de contrôle de flux est celle du canal `Control` (les données du
//!   tunnel y sont multiplexées avec le chat / presse-papiers / annotations) ;
//! - un pair lent en écriture peut retarder le fil récepteur (pas de fenêtre par
//!   flux) ;
//! - le tunnel **exige le mode étendu** ([`SessionOptions::extended_features`](crate::SessionOptions))
//!   et la capacité [`Capability::TcpTunnel`](nd_features::Capability) côté hôte ;
//!   sinon les trames sont émises mais jamais relayées (aucun fil de features).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nd_features::{pipe_bidirectional_stats, TunnelStats, TunnelStatsSnapshot};
use nd_proto::{NdError, Result};

use crate::SessionRole;

/// Genre d'une trame de tunnel (octet de tête après l'identifiant de flux).
const GENRE_OUVRIR: u8 = 1;
const GENRE_DONNEES: u8 = 2;
const GENRE_FERMER: u8 = 3;

/// Taille du tampon de copie pont ⇄ session (borne la taille d'une trame
/// `Données` ; reste bien sous la limite de fragmentation Noise).
const TAMPON: usize = 16 * 1024;

/// Période de scrutation d'un fil bloquant (lecteur de pont, accepteur) pour
/// revenir vérifier le signal d'arrêt.
const PERIODE_SCRUTATION: Duration = Duration::from_millis(150);

/// État des tunnels d'une session, partagé entre la poignée
/// ([`SessionHandle`](crate::SessionHandle)), le fil récepteur (trames entrantes)
/// et le fil émetteur de features (trames sortantes).
pub(crate) struct EtatTunnels {
    /// Ponts locaux ouverts par identifiant de flux : l'écrivain vers le tube
    /// (les octets **reçus de la session** y sont versés ; le pont les recopie
    /// vers le vrai flux TCP).
    ponts: Mutex<HashMap<u32, TcpStream>>,
    /// Trames de tunnel prêtes à émettre sur `Control` (corps
    /// `[id][genre][données]`), alimentées par les fils lecteurs, drainées par
    /// l'émetteur de features.
    sortie_tx: Sender<Vec<u8>>,
    sortie_rx: Mutex<Receiver<Vec<u8>>>,
    /// Compteurs cumulés (octets relayés, connexions) de tous les flux.
    stats: Arc<TunnelStats>,
    /// Prochain identifiant de flux attribué côté contrôleur.
    prochain_id: AtomicU32,
    /// Signal d'arrêt **global** de la session (les fils du tunnel s'y arrêtent).
    stop: Arc<AtomicBool>,
}

impl EtatTunnels {
    /// Crée l'état, adossé au signal d'arrêt global de la session.
    pub(crate) fn new(stop: Arc<AtomicBool>) -> Self {
        let (sortie_tx, sortie_rx) = mpsc::channel();
        EtatTunnels {
            ponts: Mutex::new(HashMap::new()),
            sortie_tx,
            sortie_rx: Mutex::new(sortie_rx),
            stats: Arc::new(TunnelStats::new()),
            prochain_id: AtomicU32::new(1),
            stop,
        }
    }

    /// Draine les trames de tunnel à émettre sur le canal `Control` (corps déjà
    /// encadré `[id][genre][données]`). Appelée par l'émetteur de features.
    pub(crate) fn drainer_sortie(&self) -> Vec<Vec<u8>> {
        let rx = self.sortie_rx.lock().expect("verrou de la file tunnel");
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Enregistre l'écrivain du pont d'un flux (remplace un éventuel ancien).
    fn enregistrer(&self, id: u32, ecrivain: TcpStream) {
        self.ponts
            .lock()
            .expect("verrou des ponts tunnel")
            .insert(id, ecrivain);
    }

    /// Retire et **coupe** le pont d'un flux (idempotent).
    fn fermer_pont(&self, id: u32) {
        if let Some(flux) = self
            .ponts
            .lock()
            .expect("verrou des ponts tunnel")
            .remove(&id)
        {
            let _ = flux.shutdown(Shutdown::Both);
        }
    }

    /// Écrit des octets **reçus de la session** vers le pont du flux `id` (le
    /// pont les recopie ensuite vers le vrai flux TCP). Coupe le flux sur échec.
    fn ecrire_vers_pont(&self, id: u32, donnees: &[u8]) {
        let ecrivain = self
            .ponts
            .lock()
            .expect("verrou des ponts tunnel")
            .get(&id)
            .and_then(|flux| flux.try_clone().ok());
        if let Some(mut ecrivain) = ecrivain {
            if ecrivain.write_all(donnees).is_err() {
                self.fermer_pont(id);
            }
        }
    }

    /// Empile une trame `[id][genre][données]` vers l'émetteur de features.
    fn emettre(&self, id: u32, genre: u8, donnees: &[u8]) {
        let _ = self.sortie_tx.send(encadrer(id, genre, donnees));
    }

    /// Traite une trame de tunnel **reçue** du pair (démultiplexée depuis
    /// `Control`). `autorise` reflète [`Capability::TcpTunnel`](nd_features::Capability)
    /// côté hôte (vérifiée par l'appelant).
    pub(crate) fn recevoir(
        etat: &Arc<EtatTunnels>,
        payload: &[u8],
        role: SessionRole,
        autorise: bool,
    ) {
        let Some((id, genre, corps)) = desencadrer(payload) else {
            return;
        };
        match genre {
            GENRE_OUVRIR => ouvrir_cote_hote(etat, id, corps, role, autorise),
            GENRE_DONNEES => etat.ecrire_vers_pont(id, corps),
            GENRE_FERMER => etat.fermer_pont(id),
            _ => {}
        }
    }
}

/// Ouvre un tunnel : écoute sur `127.0.0.1:port_local` et relaie chaque
/// connexion locale vers `cible` **à travers la session** (l'hôte ouvre la
/// connexion réelle vers `cible`). Rend une [`TunnelHandle`] (adresse écoutée,
/// statistiques, arrêt).
///
/// # Errors
/// Échec de liaison de l'écouteur local (port déjà pris, droits…).
pub(crate) fn open_tunnel(
    etat: &Arc<EtatTunnels>,
    port_local: u16,
    cible: SocketAddr,
) -> Result<TunnelHandle> {
    let ecouteur = TcpListener::bind((Ipv4Addr::LOCALHOST, port_local)).map_err(NdError::Io)?;
    ecouteur.set_nonblocking(true).map_err(NdError::Io)?;
    let local_addr = ecouteur.local_addr().map_err(NdError::Io)?;

    let stop_accept = Arc::new(AtomicBool::new(false));
    let etat_accept = Arc::clone(etat);
    let stop_boucle = Arc::clone(&stop_accept);
    let accepteur = thread::Builder::new()
        .name("nd-tunnel-accept".to_owned())
        .spawn(move || boucle_accepteur(&ecouteur, &etat_accept, &stop_boucle, cible))
        .map_err(|e| NdError::Io(io::Error::other(e.to_string())))?;

    Ok(TunnelHandle {
        local_addr,
        stats: Arc::clone(&etat.stats),
        stop: stop_accept,
        accepteur: Some(accepteur),
    })
}

/// Boucle d'acceptation (côté contrôleur) : pour chaque connexion locale, alloue
/// un identifiant, demande à l'hôte d'ouvrir la cible (`Ouvrir`), puis ponte le
/// flux. S'arrête au signal du tunnel **ou** de la session.
fn boucle_accepteur(
    ecouteur: &TcpListener,
    etat: &Arc<EtatTunnels>,
    stop: &Arc<AtomicBool>,
    cible: SocketAddr,
) {
    while !stop.load(Ordering::Relaxed) && !etat.stop.load(Ordering::Relaxed) {
        match ecouteur.accept() {
            Ok((client, _adresse)) => {
                // L'écouteur est non-bloquant (boucle d'`accept` interruptible) ;
                // la socket acceptée en **hérite** sur certaines plateformes
                // (Windows) : la remettre en bloquant pour le pont
                // (`pipe_bidirectional_stats` attend des flux bloquants).
                let _ = client.set_nonblocking(false);
                let id = etat.prochain_id.fetch_add(1, Ordering::Relaxed);
                // `Ouvrir` d'abord (canal fiable ordonné : arrive avant les données).
                etat.emettre(id, GENRE_OUVRIR, cible.to_string().as_bytes());
                if ponter(etat, id, client).is_err() {
                    etat.emettre(id, GENRE_FERMER, &[]);
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(PERIODE_SCRUTATION);
            }
            Err(_) => break,
        }
    }
}

/// Traite un `Ouvrir` reçu côté **hôte** : ouvre la connexion réelle vers la
/// cible (dans un fil dédié pour ne pas bloquer le récepteur), puis ponte. Un
/// contrôleur qui recevrait un `Ouvrir` l'ignore (seul l'hôte compose la cible).
fn ouvrir_cote_hote(
    etat: &Arc<EtatTunnels>,
    id: u32,
    corps: &[u8],
    role: SessionRole,
    autorise: bool,
) {
    if role != SessionRole::Controlled {
        return;
    }
    if !autorise {
        etat.emettre(id, GENRE_FERMER, &[]);
        return;
    }
    let Some(cible) = std::str::from_utf8(corps)
        .ok()
        .and_then(|s| s.parse::<SocketAddr>().ok())
    else {
        etat.emettre(id, GENRE_FERMER, &[]);
        return;
    };
    // Prépare le pont **tout de suite** (écrivain enregistré, fil lecteur lancé) :
    // les trames `Données` qui suivent l'`Ouvrir` dans le flux ordonné trouvent
    // ainsi le pont, même si la connexion vers la cible est encore en cours (les
    // octets s'accumulent dans le tube et sont drainés au démarrage du relais).
    let interne = match preparer_pont(etat, id) {
        Ok(interne) => interne,
        Err(_) => {
            etat.emettre(id, GENRE_FERMER, &[]);
            return;
        }
    };
    let etat = Arc::clone(etat);
    let stats = Arc::clone(&etat.stats);
    let _ = thread::Builder::new()
        .name("nd-tunnel-connect".to_owned())
        .spawn(move || match TcpStream::connect(cible) {
            // Relais octet-à-octet réel entre la cible et le tube déjà pontté.
            Ok(distant) => {
                let _ = pipe_bidirectional_stats(distant, interne, &stats);
            }
            Err(_) => {
                etat.fermer_pont(id);
                etat.emettre(id, GENRE_FERMER, &[]);
            }
        });
}

/// Ponte un vrai flux TCP `reel` au canal de session : prépare le tube (écrivain
/// enregistré + fil lecteur), puis lance le relais octet-à-octet
/// ([`pipe_bidirectional_stats`]) entre `reel` et le tube. Utilisé côté
/// contrôleur, où `reel` (la connexion locale acceptée) est déjà disponible.
fn ponter(etat: &Arc<EtatTunnels>, id: u32, reel: TcpStream) -> io::Result<()> {
    let interne = preparer_pont(etat, id)?;
    let stats = Arc::clone(&etat.stats);
    thread::Builder::new()
        .name("nd-tunnel-pipe".to_owned())
        .spawn(move || {
            let _ = pipe_bidirectional_stats(reel, interne, &stats);
        })?;
    Ok(())
}

/// Crée le tube local d'un flux, **enregistre l'écrivain** (pour y verser les
/// octets reçus de la session) et lance le fil lecteur (qui pompe l'autre sens
/// vers la session). Rend l'extrémité `interne` à relier au vrai flux TCP.
fn preparer_pont(etat: &Arc<EtatTunnels>, id: u32) -> io::Result<TcpStream> {
    let (interne, externe) = paire_locale()?;
    etat.enregistrer(id, externe.try_clone()?);
    let etat_lecteur = Arc::clone(etat);
    thread::Builder::new()
        .name("nd-tunnel-read".to_owned())
        .spawn(move || pomper_vers_session(&etat_lecteur, id, &externe))?;
    Ok(interne)
}

/// Lit le tube et émet chaque bloc en `Données` ; propage la fin en `Fermer`.
/// S'arrête au signal global de session (grâce au délai de lecture).
fn pomper_vers_session(etat: &Arc<EtatTunnels>, id: u32, tube: &TcpStream) {
    let mut lecteur = match tube.try_clone() {
        Ok(l) => l,
        Err(_) => return,
    };
    let _ = lecteur.set_read_timeout(Some(PERIODE_SCRUTATION));
    let mut tampon = [0u8; TAMPON];
    loop {
        if etat.stop.load(Ordering::Relaxed) {
            break;
        }
        match lecteur.read(&mut tampon) {
            Ok(0) => {
                etat.emettre(id, GENRE_FERMER, &[]);
                break;
            }
            Ok(n) => etat.emettre(id, GENRE_DONNEES, &tampon[..n]),
            Err(ref e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                etat.emettre(id, GENRE_FERMER, &[]);
                break;
            }
        }
    }
    // Nettoyage local : ce flux n'a plus de pont (le pair a reçu `Fermer`).
    etat.ponts
        .lock()
        .expect("verrou des ponts tunnel")
        .remove(&id);
}

/// Crée une paire de flux TCP connectés via l'interface locale (émulation de
/// `socketpair` : le tube qui relie le relais réel au pompage vers la session).
fn paire_locale() -> io::Result<(TcpStream, TcpStream)> {
    let ecouteur = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let adresse = ecouteur.local_addr()?;
    let un = TcpStream::connect(adresse)?;
    let (autre, _) = ecouteur.accept()?;
    Ok((un, autre))
}

/// Encadre une trame de tunnel : `[id u32 BE][genre u8][données]`.
fn encadrer(id: u32, genre: u8, donnees: &[u8]) -> Vec<u8> {
    let mut trame = Vec::with_capacity(5 + donnees.len());
    trame.extend_from_slice(&id.to_be_bytes());
    trame.push(genre);
    trame.extend_from_slice(donnees);
    trame
}

/// Décode une trame de tunnel en `(id, genre, données)`, ou `None` si tronquée.
fn desencadrer(trame: &[u8]) -> Option<(u32, u8, &[u8])> {
    let id = u32::from_be_bytes(trame.get(0..4)?.try_into().ok()?);
    let genre = *trame.get(4)?;
    Some((id, genre, &trame[5..]))
}

/// Poignée d'un tunnel ouvert : adresse locale écoutée, statistiques cumulées,
/// et arrêt de l'acceptation. Lâcher la poignée (ou [`TunnelHandle::close`])
/// cesse d'accepter de nouvelles connexions locales.
pub struct TunnelHandle {
    local_addr: SocketAddr,
    stats: Arc<TunnelStats>,
    stop: Arc<AtomicBool>,
    accepteur: Option<JoinHandle<()>>,
}

impl TunnelHandle {
    /// Adresse locale réellement écoutée (utile après un bind sur le port 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Instantané des compteurs de tunnel de la session (octets relayés dans
    /// chaque sens, connexions) — **cumulés sur tous les flux** de la session.
    #[must_use]
    pub fn stats(&self) -> TunnelStatsSnapshot {
        self.stats.snapshot()
    }

    /// Cesse d'accepter de nouvelles connexions locales et attend le fil
    /// d'acceptation (les flux déjà pontés se terminent avec la session).
    pub fn close(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(accepteur) = self.accepteur.take() {
            let _ = accepteur.join();
        }
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encadrage_aller_retour() {
        let trame = encadrer(0x0102_0304, GENRE_DONNEES, b"salut");
        let (id, genre, corps) = desencadrer(&trame).expect("désencadrage");
        assert_eq!(id, 0x0102_0304);
        assert_eq!(genre, GENRE_DONNEES);
        assert_eq!(corps, b"salut");
        // Trame vide (juste id + genre) : corps vide, pas de panique.
        let ferme = encadrer(7, GENRE_FERMER, &[]);
        assert_eq!(desencadrer(&ferme), Some((7, GENRE_FERMER, &[][..])));
        // Tronquée : rejetée.
        assert!(desencadrer(&[0, 0, 0]).is_none());
    }

    #[test]
    fn paire_locale_est_connectee() {
        let (mut a, mut b) = paire_locale().expect("paire locale");
        a.write_all(b"ping").expect("écriture");
        let mut tampon = [0u8; 4];
        b.read_exact(&mut tampon).expect("lecture");
        assert_eq!(&tampon, b"ping");
    }

    /// Bout-en-bout du **relais** de tunnel, sans session : deux [`EtatTunnels`]
    /// (contrôleur, hôte) reliés par un fil qui recopie les trames de sortie de
    /// l'un vers l'entrée de l'autre. Prouve que la plomberie du tunnel relaie
    /// bien une connexion TCP locale jusqu'à un service que « l'hôte » joint.
    #[test]
    fn relais_bout_en_bout_sans_session() {
        use std::time::Instant;

        // Serveur d'écho (la « cible distante » jointe côté hôte).
        let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind écho");
        let echo_addr = echo.local_addr().expect("adresse écho");
        thread::spawn(move || {
            for flux in echo.incoming() {
                let Ok(mut flux) = flux else { break };
                thread::spawn(move || {
                    let mut tampon = [0u8; 1024];
                    while let Ok(n) = flux.read(&mut tampon) {
                        if n == 0 || flux.write_all(&tampon[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let stop = Arc::new(AtomicBool::new(false));
        let ctl = Arc::new(EtatTunnels::new(Arc::clone(&stop)));
        let hote = Arc::new(EtatTunnels::new(Arc::clone(&stop)));

        // Relais : sortie contrôleur → hôte, sortie hôte → contrôleur.
        let relais_ctl = Arc::clone(&ctl);
        let relais_hote = Arc::clone(&hote);
        let relais_stop = Arc::clone(&stop);
        let relais = thread::spawn(move || {
            while !relais_stop.load(Ordering::Relaxed) {
                for corps in relais_ctl.drainer_sortie() {
                    EtatTunnels::recevoir(&relais_hote, &corps, SessionRole::Controlled, true);
                }
                for corps in relais_hote.drainer_sortie() {
                    EtatTunnels::recevoir(&relais_ctl, &corps, SessionRole::Controller, true);
                }
                thread::sleep(Duration::from_millis(5));
            }
        });

        let tunnel = open_tunnel(&ctl, 0, echo_addr).expect("ouverture tunnel");
        let mut client = TcpStream::connect(tunnel.local_addr()).expect("connexion locale");
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("délai");
        client.write_all(b"salut-tunnel").expect("écriture");
        let mut recu = [0u8; 12];
        client.read_exact(&mut recu).expect("écho relayé");
        assert_eq!(&recu, b"salut-tunnel");

        let echeance = Instant::now() + Duration::from_secs(5);
        while tunnel.stats().octets_total() < 12 && Instant::now() < echeance {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(tunnel.stats().octets_total() >= 12, "octets comptés");

        drop(client);
        stop.store(true, Ordering::Relaxed);
        tunnel.close();
        let _ = relais.join();
    }
}
