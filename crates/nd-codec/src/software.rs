//! Backend logiciel H.264 via la crate `openh264` (Cisco OpenH264).
//!
//! C'est le **repli logiciel** du plan 03 : il fonctionne partout, sans GPU. Les
//! backends matériels (NVENC sur NVIDIA, Media Foundation sur Windows, VideoToolbox
//! sur macOS…) seront ajoutés derrière les mêmes traits [`VideoEncoder`] /
//! [`VideoDecoder`]. L'usage est réglé sur « contenu écran temps réel »
//! (`ScreenContentRealTime`), adapté au bureau (voir plan 03 §optimisation desktop).
//!
//! ## Boucle de performance (plan 03/04)
//!
//! - **Débit à chaud** : [`VideoEncoder::set_target_bitrate`] est réellement câblé
//!   via `ENCODER_OPTION_BITRATE`/`ENCODER_OPTION_MAX_BITRATE` (FFI openh264) — le
//!   contrôle de débit interne suit la consigne dès la trame suivante, sans
//!   réinitialisation ni image-clé parasite. C'est le levier qu'actionne l'ABR
//!   ([`crate::RateController`]).
//! - **Encodage delta** (opt-in, [`VideoEncoder::set_delta_mode`]) : un canevas
//!   I420 persistant reçoit les conversions BGRA → YUV **restreintes aux régions
//!   modifiées** (`CapturedFrame::dirty`), et les trames sans changement sont
//!   **sautées** (trame de répétition à données vides). Politique et limites :
//!   voir le module [`crate::delta`].

use std::os::raw::{c_int, c_void};
use std::ptr::addr_of_mut;

use nd_capture::{CapturedFrame, FrameImage};
use nd_proto::{NdError, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    Encoder, EncoderConfig as OhConfig, FrameType, RateControlMode, UsageType,
};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use openh264_sys2::{
    SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL,
};

use crate::delta::{aire_totale, rects_pairs_bornes, RectPair, SuiviDelta};
use crate::{
    CodecCaps, CodecKind, DecodedFrame, EncodedChunk, EncoderConfig, VideoDecoder, VideoEncoder,
};

/// Convertit une erreur `openh264` en `NdError::Codec`.
fn codec_err(e: openh264::Error) -> NdError {
    NdError::Codec(e.to_string())
}

// ---------------------------------------------------------------------------
// Canevas I420 persistant (conversion BGRA → YUV, pleine ou par régions)
// ---------------------------------------------------------------------------

/// Canevas I420 persistant entre les trames : le plan Y (`l×h` octets) puis les
/// plans U et V (`l/2×h/2` octets chacun). Il reçoit la conversion BGRA → I420
/// soit en plein cadre, soit **restreinte aux régions modifiées** (mode delta) :
/// l'encodeur voit toujours une image complète cohérente, mais la conversion ne
/// coûte que la surface réellement changée.
///
/// Conversion **BT.601 pleine plage** (Y ∈ [0, 255]) : le décodeur de cette crate
/// ([`Openh264Decoder`], `write_rgba8` d'openh264) interprète la YUV en pleine
/// plage — l'aller-retour NovaDesk hôte → viewer est donc colorimétriquement
/// cohérent (l'ancienne conversion plage limitée plafonnait le PSNR de la boucle
/// à ≈ 26 dB par simple écart de plage, avant toute perte de compression).
struct CanevasI420 {
    largeur: usize,
    hauteur: usize,
    donnees: Vec<u8>,
}

impl CanevasI420 {
    fn vide() -> Self {
        Self {
            largeur: 0,
            hauteur: 0,
            donnees: Vec::new(),
        }
    }

    /// Vrai si le canevas contient une image complète aux dimensions `l`×`h`
    /// (une conversion pleine a déjà eu lieu à cette taille).
    fn compatible(&self, largeur: usize, hauteur: usize) -> bool {
        self.largeur == largeur && self.hauteur == hauteur && !self.donnees.is_empty()
    }

    /// (Re)dimensionne le canevas (contenu remis à zéro — une conversion pleine
    /// doit suivre).
    fn redimensionner(&mut self, largeur: usize, hauteur: usize) {
        self.largeur = largeur;
        self.hauteur = hauteur;
        self.donnees.clear();
        self.donnees
            .resize(largeur * hauteur + (largeur / 2) * (hauteur / 2) * 2, 0);
    }

