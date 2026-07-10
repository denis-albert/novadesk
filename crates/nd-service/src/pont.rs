//! **Pont service ↔ assistant** (côté service, session 0).
//!
//! À l'établissement d'une session non surveillée, le service :
//!
//! 1. crée le **serveur** de tube nommé ([`crate::tube`]) ;
//! 2. **lance l'assistant** dans la session active ([`crate::session0`]) avec
//!    l'argument `helper <chemin_pipe>` ;
//! 3. attend sa connexion, puis démarre un **thread de lecture** qui décode les
//!    [`MessageAssistant`](crate::canal::MessageAssistant) : les **trames** vont
//!    dans une file bornée (consommée par [`CapteurAssistant`]), les événements /
//!    moniteurs / erreurs alimentent un état partagé.
//!
//! Le pont expose alors deux **adaptateurs prêts pour le moteur de session** :
//!
//! * [`CapteurAssistant`] implémente [`nd_capture::ScreenCapturer`] — chaque
//!   `next_frame` rend une trame réelle du bureau utilisateur (venue de
//!   l'assistant), là où un capteur créé en session 0 ne verrait qu'un bureau vide ;
//! * [`InjecteurAssistant`] implémente [`nd_input::InputInjector`] — chaque entrée
//!   est encodée et transmise à l'assistant, qui l'injecte dans la session.
//!
//! # Raccordement au pipeline vidéo (encodeur)
//!
//! Ces deux adaptateurs ont **exactement** la forme attendue par le moteur hôte de
//! `nd-core` : `nd-core` accepte désormais un capteur/injecteur **injecté** par
//! époque via des **fabriques** additives
//! ([`nd_core::UnattendedHost::start_with_admission_enrichie_fabriques`],
//! [`nd_core::FabriqueCapteur`] / [`nd_core::FabriqueInjecteur`]) — à défaut, il
//! retombe sur ses `create_capturer()` / `create_injector()` système. Le raccord
//! est porté par le [`GestionnairePont`] : il (re)crée un pont **par époque
//! servie** et remet `pont.capteur()` / `pont.injecteur()` du **même** assistant
//! au moteur, dont les trames alimentent alors l'encodeur matériel puis le canal
//! vidéo chiffré (voir [`crate::hote::demarrer`]). Le pont reste par ailleurs
//! **pilotable directement** pour les essais manuels (voir la boucle assistant et
//! `probe-assistant`).
//!
//! # Cycle de vie
//!
//! La **mort de l'assistant** (crash, fermeture de session) ferme le tube : le
//! thread de lecture atteint l'EOF, [`PontAssistant::est_vivant`] passe à faux et
//! [`CapteurAssistant::next_frame`] finit par renvoyer une erreur — ce qui, dans le
//! moteur `nd-core`, **termine l'époque** ; le service revient en attente et
//! (re)lance un assistant à la session suivante. Recréer l'assistant **au sein**
//! d'une session (déverrouillage, reconnexion) se fait en construisant un nouveau
//! [`PontAssistant`].
//!
//! (Le module est déjà compilé sous `#[cfg(windows)]` par sa déclaration dans
//! `main.rs`.)

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nd_capture::{
    CaptureConfig, CaptureEvent, CapturedFrame, MonitorInfo, PixelFormat, Rect, ScreenCapturer,
};
use nd_input::{InputInjector, MouseButton};
use nd_proto::{MonitorId, NdError, Result};

use crate::canal::{self, EvenementEntree, MessageAssistant, MessageService};
use crate::session0;
use crate::tube::{EcrivainTube, LecteurTube, ServeurTube};

/// Profondeur de la file de trames (backpressure vidéo) : au-delà, la trame la plus
/// **récente** est abandonnée — on privilégie la fraîcheur (un flux temps réel
/// n'accumule pas de retard).
const FILE_TRAMES: usize = 8;

/// Délai d'attente d'une trame dans [`CapteurAssistant::next_frame`] avant de
/// rendre une trame **vide** (heartbeat, comme un délai DXGI) plutôt que de bloquer.
const DELAI_TRAME: Duration = Duration::from_secs(2);

