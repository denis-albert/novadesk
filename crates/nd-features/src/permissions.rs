//! Permissions granulaires et journal d'audit (voir plan 13, §permissions).
//!
//! Ce module affine le modèle historique [`Permissions`] (six booléens) en un
//! jeu de capacités individuelles ([`Capability`]) regroupées dans un
//! [`PermissionSet`] au **refus par défaut** : rien n'est permis tant que le
//! poste contrôlé n'a rien accordé. Les demandes interactives passent par un
//! [`PermissionBroker`], qui trace chaque événement (demande, décision,
//! révocation, action tentée) dans un journal d'audit consultable — qui a
//! fait quoi, et quand.
//!
//! Compatibilité : les conversions `From` entre [`Permissions`] et
//! [`PermissionSet`] sont **conservatrices** — elles n'élargissent jamais les
//! droits. Une capacité sans équivalent chez les six booléens (redémarrage
//! distant, tunnel…) n'est donc jamais accordée par conversion.
//!
//! Règle transverse (voir `lib.rs`) : ces vérifications s'appliquent côté
//! machine contrôlée, jamais seulement dans l'interface du contrôleur.
//!
//! # Contrat d'intégration (orchestrateur `nd-core`)
//!
//! L'application effective des permissions se câble **côté machine
//! contrôlée**, dans l'orchestrateur, à ces points de passage :
//!
//! | Action de session                          | Garde à poser avant l'action                          |
//! |--------------------------------------------|-------------------------------------------------------|
//! | injecter un [`InputEvent`] (`apply_input`) | [`Capability::required_for_input`] + garde ci-dessous |
//! | ouvrir le canal fichiers (envoi)           | [`Capability::FileUpload`]                            |
//! | ouvrir le canal fichiers (réception)       | [`Capability::FileDownload`]                          |
//! | démarrer la capture audio                  | [`Capability::Audio`]                                 |
//! | synchroniser le presse-papiers (lecture)   | [`Capability::ClipboardRead`]                         |
//! | synchroniser le presse-papiers (écriture)  | [`Capability::ClipboardWrite`]                        |
//! | ouvrir un enregistreur (`recording`)       | [`Capability::SessionRecording`]                      |
//! | appliquer un `PrivacyState` (`privacy`)    | [`Capability::PrivacyMode`]                           |
//! | ouvrir un `LocalForwarder` (`tunnel`)      | [`Capability::TcpTunnel`]                             |
//! | redémarrer la machine                      | [`Capability::RestartRemote`]                         |
//!
//! Deux niveaux de garde, à choisir selon la fréquence :
//! - [`PermissionBroker::authorize`] (ou [`PermissionBroker::authorize_input`])
//!   vérifie **et journalise** — à utiliser pour les actions ponctuelles
//!   (ouverture de canal, démarrage d'enregistrement) et pour tout **refus** ;
//! - [`PermissionBroker::is_allowed`] vérifie **sans journaliser** — le chemin
//!   chaud des entrées (des centaines de mouvements de souris par seconde ne
//!   doivent pas gonfler le journal d'audit). Motif recommandé pour le flux
//!   d'entrées : `is_allowed` par événement, et `authorize_input` (journalisé)
//!   au premier événement suivant un changement d'ensemble accordé ou lors
//!   d'un blocage, pour tracer « qui a été bloqué, quand ».

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use nd_proto::{InputEvent, NdError, Result};

use crate::Permissions;

/// Une capacité individuelle qu'une session peut se voir accorder.
///
/// La liste est volontairement plate (lecture et écriture du presse-papiers
/// séparées, envoi et réception de fichiers séparés) : chaque variante est un
/// bit du [`PermissionSet`], ce qui rend l'accord et la révocation atomiques.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Voir l'écran de la machine contrôlée.
    ViewScreen,
    /// Déplacer la souris et cliquer.
    ControlMouse,
    /// Frapper au clavier.
    ControlKeyboard,
    /// Lire le presse-papiers de la machine contrôlée.
    ClipboardRead,
    /// Écrire dans le presse-papiers de la machine contrôlée.
    ClipboardWrite,
    /// Déposer des fichiers vers la machine contrôlée.
    FileUpload,
    /// Récupérer des fichiers depuis la machine contrôlée.
    FileDownload,
    /// Entendre le son de la machine contrôlée.
    Audio,
    /// Redémarrer la machine contrôlée (et reprendre la session ensuite).
    RestartRemote,
    /// Enregistrer la session (voir `recording`).
    SessionRecording,
    /// Activer le mode confidentialité (voir `privacy`).
    PrivacyMode,
    /// Ouvrir des tunnels TCP (voir `tunnel`).
    TcpTunnel,
}

