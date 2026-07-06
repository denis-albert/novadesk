//! Serveur de relais NovaDesk (plans 05/11 — connectivité/NAT, relais géré).
//!
//! Achemine le trafic chiffré de bout en bout entre deux pairs quand le P2P échoue
//! (NAT symétrique/CGNAT). Le relais est un **tuyau aveugle** : chaque client
//! annonce d'abord un **ticket** (trame `[u32 BE len][ticket]`) ; le premier pair
//! d'un ticket est mis en attente, et à l'arrivée du second pair porteur du même
//! ticket, le relais fait transiter les octets dans les deux sens sans jamais les
//! inspecter (le média est chiffré de bout en bout, voir plan 06).
//!
//! Volet « relais géré » (plan 11) : métriques d'exploitation ([`RelayMetrics`]),
//! quotas ([`ConfigRelais`] — paires actives simultanées, octets par paire) et
//! sélection du relais le plus proche côté client ([`select_relay`], exposée ici
//! via le mode diagnostic `--sonder`). Les tickets signés viendront au plan 11.
//!
//! Implémentation std pure (TCP bloquant, threads), dans l'esprit de
//! `nd-signaling`.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Adresse d'écoute par défaut du relais.
const ADRESSE_DEFAUT: &str = "0.0.0.0:9100";

/// Taille maximale acceptée pour un ticket (une annonce plus grande est rejetée).
const TAILLE_TICKET_MAX: usize = 1024;

/// Nombre maximal de paires actives simultanées par défaut.
const MAX_PAIRES_ACTIVES_DEFAUT: usize = 1000;

/// Taille des tranches de copie du pipe (comptage local, aucun partage).
const TAILLE_TRANCHE: usize = 64 * 1024;

/// Délai maximal accordé à chaque sonde RTT de [`select_relay`].
const DELAI_SONDE_RELAIS: Duration = Duration::from_millis(800);

/// Intervalle de publication des métriques sur la sortie standard.
const INTERVALLE_METRIQUES: Duration = Duration::from_secs(60);

/// Quotas du relais (plan 11 — relais géré).
#[derive(Clone, Copy, Debug)]
pub struct ConfigRelais {
    /// Nombre maximal de paires relayées simultanément. La connexion qui
    /// compléterait une paire au-delà est refusée proprement (fermée) et
    /// comptée ; le pair déjà en attente reste dans la table.
    pub max_paires_actives: usize,
    /// Quota d'octets relayés par paire (deux sens confondus), `None` =
    /// illimité. Atteint, le pipe est coupé (les deux pairs sont fermés).
    pub quota_octets_par_paire: Option<u64>,
}

impl Default for ConfigRelais {
    fn default() -> Self {
        Self {
            max_paires_actives: MAX_PAIRES_ACTIVES_DEFAUT,
            quota_octets_par_paire: None,
        }
    }
}

/// Compteurs d'exploitation du relais (plan 11 — relais géré).
///
/// Tous les compteurs sont atomiques et mis à jour **hors du chemin chaud** du
/// pipe : une mise à jour par événement de paire (ouverture, clôture, rejet),
/// jamais par tranche d'octets — les octets sont comptés localement par chaque
/// sens puis crédités en une fois à la clôture.
#[derive(Default)]
pub struct RelayMetrics {
    /// Paires actuellement en cours de relais.
    paires_actives: AtomicUsize,
    /// Octets relayés depuis le démarrage (deux sens confondus, cumul).
    octets_relayes: AtomicU64,
    /// Paires servies depuis le démarrage (cumul, paires closes comprises).
    paires_servies: AtomicU64,
    /// Annonces rejetées : ticket invalide/incomplet ou paire refusée par quota.
    tickets_rejetes: AtomicU64,
}

