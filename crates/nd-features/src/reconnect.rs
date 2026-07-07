//! Politique de reconnexion automatique : backoff exponentiel plafonné,
//! jitter déterministe (dérivé du numéro de tentative, jamais de l'horloge)
//! et petite machine à états pilotant la boucle de reconnexion.
//!
//! Le module ne dort jamais lui-même : il calcule des délais ([`Duration`])
//! que l'appelant applique avec son propre minuteur. Cela rend tout le
//! comportement testable sans horloge (voir plan 04, §reconnexion).
//!
//! # Contrat d'intégration (orchestrateur `nd-core`)
//!
//! La boucle de session de `nd-core` pilote un [`ReconnectController`] par
//! **événements**, sans connaître la politique de backoff :
//! - à la perte du lien (fermeture QUIC, timeout keepalive) →
//!   [`ReconnectController::on_disconnect`] ;
//! - avant chaque tentative → [`ReconnectController::next_delay`], qui rend
//!   le délai à observer (avec le minuteur de l'appelant) ou `None` quand la
//!   politique a épuisé ses tentatives (informer l'UI, arrêter la boucle) ;
//! - dès qu'une connexion aboutit (ou que l'utilisateur annule) →
//!   [`ReconnectController::reset`].
//!
//! Les signaux redondants sont inoffensifs : un second `on_disconnect`
//! pendant une reconnexion en cours ne remet pas le backoff à zéro.

use std::time::Duration;

/// Paramètres du backoff exponentiel entre deux tentatives de reconnexion.
///
/// Le délai avant la tentative `n` (1-indexée) vaut
/// `base_delay_ms × multiplier^(n-1)`, plafonné à `max_delay_ms`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectPolicy {
    /// Délai avant la première tentative, en millisecondes.
    pub base_delay_ms: u64,
    /// Plafond du délai entre deux tentatives, en millisecondes.
    pub max_delay_ms: u64,
    /// Facteur multiplicatif entre deux tentatives (≥ 1 ; toute valeur
    /// inférieure ou non finie est traitée comme 1, backoff constant).
    pub multiplier: f64,
    /// Nombre maximal de tentatives, ou `None` pour persévérer sans limite
    /// (jusqu'à annulation explicite par l'utilisateur).
    pub max_attempts: Option<u32>,
    /// Si vrai, applique un jitter déterministe : le délai est multiplié par
    /// un facteur dans `[0,5 ; 1,0[` dérivé du numéro de tentative.
    pub jitter: bool,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        // Défaut : 500 ms doublés à chaque échec, plafonnés à 30 s,
        // sans limite de tentatives (l'utilisateur peut toujours annuler).
        ReconnectPolicy {
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            multiplier: 2.0,
            max_attempts: None,
            jitter: true,
        }
    }
}

impl ReconnectPolicy {
    /// Délai à observer avant la tentative `attempt` (1-indexée : la première
    /// tentative de reconnexion porte le numéro 1).
    ///
    /// Renvoie `None` quand `max_attempts` est atteint : l'appelant doit
    /// abandonner. Un `attempt` de 0 est traité comme 1.
    #[must_use]
    pub fn next_delay(&self, attempt: u32) -> Option<Duration> {
        let attempt = attempt.max(1);
        if let Some(max) = self.max_attempts {
            if attempt > max {
                return None;
            }
        }

        // Multiplicateur assaini : jamais de délai qui rétrécit ni de NaN.
        let facteur = if self.multiplier.is_finite() && self.multiplier >= 1.0 {
            self.multiplier
        } else {
            1.0
        };
        // Un exposant hors i32 donnerait de toute façon +inf, plafonné ensuite.
        let exposant = i32::try_from(attempt - 1).unwrap_or(i32::MAX);
        let brut = (self.base_delay_ms as f64) * facteur.powi(exposant);
        let plafonne = brut.min(self.max_delay_ms as f64);
        let final_ms = if self.jitter {
            plafonne * jitter_factor(attempt)
        } else {
            plafonne
        };
        Some(Duration::from_millis(final_ms.round() as u64))
    }
}

