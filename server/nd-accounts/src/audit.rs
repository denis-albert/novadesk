//! Journal d'audit et enregistrements de session NovaDesk — **en mémoire**
//! (plan 11, conformité : RGPD, journal d'accès).
//!
//! [`AuditLog`] consigne des [`AuditEvent`] horodatés (création de compte,
//! connexions, 2FA, sessions, refus de permission) dans un tampon borné :
//! au-delà de la capacité, les événements les plus anciens sont éliminés
//! (rotation). Chaque entrée se sérialise en une ligne texte lisible
//! (`Display`), prête pour un futur export fichier/SIEM.
//!
//! [`SessionRecordStore`] tient le registre des sessions de bureau à distance
//! actives (comptes contrôleur/contrôlé, permissions, début) et calcule la
//! durée à la clôture. Comme le reste du service : aucune base de données,
//! tout vit dans un `Arc<Mutex<...>>` clonable et thread-safe.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::unix_maintenant;

/// Capacité par défaut du journal d'audit (nombre d'événements conservés).
pub const CAPACITE_DEFAUT: usize = 10_000;

// ---------------------------------------------------------------------------
// Événements
// ---------------------------------------------------------------------------

/// Événement d'audit métier (sans horodatage — voir [`AuditEntry`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    /// Un compte a été créé (`register`).
    AccountCreated {
        /// E-mail du compte créé.
        email: String,
    },
    /// Connexion réussie (`login` ou `login_2fa`).
    LoginSuccess {
        /// E-mail du compte connecté.
        email: String,
    },
    /// Tentative de connexion refusée (mot de passe ou code TOTP incorrect).
    LoginFailure {
        /// E-mail présenté lors de la tentative.
        email: String,
    },
    /// La 2FA TOTP a été activée sur le compte.
    TwoFactorEnabled {
        /// E-mail du compte protégé.
        email: String,
    },
    /// Une session de bureau à distance a démarré.
    SessionStarted {
        /// E-mail du compte contrôleur (côté pilote).
        email: String,
        /// Identifiant de corrélation de la session.
        session_id: String,
        /// Identifiant du pair contrôlé (poste distant).
        peer_id: String,
    },
    /// Une session de bureau à distance s'est terminée.
    SessionEnded {
        /// Identifiant de corrélation de la session.
        session_id: String,
        /// Durée de la session, en secondes.
        duration_secs: u64,
    },
    /// Une action a été refusée par le contrôle d'accès.
    PermissionDenied {
        /// E-mail du compte demandeur.
        email: String,
        /// Action refusée (libellé libre, p. ex. `transfert_fichiers`).
        action: String,
    },
}

impl AuditEvent {
    /// E-mail du compte concerné, si l'événement en porte un
    /// (`SessionEnded` n'en a pas : la corrélation passe par `session_id`).
    #[must_use]
    pub fn email(&self) -> Option<&str> {
        match self {
            AuditEvent::AccountCreated { email }
            | AuditEvent::LoginSuccess { email }
            | AuditEvent::LoginFailure { email }
            | AuditEvent::TwoFactorEnabled { email }
            | AuditEvent::SessionStarted { email, .. }
            | AuditEvent::PermissionDenied { email, .. } => Some(email),
            AuditEvent::SessionEnded { .. } => None,
        }
    }
}

impl fmt::Display for AuditEvent {
    /// Forme texte lisible `etiquette champ=valeur ...` (pour l'export ligne à ligne).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuditEvent::AccountCreated { email } => write!(f, "compte_cree email={email}"),
            AuditEvent::LoginSuccess { email } => write!(f, "connexion_reussie email={email}"),
            AuditEvent::LoginFailure { email } => write!(f, "connexion_echouee email={email}"),
            AuditEvent::TwoFactorEnabled { email } => write!(f, "2fa_activee email={email}"),
            AuditEvent::SessionStarted {
                email,
                session_id,
                peer_id,
            } => write!(
                f,
                "session_demarree email={email} session={session_id} pair={peer_id}"
            ),
            AuditEvent::SessionEnded {
                session_id,
                duration_secs,
            } => write!(
                f,
                "session_terminee session={session_id} duree_s={duration_secs}"
            ),
            AuditEvent::PermissionDenied { email, action } => {
                write!(f, "permission_refusee email={email} action={action}")
            }
        }
    }
}

