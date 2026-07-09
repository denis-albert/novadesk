//! Serveur de relais NovaDesk (plans 05/11 — connectivité/NAT, relais géré).
//!
//! Achemine le trafic chiffré de bout en bout entre deux pairs quand le P2P
//! échoue (NAT symétrique/CGNAT). Le relais est un **tuyau aveugle** : chaque
//! client annonce d'abord un **ticket** (trame `[u32 BE len][ticket]`) ; le
//! premier pair d'un ticket est mis en attente, et à l'arrivée du second pair
//! porteur du même ticket, le relais fait transiter les octets dans les deux
//! sens sans jamais les inspecter (le média est chiffré de bout en bout,
//! voir plan 06).
//!
//! **Tickets signés (plan 11)** : le relais n'accepte que des tickets
//! [`TicketRelais`] signés **Ed25519** par l'autorité du déploiement (clé
//! publique configurée au démarrage), avec une **portée** (paire d'IDs) et une
//! **expiration** — tout ticket non signé, altéré ou expiré est refusé.
//! L'émetteur (le courtier de session, plan 05/09) remet le **même** ticket
//! aux deux pairs ; le relais les apparie sur ses octets exacts. Le tuyau
//! reste aveugle : les IDs de la portée engagent l'émetteur et bornent le
//! rejeu, le relais n'associe jamais les octets relayés aux IDs.
//!
//! Volet « relais géré » (plan 11) : métriques d'exploitation
//! ([`RelayMetrics`]), quotas ([`ConfigRelais`] — paires actives simultanées,
//! octets par paire **bornés par défaut**, connexions simultanées par IP) et
//! sélection du relais le plus proche côté client ([`select_relay`], exposée
//! ici via le mode diagnostic `--sonder`).
//!
//! **Cas limites fermés** : l'annonce de ticket est bornée par un délai d'E/S
//! ([`DELAI_ANNONCE`] — une connexion muette ne retient ni thread ni place de
//! quota indéfiniment) ; un pair resté **en attente** au-delà de l'échéance de
//! son ticket est **purgé** (sa socket fermée, sa place IP rendue) — passé
//! cette échéance, aucun second pair ne peut de toute façon plus être accepté,
//! son annonce serait refusée comme expirée. La purge court à chaque annonce
//! et en tâche de fond ([`INTERVALLE_PURGE_ATTENTES`]). Une fois la paire
//! appariée, plus aucun délai n'est imposé au tuyau (les sessions longues et
//! silencieuses sont légitimes ; le quota d'octets borne l'abus).
//!
//! Implémentation std pure (TCP bloquant, threads), dans l'esprit de
//! `nd-signaling`.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use nd_api::auth::{cle_publique_depuis_hex, maintenant_unix, TicketRelais, VerifyingKey};

/// Adresse d'écoute par défaut du relais.
const ADRESSE_DEFAUT: &str = "0.0.0.0:9100";

/// Taille maximale acceptée pour une annonce (une annonce plus grande est
/// rejetée avant même la vérification de signature).
const TAILLE_TICKET_MAX: usize = 1024;

/// Nombre maximal de paires actives simultanées par défaut.
const MAX_PAIRES_ACTIVES_DEFAUT: usize = 1000;

/// Quota d'octets relayés par paire, par défaut : 8 Gio (deux sens confondus).
/// Une session de bureau à distance légitime tient large dessous ; un abus de
/// bande passante est coupé.
const QUOTA_OCTETS_PAR_PAIRE_DEFAUT: u64 = 8 * 1024 * 1024 * 1024;

/// Nombre maximal de connexions simultanées par adresse IP, par défaut.
const MAX_CONNEXIONS_PAR_IP_DEFAUT: usize = 32;

/// Taille des tranches de copie du pipe (comptage local, aucun partage).
const TAILLE_TRANCHE: usize = 64 * 1024;

/// Délai maximal accordé à chaque sonde RTT de [`select_relay`].
const DELAI_SONDE_RELAIS: Duration = Duration::from_millis(800);

/// Intervalle de publication des métriques sur la sortie standard.
const INTERVALLE_METRIQUES: Duration = Duration::from_secs(60);

