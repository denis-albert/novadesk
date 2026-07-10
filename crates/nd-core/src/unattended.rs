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
//! # Contrôle d'admission automatique ([`UnattendedHost::start_with_admission`])
//!
//! Le démarrage historique ci-dessus soumet **chaque** appelant au crochet
//! `accept` (le dialogue de l'UI) : l'accès n'est pas réellement « non
//! surveillé ». La variante additive [`UnattendedHost::start_with_admission`]
//! déplace la décision **dans le canal chiffré Noise** :
//!
//! ```text
//!   boucle : await_p2p ──► QUIC → Noise (répondeur) → admission :
//!     1. appareil de confiance (ID du punch)            → admis
//!     2. DemandeAdmission avec mot de passe validé      → admis
//!        (mot de passe invalide                         → refusé, sans dialogue)
//!     3. DemandeAdmission avec invitation valide        → admis (profil de
//!        l'invitation, code consommé ; invalide         → refusé, sans dialogue)
//!     4. aucune preuve                                  → crochet accept (repli
//!        manuel enrichi) ; sans décision → refus à l'expiration côté appelant
//! ```
//!
//! Les étapes 3 (invitations) et l'enrichissement du dialogue manuel (nom
//! d'affichage + profil demandé) sont portés par la variante additive
//! [`UnattendedHost::start_with_admission_enrichie`] ; le démarrage
//! [`UnattendedHost::start_with_admission`] s'arrête aux étapes 1, 2, 4 (aucune
//! invitation, dialogue nu).
//!
//! Rien ne révèle au pair la raison d'une admission ou d'un refus, et le clair
//! du mot de passe (comme le code d'invitation) ne circule que dans le canal
//! Noise (voir `session::ControleAdmission`).
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
use nd_transport::{QuicTransport, ServerIdentity};