impl Capability {
    /// Toutes les capacités, dans l'ordre stable de leurs bits.
    pub const ALL: [Capability; 12] = [
        Capability::ViewScreen,
        Capability::ControlMouse,
        Capability::ControlKeyboard,
        Capability::ClipboardRead,
        Capability::ClipboardWrite,
        Capability::FileUpload,
        Capability::FileDownload,
        Capability::Audio,
        Capability::RestartRemote,
        Capability::SessionRecording,
        Capability::PrivacyMode,
        Capability::TcpTunnel,
    ];

    /// Bit stable de la capacité dans un [`PermissionSet`]. Ne jamais
    /// renuméroter : ces valeurs ont vocation à être sérialisées.
    fn bit(self) -> u16 {
        match self {
            Capability::ViewScreen => 1 << 0,
            Capability::ControlMouse => 1 << 1,
            Capability::ControlKeyboard => 1 << 2,
            Capability::ClipboardRead => 1 << 3,
            Capability::ClipboardWrite => 1 << 4,
            Capability::FileUpload => 1 << 5,
            Capability::FileDownload => 1 << 6,
            Capability::Audio => 1 << 7,
            Capability::RestartRemote => 1 << 8,
            Capability::SessionRecording => 1 << 9,
            Capability::PrivacyMode => 1 << 10,
            Capability::TcpTunnel => 1 << 11,
        }
    }

    /// Capacité requise pour injecter cet événement d'entrée sur la machine
    /// contrôlée — la table de correspondance que l'orchestrateur applique
    /// **avant** `nd-core::apply_input` (souris et clavier sont accordés et
    /// révoqués indépendamment).
    #[must_use]
    pub fn required_for_input(event: &InputEvent) -> Capability {
        match event {
            InputEvent::MouseMoveAbs { .. }
            | InputEvent::MouseMoveRel { .. }
            | InputEvent::MouseButton { .. }
            | InputEvent::Scroll { .. } => Capability::ControlMouse,
            InputEvent::Key { .. } | InputEvent::Unicode { .. } => Capability::ControlKeyboard,
        }
    }

    /// Libellé lisible, destiné aux boîtes de dialogue et au journal.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Capability::ViewScreen => "voir l'écran",
            Capability::ControlMouse => "contrôler la souris",
            Capability::ControlKeyboard => "contrôler le clavier",
            Capability::ClipboardRead => "lire le presse-papiers",
            Capability::ClipboardWrite => "écrire dans le presse-papiers",
            Capability::FileUpload => "envoyer des fichiers",
            Capability::FileDownload => "récupérer des fichiers",
            Capability::Audio => "entendre l'audio",
            Capability::RestartRemote => "redémarrer la machine",
            Capability::SessionRecording => "enregistrer la session",
            Capability::PrivacyMode => "activer le mode confidentialité",
            Capability::TcpTunnel => "ouvrir des tunnels TCP",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Ensemble de capacités accordées à une session. Refus par défaut :
/// l'ensemble vide ([`PermissionSet::none`], aussi `Default`) n'autorise rien.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct PermissionSet {
    bits: u16,
}

impl PermissionSet {
    /// Aucun droit — le point de départ de toute session.
    #[must_use]
    pub fn none() -> Self {
        PermissionSet { bits: 0 }
    }

    /// Toutes les capacités connues.
    #[must_use]
    pub fn full() -> Self {
        Capability::ALL.into_iter().collect()
    }

    /// Observation seule : uniquement [`Capability::ViewScreen`].
    #[must_use]
    pub fn view_only() -> Self {
        let mut ensemble = PermissionSet::none();
        ensemble.grant(Capability::ViewScreen);
        ensemble
    }

    /// Accorde une capacité ; rend vrai si elle ne l'était pas déjà.
    pub fn grant(&mut self, cap: Capability) -> bool {
        let avant = self.bits;
        self.bits |= cap.bit();
        self.bits != avant
    }

    /// Révoque une capacité ; rend vrai si elle était accordée.
    pub fn revoke(&mut self, cap: Capability) -> bool {
        let avant = self.bits;
        self.bits &= !cap.bit();
        self.bits != avant
    }

