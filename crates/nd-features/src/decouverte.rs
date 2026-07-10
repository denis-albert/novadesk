//! Découverte de pairs sur le réseau local : un beacon de présence en
//! **multicast UDP**, pur `std`, sans droits administrateur — la brique qui
//! alimente l'onglet « Découverts » de l'UI.
//!
//! Deux moitiés indépendantes, chacune sur son fil :
//! [`AnnonceurPresence`] émet périodiquement l'annonce `(id, nom)` du poste,
//! [`EcouteurPresence`] collecte les annonces des voisins et tient la liste
//! des pairs vivants ([`EcouteurPresence::pairs`]), **dédupliqués par id** et
//! **expirés** après [`TTL_PAIR`] sans nouvelle annonce.
//!
//! # Choix du groupe multicast
//!
//! Les annonces partent vers [`GROUPE_MULTICAST`] (`239.255.42.99`), une
//! adresse de la plage « IPv4 Local Scope » (`239.255.0.0/16`, RFC 2365) :
//! portée administrative locale, jamais routée hors du site — précisément la
//! plage prévue pour les protocoles applicatifs locaux. On n'émet **pas** sur
//! `224.0.0.251` (mDNS, RFC 6762) : ce groupe appartient au vrai DNS-SD et y
//! parler un protocole maison perturberait les piles mDNS du réseau. Le TTL
//! multicast vaut 1 : les annonces ne franchissent aucun routeur. Le port est
//! choisi par l'appelant ([`PORT_DECOUVERTE_DEFAUT`] à défaut), identique sur
//! tout le parc.
//!
//! # Format du datagramme de présence (version 1)
//!
//! | Décalage | Taille | Champ                                                       |
//! |----------|--------|-------------------------------------------------------------|
//! | 0        | 4      | magie `b"NDPR"`                                             |
//! | 4        | 1      | version du format (`1`)                                     |
//! | 5        | 8      | id NovaDesk (u64 grand-boutiste)                            |
//! | 13       | 2      | longueur du nom en octets (u16 grand-boutiste, ≤ 64)        |
//! | 15       | n      | nom d'affichage UTF-8 (≤ [`NOM_MAX_OCTETS`] octets)         |
//!
//! Des octets excédentaires après le nom sont tolérés (extensions futures de
//! la même version). Tout le reste — magie ou version inconnue, en-tête
//! tronqué, longueur incohérente, UTF-8 invalide, caractères de contrôle —
//! rend le datagramme **ignoré** sans paniquer ([`decoder_presence`] renvoie
//! `None`).
//!
//! ⚠️ Les annonces ne sont **ni signées ni chiffrées** : `id` et `nom` sont
//! purement indicatifs (affichage). L'authentification d'un pair passe par la
//! poignée de main chiffrée normale (`nd-crypto`), jamais par la découverte.
//!
//! # Robustesse et replis (multicast indisponible)
//!
//! - **Annonceur** : si l'envoi multicast échoue (pas de réseau, multicast
//!   filtré), le même tick retente en **diffusion limitée**
//!   (`255.255.255.255:port`), souvent laissée passer là où le multicast est
//!   filtré ; l'échec des deux est compté
//!   ([`AnnonceurPresence::echecs_emission`]) et le beacon réessaie au tick
//!   suivant — jamais fatal, jamais bloquant.
//! - **Écouteur** : l'adhésion au groupe est *best-effort* (tentée sur
//!   l'interface par défaut puis sur la boucle locale). Si elle échoue
//!   ([`EcouteurPresence::multicast_actif`] vaut `false`), le socket reste lié
//!   au port et continue de recevoir les annonces arrivées en diffusion ou en
//!   direct ([`AnnonceurPresence::demarrer_vers`]).
//! - **Anti-inondation** : la table des pairs est plafonnée à [`MAX_PAIRS`]
//!   entrées vivantes ; au-delà, les ids inconnus sont ignorés jusqu'à ce que
//!   des entrées expirent.
//! - IPv4 seulement pour ce jet ; un groupe IPv6 (`ff02::/16`) pourra
//!   s'ajouter sans toucher au format des datagrammes.
//!
//! # Exemple
//!
//! ```no_run
//! use nd_features::decouverte::{
//!     AnnonceurPresence, EcouteurPresence, OptionsEcoute, PORT_DECOUVERTE_DEFAUT,
//! };
//! use nd_proto::NovaId;
//!
//! # fn main() -> nd_proto::Result<()> {
//! let moi = NovaId(123_456_789);
//! // J'annonce ma présence…
//! let _annonceur = AnnonceurPresence::demarrer(moi, "Mon poste", PORT_DECOUVERTE_DEFAUT)?;
//! // …et je découvre les voisins (ma propre annonce exclue).
//! let ecouteur = EcouteurPresence::demarrer_avec(
//!     PORT_DECOUVERTE_DEFAUT,
//!     OptionsEcoute {
//!         exclure: Some(moi),
//!         ..OptionsEcoute::default()
//!     },
//! )?;
//! for pair in ecouteur.pairs() {
//!     println!("{} — {} ({})", pair.id, pair.nom, pair.adresse);
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nd_proto::{NovaId, Result};