use crate::p2p::{self, AttenteRendezvous, IdentiteReseau};
use crate::session::{
    vivre_epoque_hote, vivre_epoque_hote_avec_admission, CompteursSession, ControleAdmission,
    DemandeAdmissionManuelle, ParamsEpoqueHote,
};
use crate::{FabriqueCapteur, FabriqueInjecteur, HostStreamOptions};

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
        let service = Service::nouveau(
            local_id,
            rendezvous,
            stun_servers,
            identity,
            permissions,
            None,
            None,
            None,
        );
        service.lancer(move |service| service.boucle(&accept))
    }

    /// Démarre le service avec **contrôle d'admission automatique** : chaque
    /// appelant est jaugé **dans le canal chiffré Noise** (une fois la session
    /// QUIC + Noise établie), dans l'ordre :
    ///
    /// 1. `est_de_confiance(pair)` (appareil de confiance) → **admis** ;
    /// 2. sinon, mot de passe reçu du contrôleur (message `DemandeAdmission`,
    ///    émis juste après l'établissement — voir
    ///    [`SessionOptions::mot_de_passe`](crate::SessionOptions::mot_de_passe))
    ///    et validé par `verif_mdp` → **admis** ; un mot de passe **invalide
    ///    refuse immédiatement**, sans solliciter l'UI (pas d'usure de
    ///    l'utilisateur par essais successifs) ;
    /// 3. sinon (aucune preuve reçue dans la fenêtre) → **repli sur le crochet
    ///    `accept`** — le dialogue manuel existant ; sans décision, celui-ci
    ///    expire en refus côté appelant.
    ///
    /// La décision reste muette (rien ne révèle au pair si l'admission vient de
    /// la confiance ou du mot de passe) et un refus abandonne simplement la
    /// connexion, sans qu'aucun média ne circule. Le clair du mot de passe ne
    /// circule que dans le canal Noise : `verif_mdp` le compare côté appelant
    /// (typiquement à un hachage salé) — il n'est ni stocké ni journalisé ici.
    ///
    /// [`UnattendedHost::start`] reste le démarrage historique : tout appelant
    /// passe alors par `accept`, avant même l'acceptation QUIC.
    ///
    /// # Errors
    /// Mêmes conditions que [`UnattendedHost::start`].
    // Signature volontairement additive : les deux closures d'admission
    // s'ajoutent aux paramètres du démarrage historique sans le changer.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_admission(
        local_id: NovaId,
        rendezvous: SocketAddr,
        stun_servers: Vec<SocketAddr>,
        identity: ServerIdentity,
        permissions: PermissionSet,
        accept: impl Fn(NovaId) -> bool + Send + 'static,
        verif_mdp: impl Fn(&str) -> bool + Send + 'static,
        est_de_confiance: impl Fn(NovaId) -> bool + Send + 'static,
    ) -> Result<UnattendedHostHandle> {
        let service = Service::nouveau(
            local_id,
            rendezvous,
            stun_servers,
            identity,
            permissions,
            None,
            None,
            None,
        );
        service.lancer(move |service| {
            // Adapte le crochet historique (ID seul) au crochet enrichi ; ce mode
            // n'honore aucune invitation (validateur toujours `None`).
            let crochet = move |demande: &DemandeAdmissionManuelle| accept(demande.pair);
            let sans_invitation = |_pair: NovaId, _code: &str| -> Option<PermissionSet> { None };
            service.boucle_admission(&crochet, &verif_mdp, &est_de_confiance, &sans_invitation);
        })
    }

    /// Démarre le service avec **admission automatique enrichie** : superset
    /// additif de [`UnattendedHost::start_with_admission`] qui honore en plus les
    /// **invitations éphémères** et remonte au crochet manuel une **demande
    /// enrichie** (nom d'affichage + profil demandé). L'ordre de décision, **dans
    /// le canal chiffré Noise**, est :
    ///
    /// 1. `est_de_confiance(pair)` (appareil de confiance) → **admis** ;
    /// 2. sinon, mot de passe reçu et validé par `verif_mdp` → **admis** ; un mot
    ///    de passe **invalide refuse immédiatement**, sans solliciter l'UI ;
    /// 3. sinon, **code d'invitation** présenté et validé par `verif_invitation`
    ///    (non expiré, non déjà consommé) → **admis avec le profil de
    ///    l'invitation**, et le code est **consommé** (usage unique) ; une
    ///    invitation invalide **refuse**, sans dialogue ;
    /// 4. sinon (aucune preuve) → **repli sur le crochet `accept`** — le dialogue
    ///    manuel, qui reçoit une [`DemandeAdmissionManuelle`] (ID + nom
    ///    d'affichage + profil demandé s'ils ont été déclarés) pour un affichage
    ///    riche ; sans décision, il expire en refus côté appelant.
    ///
    /// `verif_invitation(pair, code)` rend `Some(profil)` si le code est valide —
    /// et le **consomme** alors — ou `None` sinon : l'appelant y branche son
    /// magasin ([`nd_features::invite::InviteStore`]) et la table code → profil.
    /// Le clair (mot de passe, code) ne circule que dans le canal Noise ; rien
    /// n'est honoré avant l'admission.
    ///
    /// # Enregistrement authentifié (« Internet par ID »)
    ///
    /// `identite_reseau` est le passage **additif** du jeton + clé de possession :
    /// `Some(..)` → l'hôte publie son ID au rendez-vous par un `register`
    /// **authentifié** (`register_authentifie`), exigé par le rendez-vous de
    /// production ; `None` → enregistrement **nu** (comportement historique,
    /// registre de développement / LAN). Voir [`IdentiteReseau`].
    ///
    /// # Errors
    /// Mêmes conditions que [`UnattendedHost::start`].
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_admission_enrichie(
        local_id: NovaId,
        rendezvous: SocketAddr,
        stun_servers: Vec<SocketAddr>,
        identity: ServerIdentity,
        permissions: PermissionSet,
        accept: impl Fn(&DemandeAdmissionManuelle) -> bool + Send + 'static,
        verif_mdp: impl Fn(&str) -> bool + Send + 'static,
        est_de_confiance: impl Fn(NovaId) -> bool + Send + 'static,
        verif_invitation: impl Fn(NovaId, &str) -> Option<PermissionSet> + Send + 'static,
        identite_reseau: Option<IdentiteReseau>,
    ) -> Result<UnattendedHostHandle> {
        // Superset strict : capteur/injecteur **système** (aucune fabrique
        // injectée). Le comportement historique est ainsi préservé mot pour mot,
        // et les appelants existants (nd-ffi, tests, exemples) restent inchangés.
        Self::start_with_admission_enrichie_fabriques(
            local_id,
            rendezvous,
            stun_servers,
            identity,
            permissions,
            accept,
            verif_mdp,
            est_de_confiance,
            verif_invitation,
            identite_reseau,
            None,
            None,
        )
    }

    /// Démarre le service avec admission enrichie **et fabriques de capteur /
    /// injecteur injectées** : superset **additif** de
    /// [`UnattendedHost::start_with_admission_enrichie`] (mêmes règles
    /// d'admission, à l'identique) dont les deux derniers paramètres branchent le
    /// **point d'injection** de la boucle hôte.
    ///
    /// À **chaque époque servie**, la boucle hôte appelle `capturer_factory`
    /// (resp. `injector_factory`) — si `Some(..)` — pour obtenir le
    /// [`ScreenCapturer`](nd_capture::ScreenCapturer) (resp.
    /// [`InputInjector`](nd_input::InputInjector)) de l'époque, **au lieu** de
    /// [`nd_capture::create_capturer`] (resp. [`nd_input::create_injector`]). Un
    /// paramètre à `None` conserve la brique système par défaut. Une **instance
    /// neuve par époque** est requise : une capture / injection n'est pas
    /// rejouable d'une connexion à l'autre.
    ///
    /// C'est le raccord de l'accès non surveillé **en service** : `nd-service` y
    /// fournit un `CapteurAssistant` / `InjecteurAssistant` adossés à un assistant
    /// lancé dans la **session active**, de sorte que le service capture le
    /// **vrai bureau** de l'utilisateur (là où un capteur créé en session 0 ne
    /// verrait qu'un bureau vide) et injecte dans **sa** session. Si une fabrique
    /// échoue (assistant indisponible — pas de session active, privilège
    /// manquant), l'erreur remonte comme n'importe quel échec de capture : la
    /// boucle hôte **avorte proprement l'époque**, l'erreur est consignée
    /// ([`UnattendedHostHandle::last_error`]) et le service **retourne à
    /// l'attente** — un appelant suivant relance une tentative (donc un nouvel
    /// assistant) quand une session redevient servable.
    ///
    /// # Errors
    /// Mêmes conditions que [`UnattendedHost::start`].
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_admission_enrichie_fabriques(
        local_id: NovaId,
        rendezvous: SocketAddr,
        stun_servers: Vec<SocketAddr>,
        identity: ServerIdentity,
        permissions: PermissionSet,
        accept: impl Fn(&DemandeAdmissionManuelle) -> bool + Send + 'static,
        verif_mdp: impl Fn(&str) -> bool + Send + 'static,
        est_de_confiance: impl Fn(NovaId) -> bool + Send + 'static,
        verif_invitation: impl Fn(NovaId, &str) -> Option<PermissionSet> + Send + 'static,
        identite_reseau: Option<IdentiteReseau>,
        capturer_factory: Option<FabriqueCapteur>,
        injector_factory: Option<FabriqueInjecteur>,
    ) -> Result<UnattendedHostHandle> {
        let service = Service::nouveau(
            local_id,
            rendezvous,
            stun_servers,
            identity,
            permissions,
            identite_reseau,
            capturer_factory,
            injector_factory,
        );
        service.lancer(move |service| {
            service.boucle_admission(&accept, &verif_mdp, &est_de_confiance, &verif_invitation);
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

    /// Nombre d'appelants **refusés** : par le hook d'acceptation (démarrage
    /// historique [`UnattendedHost::start`]), ou par le contrôle d'admission
    /// automatique ([`UnattendedHost::start_with_admission`] : mot de passe
    /// invalide, ou aucune preuve et approbation manuelle négative/expirée).
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
    /// Identité réseau optionnelle : `Some(..)` → enregistrement **authentifié**
    /// au rendez-vous de production ; `None` → enregistrement **nu**.
    identite_reseau: Option<IdentiteReseau>,
    /// Fabrique de capteur d'écran fournie à chaque époque servie (`None` =
    /// capteur système par défaut). Raccord de l'accès non surveillé en service :
    /// `nd-service` y branche son `CapteurAssistant`. Voir [`FabriqueCapteur`].
    capturer_factory: Option<FabriqueCapteur>,
    /// Fabrique d'injecteur d'entrées fournie à chaque époque servie (`None` =
    /// injecteur système par défaut). Voir [`FabriqueInjecteur`].
    injector_factory: Option<FabriqueInjecteur>,
    stop: Arc<AtomicBool>,
    compteurs: Arc<CompteursSession>,
    sessions_servies: Arc<AtomicU64>,
    pairs_refuses: Arc<AtomicU64>,
    derniere_erreur: Arc<Mutex<Option<String>>>,
}

impl Service {
    /// Construit l'état du service (et les poignées partagées qui iront dans la
    /// [`UnattendedHostHandle`]).
    #[allow(clippy::too_many_arguments)]
    fn nouveau(
        local_id: NovaId,
        rendezvous: SocketAddr,
        stun_servers: Vec<SocketAddr>,
        identity: ServerIdentity,
        permissions: PermissionSet,
        identite_reseau: Option<IdentiteReseau>,
        capturer_factory: Option<FabriqueCapteur>,
        injector_factory: Option<FabriqueInjecteur>,
    ) -> Service {
        Service {
            local_id,
            rendezvous,
            stun_servers,
            identity,
            permissions,
            identite_reseau,
            capturer_factory,
            injector_factory,
            stop: Arc::new(AtomicBool::new(false)),
            compteurs: Arc::new(CompteursSession::default()),
            sessions_servies: Arc::new(AtomicU64::new(0)),
            pairs_refuses: Arc::new(AtomicU64::new(0)),
            derniere_erreur: Arc::new(Mutex::new(None)),
        }
    }

    /// Lance le thread de service avec `corps` (l'une des boucles) et rend la
    /// poignée d'observation/arrêt.
    fn lancer(self, corps: impl FnOnce(&Service) + Send + 'static) -> Result<UnattendedHostHandle> {
        let stop = Arc::clone(&self.stop);
        let compteurs = Arc::clone(&self.compteurs);
        let sessions_servies = Arc::clone(&self.sessions_servies);
        let pairs_refuses = Arc::clone(&self.pairs_refuses);
        let derniere_erreur = Arc::clone(&self.derniere_erreur);
        let thread = thread::Builder::new()
            .name("nd-hote-non-surveille".to_owned())
            .spawn(move || corps(&self))?;

        Ok(UnattendedHostHandle {
            stop,
            thread: Some(thread),
            compteurs,
            sessions_servies,
            pairs_refuses,
            derniere_erreur,
        })
    }

    /// Boucle de service **historique** : le crochet `accept` tranche au moment
    /// du punch (avant l'acceptation QUIC) ; un admis est servi aussitôt.
    fn boucle(&self, accept: &(impl Fn(NovaId) -> bool + Send + 'static)) {
        // Admission : hook de l'appelant + compteur de refus.
        let refus = Arc::clone(&self.pairs_refuses);
        let admission = move |pair: NovaId| {
            let admis = accept(pair);
            if !admis {
                refus.fetch_add(1, Ordering::Relaxed);
            }
            admis
        };
        self.boucle_attente(&admission, &|transport, pair| {
            self.sessions_servies.fetch_add(1, Ordering::Relaxed);
            if let Err(erreur) = vivre_epoque_hote(transport, &self.params_epoque(pair)) {
                self.note_erreur(&format!("session avec {pair} : {erreur}"));
            }
        });
    }

    /// Boucle de service à **admission automatique** : tout appelant atteint le
    /// canal Noise, la décision se prend **dedans** — appareil de confiance,
    /// mot de passe prouvé, invitation valide, sinon repli sur le crochet manuel
    /// enrichi (voir [`UnattendedHost::start_with_admission_enrichie`]). Les
    /// compteurs `sessions_servies`/`pairs_refuses` sont tenus par l'époque
    /// d'admission.
    fn boucle_admission(
        &self,
        crochet_manuel: &impl Fn(&DemandeAdmissionManuelle) -> bool,
        verif_mdp: &impl Fn(&str) -> bool,
        est_de_confiance: &impl Fn(NovaId) -> bool,
        verif_invitation: &impl Fn(NovaId, &str) -> Option<PermissionSet>,
    ) {
        self.boucle_attente(&|_pair| true, &|transport, pair| {
            let controle = ControleAdmission {
                verif_mdp,
                est_de_confiance,
                verif_invitation,
                crochet_manuel,
                sessions_servies: &self.sessions_servies,
                pairs_refuses: &self.pairs_refuses,
            };
            if let Err(erreur) =
                vivre_epoque_hote_avec_admission(transport, self.params_epoque(pair), &controle)
            {
                self.note_erreur(&format!("session avec {pair} : {erreur}"));
            }
        });
    }

    /// Boucle d'attente commune : publie l'ID au rendez-vous, attend un
    /// appelant (filtre d'admission au punch fourni) et confie chaque entrant à
    /// `servir` — une session à la fois, jusqu'au signal d'arrêt.
    fn boucle_attente(
        &self,
        admission: &dyn Fn(NovaId) -> bool,
        servir: &dyn Fn(QuicTransport, NovaId),
    ) {
        let rv = RendezvousClient::new(self.rendezvous);
        while !self.stop.load(Ordering::Relaxed) {
            let attente = AttenteRendezvous {
                rv: &rv,
                local_id: self.local_id,
                identite: &self.identity,
                stun_servers: &self.stun_servers,
                relay: None,
                admission,
                // Présente → `register` authentifié ; `None` → nu (dev / LAN).
                identite_reseau: self.identite_reseau.as_ref(),
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
            servir(transport, pair);
        }
    }

    /// Paramètres d'une époque hôte du service pour l'appelant `pair`.
    fn params_epoque(&self, pair: NovaId) -> ParamsEpoqueHote<'_> {
        ParamsEpoqueHote {
            permissions: self.permissions,
            flux: HostStreamOptions::default(),
            compteurs: &self.compteurs,
            stop: &self.stop,
            etats: None,
            pair,
            // Raccourcis hôte par défaut ; `Disconnect` ne termine que la
            // session en cours — le service retourne à l'attente.
            raccourcis: crate::raccourcis_hote_defaut(),
            deconnexion_globale: false,
            // Fabriques injectées au démarrage du service (clonées par époque) :
            // `nd-service` y fournit capteur/injecteur adossés à l'assistant, de
            // sorte que la boucle hôte capture le vrai bureau et injecte dans la
            // session de l'utilisateur. `None` = capteur/injecteur système.
            capturer_factory: self.capturer_factory.clone(),
            injector_factory: self.injector_factory.clone(),
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