/// Facteur de jitter déterministe dans `[0,5 ; 1,0[`, dérivé uniquement du
/// numéro de tentative (mélangeur SplitMix64) : reproductible en test, et
/// suffisant pour désynchroniser des clients qui retomberaient en même temps.
fn jitter_factor(attempt: u32) -> f64 {
    let mut z = u64::from(attempt).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 53 bits de poids fort → fraction uniforme dans [0 ; 1[.
    let fraction = (z >> 11) as f64 / (1u64 << 53) as f64;
    0.5 + fraction / 2.0
}

/// État courant de la boucle de reconnexion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReconnectState {
    /// Connecté (ou pas encore déconnecté) : aucune reconnexion en cours.
    #[default]
    Idle,
    /// Déconnecté : on attend le délai avant la tentative `until_attempt`
    /// (1-indexée).
    Waiting {
        /// Numéro de la tentative que l'on s'apprête à faire.
        until_attempt: u32,
    },
    /// `max_attempts` atteint : la reconnexion automatique a abandonné.
    GaveUp,
}

impl ReconnectState {
    /// Signale une déconnexion : (re)démarre la séquence à la tentative 1.
    ///
    /// Renvoie le délai à observer avant cette première tentative, ou `None`
    /// si la politique interdit toute tentative (`max_attempts == Some(0)`),
    /// auquel cas l'état passe directement à [`ReconnectState::GaveUp`].
    pub fn on_disconnected(&mut self, policy: &ReconnectPolicy) -> Option<Duration> {
        self.programme(policy, 1)
    }

    /// Signale l'échec de la tentative en cours : programme la suivante.
    ///
    /// Renvoie le délai avant la prochaine tentative, ou `None` quand
    /// `max_attempts` est atteint (l'état passe à [`ReconnectState::GaveUp`]).
    /// Sans effet (renvoie `None`) hors de l'état `Waiting`.
    pub fn on_attempt_failed(&mut self, policy: &ReconnectPolicy) -> Option<Duration> {
        match *self {
            ReconnectState::Waiting { until_attempt } => self.programme(policy, until_attempt + 1),
            ReconnectState::Idle | ReconnectState::GaveUp => None,
        }
    }

    /// Signale une connexion (ré)établie : retour au repos, compteur oublié.
    pub fn on_connected(&mut self) {
        *self = ReconnectState::Idle;
    }

    /// La reconnexion automatique a-t-elle abandonné ?
    #[must_use]
    pub fn has_given_up(&self) -> bool {
        matches!(self, ReconnectState::GaveUp)
    }

    /// Passe en attente de la tentative `attempt`, ou abandonne si la
    /// politique n'autorise plus de tentative.
    fn programme(&mut self, policy: &ReconnectPolicy, attempt: u32) -> Option<Duration> {
        match policy.next_delay(attempt) {
            Some(delai) => {
                *self = ReconnectState::Waiting {
                    until_attempt: attempt,
                };
                Some(delai)
            }
            None => {
                *self = ReconnectState::GaveUp;
                None
            }
        }
    }
}

/// Contrôleur de reconnexion orienté événements : enveloppe
/// [`ReconnectPolicy`] + [`ReconnectState`] derrière l'API que la boucle de
/// session de `nd-core` consomme (voir le contrat d'intégration en tête de
/// module). Ne dort jamais : rend des délais, l'appelant tient le minuteur.
///
/// ```
/// use nd_features::ReconnectController;
///
/// let mut ctl = ReconnectController::default();
/// ctl.on_disconnect(); // le lien vient de tomber
/// let mut tentatives = 0;
/// while let Some(_delai) = ctl.next_delay() {
///     // ici : attendre `_delai`, puis tenter la reconnexion…
///     tentatives += 1;
///     if tentatives == 3 {
///         ctl.reset(); // troisième tentative : la connexion a abouti
///         break;
///     }
/// }
/// assert!(!ctl.is_reconnecting());
/// assert!(!ctl.has_given_up());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ReconnectController {
    policy: ReconnectPolicy,
    etat: ReconnectState,
    /// Délai calculé par le dernier événement et pas encore remis à
    /// l'appelant par [`ReconnectController::next_delay`].
    delai_arme: Option<Duration>,
}

impl ReconnectController {
    /// Contrôleur au repos, avec la politique donnée.
    #[must_use]
    pub fn new(policy: ReconnectPolicy) -> Self {
        ReconnectController {
            policy,
            etat: ReconnectState::Idle,
            delai_arme: None,
        }
    }

    /// Signale la perte du lien : arme la première tentative.
    ///
    /// Idempotent : si une reconnexion est déjà en cours (ou déjà abandonnée),
    /// les signaux redondants — plusieurs canaux constatant la même coupure —
    /// ne remettent pas la séquence de backoff à zéro.
    pub fn on_disconnect(&mut self) {
        if self.etat == ReconnectState::Idle {
            self.delai_arme = self.etat.on_disconnected(&self.policy);
        }
    }