impl RelayMetrics {
    /// Tente de réserver une place de paire active ; refuse au-delà de `max`.
    /// La réservation est atomique (aucune sur-admission possible en course).
    fn ouvrir_paire(&self, max: usize) -> bool {
        let reservation =
            self.paires_actives
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |actives| {
                    (actives < max).then_some(actives + 1)
                });
        if reservation.is_ok() {
            self.paires_servies.fetch_add(1, Ordering::SeqCst);
        }
        reservation.is_ok()
    }

    /// Clôt une paire : crédite ses octets relayés puis libère sa place (dans
    /// cet ordre, pour qu'un observateur voyant la paire fermée voie ses octets).
    fn fermer_paire(&self, octets: u64) {
        self.octets_relayes.fetch_add(octets, Ordering::SeqCst);
        self.paires_actives.fetch_sub(1, Ordering::SeqCst);
    }

    /// Comptabilise une annonce rejetée (ticket invalide ou quota atteint).
    fn rejeter_ticket(&self) {
        self.tickets_rejetes.fetch_add(1, Ordering::SeqCst);
    }

    /// Instantané lisible des compteurs, pour le journal d'exploitation et les
    /// tests.
    pub fn snapshot(&self) -> SnapshotMetriques {
        SnapshotMetriques {
            paires_actives: self.paires_actives.load(Ordering::SeqCst),
            octets_relayes: self.octets_relayes.load(Ordering::SeqCst),
            paires_servies: self.paires_servies.load(Ordering::SeqCst),
            tickets_rejetes: self.tickets_rejetes.load(Ordering::SeqCst),
        }
    }
}

/// Instantané des métriques du relais à un instant donné.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotMetriques {
    /// Paires actuellement en cours de relais.
    pub paires_actives: usize,
    /// Octets relayés depuis le démarrage (deux sens confondus).
    pub octets_relayes: u64,
    /// Paires servies depuis le démarrage (cumul).
    pub paires_servies: u64,
    /// Annonces rejetées (ticket invalide ou quota atteint).
    pub tickets_rejetes: u64,
}

impl fmt::Display for SnapshotMetriques {
    fn fmt(&self, formateur: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formateur,
            "paires actives : {}, paires servies : {}, octets relayés : {}, tickets rejetés : {}",
            self.paires_actives, self.paires_servies, self.octets_relayes, self.tickets_rejetes
        )
    }
}

/// État partagé du relais : table d'appariement, quotas et métriques.
pub struct Relais {
    /// Table des pairs en attente d'appariement : ticket → connexion du premier pair.
    en_attente: Mutex<HashMap<Vec<u8>, TcpStream>>,
    /// Quotas appliqués aux nouvelles paires.
    config: ConfigRelais,
    /// Compteurs d'exploitation.
    metriques: RelayMetrics,
}

impl Relais {
    /// Crée l'état d'un relais avec les quotas donnés.
    fn new(config: ConfigRelais) -> Self {
        Self {
            en_attente: Mutex::default(),
            config,
            metriques: RelayMetrics::default(),
        }
    }
}

fn main() -> io::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // Mode diagnostic côté client : `nd-relay --sonder <adresse>...` mesure le
    // RTT vers chaque relais candidat et affiche le plus proche.
    if arguments
        .first()
        .is_some_and(|premier| premier == "--sonder")
    {
        return sonder_et_afficher(&arguments[1..]);
    }

    // Adresse d'écoute : premier argument CLI, sinon la valeur par défaut.
    let adresse = arguments
        .first()
        .cloned()
        .unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    let listener = TcpListener::bind(&adresse)?;
    let relais = Arc::new(Relais::new(ConfigRelais::default()));
    println!(
        "nd-relay — NovaDesk (protocole v{}) — relais opaque en écoute sur {} (quota : {} paires actives)",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?,
        relais.config.max_paires_actives
    );

    // Journal d'exploitation : publication périodique des métriques.
    let pour_journal = Arc::clone(&relais);
    thread::spawn(move || journaliser_metriques(&pour_journal));

    servir(&listener, &relais)
}

/// Mode `--sonder` : analyse les adresses candidates, choisit le relais le plus
/// proche via [`select_relay`] et l'affiche.
fn sonder_et_afficher(candidats_bruts: &[String]) -> io::Result<()> {
    let candidats = candidats_bruts
        .iter()
        .map(|brut| {
            brut.parse::<SocketAddr>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("adresse de relais invalide : {brut}"),
                )
            })
        })
        .collect::<io::Result<Vec<SocketAddr>>>()?;
    match select_relay(&candidats) {
        Some(adresse) => {
            println!("relais retenu : {adresse}");
            Ok(())
        }
        None => Err(io::Error::other("aucun relais candidat joignable")),
    }
}

/// Publie périodiquement un instantané des métriques sur la sortie standard.
fn journaliser_metriques(relais: &Relais) {
    loop {
        thread::sleep(INTERVALLE_METRIQUES);
        println!("nd-relay — métriques — {}", relais.metriques.snapshot());
    }
}

