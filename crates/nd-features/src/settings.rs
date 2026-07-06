//! Réglages de session côté contrôleur : préréglage de qualité, options de
//! confort (curseur, audio, presse-papiers…), validation et sérialisation
//! binaire pour la configuration (voir plan 10, §réglages).
//!
//! Format binaire (entiers petit-boutistes) :
//! - en-tête : magic `NDST` (4 octets) puis version `u16` ;
//! - puis `[u8 qualité][u8 drapeaux][u8 échelle %][u16 plafond fps]`
//!   (plafond 0 = aucun plafond).

use nd_proto::{NdError, Result};

/// Magic en tête d'un bloc de réglages de session.
pub const MAGIC: &[u8; 4] = b"NDST";

/// Version courante du format de réglages.
pub const VERSION: u16 = 1;

/// Bornes de l'échelle d'affichage, en pourcentage.
pub const SCALE_MIN: u8 = 25;
/// Borne haute de l'échelle d'affichage, en pourcentage.
pub const SCALE_MAX: u8 = 100;

/// Plafond maximal admissible pour la cadence d'images.
pub const FPS_CAP_MAX: u16 = 240;

/// Préréglage de qualité vidéo de la session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityPreset {
    /// Adaptatif : part d'un profil équilibré puis suit la bande passante.
    #[default]
    Auto,
    /// Fidélité maximale (réseau local, revue graphique).
    HighQuality,
    /// Compromis fluidité/netteté pour un usage courant.
    Balanced,
    /// Économe : liaisons mobiles ou très contraintes.
    LowBandwidth,
}

/// Paramètres concrets dérivés d'un [`QualityPreset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityParams {
    /// Cadence cible, en images par seconde.
    pub target_fps: u16,
    /// Débit vidéo maximal, en kilobits par seconde.
    pub max_bitrate_kbps: u32,
    /// Qualité de quantification (0 = minimale, 100 = maximale).
    pub quality: u8,
    /// Si vrai, le codec adapte débit et cadence à la bande passante mesurée.
    pub adaptive: bool,
}

impl QualityPreset {
    /// Paramètres concrets du préréglage.
    #[must_use]
    pub fn params(self) -> QualityParams {
        match self {
            QualityPreset::Auto => QualityParams {
                target_fps: 30,
                max_bitrate_kbps: 8_000,
                quality: 75,
                adaptive: true,
            },
            QualityPreset::HighQuality => QualityParams {
                target_fps: 60,
                max_bitrate_kbps: 20_000,
                quality: 90,
                adaptive: false,
            },
            QualityPreset::Balanced => QualityParams {
                target_fps: 30,
                max_bitrate_kbps: 8_000,
                quality: 75,
                adaptive: false,
            },
            QualityPreset::LowBandwidth => QualityParams {
                target_fps: 15,
                max_bitrate_kbps: 1_500,
                quality: 50,
                adaptive: false,
            },
        }
    }

    /// Code binaire stable du préréglage (contrat du format de réglages).
    #[must_use]
    pub fn encode(self) -> u8 {
        match self {
            QualityPreset::Auto => 0,
            QualityPreset::HighQuality => 1,
            QualityPreset::Balanced => 2,
            QualityPreset::LowBandwidth => 3,
        }
    }

    /// Préréglage correspondant au code, ou `None` si le code est inconnu.
    #[must_use]
    pub fn decode(code: u8) -> Option<Self> {
        Some(match code {
            0 => QualityPreset::Auto,
            1 => QualityPreset::HighQuality,
            2 => QualityPreset::Balanced,
            3 => QualityPreset::LowBandwidth,
            _ => return None,
        })
    }
}

// Bits du champ « drapeaux » sérialisé.
const FLAG_CURSOR: u8 = 0b0000_0001;
const FLAG_AUDIO: u8 = 0b0000_0010;
const FLAG_MIC: u8 = 0b0000_0100;
const FLAG_CLIPBOARD: u8 = 0b0000_1000;
const FLAG_FILES: u8 = 0b0001_0000;
const FLAG_MASK: u8 = 0b0001_1111;

/// Taille exacte d'un bloc de réglages sérialisé.
const TAILLE_SERIALISEE: usize = 4 + 2 + 1 + 1 + 1 + 2;

/// Réglages d'une session de contrôle à distance.
///
/// Rappel transverse : ces réglages expriment ce que le **contrôleur**
/// souhaite ; le poste contrôlé reste maître via ses [`crate::Permissions`]
/// (p. ex. `allow_file_transfer` sans la permission `files` reste inerte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSettings {
    /// Préréglage de qualité vidéo.
    pub quality: QualityPreset,
    /// Dessiner le curseur distant dans l'image.
    pub capture_cursor: bool,
    /// Restituer l'audio du poste distant.
    pub play_audio: bool,
    /// Transmettre le micro local vers le poste distant.
    pub enable_mic: bool,
    /// Synchroniser le presse-papiers dans les deux sens.
    pub clipboard_sync: bool,
    /// Autoriser le transfert de fichiers pendant la session.
    pub allow_file_transfer: bool,
    /// Échelle d'affichage, en pourcentage (`SCALE_MIN..=SCALE_MAX`).
    pub scale_percent: u8,
    /// Plafond de cadence imposé par l'utilisateur (`1..=FPS_CAP_MAX`),
    /// ou `None` pour laisser le préréglage décider.
    pub fps_cap: Option<u16>,
}