/// Comment lancer l'assistant dans la session active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeLancement {
    /// Sous le **jeton de l'utilisateur** connecté (bureau `Default` uniquement).
    Utilisateur,
    /// En **SYSTEM** dans la session active : requis pour le **bureau sécurisé**
    /// (UAC / verrouillage / Winlogon). Voir [`crate::session0::lancer_systeme_dans_session_active`].
    Systeme,
}

/// État partagé entre le thread de lecture et les adaptateurs.
#[derive(Default)]
struct EtatPartage {
    /// L'assistant est-il connecté et le tube ouvert ?
    vivant: AtomicBool,
    /// Poignée de main reçue (l'assistant a démarré sa capture).
    pret: AtomicBool,
    /// Événements de capture hors flux (résolution, bureau sécurisé).
    evenements: Mutex<VecDeque<CaptureEvent>>,
    /// Derniers moniteurs annoncés par l'assistant.
    moniteurs: Mutex<Vec<MonitorInfo>>,
    /// Dernière erreur non fatale remontée par l'assistant.
    derniere_erreur: Mutex<Option<String>>,
}

/// Pont vivant vers un assistant lancé dans la session active.
pub struct PontAssistant {
    /// Serveur du tube assistant→service (le service y **lit** les trames).
    serveur_a2s: ServeurTube,
    /// Serveur du tube service→assistant (le service y **écrit** les entrées).
    serveur_s2a: ServeurTube,
    ecrivain: Arc<Mutex<EcrivainTube>>,
    rx_trames: Option<Receiver<CapturedFrame>>,
    etat: Arc<EtatPartage>,
    lecteur: Option<JoinHandle<()>>,
    pid: u32,
}

impl PontAssistant {
    /// Crée les deux tubes, lance l'assistant (`exe helper <base>`) dans la session
    /// active selon `mode`, attend ses connexions et démarre le thread de lecture.
    ///
    /// # Errors
    /// Erreur si un tube ne peut être créé, si le lancement dans la session active
    /// échoue (aucune session/utilisateur, privilège manquant), ou si l'assistant
    /// ne se connecte pas.
    pub fn demarrer(exe: &Path, mode: ModeLancement) -> std::result::Result<Self, String> {
        let base = nom_tube_unique();
        let (serveur_a2s, serveur_s2a) = creer_serveurs(&base)?;
        let args = vec!["helper".to_owned(), base];
        let pid = match mode {
            ModeLancement::Utilisateur => session0::lancer_dans_session_active(exe, &args)?,
            ModeLancement::Systeme => session0::lancer_systeme_dans_session_active(exe, &args)?,
        };
        Self::depuis_serveurs(serveur_a2s, serveur_s2a, pid)
    }

    /// Variante **locale** (essai manuel sans service) : lance l'assistant comme
    /// **processus enfant dans la session courante**, sans passer par la session 0
    /// ni SYSTEM. Ne couvre donc que le bureau `Default` (pas le bureau sécurisé),
    /// mais permet de vérifier capture → tube → capteur de bout en bout depuis une
    /// session interactive ordinaire.
    ///
    /// # Errors
    /// Erreur si un tube ne peut être créé, si le lancement du processus échoue, ou
    /// si l'assistant ne se connecte pas.
    pub fn demarrer_local(exe: &Path) -> std::result::Result<Self, String> {
        let base = nom_tube_unique();
        let (serveur_a2s, serveur_s2a) = creer_serveurs(&base)?;
        let enfant = std::process::Command::new(exe)
            .args(["helper", &base])
            .spawn()
            .map_err(|e| format!("lancement local de l'assistant impossible : {e}"))?;
        let pid = enfant.id();
        // On détache l'enfant : sa mort est détectée par l'EOF du tube (le thread
        // de lecture le remonte via `est_vivant`).
        drop(enfant);
        Self::depuis_serveurs(serveur_a2s, serveur_s2a, pid)
    }