/// Groupe multicast des annonces de présence — plage « IPv4 Local Scope »
/// (RFC 2365), voir le choix documenté en tête de module.
pub const GROUPE_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);

/// Port UDP proposé par défaut pour la découverte (miroir mnémonique du
/// groupe `…42.99` ; aucune assignation IANA connue). L'appelant reste libre
/// d'en choisir un autre, pourvu qu'il soit le même sur tout le parc.
pub const PORT_DECOUVERTE_DEFAUT: u16 = 42_099;

/// Cadence d'émission des annonces de présence (première émission immédiate).
pub const PERIODE_ANNONCE: Duration = Duration::from_secs(2);

/// Durée sans nouvelle annonce au bout de laquelle un pair est retiré de la
/// liste : cinq [`PERIODE_ANNONCE`], soit jusqu'à quatre datagrammes perdus
/// tolérés avant de déclarer un voisin parti.
pub const TTL_PAIR: Duration = Duration::from_secs(10);

/// Taille maximale du nom d'affichage annoncé, en octets UTF-8 (tronqué sur
/// une frontière de caractère au-delà).
pub const NOM_MAX_OCTETS: usize = 64;

/// Plafond d'entrées vivantes de la table des pairs (anti-inondation) : les
/// ids inconnus reçus table pleine sont ignorés jusqu'à expiration d'entrées.
pub const MAX_PAIRS: usize = 512;

/// Magie ouvrant chaque datagramme de présence.
const MAGIE: [u8; 4] = *b"NDPR";
/// Version du format de datagramme (voir le tableau en tête de module).
const VERSION: u8 = 1;
/// Taille de l'en-tête fixe : magie (4) + version (1) + id (8) + longueur (2).
const ENTETE_OCTETS: usize = 15;
/// Délai de lecture du socket d'écoute : borne l'attente entre deux
/// vérifications du drapeau d'arrêt (réactivité de l'arrêt ≤ ce délai).
const DELAI_SCRUTATION: Duration = Duration::from_millis(200);

/// Pair NovaDesk vu sur le réseau local via ses annonces de présence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairDecouvert {
    /// Identifiant NovaDesk annoncé (indicatif : non authentifié).
    pub id: NovaId,
    /// Nom d'affichage annoncé (indicatif : non authentifié).
    pub nom: String,
    /// Adresse source de la dernière annonce reçue. L'IP identifie la
    /// machine ; le port est celui, éphémère, de son annonceur.
    pub adresse: SocketAddr,
    /// Réception de la dernière annonce (horloge monotone locale).
    pub vu_le: Instant,
}

// ---------------------------------------------------------------------------
// Encodage / décodage du datagramme de présence
// ---------------------------------------------------------------------------

/// Encode l'annonce de présence `(id, nom)` au format documenté en tête de
/// module. Le nom est tronqué à [`NOM_MAX_OCTETS`] octets sur une frontière
/// de caractère UTF-8.
#[must_use]
pub fn encoder_presence(id: NovaId, nom: &str) -> Vec<u8> {
    let nom = tronquer_utf8(nom, NOM_MAX_OCTETS);
    let mut datagramme = Vec::with_capacity(ENTETE_OCTETS + nom.len());
    datagramme.extend_from_slice(&MAGIE);
    datagramme.push(VERSION);
    datagramme.extend_from_slice(&id.as_u64().to_be_bytes());
    // Borné à NOM_MAX_OCTETS (64) : tient toujours sur u16.
    datagramme.extend_from_slice(&(nom.len() as u16).to_be_bytes());
    datagramme.extend_from_slice(nom.as_bytes());
    datagramme
}

