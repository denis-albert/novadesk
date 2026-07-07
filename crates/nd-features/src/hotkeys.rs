//! Raccourcis clavier de session : modèle [`Hotkey`] (modificateurs +
//! touche), table [`HotkeyMap`] associant des raccourcis à des actions
//! applicatives, et sérialisation binaire pour la configuration.
//!
//! Format binaire (entiers petit-boutistes) :
//! - en-tête : magic `NDHK` (4 octets) puis version `u16` ;
//! - puis `u32 nombre_de_liens`, et pour chaque lien :
//!   `[u8 modificateurs][u32 touche][u32 code_action]`.
//!
//! # Intégration — le dispatch N'EST PAS implémenté ici
//!
//! Ce module fournit la table et sa persistance ; il n'écoute aucun clavier.
//! Le branchement attendu, côté **contrôleur** (fenêtre de session de l'UI) :
//! 1. dans la boucle d'événements clavier de la fenêtre, normaliser
//!    l'événement en [`Hotkey`] (mêmes bits de modificateurs, code de touche
//!    du protocole d'entrées `nd-proto`) ;
//! 2. si [`HotkeyMap::lookup`] rend une [`HostAction`], **consommer**
//!    l'événement localement (ne pas l'envoyer au poste distant) et exécuter
//!    l'action via l'orchestrateur — `SendCtrlAltDel` et `ToggleInputBlock`
//!    repassent par les permissions ([`crate::Capability`]) côté contrôlé ;
//! 3. sinon, laisser l'événement suivre le chemin normal d'injection.
//!
//! TODO(nd-core/UI) : point de branchement du lookup dans la boucle
//! d'événements de la fenêtre de session — rien n'est câblé ici.

use std::collections::BTreeMap;

use nd_proto::{NdError, Result};

/// Magic en tête d'une configuration de raccourcis.
pub const MAGIC: &[u8; 4] = b"NDHK";

/// Version courante du format de configuration des raccourcis.
pub const VERSION: u16 = 1;

/// Un raccourci clavier : combinaison de modificateurs et d'une touche.
///
/// `key` est un code de touche indépendant de la plate-forme (celui du
/// protocole d'entrées, voir `nd-proto`) ; ce module le traite comme opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hotkey {
    /// Modificateurs actifs, combinaison des bits [`Hotkey::CTRL`],
    /// [`Hotkey::ALT`], [`Hotkey::SHIFT`] et [`Hotkey::WIN`].
    pub modifiers: u8,
    /// Code de la touche principale.
    pub key: u32,
}

impl Hotkey {
    /// Bit du modificateur Ctrl.
    pub const CTRL: u8 = 0b0001;
    /// Bit du modificateur Alt.
    pub const ALT: u8 = 0b0010;
    /// Bit du modificateur Shift (Maj).
    pub const SHIFT: u8 = 0b0100;
    /// Bit du modificateur Win (touche logo / Super).
    pub const WIN: u8 = 0b1000;
    /// Masque de tous les bits de modificateurs connus.
    pub const MOD_MASK: u8 = 0b1111;

    /// Construit un raccourci. Les bits de modificateurs inconnus sont
    /// ignorés (masqués), pour qu'un même raccourci ait une forme canonique.
    #[must_use]
    pub fn new(modifiers: u8, key: u32) -> Self {
        Hotkey {
            modifiers: modifiers & Self::MOD_MASK,
            key,
        }
    }

    /// Le raccourci contient-il tous les modificateurs demandés ?
    #[must_use]
    pub fn has(self, modifiers: u8) -> bool {
        self.modifiers & modifiers == modifiers & Self::MOD_MASK
    }
}

