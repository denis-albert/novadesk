//! `novadesk-svc` — **service Windows** de NovaDesk : hôte « accès non surveillé »
//! tournant en **LocalSystem** (session 0), prérequis du vrai accès non surveillé
//! persistant et du bureau sécurisé (SAS, Ctrl+Alt+Suppr).
//!
//! # Sous-commandes
//!
//! | Commande                     | Rôle                                                        |
//! |------------------------------|-------------------------------------------------------------|
//! | `install`                    | Enregistre le service (auto, LocalSystem) + politique SAS. **Admin.** |
//! | `uninstall`                  | Arrête et supprime le service. **Admin.**                   |
//! | `run`                        | Mode service (appelé par le SCM). Ne pas lancer à la main.  |
//! | `run-console`                | Lance l'hôte au premier plan (débogage, Ctrl+Entrée pour finir). |
//! | `set-password <mot>`         | Écrit le haché du mot de passe non surveillé dans la config. |
//! | `probe-session [exe [args…]]`| Diagnostic session 0 ; lance éventuellement `exe` dans la session active. |
//!
//! # Ce qui marche vs ce qui manque
//!
//! Le service **vit** en LocalSystem, lit sa **configuration machine** sous
//! `C:\ProgramData\NovaDesk` (voir [`config`]), **publie** son ID au rendez-vous,
//! **admet** selon mot de passe permanent / appareils de confiance / ACL (dans le
//! canal chiffré Noise) et **sert** une session — le tout sans surveillance. La
//! **capture du bureau de l'utilisateur interactif depuis la session 0** exige un
//! assistant lancé dans la session active : la **voie** est implémentée
//! ([`session0`]), l'**assistant** (capture + entrées + bureau sécurisé) reste à
//! écrire. Voir [`session0`] pour le détail.

// Modules indépendants de la plateforme (testables partout).
mod config;
mod hote;
mod journal;

// Modules spécifiques à Windows (SCM, registre, session 0).
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod sas;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod session0;

/// Nom court du service (clé SCM, `sc query novadesk-svc`).
pub const NOM_SERVICE: &str = "novadesk-svc";
/// Nom affiché dans la console des services.
pub const NOM_AFFICHAGE: &str = "NovaDesk — hôte accès non surveillé";

fn main() {
    std::process::exit(executer());
}

/// Analyse les arguments et exécute la sous-commande ; renvoie le code de sortie.
fn executer() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let commande = args.get(1).map(String::as_str).unwrap_or("help");
    match commande {
        "run" => run(),
        "run-console" => run_console(),
        "install" => sous_commande_admin(installer),
        "uninstall" => sous_commande_admin(desinstaller),
        "set-password" => set_password(&args),
        "probe-session" => probe_session(&args),
        "help" | "--help" | "-h" => {
            afficher_aide();
            0
        }
        autre => {
            eprintln!("commande inconnue : « {autre} »\n");
            afficher_aide();
            2
        }
    }
}

/// Mode service : rend la main au SCM (Windows uniquement).
fn run() -> i32 {
    #[cfg(windows)]
    {
        match service::lancer_dispatcher() {
            Ok(()) => 0,
            Err(e) => {
                journal::journaliser_defaut(&format!("run : {e}"));
                eprintln!("{e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("« run » (mode service) n'est disponible que sous Windows.");
        1
    }
}

/// Débogage : lance l'hôte non surveillé au premier plan jusqu'à une entrée clavier.
fn run_console() -> i32 {
    let repertoire = config::repertoire_service();
    let cfg = match config::charger(repertoire) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("configuration invalide : {e}");
            return 2;
        }
    };
    println!(
        "Hôte non surveillé : ID {} — publication au rendez-vous {}",
        cfg.id, cfg.rendezvous
    );
    #[cfg(windows)]
    println!("{}", session0::etat_session_active());

    let poignee = match hote::demarrer(&cfg) {
        Ok(poignee) => poignee,
        Err(e) => {
            eprintln!("{e}");
            return 3;
        }
    };
    println!("Démarré. Appuyez sur Entrée pour arrêter…");
    let mut ligne = String::new();
    let _ = std::io::stdin().read_line(&mut ligne);
    poignee.stop();
    println!("Arrêté.");
    0
}

/// Écrit (ou efface, si vide) le haché du mot de passe non surveillé.
fn set_password(args: &[String]) -> i32 {
    let Some(mot) = args.get(2) else {
        eprintln!("usage : novadesk-svc set-password <mot de passe>");
        return 2;
    };
    let repertoire = config::repertoire_service();
    match config::definir_mot_de_passe(&repertoire, mot) {
        Ok(()) => {
            if mot.is_empty() {
                println!("Mot de passe non surveillé effacé.");
            } else {
                println!("Mot de passe non surveillé enregistré (haché salé, jamais en clair).");
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Diagnostic session 0 ↔ session active ; lance éventuellement un exe dans la
/// session active (validation de la voie session 0 → session utilisateur).
fn probe_session(_args: &[String]) -> i32 {
    #[cfg(windows)]
    {
        println!("{}", session0::etat_session_active());
        if let Some(exe) = _args.get(2) {
            let reste: Vec<String> = _args.iter().skip(3).cloned().collect();
            match session0::lancer_dans_session_active(std::path::Path::new(exe), &reste) {
                Ok(pid) => println!("Processus lancé dans la session active : PID {pid}."),
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            }
        }
        0
    }
    #[cfg(not(windows))]
    {
        eprintln!("« probe-session » n'est disponible que sous Windows.");
        1
    }
}

/// Exécute une sous-commande d'administration (install/uninstall), Windows only.
#[cfg(windows)]
fn sous_commande_admin(action: fn() -> Result<(), String>) -> i32 {
    match action() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(windows)]
fn installer() -> Result<(), String> {
    install::installer()
}

#[cfg(windows)]
fn desinstaller() -> Result<(), String> {
    install::desinstaller()
}

/// Repli hors Windows pour les sous-commandes d'administration.
#[cfg(not(windows))]
fn sous_commande_admin(_action: fn() -> Result<(), String>) -> i32 {
    eprintln!("l'installation du service n'est disponible que sous Windows.");
    1
}

#[cfg(not(windows))]
fn installer() -> Result<(), String> {
    Err("Windows uniquement".to_owned())
}

#[cfg(not(windows))]
fn desinstaller() -> Result<(), String> {
    Err("Windows uniquement".to_owned())
}

/// Affiche l'aide des sous-commandes.
fn afficher_aide() {
    println!(
        "novadesk-svc — service NovaDesk (hôte accès non surveillé, LocalSystem)\n\n\
         USAGE :\n  \
         novadesk-svc <commande>\n\n\
         COMMANDES :\n  \
         install                 Enregistre le service (auto, LocalSystem) + politique SAS  [admin]\n  \
         uninstall               Arrête et supprime le service                              [admin]\n  \
         run                     Mode service (appelé par le SCM ; ne pas lancer à la main)\n  \
         run-console             Lance l'hôte au premier plan (débogage)\n  \
         set-password <mot>      Écrit le haché du mot de passe non surveillé\n  \
         probe-session [exe …]   Diagnostic session 0 ; lance éventuellement exe dans la session active\n  \
         help                    Affiche cette aide\n\n\
         CONFIGURATION : {}",
        config::repertoire_service().join("config.json").display()
    );
}
