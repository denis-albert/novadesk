//! Reconnexion **transparente** : [`ReconnectingTransport`] enveloppe un
//! [`Transport`] et rétablit automatiquement la connexion sous-jacente après une
//! coupure, sans que l'appelant ait à intervenir.
//!
//! # Principe
//!
//! L'enveloppe scrute [`Transport::is_connected`] au fil des appels `send` /
//! `poll_recv` (voir plan 04 : la boucle de session s'accroche à
//! `is_connected` / `on_disconnect`). Dès qu'une coupure est constatée, elle
//! **fabrique une nouvelle connexion** via une closure fournie
//! (`Fn() -> Result<Box<dyn Transport>>`), **ré-ouvre les canaux logiques**
//! précédemment ouverts (dans le même ordre → mêmes [`ChannelHandle`]), puis
//! reprend l'appel. Un **backoff exponentiel** ([`Backoff`]) espace les
//! tentatives infructueuses. L'état est observable ([`EtatReconnexion`]).
//!
//! La closure encapsule *comment* on se reconnecte : re-`connect` direct,
//! re-`accept`, re-négociation par le rendez-vous (`nd-core`), repli relais…
//! L'enveloppe ne connaît que « produis-moi un nouveau transport connecté ».
//!
//! # Transparence et détection
//!
//! La détection repose sur `is_connected` du transport enveloppé. Le
//! [`crate::QuicTransport`] le renseigne fidèlement (fermeture par le pair,
//! erreur transport, délai d'inactivité). Un intermédiaire qui enveloppe lui
//! aussi un `Transport` (chiffrement, comptage…) **doit relayer** `is_connected`
//! de son transport interne, sinon la coupure reste invisible à travers lui.
//!
//! La détection est **paresseuse** : une enveloppe inactive (aucun `send` ni
//! `poll_recv`) ne se reconnecte pas tant qu'aucun appel ne la sollicite.
//! Pendant une panne, `send` / `poll_recv` **bloquent** sur le backoff jusqu'au
//! rétablissement (ou renvoient une erreur si [`Backoff::max_tentatives`] est
//! atteint) : la reconnexion est synchrone, sur le thread appelant.
//!
//! # Limites (pertes en vol)
//!
//! La reconnexion établit une **nouvelle** connexion QUIC (nouvelle session
//! TLS) : ce n'est pas une reprise de session au sens QUIC. Par conséquent :
//!
//! * les datagrammes **non fiables** (vidéo/audio) en vol au moment de la
//!   coupure sont **perdus** — c'est acceptable pour le média, qui se resynchro-
//!   nise à l'image-clé suivante ;
//! * une charge **fiable** en cours d'émission (mise en file mais pas encore
//!   partie sur l'ancienne connexion) ou déjà reçue mais pas encore drainée de
//!   l'ancien transport est **perdue** : la reprise ne rejoue pas les octets. Le
//!   protocole applicatif au-dessus (accusés, resynchronisation) doit tolérer ce
//!   trou, comme après n'importe quelle coupure réseau ;
//! * la continuité suppose que le **pair** ré-accepte (la closure côté appelé
//!   ré-`accept`) : l'enveloppe ne recrée qu'un seul côté du lien.
//!
//! Les canaux logiques, eux, sont **rétablis** : les [`ChannelHandle`] rendus
//! avant la coupure restent valides après reconnexion.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nd_proto::{ChannelKind, NdError, Reliability, Result};

use crate::{ChannelHandle, PathEstimate, Transport};

/// Fabrique d'un nouveau transport connecté (alias pour éviter la complexité de
/// type dans les signatures/champs — voir `clippy::type_complexity`).
type FabriqueTransport = Box<dyn Fn() -> Result<Box<dyn Transport>> + Send>;

/// Politique de backoff exponentiel entre deux tentatives de reconnexion.
///
/// Le délai part de `delai_initial` et double à chaque échec, plafonné à
/// `delai_max`. Une reconnexion **réussie du premier coup n'attend pas**.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// Délai avant la deuxième tentative (après le premier échec).
    pub delai_initial: Duration,
    /// Plafond du délai entre tentatives.
    pub delai_max: Duration,
    /// Nombre maximal de tentatives avant d'abandonner (`None` = illimité, la
    /// reconnexion réessaie indéfiniment).
    pub max_tentatives: Option<u32>,
}