    /// Convertit le rectangle `r` (aligné pair, borné — contrat [`RectPair`]) du
    /// tampon BGRA (`stride` octets par ligne) vers les plans I420 du canevas.
    /// BT.601 **pleine plage** (voir doc de la struct) ; chroma moyennée par bloc
    /// 2×2. Chemin scalaire (SIMD = optimisation future, plan 03).
    fn convertir_rect(&mut self, bgra: &[u8], stride: usize, r: RectPair) {
        let (l, h) = (self.largeur, self.hauteur);
        let (plan_y, reste) = self.donnees.split_at_mut(l * h);
        let (plan_u, plan_v) = reste.split_at_mut((l / 2) * (h / 2));

        // Luminance : un échantillon par pixel du rectangle.
        // Coefficients entiers ×256 : Y = 0,299 R + 0,587 G + 0,114 B.
        for y in r.y..r.y + r.h {
            let src = &bgra[y * stride + r.x * 4..y * stride + (r.x + r.w) * 4];
            let dst = &mut plan_y[y * l + r.x..y * l + r.x + r.w];
            for (px, dy) in src.chunks_exact(4).zip(dst.iter_mut()) {
                let (b, g, rr) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
                *dy = ((77 * rr + 150 * g + 29 * b + 128) >> 8) as u8;
            }
        }

        // Chrominance : moyenne RGB de chaque bloc 2×2 (coordonnées paires
        // garanties), un couple U/V par bloc.
        // U = −0,168 736 R − 0,331 264 G + 0,5 B + 128 ; V symétrique.
        let lc = l / 2;
        for by in r.y / 2..(r.y + r.h) / 2 {
            for bx in r.x / 2..(r.x + r.w) / 2 {
                let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
                for dy in 0..2 {
                    let off = (by * 2 + dy) * stride + bx * 2 * 4;
                    for dx in 0..2 {
                        let px = &bgra[off + dx * 4..off + dx * 4 + 3];
                        sb += i32::from(px[0]);
                        sg += i32::from(px[1]);
                        sr += i32::from(px[2]);
                    }
                }
                let (b, g, rr) = (sb / 4, sg / 4, sr / 4);
                plan_u[by * lc + bx] =
                    ((((-43 * rr - 85 * g + 128 * b) + 128) >> 8) + 128).clamp(0, 255) as u8;
                plan_v[by * lc + bx] =
                    ((((128 * rr - 107 * g - 21 * b) + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        }
    }
}

/// Le canevas se présente directement comme source YUV de l'encodeur openh264
/// (aucune copie supplémentaire).
impl YUVSource for CanevasI420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.largeur, self.hauteur)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.largeur, self.largeur / 2, self.largeur / 2)
    }

    fn y(&self) -> &[u8] {
        &self.donnees[..self.largeur * self.hauteur]
    }

    fn u(&self) -> &[u8] {
        let y = self.largeur * self.hauteur;
        &self.donnees[y..y + (self.largeur / 2) * (self.hauteur / 2)]
    }

    fn v(&self) -> &[u8] {
        let y = self.largeur * self.hauteur;
        let c = (self.largeur / 2) * (self.hauteur / 2);
        &self.donnees[y + c..y + 2 * c]
    }
}

// ---------------------------------------------------------------------------
// Encodeur
// ---------------------------------------------------------------------------

/// Encodeur H.264 logiciel.
pub struct Openh264Encoder {
    /// Encodeur openh264 ; instancié à `configure` (dépend du débit/fps).
    inner: Option<Encoder>,
    /// Dernière configuration appliquée (le débit y est tenu à jour par
    /// [`VideoEncoder::set_target_bitrate`], pour l'observabilité).
    cfg: Option<EncoderConfig>,
    /// Canevas I420 persistant (conversion pleine ou par régions, voir doc).
    canevas: CanevasI420,
    /// État du mode delta (saut de trames, image-clé après repos).
    suivi: SuiviDelta,
    /// Débit demandé mais pas encore accepté par openh264 (l'encodeur interne ne
    /// s'initialise qu'au premier `encode` : une consigne posée avant est rejouée
    /// juste après — au pire une trame au débit précédent).
    debit_en_attente_kbps: Option<u32>,
}

