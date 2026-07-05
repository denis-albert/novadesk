//! Serveur de relais NovaDesk (plan 05 — connectivité/NAT).
//!
//! Achemine le trafic chiffré de bout en bout entre deux pairs quand le P2P échoue
//! (NAT symétrique/CGNAT). Le relais est un **tuyau aveugle** : chaque client
//! annonce d'abord un **ticket** (trame `[u32 BE len][ticket]`) ; le premier pair
//! d'un ticket est mis en attente, et à l'arrivée du second pair porteur du même
//! ticket, le relais fait transiter les octets dans les deux sens sans jamais les
//! inspecter (le média est chiffré de bout en bout, voir plan 06).
//!
//! Implémentation std pure (TCP bloquant, threads), dans l'esprit de
//! `nd-signaling`. Le relais de production (quotas, tickets signés, métriques)
//! viendra avec les plans 05/11.

use std::collections::HashMap;
use std::io::{self, Read};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// Adresse d'écoute par défaut du relais.
const ADRESSE_DEFAUT: &str = "0.0.0.0:9100";

/// Taille maximale acceptée pour un ticket (une annonce plus grande est rejetée).
const TAILLE_TICKET_MAX: usize = 1024;

/// Table des pairs en attente d'appariement : ticket → connexion du premier pair.
type TicketsEnAttente = Arc<Mutex<HashMap<Vec<u8>, TcpStream>>>;

fn main() -> io::Result<()> {
    // Adresse d'écoute : premier argument CLI, sinon la valeur par défaut.
    let adresse = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    let listener = TcpListener::bind(&adresse)?;
    println!(
        "nd-relay — NovaDesk (protocole v{}) — relais opaque en écoute sur {}",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?
    );
    servir(&listener, &TicketsEnAttente::default())
}

/// Boucle d'acceptation du relais (bloquante, un thread par connexion).
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
fn servir(listener: &TcpListener, en_attente: &TicketsEnAttente) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let table = Arc::clone(en_attente);
        thread::spawn(move || {
            // Une annonce invalide ou une déconnexion précoce ferme simplement
            // la connexion fautive, sans impacter le reste du relais.
            let _ = apparier(stream, &table);
        });
    }
    Ok(())
}

/// Lit la trame d'annonce (`[u32 BE len][ticket]`) et apparie la connexion.
///
/// Premier pair d'un ticket : mis en attente dans la table. Second pair : le
/// couple est retiré de la table et le relais bidirectionnel démarre.
fn apparier(mut stream: TcpStream, en_attente: &TicketsEnAttente) -> io::Result<()> {
    let ticket = lire_ticket(&mut stream)?;

    // Section critique courte : retire le pair en attente ou dépose la connexion.
    let paire = {
        let mut table = en_attente.lock().unwrap();
        match table.remove(&ticket) {
            Some(premier) => Some((premier, stream)),
            None => {
                table.insert(ticket, stream);
                None
            }
        }
    };

    match paire {
        Some((premier, second)) => relayer(premier, second),
        None => Ok(()), // Premier arrivé : il attend son pair dans la table.
    }
}

/// Lit la trame d'annonce et renvoie le ticket, ou une erreur si l'annonce est
/// incomplète (déconnexion en cours de trame), vide ou trop grande.
fn lire_ticket(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut prefixe = [0u8; 4];
    stream.read_exact(&mut prefixe)?;
    let longueur = u32::from_be_bytes(prefixe) as usize;
    if longueur == 0 || longueur > TAILLE_TICKET_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ticket vide ou trop grand",
        ));
    }
    let mut ticket = vec![0u8; longueur];
    stream.read_exact(&mut ticket)?;
    Ok(ticket)
}

/// Fait transiter les octets dans les deux sens, sans jamais les inspecter,
/// jusqu'à fermeture d'un des deux pairs (l'autre est alors fermé aussi).
fn relayer(a: TcpStream, b: TcpStream) -> io::Result<()> {
    let lecture_a = a.try_clone()?;
    let lecture_b = b.try_clone()?;
    // Sens A→B dans un thread dédié, sens B→A dans le thread courant.
    let sens_ab = thread::spawn(move || copier_puis_fermer(lecture_a, b));
    copier_puis_fermer(lecture_b, a);
    let _ = sens_ab.join();
    Ok(())
}

/// Copie opaque `source` → `destination` (via `io::copy`), puis ferme les deux
/// connexions : la déconnexion d'un pair entraîne la fermeture de l'autre.
fn copier_puis_fermer(mut source: TcpStream, mut destination: TcpStream) {
    let _ = io::copy(&mut source, &mut destination);
    // Débloque le sens opposé (les clones partagent la socket sous-jacente).
    let _ = destination.shutdown(Shutdown::Both);
    let _ = source.shutdown(Shutdown::Both);
}