    /// Délai à observer avant la **prochaine** tentative de reconnexion.
    ///
    /// Premier appel après [`ReconnectController::on_disconnect`] : délai de
    /// la tentative 1. Chaque appel suivant acte l'échec de la tentative
    /// précédente et programme la suivante. Rend `None` quand la politique a
    /// épuisé `max_attempts` ([`ReconnectController::has_given_up`] devient
    /// vrai), ou si aucune coupure n'a été signalée.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if let Some(delai) = self.delai_arme.take() {
            return Some(delai);
        }
        self.etat.on_attempt_failed(&self.policy)
    }

    /// Signale une connexion (r)établie ou un abandon utilisateur : retour au
    /// repos, compteur de tentatives oublié.
    pub fn reset(&mut self) {
        self.etat.on_connected();
        self.delai_arme = None;
    }

    /// État courant de la machine à états sous-jacente.
    #[must_use]
    pub fn state(&self) -> ReconnectState {
        self.etat
    }

    /// Numéro (1-indexé) de la tentative en cours de programmation, ou `None`
    /// au repos / après abandon.
    #[must_use]
    pub fn attempt(&self) -> Option<u32> {
        match self.etat {
            ReconnectState::Waiting { until_attempt } => Some(until_attempt),
            ReconnectState::Idle | ReconnectState::GaveUp => None,
        }
    }

    /// Une reconnexion automatique est-elle en cours ?
    #[must_use]
    pub fn is_reconnecting(&self) -> bool {
        matches!(self.etat, ReconnectState::Waiting { .. })
    }

    /// La reconnexion automatique a-t-elle abandonné (`max_attempts` atteint) ?
    #[must_use]
    pub fn has_given_up(&self) -> bool {
        self.etat.has_given_up()
    }

    /// Politique de backoff appliquée.
    #[must_use]
    pub fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }
}