/// Décode une annonce de présence ; `None` si le datagramme est malformé.
///
/// Aucun cas ne panique : magie ou version inconnue, en-tête tronqué,
/// longueur incohérente, UTF-8 invalide ou caractère de contrôle dans le nom
/// (protège l'UI d'injections d'échappements) ⇒ datagramme ignoré. Les
/// octets excédentaires après le nom sont tolérés (extensions futures).
#[must_use]
pub fn decoder_presence(datagramme: &[u8]) -> Option<(NovaId, String)> {
    if datagramme.len() < ENTETE_OCTETS || datagramme[..4] != MAGIE || datagramme[4] != VERSION {
        return None;
    }
    let id = u64::from_be_bytes(datagramme[5..13].try_into().ok()?);
    let nom_octets = usize::from(u16::from_be_bytes(datagramme[13..15].try_into().ok()?));
    if nom_octets > NOM_MAX_OCTETS {
        return None;
    }
    let corps = &datagramme[ENTETE_OCTETS..];
    if corps.len() < nom_octets {
        return None;
    }
    let nom = std::str::from_utf8(&corps[..nom_octets]).ok()?;
    if nom.chars().any(char::is_control) {
        return None;
    }
    Some((NovaId(id), nom.to_owned()))
}

/// Tronque `texte` à au plus `max` octets, sur une frontière de caractère.
fn tronquer_utf8(texte: &str, max: usize) -> &str {
    if texte.len() <= max {
        return texte;
    }
    let mut fin = max;
    while !texte.is_char_boundary(fin) {
        fin -= 1;
    }
    &texte[..fin]
}

// ---------------------------------------------------------------------------
// Annonceur
// ---------------------------------------------------------------------------

/// Signal d'arrêt partagé avec le fil d'annonce : un booléen sous mutex et sa
/// condvar, pour interrompre **immédiatement** l'attente entre deux émissions
/// (pas de scrutation, arrêt sans délai même avec une longue période).
#[derive(Debug)]
struct SignalArret {
    demande: Mutex<bool>,
    reveil: Condvar,
}

impl SignalArret {
    fn new() -> Self {
        SignalArret {
            demande: Mutex::new(false),
            reveil: Condvar::new(),
        }
    }

    /// Demande l'arrêt et réveille le fil en attente.
    fn demander(&self) {
        *self.demande.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.reveil.notify_all();
    }

    /// Attend `delai` ou la demande d'arrêt (réveils parasites absorbés) ;
    /// renvoie `true` si l'arrêt a été demandé.
    fn attendre(&self, delai: Duration) -> bool {
        let garde = self.demande.lock().unwrap_or_else(PoisonError::into_inner);
        let (garde, _) = self
            .reveil
            .wait_timeout_while(garde, delai, |arret| !*arret)
            .unwrap_or_else(PoisonError::into_inner);
        *garde
    }
}

/// Compteurs du beacon, partagés entre le fil d'annonce et les accesseurs.
#[derive(Debug, Default)]
struct CompteursAnnonce {
    emises: AtomicU64,
    echecs: AtomicU64,
}

/// Destination des annonces.
#[derive(Debug, Clone, Copy)]
enum CibleAnnonce {
    /// [`GROUPE_MULTICAST`] sur ce port ; chaque tick en échec retente
    /// aussitôt en diffusion limitée (`255.255.255.255`), voir tête de module.
    Multicast(u16),
    /// Adresse explicite (unicast ou diffusion), sans repli.
    Directe(SocketAddr),
}

/// Beacon de présence : émet l'annonce `(id, nom)` du poste toutes les
/// périodes depuis un fil dédié (première émission immédiate). S'arrête via
/// [`AnnonceurPresence::arreter`] ou à la destruction (best-effort).
#[derive(Debug)]
pub struct AnnonceurPresence {
    arret: Arc<SignalArret>,
    compteurs: Arc<CompteursAnnonce>,
    fil: Option<JoinHandle<()>>,
}

impl AnnonceurPresence {
    /// Démarre le beacon : annonce `(id, nom)` toutes les [`PERIODE_ANNONCE`]
    /// vers [`GROUPE_MULTICAST`]`:port`.
    ///
    /// Seule l'ouverture du socket local peut échouer ; les échecs d'émission
    /// ultérieurs (réseau absent, multicast filtré…) ne sont **pas** fatals :
    /// ils sont comptés ([`AnnonceurPresence::echecs_emission`]) et le beacon
    /// réessaie au tick suivant.
    pub fn demarrer(id: NovaId, nom: &str, port: u16) -> Result<Self> {
        Self::demarrer_avec_periode(id, nom, port, PERIODE_ANNONCE)
    }