/// Actions applicatives déclenchables par raccourci côté contrôleur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostAction {
    /// Basculer l'affichage plein écran.
    ToggleFullscreen,
    /// Envoyer Ctrl+Alt+Suppr au poste contrôlé.
    SendCtrlAltDel,
    /// Bloquer/débloquer les entrées locales du poste contrôlé.
    ToggleInputBlock,
    /// Basculer la session en lecture seule (et retour).
    ToggleViewOnly,
    /// Capturer l'écran distant dans un fichier.
    TakeScreenshot,
    /// Démarrer/arrêter l'enregistrement de session.
    ToggleRecording,
    /// Fermer la session en cours.
    Disconnect,
}

/// Action encodable en `u32` pour la sérialisation de la configuration.
pub trait ActionCodec: Sized {
    /// Code binaire stable de l'action (contrat de compatibilité du format).
    fn encode(&self) -> u32;
    /// Action correspondant au code, ou `None` si le code est inconnu.
    fn decode(code: u32) -> Option<Self>;
}

impl ActionCodec for HostAction {
    fn encode(&self) -> u32 {
        match self {
            HostAction::ToggleFullscreen => 0,
            HostAction::SendCtrlAltDel => 1,
            HostAction::ToggleInputBlock => 2,
            HostAction::ToggleViewOnly => 3,
            HostAction::TakeScreenshot => 4,
            HostAction::ToggleRecording => 5,
            HostAction::Disconnect => 6,
        }
    }

    fn decode(code: u32) -> Option<Self> {
        Some(match code {
            0 => HostAction::ToggleFullscreen,
            1 => HostAction::SendCtrlAltDel,
            2 => HostAction::ToggleInputBlock,
            3 => HostAction::ToggleViewOnly,
            4 => HostAction::TakeScreenshot,
            5 => HostAction::ToggleRecording,
            6 => HostAction::Disconnect,
            _ => return None,
        })
    }
}

/// Table de raccourcis : associe chaque [`Hotkey`] à une action.
///
/// Un raccourci ne pointe que vers une seule action : relier un raccourci
/// déjà pris **écrase** l'ancien lien (l'ancienne action est renvoyée).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyMap<A> {
    // BTreeMap : ordre stable → sérialisation déterministe.
    liens: BTreeMap<Hotkey, A>,
}

impl<A> Default for HotkeyMap<A> {
    fn default() -> Self {
        HotkeyMap {
            liens: BTreeMap::new(),
        }
    }
}

impl<A> HotkeyMap<A> {
    /// Table vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Relie `hotkey` à `action`. Si le raccourci était déjà pris, le nouveau
    /// lien écrase l'ancien et l'ancienne action est renvoyée.
    pub fn bind(&mut self, hotkey: Hotkey, action: A) -> Option<A> {
        self.liens.insert(hotkey, action)
    }

    /// Supprime le lien de `hotkey` et renvoie l'action qui y était reliée.
    pub fn unbind(&mut self, hotkey: Hotkey) -> Option<A> {
        self.liens.remove(&hotkey)
    }

    /// Action reliée à `hotkey`, s'il y en a une.
    #[must_use]
    pub fn lookup(&self, hotkey: Hotkey) -> Option<&A> {
        self.liens.get(&hotkey)
    }

    /// Nombre de raccourcis reliés.
    #[must_use]
    pub fn len(&self) -> usize {
        self.liens.len()
    }

    /// La table est-elle vide ?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.liens.is_empty()
    }

    /// Itère sur les liens, dans l'ordre canonique des raccourcis.
    pub fn iter(&self) -> impl Iterator<Item = (&Hotkey, &A)> {
        self.liens.iter()
    }
}

