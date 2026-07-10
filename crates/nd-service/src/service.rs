//! Mode **service** : point d'entrée appelé par le gestionnaire de contrôle des
//! services (SCM) via `novadesk-svc run`.
//!
//! Le SCM lance le processus, qui rend la main au dispatcher
//! ([`lancer_dispatcher`]) ; celui-ci appelle [`service_main`] sur un thread
//! dédié. On y enregistre un gestionnaire de contrôle (Stop/Shutdown), on passe à
//! l'état `Running`, on démarre l'hôte non surveillé puis on **attend** l'ordre
//! d'arrêt avant de fermer proprement.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::{config, hote, journal, session0, NOM_SERVICE};

/// Type de service : processus propre (un seul service par processus).
const TYPE_SERVICE: ServiceType = ServiceType::OWN_PROCESS;

define_windows_service!(ffi_service_main, service_main);

/// Rend la main au SCM : bloque jusqu'à la fin du service. Appelé par
/// `novadesk-svc run`.
///
/// # Errors
/// Erreur si le processus n'a pas été démarré par le SCM (ex. lancé à la main
/// sans contexte de service) — utiliser `run-console` pour un test interactif.
pub fn lancer_dispatcher() -> Result<(), String> {
    service_dispatcher::start(NOM_SERVICE, ffi_service_main).map_err(|e| {
        format!("démarrage du dispatcher de service impossible (le SCM doit lancer « run ») : {e}")
    })
}

/// Corps du service, exécuté sur le thread fourni par le dispatcher.
fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = executer_service() {
        // Sans console : on ne peut que journaliser (le SCM ne lit pas stderr).
        journal::journaliser_defaut(&format!("service : échec — {e}"));
    }
}

/// Construit un [`ServiceStatus`] pour l'état `etat` (contrôles Stop/Shutdown
/// acceptés une fois en marche).
fn statut(etat: ServiceState, accepte_arret: bool) -> ServiceStatus {
    ServiceStatus {
        service_type: TYPE_SERVICE,
        current_state: etat,
        controls_accepted: if accepte_arret {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

/// Enregistrement du gestionnaire, démarrage de l'hôte, attente de l'arrêt.
fn executer_service() -> Result<(), String> {
    let repertoire = config::repertoire_service();
    journal::journaliser(&repertoire, "service : démarrage");

    // Canal d'arrêt alimenté par le gestionnaire de contrôle (thread du SCM).
    let (tx_arret, rx_arret) = mpsc::channel::<()>();
    let gestion = move |controle| -> ServiceControlHandlerResult {
        match controle {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx_arret.send(());
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let poignee_statut = service_control_handler::register(NOM_SERVICE, gestion)
        .map_err(|e| format!("enregistrement du gestionnaire de contrôle impossible : {e}"))?;

    // Diagnostic session 0 ↔ session active (voir `session0`).
    journal::journaliser(&repertoire, &session0::etat_session_active());

    // Démarrage de l'hôte : une configuration incomplète (ex. rendez-vous absent)
    // ne fait pas planter le service — il reste vivant pour permettre l'arrêt
    // propre et laisse une trace dans le journal.
    let poignee_hote = match config::charger(repertoire.clone()) {
        Ok(cfg) => {
            journal::journaliser(
                &repertoire,
                &format!(
                    "hôte : ID {} — publication au rendez-vous {}",
                    cfg.id, cfg.rendezvous
                ),
            );
            match hote::demarrer(&cfg) {
                Ok(poignee) => Some(poignee),
                Err(e) => {
                    journal::journaliser(&repertoire, &format!("hôte non démarré : {e}"));
                    None
                }
            }
        }
        Err(e) => {
            journal::journaliser(&repertoire, &format!("configuration invalide : {e}"));
            None
        }
    };

    // En marche : le service accepte désormais Stop/Shutdown.
    poignee_statut
        .set_service_status(statut(ServiceState::Running, true))
        .map_err(|e| format!("passage à l'état « en marche » impossible : {e}"))?;

    // Attente bloquante de l'ordre d'arrêt.
    let _ = rx_arret.recv();
    journal::journaliser(&repertoire, "service : arrêt demandé");

    // Arrêt en cours (le SCM patiente).
    let _ = poignee_statut.set_service_status(statut(ServiceState::StopPending, false));
    if let Some(poignee) = poignee_hote {
        poignee.stop();
    }
    poignee_statut
        .set_service_status(statut(ServiceState::Stopped, false))
        .map_err(|e| format!("passage à l'état « arrêté » impossible : {e}"))?;
    journal::journaliser(&repertoire, "service : arrêté");
    Ok(())
}