impl Default for ReconnectController {
    fn default() -> Self {
        ReconnectController::new(ReconnectPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Politique de référence sans jitter, pour des délais exacts.
    fn politique_nette() -> ReconnectPolicy {
        ReconnectPolicy {
            base_delay_ms: 100,
            max_delay_ms: 10_000,
            multiplier: 2.0,
            max_attempts: None,
            jitter: false,
        }
    }

    #[test]
    fn backoff_progresse_exponentiellement() {
        let p = politique_nette();
        let attendus = [100u64, 200, 400, 800, 1600];
        for (i, attendu) in attendus.iter().enumerate() {
            let tentative = u32::try_from(i).unwrap() + 1;
            assert_eq!(
                p.next_delay(tentative),
                Some(Duration::from_millis(*attendu)),
                "tentative {tentative}"
            );
        }
    }

    #[test]
    fn delai_plafonne_a_max_delay() {
        let p = ReconnectPolicy {
            max_delay_ms: 500,
            ..politique_nette()
        };
        assert_eq!(p.next_delay(3), Some(Duration::from_millis(400)));
        assert_eq!(p.next_delay(4), Some(Duration::from_millis(500)));
        // Même très loin (exposant énorme), le plafond tient sans déborder.
        assert_eq!(p.next_delay(1_000_000), Some(Duration::from_millis(500)));
    }

    #[test]
    fn abandon_apres_max_attempts() {
        let p = ReconnectPolicy {
            max_attempts: Some(3),
            ..politique_nette()
        };
        assert!(p.next_delay(1).is_some());
        assert!(p.next_delay(3).is_some());
        assert_eq!(p.next_delay(4), None);
        assert_eq!(p.next_delay(u32::MAX), None);
    }

    #[test]
    fn tentative_zero_traitee_comme_premiere() {
        let p = politique_nette();
        assert_eq!(p.next_delay(0), p.next_delay(1));
    }

    #[test]
    fn multiplicateur_invalide_assaini() {
        // Multiplicateur < 1 ou NaN → backoff constant, jamais de panique.
        for mult in [0.5, -3.0, f64::NAN, f64::INFINITY] {
            let p = ReconnectPolicy {
                multiplier: mult,
                ..politique_nette()
            };
            let d = p.next_delay(5).unwrap();
            assert!(
                d <= Duration::from_millis(p.max_delay_ms),
                "multiplicateur {mult} : délai {d:?} hors plafond"
            );
        }
    }

    #[test]
    fn jitter_borne_et_deterministe() {
        let p = ReconnectPolicy {
            jitter: true,
            ..politique_nette()
        };
        let sans_jitter = politique_nette();
        let mut distincts = std::collections::BTreeSet::new();
        for tentative in 1..=50 {
            let plein = sans_jitter.next_delay(tentative).unwrap();
            let brouille = p.next_delay(tentative).unwrap();
            // Borné : dans [plein/2 ; plein] (arrondi à la milliseconde près).
            assert!(
                brouille >= plein / 2 && brouille <= plein,
                "tentative {tentative} : {brouille:?} hors de [{:?} ; {plein:?}]",
                plein / 2
            );
            // Déterministe : même tentative → même délai.
            assert_eq!(brouille, p.next_delay(tentative).unwrap());
            distincts.insert(brouille);
        }
        // Le jitter fait effectivement varier les délais.
        assert!(distincts.len() > 1);
    }

    #[test]
    fn machine_a_etats_cycle_nominal() {
        let p = politique_nette();
        let mut etat = ReconnectState::default();
        assert_eq!(etat, ReconnectState::Idle);

        assert_eq!(etat.on_disconnected(&p), Some(Duration::from_millis(100)));
        assert_eq!(etat, ReconnectState::Waiting { until_attempt: 1 });

        assert_eq!(etat.on_attempt_failed(&p), Some(Duration::from_millis(200)));
        assert_eq!(etat, ReconnectState::Waiting { until_attempt: 2 });

        etat.on_connected();
        assert_eq!(etat, ReconnectState::Idle);

        // Nouvelle déconnexion : la séquence repart de la tentative 1.
        assert_eq!(etat.on_disconnected(&p), Some(Duration::from_millis(100)));
        assert_eq!(etat, ReconnectState::Waiting { until_attempt: 1 });
    }

    #[test]
    fn machine_a_etats_abandonne_apres_max() {
        let p = ReconnectPolicy {
            max_attempts: Some(2),
            ..politique_nette()
        };
        let mut etat = ReconnectState::Idle;
        assert!(etat.on_disconnected(&p).is_some()); // tentative 1
        assert!(etat.on_attempt_failed(&p).is_some()); // tentative 2
        assert_eq!(etat.on_attempt_failed(&p), None); // tentative 3 refusée
        assert!(etat.has_given_up());
        // GaveUp est stable tant qu'on ne se reconnecte pas.
        assert_eq!(etat.on_attempt_failed(&p), None);
        assert!(etat.has_given_up());
        etat.on_connected();
        assert_eq!(etat, ReconnectState::Idle);
    }

    #[test]
    fn zero_tentative_autorisee_abandonne_immediatement() {
        let p = ReconnectPolicy {
            max_attempts: Some(0),
            ..politique_nette()
        };
        let mut etat = ReconnectState::Idle;
        assert_eq!(etat.on_disconnected(&p), None);
        assert!(etat.has_given_up());
    }

    #[test]
    fn controleur_cycle_nominal() {
        let mut ctl = ReconnectController::new(politique_nette());
        assert!(!ctl.is_reconnecting());
        assert_eq!(ctl.attempt(), None);
        // Sans coupure signalée, il n'y a rien à programmer.
        assert_eq!(ctl.next_delay(), None);

        ctl.on_disconnect();
        assert!(ctl.is_reconnecting());
        assert_eq!(ctl.attempt(), Some(1));
        // Suite exacte de la politique : 100, 200, 400 ms…
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(100)));
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(200)));
        assert_eq!(ctl.attempt(), Some(2));
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(400)));

        // La connexion aboutit : retour au repos, la séquence repartira de 1.
        ctl.reset();
        assert!(!ctl.is_reconnecting());
        ctl.on_disconnect();
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn controleur_on_disconnect_idempotent() {
        let mut ctl = ReconnectController::new(politique_nette());
        ctl.on_disconnect();
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(100)));
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(200)));
        // Signal redondant en pleine reconnexion : le backoff ne repart pas.
        ctl.on_disconnect();
        assert_eq!(ctl.attempt(), Some(2));
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(400)));
    }

    #[test]
    fn controleur_abandonne_puis_reset_reamorce() {
        let mut ctl = ReconnectController::new(ReconnectPolicy {
            max_attempts: Some(2),
            ..politique_nette()
        });
        ctl.on_disconnect();
        assert!(ctl.next_delay().is_some()); // tentative 1
        assert!(ctl.next_delay().is_some()); // tentative 2
        assert_eq!(ctl.next_delay(), None); // épuisé
        assert!(ctl.has_given_up());
        assert_eq!(ctl.attempt(), None);
        // Abandonné : un nouveau signal de coupure ne relance rien tout seul…
        ctl.on_disconnect();
        assert!(ctl.has_given_up());
        // … c'est reset() (reconnexion manuelle réussie) qui réamorce.
        ctl.reset();
        ctl.on_disconnect();
        assert_eq!(ctl.next_delay(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn controleur_zero_tentative_donne_abandon_immediat() {
        let mut ctl = ReconnectController::new(ReconnectPolicy {
            max_attempts: Some(0),
            ..politique_nette()
        });
        ctl.on_disconnect();
        assert_eq!(ctl.next_delay(), None);
        assert!(ctl.has_given_up());
    }
}