impl Default for SessionSettings {
    fn default() -> Self {
        // Défaut prudent : rien qui capte le micro ni ne transfère de
        // fichiers sans choix explicite de l'utilisateur.
        SessionSettings {
            quality: QualityPreset::Auto,
            capture_cursor: true,
            play_audio: true,
            enable_mic: false,
            clipboard_sync: true,
            allow_file_transfer: false,
            scale_percent: 100,
            fps_cap: None,
        }
    }
}

impl SessionSettings {
    /// Vérifie la cohérence des réglages (bornes d'échelle et de cadence).
    pub fn validate(&self) -> Result<()> {
        if !(SCALE_MIN..=SCALE_MAX).contains(&self.scale_percent) {
            return Err(NdError::Protocol(format!(
                "échelle {} % hors bornes ({SCALE_MIN}..={SCALE_MAX})",
                self.scale_percent
            )));
        }
        if let Some(fps) = self.fps_cap {
            if !(1..=FPS_CAP_MAX).contains(&fps) {
                return Err(NdError::Protocol(format!(
                    "plafond de cadence {fps} hors bornes (1..={FPS_CAP_MAX})"
                )));
            }
        }
        Ok(())
    }

    /// Paramètres de qualité effectifs : ceux du préréglage, la cadence
    /// étant écrêtée par `fps_cap` s'il est renseigné.
    #[must_use]
    pub fn effective_params(&self) -> QualityParams {
        let mut params = self.quality.params();
        if let Some(plafond) = self.fps_cap {
            params.target_fps = params.target_fps.min(plafond);
        }
        params
    }