/// Délai d'E/S accordé à la phase d'annonce (lecture du ticket) : au-delà, la
/// connexion muette est fermée et sa place de quota IP rendue. Le délai est
/// **levé** une fois le ticket vérifié — le tuyau relayé, lui, n'expire pas.
const DELAI_ANNONCE: Duration = Duration::from_secs(30);

/// Intervalle du balayage de fond des pairs en attente dont le ticket a
/// expiré (la purge court aussi, inline, à chaque annonce valide).
const INTERVALLE_PURGE_ATTENTES: Duration = Duration::from_secs(30);

/// Quotas du relais (plan 11 — relais géré).
#[derive(Clone, Copy, Debug)]
pub struct ConfigRelais {
    /// Nombre maximal de paires relayées simultanément. La connexion qui
    /// compléterait une paire au-delà est refusée proprement (fermée) et
    /// comptée ; le pair déjà en attente reste dans la table.
    pub max_paires_actives: usize,
    /// Quota d'octets relayés par paire (deux sens confondus), `None` =
    /// illimité. Atteint, le pipe est coupé (les deux pairs sont fermés).
    /// Par défaut : [`QUOTA_OCTETS_PAR_PAIRE_DEFAUT`].
    pub quota_octets_par_paire: Option<u64>,
    /// Nombre maximal de connexions simultanées depuis une même adresse IP
    /// (pairs en attente et pairs relayés confondus). Au-delà, la connexion
    /// est refusée (fermée) et comptée.
    pub max_connexions_par_ip: usize,
}

