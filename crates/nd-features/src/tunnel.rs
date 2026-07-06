//! Tunnel TCP (redirection de ports) : plomberie pure `std`.
//!
//! Pour ce jet, le flux « distant » est un [`TcpStream`] local ; dans le
//! produit final, ce sera un canal chiffré de la session NovaDesk exposant la
//! même interface. La mécanique (pipe bidirectionnel, écouteur local,
//! propagation des fins de flux) reste identique.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread;

/// Copie `source` vers `destination` jusqu'à la fin du flux, puis propage la
/// fin en fermant le sens d'écriture de `destination` (shutdown en cascade) :
/// le pair d'en face voit un EOF propre, ce qui termine la copie du sens
/// opposé au lieu de la laisser bloquée.
fn copier_puis_fermer(mut source: TcpStream, mut destination: TcpStream) -> io::Result<u64> {
    let resultat = io::copy(&mut source, &mut destination);
    // Quoi qu'il arrive (fin propre ou erreur), on signale « plus rien à
    // écrire ». L'échec du shutdown (pair déjà parti) n'apporte rien de plus.
    let _ = destination.shutdown(Shutdown::Write);
    resultat
}

/// Fait transiter les octets dans les deux sens entre `a` et `b`, jusqu'à la
/// fermeture des deux flux. Bloquant : deux threads exécutent chacun un
/// `io::copy`, et la fin d'un sens est propagée à l'autre par `shutdown`.
pub fn pipe_bidirectional(a: TcpStream, b: TcpStream) -> io::Result<()> {
    let a_bis = a.try_clone()?;
    let b_bis = b.try_clone()?;

    let aller = thread::Builder::new()
        .name("nd-tunnel-aller".into())
        .spawn(move || copier_puis_fermer(a, b))?;
    let retour = thread::Builder::new()
        .name("nd-tunnel-retour".into())
        .spawn(move || copier_puis_fermer(b_bis, a_bis))?;

    let resultat_aller = aller
        .join()
        .map_err(|_| io::Error::other("panique dans le thread de copie aller"))?;
    let resultat_retour = retour
        .join()
        .map_err(|_| io::Error::other("panique dans le thread de copie retour"))?;

    resultat_aller?;
    resultat_retour?;
    Ok(())
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
}