    /// Variante de [`AnnonceurPresence::demarrer`] à cadence choisie (tests,
    /// économie d'énergie). La période est plancher à 10 ms pour ne pas
    /// inonder le réseau.
    pub fn demarrer_avec_periode(
        id: NovaId,
        nom: &str,
        port: u16,
        periode: Duration,
    ) -> Result<Self> {
        Self::lancer(id, nom, CibleAnnonce::Multicast(port), periode)
    }

    /// Annonce vers une adresse explicite (unicast ou diffusion) au lieu du
    /// groupe multicast : repli quand le multicast est indisponible, relais
    /// de présence, tests sur la boucle locale.
    pub fn demarrer_vers(
        id: NovaId,
        nom: &str,
        destination: SocketAddr,
        periode: Duration,
    ) -> Result<Self> {
        Self::lancer(id, nom, CibleAnnonce::Directe(destination), periode)
    }

    /// Tronc commun : ouvre le socket d'émission puis lance le fil du beacon.
    fn lancer(id: NovaId, nom: &str, cible: CibleAnnonce, periode: Duration) -> Result<Self> {
        // Socket éphémère : seul l'envoi compte, l'OS choisit port et interface.
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        // Réglages best-effort : leur échec n'empêche pas d'annoncer.
        let _ = socket.set_multicast_ttl_v4(1); // portée : sous-réseau local uniquement
        let _ = socket.set_multicast_loop_v4(true); // la machine locale s'entend elle-même
        let _ = socket.set_broadcast(true); // autorise le repli en diffusion limitée

        let message = encoder_presence(id, nom);
        let periode = periode.max(Duration::from_millis(10));
        let arret = Arc::new(SignalArret::new());
        let compteurs = Arc::new(CompteursAnnonce::default());
        let fil = thread::Builder::new()
            .name("nd-decouverte-annonce".into())
            .spawn({
                let arret = Arc::clone(&arret);
                let compteurs = Arc::clone(&compteurs);
                move || loop {
                    emettre(&socket, &message, cible, &compteurs);
                    if arret.attendre(periode) {
                        break;
                    }
                }
            })?;
        Ok(AnnonceurPresence {
            arret,
            compteurs,
            fil: Some(fil),
        })
    }

    /// Annonces effectivement remises au système depuis le démarrage.
    #[must_use]
    pub fn annonces_emises(&self) -> u64 {
        self.compteurs.emises.load(Ordering::Relaxed)
    }

    /// Ticks dont l'émission a échoué (multicast **et** repli diffusion) —
    /// non fatal : le beacon réessaie au tick suivant.
    #[must_use]
    pub fn echecs_emission(&self) -> u64 {
        self.compteurs.echecs.load(Ordering::Relaxed)
    }

    /// Arrête le beacon et attend la fin de son fil. Retour rapide :
    /// l'attente entre deux émissions est interrompue immédiatement.
    pub fn arreter(mut self) {
        self.stopper();
    }

    /// Arrêt idempotent partagé entre [`AnnonceurPresence::arreter`] et `Drop`.
    fn stopper(&mut self) {
        self.arret.demander();
        if let Some(fil) = self.fil.take() {
            // Best-effort : un fil qui aurait paniqué est déjà terminé.
            let _ = fil.join();
        }
    }
}

impl Drop for AnnonceurPresence {
    fn drop(&mut self) {
        self.stopper();
    }
}