impl Default for ConfigRelais {
    fn default() -> Self {
        Self {
            max_paires_actives: MAX_PAIRES_ACTIVES_DEFAUT,
            quota_octets_par_paire: Some(QUOTA_OCTETS_PAR_PAIRE_DEFAUT),
            max_connexions_par_ip: MAX_CONNEXIONS_PAR_IP_DEFAUT,
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
    /// Annonces rejetées : ticket incomplet, non signé, altéré, expiré, ou
    /// paire refusée par le quota de paires actives.
    tickets_rejetes: AtomicU64,
    /// Connexions refusées par le quota de connexions par IP.
    connexions_rejetees: AtomicU64,
    /// Pairs en attente purgés parce que leur ticket a expiré sans que le
    /// second pair n'arrive (voir [`purger_attentes_expirees`]).
    attentes_purgees: AtomicU64,
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

    /// Comptabilise une connexion refusée par le quota par IP.
    fn rejeter_connexion(&self) {
        self.connexions_rejetees.fetch_add(1, Ordering::SeqCst);
    }

    /// Comptabilise `n` pairs en attente purgés (tickets expirés sans pair).
    fn purger_attentes(&self, n: u64) {
        self.attentes_purgees.fetch_add(n, Ordering::SeqCst);
    }

    /// Instantané lisible des compteurs, pour le journal d'exploitation et les
    /// tests.
    pub fn snapshot(&self) -> SnapshotMetriques {
        SnapshotMetriques {
            paires_actives: self.paires_actives.load(Ordering::SeqCst),
            octets_relayes: self.octets_relayes.load(Ordering::SeqCst),
            paires_servies: self.paires_servies.load(Ordering::SeqCst),
            tickets_rejetes: self.tickets_rejetes.load(Ordering::SeqCst),
            connexions_rejetees: self.connexions_rejetees.load(Ordering::SeqCst),
            attentes_purgees: self.attentes_purgees.load(Ordering::SeqCst),
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
    /// Annonces rejetées (ticket invalide/non signé/expiré ou quota atteint).
    pub tickets_rejetes: u64,
    /// Connexions refusées par le quota de connexions par IP.
    pub connexions_rejetees: u64,
    /// Pairs en attente purgés (ticket expiré sans second pair).
    pub attentes_purgees: u64,
}

impl fmt::Display for SnapshotMetriques {
    fn fmt(&self, formateur: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formateur,
            "paires actives : {}, paires servies : {}, octets relayés : {}, \
             tickets rejetés : {}, connexions rejetées : {}, attentes purgées : {}",
            self.paires_actives,
            self.paires_servies,
            self.octets_relayes,
            self.tickets_rejetes,
            self.connexions_rejetees,
            self.attentes_purgees
        )
    }
}

/// Compteurs de connexions simultanées par adresse IP (quota anti-abus).
type CompteursIp = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// Garde RAII d'une place de connexion pour une IP : la place est rendue au
/// `drop` (fin du pipe, fin d'attente ou refus en cours de route).
struct GardeIp {
    compteurs: CompteursIp,
    ip: IpAddr,
}

impl GardeIp {
    /// Réserve une place pour `ip` ; `None` si le quota `max` est atteint.
    fn prendre(compteurs: &CompteursIp, ip: IpAddr, max: usize) -> Option<Self> {
        let mut table = compteurs.lock().unwrap();
        let compte = table.entry(ip).or_insert(0);
        if *compte >= max {
            return None;
        }
        *compte += 1;
        Some(Self {
            compteurs: Arc::clone(compteurs),
            ip,
        })
    }
}

impl Drop for GardeIp {
    fn drop(&mut self) {
        let mut table = self.compteurs.lock().unwrap();
        if let Some(compte) = table.get_mut(&self.ip) {
            *compte -= 1;
            if *compte == 0 {
                table.remove(&self.ip);
            }
        }
    }
}

/// Pair en attente d'appariement : sa connexion, l'échéance de son ticket
/// (au-delà, l'entrée est purgée — voir [`purger_attentes_expirees`]) et sa
/// place de quota IP (tenue tant que la connexion vit dans la table).
struct EnAttente {
    stream: TcpStream,
    /// Échéance du ticket annoncé (secondes UNIX) : passé cet instant, aucun
    /// second pair ne peut plus être accepté, l'attente est donc vaine.
    expire_a: u64,
    _garde: GardeIp,
}

/// État partagé du relais : table d'appariement, quotas, clé de vérification
/// et métriques.
pub struct Relais {
    /// Table des pairs en attente d'appariement : octets exacts du ticket →
    /// premier pair (et sa garde de quota IP).
    en_attente: Mutex<HashMap<Vec<u8>, EnAttente>>,
    /// Quotas appliqués aux nouvelles paires et connexions.
    config: ConfigRelais,
    /// Clé publique de l'autorité qui signe les tickets de relais.
    cle_autorite: VerifyingKey,
    /// Connexions simultanées par IP (quota).
    connexions_par_ip: CompteursIp,
    /// Compteurs d'exploitation.
    metriques: RelayMetrics,
}

impl Relais {
    /// Crée l'état d'un relais avec les quotas donnés et la clé publique de
    /// l'autorité de tickets.
    fn new(config: ConfigRelais, cle_autorite: VerifyingKey) -> Self {
        Self {
            en_attente: Mutex::default(),
            config,
            cle_autorite,
            connexions_par_ip: Arc::default(),
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

    // Usage serveur : `nd-relay <cle-publique-autorite-hex> [adresse:port]`.
    // Sans clé d'autorité, le relais ne démarre pas (fermé par défaut).
    let cle_hex = arguments.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage : nd-relay <cle-publique-autorite-hex> [adresse:port] | --sonder <adresse>... \
             (clé affichée par nd-api au démarrage)",
        )
    })?;
    let cle_autorite = cle_publique_depuis_hex(cle_hex).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "clé publique d'autorité invalide (64 caractères hexadécimaux attendus)",
        )
    })?;
    let adresse = arguments
        .get(1)
        .cloned()
        .unwrap_or_else(|| ADRESSE_DEFAUT.to_string());
    let listener = TcpListener::bind(&adresse)?;
    let relais = Arc::new(Relais::new(ConfigRelais::default(), cle_autorite));
    println!(
        "nd-relay — NovaDesk (protocole v{}) — relais opaque à tickets signés en écoute sur {} \
         (quotas : {} paires actives, {:?} octets/paire, {} connexions/IP)",
        nd_proto::ProtocolVersion::CURRENT,
        listener.local_addr()?,
        relais.config.max_paires_actives,
        relais.config.quota_octets_par_paire,
        relais.config.max_connexions_par_ip
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
/// Démarre aussi le balayage de fond des pairs en attente expirés — la purge
/// inline de chaque annonce ne suffit pas quand le trafic s'arrête.
///
/// # Errors
/// Renvoie une erreur si l'acceptation d'une connexion échoue.
fn servir(listener: &TcpListener, relais: &Arc<Relais>) -> io::Result<()> {
    let pour_purge = Arc::downgrade(relais);
    thread::spawn(move || balayer_attentes_expirees(&pour_purge));
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

/// Balayage périodique des attentes expirées ; s'arrête quand le relais
/// n'existe plus (référence faible : le balayeur ne le maintient pas en vie).
fn balayer_attentes_expirees(relais: &Weak<Relais>) {
    loop {
        thread::sleep(INTERVALLE_PURGE_ATTENTES);
        let Some(relais) = relais.upgrade() else {
            return;
        };
        purger_attentes_expirees(&relais, maintenant_unix());
    }
}

/// Retire de la table les pairs en attente dont le ticket a expiré : passé
/// l'échéance, l'annonce d'un second pair serait de toute façon refusée
/// (ticket expiré), l'entrée ne fait donc plus que retenir une socket et une
/// place de quota IP. Les connexions purgées sont fermées **hors verrou**
/// (leur `drop` rend aussi la place IP) et comptées dans les métriques.
fn purger_attentes_expirees(relais: &Relais, maintenant: u64) {
    let purgees: Vec<EnAttente> = {
        let mut table = relais.en_attente.lock().unwrap();
        let cles: Vec<Vec<u8>> = table
            .iter()
            .filter(|(_, attente)| attente.expire_a <= maintenant)
            .map(|(cle, _)| cle.clone())
            .collect();
        cles.into_iter()
            .filter_map(|cle| table.remove(&cle))
            .collect()
    };
    if !purgees.is_empty() {
        relais.metriques.purger_attentes(purgees.len() as u64);
    }
    // `drop(purgees)` : fermeture des sockets et restitution des places IP.
}

/// Lit la trame d'annonce (`[u32 BE len][ticket]`), **vérifie le ticket**
/// (signature de l'autorité, expiration) et apparie la connexion.
///
/// Premier pair d'un ticket : mis en attente dans la table. Second pair : si le
/// quota de paires actives le permet, le couple est retiré de la table et le
/// relais bidirectionnel démarre ; sinon la nouvelle connexion est refusée
/// (fermée et comptée) et le premier pair reste en attente.
fn apparier(mut stream: TcpStream, relais: &Relais) -> io::Result<()> {
    // Quota de connexions par IP, avant toute lecture (anti-abus).
    let ip = stream.peer_addr()?.ip();
    let Some(garde) = GardeIp::prendre(
        &relais.connexions_par_ip,
        ip,
        relais.config.max_connexions_par_ip,
    ) else {
        relais.metriques.rejeter_connexion();
        let _ = stream.shutdown(Shutdown::Both);
        return Err(io::Error::other("quota de connexions par IP atteint"));
    };

    // L'annonce est bornée dans le temps : une connexion muette (ou qui
    // égrène son ticket octet par octet) est fermée au bout du délai, sa
    // place de quota IP rendue.
    stream.set_read_timeout(Some(DELAI_ANNONCE))?;
    stream.set_write_timeout(Some(DELAI_ANNONCE))?;
    let ticket = match lire_ticket(&mut stream) {
        Ok(ticket) => ticket,
        Err(erreur) => {
            relais.metriques.rejeter_ticket();
            return Err(erreur);
        }
    };
    // Seuls les tickets signés par l'autorité et non expirés entrent.
    let ticket_verifie =
        match TicketRelais::verifier(&ticket, &relais.cle_autorite, maintenant_unix()) {
            Ok(ticket_verifie) => ticket_verifie,
            Err(refus) => {
                relais.metriques.rejeter_ticket();
                let _ = stream.shutdown(Shutdown::Both);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    refus.to_string(),
                ));
            }
        };
    // Annonce valide : le délai d'E/S est levé — le pair peut attendre son
    // homologue, puis la paire relayer une session longue et silencieuse.
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    // Les attentes dont le ticket a expiré ne serviront plus jamais : purge
    // (leur socket est fermée, leur place IP rendue).
    purger_attentes_expirees(relais, maintenant_unix());

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
                    Some((
                        premier,
                        EnAttente {
                            stream,
                            expire_a: ticket_verifie.expire_le,
                            _garde: garde,
                        },
                    ))
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
                // Premier arrivé : sa garde IP part avec lui dans la table.
                table.insert(
                    ticket,
                    EnAttente {
                        stream,
                        expire_a: ticket_verifie.expire_le,
                        _garde: garde,
                    },
                );
                None
            }
        }
    };

    match paire {
        Some((premier, second)) => {
            // Quelle que soit l'issue du pipe, les places IP sont rendues puis
            // la place de paire est libérée et les octets crédités.
            let (flux_premier, garde_premier) = (premier.stream, premier._garde);
            let (flux_second, garde_second) = (second.stream, second._garde);
            let octets = relayer(
                flux_premier,
                flux_second,
                relais.config.quota_octets_par_paire,
            );
            drop(garde_premier);
            drop(garde_second);
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
    use nd_api::auth::Autorite;

    /// Délai de garde des lectures côté client (évite qu'un test ne bloque).
    const DELAI_TEST: Duration = Duration::from_secs(5);

    /// Autorité de test déterministe qui signe les tickets des tests.
    fn autorite_test() -> Autorite {
        Autorite::depuis_graine(&[13u8; 32])
    }

    /// Ticket signé valide pour la paire (`id_a`, `id_b`), expirant dans 60 s.
    fn ticket_signe(autorite: &Autorite, id_a: u64, id_b: u64) -> Vec<u8> {
        autorite
            .emettre_ticket_relais(id_a, id_b, maintenant_unix() + 60)
            .to_bytes()
    }

    /// Lance un relais configuré sur `127.0.0.1:0` dans un thread et renvoie
    /// son adresse, son état partagé et l'autorité qui signe ses tickets.
    fn demarrer_relais_configure(config: ConfigRelais) -> (SocketAddr, Arc<Relais>, Autorite) {
        let autorite = autorite_test();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind relais");
        let adresse = listener.local_addr().expect("adresse locale");
        let relais = Arc::new(Relais::new(config, autorite.cle_publique()));
        let pour_service = Arc::clone(&relais);
        thread::spawn(move || {
            let _ = servir(&listener, &pour_service);
        });
        (adresse, relais, autorite)
    }

    /// Lance un relais avec la configuration par défaut.
    fn demarrer_relais() -> (SocketAddr, Autorite) {
        let (adresse, _, autorite) = demarrer_relais_configure(ConfigRelais::default());
        (adresse, autorite)
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

    /// Vérifie que la connexion a été refusée : fermée sans un octet transmis.
    fn verifier_refus(stream: &mut TcpStream) {
        let mut tampon = Vec::new();
        match stream.read_to_end(&mut tampon) {
            Ok(0) | Err(_) => {} // Fermée proprement, rien reçu.
            Ok(n) => panic!("octets inattendus malgré le refus : {n}"),
        }
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

    /// Attend (avec délai de garde) que toutes les places IP soient rendues.
    fn attendre_places_ip_rendues(relais: &Relais) {
        let depart = Instant::now();
        while !relais.connexions_par_ip.lock().unwrap().is_empty() {
            assert!(
                depart.elapsed() < DELAI_TEST,
                "places IP jamais rendues après le délai de garde"
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
        let (adresse, autorite) = demarrer_relais();
        let ticket = ticket_signe(&autorite, 111, 222);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);

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
        let (adresse, autorite) = demarrer_relais();
        let ticket_1 = ticket_signe(&autorite, 1, 2);
        let ticket_2 = ticket_signe(&autorite, 3, 4);
        let mut a1 = annoncer(adresse, &ticket_1);
        let mut a2 = annoncer(adresse, &ticket_2);
        let mut b1 = annoncer(adresse, &ticket_1);
        let mut b2 = annoncer(adresse, &ticket_2);

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
        let (adresse, autorite) = demarrer_relais();
        let ticket = ticket_signe(&autorite, 5, 6);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);

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
    fn ticket_non_signe_refuse() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais::default());

        // Ticket « à l'ancienne » (octets arbitraires, non signés) : refusé.
        let mut nu = annoncer(adresse, b"ticket-alpha");
        verifier_refus(&mut nu);
        // Ticket au bon format mais altéré après signature : refusé.
        let mut altere = ticket_signe(&autorite, 1, 2);
        altere[10] ^= 1;
        let mut falsifie = annoncer(adresse, &altere);
        verifier_refus(&mut falsifie);
        // Ticket signé par une autre autorité : refusé.
        let intrus = Autorite::depuis_graine(&[77u8; 32]);
        let mut etranger = annoncer(adresse, &ticket_signe(&intrus, 1, 2));
        verifier_refus(&mut etranger);

        assert_eq!(relais.metriques.snapshot().tickets_rejetes, 3);
        // Aucun de ces refus n'a mis de pair en attente.
        assert!(relais.en_attente.lock().unwrap().is_empty());

        // Le relais continue de servir les tickets signés valides.
        let ticket = ticket_signe(&autorite, 1, 2);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);
        a.write_all(b"toujours vivant").expect("envoi a");
        verifier_reception(&mut b, b"toujours vivant");
    }

    #[test]
    fn ticket_expire_refuse() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais::default());

        // Expiré il y a dix secondes : refusé à l'annonce.
        let perime = autorite
            .emettre_ticket_relais(1, 2, maintenant_unix() - 10)
            .to_bytes();
        let mut refuse = annoncer(adresse, &perime);
        verifier_refus(&mut refuse);
        assert_eq!(relais.metriques.snapshot().tickets_rejetes, 1);

        // Un ticket de même portée mais encore valide passe, lui.
        let valide = ticket_signe(&autorite, 1, 2);
        let mut a = annoncer(adresse, &valide);
        let mut b = annoncer(adresse, &valide);
        a.write_all(b"dans les temps").expect("envoi a");
        verifier_reception(&mut b, b"dans les temps");
    }

    #[test]
    fn annonce_invalide_n_empeche_pas_le_service() {
        let (adresse, autorite) = demarrer_relais();

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
        verifier_refus(&mut trop_grand);

        // Le relais continue de servir les annonces valides.
        let ticket = ticket_signe(&autorite, 7, 8);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);
        a.write_all(b"toujours vivant").expect("envoi a");
        verifier_reception(&mut b, b"toujours vivant");
    }

    #[test]
    fn metriques_comptent_paires_et_octets() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais::default());
        let ticket = ticket_signe(&autorite, 9, 10);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);

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
        assert_eq!(instantane.connexions_rejetees, 0, "aucun refus par IP");
    }

    #[test]
    fn quota_de_paires_refuse_la_paire_en_trop() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais {
            max_paires_actives: 1,
            ..ConfigRelais::default()
        });

        // Première paire : occupe l'unique place autorisée.
        let ticket_1 = ticket_signe(&autorite, 1, 2);
        let mut a1 = annoncer(adresse, &ticket_1);
        let mut b1 = annoncer(adresse, &ticket_1);
        a1.write_all(b"occupe la place").expect("envoi a1");
        verifier_reception(&mut b1, b"occupe la place");

        // Seconde paire : le premier pair est mis en attente (ce n'est pas
        // encore une paire), puis la connexion qui la compléterait est refusée.
        let ticket_2 = ticket_signe(&autorite, 3, 4);
        let mut en_attente = annoncer(adresse, &ticket_2);
        attendre_ticket_en_attente(&relais, &ticket_2);
        let mut refuse = annoncer(adresse, &ticket_2);
        verifier_refus(&mut refuse);
        let instantane = relais.metriques.snapshot();
        assert_eq!(instantane.tickets_rejetes, 1, "refus par quota compté");
        assert_eq!(instantane.paires_actives, 1, "la première paire seule");

        // La place libérée, le pair resté en attente est enfin servi.
        drop(a1);
        drop(b1);
        attendre_paires_actives(&relais, 0);
        let mut nouveau = annoncer(adresse, &ticket_2);
        nouveau.write_all(b"seconde chance").expect("envoi nouveau");
        verifier_reception(&mut en_attente, b"seconde chance");
    }

    #[test]
    fn quota_d_octets_coupe_la_paire() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais {
            quota_octets_par_paire: Some(8),
            ..ConfigRelais::default()
        });
        let ticket = ticket_signe(&autorite, 11, 12);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);

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
    fn pair_en_attente_purge_apres_expiration_du_ticket() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais::default());

        // Un pair annonce un ticket qui expire dans une seconde ; son
        // homologue n'arrivera jamais.
        let ticket_court = autorite
            .emettre_ticket_relais(21, 22, maintenant_unix() + 1)
            .to_bytes();
        let mut abandonne = annoncer(adresse, &ticket_court);
        attendre_ticket_en_attente(&relais, &ticket_court);

        // L'échéance passe, puis une annonce valide déclenche la purge inline.
        thread::sleep(Duration::from_millis(1_600));
        let ticket_frais = ticket_signe(&autorite, 23, 24);
        let _premier_frais = annoncer(adresse, &ticket_frais);
        attendre_ticket_en_attente(&relais, &ticket_frais);

        // L'attente expirée a été retirée, comptée, et sa connexion fermée
        // (sans jamais avoir relayé le moindre octet).
        assert!(!relais
            .en_attente
            .lock()
            .unwrap()
            .contains_key(&ticket_court));
        assert_eq!(relais.metriques.snapshot().attentes_purgees, 1);
        verifier_refus(&mut abandonne);

        // Rejouer le ticket expiré est refusé à l'annonce (pas de re-dépôt).
        let mut rejoue = annoncer(adresse, &ticket_court);
        verifier_refus(&mut rejoue);
        assert_eq!(relais.metriques.snapshot().tickets_rejetes, 1);
        assert!(!relais
            .en_attente
            .lock()
            .unwrap()
            .contains_key(&ticket_court));

        // Le pair frais, lui, est toujours en attente et sera bien servi.
        let mut second_frais = annoncer(adresse, &ticket_frais);
        let mut premier_frais = _premier_frais;
        premier_frais
            .write_all(b"toujours apparie")
            .expect("envoi du pair frais");
        verifier_reception(&mut second_frais, b"toujours apparie");
    }

    #[test]
    fn quota_de_connexions_par_ip_refuse_l_exces_puis_rend_les_places() {
        let (adresse, relais, autorite) = demarrer_relais_configure(ConfigRelais {
            max_connexions_par_ip: 2,
            ..ConfigRelais::default()
        });

        // Une paire active occupe les deux places de 127.0.0.1.
        let ticket = ticket_signe(&autorite, 1, 2);
        let mut a = annoncer(adresse, &ticket);
        let mut b = annoncer(adresse, &ticket);
        a.write_all(b"place 1 et 2").expect("envoi a");
        verifier_reception(&mut b, b"place 1 et 2");

        // Troisième connexion de la même IP : refusée avant même l'annonce.
        let mut refuse = annoncer(adresse, &ticket_signe(&autorite, 3, 4));
        verifier_refus(&mut refuse);
        assert_eq!(relais.metriques.snapshot().connexions_rejetees, 1);

        // La paire se clôt : les places sont rendues, une nouvelle paire passe.
        drop(a);
        drop(b);
        attendre_paires_actives(&relais, 0);
        attendre_places_ip_rendues(&relais);
        let ticket_2 = ticket_signe(&autorite, 5, 6);
        let mut c = annoncer(adresse, &ticket_2);
        let mut d = annoncer(adresse, &ticket_2);
        c.write_all(b"places rendues").expect("envoi c");
        verifier_reception(&mut d, b"places rendues");
    }

    #[test]
    fn select_relay_ecarte_l_injoignable_et_choisit_le_joignable() {
        // Adresse injoignable : port réservé puis relâché (plus d'écouteur).
        let injoignable = {
            let ecouteur = TcpListener::bind("127.0.0.1:0").expect("bind temporaire");
            ecouteur.local_addr().expect("adresse locale")
        };
        let (joignable, _) = demarrer_relais();

        // Le candidat injoignable est écarté, le relais joignable est retenu.
        assert_eq!(select_relay(&[injoignable, joignable]), Some(joignable));
        assert_eq!(select_relay(&[joignable, injoignable]), Some(joignable));

        // Aucun candidat joignable (ou aucun candidat) : pas d'élu.
        assert_eq!(select_relay(&[injoignable]), None);
        assert_eq!(select_relay(&[]), None);
    }
}
