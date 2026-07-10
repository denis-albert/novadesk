//! **Mode assistant** (`novadesk-svc helper <chemin_pipe>`) : le passager de la
//! voie [`crate::session0`]. Lancé par le service **dans la session interactive
//! active**, il fait ce que la session 0 ne peut pas :
//!
//! * il **capture** le bureau réel de l'utilisateur ([`nd_capture::ScreenCapturer`],
//!   qui fonctionne dans une session interactive mais filmerait un bureau vide en
//!   session 0) et **envoie les trames** au service par le tube nommé
//!   ([`crate::tube`], protocole [`crate::canal`]) ;
//! * il **reçoit les entrées** du service et les **injecte**
//!   ([`nd_input::InputInjector`]) ;
//! * il **suit le bureau d'entrée** ([`crate::bureau`]) : à chaque bascule (UAC,
//!   verrouillage → `Winlogon`) il ré-associe son thread et **recrée le capteur**,
//!   pour continuer à filmer/piloter le bureau sécurisé (sous réserve d'être lancé
//!   en SYSTEM — voir [`crate::bureau`]).
//!
//! # Threads
//!
//! ```text
//! executer()
//!   ├─ thread « entrées »  : lit le canal service→assistant, injecte, relaie les
//!   │                        commandes capteur (région / moniteur / config)
//!   └─ thread principal    : boucle de capture → trames sur le canal assistant→service
//! ```
//!
//! Le capteur et l'injecteur sont **sensibles au bureau** : chaque thread associe
//! le sien au bureau d'entrée courant avant d'agir. La capture et l'injection sur
//! le **bureau sécurisé** exigent que le processus tourne en SYSTEM (privilège que
//! le service confère en lançant l'assistant avec le jeton adéquat).
//!
//! (Le module est déjà compilé sous `#[cfg(windows)]` par sa déclaration dans
//! `main.rs`.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nd_capture::{CaptureConfig, Rect, ScreenCapturer};
use nd_proto::MonitorId;

use crate::bureau::{self, BureauEntree};
use crate::canal::{self, MessageAssistant, MessageService};
use crate::tube;

/// Cadence de capture cible par défaut (le pipeline descend plus bas si l'écran
/// est statique — les trames vides restent minuscules sur le canal).
const FPS_DEFAUT: u32 = 30;

/// Commande interne relayée du thread « entrées » vers la boucle de capture (le
/// capteur vit sur le thread principal, associé au bon bureau).
enum CommandeCapture {
    /// Restreindre / rétablir la région capturée (« cadre d'écran »).
    Region(Option<Rect>),
    /// Basculer la diffusion vers le moniteur d'index donné.
    Moniteur(u32),
    /// Reconfigurer la capture (cadence, curseur).
    Configurer { fps: u32, curseur: bool },
}

/// Point d'entrée du mode assistant : se connecte au tube du service et sert la
/// session jusqu'à l'arrêt (ordre du service, ou tube rompu).
///
/// # Errors
/// Erreur si la connexion au tube échoue, ou si l'injecteur/capteur est
/// indisponible au démarrage.
pub fn executer(chemin_pipe: &str) -> Result<(), String> {
    // Deux tubes, un par sens (voir `tube::noms_duplex`) : on se connecte à a2s
    // **puis** s2a, ordre dans lequel le service les accepte.
    let (nom_a2s, nom_s2a) = tube::noms_duplex(chemin_pipe);
    // Sens assistant→service : on ne garde que l'**écrivain** (trames).
    let (_, ecrivain) = tube::connecter_client(&nom_a2s)
        .map_err(|e| format!("connexion au tube « {nom_a2s} » impossible : {e}"))?
        .scinder();
    // Sens service→assistant : on ne garde que le **lecteur** (entrées).
    let (lecteur, _) = tube::connecter_client(&nom_s2a)
        .map_err(|e| format!("connexion au tube « {nom_s2a} » impossible : {e}"))?
        .scinder();

    let arret = Arc::new(AtomicBool::new(false));
    let (tx_cmd, rx_cmd) = mpsc::channel::<CommandeCapture>();

    // Thread « entrées » : lit le canal service→assistant, injecte, relaie les
    // commandes capteur. L'injecteur est créé ici (échec = assistant inutile).
    let injecteur = nd_input::create_injector()
        .map_err(|e| format!("injecteur d'entrées indisponible : {e}"))?;
    let arret_entrees = Arc::clone(&arret);
    let poignee_entrees = thread::Builder::new()
        .name("nd-assistant-entrees".to_owned())
        .spawn(move || boucle_entrees(lecteur, injecteur.as_ref(), &tx_cmd, &arret_entrees))
        .map_err(|e| format!("thread d'entrées impossible : {e}"))?;

    // Boucle de capture sur le thread principal.
    let resultat = boucle_capture(ecrivain, &rx_cmd, &arret, None);

    // Fin : signale l'arrêt et attend le thread d'entrées.
    arret.store(true, Ordering::Relaxed);
    let _ = poignee_entrees.join();
    resultat
}

