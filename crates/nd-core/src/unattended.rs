//! Service hôte « **accès non surveillé** » : un poste contrôlé qui publie son
//! ID au rendez-vous, attend les appelants en continu, consulte un **hook
//! d'acceptation** (le futur dialogue de l'UI ou la liste blanche de l'accès
//! non surveillé) puis sert une session hôte complète — capture → encodeur
//! matériel (repli logiciel) → ABR → QUIC chiffré Noise, entrées filtrées par
//! les permissions.
//!
//! ```text
//! UnattendedHost::start(...) ──► thread « nd-hote-non-surveille »
//!   register(ID, certificat) + heartbeat
//!   boucle : await_p2p ──► accept(pair) ?
//!     ├─ refusé  : socket percée abandonnée (aucun octet applicatif), on continue
//!     └─ accepté : QUIC sur la socket percée → Noise (répondeur) → session hôte
//!                  (une session à la fois ; retour à l'attente à la fin)
//! ```
//!
//! Une erreur de session (capture indisponible, pair disparu pendant le
//! handshake…) est consignée ([`UnattendedHostHandle::last_error`]) et le
//! service **retourne à l'attente** : un hôte non surveillé doit survivre à ses
//! sessions. L'arrêt propre passe par [`UnattendedHostHandle::stop`].
//!
//! L'identité TLS est **fournie** par l'appelant : persistée entre démarrages,
//! elle garde le certificat publié épinglable par les contrôleurs habituels
//! (voir `nd-transport::ServerIdentity::from_der_parts`).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nd_features::PermissionSet;
use nd_proto::{NovaId, Result};
use nd_signaling::RendezvousClient;
use nd_transport::ServerIdentity;

use crate::p2p::{self, AttenteRendezvous};
use crate::session::{vivre_epoque_hote, CompteursSession, ParamsEpoqueHote};
use crate::HostStreamOptions;

/// Délai maximal accordé au thread de service pour se terminer dans
/// [`UnattendedHostHandle::stop`].
const DELAI_ARRET: Duration = Duration::from_secs(5);

/// Fenêtre d'attente d'un appelant par itération : courte pour vérifier
/// fréquemment le signal d'arrêt (l'attente elle-même est illimitée).
const FENETRE_ATTENTE: Duration = Duration::from_secs(3);

/// Service hôte « accès non surveillé ». Façade sans état : tout le vivant
/// appartient au thread de service et à la [`UnattendedHostHandle`] rendue par
/// [`UnattendedHost::start`].
pub struct UnattendedHost;

impl UnattendedHost {
    /// Démarre le service : publie `local_id` au serveur de rendez-vous avec le
    /// certificat de `identity`, puis boucle en attente de connexions
    /// entrantes. Pour chaque appelant, `accept(pair)` est consulté **avant**
    /// tout octet applicatif (point d'ancrage du dialogue d'acceptation de
    /// l'UI) ; si accepté, une session hôte complète est servie avec les
    /// `permissions` données (entrées filtrées côté contrôlé), une session à
    /// la fois.
    ///
    /// `stun_servers` alimente les candidats de punch (vide = LAN/loopback).
    /// La traversée d'un vrai NAT dépend du type de NAT (voir `nd-signaling`) ;
    /// aucun repli relais n'est branché ici (le courtier de tickets signés est
    /// le lot 07).
    ///
    /// # Errors
    /// Erreur si le thread de service ne peut pas être créé. Les erreurs
    /// d'exécution (rendez-vous injoignable, session avortée…) sont consignées
    /// dans [`UnattendedHostHandle::last_error`] sans arrêter le service —
    /// sauf l'échec d'enregistrement initial, qui l'arrête (ID jamais publié).
    pub fn start(
        local_id: NovaId,
        rendezvous: SocketAddr,
        stun_servers: Vec<SocketAddr>,
        identity: ServerIdentity,
        permissions: PermissionSet,
        accept: impl Fn(NovaId) -> bool + Send + 'static,
    ) -> Result<UnattendedHostHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let compteurs = Arc::new(CompteursSession::default());
        let sessions_servies = Arc::new(AtomicU64::new(0));
        let pairs_refuses = Arc::new(AtomicU64::new(0));
        let derniere_erreur = Arc::new(Mutex::new(None));

        let service = Service {
            local_id,
            rendezvous,
            stun_servers,
            identity,
            permissions,
            stop: Arc::clone(&stop),
            compteurs: Arc::clone(&compteurs),
            sessions_servies: Arc::clone(&sessions_servies),
            pairs_refuses: Arc::clone(&pairs_refuses),
            derniere_erreur: Arc::clone(&derniere_erreur),
        };
        let thread = thread::Builder::new()
            .name("nd-hote-non-surveille".to_owned())
            .spawn(move || service.boucle(&accept))?;