    /// La capacité est-elle accordée ?
    #[must_use]
    pub fn allows(self, cap: Capability) -> bool {
        self.bits & cap.bit() != 0
    }

    /// Toutes ces capacités sont-elles accordées ?
    #[must_use]
    pub fn allows_all(self, caps: impl IntoIterator<Item = Capability>) -> bool {
        caps.into_iter().all(|cap| self.allows(cap))
    }

    /// Les capacités accordées, dans l'ordre de [`Capability::ALL`].
    pub fn granted(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .into_iter()
            .filter(move |cap| self.allows(*cap))
    }

    /// Nombre de capacités accordées.
    #[must_use]
    pub fn count(self) -> usize {
        self.bits.count_ones() as usize
    }

    /// Vrai si rien n'est accordé.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Union : accordé dans l'un ou l'autre.
    #[must_use]
    pub fn union(self, autre: Self) -> Self {
        PermissionSet {
            bits: self.bits | autre.bits,
        }
    }

    /// Intersection : accordé dans les deux (sert à borner une décision à ce
    /// qui était réellement demandé).
    #[must_use]
    pub fn intersection(self, autre: Self) -> Self {
        PermissionSet {
            bits: self.bits & autre.bits,
        }
    }
}

impl FromIterator<Capability> for PermissionSet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        let mut ensemble = PermissionSet::none();
        for cap in iter {
            ensemble.grant(cap);
        }
        ensemble
    }
}

impl fmt::Debug for PermissionSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PermissionSet")?;
        f.debug_set().entries(self.granted()).finish()
    }
}

impl From<Permissions> for PermissionSet {
    /// Traduit les six booléens historiques. `view_only` neutralise les
    /// capacités d'entrée, comme le faisait `Permissions::allows_input`.
    /// Voir l'écran est toujours accordé : c'est le sens même d'une session.
    fn from(anciennes: Permissions) -> Self {
        let mut ensemble = PermissionSet::view_only();
        if anciennes.keyboard && !anciennes.view_only {
            ensemble.grant(Capability::ControlKeyboard);
        }
        if anciennes.mouse && !anciennes.view_only {
            ensemble.grant(Capability::ControlMouse);
        }
        if anciennes.clipboard {
            ensemble.grant(Capability::ClipboardRead);
            ensemble.grant(Capability::ClipboardWrite);
        }
        if anciennes.files {
            ensemble.grant(Capability::FileUpload);
            ensemble.grant(Capability::FileDownload);
        }
        if anciennes.audio {
            ensemble.grant(Capability::Audio);
        }
        ensemble
    }
}

impl From<PermissionSet> for Permissions {
    /// Réduction conservatrice vers les six booléens : un booléen n'est vrai
    /// que si **toutes** les capacités qu'il recouvre sont accordées (un
    /// presse-papiers en lecture seule redevient `clipboard: false`, jamais
    /// l'inverse — on n'élargit pas les droits en changeant de modèle).
    fn from(ensemble: PermissionSet) -> Self {
        let keyboard = ensemble.allows(Capability::ControlKeyboard);
        let mouse = ensemble.allows(Capability::ControlMouse);
        Permissions {
            keyboard,
            mouse,
            clipboard: ensemble.allows(Capability::ClipboardRead)
                && ensemble.allows(Capability::ClipboardWrite),
            files: ensemble.allows(Capability::FileUpload)
                && ensemble.allows(Capability::FileDownload),
            audio: ensemble.allows(Capability::Audio),
            view_only: !keyboard && !mouse,
        }
    }
}

/// Sort d'une demande d'autorisation interactive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// En attente de la décision de l'utilisateur du poste contrôlé.
    Pending,
    /// Accordée — éventuellement partiellement : l'ensemble retenu.
    Granted(PermissionSet),
    /// Refusée en bloc.
    Denied,
}

/// Demande d'autorisation adressée au poste contrôlé (« Alice souhaite
/// contrôler le clavier et lire le presse-papiers »).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Identifiant stable de la demande (attribué par le [`PermissionBroker`]).
    pub id: u64,
    /// Identité déclarée du demandeur (contrôleur).
    pub requester: String,
    /// Capacités demandées.
    pub caps: PermissionSet,
    /// Décision courante.
    pub decision: PermissionDecision,
}