impl Openh264Encoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: None,
            cfg: None,
            canevas: CanevasI420::vide(),
            suivi: SuiviDelta::new(),
            debit_en_attente_kbps: None,
        }
    }

    /// Applique la consigne de débit en attente via l'API FFI d'openh264
    /// (`ENCODER_OPTION_MAX_BITRATE` puis `ENCODER_OPTION_BITRATE`, couche
    /// `SPATIAL_LAYER_ALL`). Sans effet si l'encodeur interne n'est pas encore
    /// initialisé (openh264 répond `cmInitExpected` : la consigne reste en
    /// attente et sera rejouée après la première trame encodée).
    #[allow(unsafe_code)]
    fn appliquer_debit_en_attente(&mut self) {
        let Some(kbps) = self.debit_en_attente_kbps else {
            return;
        };
        let Some(enc) = self.inner.as_mut() else {
            return;
        };
        let bps = i32::try_from(u64::from(kbps).saturating_mul(1000))
            .unwrap_or(i32::MAX)
            .max(1);
        // Le max d'abord (autorise la montée), puis la cible : openh264 clippe la
        // cible sur le max courant.
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bps,
        };
        // SAFETY : `ENCODER_OPTION_(MAX_)BITRATE` attendent un `SBitrateInfo*`
        // valide le temps de l'appel ; `info` vit sur la pile pendant les deux
        // appels et openh264 n'en conserve pas le pointeur (copie des champs).
        // Ces options ne touchent pas aux paramètres dont la crate `openh264`
        // dépend (dimensions, format) — seule la consigne du contrôle de débit
        // change, ce qui est précisément l'usage documenté de l'API.
        let (res_max, res_cible): (c_int, c_int) = unsafe {
            let api = enc.raw_api();
            (
                api.set_option(
                    ENCODER_OPTION_MAX_BITRATE,
                    addr_of_mut!(info).cast::<c_void>(),
                ),
                api.set_option(ENCODER_OPTION_BITRATE, addr_of_mut!(info).cast::<c_void>()),
            )
        };
        if res_max == 0 && res_cible == 0 {
            self.debit_en_attente_kbps = None;
        }
    }
}

impl Default for Openh264Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoEncoder for Openh264Encoder {
    fn capabilities() -> CodecCaps {
        CodecCaps {
            hardware: false,
            kinds: vec![CodecKind::H264],
            max_width: 3840,
            max_height: 2160,
        }
    }