    /// Accepte les deux connexions de l'assistant (a2s **puis** s2a, ordre suivi
    /// côté assistant) puis démarre le thread de lecture. Tail commun à
    /// [`Self::demarrer`] et [`Self::demarrer_local`].
    fn depuis_serveurs(
        serveur_a2s: ServeurTube,
        serveur_s2a: ServeurTube,
        pid: u32,
    ) -> std::result::Result<Self, String> {
        // Sens assistant→service : on ne garde que le **lecteur**.
        let (lecteur, _) = serveur_a2s
            .attendre_client()
            .map_err(|e| format!("assistant (PID {pid}) non connecté (a2s) : {e}"))?
            .scinder();
        // Sens service→assistant : on ne garde que l'**écrivain**.
        let (_, ecrivain) = serveur_s2a
            .attendre_client()
            .map_err(|e| format!("assistant (PID {pid}) non connecté (s2a) : {e}"))?
            .scinder();

        let (tx, rx) = mpsc::sync_channel::<CapturedFrame>(FILE_TRAMES);
        let etat = Arc::new(EtatPartage::default());
        etat.vivant.store(true, Ordering::Relaxed);
        let etat_lecteur = Arc::clone(&etat);
        let lecteur_th = thread::Builder::new()
            .name("nd-pont-lecture".to_owned())
            .spawn(move || boucle_lecture(lecteur, &tx, &etat_lecteur))
            .map_err(|e| format!("thread de lecture du pont impossible : {e}"))?;

        Ok(PontAssistant {
            serveur_a2s,
            serveur_s2a,
            ecrivain: Arc::new(Mutex::new(ecrivain)),
            rx_trames: Some(rx),
            etat,
            lecteur: Some(lecteur_th),
            pid,
        })
    }

    /// Récupère le **capteur** (une seule fois) : à confier au moteur de session
    /// comme [`nd_capture::ScreenCapturer`]. Rend `None` s'il a déjà été pris.
    pub fn capteur(&mut self) -> Option<CapteurAssistant> {
        self.rx_trames.take().map(|rx| CapteurAssistant {
            rx,
            ecrivain: Arc::clone(&self.ecrivain),
            etat: Arc::clone(&self.etat),
            origine: Instant::now(),
            derniere_dim: (0, 0, MonitorId(0)),
        })
    }

    /// Fabrique un **injecteur** partageant le tube ([`nd_input::InputInjector`]).
    #[must_use]
    pub fn injecteur(&self) -> InjecteurAssistant {
        InjecteurAssistant {
            ecrivain: Arc::clone(&self.ecrivain),
        }
    }

    /// L'assistant est-il toujours connecté (tube ouvert) ?
    #[must_use]
    pub fn est_vivant(&self) -> bool {
        self.etat.vivant.load(Ordering::Relaxed)
    }