/// Entrée du journal : événement + horodatage Unix (secondes, `SystemTime`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Temps Unix de l'enregistrement, en secondes.
    pub unix_secs: u64,
    /// Événement consigné.
    pub event: AuditEvent,
}

impl fmt::Display for AuditEntry {
    /// Sérialisation en une ligne texte lisible : `[horodatage] evenement`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.unix_secs, self.event)
    }
}

// ---------------------------------------------------------------------------
// Journal d'audit
// ---------------------------------------------------------------------------

/// Journal d'audit en mémoire (thread-safe, clonable : les clones partagent
/// le même tampon). Borné : au-delà de la capacité, les événements les plus
/// anciens sont éliminés (rotation).
#[derive(Clone)]
pub struct AuditLog {
    entrees: Arc<Mutex<VecDeque<AuditEntry>>>,
    capacite: usize,
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditLog {
    /// Journal vide avec la capacité par défaut ([`CAPACITE_DEFAUT`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(CAPACITE_DEFAUT)
    }

    /// Journal vide gardant au plus `capacite` événements (borné à 1 minimum,
    /// pour qu'un journal ne soit jamais un trou noir silencieux).
    #[must_use]
    pub fn with_capacity(capacite: usize) -> Self {
        Self {
            entrees: Arc::new(Mutex::new(VecDeque::new())),
            capacite: capacite.max(1),
        }
    }

    /// Consigne un événement, horodaté à l'heure système courante.
    pub fn record(&self, event: AuditEvent) {
        self.record_at(event, unix_maintenant());
    }

    /// Consigne un événement avec un horodatage explicite (tests
    /// déterministes, ré-import d'événements). Applique la rotation.
    pub fn record_at(&self, event: AuditEvent, unix_secs: u64) {
        let mut entrees = self.entrees.lock().unwrap();
        entrees.push_back(AuditEntry { unix_secs, event });
        while entrees.len() > self.capacite {
            entrees.pop_front();
        }
    }

    /// Les `n` événements les plus récents, du plus ancien au plus récent.
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<AuditEntry> {
        let entrees = self.entrees.lock().unwrap();
        let saut = entrees.len().saturating_sub(n);
        entrees.iter().skip(saut).cloned().collect()
    }

    /// Tous les événements concernant un compte (voir [`AuditEvent::email`]),
    /// dans l'ordre d'enregistrement — le « journal d'accès » d'un compte.
    #[must_use]
    pub fn for_account(&self, email: &str) -> Vec<AuditEntry> {
        self.entrees
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.event.email() == Some(email))
            .cloned()
            .collect()
    }

    /// Nombre d'événements actuellement conservés (≤ capacité).
    #[must_use]
    pub fn count(&self) -> usize {
        self.entrees.lock().unwrap().len()
    }
}

// ---------------------------------------------------------------------------
// Enregistrements de session
// ---------------------------------------------------------------------------

/// Enregistrement d'une session de bureau à distance active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Identifiant de corrélation de la session (voir [`identifiant_session`]).
    pub session_id: String,
    /// Compte contrôleur (côté pilote).
    pub controleur: String,
    /// Compte ou pair contrôlé (poste distant).
    pub controle: String,
    /// Début de la session, en temps Unix (secondes).
    pub debut_unix_secs: u64,
    /// Permissions accordées à la session (libellés libres,
    /// p. ex. `affichage`, `controle`, `presse_papiers`, `transfert_fichiers`).
    pub permissions: Vec<String>,
}