    fn configure(&mut self, cfg: EncoderConfig) -> Result<()> {
        if cfg.kind != CodecKind::H264 {
            return Err(NdError::Codec(
                "backend logiciel openh264 : H.264 uniquement".into(),
            ));
        }
        // `enable_skip_frame(true)` : sans lui, openh264 avertit explicitement que
        // « bitrate can't be controlled … without enabling skip frame » — le RC
        // ajuste le QP mais ne plafonne pas le débit. Avec lui, une trame en
        // dépassement devient un `FrameType::Skip` à 0 octet, c'est-à-dire
        // exactement notre **trame de répétition** (le décodeur renvoie `None`,
        // le viewer garde l'image précédente) : la consigne de l'ABR est tenue.
        let oh = OhConfig::new()
            .set_bitrate_bps(cfg.target_bitrate_kbps.saturating_mul(1000))
            .max_frame_rate(cfg.max_fps as f32)
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .enable_skip_frame(true);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), oh).map_err(codec_err)?;
        self.inner = Some(enc);
        self.cfg = Some(cfg);
        // Nouveau flux : canevas invalide (conversion pleine exigée), compteurs
        // delta remis à zéro, consigne de débit reprise de la configuration.
        self.canevas = CanevasI420::vide();
        self.suivi.reinitialiser();
        self.debit_en_attente_kbps = None;
        Ok(())
    }

    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedChunk> {
        if self.inner.is_none() {
            return Err(NdError::Codec(
                "encodeur non configuré (appeler configure)".into(),
            ));
        }

        let (l, h) = (frame.width as usize, frame.height as usize);
        if l == 0 || h == 0 || l % 2 != 0 || h % 2 != 0 {
            return Err(NdError::Codec(
                "dimensions impaires non supportées par H.264".into(),
            ));
        }
        let compatible = self.canevas.compatible(l, h);

        // 1. Saut d'encodage (mode delta) : rien n'a changé → trame de répétition
        //    à données vides, sans conversion ni passage par l'encodeur. Le
        //    décodeur ([`Openh264Decoder`]) la traite comme « pas de nouvelle
        //    image » et le viewer garde la précédente.
        if self.suivi.doit_sauter(frame, force_keyframe, compatible) {
            self.suivi.note_saut();
            return Ok(EncodedChunk {
                data: Vec::new(),
                is_keyframe: false,
                monitor: frame.monitor,
                timestamp_us: frame.timestamp_us,
            });
        }

        // 2. Dès qu'on encode réellement, il faut des pixels.
        let Some(FrameImage::Cpu { data, stride }) = frame.image.as_ref() else {
            return Err(NdError::Codec("frame sans pixels CPU à encoder".into()));
        };
        let stride = *stride;
        if stride < l * 4 || data.len() < stride * h {
            return Err(NdError::Codec(
                "taille de frame incohérente (attendu stride ≥ largeur*4 et stride*hauteur octets)"
                    .into(),
            ));
        }

        // 3. Conversion BGRA → I420 : restreinte aux régions modifiées si le mode
        //    delta est actif et le canevas à jour (liste vide = rien à reconvertir,
        //    ex. image-clé forcée sur écran statique) ; pleine sinon.
        let rects = if self.suivi.actif() && compatible {
            rects_pairs_bornes(&frame.dirty, frame.width, frame.height)
        } else {
            if !compatible {
                self.canevas.redimensionner(l, h);
            }
            vec![RectPair::plein(l, h)]
        };
        let aire_image = (l * h) as u64;
        let aire_modifiee = aire_totale(&rects, aire_image);
        for r in &rects {
            self.canevas.convertir_rect(data, stride, *r);
        }

        // 4. Image-clé : demandée par l'appelant, ou resynchronisation adaptative
        //    après une longue période statique (voir `delta`).
        let force_cle = force_keyframe
            || (self.suivi.actif() && self.suivi.keyframe_apres_repos(aire_modifiee, aire_image));

        // 5. Encodage du canevas (emprunts disjoints : encodeur + canevas).
        let Self { inner, canevas, .. } = self;
        let enc = inner
            .as_mut()
            .expect("encodeur vérifié en tête de fonction");
        if force_cle {
            enc.force_intra_frame();
        }
        let bitstream = enc.encode(&*canevas).map_err(codec_err)?;
        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let payload = bitstream.to_vec();

        self.suivi.note_encodage();
        // L'encodeur interne est désormais initialisé : rejoue une éventuelle
        // consigne de débit posée avant la première trame.
        self.appliquer_debit_en_attente();

        Ok(EncodedChunk {
            data: payload,
            is_keyframe,
            monitor: frame.monitor,
            timestamp_us: frame.timestamp_us,
        })
    }

    fn set_target_bitrate(&mut self, kbps: u32) {
        let kbps = kbps.max(1);
        if let Some(cfg) = self.cfg.as_mut() {
            cfg.target_bitrate_kbps = kbps;
        }
        self.debit_en_attente_kbps = Some(kbps);
        self.appliquer_debit_en_attente();
    }

    fn set_delta_mode(&mut self, actif: bool) {
        self.suivi.set_actif(actif);
    }

    fn nom_backend(&self) -> &str {
        "openh264 (encodeur H.264 logiciel)"
    }
}

// ---------------------------------------------------------------------------
// Décodeur
// ---------------------------------------------------------------------------

/// Décodeur H.264 logiciel (pour vérification et pour le côté viewer).
pub struct Openh264Decoder {
    inner: Decoder,
    /// Forçage local de la sortie NV12 : `Some(true/false)` court-circuite la
    /// préférence globale [`crate::sortie_nv12_preferee`] (utilisé par les
    /// tests, qui tournent en parallèle et ne doivent pas se disputer l'état
    /// global) ; `None` (défaut) = suivre la préférence globale, relue **à
    /// chaque trame** (bascule à chaud quand la texture GPU s'attache).
    sortie_nv12: Option<bool>,
}

impl Openh264Decoder {
    pub fn new() -> Result<Self> {
        let dec = Decoder::new().map_err(codec_err)?;
        Ok(Self {
            inner: dec,
            sortie_nv12: None,
        })
    }

    /// Force (ou libère, avec `None`) la sortie NV12 de **ce** décodeur,
    /// indépendamment de la préférence globale. Voir le champ `sortie_nv12`.
    ///
    /// **Réservé aux tests** : la production pilote la sortie NV12 par la
    /// préférence globale du processus ([`crate::preferer_sortie_nv12`], posée par
    /// la couche d'affichage quand une texture GPU D3D11 est vivante) ; ce forçage
    /// par décodeur n'existe que pour isoler les tests parallèles de l'état global.
    #[cfg(test)]
    pub fn regler_sortie_nv12(&mut self, forcee: Option<bool>) {
        self.sortie_nv12 = forcee;
    }
}