        Ok(UnattendedHostHandle {
            stop,
            thread: Some(thread),
            compteurs,
            sessions_servies,
            pairs_refuses,
            derniere_erreur,
        })
    }
}

/// Poignée du service hôte non surveillé : observabilité et arrêt propre.
pub struct UnattendedHostHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    compteurs: Arc<CompteursSession>,
    sessions_servies: Arc<AtomicU64>,
    pairs_refuses: Arc<AtomicU64>,
    derniere_erreur: Arc<Mutex<Option<String>>>,
}

impl UnattendedHostHandle {
    /// Nombre de sessions hôtes **acceptées et servies** (démarrées) depuis le
    /// lancement du service.
    #[must_use]
    pub fn sessions_served(&self) -> u64 {
        self.sessions_servies.load(Ordering::Relaxed)
    }

    /// Nombre d'appelants **refusés** par le hook d'acceptation.
    #[must_use]
    pub fn peers_refused(&self) -> u64 {
        self.pairs_refuses.load(Ordering::Relaxed)
    }

    /// Instantané des statistiques de session (cumulées sur les sessions
    /// servies) : entrées appliquées/refusées, débit ABR, octets…
    #[must_use]
    pub fn stats(&self) -> crate::SessionStats {
        self.compteurs.instantane()
    }