/// État interne : sessions actives par identifiant + compteur d'unicité.
#[derive(Default)]
struct EtatSessions {
    actives: HashMap<String, SessionRecord>,
    compteur: u64,
}

/// Registre des sessions actives (thread-safe, clonable : les clones
/// partagent le même état). Peut consigner démarrages et fins dans un
/// [`AuditLog`] (voir [`SessionRecordStore::with_audit`]).
#[derive(Clone, Default)]
pub struct SessionRecordStore {
    etat: Arc<Mutex<EtatSessions>>,
    audit: Option<AuditLog>,
}

impl SessionRecordStore {
    /// Registre vide, sans journal d'audit attaché.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registre vide qui consigne `SessionStarted` / `SessionEnded` dans le
    /// journal donné (les clones partagent état et journal).
    #[must_use]
    pub fn with_audit(audit: AuditLog) -> Self {
        Self {
            etat: Arc::default(),
            audit: Some(audit),
        }
    }

    /// Ouvre une session (début = heure système) et renvoie son identifiant.
    pub fn start_session(&self, controleur: &str, controle: &str, permissions: &[&str]) -> String {
        self.start_session_at(controleur, controle, permissions, unix_maintenant())
    }

    /// Variante à horodatage explicite (tests déterministes).
    fn start_session_at(
        &self,
        controleur: &str,
        controle: &str,
        permissions: &[&str],
        debut_unix_secs: u64,
    ) -> String {
        let session_id = {
            let mut etat = self.etat.lock().unwrap();
            etat.compteur += 1;
            let session_id = identifiant_session(etat.compteur);
            etat.actives.insert(
                session_id.clone(),
                SessionRecord {
                    session_id: session_id.clone(),
                    controleur: controleur.to_string(),
                    controle: controle.to_string(),
                    debut_unix_secs,
                    permissions: permissions.iter().map(ToString::to_string).collect(),
                },
            );
            session_id
        };
        // Audit hors verrou (le journal a son propre Mutex).
        if let Some(journal) = &self.audit {
            journal.record(AuditEvent::SessionStarted {
                email: controleur.to_string(),
                session_id: session_id.clone(),
                peer_id: controle.to_string(),
            });
        }
        session_id
    }

    /// Clôt une session : la retire du registre et renvoie sa durée en
    /// secondes (`None` si l'identifiant est inconnu ou la session déjà
    /// close). Une horloge en recul donne une durée saturée à zéro.
    pub fn end_session(&self, session_id: &str) -> Option<u64> {
        self.end_session_at(session_id, unix_maintenant())
    }

    /// Variante à horodatage explicite (tests déterministes).
    fn end_session_at(&self, session_id: &str, fin_unix_secs: u64) -> Option<u64> {
        let enregistrement = self.etat.lock().unwrap().actives.remove(session_id)?;
        let duree = fin_unix_secs.saturating_sub(enregistrement.debut_unix_secs);
        if let Some(journal) = &self.audit {
            journal.record(AuditEvent::SessionEnded {
                session_id: session_id.to_string(),
                duration_secs: duree,
            });
        }
        Some(duree)
    }

    /// Enregistrement d'une session active (None si inconnue ou close).
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<SessionRecord> {
        self.etat.lock().unwrap().actives.get(session_id).cloned()
    }

    /// Instantané des sessions actives, trié par début puis identifiant
    /// (ordre déterministe pour l'affichage et les tests).
    #[must_use]
    pub fn active_sessions(&self) -> Vec<SessionRecord> {
        let mut sessions: Vec<SessionRecord> = self
            .etat
            .lock()
            .unwrap()
            .actives
            .values()
            .cloned()
            .collect();
        sessions.sort_by(|a, b| {
            (a.debut_unix_secs, a.session_id.as_str())
                .cmp(&(b.debut_unix_secs, b.session_id.as_str()))
        });
        sessions
    }
}