impl VideoDecoder for Openh264Decoder {
    fn decode(&mut self, chunk: &EncodedChunk) -> Result<Option<DecodedFrame>> {
        // Trame de répétition (encodage delta, données vides) : pas de nouvelle
        // image — l'appelant conserve la précédente. On ne pousse pas un tampon
        // vide dans openh264.
        if chunk.data.is_empty() {
            return Ok(None);
        }
        let nv12_voulu = self.sortie_nv12.unwrap_or_else(crate::sortie_nv12_preferee);
        // Le rendu de la YUV décodée est confié à l'UI (voir plan 10) : RGBA
        // (chemin CPU historique) ou NV12 (chemin texture GPU D3D11 — simple
        // re-empaquetage des plans, la conversion couleur part sur le GPU).
        match self.inner.decode(&chunk.data).map_err(codec_err)? {
            Some(yuv) => {
                let (w, h) = yuv.dimensions();
                // NV12 exige des dimensions paires (garanties par nos encodeurs) ;
                // par prudence, une image impaire reste sur le chemin RGBA.
                if nv12_voulu && w % 2 == 0 && h % 2 == 0 {
                    let nv12 =
                        crate::nv12::i420_vers_nv12(yuv.y(), yuv.u(), yuv.v(), yuv.strides(), w, h);
                    return Ok(Some(DecodedFrame {
                        width: w as u32,
                        height: h as u32,
                        rgba: Vec::new(),
                        nv12: Some(nv12),
                    }));
                }
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some(DecodedFrame {
                    width: w as u32,
                    height: h as u32,
                    rgba,
                    nv12: None,
                }))
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::SAUTS_AVANT_RESYNC;
    use crate::{create_decoder, psnr_luma};
    use nd_capture::Rect;
    use nd_proto::MonitorId;

    /// Générateur pseudo-aléatoire déterministe (xorshift32) — pas de dépendance.
    fn xorshift32(etat: &mut u32) -> u32 {
        *etat ^= *etat << 13;
        *etat ^= *etat >> 17;
        *etat ^= *etat << 5;
        *etat
    }

    /// Frame BGRA texturée déterministe : dégradé + damier + bruit modéré changeant
    /// à chaque frame. Compressible sur une large plage de QP — le contrôle de
    /// débit a donc une vraie marge de manœuvre (du bruit pur saturerait le QP
    /// maximal quel que soit le débit cible et masquerait l'effet de la consigne).
    fn frame_texturee(w: u32, h: u32, graine: u32, index: u32) -> CapturedFrame {
        let mut etat = (graine ^ index.wrapping_mul(0x9E37_79B9)) | 1;
        let stride = w as usize * 4;
        let mut data = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * stride + x * 4;
                let damier = if (x / 8 + y / 8) % 2 == 0 { 24i32 } else { 0 };
                let bruit = (xorshift32(&mut etat) % 33) as i32 - 16;
                let b = ((x * 200 / w as usize) as i32 + damier + bruit).clamp(0, 255) as u8;
                data[o] = b;
                data[o + 1] = ((y * 200 / h as usize) as i32 + bruit).clamp(0, 255) as u8;
                data[o + 2] = b / 2 + 60;
                data[o + 3] = 255;
            }
        }
        frame_de(w, h, data, vec![Rect { x: 0, y: 0, w, h }], index)
    }

    /// Frame BGRA unie (aplat), avec régions modifiées explicites.
    fn frame_aplat(w: u32, h: u32, bgr: [u8; 3], dirty: Vec<Rect>, index: u32) -> CapturedFrame {
        let stride = w as usize * 4;
        let mut data = vec![0u8; stride * h as usize];
        for px in data.chunks_exact_mut(4) {
            px[..3].copy_from_slice(&bgr);
            px[3] = 255;
        }
        frame_de(w, h, data, dirty, index)
    }

    fn frame_de(w: u32, h: u32, data: Vec<u8>, dirty: Vec<Rect>, index: u32) -> CapturedFrame {
        CapturedFrame {
            width: w,
            height: h,
            monitor: MonitorId(0),
            format: nd_capture::PixelFormat::Bgra8,
            dirty,
            cursor: None,
            timestamp_us: u64::from(index) * 33_333,
            image: Some(FrameImage::Cpu {
                data,
                stride: w as usize * 4,
            }),
        }
    }

