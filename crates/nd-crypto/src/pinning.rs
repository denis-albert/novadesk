//! Épinglage TOFU (« trust on first use ») des empreintes de pairs — anti-MITM,
//! plan 06.
//!
//! À la première connexion à un pair, son empreinte ([`crate::PeerFingerprint`],
//! vérifiable de visu via le SAS) est épinglée dans [`KnownPeers`]. Aux connexions
//! suivantes, toute empreinte différente signale un homme-du-milieu potentiel (ou
//! une réinstallation du pair) : la connexion doit être refusée tant que
//! l'utilisateur n'a pas explicitement accepté la nouvelle empreinte.
//!
//! Format du fichier de persistance (texte, une ligne par pair) :
//!
//! ```text
//! <nom du pair> <empreinte, 64 caractères hexadécimaux>
//! ```
//!
//! L'empreinte est le dernier champ de la ligne (séparateur : dernière espace),
//! ce qui autorise des noms de pairs contenant des espaces.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use nd_proto::{NdError, Result};

use crate::identity::{decode_hex, encode_hex};
use crate::PeerFingerprint;

/// Résultat de la vérification TOFU d'une empreinte (voir [`KnownPeers::verify_or_pin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinResult {
    /// Pair encore inconnu : son empreinte vient d'être épinglée.
    FirstSeen,
    /// L'empreinte correspond à celle épinglée : le pair est bien celui attendu.
    Match,
    /// L'empreinte DIFFÈRE de celle épinglée : MITM potentiel. L'empreinte connue
    /// n'est PAS écrasée ; il faut une confirmation explicite de l'utilisateur
    /// (puis [`KnownPeers::force_pin`]) pour accepter la nouvelle.
    Changed,
}

/// Erreur type pour un fichier de pairs connus illisible ou altéré.
fn corrompu(detail: &str) -> NdError {
    NdError::Crypto(format!("fichier des pairs connus corrompu : {detail}"))
}

/// Table des pairs connus : nom de pair → empreinte épinglée à la première rencontre.
///
/// `BTreeMap` garantit une sérialisation déterministe (lignes triées par nom),
/// pratique pour les diffs et les tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KnownPeers {
    pairs: BTreeMap<String, PeerFingerprint>,
}

impl KnownPeers {
    /// Table vide (aucun pair épinglé).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Vérifie l'empreinte présentée par `nom` selon la politique TOFU :
    ///
    /// * pair inconnu → épingle l'empreinte et renvoie [`PinResult::FirstSeen`] ;
    /// * empreinte identique à celle épinglée → [`PinResult::Match`] ;
    /// * empreinte différente → [`PinResult::Changed`], SANS écraser l'empreinte
    ///   connue (l'appelant doit alerter l'utilisateur : MITM potentiel).
    ///
    /// NB : un nom contenant un saut de ligne sera refusé au moment de
    /// [`save`](Self::save) (le format de persistance est « une ligne par pair »).
    #[must_use]
    pub fn verify_or_pin(&mut self, nom: &str, empreinte: &PeerFingerprint) -> PinResult {
        match self.pairs.get(nom) {
            None => {
                self.pairs.insert(nom.to_string(), *empreinte);
                PinResult::FirstSeen
            }
            Some(connue) if connue == empreinte => PinResult::Match,
            Some(_) => PinResult::Changed,
        }
    }

    /// Remplace l'empreinte épinglée pour `nom`.
    ///
    /// À n'appeler qu'après confirmation explicite de l'utilisateur (p. ex.
    /// réinstallation légitime du pair distant, vérifiée hors bande via le SAS).
    pub fn force_pin(&mut self, nom: &str, empreinte: &PeerFingerprint) {
        self.pairs.insert(nom.to_string(), *empreinte);
    }

    /// Empreinte épinglée pour `nom`, si ce pair est connu.
    #[must_use]
    pub fn fingerprint(&self, nom: &str) -> Option<PeerFingerprint> {
        self.pairs.get(nom).copied()
    }

