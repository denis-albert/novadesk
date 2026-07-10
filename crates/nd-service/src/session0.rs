//! Passerelle **session 0 → session utilisateur active** : ce qui permet (à
//! terme) de capturer le bureau de l'utilisateur interactif depuis un service.
//!
//! # Le problème de la session 0
//!
//! Depuis Windows Vista, les services tournent en **session 0**, isolée des
//! sessions interactives des utilisateurs (session 1, 2, …). Un service
//! LocalSystem **ne voit pas** le bureau de l'utilisateur : appeler la capture
//! d'écran directement depuis le service ne capturerait que le bureau (vide) de
//! la session 0. Pour filmer l'écran réel de l'utilisateur — et y injecter des
//! entrées, y compris sur le **bureau sécurisé** (UAC, écran de verrouillage,
//! Ctrl+Alt+Suppr) — il faut lancer un **processus assistant dans la session
//! interactive active**, sous le jeton de l'utilisateur connecté.
//!
//! Ce module implémente la **mécanique de lancement** (la partie réellement
//! délicate) :
//!
//! 1. [`id_session_active`] — `WTSGetActiveConsoleSessionId` : l'ID de la session
//!    console physiquement active (`None` si aucune, ex. serveur sans écran).
//! 2. [`utilisateur_connecte`] — `WTSQueryUserToken` : obtient le jeton de
//!    l'utilisateur de cette session (réservé à **SYSTEM** — un privilège que ce
//!    service possède, contrairement à l'app en session utilisateur).
//! 3. [`lancer_dans_session_active`] — duplique le jeton en jeton **primaire**,
//!    construit le bloc d'environnement de l'utilisateur puis
//!    `CreateProcessAsUserW` sur le bureau `winsta0\default` : le processus créé
//!    tourne **dans la session de l'utilisateur**, avec son contexte.
//!
//! # Ce qui manque (prochaine étape, honnêtement)
//!
//! Ce module lance un processus **arbitraire** dans la bonne session ; il ne
//! fournit **pas encore** l'exécutable assistant qui, une fois là, réalise la
//! capture (`nd-capture`) et l'injection (`nd-input`) puis renvoie le flux au
//! service par un canal local (tube nommé / socket loopback). Autrement dit : la
//! **voie** session 0 ↔ session active est ouverte et éprouvée ; le **passager**
//! (le binaire assistant de capture/entrées, et son protocole avec le service)
//! reste à écrire. Tant qu'il n'existe pas, l'hôte du service publie son ID,
//! accepte selon mot de passe/ACL et sert une session, mais **la vidéo provient de
//! la session 0** (bureau de service). Pour le bureau sécurisé (SAS), le même
//! assistant devra en outre basculer sur le bureau `Winlogon` (`OpenInputDesktop`
//! / `SetThreadDesktop`).
#![allow(unsafe_code)]

use std::iter;
use std::path::Path;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Valeur renvoyée par `WTSGetActiveConsoleSessionId` quand aucune session
/// console n'est attachée (ex. avant l'ouverture de session, ou serveur headless).
const AUCUNE_SESSION: u32 = 0xFFFF_FFFF;

/// ID de la **session console active** (`None` si aucune n'est attachée).
#[must_use]
pub fn id_session_active() -> Option<u32> {
    // SAFETY : fonction sans argument ni effet de bord (lecture d'un état système).
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == AUCUNE_SESSION {
        None
    } else {
        Some(id)
    }
}

/// Un utilisateur est-il **connecté** dans la session `session` ? Obtient son
/// jeton via `WTSQueryUserToken` (réservé à SYSTEM) puis le referme aussitôt.
#[must_use]
pub fn utilisateur_connecte(session: u32) -> bool {
    let mut jeton = HANDLE::default();
    // SAFETY : `jeton` reçoit une poignée que l'on referme immédiatement en cas
    // de succès ; en cas d'échec (aucun utilisateur, privilège manquant) rien
    // n'est alloué.
    unsafe {
        if WTSQueryUserToken(session, &mut jeton).is_ok() {
            let _ = CloseHandle(jeton);
            true
        } else {
            false
        }
    }
}

/// Diagnostic lisible de l'état session 0 ↔ session active (journalisé au
/// démarrage du service).
#[must_use]
pub fn etat_session_active() -> String {
    match id_session_active() {
        Some(id) => format!(
            "session 0 : session console active = {id}, utilisateur connecté = {}",
            if utilisateur_connecte(id) {
                "oui"
            } else {
                "non"
            }
        ),
        None => {
            "session 0 : aucune session console active (poste verrouillé/headless ?)".to_owned()
        }
    }
}