/// Émet `message` vers `cible` et crédite les compteurs. Pour la cible
/// multicast, un échec est retenté aussitôt en diffusion limitée : les
/// réseaux qui filtrent le multicast laissent souvent passer la diffusion de
/// sous-réseau. Jamais d'erreur remontée : l'émission est best-effort.
fn emettre(socket: &UdpSocket, message: &[u8], cible: CibleAnnonce, compteurs: &CompteursAnnonce) {
    let resultat = match cible {
        CibleAnnonce::Multicast(port) => socket
            .send_to(message, (GROUPE_MULTICAST, port))
            .or_else(|_| socket.send_to(message, (Ipv4Addr::BROADCAST, port))),
        CibleAnnonce::Directe(destination) => socket.send_to(message, destination),
    };
    if resultat.is_ok() {
        compteurs.emises.fetch_add(1, Ordering::Relaxed);
    } else {
        compteurs.echecs.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Écouteur
// ---------------------------------------------------------------------------

/// Table des pairs vus : logique pure (sans réseau ni horloge implicite,
/// l'instant courant est toujours injecté), ce qui rend la déduplication et
/// l'expiration testables unitairement même sans multicast disponible.
#[derive(Debug)]
struct TablePairs {
    ttl: Duration,
    exclu: Option<NovaId>,
    pairs: HashMap<NovaId, PairDecouvert>,
}

impl TablePairs {
    fn new(ttl: Duration, exclu: Option<NovaId>) -> Self {
        TablePairs {
            ttl,
            exclu,
            pairs: HashMap::new(),
        }
    }

    /// Enregistre une annonce : nouvelle entrée ou **rafraîchissement** (même
    /// id ⇒ une seule entrée, nom/adresse/date mis à jour). Le pair local
    /// (`exclu`) n'entre jamais ; table pleine ([`MAX_PAIRS`]), les ids
    /// inconnus sont ignorés (anti-inondation).
    fn enregistrer(&mut self, id: NovaId, nom: String, adresse: SocketAddr, vu_le: Instant) {
        if self.exclu == Some(id) {
            return;
        }
        // Purge opportuniste : la table reste bornée même si personne ne lit.
        self.purger(vu_le);
        if self.pairs.len() >= MAX_PAIRS && !self.pairs.contains_key(&id) {
            return;
        }
        self.pairs.insert(
            id,
            PairDecouvert {
                id,
                nom,
                adresse,
                vu_le,
            },
        );
    }

    /// Retire les pairs dont la dernière annonce a plus de `ttl`.
    fn purger(&mut self, maintenant: Instant) {
        // `duration_since` sature à zéro si `vu_le` est « dans le futur ».
        self.pairs
            .retain(|_, pair| maintenant.duration_since(pair.vu_le) < self.ttl);
    }

    /// Instantané des pairs encore vivants à `maintenant`, trié par id.
    fn instantane(&mut self, maintenant: Instant) -> Vec<PairDecouvert> {
        self.purger(maintenant);
        let mut liste: Vec<PairDecouvert> = self.pairs.values().cloned().collect();
        liste.sort_unstable_by_key(|pair| pair.id);
        liste
    }
}

/// Compteurs de l'écouteur, partagés entre le fil de réception et les
/// accesseurs.
#[derive(Debug, Default)]
struct CompteursEcoute {
    recus: AtomicU64,
    ignores: AtomicU64,
}

/// Réglages de l'écouteur — [`OptionsEcoute::default`] reprend les valeurs de
/// production ([`TTL_PAIR`], aucune exclusion).
#[derive(Debug, Clone, Copy)]
pub struct OptionsEcoute {
    /// Durée sans annonce après laquelle un pair disparaît de
    /// [`EcouteurPresence::pairs`].
    pub ttl: Duration,
    /// Id local à exclure de la liste (sa propre annonce, entendue en boucle).
    pub exclure: Option<NovaId>,
}

impl Default for OptionsEcoute {
    fn default() -> Self {
        OptionsEcoute {
            ttl: TTL_PAIR,
            exclure: None,
        }
    }
}

/// Écouteur de présence : collecte les annonces reçues sur son port et tient
/// la table des pairs — dédupliqués par id, expirés après le TTL, le pair
/// local exclu — exposée par [`EcouteurPresence::pairs`]. S'arrête via
/// [`EcouteurPresence::arreter`] ou à la destruction (best-effort).
#[derive(Debug)]
pub struct EcouteurPresence {
    arret: Arc<AtomicBool>,
    table: Arc<Mutex<TablePairs>>,
    compteurs: Arc<CompteursEcoute>,
    adresse_locale: SocketAddr,
    multicast_actif: bool,
    fil: Option<JoinHandle<()>>,
}

impl EcouteurPresence {
    /// Démarre l'écoute sur `0.0.0.0:port` avec les réglages par défaut.
    /// Pour exclure sa propre annonce ou changer le TTL, passer par
    /// [`EcouteurPresence::demarrer_avec`].
    pub fn demarrer(port: u16) -> Result<Self> {
        Self::demarrer_avec(port, OptionsEcoute::default())
    }

    /// Démarre l'écoute avec `options` (TTL choisi, id local exclu).
    ///
    /// Le socket est lié à `0.0.0.0:port` (`port` 0 : port choisi par l'OS,
    /// relu via [`EcouteurPresence::adresse_locale`]) ; l'échec de liaison
    /// (port déjà occupé) est renvoyé tel quel. L'adhésion au groupe
    /// [`GROUPE_MULTICAST`] est *best-effort* : si elle échoue, l'écoute
    /// continue en diffusion/direct et
    /// [`EcouteurPresence::multicast_actif`] vaut `false` (repli documenté en
    /// tête de module).
    pub fn demarrer_avec(port: u16, options: OptionsEcoute) -> Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
        let adresse_locale = socket.local_addr()?;
        // Indispensable à l'arrêt propre : borne chaque attente de lecture,
        // le fil re-vérifie donc le drapeau au moins toutes les 200 ms.
        socket.set_read_timeout(Some(DELAI_SCRUTATION))?;
        // Adhésions best-effort : interface par défaut, puis boucle locale
        // (couvre les machines sans autre interface active).
        let adhesion_defaut = socket.join_multicast_v4(&GROUPE_MULTICAST, &Ipv4Addr::UNSPECIFIED);
        let adhesion_boucle = socket.join_multicast_v4(&GROUPE_MULTICAST, &Ipv4Addr::LOCALHOST);
        let multicast_actif = adhesion_defaut.is_ok() || adhesion_boucle.is_ok();
        let _ = socket.set_multicast_loop_v4(true);

        let arret = Arc::new(AtomicBool::new(false));
        let table = Arc::new(Mutex::new(TablePairs::new(options.ttl, options.exclure)));
        let compteurs = Arc::new(CompteursEcoute::default());
        let fil = thread::Builder::new()
            .name("nd-decouverte-ecoute".into())
            .spawn({
                let arret = Arc::clone(&arret);
                let table = Arc::clone(&table);
                let compteurs = Arc::clone(&compteurs);
                move || boucle_ecoute(&socket, &arret, &table, &compteurs)
            })?;
        Ok(EcouteurPresence {
            arret,
            table,
            compteurs,
            adresse_locale,
            multicast_actif,
            fil: Some(fil),
        })
    }

    /// Instantané des pairs vivants — dédupliqués par id, expirés au-delà du
    /// TTL, le pair local exclu — trié par id croissant.
    #[must_use]
    pub fn pairs(&self) -> Vec<PairDecouvert> {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .instantane(Instant::now())
    }

    /// Adresse réellement liée (utile quand `port` valait 0).
    #[must_use]
    pub fn adresse_locale(&self) -> SocketAddr {
        self.adresse_locale
    }

    /// L'adhésion au groupe [`GROUPE_MULTICAST`] a-t-elle réussi sur au moins
    /// une interface ? `false` ⇒ repli : seules les annonces en diffusion ou
    /// en direct parviennent encore à cet écouteur.
    #[must_use]
    pub fn multicast_actif(&self) -> bool {
        self.multicast_actif
    }

    /// Datagrammes reçus sur le port depuis le démarrage (valides ou non).
    #[must_use]
    pub fn datagrammes_recus(&self) -> u64 {
        self.compteurs.recus.load(Ordering::Relaxed)
    }

    /// Datagrammes reçus puis ignorés car malformés (voir
    /// [`decoder_presence`]).
    #[must_use]
    pub fn datagrammes_ignores(&self) -> u64 {
        self.compteurs.ignores.load(Ordering::Relaxed)
    }

    /// Arrête l'écoute et attend la fin de son fil (≤ un délai de scrutation,
    /// soit 200 ms).
    pub fn arreter(mut self) {
        self.stopper();
    }

    /// Arrêt idempotent partagé entre [`EcouteurPresence::arreter`] et `Drop`.
    fn stopper(&mut self) {
        self.arret.store(true, Ordering::Relaxed);
        if let Some(fil) = self.fil.take() {
            let _ = fil.join();
        }
    }
}

impl Drop for EcouteurPresence {
    fn drop(&mut self) {
        self.stopper();
    }
}

/// Boucle de réception : décode chaque datagramme et alimente la table. Les
/// datagrammes malformés sont comptés puis ignorés ; aucune erreur réseau ne
/// panique ni ne termine la boucle (seul le drapeau d'arrêt la termine).
fn boucle_ecoute(
    socket: &UdpSocket,
    arret: &AtomicBool,
    table: &Mutex<TablePairs>,
    compteurs: &CompteursEcoute,
) {
    // Large marge au-delà des 79 octets maximum du format v1. Un datagramme
    // plus grand est tronqué (Unix) ou rejeté en erreur (Windows) : deux
    // issues bénignes, le message étant de toute façon hors format.
    let mut tampon = [0u8; 512];
    while !arret.load(Ordering::Relaxed) {
        match socket.recv_from(&mut tampon) {
            Ok((longueur, source)) => {
                compteurs.recus.fetch_add(1, Ordering::Relaxed);
                match decoder_presence(&tampon[..longueur]) {
                    Some((id, nom)) => table
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .enregistrer(id, nom, source, Instant::now()),
                    None => {
                        compteurs.ignores.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            // Délai de lecture échu : simple tour de boucle (contrôle d'arrêt).
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            // Erreur transitoire (ex. ICMP « port unreachable » renvoyé à ce
            // socket sous Windows, WSAECONNRESET) : courte pause pour ne pas
            // tourner à vide, puis on continue d'écouter.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adresse(port: u16) -> SocketAddr {
        (Ipv4Addr::LOCALHOST, port).into()
    }

    // --- Encodage / décodage -------------------------------------------------

    #[test]
    fn encodage_decodage_aller_retour() {
        let datagramme = encoder_presence(NovaId(123_456_789), "Poste Café ☕");
        assert_eq!(datagramme.len(), ENTETE_OCTETS + "Poste Café ☕".len());
        let (id, nom) = decoder_presence(&datagramme).expect("annonce valide");
        assert_eq!(id, NovaId(123_456_789));
        assert_eq!(nom, "Poste Café ☕");
    }

    #[test]
    fn decodage_tolere_les_octets_excedentaires() {
        // Extensions futures de la version 1 : les octets après le nom sont ignorés.
        let mut datagramme = encoder_presence(NovaId(7), "a");
        datagramme.extend_from_slice(b"extension future");
        assert_eq!(decoder_presence(&datagramme), Some((NovaId(7), "a".into())));
    }

    #[test]
    fn nom_tronque_sur_frontiere_utf8() {
        // 33 « é » = 66 octets : la troncature à 64 doit tomber entre deux
        // caractères, jamais au milieu d'une séquence UTF-8.
        let long = "é".repeat(33);
        let datagramme = encoder_presence(NovaId(1), &long);
        let (_, nom) = decoder_presence(&datagramme).expect("annonce valide");
        assert_eq!(nom, "é".repeat(32));
        assert_eq!(nom.len(), NOM_MAX_OCTETS);
    }

    #[test]
    fn troncature_ne_coupe_jamais_un_caractere() {
        assert_eq!(tronquer_utf8("héhé", 2), "h"); // 2 tombe au milieu du « é »
        assert_eq!(tronquer_utf8("héhé", 3), "hé");
        assert_eq!(tronquer_utf8("héhé", 4), "héh");
        assert_eq!(tronquer_utf8("abc", 10), "abc");
    }

    #[test]
    fn decodage_rejette_les_datagrammes_malformes() {
        let valide = encoder_presence(NovaId(42), "poste");
        let derniere = valide.len() - 1;

        let mut magie = valide.clone();
        magie[0] = b'X';
        let mut version = valide.clone();
        version[4] = 99;
        let mut longueur_demesuree = valide.clone();
        longueur_demesuree[13] = 0xFF;
        longueur_demesuree[14] = 0xFF;
        let mut longueur_incoherente = valide.clone();
        longueur_incoherente[14] += 1; // annonce 6 octets de nom, n'en porte que 5
        let mut nom_non_utf8 = valide.clone();
        nom_non_utf8[derniere] = 0xFF;
        let mut caractere_de_controle = valide.clone();
        caractere_de_controle[derniere] = b'\n';

        let cas: [(&str, &[u8]); 9] = [
            ("datagramme vide", &[]),
            ("en-tête tronqué", &valide[..ENTETE_OCTETS - 1]),
            ("mauvaise magie", &magie),
            ("version inconnue", &version),
            ("longueur de nom démesurée", &longueur_demesuree),
            ("longueur incohérente", &longueur_incoherente),
            ("nom non UTF-8", &nom_non_utf8),
            ("caractère de contrôle dans le nom", &caractere_de_controle),
            ("bruit aléatoire", &[0xA5; 64]),
        ];
        for (etiquette, datagramme) in cas {
            assert_eq!(
                decoder_presence(datagramme),
                None,
                "cas « {etiquette} » accepté à tort"
            );
        }
        // Témoin : l'original, lui, se décode toujours.
        assert!(decoder_presence(&valide).is_some());
    }

    // --- Déduplication / expiration / exclusion ------------------------------

    #[test]
    fn table_deduplique_par_id_et_rafraichit() {
        let mut table = TablePairs::new(TTL_PAIR, None);
        let t0 = Instant::now();
        table.enregistrer(NovaId(1), "ancien".into(), adresse(1_000), t0);
        table.enregistrer(
            NovaId(1),
            "nouveau".into(),
            adresse(2_000),
            t0 + Duration::from_secs(1),
        );

        let pairs = table.instantane(t0 + Duration::from_secs(1));
        assert_eq!(pairs.len(), 1, "deux annonces du même id ⇒ une entrée");
        assert_eq!(pairs[0].nom, "nouveau");
        assert_eq!(pairs[0].adresse, adresse(2_000));
    }

    #[test]
    fn table_expire_les_pairs_non_revus() {
        let ttl = Duration::from_secs(10);
        let mut table = TablePairs::new(ttl, None);
        let t0 = Instant::now();
        table.enregistrer(NovaId(1), "éphémère".into(), adresse(1_000), t0);
        table.enregistrer(NovaId(2), "assidu".into(), adresse(2_000), t0);
        // Seul le pair 2 ré-annonce à t0+8 s.
        table.enregistrer(
            NovaId(2),
            "assidu".into(),
            adresse(2_000),
            t0 + Duration::from_secs(8),
        );

        // Juste avant le TTL, les deux vivent encore.
        assert_eq!(table.instantane(t0 + Duration::from_millis(9_999)).len(), 2);
        // Au TTL pile, le pair 1 (vu à t0) expire ; le pair 2, revu, reste.
        let restants = table.instantane(t0 + ttl);
        assert_eq!(restants.len(), 1);
        assert_eq!(restants[0].id, NovaId(2));
        // Bien plus tard : plus personne, et la table est réellement purgée
        // (les entrées sont retirées, pas seulement filtrées à l'affichage).
        assert!(table.instantane(t0 + Duration::from_secs(18)).is_empty());
        assert!(table.pairs.is_empty());
    }

    #[test]
    fn table_exclut_le_pair_local() {
        let mut table = TablePairs::new(TTL_PAIR, Some(NovaId(9)));
        let t0 = Instant::now();
        table.enregistrer(NovaId(9), "moi-même".into(), adresse(1_000), t0);
        table.enregistrer(NovaId(3), "voisin".into(), adresse(2_000), t0);

        let pairs = table.instantane(t0);
        assert_eq!(pairs.len(), 1, "sa propre annonce ne doit jamais entrer");
        assert_eq!(pairs[0].id, NovaId(3));
    }

    #[test]
    fn table_plafonnee_contre_l_inondation() {
        let mut table = TablePairs::new(TTL_PAIR, None);
        let t0 = Instant::now();
        for i in 0..(MAX_PAIRS as u64 + 100) {
            table.enregistrer(NovaId(i), format!("pair {i}"), adresse(1_000), t0);
        }
        assert_eq!(table.pairs.len(), MAX_PAIRS, "table bornée à MAX_PAIRS");
        // Un id déjà connu se rafraîchit toujours, même table pleine.
        table.enregistrer(
            NovaId(0),
            "rafraîchi".into(),
            adresse(2_000),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(table.pairs[&NovaId(0)].nom, "rafraîchi");
    }

    #[test]
    fn instantane_trie_par_id() {
        let mut table = TablePairs::new(TTL_PAIR, None);
        let t0 = Instant::now();
        for id in [5_u64, 1, 3] {
            table.enregistrer(NovaId(id), String::new(), adresse(1_000), t0);
        }
        let ids: Vec<u64> = table
            .instantane(t0)
            .iter()
            .map(|pair| pair.id.as_u64())
            .collect();
        assert_eq!(ids, [1, 3, 5]);
    }
}
