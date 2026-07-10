//! Installation / désinstallation du service auprès du SCM, et activation de la
//! politique SAS. **Droits administrateur requis** (ouverture du SCM en écriture,
//! écriture `HKLM`).
//!
//! * [`installer`] enregistre `novadesk-svc` en **démarrage automatique**, compte
//!   **LocalSystem** (session 0), commande `novadesk-svc run` ; sème la
//!   configuration machine (identité TLS + `config.json`) et active la génération
//!   logicielle du SAS (`SoftwareSASGeneration = 3`).
//! * [`desinstaller`] arrête (si besoin) puis supprime le service.

use std::ffi::OsString;
use std::thread;
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::{config, sas, NOM_AFFICHAGE, NOM_SERVICE};

/// Description affichée dans la console des services.
const DESCRIPTION: &str = "Hôte NovaDesk pour l'accès à distance non surveillé (LocalSystem, \
                           session 0). Publie l'ID de la machine, admet selon mot de passe / \
                           liste de confiance, autorise Ctrl+Alt+Suppr (SAS).";

/// Installe le service, sème la configuration machine et active le SAS.
///
/// # Errors
/// Erreur si le SCM refuse la création (droits administrateur manquants, service
/// déjà présent), si la préparation de la configuration échoue, ou si l'écriture
/// de la politique SAS échoue.
pub fn installer() -> Result<(), String> {
    let gestionnaire = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| format!("ouverture du gestionnaire de services impossible (admin ?) : {e}"))?;

    let exe = std::env::current_exe()
        .map_err(|e| format!("chemin de l'exécutable courant introuvable : {e}"))?;

    let infos = ServiceInfo {
        name: OsString::from(NOM_SERVICE),
        display_name: OsString::from(NOM_AFFICHAGE),
        service_type: ServiceType::OWN_PROCESS,
        // Démarrage automatique avec le système (accès non surveillé persistant).
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        // Argument passé par le SCM au démarrage : sous-commande `run`.
        launch_arguments: vec![OsString::from("run")],
        dependencies: vec![],
        // `None` = compte LocalSystem (SYSTEM, session 0).
        account_name: None,
        account_password: None,
    };

    let service = gestionnaire
        .create_service(&infos, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(|e| format!("création du service « {NOM_SERVICE} » impossible : {e}"))?;
    service
        .set_description(DESCRIPTION)
        .map_err(|e| format!("description du service impossible : {e}"))?;

    // Sème le répertoire machine (identité TLS + config.json) et affiche l'ID.
    let repertoire = config::repertoire_service();
    let id = config::preparer_config_initiale(&repertoire)?;

    // Autorise `SendSAS` depuis les services (Ctrl+Alt+Suppr).
    sas::activer_generation_sas()?;

    println!("Service « {NOM_SERVICE} » installé (démarrage automatique, LocalSystem).");
    println!("ID machine NovaDesk : {id}");
    println!(
        "Configuration : {} — renseignez « serveur_rendezvous » (et le mot de passe) avant de \
         démarrer le service.",
        repertoire.join("config.json").display()
    );
    println!("Politique SAS activée (SoftwareSASGeneration = 3).");
    Ok(())
}

/// Désinstalle le service (arrêt préalable si nécessaire).
///
/// # Errors
/// Erreur si le service est introuvable ou si sa suppression échoue.
pub fn desinstaller() -> Result<(), String> {
    let gestionnaire = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("ouverture du gestionnaire de services impossible : {e}"))?;

    let service = gestionnaire
        .open_service(
            NOM_SERVICE,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(|e| format!("service « {NOM_SERVICE} » introuvable : {e}"))?;

    // Arrête le service s'il tourne, et attend qu'il soit à l'arrêt (au plus ~10 s).
    if let Ok(statut) = service.query_status() {
        if statut.current_state != ServiceState::Stopped {
            let _ = service.stop();
            let echeance = Instant::now() + Duration::from_secs(10);
            while Instant::now() < echeance {
                match service.query_status() {
                    Ok(s) if s.current_state == ServiceState::Stopped => break,
                    _ => thread::sleep(Duration::from_millis(250)),
                }
            }
        }
    }

    service
        .delete()
        .map_err(|e| format!("suppression du service « {NOM_SERVICE} » impossible : {e}"))?;
    println!("Service « {NOM_SERVICE} » désinstallé.");
    Ok(())
}