// ---------------------------------------------------------------------------
// Suivi du bureau d'entrée (par thread)
// ---------------------------------------------------------------------------

/// Garde, pour un thread donné, le **bureau d'entrée** auquel il est associé, et
/// le ré-associe quand l'entrée bascule (UAC / verrouillage → `Winlogon`).
#[derive(Default)]
struct SuiviBureau {
    courant: Option<BureauEntree>,
}

impl SuiviBureau {
    /// Assure l'association du thread courant au bureau d'entrée **actuel**.
    /// Renvoie `Ok(true)` s'il a changé (ré-association effectuée), `Ok(false)`
    /// s'il est inchangé. Une erreur (bureau sécurisé sans SYSTEM…) est
    /// **non fatale** : l'appelant conserve l'association précédente.
    fn assurer(&mut self) -> Result<bool, String> {
        let entree = bureau::ouvrir_bureau_entree()?;
        let change = self.courant.as_ref().is_none_or(|c| c.nom != entree.nom);
        if change {
            entree.associer_thread()?;
            self.courant = Some(entree);
        }
        Ok(change)
    }

    /// Nom du bureau courant (diagnostic), `?` si aucun.
    fn nom(&self) -> &str {
        self.courant.as_ref().map_or("?", |c| c.nom.as_str())
    }
}

// ---------------------------------------------------------------------------
// Boucle de capture (thread principal)
// ---------------------------------------------------------------------------