impl Default for Backoff {
    /// Défaut raisonnable : 100 ms → 5 s, réessais illimités (reconnexion
    /// réputée transparente : on ne renonce pas).
    fn default() -> Self {
        Self {
            delai_initial: Duration::from_millis(100),
            delai_max: Duration::from_secs(5),
            max_tentatives: None,
        }
    }
}

/// État observable d'un [`ReconnectingTransport`], partageable entre threads
/// (par ex. pour un indicateur d'interface « reconnexion en cours… »).
#[derive(Debug, Default)]
pub struct EtatReconnexion {
    reconnexions: AtomicU64,
    tentatives_echouees: AtomicU64,
    en_cours: AtomicBool,
}

impl EtatReconnexion {
    /// Nombre de reconnexions **réussies** depuis la création.
    #[must_use]
    pub fn reconnexions(&self) -> u64 {
        self.reconnexions.load(Ordering::Relaxed)
    }

    /// Nombre de tentatives de reconnexion **infructueuses** (cumulé).
    #[must_use]
    pub fn tentatives_echouees(&self) -> u64 {
        self.tentatives_echouees.load(Ordering::Relaxed)
    }

    /// Une reconnexion est-elle **en cours** en ce moment ?
    #[must_use]
    pub fn en_cours(&self) -> bool {
        self.en_cours.load(Ordering::Relaxed)
    }
}

/// Transport à **reconnexion transparente** : enveloppe un [`Transport`] et
/// rétablit la connexion sous-jacente après coupure, en ré-ouvrant les canaux.
///
/// Implémente lui-même [`Transport`] : il se substitue à l'original sans changer
/// le code appelant. Voir la documentation du module pour la détection, le
/// backoff et les limites (pertes en vol).
pub struct ReconnectingTransport {
    inner: Box<dyn Transport>,
    fabrique: FabriqueTransport,
    backoff: Backoff,
    /// Canaux ouverts par l'appelant, dans l'ordre (dédupliqués) : rejoués à
    /// l'identique après reconnexion pour préserver les `ChannelHandle`.
    canaux: Vec<ChannelKind>,
    etat: Arc<EtatReconnexion>,
}

impl ReconnectingTransport {
    /// Enveloppe `initial` avec le backoff par défaut ([`Backoff::default`]).
    ///
    /// `reconnexion` fabrique une **nouvelle** connexion connectée quand la
    /// courante tombe (re-`connect`, re-`accept`, re-négociation…).
    pub fn new<F>(initial: Box<dyn Transport>, reconnexion: F) -> Self
    where
        F: Fn() -> Result<Box<dyn Transport>> + Send + 'static,
    {
        Self::avec_backoff(initial, reconnexion, Backoff::default())
    }

    /// Comme [`ReconnectingTransport::new`], avec une politique de [`Backoff`]
    /// explicite.
    pub fn avec_backoff<F>(initial: Box<dyn Transport>, reconnexion: F, backoff: Backoff) -> Self
    where
        F: Fn() -> Result<Box<dyn Transport>> + Send + 'static,
    {
        Self {
            inner: initial,
            fabrique: Box::new(reconnexion),
            backoff,
            canaux: Vec::new(),
            etat: Arc::new(EtatReconnexion::default()),
        }
    }

    /// Nombre de reconnexions réussies depuis la création.
    #[must_use]
    pub fn reconnexions(&self) -> u64 {
        self.etat.reconnexions()
    }

    /// Une reconnexion est-elle en cours ?
    #[must_use]
    pub fn reconnexion_en_cours(&self) -> bool {
        self.etat.en_cours()
    }

    /// Poignée partagée sur l'[`EtatReconnexion`], pour l'observer depuis un
    /// autre thread (indicateur d'interface, supervision).
    #[must_use]
    pub fn etat(&self) -> Arc<EtatReconnexion> {
        Arc::clone(&self.etat)
    }

    /// Garantit une connexion vivante avant un `send`/`poll_recv` : reconnecte
    /// si le transport enveloppé est coupé.
    fn assurer_connexion(&mut self) -> Result<()> {
        if self.inner.is_connected() {
            return Ok(());
        }
        self.reconnecter()
    }

