//! Démarrage de l'**hôte accès non surveillé** ([`nd_core::UnattendedHost`]) à
//! partir de la configuration machine ([`crate::config::ConfigService`]).
//!
//! # Admission (sans approbateur interactif)
//!
//! Le service tourne en session 0 : **aucun utilisateur** ne peut approuver une
//! demande manuellement. L'admission tranche donc **dans le canal chiffré
//! Noise** :
//!
//! 1. appareil de confiance (**confiance ∪ liste blanche d'admission**) → admis ;
//! 2. sinon, mot de passe permanent prouvé (haché salé) → admis ;
//! 3. sinon (aucune preuve) → **refus par défaut** : le crochet manuel de repli
//!    renvoie toujours `false` (pas de dialogue possible en session 0).
//!
//! Le clair du mot de passe ne circule que dans le canal Noise : il n'est comparé
//! qu'au haché persisté, jamais conservé ni journalisé.
//!
//! # Capture / injection du **vrai bureau** (assistant)
//!
//! Le service passe désormais à
//! [`UnattendedHost::start_with_admission_enrichie_fabriques`] en fournissant deux
//! **fabriques** (Windows) adossées à un [`GestionnairePont`](crate::pont::GestionnairePont) :
//! à chaque époque servie, la boucle hôte de `nd-core` obtient un
//! `CapteurAssistant` / `InjecteurAssistant` — un assistant lancé dans la
//! **session active** — au lieu d'un capteur/injecteur système qui, en session 0,
//! ne verrait qu'un bureau vide. Hors Windows (repli de compilation), aucune
//! fabrique n'est branchée et `nd-core` garde ses briques système par défaut.

use std::collections::HashSet;

use nd_core::{
    DemandeAdmissionManuelle, FabriqueCapteur, FabriqueInjecteur, UnattendedHost,
    UnattendedHostHandle,
};
use nd_features::PermissionSet;
use nd_proto::NovaId;

use crate::config::ConfigService;

/// Démarre l'hôte non surveillé selon `cfg` et renvoie sa poignée (arrêt via
/// [`UnattendedHostHandle::stop`]).
///
/// # Errors
/// Erreur si le thread de service ne peut être créé (voir
/// [`UnattendedHost::start_with_admission_enrichie_fabriques`]).
pub fn demarrer(cfg: &ConfigService) -> Result<UnattendedHostHandle, String> {
    // Confiance = appareils de confiance ∪ liste blanche d'admission (union),
    // capturée dans un ensemble pour une recherche O(1) dans la closure.
    let confiance: HashSet<u64> = cfg
        .appareils_de_confiance
        .iter()
        .chain(cfg.admission_autorisee.iter())
        .copied()
        .collect();

    // Copie du haché (l'appelant garde sa `ConfigService`) : la closure la possède.
    let mot_de_passe = cfg.mot_de_passe.clone();

    let verif_mdp = move |essai: &str| {
        mot_de_passe
            .as_ref()
            .is_some_and(|hache| hache.verifier(essai))
    };
    let est_de_confiance = move |pair: NovaId| confiance.contains(&pair.as_u64());

    // Fabriques capteur/injecteur adossées à l'assistant (Windows) : la boucle
    // hôte capture ainsi le **vrai bureau** de l'utilisateur (session active) et
    // injecte dans **sa** session, au lieu du bureau vide de la session 0.
    // Ailleurs, pas de fabrique → `nd-core` garde ses briques système par défaut.
    let (capturer_factory, injector_factory) = fabriques_assistant();

    UnattendedHost::start_with_admission_enrichie_fabriques(
        cfg.id,
        cfg.rendezvous,
        cfg.stun.clone(),
        cfg.identite.clone(),
        cfg.permissions,
        // Repli manuel : aucun approbateur en session 0 → refus par défaut. La
        // demande enrichie porte aussi le nom d'affichage / profil demandé, sans
        // usage ici (pas de dialogue).
        |_demande: &DemandeAdmissionManuelle| false,
        verif_mdp,
        est_de_confiance,
        // Pas d'invitations éphémères côté service (config machine) : aucune n'est
        // honorée.
        |_pair: NovaId, _code: &str| -> Option<PermissionSet> { None },
        // Enregistrement **nu** au rendez-vous (registre de dev / LAN), comme le
        // démarrage historique du service.
        None,
        capturer_factory,
        injector_factory,
    )
    .map_err(|e| format!("démarrage de l'hôte non surveillé impossible : {e}"))
}

/// Construit les fabriques capteur/injecteur adossées à un
/// [`GestionnairePont`](crate::pont::GestionnairePont) partagé (un assistant neuf
/// par époque servie).
///
/// Introuvable exécutable courant ⇒ pas de fabrique (repli sur les briques
/// système de `nd-core`).
#[cfg(windows)]
fn fabriques_assistant() -> (Option<FabriqueCapteur>, Option<FabriqueInjecteur>) {
    use std::sync::{Arc, Mutex};

    use nd_proto::NdError;

    use crate::pont::{GestionnairePont, ModeLancement};

    // Exécutable courant = binaire du service, relancé en `helper <pipe>` dans la
    // session active. Introuvable ⇒ pas de fabrique.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return (None, None),
    };
    // Le service tourne en SYSTEM : lancement SYSTEM dans la session active (couvre
    // le **bureau sécurisé** — UAC / verrouillage / Winlogon).
    let gestionnaire = Arc::new(Mutex::new(GestionnairePont::new(
        exe,
        ModeLancement::Systeme,
    )));

    let g_capteur = Arc::clone(&gestionnaire);
    let capturer_factory: FabriqueCapteur = Arc::new(move || {
        let mut g = g_capteur
            .lock()
            .map_err(|_| NdError::Capture("gestionnaire de pont empoisonné".to_owned()))?;
        let capteur = g.capteur().map_err(NdError::Capture)?;
        Ok(Box::new(capteur) as Box<dyn nd_capture::ScreenCapturer>)
    });

    let g_injecteur = Arc::clone(&gestionnaire);
    let injector_factory: FabriqueInjecteur = Arc::new(move || {
        let mut g = g_injecteur
            .lock()
            .map_err(|_| NdError::Input("gestionnaire de pont empoisonné".to_owned()))?;
        let injecteur = g.injecteur().map_err(NdError::Input)?;
        Ok(Box::new(injecteur) as Box<dyn nd_input::InputInjector>)
    });

    (Some(capturer_factory), Some(injector_factory))
}

/// Repli hors Windows : aucune fabrique (le service n'est fonctionnel que sous
/// Windows ; `nd-core` garde ses briques système).
#[cfg(not(windows))]
fn fabriques_assistant() -> (Option<FabriqueCapteur>, Option<FabriqueInjecteur>) {
    (None, None)
}
