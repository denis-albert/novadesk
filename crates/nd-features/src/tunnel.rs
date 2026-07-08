//! Tunnel TCP (redirection de ports) : plomberie pure `std`.
//!
//! Pour ce jet, le flux « distant » est un [`TcpStream`] local ; dans le
//! produit final, ce sera un canal chiffré de la session NovaDesk exposant la
//! même interface. La mécanique (pipe bidirectionnel, écouteur local,
//! propagation des fins de flux) reste identique.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

/// Compteurs cumulés d'un tunnel, sûrs à partager entre threads.
///
/// Les trois compteurs sont atomiques : une même instance peut être **lue** par
/// l'UI (via [`TunnelStats::snapshot`]) pendant que les threads de copie
/// l'**alimentent**. Ordonnancement `Relaxed` : on ne veut que des totaux
/// exacts, sans garantie d'ordre entre compteurs. Se partage par référence
/// (`&TunnelStats`), au besoin enveloppée dans un `Arc` par l'appelant.
#[derive(Debug, Default)]
pub struct TunnelStats {
    octets_a_vers_b: AtomicU64,
    octets_b_vers_a: AtomicU64,
    connexions: AtomicU64,
}

impl TunnelStats {
    /// Compteurs remis à zéro.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Octets relayés dans le sens a → b (client → distant) depuis le début.
    #[must_use]
    pub fn octets_a_vers_b(&self) -> u64 {
        self.octets_a_vers_b.load(Ordering::Relaxed)
    }

    /// Octets relayés dans le sens b → a (distant → client) depuis le début.
    #[must_use]
    pub fn octets_b_vers_a(&self) -> u64 {
        self.octets_b_vers_a.load(Ordering::Relaxed)
    }

    /// Total d'octets relayés, les deux sens confondus.
    #[must_use]
    pub fn octets_total(&self) -> u64 {
        self.octets_a_vers_b()
            .saturating_add(self.octets_b_vers_a())
    }

    /// Nombre de sessions de tunnel établies (une par pont mené à terme).
    #[must_use]
    pub fn connexions(&self) -> u64 {
        self.connexions.load(Ordering::Relaxed)
    }

    /// Instantané des trois compteurs, pratique pour l'affichage ou les tests.
    #[must_use]
    pub fn snapshot(&self) -> TunnelStatsSnapshot {
        TunnelStatsSnapshot {
            octets_a_vers_b: self.octets_a_vers_b(),
            octets_b_vers_a: self.octets_b_vers_a(),
            connexions: self.connexions(),
        }
    }
}

/// Instantané non-atomique des compteurs d'un [`TunnelStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TunnelStatsSnapshot {
    /// Octets relayés dans le sens a → b.
    pub octets_a_vers_b: u64,
    /// Octets relayés dans le sens b → a.
    pub octets_b_vers_a: u64,
    /// Sessions de tunnel établies.
    pub connexions: u64,
}

impl TunnelStatsSnapshot {
    /// Total d'octets relayés (les deux sens).
    #[must_use]
    pub fn octets_total(self) -> u64 {
        self.octets_a_vers_b.saturating_add(self.octets_b_vers_a)
    }
}