    fn config(w: u32, h: u32, kbps: u32) -> EncoderConfig {
        EncoderConfig {
            kind: CodecKind::H264,
            width: w,
            height: h,
            target_bitrate_kbps: kbps,
            max_fps: 30,
        }
    }

    /// Encode `n` frames texturées déterministes et renvoie le total d'octets H.264.
    fn octets_sequence_texturee(enc: &mut Openh264Encoder, n: u32, graine: u32) -> u64 {
        let mut total = 0u64;
        for i in 0..n {
            let frame = frame_texturee(320, 240, graine, i);
            let chunk = enc.encode(&frame, i == 0).expect("encode");
            total += chunk.data.len() as u64;
        }
        total
    }

    /// `set_target_bitrate` est réel : à séquence identique, un débit cible 10×
    /// plus bas produit un flux nettement plus petit (le no-op historique aurait
    /// donné des tailles identiques).
    #[test]
    fn set_target_bitrate_fait_varier_le_flux() {
        let mut enc = Openh264Encoder::new();

        enc.configure(config(320, 240, 1_000)).expect("configure");
        enc.set_target_bitrate(2_000);
        let octets_haut = octets_sequence_texturee(&mut enc, 30, 0xC0FF_EE01);

        enc.configure(config(320, 240, 1_000)).expect("configure");
        enc.set_target_bitrate(200);
        let octets_bas = octets_sequence_texturee(&mut enc, 30, 0xC0FF_EE01);

        assert!(
            octets_haut as f64 >= octets_bas as f64 * 2.0,
            "le flux doit suivre la consigne de débit (2 000 kbit/s → {octets_haut} o, \
             200 kbit/s → {octets_bas} o)"
        );
    }

    /// La consigne posée AVANT la première trame (encodeur interne pas encore
    /// initialisé) est rejouée après : les trames suivantes la respectent.
    #[test]
    fn set_target_bitrate_avant_premiere_trame() {
        let mut enc = Openh264Encoder::new();
        enc.configure(config(320, 240, 4_000)).expect("configure");
        enc.set_target_bitrate(150); // avant tout encode
        let octets = octets_sequence_texturee(&mut enc, 30, 0xBEEF_0002);

        let mut enc_haut = Openh264Encoder::new();
        enc_haut
            .configure(config(320, 240, 4_000))
            .expect("configure");
        let octets_haut = octets_sequence_texturee(&mut enc_haut, 30, 0xBEEF_0002);

        assert!(
            octets_haut as f64 >= octets as f64 * 2.0,
            "consigne pré-encodage ignorée ? (4 000 kbit/s → {octets_haut} o, \
             150 kbit/s posés avant la 1re trame → {octets} o)"
        );
        // La configuration reflète la consigne (observabilité).
        assert_eq!(enc.cfg.expect("cfg").target_bitrate_kbps, 150);
    }

    /// Mode delta : une frame sans région modifiée est sautée (chunk vide, pas
    /// d'image-clé) ; le décodeur la traite comme « pas de nouvelle image ».
    /// Sans mode delta, la même séquence ré-encode plein cadre (comportement
    /// historique conservé).
    #[test]
    fn delta_saute_les_trames_sans_changement() {
        let (w, h) = (64u32, 64u32);
        let mut enc = Openh264Encoder::new();
        enc.set_delta_mode(true);
        enc.configure(config(w, h, 2_000)).expect("configure");

        let pleine = frame_aplat(w, h, [40, 80, 120], vec![Rect { x: 0, y: 0, w, h }], 0);
        let premiere = enc.encode(&pleine, true).expect("première frame");
        assert!(premiere.is_keyframe);
        assert!(!premiere.data.is_empty());

        let statique = frame_aplat(w, h, [40, 80, 120], Vec::new(), 1);
        let saut = enc.encode(&statique, false).expect("saut");
        assert!(saut.data.is_empty(), "trame de répétition attendue");
        assert!(!saut.is_keyframe);

        let mut dec = create_decoder(CodecKind::H264).expect("décodeur");
        dec.decode(&premiere).expect("flux valide");
        assert!(
            dec.decode(&saut).expect("répétition acceptée").is_none(),
            "une trame de répétition ne produit pas d'image"
        );

        // Même séquence sans mode delta : pas de saut (comportement historique).
        let mut enc_plein = Openh264Encoder::new();
        enc_plein.configure(config(w, h, 2_000)).expect("configure");
        enc_plein.encode(&pleine, true).expect("première");
        let chunk = enc_plein.encode(&statique, false).expect("re-encode");
        assert!(
            !chunk.data.is_empty(),
            "sans mode delta, la frame est ré-encodée plein cadre"
        );
    }