/// Boucle de capture : émet [`MessageAssistant::Pret`], la liste des moniteurs,
/// puis les trames. Applique les commandes capteur relayées et **recrée le
/// capteur** à chaque bascule de bureau. S'arrête sur `arret`, sur tube rompu, ou
/// après `max_trames` trames **porteuses d'image** (borne de test ; `None` = infini).
///
/// # Errors
/// Erreur si le capteur ne peut être créé/démarré au départ.
fn boucle_capture<W: std::io::Write>(
    mut ecrivain: W,
    rx_cmd: &Receiver<CommandeCapture>,
    arret: &Arc<AtomicBool>,
    max_trames: Option<usize>,
) -> Result<(), String> {
    // Poignée de main : signale la connexion et publie les moniteurs.
    if canal::ecrire_assistant(&mut ecrivain, &MessageAssistant::Pret).is_err() {
        return Ok(());
    }
    if let Ok(moniteurs) = nd_capture::enumerate_monitors() {
        let _ = canal::ecrire_assistant(&mut ecrivain, &MessageAssistant::Moniteurs(moniteurs));
    }

    let mut suivi = SuiviBureau::default();
    let _ = suivi.assurer(); // associe au bureau courant avant de créer le capteur

    let mut monitor = MonitorId(0);
    let mut config = CaptureConfig {
        monitor,
        target_fps: FPS_DEFAUT,
        capture_cursor: true,
    };
    let mut region: Option<Rect> = None;

    let mut capteur = creer_capteur(config, region, &mut ecrivain)
        .ok_or_else(|| "capteur d'écran indisponible dans la session".to_owned())?;

    let mut envoyees = 0usize;
    while !arret.load(Ordering::Relaxed) {
        // Applique les commandes capteur en attente (venues du thread d'entrées).
        while let Ok(cmd) = rx_cmd.try_recv() {
            match cmd {
                CommandeCapture::Region(r) => {
                    region = r;
                    let _ = capteur.set_region(r);
                }
                CommandeCapture::Moniteur(index) => {
                    monitor = MonitorId(index);
                    config.monitor = monitor;
                    let _ = capteur.start(config);
                    let _ = capteur.set_region(region);
                }
                CommandeCapture::Configurer { fps, curseur } => {
                    config.target_fps = fps;
                    config.capture_cursor = curseur;
                    let _ = capteur.start(config);
                    let _ = capteur.set_region(region);
                }
            }
        }

        // Suit le bureau d'entrée : une bascule (UAC/verrouillage) exige de recréer
        // le capteur pour lier la nouvelle duplication au bon bureau.
        if let Ok(true) = suivi.assurer() {
            let _ = canal::ecrire_assistant(
                &mut ecrivain,
                &MessageAssistant::Evenement(nd_capture::CaptureEvent::SecureDesktop),
            );
            if let Some(nouveau) = creer_capteur(config, region, &mut ecrivain) {
                capteur = nouveau;
            }
        }

        // Événements hors flux du backend (résolution…), s'il en remonte.
        while let Some(ev) = capteur.poll_event() {
            let _ = canal::ecrire_assistant(&mut ecrivain, &MessageAssistant::Evenement(ev));
        }

        match capteur.next_frame() {
            Ok(trame) => {
                let a_image = trame.image.is_some();
                if canal::ecrire_assistant(&mut ecrivain, &MessageAssistant::Trame(Box::new(trame)))
                    .is_err()
                {
                    // Tube rompu : le service est parti, fin propre.
                    break;
                }
                if a_image {
                    envoyees += 1;
                    if max_trames.is_some_and(|max| envoyees >= max) {
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = canal::ecrire_assistant(
                    &mut ecrivain,
                    &MessageAssistant::Erreur(format!("capture ({}) : {e}", suivi.nom())),
                );
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    capteur.stop();
    Ok(())
}

/// Crée et démarre un capteur pour `config`/`region` ; en cas d'échec, émet une
/// erreur sur le canal et renvoie `None` (l'appelant décide de la suite).
fn creer_capteur<W: std::io::Write>(
    config: CaptureConfig,
    region: Option<Rect>,
    ecrivain: &mut W,
) -> Option<Box<dyn ScreenCapturer>> {
    match nd_capture::create_capturer() {
        Ok(mut capteur) => match capteur.start(config) {
            Ok(()) => {
                let _ = capteur.set_region(region);
                Some(capteur)
            }
            Err(e) => {
                let _ = canal::ecrire_assistant(
                    ecrivain,
                    &MessageAssistant::Erreur(format!("démarrage capture : {e}")),
                );
                None
            }
        },
        Err(e) => {
            let _ = canal::ecrire_assistant(
                ecrivain,
                &MessageAssistant::Erreur(format!("création capteur : {e}")),
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Boucle d'entrées (thread dédié)
// ---------------------------------------------------------------------------

/// Lit le canal service→assistant : injecte les entrées (en suivant le bureau) et
/// relaie les commandes capteur au thread de capture. Se termine sur `Arret`, tube
/// rompu (EOF) ou `arret` levé ailleurs.
fn boucle_entrees<R: std::io::Read>(
    mut lecteur: R,
    injecteur: &dyn nd_input::InputInjector,
    tx_cmd: &Sender<CommandeCapture>,
    arret: &Arc<AtomicBool>,
) {
    let mut suivi = SuiviBureau::default();
    while !arret.load(Ordering::Relaxed) {
        let message = match canal::lire_service(&mut lecteur) {
            Ok(m) => m,
            // EOF = service déconnecté ; toute autre erreur = tube inutilisable.
            Err(_) => break,
        };
        match message {
            MessageService::Entree(evenement) => {
                // Injecter vise le bureau d'entrée du thread : on l'y associe.
                let _ = suivi.assurer();
                let _ = canal::appliquer_entree(injecteur, evenement);
            }
            MessageService::DefinirRegion(region) => {
                let _ = tx_cmd.send(CommandeCapture::Region(region));
            }
            MessageService::BasculerMoniteur(index) => {
                let _ = tx_cmd.send(CommandeCapture::Moniteur(index));
            }
            MessageService::Configurer {
                monitor,
                fps,
                curseur,
            } => {
                // Le moniteur initial passe aussi par une bascule.
                let _ = tx_cmd.send(CommandeCapture::Moniteur(monitor));
                let _ = tx_cmd.send(CommandeCapture::Configurer { fps, curseur });
            }
            MessageService::Arret => break,
        }
    }
    arret.store(true, Ordering::Relaxed);
    // Relâche toutes les touches/boutons en fin de session (anti « stuck key »).
    injecteur.release_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Preuve capture → tube nommé → décodage** (hors service) : un thread
    /// serveur lit une trame ; le « client » (rôle assistant) capture le bureau
    /// réel de la session de test et l'émet via [`boucle_capture`] (bornée à 1
    /// trame porteuse d'image). Tolérant : dans une session sans bureau
    /// interactif (headless / session 0 / CI), la capture est indisponible et le
    /// test passe sans trame — on n'exige jamais un environnement graphique.
    #[test]
    fn capture_reelle_transmise_par_le_tube() {
        let chemin = tube::chemin_unique(0xCAB7);
        let serveur = tube::ServeurTube::creer(&chemin).expect("création serveur");

        // Serveur : accepte, puis lit des messages jusqu'à une trame ou EOF.
        let jeton = thread::spawn(move || {
            let extremite = serveur.attendre_client().expect("client connecté");
            let (mut lecteur, _ecrivain) = extremite.scinder();
            let mut trame_vue = false;
            let mut pret_vu = false;
            loop {
                match canal::lire_assistant(&mut lecteur) {
                    Ok(MessageAssistant::Pret) => pret_vu = true,
                    Ok(MessageAssistant::Trame(f)) => {
                        assert!(f.width > 0 && f.height > 0, "trame aux dimensions nulles");
                        trame_vue = true;
                        break;
                    }
                    // Moniteurs / erreurs / événements : on continue de lire.
                    Ok(_) => {}
                    Err(_) => break, // EOF : capture indisponible, toléré
                }
            }
            (pret_vu, trame_vue)
        });

        // Client (rôle assistant) : capture réelle → 1 trame → tube.
        let extremite = {
            let mut essais = 0;
            loop {
                match tube::connecter_client(&chemin) {
                    Ok(e) => break e,
                    Err(_) if essais < 100 => {
                        essais += 1;
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("connexion client impossible : {e}"),
                }
            }
        };
        let (_lecteur, ecrivain) = extremite.scinder();
        let (_tx, rx) = mpsc::channel::<CommandeCapture>();
        let arret = Arc::new(AtomicBool::new(false));
        // Borne à 1 trame porteuse d'image ; si la capture est indisponible,
        // `boucle_capture` émet Pret puis une Erreur et rend la main (pas de trame).
        let _ = boucle_capture(ecrivain, &rx, &arret, Some(1));

        let (pret_vu, trame_vue) = jeton.join().expect("thread serveur");
        assert!(pret_vu, "la poignée de main (Prêt) doit toujours arriver");
        // Si un bureau interactif est présent, on a réellement filmé une trame.
        if !trame_vue {
            eprintln!(
                "note : aucune trame porteuse d'image (session sans bureau interactif) — toléré"
            );
        }
    }
}