/// Identifiant de session : temps système (nanosecondes Unix) + compteur du
/// registre, en hexadécimal. **Non cryptographique** (aucune crate rng) :
/// c'est une clé de corrélation pour les journaux, pas un secret — les jetons
/// d'authentification restent ceux d'`AccountStore` (32 octets d'`OsRng`).
/// Le compteur garantit l'unicité au sein d'un registre même si l'horloge
/// ne bouge pas ou recule.
fn identifiant_session(compteur: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("s-{nanos:x}-{compteur:x}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Événement de test le plus simple portant un e-mail.
    fn evt(email: &str) -> AuditEvent {
        AuditEvent::LoginSuccess {
            email: email.to_string(),
        }
    }

    #[test]
    fn record_recent_et_count() {
        let journal = AuditLog::new();
        assert_eq!(journal.count(), 0);
        assert!(journal.recent(10).is_empty());

        journal.record_at(
            AuditEvent::AccountCreated {
                email: "a@x".into(),
            },
            100,
        );
        journal.record_at(evt("a@x"), 200);
        journal.record_at(evt("b@x"), 300);
        assert_eq!(journal.count(), 3);

        // `recent(2)` : les deux derniers, du plus ancien au plus récent.
        let deux = journal.recent(2);
        assert_eq!(deux.len(), 2);
        assert_eq!(deux[0].unix_secs, 200);
        assert_eq!(deux[1].unix_secs, 300);

        // n = 0 : rien ; n > taille : tout.
        assert!(journal.recent(0).is_empty());
        assert_eq!(journal.recent(99).len(), 3);

        // `record` (horloge réelle) : l'horodatage est renseigné.
        journal.record(evt("c@x"));
        assert_eq!(journal.count(), 4);
        assert!(journal.recent(1)[0].unix_secs > 0);
    }

    #[test]
    fn for_account_filtre_par_email() {
        let journal = AuditLog::new();
        journal.record_at(
            AuditEvent::AccountCreated {
                email: "a@x".into(),
            },
            1,
        );
        journal.record_at(
            AuditEvent::LoginFailure {
                email: "b@x".into(),
            },
            2,
        );
        journal.record_at(
            AuditEvent::PermissionDenied {
                email: "a@x".into(),
                action: "transfert_fichiers".into(),
            },
            3,
        );
        // `SessionEnded` ne porte pas d'e-mail : jamais renvoyé par compte.
        journal.record_at(
            AuditEvent::SessionEnded {
                session_id: "s-1".into(),
                duration_secs: 5,
            },
            4,
        );

        let pour_a = journal.for_account("a@x");
        assert_eq!(pour_a.len(), 2);
        assert_eq!(pour_a[0].unix_secs, 1);
        assert_eq!(pour_a[1].unix_secs, 3);
        assert_eq!(journal.for_account("b@x").len(), 1);
        assert!(journal.for_account("inconnu@x").is_empty());
    }

    #[test]
    fn rotation_garde_les_plus_recents() {
        let journal = AuditLog::with_capacity(3);
        for t in 1..=5 {
            journal.record_at(evt("a@x"), t);
        }
        assert_eq!(journal.count(), 3, "capacité respectée");
        let horodatages: Vec<u64> = journal.recent(99).iter().map(|e| e.unix_secs).collect();
        assert_eq!(horodatages, vec![3, 4, 5], "les plus anciens sont éliminés");

        // Capacité nulle demandée : bornée à 1, le journal reste utilisable.
        let mini = AuditLog::with_capacity(0);
        mini.record_at(evt("a@x"), 1);
        mini.record_at(evt("a@x"), 2);
        assert_eq!(mini.count(), 1);
        assert_eq!(mini.recent(1)[0].unix_secs, 2);
    }

    #[test]
    fn serialisation_en_ligne_texte() {
        let debut = AuditEntry {
            unix_secs: 1_700_000_000,
            event: AuditEvent::SessionStarted {
                email: "a@x".into(),
                session_id: "s-42".into(),
                peer_id: "ND-123".into(),
            },
        };
        assert_eq!(
            debut.to_string(),
            "[1700000000] session_demarree email=a@x session=s-42 pair=ND-123"
        );
        let fin = AuditEntry {
            unix_secs: 7,
            event: AuditEvent::SessionEnded {
                session_id: "s-42".into(),
                duration_secs: 90,
            },
        };
        assert_eq!(
            fin.to_string(),
            "[7] session_terminee session=s-42 duree_s=90"
        );
        let refus = AuditEntry {
            unix_secs: 8,
            event: AuditEvent::PermissionDenied {
                email: "a@x".into(),
                action: "presse_papiers".into(),
            },
        };
        assert_eq!(
            refus.to_string(),
            "[8] permission_refusee email=a@x action=presse_papiers"
        );
        assert_eq!(
            AuditEntry {
                unix_secs: 9,
                event: AuditEvent::AccountCreated {
                    email: "a@x".into()
                },
            }
            .to_string(),
            "[9] compte_cree email=a@x"
        );
    }

    #[test]
    fn cycle_session_duree_calculee() {
        let sessions = SessionRecordStore::new();
        let id =
            sessions.start_session_at("pilote@x", "ND-777", &["controle", "presse_papiers"], 1_000);

        let actives = sessions.active_sessions();
        assert_eq!(actives.len(), 1);
        assert_eq!(actives[0].session_id, id);
        assert_eq!(actives[0].controleur, "pilote@x");
        assert_eq!(actives[0].controle, "ND-777");
        assert_eq!(actives[0].debut_unix_secs, 1_000);
        assert_eq!(actives[0].permissions, vec!["controle", "presse_papiers"]);
        assert_eq!(sessions.session(&id).as_ref(), Some(&actives[0]));

        // Clôture : durée = fin - début ; la session quitte le registre.
        assert_eq!(sessions.end_session_at(&id, 1_042), Some(42));
        assert!(sessions.active_sessions().is_empty());
        assert!(sessions.session(&id).is_none());

        // Session inconnue (ou déjà close) : None.
        assert_eq!(sessions.end_session(&id), None);

        // Horloge en recul : durée saturée à zéro, pas de panique.
        let id2 = sessions.start_session_at("pilote@x", "ND-778", &[], 2_000);
        assert_eq!(sessions.end_session_at(&id2, 1_500), Some(0));
    }

    #[test]
    fn identifiants_de_session_uniques() {
        let sessions = SessionRecordStore::new();
        let ids: Vec<String> = (0..50)
            .map(|_| sessions.start_session("pilote@x", "ND-1", &["affichage"]))
            .collect();
        let mut dedup = ids.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            ids.len(),
            "chaque session reçoit un id distinct"
        );
        assert_eq!(sessions.active_sessions().len(), 50);
    }

    #[test]
    fn sessions_consignees_dans_le_journal() {
        let journal = AuditLog::new();
        let sessions = SessionRecordStore::with_audit(journal.clone());
        let id = sessions.start_session_at("pilote@x", "ND-9", &["affichage"], 100);
        assert_eq!(sessions.end_session_at(&id, 160), Some(60));

        let evts = journal.recent(10);
        assert_eq!(evts.len(), 2);
        assert_eq!(
            evts[0].event,
            AuditEvent::SessionStarted {
                email: "pilote@x".into(),
                session_id: id.clone(),
                peer_id: "ND-9".into(),
            }
        );
        assert_eq!(
            evts[1].event,
            AuditEvent::SessionEnded {
                session_id: id,
                duration_secs: 60,
            }
        );
        // `for_account` retrouve le démarrage via l'e-mail du contrôleur.
        assert_eq!(journal.for_account("pilote@x").len(), 1);
    }
}