    /// PID de l'assistant lancé.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Moniteurs annoncés par l'assistant (vide tant que non reçus).
    #[must_use]
    pub fn moniteurs(&self) -> Vec<MonitorInfo> {
        self.etat
            .moniteurs
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// Dernière erreur non fatale remontée par l'assistant.
    #[must_use]
    pub fn derniere_erreur(&self) -> Option<String> {
        self.etat
            .derniere_erreur
            .lock()
            .ok()
            .and_then(|e| e.clone())
    }

    /// Arrête l'assistant proprement (ordre d'arrêt + déconnexion + join).
    ///
    /// La **relance** en cours de session (déverrouillage, crash de l'assistant)
    /// se fait en construisant un nouveau [`PontAssistant`] : la mort de l'assistant
    /// clôt le flux ([`Self::est_vivant`] à faux, `next_frame` en erreur), le moteur
    /// termine l'époque, et le service repart en attente puis relance à la session
    /// suivante.
    pub fn arreter(mut self) {
        self.arreter_interne();
    }

    /// Corps de l'arrêt (réutilisé par [`Self::arreter`] et [`Drop`]).
    fn arreter_interne(&mut self) {
        // Ordre d'arrêt best-effort à l'assistant.
        if let Ok(mut e) = self.ecrivain.lock() {
            let _ = canal::ecrire_service(&mut *e, &MessageService::Arret);
        }
        self.serveur_a2s.deconnecter();
        self.serveur_s2a.deconnecter();
        self.etat.vivant.store(false, Ordering::Relaxed);
        // Libère le récepteur : le thread de lecture débloque son `try_send`.
        self.rx_trames = None;
        if let Some(th) = self.lecteur.take() {
            let _ = th.join();
        }
    }
}

impl Drop for PontAssistant {
    fn drop(&mut self) {
        self.arreter_interne();
    }
}

// ---------------------------------------------------------------------------
// Gestionnaire de pont : une fabrique de capteur/injecteur par époque servie
// ---------------------------------------------------------------------------

/// Gestionnaire du **pont assistant** pour l'hôte non surveillé servi : (re)crée
/// un [`PontAssistant`] **par époque** et remet son capteur / injecteur au moteur
/// de session `nd-core`, via les fabriques branchées sur
/// [`nd_core::UnattendedHost::start_with_admission_enrichie_fabriques`].
///
/// # Un pont par époque, capteur + injecteur appairés
///
/// Le service ne sert **qu'une session à la fois** et chaque époque a besoin d'un
/// assistant **neuf** (une capture n'est pas rejouable d'une connexion à
/// l'autre). Le moteur appelle les deux fabriques une fois par époque : le
/// gestionnaire lance un pont **au premier des deux appels** et remet l'autre
/// extrémité **du même** pont au second — capteur et injecteur parlent ainsi au
/// **même** assistant. Dès qu'une ressource déjà remise pour le pont courant est
/// redemandée (nouvelle époque), ou que l'assistant courant est mort, un pont
/// neuf est lancé et l'ancien **arrêté** (nettoyage du cycle de vie). L'**ordre**
/// des deux appels est indifférent.
///
/// # Repli (assistant indisponible)
///
/// Si l'assistant ne peut être lancé (pas de session active, privilège manquant),
/// [`Self::capteur`] / [`Self::injecteur`] rendent une erreur : la boucle hôte de
/// `nd-core` avorte alors proprement l'époque et le service retourne à l'attente
/// (voir la documentation de la fabrique côté `nd-core`).
pub struct GestionnairePont {
    /// Exécutable de l'assistant à lancer (le binaire du service lui-même,
    /// invoqué en `helper <pipe>` dans la session active).
    exe: PathBuf,
    /// Mode de lancement dans la session active (SYSTEM pour le bureau sécurisé).
    mode: ModeLancement,
    /// Pont courant + drapeaux « capteur / injecteur déjà remis pour l'époque ».
    courant: Option<PontActuel>,
}

/// Pont courant d'une époque et l'état de remise de ses deux extrémités.
struct PontActuel {
    pont: PontAssistant,
    capteur_pris: bool,
    injecteur_pris: bool,
}

impl GestionnairePont {
    /// Nouveau gestionnaire, sans pont vivant : le premier assistant est lancé à
    /// la première demande de capteur ou d'injecteur (donc à la première époque
    /// réellement servie).
    #[must_use]
    pub fn new(exe: PathBuf, mode: ModeLancement) -> Self {
        GestionnairePont {
            exe,
            mode,
            courant: None,
        }
    }

    /// Assure un pont **frais** pour la ressource demandée (`pour_capteur` =
    /// capteur, sinon injecteur) : réutilise le pont courant tant que la ressource
    /// n'en a pas déjà été tirée **et** qu'il est vivant ; sinon (nouvelle époque,
    /// ou assistant mort) arrête l'ancien et en lance un neuf.
    fn assurer_pont(&mut self, pour_capteur: bool) -> std::result::Result<&mut PontActuel, String> {
        let besoin_neuf = match &self.courant {
            None => true,
            Some(actuel) => {
                !actuel.pont.est_vivant()
                    || (pour_capteur && actuel.capteur_pris)
                    || (!pour_capteur && actuel.injecteur_pris)
            }
        };
        if besoin_neuf {
            // Fin de l'époque précédente : arrêt propre de l'assistant sortant.
            if let Some(ancien) = self.courant.take() {
                ancien.pont.arreter();
            }
            let pont = PontAssistant::demarrer(&self.exe, self.mode)?;
            self.courant = Some(PontActuel {
                pont,
                capteur_pris: false,
                injecteur_pris: false,
            });
        }
        Ok(self
            .courant
            .as_mut()
            .expect("pont courant assuré ci-dessus"))
    }