/// Ce qui s'est produit, pour le journal d'audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEvent {
    /// Une demande d'autorisation a été ouverte.
    RequestOpened { id: u64, caps: PermissionSet },
    /// Une demande a été accordée (l'ensemble effectivement retenu).
    RequestGranted { id: u64, caps: PermissionSet },
    /// Une demande a été refusée.
    RequestDenied { id: u64 },
    /// Une capacité en cours a été révoquée.
    CapabilityRevoked { cap: Capability },
    /// Une action a été tentée et autorisée.
    ActionAllowed { cap: Capability },
    /// Une action a été tentée et bloquée.
    ActionBlocked { cap: Capability },
}

/// Une ligne du journal d'audit : qui a fait quoi, et quand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Numéro d'ordre strictement croissant (fiable même si l'horloge saute).
    pub sequence: u64,
    /// Horodatage en millisecondes Unix (indicatif).
    pub unix_ms: u64,
    /// Acteur : demandeur pour une demande ou une action, décideur pour une
    /// décision ou une révocation.
    pub actor: String,
    /// L'événement lui-même.
    pub event: AuditEvent,
}

/// Guichet d'autorisations d'une session : détient l'ensemble courant,
/// arbitre les demandes interactives et tient le journal d'audit.
///
/// Toute vérification d'action passe par [`PermissionBroker::authorize`],
/// qui journalise l'issue — c'est la trace « qui a fait quoi » du plan 13.
#[derive(Debug)]
pub struct PermissionBroker {
    accordees: PermissionSet,
    demandes: Vec<PermissionRequest>,
    journal: Vec<AuditEntry>,
    prochain_id: u64,
    sequence: u64,
}

impl PermissionBroker {
    /// Guichet vierge : rien n'est accordé (refus par défaut).
    #[must_use]
    pub fn new() -> Self {
        PermissionBroker {
            accordees: PermissionSet::none(),
            demandes: Vec::new(),
            journal: Vec::new(),
            prochain_id: 1,
            sequence: 0,
        }
    }

    /// Guichet initialisé avec un ensemble déjà accordé (profil enregistré,
    /// accès non surveillé…).
    #[must_use]
    pub fn with_permissions(initiales: PermissionSet) -> Self {
        let mut guichet = PermissionBroker::new();
        guichet.accordees = initiales;
        guichet
    }

    /// L'ensemble des capacités actuellement accordées.
    #[must_use]
    pub fn permissions(&self) -> PermissionSet {
        self.accordees
    }

    /// Toutes les demandes connues, décidées ou non.
    #[must_use]
    pub fn requests(&self) -> &[PermissionRequest] {
        &self.demandes
    }

    /// La demande `id`, si elle existe.
    #[must_use]
    pub fn find_request(&self, id: u64) -> Option<&PermissionRequest> {
        self.demandes.iter().find(|demande| demande.id == id)
    }

    /// Les demandes encore en attente de décision.
    pub fn pending(&self) -> impl Iterator<Item = &PermissionRequest> {
        self.demandes
            .iter()
            .filter(|demande| demande.decision == PermissionDecision::Pending)
    }

    /// Ouvre une demande d'autorisation au nom de `requester` et rend son
    /// identifiant. Rien n'est accordé tant qu'elle n'est pas tranchée.
    pub fn request(&mut self, requester: &str, caps: PermissionSet) -> u64 {
        let id = self.prochain_id;
        self.prochain_id += 1;
        self.demandes.push(PermissionRequest {
            id,
            requester: requester.to_owned(),
            caps,
            decision: PermissionDecision::Pending,
        });
        self.journaliser(requester, AuditEvent::RequestOpened { id, caps });
        id
    }

    /// Tranche la demande `id` en accordant `accordees` — bornées à ce qui
    /// était demandé : impossible d'élargir une demande en l'accordant.
    /// Rend l'ensemble effectivement accordé.
    pub fn grant(
        &mut self,
        id: u64,
        decideur: &str,
        accordees: PermissionSet,
    ) -> Result<PermissionSet> {
        let demande = self.demande_en_attente(id)?;
        let effectives = accordees.intersection(demande.caps);
        demande.decision = PermissionDecision::Granted(effectives);
        self.accordees = self.accordees.union(effectives);
        self.journaliser(
            decideur,
            AuditEvent::RequestGranted {
                id,
                caps: effectives,
            },
        );
        Ok(effectives)
    }

