//! Démarrage de l'**hôte accès non surveillé** ([`nd_core::UnattendedHost`]) à
//! partir de la configuration machine ([`crate::config::ConfigService`]).
//!
//! # Admission (sans approbateur interactif)
//!
//! Le service tourne en session 0 : **aucun utilisateur** ne peut approuver une
//! demande manuellement. L'admission utilise donc
//! [`UnattendedHost::start_with_admission`], qui tranche **dans le canal chiffré
//! Noise** :
//!
//! 1. appareil de confiance (**confiance ∪ liste blanche d'admission**) → admis ;
//! 2. sinon, mot de passe permanent prouvé (haché salé) → admis ;
//! 3. sinon (aucune preuve) → **refus par défaut** : le crochet manuel de repli
//!    renvoie toujours `false` (pas de dialogue possible en session 0).
//!
//! Le clair du mot de passe ne circule que dans le canal Noise : il n'est comparé
//! qu'au haché persisté, jamais conservé ni journalisé.

use std::collections::HashSet;

use nd_core::{UnattendedHost, UnattendedHostHandle};
use nd_proto::NovaId;

use crate::config::ConfigService;

/// Démarre l'hôte non surveillé selon `cfg` et renvoie sa poignée (arrêt via
/// [`UnattendedHostHandle::stop`]).
///
/// # Errors
/// Erreur si le thread de service ne peut être créé (voir
/// [`UnattendedHost::start_with_admission`]).
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

    UnattendedHost::start_with_admission(
        cfg.id,
        cfg.rendezvous,
        cfg.stun.clone(),
        cfg.identite.clone(),
        cfg.permissions,
        // Repli manuel : aucun approbateur en session 0 → refus par défaut.
        |_pair| false,
        verif_mdp,
        est_de_confiance,
    )
    .map_err(|e| format!("démarrage de l'hôte non surveillé impossible : {e}"))
}