/// Boucle d'acceptation du relais (bloquante, un thread par connexion).
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
fn servir(listener: &TcpListener, relais: &Arc<Relais>) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let relais = Arc::clone(relais);
        thread::spawn(move || {
            // Une annonce invalide, un quota atteint ou une déconnexion précoce
            // ferme simplement la connexion fautive, sans impacter le reste.
            let _ = apparier(stream, &relais);
        });
    }
    Ok(())
}

/// Lit la trame d'annonce (`[u32 BE len][ticket]`) et apparie la connexion.
///
/// Premier pair d'un ticket : mis en attente dans la table. Second pair : si le
/// quota de paires actives le permet, le couple est retiré de la table et le
/// relais bidirectionnel démarre ; sinon la nouvelle connexion est refusée
/// (fermée et comptée) et le premier pair reste en attente.
fn apparier(mut stream: TcpStream, relais: &Relais) -> io::Result<()> {
    let ticket = match lire_ticket(&mut stream) {
        Ok(ticket) => ticket,
        Err(erreur) => {
            relais.metriques.rejeter_ticket();
            return Err(erreur);
        }
    };

    // Section critique courte : retire le pair en attente ou dépose la
    // connexion. La réservation de quota se fait sous le même verrou, afin que
    // le premier pair retourne en attente intact si la paire est refusée.
    let paire = {
        let mut table = relais.en_attente.lock().unwrap();
        match table.remove(&ticket) {
            Some(premier) => {
                if relais
                    .metriques
                    .ouvrir_paire(relais.config.max_paires_actives)
                {
                    Some((premier, stream))
                } else {
                    // Quota de paires atteint : refus propre de la nouvelle
                    // connexion, le premier pair reste en attente de son pair.
                    table.insert(ticket, premier);
                    relais.metriques.rejeter_ticket();
                    let _ = stream.shutdown(Shutdown::Both);
                    return Err(io::Error::other("quota de paires actives atteint"));
                }
            }
            None => {
                table.insert(ticket, stream);
                None
            }
        }
    };

    match paire {
        Some((premier, second)) => {
            // Quelle que soit l'issue du pipe, la place réservée est libérée et
            // les octets effectivement relayés sont crédités.
            let octets = relayer(premier, second, relais.config.quota_octets_par_paire);
            relais
                .metriques
                .fermer_paire(octets.as_ref().copied().unwrap_or(0));
            octets.map(|_| ())
        }
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
/// jusqu'à fermeture d'un des deux pairs (l'autre est alors fermé aussi) ou
/// épuisement du quota d'octets de la paire. Renvoie le total d'octets relayés
/// (deux sens confondus).
fn relayer(a: TcpStream, b: TcpStream, quota_octets: Option<u64>) -> io::Result<u64> {
    // Budget d'octets partagé entre les deux sens (quota par paire).
    let budget = quota_octets.map(|quota| Arc::new(AtomicU64::new(quota)));
    let lecture_a = a.try_clone()?;
    let lecture_b = b.try_clone()?;
    // Sens A→B dans un thread dédié, sens B→A dans le thread courant.
    let budget_ab = budget.clone();
    let sens_ab = thread::spawn(move || copier_puis_fermer(lecture_a, b, budget_ab.as_deref()));
    let octets_ba = copier_puis_fermer(lecture_b, a, budget.as_deref());
    let octets_ab = sens_ab.join().unwrap_or(0);
    Ok(octets_ab + octets_ba)
}

/// Copie opaque `source` → `destination` par tranches, puis ferme les deux
/// connexions : la déconnexion d'un pair entraîne la fermeture de l'autre.
///
/// Les octets sont comptés dans un cumul **local** (aucun compteur partagé sur
/// le chemin chaud) renvoyé à l'appelant ; seul l'éventuel `budget` de quota,
/// partagé entre les deux sens de la paire, est débité par tranche. La tranche
/// qui dépasserait le budget n'est pas transmise : le pipe est coupé net.
fn copier_puis_fermer(
    mut source: TcpStream,
    mut destination: TcpStream,
    budget: Option<&AtomicU64>,
) -> u64 {
    let mut total: u64 = 0;
    let mut tampon = [0u8; TAILLE_TRANCHE];
    loop {
        let lus = match source.read(&mut tampon) {
            Ok(0) | Err(_) => break, // Fin de flux ou erreur : on referme tout.
            Ok(lus) => lus,
        };
        if let Some(budget) = budget {
            let epuise = budget
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |restant| {
                    restant.checked_sub(lus as u64)
                })
                .is_err();
            if epuise {
                break; // Quota de la paire épuisé : coupure sans transmettre.
            }
        }
        if destination.write_all(&tampon[..lus]).is_err() {
            break;
        }
        total += lus as u64;
    }
    // Débloque le sens opposé (les clones partagent la socket sous-jacente).
    let _ = destination.shutdown(Shutdown::Both);
    let _ = source.shutdown(Shutdown::Both);
    total
}

/// Choisit le relais « le plus proche » parmi les candidats : mesure le RTT
/// d'un `connect` TCP chronométré (court délai) vers chacun, en parallèle, et
/// renvoie le plus rapide. Les candidats injoignables (connexion refusée ou
/// délai dépassé) sont écartés ; `None` si aucun candidat ne répond.
pub fn select_relay(candidats: &[SocketAddr]) -> Option<SocketAddr> {
    // Sondes en parallèle : le coût total est borné par le délai d'une seule
    // sonde, pas par la somme des délais des candidats injoignables.
    thread::scope(|portee| {
        let sondes: Vec<_> = candidats
            .iter()
            .map(|&adresse| portee.spawn(move || sonder_relais(adresse)))
            .collect();
        sondes
            .into_iter()
            .filter_map(|sonde| sonde.join().ok().flatten())
            .min_by_key(|&(rtt, _)| rtt)
            .map(|(_, adresse)| adresse)
    })
}

/// Sonde un candidat : durée d'un `connect` TCP borné par [`DELAI_SONDE_RELAIS`].
/// La connexion est refermée aussitôt (sonde pure, aucune annonce de ticket).
fn sonder_relais(adresse: SocketAddr) -> Option<(Duration, SocketAddr)> {
    let depart = Instant::now();
    let stream = TcpStream::connect_timeout(&adresse, DELAI_SONDE_RELAIS).ok()?;
    let rtt = depart.elapsed();
    let _ = stream.shutdown(Shutdown::Both);
    Some((rtt, adresse))
}

// ---------------------------------------------------------------------------
// Tests d'intégration (le crate est un binaire : tests embarqués ici).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Délai de garde des lectures côté client (évite qu'un test ne bloque).
    const DELAI_TEST: Duration = Duration::from_secs(5);

    /// Lance un relais configuré sur `127.0.0.1:0` dans un thread et renvoie
    /// son adresse ainsi que son état partagé (métriques, table d'attente).
    fn demarrer_relais_configure(config: ConfigRelais) -> (SocketAddr, Arc<Relais>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind relais");
        let adresse = listener.local_addr().expect("adresse locale");
        let relais = Arc::new(Relais::new(config));
        let pour_service = Arc::clone(&relais);
        thread::spawn(move || {
            let _ = servir(&listener, &pour_service);
        });
        (adresse, relais)
    }

    /// Lance un relais avec la configuration par défaut et renvoie son adresse.
    fn demarrer_relais() -> SocketAddr {
        demarrer_relais_configure(ConfigRelais::default()).0
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

    /// Attend (avec délai de garde) que le compteur de paires actives atteigne
    /// `attendu` — la clôture d'une paire est asynchrone côté relais.
    fn attendre_paires_actives(relais: &Relais, attendu: usize) {
        let depart = Instant::now();
        while relais.metriques.snapshot().paires_actives != attendu {
            assert!(
                depart.elapsed() < DELAI_TEST,
                "paires actives ≠ {attendu} après le délai de garde"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Attend (avec délai de garde) qu'un ticket soit enregistré en attente.
    fn attendre_ticket_en_attente(relais: &Relais, ticket: &[u8]) {
        let depart = Instant::now();
        while !relais.en_attente.lock().unwrap().contains_key(ticket) {
            assert!(
                depart.elapsed() < DELAI_TEST,
                "ticket jamais enregistré en attente"
            );
            thread::sleep(Duration::from_millis(5));
        }
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

    #[test]
    fn metriques_comptent_paires_et_octets() {
        let (adresse, relais) = demarrer_relais_configure(ConfigRelais::default());
        let mut a = annoncer(adresse, b"ticket-metriques");
        let mut b = annoncer(adresse, b"ticket-metriques");

        // Échange vérifié : 5 octets A→B puis 3 octets B→A.
        a.write_all(b"12345").expect("envoi a");
        verifier_reception(&mut b, b"12345");
        b.write_all(b"abc").expect("envoi b");
        verifier_reception(&mut a, b"abc");

        // Pendant l'échange, exactement une paire est active.
        assert_eq!(relais.metriques.snapshot().paires_actives, 1);

        // Fermeture des deux pairs : la paire se clôt et crédite ses octets.
        drop(a);
        drop(b);
        attendre_paires_actives(&relais, 0);
        let instantane = relais.metriques.snapshot();
        assert_eq!(instantane.paires_servies, 1, "une paire servie (cumul)");
        assert_eq!(instantane.octets_relayes, 8, "5 + 3 octets relayés");
        assert_eq!(instantane.tickets_rejetes, 0, "aucune annonce rejetée");
    }

    #[test]
    fn quota_de_paires_refuse_la_paire_en_trop() {
        let (adresse, relais) = demarrer_relais_configure(ConfigRelais {
            max_paires_actives: 1,
            ..ConfigRelais::default()
        });

        // Première paire : occupe l'unique place autorisée.
        let mut a1 = annoncer(adresse, b"quota-1");
        let mut b1 = annoncer(adresse, b"quota-1");
        a1.write_all(b"occupe la place").expect("envoi a1");
        verifier_reception(&mut b1, b"occupe la place");

        // Seconde paire : le premier pair est mis en attente (ce n'est pas
        // encore une paire), puis la connexion qui la compléterait est refusée.
        let mut en_attente = annoncer(adresse, b"quota-2");
        attendre_ticket_en_attente(&relais, b"quota-2");
        let mut refuse = annoncer(adresse, b"quota-2");
        let mut tampon = Vec::new();
        match refuse.read_to_end(&mut tampon) {
            Ok(0) | Err(_) => {} // Refusée proprement : fermée sans un octet.
            Ok(n) => panic!("octets inattendus malgré le quota : {n}"),
        }
        let instantane = relais.metriques.snapshot();
        assert_eq!(instantane.tickets_rejetes, 1, "refus par quota compté");
        assert_eq!(instantane.paires_actives, 1, "la première paire seule");

        // La place libérée, le pair resté en attente est enfin servi.
        drop(a1);
        drop(b1);
        attendre_paires_actives(&relais, 0);
        let mut nouveau = annoncer(adresse, b"quota-2");
        nouveau.write_all(b"seconde chance").expect("envoi nouveau");
        verifier_reception(&mut en_attente, b"seconde chance");
    }

    #[test]
    fn quota_d_octets_coupe_la_paire() {
        let (adresse, relais) = demarrer_relais_configure(ConfigRelais {
            quota_octets_par_paire: Some(8),
            ..ConfigRelais::default()
        });
        let mut a = annoncer(adresse, b"quota-octets");
        let mut b = annoncer(adresse, b"quota-octets");

        // 8 octets : pile le budget de la paire, tout passe.
        a.write_all(b"12345678").expect("envoi sous quota");
        verifier_reception(&mut b, b"12345678");

        // Le moindre octet supplémentaire coupe le pipe sans être transmis.
        a.write_all(b"depassement").expect("envoi au-delà du quota");
        let mut reste = Vec::new();
        match b.read_to_end(&mut reste) {
            Ok(0) | Err(_) => {} // Paire coupée proprement.
            Ok(n) => panic!("octets relayés au-delà du quota : {n}"),
        }

        // Seuls les octets sous quota sont crédités aux métriques.
        drop(a);
        drop(b);
        attendre_paires_actives(&relais, 0);
        let instantane = relais.metriques.snapshot();
        assert_eq!(instantane.octets_relayes, 8, "octets sous quota seulement");
        assert_eq!(instantane.paires_servies, 1);
    }

    #[test]
    fn select_relay_ecarte_l_injoignable_et_choisit_le_joignable() {
        // Adresse injoignable : port réservé puis relâché (plus d'écouteur).
        let injoignable = {
            let ecouteur = TcpListener::bind("127.0.0.1:0").expect("bind temporaire");
            ecouteur.local_addr().expect("adresse locale")
        };
        let joignable = demarrer_relais();

        // Le candidat injoignable est écarté, le relais joignable est retenu.
        assert_eq!(select_relay(&[injoignable, joignable]), Some(joignable));
        assert_eq!(select_relay(&[joignable, injoignable]), Some(joignable));

        // Aucun candidat joignable (ou aucun candidat) : pas d'élu.
        assert_eq!(select_relay(&[injoignable]), None);
        assert_eq!(select_relay(&[]), None);
    }
}