impl<A: ActionCodec> HotkeyMap<A> {
    /// Sérialise la table pour la configuration (format d'en-tête de module).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut octets = Vec::with_capacity(4 + 2 + 4 + self.liens.len() * 9);
        octets.extend_from_slice(MAGIC);
        octets.extend_from_slice(&VERSION.to_le_bytes());
        // Le nombre de liens tient toujours en u32 : chaque lien occupe
        // 9 octets, une table de plus de u32::MAX liens est irréaliste.
        octets.extend_from_slice(&(self.liens.len() as u32).to_le_bytes());
        for (raccourci, action) in &self.liens {
            octets.push(raccourci.modifiers);
            octets.extend_from_slice(&raccourci.key.to_le_bytes());
            octets.extend_from_slice(&action.encode().to_le_bytes());
        }
        octets
    }

    /// Relit une table sérialisée par [`HotkeyMap::to_bytes`].
    ///
    /// Refuse : magic ou version inconnus, bits de modificateurs inconnus,
    /// code d'action inconnu, raccourci en double, flux tronqué ou excédent.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut curseur = Curseur { data, position: 0 };
        if curseur.prend(4)? != MAGIC {
            return Err(NdError::Protocol(
                "magic NDHK absent : ce n'est pas une configuration de raccourcis".into(),
            ));
        }
        let version = u16::from_le_bytes(curseur.prend(2)?.try_into().expect("2 octets"));
        if version != VERSION {
            return Err(NdError::Protocol(format!(
                "version de configuration {version} non gérée (attendu {VERSION})"
            )));
        }
        let nombre = u32::from_le_bytes(curseur.prend(4)?.try_into().expect("4 octets"));

        let mut liens = BTreeMap::new();
        for _ in 0..nombre {
            let modifiers = curseur.prend(1)?[0];
            if modifiers & !Hotkey::MOD_MASK != 0 {
                return Err(NdError::Protocol(format!(
                    "bits de modificateurs inconnus : {modifiers:#010b}"
                )));
            }
            let key = u32::from_le_bytes(curseur.prend(4)?.try_into().expect("4 octets"));
            let code = u32::from_le_bytes(curseur.prend(4)?.try_into().expect("4 octets"));
            let action = A::decode(code)
                .ok_or_else(|| NdError::Protocol(format!("code d'action inconnu : {code}")))?;
            let raccourci = Hotkey { modifiers, key };
            if liens.insert(raccourci, action).is_some() {
                return Err(NdError::Protocol(format!(
                    "raccourci en double dans la configuration : {raccourci:?}"
                )));
            }
        }
        if curseur.position != data.len() {
            return Err(NdError::Protocol(
                "octets excédentaires après la configuration de raccourcis".into(),
            ));
        }
        Ok(HotkeyMap { liens })
    }
}