    /// Boucle de reconnexion avec backoff exponentiel. Remplace le transport
    /// interne et ré-ouvre les canaux dès qu'une fabrication réussit.
    fn reconnecter(&mut self) -> Result<()> {
        self.etat.en_cours.store(true, Ordering::Relaxed);
        let mut delai = self.backoff.delai_initial;
        let mut tentative: u32 = 0;
        let resultat = loop {
            tentative += 1;
            match (self.fabrique)() {
                Ok(nouveau) => {
                    self.inner = nouveau;
                    self.rouvrir_canaux();
                    self.etat.reconnexions.fetch_add(1, Ordering::Relaxed);
                    break Ok(());
                }
                Err(cause) => {
                    self.etat
                        .tentatives_echouees
                        .fetch_add(1, Ordering::Relaxed);
                    if matches!(self.backoff.max_tentatives, Some(max) if tentative >= max) {
                        break Err(NdError::Transport(format!(
                            "reconnexion abandonnée après {tentative} tentative(s) : {cause}"
                        )));
                    }
                    std::thread::sleep(delai);
                    // Double le délai, plafonné, sans jamais déborder `Duration`.
                    delai = delai
                        .checked_mul(2)
                        .unwrap_or(self.backoff.delai_max)
                        .min(self.backoff.delai_max);
                }
            }
        };
        self.etat.en_cours.store(false, Ordering::Relaxed);
        resultat
    }

    /// Ré-ouvre sur le nouveau transport les canaux mémorisés, dans l'ordre :
    /// comme l'indexation des canaux est par ordre d'ouverture, les
    /// [`ChannelHandle`] rendus avant la coupure restent valides.
    fn rouvrir_canaux(&mut self) {
        for i in 0..self.canaux.len() {
            let kind = self.canaux[i];
            self.inner.open_channel(kind);
        }
    }
}