    /// Refuse la demande `id` en bloc.
    pub fn deny(&mut self, id: u64, decideur: &str) -> Result<()> {
        let demande = self.demande_en_attente(id)?;
        demande.decision = PermissionDecision::Denied;
        self.journaliser(decideur, AuditEvent::RequestDenied { id });
        Ok(())
    }

    /// Révoque immédiatement une capacité en cours de session ; rend vrai si
    /// elle était accordée. La révocation est journalisée.
    pub fn revoke(&mut self, decideur: &str, cap: Capability) -> bool {
        let retiree = self.accordees.revoke(cap);
        if retiree {
            self.journaliser(decideur, AuditEvent::CapabilityRevoked { cap });
        }
        retiree
    }

    /// Vérifie qu'`actor` peut exercer `cap`, et journalise l'issue — c'est
    /// le point de passage obligé de toute action de session.
    pub fn authorize(&mut self, actor: &str, cap: Capability) -> bool {
        let autorise = self.accordees.allows(cap);
        let evenement = if autorise {
            AuditEvent::ActionAllowed { cap }
        } else {
            AuditEvent::ActionBlocked { cap }
        };
        self.journaliser(actor, evenement);
        autorise
    }

    /// Vérifie qu'`actor` peut injecter cet événement d'entrée, et journalise
    /// l'issue (voir [`Capability::required_for_input`] pour la table).
    pub fn authorize_input(&mut self, actor: &str, event: &InputEvent) -> bool {
        self.authorize(actor, Capability::required_for_input(event))
    }

    /// La capacité est-elle accordée ? Garde **sans journalisation**, pour le
    /// chemin chaud (flux d'entrées) — voir le contrat d'intégration en tête
    /// de module pour l'articulation avec [`PermissionBroker::authorize`].
    ///
    /// ```
    /// use nd_features::{Capability, PermissionBroker, PermissionSet};
    /// use nd_proto::InputEvent;
    ///
    /// let broker = PermissionBroker::with_permissions(PermissionSet::view_only());
    /// let clic = InputEvent::MouseButton { button: 0, down: true };
    /// // Session en observation seule : l'injection doit être écartée.
    /// assert!(!broker.is_allowed(Capability::required_for_input(&clic)));
    /// ```
    #[must_use]
    pub fn is_allowed(&self, cap: Capability) -> bool {
        self.accordees.allows(cap)
    }

    /// Le journal d'audit, dans l'ordre des événements.
    #[must_use]
    pub fn journal(&self) -> &[AuditEntry] {
        &self.journal
    }

    /// Retrouve la demande `id` si elle est encore en attente.
    fn demande_en_attente(&mut self, id: u64) -> Result<&mut PermissionRequest> {
        let demande = self
            .demandes
            .iter_mut()
            .find(|demande| demande.id == id)
            .ok_or_else(|| NdError::Protocol(format!("demande d'autorisation {id} inconnue")))?;
        if demande.decision != PermissionDecision::Pending {
            return Err(NdError::Protocol(format!(
                "demande d'autorisation {id} déjà tranchée"
            )));
        }
        Ok(demande)
    }

    fn journaliser(&mut self, acteur: &str, evenement: AuditEvent) {
        self.sequence += 1;
        self.journal.push(AuditEntry {
            sequence: self.sequence,
            unix_ms: maintenant_unix_ms(),
            actor: acteur.to_owned(),
            event: evenement,
        });
    }
}

impl Default for PermissionBroker {
    fn default() -> Self {
        PermissionBroker::new()
    }
}