/// Lance `exe` (avec `args`) **dans la session utilisateur active**, sous le jeton
/// de l'utilisateur connecté, sur le bureau `winsta0\default`. Renvoie le PID créé.
///
/// C'est la voie par laquelle un futur assistant de capture/entrées sera démarré
/// (voir la documentation du module). Nécessite le privilège SYSTEM (le service
/// le possède) et un utilisateur connecté dans la session active.
///
/// # Errors
/// Erreur s'il n'y a pas de session active, pas d'utilisateur connecté, ou si la
/// duplication du jeton / la création du processus échoue.
pub fn lancer_dans_session_active(exe: &Path, args: &[String]) -> Result<u32, String> {
    let session = id_session_active().ok_or_else(|| "aucune session console active".to_owned())?;

    let mut jeton_utilisateur = HANDLE::default();
    // SAFETY : `jeton_utilisateur` reçoit la poignée du jeton de l'utilisateur de
    // la session ; refermée en fin de fonction.
    unsafe { WTSQueryUserToken(session, &mut jeton_utilisateur) }.map_err(|e| {
        format!(
            "aucun jeton utilisateur pour la session {session} (utilisateur non connecté ?) : {e}"
        )
    })?;

    let resultat = lancer_avec_jeton(jeton_utilisateur, exe, args);

    // SAFETY : jeton obtenu de `WTSQueryUserToken`, refermé une seule fois.
    unsafe {
        let _ = CloseHandle(jeton_utilisateur);
    }
    resultat
}

/// Cœur du lancement : duplique le jeton en jeton **primaire**, monte le bloc
/// d'environnement de l'utilisateur et crée le processus. Le jeton primaire et le
/// bloc d'environnement sont libérés avant le retour.
fn lancer_avec_jeton(
    jeton_utilisateur: HANDLE,
    exe: &Path,
    args: &[String],
) -> Result<u32, String> {
    let mut jeton_primaire = HANDLE::default();
    // SAFETY : duplication en jeton primaire assignable à un nouveau processus ;
    // `jeton_primaire` est refermé plus bas.
    unsafe {
        DuplicateTokenEx(
            jeton_utilisateur,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut jeton_primaire,
        )
    }
    .map_err(|e| format!("duplication du jeton utilisateur impossible : {e}"))?;

    // Bloc d'environnement de l'utilisateur (variables de sa session).
    let mut environnement: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY : `environnement` reçoit un bloc alloué par le système, libéré par
    // `DestroyEnvironmentBlock` ci-dessous. `binherit = false` : n'hérite pas de
    // l'environnement (vide) du service.
    let environnement_ok =
        unsafe { CreateEnvironmentBlock(&mut environnement, jeton_primaire, false) }.is_ok();

    // Tampons UTF-16 maintenus vivants pour toute la durée de l'appel (les
    // pointeurs passés au système les référencent).
    let appli: Vec<u16> = utf16z(&exe.to_string_lossy());
    // Ligne de commande mutable (PWSTR) : « "exe" arg1 arg2 », UTF-16 terminée par 0.
    let mut ligne = ligne_de_commande(exe, args);
    // Bureau cible : la station/bureau interactif par défaut de la session.
    let mut bureau: Vec<u16> = utf16z(r"winsta0\default");

    let demarrage = STARTUPINFOW {
        cb: u32::try_from(core::mem::size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR::from_raw(bureau.as_mut_ptr()),
        ..Default::default()
    };
    let mut infos = PROCESS_INFORMATION::default();

    let env_ptr = if environnement_ok {
        Some(environnement.cast_const())
    } else {
        None
    };

    // SAFETY : `jeton_primaire` est un jeton primaire valide ; `appli`, `ligne` et
    // `bureau` vivent jusqu'après l'appel ; `env_ptr` (si présent) pointe le bloc
    // alloué ; `infos` reçoit les poignées du processus créé (refermées aussitôt).
    let cree = unsafe {
        CreateProcessAsUserW(
            jeton_primaire,
            PCWSTR::from_raw(appli.as_ptr()),
            PWSTR::from_raw(ligne.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            env_ptr,
            PCWSTR::null(),
            &demarrage,
            &mut infos,
        )
    };

    // Libérations, quel que soit le résultat.
    if environnement_ok {
        // SAFETY : bloc alloué par `CreateEnvironmentBlock`, libéré une seule fois.
        unsafe {
            let _ = DestroyEnvironmentBlock(environnement);
        }
    }
    // SAFETY : jeton primaire issu de `DuplicateTokenEx`, refermé une seule fois.
    unsafe {
        let _ = CloseHandle(jeton_primaire);
    }

    match cree {
        Ok(()) => {
            let pid = infos.dwProcessId;
            // On n'a pas besoin des poignées du nouveau processus côté service.
            // SAFETY : poignées valides renseignées par `CreateProcessAsUserW`.
            unsafe {
                let _ = CloseHandle(infos.hThread);
                let _ = CloseHandle(infos.hProcess);
            }
            Ok(pid)
        }
        Err(e) => Err(format!(
            "création du processus dans la session active impossible : {e}"
        )),
    }
}

/// Construit la ligne de commande `"exe" arg1 arg2` en tampon UTF-16 terminé par 0
/// (modifiable, comme l'exige `CreateProcessAsUserW`).
fn ligne_de_commande(exe: &Path, args: &[String]) -> Vec<u16> {
    let mut ligne = format!("\"{}\"", exe.to_string_lossy());
    for arg in args {
        ligne.push(' ');
        ligne.push_str(arg);
    }
    utf16z(&ligne)
}

/// Encode une chaîne en tampon UTF-16 terminé par un zéro (API `*W`).
fn utf16z(texte: &str) -> Vec<u16> {
    texte.encode_utf16().chain(iter::once(0)).collect()
}