    /// Mode delta : une frame `image: None` + `dirty` vide (délai de capture sans
    /// changement, cas DXGI) devient une trame de répétition au lieu d'une erreur.
    #[test]
    fn delta_accepte_les_frames_sans_pixels() {
        let (w, h) = (64u32, 64u32);
        let mut enc = Openh264Encoder::new();
        enc.set_delta_mode(true);
        enc.configure(config(w, h, 2_000)).expect("configure");
        let pleine = frame_aplat(w, h, [10, 20, 30], vec![Rect { x: 0, y: 0, w, h }], 0);
        enc.encode(&pleine, true).expect("première frame");

        let mut vide = frame_aplat(w, h, [0, 0, 0], Vec::new(), 1);
        vide.image = None;
        let saut = enc.encode(&vide, false).expect("répétition");
        assert!(saut.data.is_empty());

        // Sans mode delta, l'absence de pixels reste une erreur (contrat inchangé).
        let mut enc_plein = Openh264Encoder::new();
        enc_plein.configure(config(w, h, 2_000)).expect("configure");
        assert!(enc_plein.encode(&vide, false).is_err());
    }

    /// Mode delta : la conversion restreinte aux régions modifiées produit une
    /// image finale fidèle (le canevas intègre le patch, le reste est conservé).
    #[test]
    fn delta_conversion_partielle_fidele() {
        let (w, h) = (64u32, 64u32);
        let mut enc = Openh264Encoder::new();
        enc.set_delta_mode(true);
        enc.configure(config(w, h, 4_000)).expect("configure");
        let mut dec = create_decoder(CodecKind::H264).expect("décodeur");

        // Frame 1 : fond uni, conversion pleine.
        let fond = frame_aplat(w, h, [200, 200, 200], vec![Rect { x: 0, y: 0, w, h }], 0);
        let c1 = enc.encode(&fond, true).expect("frame 1");
        dec.decode(&c1).expect("décodage 1");

        // Frame 2 : un carré sombre 16×16 en (24, 24), seule région annoncée.
        let mut patch = frame_aplat(
            w,
            h,
            [200, 200, 200],
            vec![Rect {
                x: 24,
                y: 24,
                w: 16,
                h: 16,
            }],
            1,
        );
        let Some(FrameImage::Cpu { data, stride }) = patch.image.as_mut() else {
            unreachable!()
        };
        for y in 24..40usize {
            for x in 24..40usize {
                let o = y * *stride + x * 4;
                data[o..o + 4].copy_from_slice(&[20, 20, 20, 255]);
            }
        }
        let attendu_rgba: Vec<u8> = data
            .chunks_exact(4)
            .flat_map(|px| [px[2], px[1], px[0], px[3]])
            .collect();

        let c2 = enc.encode(&patch, false).expect("frame 2");
        assert!(!c2.data.is_empty(), "une région modifiée doit être encodée");
        // Quelques trames identiques de plus pour laisser le décodeur produire
        // l'image la plus récente, puis comparaison à la source.
        let mut derniere = dec.decode(&c2).expect("décodage 2");
        for i in 2..6u32 {
            let suite = frame_aplat(w, h, [0, 0, 0], Vec::new(), i); // sautée
            let chunk = enc.encode(&suite, false).expect("saut");
            assert!(chunk.data.is_empty());
            let encore = frame_repeat_reencode(&mut enc, &patch, i);
            if let Some(img) = dec.decode(&encore).expect("décodage") {
                derniere = Some(img);
            }
        }
        let image = derniere.expect("au moins une image décodée");
        assert_eq!((image.width, image.height), (w, h));
        let psnr = psnr_luma(&attendu_rgba, &image.rgba).expect("psnr");
        assert!(
            psnr > 30.0,
            "l'image reconstruite doit être fidèle (PSNR luma = {psnr:.1} dB)"
        );
    }