    /// Dernière erreur d'exécution consignée par le service (`None` si tout va
    /// bien). Le service survit aux erreurs de session ; seule l'impossibilité
    /// de publier l'ID au démarrage l'arrête.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.derniere_erreur
            .lock()
            .expect("verrou de la dernière erreur")
            .clone()
    }

    /// Le thread de service tourne-t-il encore ?
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Arrête le service : lève le signal d'arrêt puis attend la fin du thread
    /// (au plus ~5 s — un punch en cours peut retarder la sortie ; au-delà, le
    /// thread est détaché et se terminera de lui-même).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let echeance = Instant::now() + DELAI_ARRET;
            while !thread.is_finished() && Instant::now() < echeance {
                thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for UnattendedHostHandle {
    /// Lâcher la poignée demande l'arrêt du service **sans** attendre sa fin
    /// (voir [`UnattendedHostHandle::stop`] pour un arrêt bloquant).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// État interne du thread de service.
struct Service {
    local_id: NovaId,
    rendezvous: SocketAddr,
    stun_servers: Vec<SocketAddr>,
    identity: ServerIdentity,
    permissions: PermissionSet,
    stop: Arc<AtomicBool>,
    compteurs: Arc<CompteursSession>,
    sessions_servies: Arc<AtomicU64>,
    pairs_refuses: Arc<AtomicU64>,
    derniere_erreur: Arc<Mutex<Option<String>>>,
}

impl Service {
    /// Boucle de service : attendre un appelant admis, servir sa session,
    /// recommencer — jusqu'au signal d'arrêt.
    fn boucle(&self, accept: &(impl Fn(NovaId) -> bool + Send + 'static)) {
        let rv = RendezvousClient::new(self.rendezvous);
        // Admission : hook de l'appelant + compteur de refus.
        let refus = Arc::clone(&self.pairs_refuses);
        let admission = move |pair: NovaId| {
            let admis = accept(pair);
            if !admis {
                refus.fetch_add(1, Ordering::Relaxed);
            }
            admis
        };

        while !self.stop.load(Ordering::Relaxed) {
            let attente = AttenteRendezvous {
                rv: &rv,
                local_id: self.local_id,
                identite: &self.identity,
                stun_servers: &self.stun_servers,
                relay: None,
                admission: &admission,
            };
            // Attente bornée par fenêtre : la boucle revient vérifier `stop`
            // (et rafraîchir le heartbeat) entre deux fenêtres.
            let entrant =
                match p2p::accepter_par_rendezvous(&attente, Some(FENETRE_ATTENTE), &self.stop) {
                    Ok(entrant) => entrant,
                    Err(erreur) => {
                        // Fenêtre vide = attente normale ; les vraies erreurs
                        // (rendez-vous injoignable…) sont consignées et l'attente
                        // reprend — un hôte non surveillé persévère.
                        if !erreur.to_string().contains("aucune connexion entrante") {
                            self.note_erreur(&erreur.to_string());
                        }
                        continue;
                    }
                };
            let (transport, pair) = entrant;
            self.sessions_servies.fetch_add(1, Ordering::Relaxed);

            let params = ParamsEpoqueHote {
                permissions: self.permissions,
                flux: HostStreamOptions::default(),
                compteurs: &self.compteurs,
                stop: &self.stop,
                etats: None,
                pair,
            };
            if let Err(erreur) = vivre_epoque_hote(transport, &params) {
                self.note_erreur(&format!("session avec {pair} : {erreur}"));
            }
        }
    }

    /// Consigne une erreur d'exécution (la plus récente est conservée).
    fn note_erreur(&self, erreur: &str) {
        *self
            .derniere_erreur
            .lock()
            .expect("verrou de la dernière erreur") = Some(erreur.to_owned());
    }
}

// ---------------------------------------------------------------------------
// Tests (boucle locale ; la session hôte complète — capture réelle incluse —
// est prouvée par examples/session_integree_demo.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nd_signaling::{establish_p2p, serve, ConnAttempt, Registry};
    use std::net::TcpListener;

    /// Démarre un serveur de rendez-vous éphémère et rend son adresse.
    fn rendezvous_ephemere() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind rendez-vous");
        let addr = listener.local_addr().expect("adresse rendez-vous");
        thread::spawn(move || {
            let _ = serve(listener, Registry::new());
        });
        addr
    }

    #[test]
    fn demarre_publie_et_s_arrete_proprement() {
        let rv_addr = rendezvous_ephemere();
        let id = NovaId(505_050_505);
        let identite = ServerIdentity::generate().expect("identité");
        let poignee = UnattendedHost::start(
            id,
            rv_addr,
            vec![],
            identite,
            PermissionSet::view_only(),
            |_pair| true,
        )
        .expect("start");

        // L'ID est publié (résoluble) dès que le service a enregistré.
        let rv = RendezvousClient::new(rv_addr);
        let echeance = Instant::now() + Duration::from_secs(5);
        let mut fiche = None;
        while fiche.is_none() && Instant::now() < echeance {
            fiche = rv.lookup(id).ok();
            thread::sleep(Duration::from_millis(20));
        }
        let fiche = fiche.expect("ID publié au rendez-vous");
        assert!(!fiche.cert_der.is_empty(), "certificat publié");

        assert!(poignee.is_running());
        assert_eq!(poignee.sessions_served(), 0);
        poignee.stop();
    }

    /// Le hook d'acceptation est consulté **avant** d'accepter QUIC : un
    /// appelant refusé perce (le punch réussit) mais aucune session ne démarre.
    #[test]
    fn appelant_refuse_par_le_hook_sans_session() {
        let rv_addr = rendezvous_ephemere();
        let hote_id = NovaId(606_060_606);
        let admis_id = NovaId(42);
        let intrus_id = NovaId(666);

        let vus = Arc::new(Mutex::new(Vec::new()));
        let vus_hook = Arc::clone(&vus);
        let identite = ServerIdentity::generate().expect("identité");
        let poignee = UnattendedHost::start(
            hote_id,
            rv_addr,
            vec![],
            identite,
            PermissionSet::view_only(),
            move |pair| {
                vus_hook.lock().expect("verrou des pairs vus").push(pair);
                pair == admis_id
            },
        )
        .expect("start");

        // L'intrus tente sa chance : le punch aboutit (loopback), mais le hook
        // refuse — l'hôte abandonne la socket sans accepter QUIC.
        let rv = RendezvousClient::new(rv_addr);
        let echeance = Instant::now() + Duration::from_secs(15);
        let mut perce = false;
        while !perce && Instant::now() < echeance {
            match establish_p2p(&rv, intrus_id, hote_id, &[]) {
                Ok(ConnAttempt::Direct(_chemin)) => perce = true,
                // L'hôte n'est pas encore en attente (candidats non publiés).
                Ok(ConnAttempt::RelayFallback { .. }) | Err(_) => {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        assert!(perce, "le punch loopback doit aboutir");

        // Le hook a bien été consulté avec l'ID de l'intrus, et l'a refusé.
        let echeance = Instant::now() + Duration::from_secs(10);
        while poignee.peers_refused() == 0 && Instant::now() < echeance {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(poignee.peers_refused() >= 1, "refus compté");
        assert!(
            vus.lock()
                .expect("verrou des pairs vus")
                .contains(&intrus_id),
            "le hook a vu l'intrus"
        );
        assert_eq!(poignee.sessions_served(), 0, "aucune session servie");
        assert!(poignee.is_running(), "le service continue d'attendre");
        poignee.stop();
    }
}