// ---------------------------------------------------------------------------
// Tests d'intégration (le crate est un binaire : tests embarqués ici).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::time::Duration;

    /// Délai de garde des lectures côté client (évite qu'un test ne bloque).
    const DELAI_TEST: Duration = Duration::from_secs(5);

    /// Lance un relais sur `127.0.0.1:0` dans un thread et renvoie son adresse.
    fn demarrer_relais() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind relais");
        let adresse = listener.local_addr().expect("adresse locale");
        thread::spawn(move || {
            let _ = servir(&listener, &TicketsEnAttente::default());
        });
        adresse
    }

    /// Connecte un client au relais et envoie sa trame d'annonce de ticket.
    fn annoncer(adresse: SocketAddr, ticket: &[u8]) -> TcpStream {
        let mut stream = TcpStream::connect(adresse).expect("connexion au relais");
        stream
            .set_read_timeout(Some(DELAI_TEST))
            .expect("délai de lecture");
        stream
            .write_all(&(ticket.len() as u32).to_be_bytes())
            .expect("préfixe du ticket");
        stream.write_all(ticket).expect("ticket");
        stream
    }

    /// Lit exactement `attendu.len()` octets et vérifie leur contenu.
    fn verifier_reception(stream: &mut TcpStream, attendu: &[u8]) {
        let mut tampon = vec![0u8; attendu.len()];
        stream.read_exact(&mut tampon).expect("lecture relais");
        assert_eq!(tampon, attendu, "octets altérés par le relais");
    }

    #[test]
    fn relais_bidirectionnel_par_ticket() {
        let adresse = demarrer_relais();
        let mut a = annoncer(adresse, b"ticket-alpha");
        let mut b = annoncer(adresse, b"ticket-alpha");

        // A → relais → B, puis B → relais → A : les octets arrivent intacts.
        a.write_all(b"bonjour de A \x00\xff").expect("envoi A");
        verifier_reception(&mut b, b"bonjour de A \x00\xff");
        b.write_all(b"salut de B \x01\xfe").expect("envoi B");
        verifier_reception(&mut a, b"salut de B \x01\xfe");

        // Second aller-retour : le tuyau reste ouvert.
        a.write_all(b"encore A").expect("envoi A2");
        verifier_reception(&mut b, b"encore A");
    }

    #[test]
    fn tickets_simultanes_sans_melange() {
        let adresse = demarrer_relais();
        let mut a1 = annoncer(adresse, b"ticket-1");
        let mut a2 = annoncer(adresse, b"ticket-2");
        let mut b1 = annoncer(adresse, b"ticket-1");
        let mut b2 = annoncer(adresse, b"ticket-2");

        // Chaque paire ne voit que le trafic de son propre ticket.
        a1.write_all(b"message-paire-1").expect("envoi a1");
        a2.write_all(b"message-paire-2").expect("envoi a2");
        verifier_reception(&mut b1, b"message-paire-1");
        verifier_reception(&mut b2, b"message-paire-2");
        b2.write_all(b"retour-paire-2").expect("envoi b2");
        b1.write_all(b"retour-paire-1").expect("envoi b1");
        verifier_reception(&mut a2, b"retour-paire-2");
        verifier_reception(&mut a1, b"retour-paire-1");
    }

    #[test]
    fn deconnexion_d_un_pair_ferme_l_autre() {
        let adresse = demarrer_relais();
        let mut a = annoncer(adresse, b"ticket-fin");
        let mut b = annoncer(adresse, b"ticket-fin");

        // S'assure que la paire est bien établie avant de couper.
        a.write_all(b"ping").expect("envoi a");
        verifier_reception(&mut b, b"ping");

        // A se déconnecte : le relais doit fermer la connexion de B.
        drop(a);
        let mut reste = Vec::new();
        match b.read_to_end(&mut reste) {
            Ok(0) => {} // Fin de flux propre.
            Ok(n) => panic!("octets inattendus après déconnexion : {n}"),
            Err(_) => {} // Réinitialisation de connexion : fermeture acceptée aussi.
        }
    }

    #[test]
    fn annonce_invalide_n_empeche_pas_le_service() {
        let adresse = demarrer_relais();

        // Ticket incomplet : préfixe annonçant 16 octets, mais 4 seulement envoyés.
        let mut incomplet = TcpStream::connect(adresse).expect("connexion");
        incomplet
            .write_all(&16u32.to_be_bytes())
            .expect("préfixe incomplet");
        incomplet.write_all(b"abcd").expect("ticket tronqué");
        drop(incomplet);

        // Ticket trop grand : rejeté, la connexion est fermée par le relais.
        let mut trop_grand = TcpStream::connect(adresse).expect("connexion");
        trop_grand
            .set_read_timeout(Some(DELAI_TEST))
            .expect("délai de lecture");
        trop_grand
            .write_all(&(1u32 << 20).to_be_bytes())
            .expect("préfixe démesuré");
        let mut tampon = Vec::new();
        match trop_grand.read_to_end(&mut tampon) {
            Ok(0) | Err(_) => {} // Fermé sans avoir été apparié.
            Ok(n) => panic!("octets inattendus du relais : {n}"),
        }

        // Le relais continue de servir les annonces valides.
        let mut a = annoncer(adresse, b"ticket-sain");
        let mut b = annoncer(adresse, b"ticket-sain");
        a.write_all(b"toujours vivant").expect("envoi a");
        verifier_reception(&mut b, b"toujours vivant");
    }
}