    /// Remet le **capteur** de l'époque courante (assistant → moteur).
    ///
    /// # Errors
    /// Assistant non démarrable (pas de session active, privilège manquant), ou
    /// capteur déjà consommé pour ce pont (ne devrait pas arriver : un pont neuf
    /// est assuré à chaque époque).
    pub fn capteur(&mut self) -> std::result::Result<CapteurAssistant, String> {
        let actuel = self.assurer_pont(true)?;
        let capteur = actuel
            .pont
            .capteur()
            .ok_or_else(|| "capteur de l'assistant déjà consommé".to_owned())?;
        actuel.capteur_pris = true;
        Ok(capteur)
    }

    /// Remet l'**injecteur** de l'époque courante (moteur → assistant).
    ///
    /// # Errors
    /// Assistant non démarrable (pas de session active, privilège manquant).
    pub fn injecteur(&mut self) -> std::result::Result<InjecteurAssistant, String> {
        let actuel = self.assurer_pont(false)?;
        let injecteur = actuel.pont.injecteur();
        actuel.injecteur_pris = true;
        Ok(injecteur)
    }
}

/// Nom de base unique pour une session assistant (PID service + compteur process).
fn nom_tube_unique() -> String {
    static COMPTEUR: AtomicU64 = AtomicU64::new(0);
    crate::tube::chemin_unique(COMPTEUR.fetch_add(1, Ordering::Relaxed))
}

/// Crée les deux serveurs de tube (un par sens) pour la base `base`.
fn creer_serveurs(base: &str) -> std::result::Result<(ServeurTube, ServeurTube), String> {
    let (a2s, s2a) = crate::tube::noms_duplex(base);
    let serveur_a2s = ServeurTube::creer(&a2s)
        .map_err(|e| format!("serveur de tube « {a2s} » impossible : {e}"))?;
    let serveur_s2a = ServeurTube::creer(&s2a)
        .map_err(|e| format!("serveur de tube « {s2a} » impossible : {e}"))?;
    Ok((serveur_a2s, serveur_s2a))
}

/// Thread de lecture : décode les messages de l'assistant et les aiguille.
fn boucle_lecture(
    mut lecteur: LecteurTube,
    tx: &SyncSender<CapturedFrame>,
    etat: &Arc<EtatPartage>,
) {
    loop {
        match canal::lire_assistant(&mut lecteur) {
            Ok(MessageAssistant::Pret) => etat.pret.store(true, Ordering::Relaxed),
            Ok(MessageAssistant::Trame(trame)) => match tx.try_send(*trame) {
                Ok(()) => {}
                // File pleine : on jette la trame (fraîcheur > exhaustivité).
                Err(TrySendError::Full(_)) => {}
                // Récepteur disparu (capteur libéré) : plus personne ne consomme.
                Err(TrySendError::Disconnected(_)) => break,
            },
            Ok(MessageAssistant::Evenement(ev)) => {
                if let Ok(mut file) = etat.evenements.lock() {
                    file.push_back(ev);
                }
            }
            Ok(MessageAssistant::Moniteurs(liste)) => {
                if let Ok(mut m) = etat.moniteurs.lock() {
                    *m = liste;
                }
            }
            Ok(MessageAssistant::Erreur(txt)) => {
                if let Ok(mut e) = etat.derniere_erreur.lock() {
                    *e = Some(txt);
                }
            }
            // EOF / tube rompu : l'assistant s'est arrêté.
            Err(_) => break,
        }
    }
    etat.vivant.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Adaptateur capteur (ScreenCapturer) : trames de l'assistant → moteur de session
// ---------------------------------------------------------------------------

/// Capteur alimenté par les trames de l'assistant, prêt pour le pipeline vidéo.
pub struct CapteurAssistant {
    rx: Receiver<CapturedFrame>,
    ecrivain: Arc<Mutex<EcrivainTube>>,
    etat: Arc<EtatPartage>,
    origine: Instant,
    /// Dernières dimensions vues (pour fabriquer une trame vide sur délai).
    derniere_dim: (u32, u32, MonitorId),
}

impl CapteurAssistant {
    /// Envoie une commande de contrôle à l'assistant.
    fn commander(&self, msg: &MessageService) -> Result<()> {
        let mut e = self
            .ecrivain
            .lock()
            .map_err(|_| NdError::Capture("écrivain du tube empoisonné".into()))?;
        canal::ecrire_service(&mut *e, msg).map_err(|err| NdError::Capture(err.to_string()))
    }

    /// Trame **vide** (pas de nouveau contenu) aux dernières dimensions connues :
    /// heartbeat rendu sur délai, comme un `AcquireNextFrame` en temps mort.
    fn trame_vide(&self) -> CapturedFrame {
        let (width, height, monitor) = self.derniere_dim;
        CapturedFrame {
            width,
            height,
            monitor,
            format: PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: self.origine.elapsed().as_micros() as u64,
            image: None,
        }
    }
}

impl ScreenCapturer for CapteurAssistant {
    fn start(&mut self, cfg: CaptureConfig) -> Result<()> {
        self.derniere_dim.2 = cfg.monitor;
        self.commander(&MessageService::Configurer {
            monitor: cfg.monitor.0,
            fps: cfg.target_fps,
            curseur: cfg.capture_cursor,
        })
    }

    fn next_frame(&mut self) -> Result<CapturedFrame> {
        match self.rx.recv_timeout(DELAI_TRAME) {
            Ok(trame) => {
                self.derniere_dim = (trame.width, trame.height, trame.monitor);
                Ok(trame)
            }
            // Temps mort : rien de neuf, on rend une trame vide (ré-encodage delta).
            Err(RecvTimeoutError::Timeout) => Ok(self.trame_vide()),
            // Producteur disparu : l'assistant est mort → l'époque doit se terminer.
            Err(RecvTimeoutError::Disconnected) => Err(NdError::Capture(
                "assistant déconnecté (flux de trames clos)".into(),
            )),
        }
    }

    fn poll_event(&mut self) -> Option<CaptureEvent> {
        self.etat
            .evenements
            .lock()
            .ok()
            .and_then(|mut f| f.pop_front())
    }

    fn stop(&mut self) {
        let _ = self.commander(&MessageService::Arret);
    }

    /// Restreint la capture à une sous-région : **délégué à l'assistant** (dont le
    /// backend DXGI borne la région). Contrairement au défaut du trait (qui refuse
    /// toute sous-région), on la transmet et on réussit.
    fn set_region(&mut self, region: Option<Rect>) -> Result<()> {
        self.commander(&MessageService::DefinirRegion(region))
    }
}

// ---------------------------------------------------------------------------
// Adaptateur injecteur (InputInjector) : entrées du contrôleur → assistant
// ---------------------------------------------------------------------------

/// Injecteur qui transmet chaque entrée à l'assistant pour injection en session.
pub struct InjecteurAssistant {
    ecrivain: Arc<Mutex<EcrivainTube>>,
}

impl InjecteurAssistant {
    /// Encode et transmet un événement d'entrée à l'assistant.
    fn envoyer(&self, evenement: EvenementEntree) -> Result<()> {
        let mut e = self
            .ecrivain
            .lock()
            .map_err(|_| NdError::Input("écrivain du tube empoisonné".into()))?;
        canal::ecrire_service(&mut *e, &MessageService::Entree(evenement))
            .map_err(|err| NdError::Input(err.to_string()))
    }
}

impl InputInjector for InjecteurAssistant {
    fn mouse_move_abs(&self, x: f64, y: f64, monitor: MonitorId) -> Result<()> {
        self.envoyer(EvenementEntree::SourisAbsolue {
            x,
            y,
            monitor: monitor.0,
        })
    }

    fn mouse_move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        self.envoyer(EvenementEntree::SourisRelative { dx, dy })
    }

    fn mouse_button(&self, btn: MouseButton, down: bool) -> Result<()> {
        self.envoyer(EvenementEntree::Bouton {
            bouton: btn,
            enfonce: down,
        })
    }

    fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        self.envoyer(EvenementEntree::Molette { dx, dy })
    }

    fn key(&self, scancode: u32, down: bool) -> Result<()> {
        self.envoyer(EvenementEntree::Touche {
            scancode,
            enfonce: down,
        })
    }

    fn unicode(&self, ch: char) -> Result<()> {
        self.envoyer(EvenementEntree::Unicode { caractere: ch })
    }

    fn release_all(&self) {
        let _ = self.envoyer(EvenementEntree::ToutRelacher);
    }
}