/// Copie `source` vers `destination` jusqu'à la fin du flux en cumulant les
/// octets transférés dans `compteur`, puis propage la fin en fermant le sens
/// d'écriture de `destination` (shutdown en cascade) : le pair d'en face voit
/// un EOF propre, ce qui termine la copie du sens opposé au lieu de la laisser
/// bloquée.
fn copier_puis_fermer_compte(
    mut source: TcpStream,
    mut destination: TcpStream,
    compteur: &AtomicU64,
) -> io::Result<u64> {
    let mut tampon = [0u8; 16 * 1024];
    let mut total = 0u64;
    let resultat = loop {
        match source.read(&mut tampon) {
            Ok(0) => break Ok(total),
            Ok(n) => {
                if let Err(e) = destination.write_all(&tampon[..n]) {
                    break Err(e);
                }
                total += n as u64;
                // Compteur alimenté au fil de l'eau : l'UI voit la progression.
                compteur.fetch_add(n as u64, Ordering::Relaxed);
            }
            // Lecture interrompue par un signal : on retente.
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    // Quoi qu'il arrive (fin propre ou erreur), on signale « plus rien à
    // écrire ». L'échec du shutdown (pair déjà parti) n'apporte rien de plus.
    let _ = destination.shutdown(Shutdown::Write);
    resultat
}

/// Cœur du pont bidirectionnel : relaie les octets dans les deux sens entre `a`
/// et `b` jusqu'à la fermeture des deux flux, en alimentant `stats`. Bloquant :
/// deux threads délimités copient chacun un sens, et la fin d'un sens est
/// propagée à l'autre par `shutdown`.
fn pipe_core(a: TcpStream, b: TcpStream, stats: &TunnelStats) -> io::Result<()> {
    let a_bis = a.try_clone()?;
    let b_bis = b.try_clone()?;
    stats.connexions.fetch_add(1, Ordering::Relaxed);

    thread::scope(|portee| -> io::Result<()> {
        let aller = thread::Builder::new()
            .name("nd-tunnel-aller".into())
            .spawn_scoped(portee, || {
                copier_puis_fermer_compte(a, b, &stats.octets_a_vers_b)
            })?;
        let retour = thread::Builder::new()
            .name("nd-tunnel-retour".into())
            .spawn_scoped(portee, || {
                copier_puis_fermer_compte(b_bis, a_bis, &stats.octets_b_vers_a)
            })?;

        aller
            .join()
            .map_err(|_| io::Error::other("panique dans le thread de copie aller"))??;
        retour
            .join()
            .map_err(|_| io::Error::other("panique dans le thread de copie retour"))??;
        Ok(())
    })
}

/// Fait transiter les octets dans les deux sens entre `a` et `b`, jusqu'à la
/// fermeture des deux flux. Bloquant : deux threads relaient chacun un sens, et
/// la fin d'un sens est propagée à l'autre par `shutdown`.
///
/// Variante instrumentée : [`pipe_bidirectional_stats`].
pub fn pipe_bidirectional(a: TcpStream, b: TcpStream) -> io::Result<()> {
    pipe_core(a, b, &TunnelStats::new())
}

/// Comme [`pipe_bidirectional`], mais alimente `stats` (octets relayés dans
/// chaque sens, compteur de connexions) au fil de la copie.
pub fn pipe_bidirectional_stats(a: TcpStream, b: TcpStream, stats: &TunnelStats) -> io::Result<()> {
    pipe_core(a, b, stats)
}

/// Redirection de port locale : écoute sur un port TCP local et relie chaque
/// connexion entrante à un flux « distant » fourni par un callback.
///
/// Le callback reçoit l'adresse du client accepté et rend le flux à relier
/// (pour ce jet : un `TcpStream` obtenu par exemple via `TcpStream::connect`).
#[derive(Debug)]
pub struct LocalForwarder {
    ecouteur: TcpListener,
}

impl LocalForwarder {
    /// Écoute sur `adresse` (par exemple `127.0.0.1:0` pour un port éphémère).
    pub fn bind(adresse: SocketAddr) -> io::Result<Self> {
        Ok(LocalForwarder {
            ecouteur: TcpListener::bind(adresse)?,
        })
    }

    /// Adresse locale réellement écoutée (utile après un bind sur le port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.ecouteur.local_addr()
    }

    /// Accepte **une** connexion entrante, obtient le flux distant via
    /// `connecter`, puis relie les deux jusqu'à la fin de la session
    /// (bloquant). Brique de base testable de la boucle [`LocalForwarder::run`].
    pub fn forward_one<F>(&self, connecter: F) -> io::Result<()>
    where
        F: FnOnce(SocketAddr) -> io::Result<TcpStream>,
    {
        let (client, adresse_client) = self.ecouteur.accept()?;
        let distant = connecter(adresse_client)?;
        pipe_bidirectional(client, distant)
    }

    /// Sert les connexions entrantes en boucle, l'une après l'autre. Ne rend
    /// la main que sur une erreur d'`accept` ou de `connecter` ; une session
    /// déjà établie qui casse (reset…) ne condamne pas l'écouteur.
    pub fn run<F>(&self, mut connecter: F) -> io::Result<()>
    where
        F: FnMut(SocketAddr) -> io::Result<TcpStream>,
    {
        loop {
            let (client, adresse_client) = self.ecouteur.accept()?;
            let distant = connecter(adresse_client)?;
            let _ = pipe_bidirectional(client, distant);
        }
    }

    /// Comme [`LocalForwarder::forward_one`], mais alimente `stats` (octets
    /// relayés dans chaque sens et compteur de connexions).
    pub fn forward_one_stats<F>(&self, connecter: F, stats: &TunnelStats) -> io::Result<()>
    where
        F: FnOnce(SocketAddr) -> io::Result<TcpStream>,
    {
        let (client, adresse_client) = self.ecouteur.accept()?;
        let distant = connecter(adresse_client)?;
        pipe_bidirectional_stats(client, distant, stats)
    }

    /// Comme [`LocalForwarder::run`], mais alimente `stats` au fil des
    /// connexions servies (compteurs cumulés sur toute la durée de vie).
    pub fn run_stats<F>(&self, mut connecter: F, stats: &TunnelStats) -> io::Result<()>
    where
        F: FnMut(SocketAddr) -> io::Result<TcpStream>,
    {
        loop {
            let (client, adresse_client) = self.ecouteur.accept()?;
            let distant = connecter(adresse_client)?;
            let _ = pipe_bidirectional_stats(client, distant, stats);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::Ipv4Addr;

    /// Paire de flux TCP connectés via l'interface locale.
    fn paire_tcp() -> (TcpStream, TcpStream) {
        let ecouteur = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let adresse = ecouteur.local_addr().unwrap();
        let client = TcpStream::connect(adresse).unwrap();
        let (serveur, _) = ecouteur.accept().unwrap();
        (client, serveur)
    }

    #[test]
    fn stats_comptent_les_octets_des_deux_sens() {
        // Topologie : gauche <-> (interne_a =pont= interne_b) <-> droite.
        let (mut gauche, interne_a) = paire_tcp();
        let (interne_b, mut droite) = paire_tcp();
        let stats = TunnelStats::new();

        // a → b : `gauche` envoie 5 octets ; b → a : `droite` en renvoie 3.
        let ecrivain_gauche = {
            let mut flux = gauche.try_clone().unwrap();
            thread::spawn(move || {
                flux.write_all(b"hello").unwrap();
                flux.shutdown(Shutdown::Write).unwrap();
            })
        };
        let ecrivain_droite = {
            let mut flux = droite.try_clone().unwrap();
            thread::spawn(move || {
                flux.write_all(b"abc").unwrap();
                flux.shutdown(Shutdown::Write).unwrap();
            })
        };

        // On draine les deux extrémités pour que les copies atteignent l'EOF.
        let lecteur_droite = thread::spawn(move || {
            let mut recu = Vec::new();
            droite.read_to_end(&mut recu).unwrap();
            recu
        });
        let lecteur_gauche = thread::spawn(move || {
            let mut recu = Vec::new();
            gauche.read_to_end(&mut recu).unwrap();
            recu
        });

        // Bloquant jusqu'à la fin des deux sens (les deux threads internes).
        pipe_bidirectional_stats(interne_a, interne_b, &stats).unwrap();

        assert_eq!(lecteur_droite.join().unwrap(), b"hello");
        assert_eq!(lecteur_gauche.join().unwrap(), b"abc");
        ecrivain_gauche.join().unwrap();
        ecrivain_droite.join().unwrap();

        let instantane = stats.snapshot();
        assert_eq!(instantane.octets_a_vers_b, 5);
        assert_eq!(instantane.octets_b_vers_a, 3);
        assert_eq!(instantane.connexions, 1);
        assert_eq!(instantane.octets_total(), 8);
    }
}