    /// Nombre de pairs épinglés.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Vrai si aucun pair n'est épinglé.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Enregistre la table dans `path` (format documenté en tête de module).
    ///
    /// Refuse un nom de pair vide ou contenant un saut de ligne, qui casserait le
    /// format « une ligne par pair ».
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut contenu = String::new();
        for (nom, empreinte) in &self.pairs {
            if nom.is_empty() || nom.contains(['\n', '\r']) {
                return Err(NdError::Crypto(format!(
                    "nom de pair impossible à persister : {nom:?}"
                )));
            }
            contenu.push_str(nom);
            contenu.push(' ');
            contenu.push_str(&encode_hex(&empreinte.0));
            contenu.push('\n');
        }
        fs::write(path, contenu)?;
        Ok(())
    }

    /// Charge une table depuis `path`. Toute ligne mal formée (séparateur absent,
    /// empreinte non hexadécimale ou de mauvaise taille, nom en double) est refusée
    /// avec [`NdError::Crypto`] : on ne travaille jamais avec une table partielle.
    pub fn load(path: &Path) -> Result<Self> {
        let octets = fs::read(path)?;
        let texte = String::from_utf8(octets).map_err(|_| corrompu("contenu non UTF-8"))?;

        let mut pairs = BTreeMap::new();
        for ligne in texte.lines() {
            if ligne.is_empty() {
                continue;
            }
            // L'empreinte est le dernier champ : la coupure se fait sur la
            // dernière espace, les noms peuvent donc en contenir.
            let Some((nom, empreinte_hex)) = ligne.rsplit_once(' ') else {
                return Err(corrompu("ligne sans séparateur"));
            };
            if nom.is_empty() {
                return Err(corrompu("nom de pair vide"));
            }
            let octets =
                decode_hex(empreinte_hex).map_err(|_| corrompu("empreinte non hexadécimale"))?;
            let empreinte: [u8; 32] = octets
                .try_into()
                .map_err(|_| corrompu("empreinte de taille inattendue"))?;
            if pairs
                .insert(nom.to_string(), PeerFingerprint(empreinte))
                .is_some()
            {
                return Err(corrompu("nom de pair en double"));
            }
        }
        Ok(Self { pairs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::FichierTemp;

    /// Empreinte de test remplie d'un octet constant.
    fn fp(octet: u8) -> PeerFingerprint {
        PeerFingerprint([octet; 32])
    }

    #[test]
    fn tofu_first_seen_puis_match_puis_changed() {
        let mut connus = KnownPeers::new();
        let empreinte = fp(0x11);

        // Première rencontre : épinglage.
        assert_eq!(
            connus.verify_or_pin("poste-bureau", &empreinte),
            PinResult::FirstSeen
        );
        // Même empreinte : authentifié.
        assert_eq!(
            connus.verify_or_pin("poste-bureau", &empreinte),
            PinResult::Match
        );
        // Empreinte différente : alerte, et l'empreinte épinglée reste l'originale.
        let intruse = fp(0x22);
        assert_eq!(
            connus.verify_or_pin("poste-bureau", &intruse),
            PinResult::Changed
        );
        assert_eq!(connus.fingerprint("poste-bureau"), Some(empreinte));

        // Après confirmation explicite de l'utilisateur, force_pin accepte la
        // nouvelle empreinte, qui devient la référence.
        connus.force_pin("poste-bureau", &intruse);
        assert_eq!(
            connus.verify_or_pin("poste-bureau", &intruse),
            PinResult::Match
        );
    }

    #[test]
    fn les_pairs_sont_distingues_par_nom() {
        let mut connus = KnownPeers::new();
        assert_eq!(
            connus.verify_or_pin("alice", &fp(0xAA)),
            PinResult::FirstSeen
        );
        assert_eq!(connus.verify_or_pin("bob", &fp(0xBB)), PinResult::FirstSeen);
        assert_eq!(connus.verify_or_pin("alice", &fp(0xAA)), PinResult::Match);
        assert_eq!(connus.verify_or_pin("bob", &fp(0xAA)), PinResult::Changed);
        assert_eq!(connus.len(), 2);
        assert!(!connus.is_empty());
    }

    #[test]
    fn save_puis_load_restituent_la_table() {
        let mut connus = KnownPeers::new();
        // Le nom peut contenir des espaces : l'empreinte est le dernier champ.
        let _ = connus.verify_or_pin("pc du salon", &fp(0x01));
        let _ = connus.verify_or_pin("portable-atelier", &fp(0x02));

        let fichier = FichierTemp::nouveau("pairs-connus");
        connus.save(fichier.chemin()).expect("enregistrement");

        let recharges = KnownPeers::load(fichier.chemin()).expect("rechargement");
        assert_eq!(recharges, connus);
        // La table rechargée applique bien la politique TOFU.
        let mut recharges = recharges;
        assert_eq!(
            recharges.verify_or_pin("pc du salon", &fp(0x01)),
            PinResult::Match
        );
        assert_eq!(
            recharges.verify_or_pin("pc du salon", &fp(0x03)),
            PinResult::Changed
        );
    }

    #[test]
    fn save_et_load_d_une_table_vide() {
        let fichier = FichierTemp::nouveau("pairs-vides");
        KnownPeers::new()
            .save(fichier.chemin())
            .expect("enregistrement");
        let recharges = KnownPeers::load(fichier.chemin()).expect("rechargement");
        assert!(recharges.is_empty());
    }

    #[test]
    fn load_refuse_un_fichier_corrompu() {
        let cas = [
            // Pas de séparateur.
            "sansseparateur\n".to_string(),
            // Empreinte non hexadécimale.
            "alice zz\n".to_string(),
            // Empreinte trop courte.
            format!("alice {}\n", "00".repeat(8)),
            // Nom en double.
            format!("alice {}\nalice {}\n", "00".repeat(32), "11".repeat(32)),
        ];
        for contenu in cas {
            let fichier = FichierTemp::nouveau("pairs-corrompus");
            std::fs::write(fichier.chemin(), &contenu).expect("écriture du fichier de test");
            assert!(
                matches!(KnownPeers::load(fichier.chemin()), Err(NdError::Crypto(_))),
                "contenu accepté à tort : {contenu:?}"
            );
        }
    }

    #[test]
    fn save_refuse_un_nom_avec_saut_de_ligne() {
        let mut connus = KnownPeers::new();
        let _ = connus.verify_or_pin("nom\nmalveillant", &fp(0x42));
        let fichier = FichierTemp::nouveau("pairs-nom-invalide");
        assert!(matches!(
            connus.save(fichier.chemin()),
            Err(NdError::Crypto(_))
        ));
    }
}