/// Petit curseur de lecture sur tranche, avec erreur de troncature explicite.
struct Curseur<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Curseur<'a> {
    /// Prend les `n` octets suivants, ou signale une configuration tronquée.
    fn prend(&mut self, n: usize) -> Result<&'a [u8]> {
        let fin = self
            .position
            .checked_add(n)
            .filter(|&f| f <= self.data.len());
        match fin {
            Some(fin) => {
                let tranche = &self.data[self.position..fin];
                self.position = fin;
                Ok(tranche)
            }
            None => Err(NdError::Protocol(
                "configuration de raccourcis tronquée".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ctrl+Alt+F (code de touche arbitraire pour les tests).
    fn ctrl_alt_f() -> Hotkey {
        Hotkey::new(Hotkey::CTRL | Hotkey::ALT, 0x46)
    }

    #[test]
    fn bind_puis_lookup() {
        let mut table = HotkeyMap::new();
        assert!(table.is_empty());
        assert_eq!(table.bind(ctrl_alt_f(), HostAction::ToggleFullscreen), None);
        assert_eq!(
            table.lookup(ctrl_alt_f()),
            Some(&HostAction::ToggleFullscreen)
        );
        // Même touche sans les mêmes modificateurs : pas de correspondance.
        assert_eq!(table.lookup(Hotkey::new(Hotkey::CTRL, 0x46)), None);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn rebind_ecrase_l_ancien_lien() {
        let mut table = HotkeyMap::new();
        table.bind(ctrl_alt_f(), HostAction::ToggleFullscreen);
        let ancien = table.bind(ctrl_alt_f(), HostAction::TakeScreenshot);
        assert_eq!(ancien, Some(HostAction::ToggleFullscreen));
        assert_eq!(
            table.lookup(ctrl_alt_f()),
            Some(&HostAction::TakeScreenshot)
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn unbind_libere_le_raccourci() {
        let mut table = HotkeyMap::new();
        table.bind(ctrl_alt_f(), HostAction::Disconnect);
        assert_eq!(table.unbind(ctrl_alt_f()), Some(HostAction::Disconnect));
        assert_eq!(table.lookup(ctrl_alt_f()), None);
        assert_eq!(table.unbind(ctrl_alt_f()), None);
    }

    #[test]
    fn modificateurs_inconnus_masques_a_la_construction() {
        let h = Hotkey::new(0xFF, 1);
        assert_eq!(h.modifiers, Hotkey::MOD_MASK);
        assert!(h.has(Hotkey::CTRL | Hotkey::WIN));
        assert!(!Hotkey::new(Hotkey::SHIFT, 1).has(Hotkey::CTRL));
    }

    #[test]
    fn aller_retour_binaire() {
        let mut table = HotkeyMap::new();
        table.bind(ctrl_alt_f(), HostAction::ToggleFullscreen);
        table.bind(
            Hotkey::new(Hotkey::CTRL | Hotkey::SHIFT, 0x2E),
            HostAction::SendCtrlAltDel,
        );
        table.bind(Hotkey::new(Hotkey::WIN, 0x50), HostAction::ToggleRecording);

        let octets = table.to_bytes();
        assert_eq!(&octets[..4], MAGIC);
        let relue = HotkeyMap::<HostAction>::from_bytes(&octets).unwrap();
        assert_eq!(relue, table);
    }

    #[test]
    fn table_vide_fait_l_aller_retour() {
        let table: HotkeyMap<HostAction> = HotkeyMap::new();
        let relue = HotkeyMap::<HostAction>::from_bytes(&table.to_bytes()).unwrap();
        assert!(relue.is_empty());
    }

    #[test]
    fn codes_d_action_stables_a_l_aller_retour() {
        for action in [
            HostAction::ToggleFullscreen,
            HostAction::SendCtrlAltDel,
            HostAction::ToggleInputBlock,
            HostAction::ToggleViewOnly,
            HostAction::TakeScreenshot,
            HostAction::ToggleRecording,
            HostAction::Disconnect,
        ] {
            assert_eq!(HostAction::decode(action.encode()), Some(action));
        }
        assert_eq!(HostAction::decode(999), None);
    }

    #[test]
    fn flux_invalides_refuses() {
        // Magic erroné.
        assert!(HotkeyMap::<HostAction>::from_bytes(b"XXXX\x01\x00\x00\x00\x00\x00").is_err());
        // Version inconnue.
        let mut octets = MAGIC.to_vec();
        octets.extend_from_slice(&9u16.to_le_bytes());
        octets.extend_from_slice(&0u32.to_le_bytes());
        assert!(HotkeyMap::<HostAction>::from_bytes(&octets).is_err());

        let mut table = HotkeyMap::new();
        table.bind(ctrl_alt_f(), HostAction::Disconnect);
        let octets = table.to_bytes();
        // Tronqué.
        assert!(HotkeyMap::<HostAction>::from_bytes(&octets[..octets.len() - 1]).is_err());
        // Excédent.
        let mut trop = octets.clone();
        trop.push(0);
        assert!(HotkeyMap::<HostAction>::from_bytes(&trop).is_err());
        // Code d'action inconnu (les 4 derniers octets du lien).
        let mut mauvais = octets.clone();
        let fin = mauvais.len();
        mauvais[fin - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(HotkeyMap::<HostAction>::from_bytes(&mauvais).is_err());
        // Bits de modificateurs inconnus (1er octet du lien, après l'en-tête).
        let mut mauvais = octets;
        mauvais[10] = 0xF0;
        assert!(HotkeyMap::<HostAction>::from_bytes(&mauvais).is_err());
    }
}