impl Transport for ReconnectingTransport {
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
        let handle = self.inner.open_channel(kind);
        // Mémorise pour ré-ouverture après reconnexion (déduplication par type,
        // comme le transport QUIC sous-jacent : même indexation → même handle).
        if !self.canaux.contains(&kind) {
            self.canaux.push(kind);
        }
        handle
    }

    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, reliability: Reliability) -> Result<()> {
        self.assurer_connexion()?;
        self.inner.send(ch, data, reliability)
    }

    fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
        self.assurer_connexion()?;
        self.inner.poll_recv()
    }

    fn path_estimate(&self) -> PathEstimate {
        self.inner.path_estimate()
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bind, connect};
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Instant;

    /// Transport factice contrôlable, pour tester la logique de reconnexion sans
    /// réseau réel : état de connexion piloté, canaux ouverts enregistrés.
    struct FauxTransport {
        connecte: bool,
        canaux_ouverts: Arc<Mutex<Vec<ChannelKind>>>,
    }

    impl FauxTransport {
        fn coupe() -> Self {
            Self {
                connecte: false,
                canaux_ouverts: Arc::default(),
            }
        }
    }

    impl Transport for FauxTransport {
        fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
            let mut ouverts = self.canaux_ouverts.lock().expect("verrou canaux");
            if let Some(i) = ouverts.iter().position(|k| *k == kind) {
                return ChannelHandle(i as u32);
            }
            ouverts.push(kind);
            ChannelHandle((ouverts.len() - 1) as u32)
        }
        fn send(&mut self, _ch: ChannelHandle, _data: Vec<u8>, _r: Reliability) -> Result<()> {
            Ok(())
        }
        fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
            Ok(None)
        }
        fn path_estimate(&self) -> PathEstimate {
            PathEstimate::default()
        }
        fn is_connected(&self) -> bool {
            self.connecte
        }
    }

    /// Draine `poll_recv` jusqu'au prochain message ou à l'expiration.
    fn attendre(t: &mut Box<dyn Transport>, timeout: Duration) -> Option<Vec<u8>> {
        let debut = Instant::now();
        while debut.elapsed() < timeout {
            if let Some((_, d)) = t.poll_recv().expect("poll_recv") {
                return Some(d);
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    /// Attend que l'enveloppe voie la coupure (`is_connected` faux).
    fn attendre_coupure(t: &ReconnectingTransport, timeout: Duration) -> bool {
        let debut = Instant::now();
        while debut.elapsed() < timeout {
            if !t.is_connected() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Reconnexion transparente en bouclage QUIC : après coupure simulée (chute
    /// du serveur), un simple `send` rétablit la connexion, rouvre le canal et
    /// délivre le message au nouveau serveur — le handle d'avant reste valide.
    #[test]
    fn reconnexion_transparente_en_loopback_apres_coupure() {
        let listener = bind("127.0.0.1:0".parse().expect("adresse")).expect("bind");
        let addr = listener.local_addr();
        let cert = listener.server_cert_der();

        // Boucle d'acceptation : l'endpoint survit aux connexions individuelles.
        let (tx_srv, rx_srv) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(transport) = listener.accept() {
                if tx_srv.send(transport).is_err() {
                    break;
                }
            }
        });

        // Client enveloppé : la fabrique re-connecte au même écouteur (même cert).
        let cert_fabrique = cert.clone();
        let mut client = ReconnectingTransport::new(
            connect(addr, &cert).expect("connexion initiale"),
            move || connect(addr, &cert_fabrique),
        );

        let mut serveur1 = rx_srv
            .recv_timeout(Duration::from_secs(5))
            .expect("1re connexion acceptée");

        // Échange initial.
        let h = client.open_channel(ChannelKind::Control);
        client
            .send(h, b"avant".to_vec(), Reliability::Reliable)
            .expect("send avant");
        assert_eq!(
            attendre(&mut serveur1, Duration::from_secs(5)).expect("message avant"),
            b"avant"
        );
        assert_eq!(client.reconnexions(), 0);

        // Coupure simulée : la chute du serveur ferme la connexion du client.
        drop(serveur1);
        assert!(
            attendre_coupure(&client, Duration::from_secs(15)),
            "la coupure doit être détectée"
        );

        // Un envoi déclenche la reconnexion transparente : même handle `h`.
        client
            .send(h, b"apres".to_vec(), Reliability::Reliable)
            .expect("send apres (reconnecte)");
        assert!(client.reconnexions() >= 1, "au moins une reconnexion");
        assert!(!client.reconnexion_en_cours());
        assert!(client.is_connected());

        // Le nouveau serveur reçoit le message ré-émis sur le canal rétabli.
        let mut serveur2 = rx_srv
            .recv_timeout(Duration::from_secs(5))
            .expect("2e connexion acceptée");
        assert_eq!(
            attendre(&mut serveur2, Duration::from_secs(5)).expect("message apres"),
            b"apres"
        );
    }

    /// Sans réseau : quand la fabrique échoue toujours, la reconnexion abandonne
    /// après `max_tentatives` et remonte l'erreur (le backoff est court).
    #[test]
    fn reconnexion_abandonne_apres_max_tentatives() {
        let backoff = Backoff {
            delai_initial: Duration::from_millis(1),
            delai_max: Duration::from_millis(1),
            max_tentatives: Some(3),
        };
        let mut rt = ReconnectingTransport::avec_backoff(
            Box::new(FauxTransport::coupe()),
            || Err(NdError::Transport("indisponible".into())),
            backoff,
        );

        // `send` déclenche la reconnexion (transport coupé) → échec après 3 essais.
        let erreur = rt
            .send(ChannelHandle(0), vec![1, 2, 3], Reliability::Reliable)
            .expect_err("doit abandonner");
        assert!(matches!(erreur, NdError::Transport(_)));
        assert_eq!(rt.reconnexions(), 0);
        assert_eq!(rt.etat().tentatives_echouees(), 3);
        assert!(!rt.reconnexion_en_cours());
    }

    /// Sans réseau : une fabrique qui réussit rouvre bien les canaux mémorisés
    /// sur le nouveau transport et incrémente le compteur.
    #[test]
    fn reconnexion_rouvre_les_canaux() {
        // Canaux observés sur le transport fabriqué à la reconnexion.
        let observes: Arc<Mutex<Vec<ChannelKind>>> = Arc::default();
        let observes_fab = Arc::clone(&observes);
        let mut rt = ReconnectingTransport::new(Box::new(FauxTransport::coupe()), move || {
            Ok(Box::new(FauxTransport {
                connecte: true,
                canaux_ouverts: Arc::clone(&observes_fab),
            }) as Box<dyn Transport>)
        });

        // L'appelant ouvre deux canaux sur le transport (coupé) initial.
        let h0 = rt.open_channel(ChannelKind::Control);
        let h1 = rt.open_channel(ChannelKind::Input);
        assert_eq!((h0, h1), (ChannelHandle(0), ChannelHandle(1)));

        // Un poll déclenche la reconnexion (transport initial coupé).
        rt.poll_recv().expect("poll_recv");
        assert_eq!(rt.reconnexions(), 1);
        assert!(rt.is_connected());

        // Les mêmes canaux ont été rejoués, dans l'ordre, sur le nouveau transport.
        assert_eq!(
            *observes.lock().expect("verrou"),
            vec![ChannelKind::Control, ChannelKind::Input]
        );
    }
}