    /// Sérialise les réglages pour la configuration. Le résultat n'est
    /// relisible par [`SessionSettings::from_bytes`] que si
    /// [`SessionSettings::validate`] passe.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut drapeaux = 0u8;
        for (actif, bit) in [
            (self.capture_cursor, FLAG_CURSOR),
            (self.play_audio, FLAG_AUDIO),
            (self.enable_mic, FLAG_MIC),
            (self.clipboard_sync, FLAG_CLIPBOARD),
            (self.allow_file_transfer, FLAG_FILES),
        ] {
            if actif {
                drapeaux |= bit;
            }
        }
        let mut octets = Vec::with_capacity(TAILLE_SERIALISEE);
        octets.extend_from_slice(MAGIC);
        octets.extend_from_slice(&VERSION.to_le_bytes());
        octets.push(self.quality.encode());
        octets.push(drapeaux);
        octets.push(self.scale_percent);
        octets.extend_from_slice(&self.fps_cap.unwrap_or(0).to_le_bytes());
        octets
    }

    /// Relit des réglages sérialisés par [`SessionSettings::to_bytes`].
    ///
    /// Refuse : magic ou version inconnus, taille inexacte, préréglage ou
    /// bits de drapeaux inconnus, et tout réglage rejeté par
    /// [`SessionSettings::validate`].
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() != TAILLE_SERIALISEE {
            return Err(NdError::Protocol(format!(
                "bloc de réglages de {} octets (attendu {TAILLE_SERIALISEE})",
                data.len()
            )));
        }
        if &data[..4] != MAGIC {
            return Err(NdError::Protocol(
                "magic NDST absent : ce n'est pas un bloc de réglages".into(),
            ));
        }
        let version = u16::from_le_bytes([data[4], data[5]]);
        if version != VERSION {
            return Err(NdError::Protocol(format!(
                "version de réglages {version} non gérée (attendu {VERSION})"
            )));
        }
        let quality = QualityPreset::decode(data[6]).ok_or_else(|| {
            NdError::Protocol(format!("préréglage de qualité inconnu : {}", data[6]))
        })?;
        let drapeaux = data[7];
        if drapeaux & !FLAG_MASK != 0 {
            return Err(NdError::Protocol(format!(
                "bits de drapeaux inconnus : {drapeaux:#010b}"
            )));
        }
        let plafond = u16::from_le_bytes([data[9], data[10]]);
        let reglages = SessionSettings {
            quality,
            capture_cursor: drapeaux & FLAG_CURSOR != 0,
            play_audio: drapeaux & FLAG_AUDIO != 0,
            enable_mic: drapeaux & FLAG_MIC != 0,
            clipboard_sync: drapeaux & FLAG_CLIPBOARD != 0,
            allow_file_transfer: drapeaux & FLAG_FILES != 0,
            scale_percent: data[8],
            fps_cap: (plafond != 0).then_some(plafond),
        };
        reglages.validate()?;
        Ok(reglages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_ordonnes_du_plus_riche_au_plus_econome() {
        let hq = QualityPreset::HighQuality.params();
        let eq = QualityPreset::Balanced.params();
        let eco = QualityPreset::LowBandwidth.params();
        assert!(hq.max_bitrate_kbps > eq.max_bitrate_kbps);
        assert!(eq.max_bitrate_kbps > eco.max_bitrate_kbps);
        assert!(hq.target_fps > eq.target_fps);
        assert!(eq.target_fps > eco.target_fps);
        assert!(hq.quality > eq.quality && eq.quality > eco.quality);
        // Seul Auto est adaptatif.
        assert!(QualityPreset::Auto.params().adaptive);
        assert!(!hq.adaptive && !eq.adaptive && !eco.adaptive);
    }

    #[test]
    fn defaut_prudent_et_valide() {
        let d = SessionSettings::default();
        assert_eq!(d.quality, QualityPreset::Auto);
        assert!(d.capture_cursor && d.play_audio && d.clipboard_sync);
        assert!(!d.enable_mic && !d.allow_file_transfer);
        assert_eq!(d.scale_percent, 100);
        assert_eq!(d.fps_cap, None);
        d.validate().unwrap();
    }

    #[test]
    fn validation_rejette_les_bornes_depassees() {
        let mut r = SessionSettings {
            scale_percent: SCALE_MIN - 1,
            ..SessionSettings::default()
        };
        assert!(r.validate().is_err());
        r.scale_percent = SCALE_MAX;
        r.validate().unwrap();
        r.fps_cap = Some(0);
        assert!(r.validate().is_err());
        r.fps_cap = Some(FPS_CAP_MAX + 1);
        assert!(r.validate().is_err());
        r.fps_cap = Some(FPS_CAP_MAX);
        r.validate().unwrap();
    }

    #[test]
    fn plafond_de_cadence_ecrete_le_preset() {
        let mut r = SessionSettings {
            quality: QualityPreset::HighQuality,
            ..SessionSettings::default()
        };
        assert_eq!(r.effective_params().target_fps, 60);
        r.fps_cap = Some(24);
        assert_eq!(r.effective_params().target_fps, 24);
        // Un plafond plus haut que le préréglage ne change rien.
        r.fps_cap = Some(144);
        assert_eq!(r.effective_params().target_fps, 60);
    }

    #[test]
    fn aller_retour_binaire() {
        let reglages = SessionSettings {
            quality: QualityPreset::LowBandwidth,
            capture_cursor: false,
            play_audio: false,
            enable_mic: true,
            clipboard_sync: false,
            allow_file_transfer: true,
            scale_percent: 50,
            fps_cap: Some(24),
        };
        let octets = reglages.to_bytes();
        assert_eq!(octets.len(), TAILLE_SERIALISEE);
        assert_eq!(&octets[..4], MAGIC);
        assert_eq!(SessionSettings::from_bytes(&octets).unwrap(), reglages);

        // Le défaut (fps_cap = None) fait aussi l'aller-retour.
        let defaut = SessionSettings::default();
        assert_eq!(
            SessionSettings::from_bytes(&defaut.to_bytes()).unwrap(),
            defaut
        );
    }

    #[test]
    fn presets_stables_a_l_aller_retour() {
        for preset in [
            QualityPreset::Auto,
            QualityPreset::HighQuality,
            QualityPreset::Balanced,
            QualityPreset::LowBandwidth,
        ] {
            assert_eq!(QualityPreset::decode(preset.encode()), Some(preset));
        }
        assert_eq!(QualityPreset::decode(200), None);
    }

    #[test]
    fn blocs_invalides_refuses() {
        let bon = SessionSettings::default().to_bytes();
        // Taille inexacte (tronqué et excédent).
        assert!(SessionSettings::from_bytes(&bon[..bon.len() - 1]).is_err());
        let mut trop = bon.clone();
        trop.push(0);
        assert!(SessionSettings::from_bytes(&trop).is_err());
        // Magic erroné.
        let mut mauvais = bon.clone();
        mauvais[0] = b'X';
        assert!(SessionSettings::from_bytes(&mauvais).is_err());
        // Version inconnue.
        let mut mauvais = bon.clone();
        mauvais[4] = 9;
        assert!(SessionSettings::from_bytes(&mauvais).is_err());
        // Préréglage inconnu.
        let mut mauvais = bon.clone();
        mauvais[6] = 42;
        assert!(SessionSettings::from_bytes(&mauvais).is_err());
        // Drapeaux inconnus.
        let mut mauvais = bon.clone();
        mauvais[7] |= 0b1000_0000;
        assert!(SessionSettings::from_bytes(&mauvais).is_err());
        // Échelle hors bornes : la validation s'applique aussi en lecture.
        let mut mauvais = bon;
        mauvais[8] = 10;
        assert!(SessionSettings::from_bytes(&mauvais).is_err());
    }
}