/// Horodatage courant en millisecondes Unix (0 si l'horloge précède 1970).
fn maintenant_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |ecart| {
            u64::try_from(ecart.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refus_par_defaut() {
        let ensemble = PermissionSet::default();
        assert!(ensemble.is_empty());
        for cap in Capability::ALL {
            assert!(!ensemble.allows(cap), "{cap} ne devrait pas être accordée");
        }
    }

    #[test]
    fn accorder_et_revoquer() {
        let mut ensemble = PermissionSet::none();
        assert!(ensemble.grant(Capability::ControlMouse));
        assert!(!ensemble.grant(Capability::ControlMouse)); // déjà accordée
        assert!(ensemble.allows(Capability::ControlMouse));
        assert!(!ensemble.allows(Capability::ControlKeyboard));

        assert!(ensemble.revoke(Capability::ControlMouse));
        assert!(!ensemble.revoke(Capability::ControlMouse)); // déjà révoquée
        assert!(ensemble.is_empty());
    }

    #[test]
    fn full_et_view_only() {
        assert!(PermissionSet::full().allows_all(Capability::ALL));
        assert_eq!(PermissionSet::full().count(), Capability::ALL.len());

        let observation = PermissionSet::view_only();
        assert!(observation.allows(Capability::ViewScreen));
        assert_eq!(observation.count(), 1);
    }

    #[test]
    fn granted_suit_l_ordre_stable() {
        let ensemble: PermissionSet = [Capability::Audio, Capability::ClipboardRead]
            .into_iter()
            .collect();
        let liste: Vec<Capability> = ensemble.granted().collect();
        assert_eq!(liste, vec![Capability::ClipboardRead, Capability::Audio]);
    }

    #[test]
    fn conversion_depuis_permissions_full() {
        let ensemble = PermissionSet::from(Permissions::full());
        assert!(ensemble.allows_all([
            Capability::ViewScreen,
            Capability::ControlMouse,
            Capability::ControlKeyboard,
            Capability::ClipboardRead,
            Capability::ClipboardWrite,
            Capability::FileUpload,
            Capability::FileDownload,
            Capability::Audio,
        ]));
        // Les capacités sans équivalent historique ne sont jamais déduites.
        assert!(!ensemble.allows(Capability::RestartRemote));
        assert!(!ensemble.allows(Capability::TcpTunnel));
        // Aller-retour sans perte pour le contrôle complet.
        assert_eq!(Permissions::from(ensemble), Permissions::full());
    }

    #[test]
    fn conversion_view_only_aller_retour() {
        let ensemble = PermissionSet::from(Permissions::view_only());
        assert_eq!(ensemble, PermissionSet::view_only());
        assert_eq!(Permissions::from(ensemble), Permissions::view_only());
    }

    #[test]
    fn view_only_neutralise_les_entrees() {
        // Cas incohérent du vieux modèle : clavier accordé mais view_only.
        let anciennes = Permissions {
            keyboard: true,
            mouse: true,
            clipboard: false,
            files: false,
            audio: false,
            view_only: true,
        };
        let ensemble = PermissionSet::from(anciennes);
        assert!(!ensemble.allows(Capability::ControlKeyboard));
        assert!(!ensemble.allows(Capability::ControlMouse));
        assert!(ensemble.allows(Capability::ViewScreen));
    }

    #[test]
    fn reduction_conservatrice_vers_les_booleens() {
        // Presse-papiers en lecture seule : le booléen global reste faux
        // (on n'élargit jamais les droits en changeant de modèle).
        let mut ensemble = PermissionSet::view_only();
        ensemble.grant(Capability::ClipboardRead);
        let anciennes = Permissions::from(ensemble);
        assert!(!anciennes.clipboard);
        assert!(anciennes.view_only);

        ensemble.grant(Capability::ClipboardWrite);
        assert!(Permissions::from(ensemble).clipboard);
    }

    #[test]
    fn guichet_demande_puis_accord_partiel() {
        let mut guichet = PermissionBroker::new();
        let demande: PermissionSet = [Capability::ControlMouse, Capability::ClipboardRead]
            .into_iter()
            .collect();
        let id = guichet.request("alice", demande);
        assert_eq!(guichet.pending().count(), 1);

        // L'hôte accorde « tout » : l'accord est borné à ce qui était demandé.
        let effectives = guichet.grant(id, "hôte", PermissionSet::full()).unwrap();
        assert_eq!(effectives, demande);
        assert!(guichet.permissions().allows(Capability::ControlMouse));
        assert!(!guichet.permissions().allows(Capability::ControlKeyboard));
        assert_eq!(guichet.pending().count(), 0);
        assert_eq!(
            guichet.find_request(id).unwrap().decision,
            PermissionDecision::Granted(demande)
        );

        // Journal : ouverture par « alice », accord par « hôte ».
        let journal = guichet.journal();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].actor, "alice");
        assert_eq!(
            journal[0].event,
            AuditEvent::RequestOpened { id, caps: demande }
        );
        assert_eq!(journal[1].actor, "hôte");
        assert_eq!(
            journal[1].event,
            AuditEvent::RequestGranted { id, caps: demande }
        );
        assert!(journal[0].sequence < journal[1].sequence);
    }

    #[test]
    fn guichet_refus_et_demandes_deja_tranchees() {
        let mut guichet = PermissionBroker::new();
        let id = guichet.request("mallory", PermissionSet::full());
        guichet.deny(id, "hôte").unwrap();

        assert!(guichet.permissions().is_empty());
        assert_eq!(
            guichet.find_request(id).unwrap().decision,
            PermissionDecision::Denied
        );
        // Une demande tranchée ne se rejoue pas, un id inconnu non plus.
        assert!(guichet.deny(id, "hôte").is_err());
        assert!(guichet.grant(id, "hôte", PermissionSet::full()).is_err());
        assert!(guichet.grant(999, "hôte", PermissionSet::full()).is_err());
    }

    #[test]
    fn authorize_journalise_qui_a_fait_quoi() {
        let mut guichet = PermissionBroker::with_permissions(PermissionSet::view_only());
        assert!(guichet.authorize("alice", Capability::ViewScreen));
        assert!(!guichet.authorize("alice", Capability::ControlKeyboard));

        let journal = guichet.journal();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].actor, "alice");
        assert_eq!(
            journal[0].event,
            AuditEvent::ActionAllowed {
                cap: Capability::ViewScreen
            }
        );
        assert_eq!(
            journal[1].event,
            AuditEvent::ActionBlocked {
                cap: Capability::ControlKeyboard
            }
        );
    }

    #[test]
    fn mapping_input_vers_capacite_complet() {
        // Chaque variante du protocole d'entrées a une capacité requise claire.
        let souris = [
            InputEvent::MouseMoveAbs {
                x: 0.5,
                y: 0.5,
                monitor: 0,
            },
            InputEvent::MouseMoveRel { dx: 1.0, dy: -2.0 },
            InputEvent::MouseButton {
                button: 0,
                down: true,
            },
            InputEvent::Scroll { dx: 0.0, dy: 3.0 },
        ];
        for evenement in &souris {
            assert_eq!(
                Capability::required_for_input(evenement),
                Capability::ControlMouse,
                "{evenement:?}"
            );
        }
        let clavier = [
            InputEvent::Key {
                scancode: 0x1C,
                down: true,
            },
            InputEvent::Unicode { codepoint: 0xE9 },
        ];
        for evenement in &clavier {
            assert_eq!(
                Capability::required_for_input(evenement),
                Capability::ControlKeyboard,
                "{evenement:?}"
            );
        }
    }

    #[test]
    fn is_allowed_ne_journalise_pas() {
        let broker = PermissionBroker::with_permissions(PermissionSet::view_only());
        assert!(broker.is_allowed(Capability::ViewScreen));
        assert!(!broker.is_allowed(Capability::ControlMouse));
        // Garde du chemin chaud : aucune trace dans le journal d'audit.
        assert!(broker.journal().is_empty());
    }

    #[test]
    fn authorize_input_applique_le_mapping_et_journalise() {
        let mut broker =
            PermissionBroker::with_permissions([Capability::ControlMouse].into_iter().collect());
        let clic = InputEvent::MouseButton {
            button: 0,
            down: true,
        };
        let frappe = InputEvent::Key {
            scancode: 0x1C,
            down: true,
        };
        assert!(broker.authorize_input("alice", &clic));
        assert!(!broker.authorize_input("alice", &frappe)); // clavier non accordé

        let journal = broker.journal();
        assert_eq!(journal.len(), 2);
        assert_eq!(
            journal[0].event,
            AuditEvent::ActionAllowed {
                cap: Capability::ControlMouse
            }
        );
        assert_eq!(
            journal[1].event,
            AuditEvent::ActionBlocked {
                cap: Capability::ControlKeyboard
            }
        );
    }

    #[test]
    fn revocation_immediate_et_journalisee() {
        let mut guichet = PermissionBroker::with_permissions(PermissionSet::full());
        assert!(guichet.revoke("hôte", Capability::Audio));
        assert!(!guichet.permissions().allows(Capability::Audio));
        // Révoquer deux fois ne journalise qu'une fois.
        assert!(!guichet.revoke("hôte", Capability::Audio));
        assert_eq!(guichet.journal().len(), 1);
        assert_eq!(
            guichet.journal()[0].event,
            AuditEvent::CapabilityRevoked {
                cap: Capability::Audio
            }
        );
    }
}