    /// Ré-encode `patch` en annonçant sa région modifiée (aide du test ci-dessus :
    /// le canevas ne change pas, mais l'encodeur produit une trame P décodable).
    fn frame_repeat_reencode(
        enc: &mut Openh264Encoder,
        patch: &CapturedFrame,
        index: u32,
    ) -> EncodedChunk {
        let mut f = patch.clone();
        f.timestamp_us = u64::from(index) * 33_333;
        enc.encode(&f, false).expect("ré-encodage")
    }

    /// Image-clé adaptative : après une longue période statique (sauts), un
    /// changement plein écran est encodé en image-clé (resynchronisation).
    #[test]
    fn delta_keyframe_apres_repos() {
        let (w, h) = (64u32, 64u32);
        let mut enc = Openh264Encoder::new();
        enc.set_delta_mode(true);
        enc.configure(config(w, h, 2_000)).expect("configure");

        let fond = frame_aplat(w, h, [64, 64, 64], vec![Rect { x: 0, y: 0, w, h }], 0);
        enc.encode(&fond, true).expect("première frame");

        for i in 0..SAUTS_AVANT_RESYNC {
            let statique = frame_aplat(w, h, [64, 64, 64], Vec::new(), i + 1);
            let chunk = enc.encode(&statique, false).expect("saut");
            assert!(chunk.data.is_empty());
        }

        let bascule = frame_aplat(
            w,
            h,
            [220, 30, 30],
            vec![Rect { x: 0, y: 0, w, h }],
            SAUTS_AVANT_RESYNC + 1,
        );
        let chunk = enc.encode(&bascule, false).expect("bascule");
        assert!(
            chunk.is_keyframe,
            "après {SAUTS_AVANT_RESYNC} sauts, un changement plein écran doit \
             produire une image-clé de resynchronisation"
        );
    }

    /// Un chunk à données vides (trame de répétition) est accepté par le décodeur
    /// et ne produit pas d'image — y compris en tout début de flux.
    #[test]
    fn decodeur_accepte_chunk_vide() {
        let mut dec = Openh264Decoder::new().expect("décodeur");
        let chunk = EncodedChunk {
            data: Vec::new(),
            is_keyframe: false,
            monitor: MonitorId(0),
            timestamp_us: 0,
        };
        assert!(dec.decode(&chunk).expect("répétition acceptée").is_none());
    }

    /// Sortie NV12 (chemin texture GPU D3D11) : le même flux décodé en NV12 puis
    /// converti par le repli CPU ([`crate::nv12_vers_rgba`]) est fidèle à la
    /// sortie RGBA historique (les deux ne diffèrent que par l'arrondi des deux
    /// conversions YUV→RGB — PSNR élevé exigé). Le forçage local évite l'état
    /// global (tests parallèles).
    #[test]
    fn decodeur_sortie_nv12_fidele_au_rgba() {
        let (w, h) = (320u32, 240u32);
        let mut enc = Openh264Encoder::new();
        enc.configure(config(w, h, 4_000)).expect("configure");
        let chunk = enc
            .encode(&frame_texturee(w, h, 0xABCD_1234, 0), true)
            .expect("encode");

        let mut dec_rgba = Openh264Decoder::new().expect("décodeur RGBA");
        dec_rgba.regler_sortie_nv12(Some(false));
        let reference = dec_rgba
            .decode(&chunk)
            .expect("décodage RGBA")
            .expect("image RGBA");
        assert!(!reference.rgba.is_empty());
        assert!(reference.nv12.is_none());

        let mut dec_nv12 = Openh264Decoder::new().expect("décodeur NV12");
        dec_nv12.regler_sortie_nv12(Some(true));
        let image = dec_nv12
            .decode(&chunk)
            .expect("décodage NV12")
            .expect("image NV12");
        assert!(image.rgba.is_empty(), "la sortie NV12 ne porte pas de RGBA");
        let nv12 = image.nv12.as_ref().expect("tampon NV12");
        assert_eq!(nv12.len(), (w * h + w * h / 2) as usize, "taille NV12");

        let rgba = image.en_rgba();
        assert_eq!(rgba.len(), reference.rgba.len());
        let psnr = psnr_luma(&reference.rgba, &rgba).expect("psnr");
        assert!(
            psnr > 40.0,
            "NV12 + repli CPU doit rester fidèle au RGBA direct (PSNR = {psnr:.1} dB)"
        );
    }
}
