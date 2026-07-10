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
//! | `helper <chemin_pipe>`       | Mode **assistant** dans la session active (lancé par le service ; capture + entrées). |
//!
//! # Ce qui marche vs ce qui manque
//!
//! Le service **vit** en LocalSystem, lit sa **configuration machine** sous
//! `C:\ProgramData\NovaDesk` (voir [`config`]), **publie** son ID au rendez-vous,
//! **admet** selon mot de passe permanent / appareils de confiance / ACL (dans le
//! canal chiffré Noise) et **sert** une session — le tout sans surveillance. La
//! **capture du bureau de l'utilisateur interactif depuis la session 0** passe par
//! l'**assistant** (`helper`) : lancé dans la session active ([`session0`]), il
//! capture le bureau ([`assistant`]) et renvoie les trames au service par un tube
//! nommé ([`tube`], protocole [`canal`]) ; le service les expose au moteur de
//! session via [`pont`] et transmet les entrées en sens inverse. Le **raccordement
//! final** des trames de l'assistant à l'encodeur de `nd-core` est en place : le
//! service branche les fabriques de capteur/injecteur (adossées au
//! [`pont::GestionnairePont`]) sur
//! [`nd_core::UnattendedHost::start_with_admission_enrichie_fabriques`] (voir
//! [`hote::demarrer`]). La bascule sur le **bureau sécurisé** ([`bureau`]) exige de
//! lancer l'assistant en SYSTEM ([`session0::lancer_systeme_dans_session_active`]) ;
//! sa **vérification live sous UAC** reste à faire (aucun droit admin ici).

// Modules indépendants de la plateforme (testables partout).
mod canal;
mod config;
mod hote;
mod journal;

// Modules spécifiques à Windows (SCM, registre, session 0, assistant, tube).
#[cfg(windows)]
mod assistant;
#[cfg(windows)]
mod bureau;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod pont;
#[cfg(windows)]
mod sas;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod session0;
#[cfg(windows)]
mod tube;

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
        "helper" => helper(&args),
        "probe-assistant" => probe_assistant(&args),
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

/// Mode **assistant** : `novadesk-svc helper <chemin_pipe>`. Lancé par le service
/// dans la session active (voir [`assistant`] et [`session0`]) ; capture le bureau
/// interactif et injecte les entrées via le tube nommé du service. Ne pas lancer à
/// la main (sauf essai manuel : cf. la doc de vérification de bout en bout).
fn helper(_args: &[String]) -> i32 {
    #[cfg(windows)]
    {
        let Some(chemin) = _args.get(2) else {
            eprintln!("usage : novadesk-svc helper <chemin_pipe>");
            return 2;
        };
        match assistant::executer(chemin) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("« helper » (mode assistant) n'est disponible que sous Windows.");
        1
    }
}

/// Essai **de bout en bout** du pont assistant : lance l'assistant, tire quelques
/// trames par le tube et affiche un résumé. Exerce toute la chaîne
/// [`pont`] → [`tube`] → [`assistant`] → [`canal`].
///
/// * `probe-assistant --local`  : lance l'assistant comme **processus enfant dans
///   la session courante** (essai depuis une session interactive ordinaire, sans
///   service ni SYSTEM — couvre le bureau `Default`) ;
/// * `probe-assistant --systeme`: lance en **SYSTEM dans la session active** (bureau
///   sécurisé ; nécessite d'être SYSTEM, p. ex. via le service ou `psexec -s`) ;
/// * `probe-assistant`          : lance sous le **jeton utilisateur** de la session
///   active (nécessite SYSTEM pour `WTSQueryUserToken`).
fn probe_assistant(_args: &[String]) -> i32 {
    #[cfg(windows)]
    {
        use nd_capture::{CaptureConfig, ScreenCapturer};
        use nd_input::InputInjector;
        use nd_proto::MonitorId;

        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("exécutable courant introuvable : {e}");
                return 1;
            }
        };
        let demarrage = match _args.get(2).map(String::as_str).unwrap_or("") {
            "--local" => pont::PontAssistant::demarrer_local(&exe),
            "--systeme" => pont::PontAssistant::demarrer(&exe, pont::ModeLancement::Systeme),
            _ => pont::PontAssistant::demarrer(&exe, pont::ModeLancement::Utilisateur),
        };
        let mut pont = match demarrage {
            Ok(p) => p,
            Err(e) => {
                eprintln!("pont assistant non démarré : {e}");
                return 1;
            }
        };
        println!(
            "Assistant lancé (PID {}), vivant = {}.",
            pont.pid(),
            pont.est_vivant()
        );

        // Sens entrées service → assistant (démontré par un release_all inoffensif).
        pont.injecteur().release_all();

        let Some(mut capteur) = pont.capteur() else {
            eprintln!("capteur déjà consommé");
            return 1;
        };
        if let Err(e) = capteur.start(CaptureConfig {
            monitor: MonitorId(0),
            target_fps: 30,
            capture_cursor: true,
        }) {
            eprintln!("démarrage capture : {e}");
        }

        let debut = std::time::Instant::now();
        let mut total = 0usize;
        let mut avec_image = 0usize;
        while debut.elapsed() < std::time::Duration::from_secs(3) && total < 120 {
            match capteur.next_frame() {
                Ok(trame) => {
                    total += 1;
                    if trame.image.is_some() {
                        avec_image += 1;
                        if avec_image == 1 {
                            println!("Première trame réelle : {}x{}.", trame.width, trame.height);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("capture interrompue : {e}");
                    break;
                }
            }
        }
        println!("Trames reçues : {total} (dont {avec_image} porteuses d'image).");
        println!("Moniteurs annoncés : {}.", pont.moniteurs().len());
        if let Some(err) = pont.derniere_erreur() {
            println!("Dernière erreur assistant : {err}");
        }
        capteur.stop();
        pont.arreter();
        0
    }
    #[cfg(not(windows))]
    {
        eprintln!("« probe-assistant » n'est disponible que sous Windows.");
        1
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
         helper <chemin_pipe>    Mode assistant (lancé par le service dans la session active ; ne pas lancer à la main)\n  \
         probe-assistant [--local|--systeme]  Essai de bout en bout du pont assistant (capture → tube → capteur)\n  \
         help                    Affiche cette aide\n\n\
         CONFIGURATION : {}",
        config::repertoire_service().join("config.json").display()
    );
}
