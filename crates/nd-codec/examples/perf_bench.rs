//! Banc de performance nd-codec — boucle ABR + encodage delta (plan 03/04/14).
//!
//! Mesure sur une **séquence synthétique déterministe** (graine fixe, aucune
//! dépendance à l'écran réel) l'encodeur H.264 **logiciel** (openh264 — NVENC est
//! un lot ultérieur, hors périmètre) :
//!
//! - **Scénario A — écran statique** : la même image, régions modifiées vides.
//!   Avant/après : delta OFF (plein cadre ré-encodé, comportement historique)
//!   contre delta ON (trames de répétition sautées).
//! - **Scénario B — petit mouvement** : un carré 96×96 se déplace, `dirty` exact.
//!   Le delta restreint la conversion couleur à la surface annoncée.
//! - **Scénario C — mouvement plein cadre** : moitié basse « vidéo » texturée
//!   changeant à chaque frame (référence charge maximale, et support des mesures
//!   qualité PSNR/SSIM et de la boucle ABR).
//! - **ABR en boucle fermée** : `RateController` reçoit des estimations réseau et
//!   pilote `set_target_bitrate` ; on mesure le débit **réellement produit**.
//!
//! ## Méthodologie (chiffres honnêtes)
//!
//! - Contenu généré **hors chronométrage** : seuls les appels `encode()` sont
//!   mesurés (`Instant::now()` autour du seul appel).
//! - Moyennes d'octets **hors première trame** (l'image-clé initiale est
//!   rapportée à part) ; débits mesurés = octets produits × 8 × fps / n / 1000 à
//!   cadence simulée de 30 fps.
//! - Les temps dépendent de la machine ; les **assertions** ne portent que sur
//!   des rapports très larges (×4) ou sur des octets (déterministes).
//! - PSNR/SSIM : image décodée par le décodeur openh264 de la crate, comparée à
//!   la source RGBA de la même frame (métriques de `nd_codec::metrics`).
//!
//! Lancer : `cargo run --example perf_bench -p nd-codec --release`

use std::time::Instant;

use nd_capture::{CapturedFrame, FrameImage, PixelFormat, Rect};
use nd_codec::{
    create_decoder, create_encoder, psnr_luma, ssim_luma, CodecKind, ContentProfile, EncoderConfig,
    NetworkEstimate, RateController, VideoEncoder,
};
use nd_proto::MonitorId;

const LARGEUR: u32 = 1280;
const HAUTEUR: u32 = 720;
const FPS_SIMULE: u32 = 30;
const GRAINE: u32 = 0x5EED_CAFE;

// ---------------------------------------------------------------------------
// Génération de contenu déterministe
// ---------------------------------------------------------------------------

fn xorshift32(etat: &mut u32) -> u32 {
    *etat ^= *etat << 13;
    *etat ^= *etat >> 17;
    *etat ^= *etat << 5;
    *etat
}

/// Peintre BGRA réutilisable : « bureau » synthétique (dégradé + bandes de texte
/// simulé), carré mobile, zone vidéo texturée.
struct Peintre {
    data: Vec<u8>,
}

impl Peintre {
    fn nouveau() -> Self {
        let mut p = Self {
            data: vec![0u8; (LARGEUR * HAUTEUR * 4) as usize],
        };
        p.fond();
        p
    }

    /// Fond « bureau » : dégradé doux + fines lignes contrastées (texte simulé)
    /// dans la moitié haute. Entièrement déterministe.
    fn fond(&mut self) {
        let stride = LARGEUR as usize * 4;
        for y in 0..HAUTEUR as usize {
            for x in 0..LARGEUR as usize {
                let o = y * stride + x * 4;
                // « Texte » : lignes horizontales sombres périodiques sur fond clair.
                let texte = y < 360 && (y % 14) < 3 && (x / 6 + y / 14) % 5 != 0;
                let (b, g, r) = if texte {
                    (30u8, 30u8, 30u8)
                } else {
                    (
                        (200 - (y * 60) / HAUTEUR as usize) as u8,
                        (210 - (x * 40) / LARGEUR as usize) as u8,
                        220u8,
                    )
                };
                self.data[o] = b;
                self.data[o + 1] = g;
                self.data[o + 2] = r;
                self.data[o + 3] = 255;
            }
        }
    }

    /// Dessine un carré plein de côté `cote` en (`x`, `y`).
    fn carre(&mut self, x: usize, y: usize, cote: usize, bgr: [u8; 3]) {
        let stride = LARGEUR as usize * 4;
        for dy in 0..cote {
            for dx in 0..cote {
                let o = (y + dy) * stride + (x + dx) * 4;
                self.data[o..o + 3].copy_from_slice(&bgr);
                self.data[o + 3] = 255;
            }
        }
    }

    /// Remplit la moitié basse (« zone vidéo », 1280×360) d'une texture bruitée
    /// dépendant de `index` : contenu difficile mais compressible, qui change à
    /// chaque frame — le contrôle de débit est réellement contraint.
    fn zone_video(&mut self, index: u32) {
        let stride = LARGEUR as usize * 4;
        let mut etat = (GRAINE ^ index.wrapping_mul(0x9E37_79B9)) | 1;
        for y in 360..HAUTEUR as usize {
            for x in 0..LARGEUR as usize {
                let o = y * stride + x * 4;
                let bruit = (xorshift32(&mut etat) % 49) as i32 - 24;
                let base = ((x + y + index as usize * 4) % 220) as i32;
                let v = (base + bruit).clamp(0, 255) as u8;
                self.data[o] = v;
                self.data[o + 1] = (base / 2 + 40 + bruit / 2).clamp(0, 255) as u8;
                self.data[o + 2] = (220 - base / 2).clamp(0, 255) as u8;
                self.data[o + 3] = 255;
            }
        }
    }

    /// Émet une `CapturedFrame` (copie des pixels courants) avec les régions
    /// modifiées annoncées `dirty`.
    fn frame(&self, dirty: Vec<Rect>, index: u32) -> CapturedFrame {
        CapturedFrame {
            width: LARGEUR,
            height: HAUTEUR,
            monitor: MonitorId(0),
            format: PixelFormat::Bgra8,
            dirty,
            cursor: None,
            timestamp_us: u64::from(index) * 1_000_000 / u64::from(FPS_SIMULE),
            image: Some(FrameImage::Cpu {
                data: self.data.clone(),
                stride: LARGEUR as usize * 4,
            }),
        }
    }

    /// Source RGBA (pour PSNR/SSIM : le décodeur produit du RGBA).
    fn rgba(&self) -> Vec<u8> {
        self.data
            .chunks_exact(4)
            .flat_map(|px| [px[2], px[1], px[0], px[3]])
            .collect()
    }
}

fn rect_plein() -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: LARGEUR,
        h: HAUTEUR,
    }
}

// ---------------------------------------------------------------------------
// Mesure
// ---------------------------------------------------------------------------

/// Statistiques d'une passe d'encodage.
#[derive(Default)]
struct Stats {
    trames: u32,
    /// Octets de la première trame (image-clé initiale), rapportée à part.
    octets_premiere: u64,
    /// Octets des trames suivantes.
    octets_suite: u64,
    /// Trames de répétition (données vides) parmi les suivantes.
    sautees: u32,
    /// Temps cumulé passé dans `encode()` (toutes trames), en nanosecondes.
    duree_encode_ns: u128,
    /// Pire temps d'un `encode()`, en nanosecondes.
    pire_encode_ns: u128,
}

impl Stats {
    fn ajoute(&mut self, chunk_len: usize, duree_ns: u128) {
        if self.trames == 0 {
            self.octets_premiere = chunk_len as u64;
        } else {
            self.octets_suite += chunk_len as u64;
            if chunk_len == 0 {
                self.sautees += 1;
            }
        }
        self.trames += 1;
        self.duree_encode_ns += duree_ns;
        self.pire_encode_ns = self.pire_encode_ns.max(duree_ns);
    }

    fn octets_par_trame_suite(&self) -> f64 {
        if self.trames <= 1 {
            return 0.0;
        }
        self.octets_suite as f64 / f64::from(self.trames - 1)
    }

    fn ms_par_trame(&self) -> f64 {
        if self.trames == 0 {
            return 0.0;
        }
        self.duree_encode_ns as f64 / f64::from(self.trames) / 1e6
    }

    fn fps_encodage(&self) -> f64 {
        let ms = self.ms_par_trame();
        if ms <= 0.0 {
            0.0
        } else {
            1000.0 / ms
        }
    }

    fn kbps_mesure(&self) -> f64 {
        if self.trames <= 1 {
            return 0.0;
        }
        self.octets_par_trame_suite() * 8.0 * f64::from(FPS_SIMULE) / 1000.0
    }

    fn imprime(&self, etiquette: &str) {
        println!(
            "  {etiquette:<34} {:>9.1} o/trame | {:>7.3} ms/trame (max {:>7.3}) | \
             {:>6.0} fps encodage | {:>8.0} kbit/s | 1re (clé) {:>7} o | sautées {:>3}/{}",
            self.octets_par_trame_suite(),
            self.ms_par_trame(),
            self.pire_encode_ns as f64 / 1e6,
            self.fps_encodage(),
            self.kbps_mesure(),
            self.octets_premiere,
            self.sautees,
            self.trames.saturating_sub(1),
        );
    }
}

/// Encode une trame en chronométrant le seul appel `encode()`.
fn encode_chrono(
    enc: &mut dyn VideoEncoder,
    frame: &CapturedFrame,
    force_cle: bool,
    stats: &mut Stats,
) -> Vec<u8> {
    let debut = Instant::now();
    let chunk = enc.encode(frame, force_cle).expect("encode");
    stats.ajoute(chunk.data.len(), debut.elapsed().as_nanos());
    chunk.data
}

fn encodeur_configure(kbps: u32, delta: bool) -> Box<dyn VideoEncoder> {
    let mut enc = create_encoder(CodecKind::H264).expect("encodeur H.264 logiciel");
    enc.set_delta_mode(delta);
    enc.configure(EncoderConfig {
        kind: CodecKind::H264,
        width: LARGEUR,
        height: HAUTEUR,
        target_bitrate_kbps: kbps,
        max_fps: FPS_SIMULE,
    })
    .expect("configure");
    enc
}

// ---------------------------------------------------------------------------
// Scénarios
// ---------------------------------------------------------------------------

/// Scénario A — écran statique : 1 trame pleine puis `n − 1` trames identiques
/// (`dirty` vide).
fn scenario_statique(delta: bool, n: u32) -> Stats {
    let mut enc = encodeur_configure(4_000, delta);
    let peintre = Peintre::nouveau();
    let mut stats = Stats::default();
    for i in 0..n {
        let dirty = if i == 0 {
            vec![rect_plein()]
        } else {
            Vec::new()
        };
        let frame = peintre.frame(dirty, i);
        encode_chrono(enc.as_mut(), &frame, i == 0, &mut stats);
    }
    stats
}

/// Scénario B — petit mouvement : un carré 96×96 se déplace (trajectoire
/// déterministe), `dirty` = ancienne + nouvelle position.
fn scenario_petit_mouvement(delta: bool, n: u32) -> Stats {
    const COTE: u32 = 96;
    let pos = |i: u32| -> (u32, u32) {
        (
            (i * 37) % (LARGEUR - COTE),
            120 + (i * 23) % (HAUTEUR - COTE - 120),
        )
    };
    let mut enc = encodeur_configure(4_000, delta);
    let mut peintre = Peintre::nouveau();
    let mut stats = Stats::default();
    for i in 0..n {
        peintre.fond(); // efface l'ancienne position
        let (x, y) = pos(i);
        peintre.carre(x as usize, y as usize, COTE as usize, [20, 60, 200]);
        let dirty = if i == 0 {
            vec![rect_plein()]
        } else {
            let (ax, ay) = pos(i - 1);
            vec![
                Rect {
                    x: ax,
                    y: ay,
                    w: COTE,
                    h: COTE,
                },
                Rect {
                    x,
                    y,
                    w: COTE,
                    h: COTE,
                },
            ]
        };
        let frame = peintre.frame(dirty, i);
        encode_chrono(enc.as_mut(), &frame, i == 0, &mut stats);
    }
    stats
}

/// Paire de mesure qualité : (source RGBA, image décodée RGBA) de la même trame.
type PaireQualite = (Vec<u8>, Vec<u8>);

/// Scénario C — mouvement soutenu : la moitié basse change à chaque frame.
/// Renvoie aussi la dernière paire (source RGBA, image décodée) pour la qualité.
fn scenario_video(kbps: u32, n: u32) -> (Stats, Option<PaireQualite>) {
    let mut enc = encodeur_configure(kbps, false);
    let mut dec = create_decoder(CodecKind::H264).expect("décodeur");
    let mut peintre = Peintre::nouveau();
    let mut stats = Stats::default();
    let mut derniere_paire: Option<PaireQualite> = None;
    for i in 0..n {
        peintre.zone_video(i);
        let frame = peintre.frame(vec![rect_plein()], i);
        let donnees = encode_chrono(enc.as_mut(), &frame, i == 0, &mut stats);
        let chunk = nd_codec::EncodedChunk {
            data: donnees,
            is_keyframe: i == 0,
            monitor: MonitorId(0),
            timestamp_us: frame.timestamp_us,
        };
        if let Some(img) = dec.decode(&chunk).expect("flux valide") {
            derniere_paire = Some((peintre.rgba(), img.rgba));
        }
    }
    (stats, derniere_paire)
}

/// Boucle ABR fermée : le contrôleur reçoit les estimations réseau et pilote le
/// débit de l'encodeur ; on mesure le débit réellement produit par phase.
fn scenario_abr() {
    let base = EncoderConfig {
        kind: CodecKind::H264,
        width: LARGEUR,
        height: HAUTEUR,
        target_bitrate_kbps: 4_000,
        max_fps: FPS_SIMULE,
    };
    let mut enc = create_encoder(CodecKind::H264).expect("encodeur");
    enc.configure(base).expect("configure");
    let mut rc = RateController::new(base, ContentProfile::Video);
    let mut peintre = Peintre::nouveau();

    println!("\n=== ABR en boucle fermée (RateController → set_target_bitrate) ===");
    println!(
        "  base 4 000 kbit/s @ {LARGEUR}x{HAUTEUR} {FPS_SIMULE} fps, profil Vidéo ; \
         20 trames par phase, estimation appliquée à chaque trame"
    );

    let phases: [(&str, NetworkEstimate); 7] = [
        (
            "sain      20 Mbit/s   20 ms  0 %",
            NetworkEstimate {
                bandwidth_kbps: 20_000,
                rtt_ms: 20,
                loss: 0.0,
            },
        ),
        (
            "dégradé  2,5 Mbit/s   60 ms  0 %",
            NetworkEstimate {
                bandwidth_kbps: 2_500,
                rtt_ms: 60,
                loss: 0.0,
            },
        ),
        (
            "mauvais  600 kbit/s  200 ms  2 %",
            NetworkEstimate {
                bandwidth_kbps: 600,
                rtt_ms: 200,
                loss: 0.02,
            },
        ),
        (
            "effondré 250 kbit/s  300 ms  8 %",
            NetworkEstimate {
                bandwidth_kbps: 250,
                rtt_ms: 300,
                loss: 0.08,
            },
        ),
        (
            "rétabli   20 Mbit/s   20 ms  0 %",
            NetworkEstimate {
                bandwidth_kbps: 20_000,
                rtt_ms: 20,
                loss: 0.0,
            },
        ),
        (
            "rétabli   20 Mbit/s   20 ms  0 %",
            NetworkEstimate {
                bandwidth_kbps: 20_000,
                rtt_ms: 20,
                loss: 0.0,
            },
        ),
        (
            "rétabli   20 Mbit/s   20 ms  0 %",
            NetworkEstimate {
                bandwidth_kbps: 20_000,
                rtt_ms: 20,
                loss: 0.0,
            },
        ),
    ];

    let mut index = 0u32;
    let mut kbps_par_phase = Vec::new();
    let mut palier_max = 0usize;
    for (etiquette, estimation) in phases {
        let mut octets = 0u64;
        let mut trames = 0u32;
        let mut cible = rc.current_config();
        for _ in 0..20 {
            cible = rc.apply_network_estimate(enc.as_mut(), estimation);
            peintre.zone_video(index);
            let frame = peintre.frame(vec![rect_plein()], index);
            let chunk = enc.encode(&frame, index == 0).expect("encode");
            // La 1re trame du banc (image-clé) est exclue de la mesure de débit.
            if index > 0 {
                octets += chunk.data.len() as u64;
                trames += 1;
            }
            index += 1;
        }
        let kbps = octets as f64 * 8.0 * f64::from(FPS_SIMULE) / f64::from(trames.max(1)) / 1000.0;
        palier_max = palier_max.max(rc.palier());
        println!(
            "  {etiquette} → palier {} | consigne {:>4} kbit/s | recommandation {:>4}x{:<4} @ {:>2} fps | mesuré {:>6.0} kbit/s",
            rc.palier(),
            cible.target_bitrate_kbps,
            cible.width,
            cible.height,
            cible.max_fps,
            kbps,
        );
        kbps_par_phase.push((kbps, rc.palier(), cible.target_bitrate_kbps));
    }

    // Garde-fous : l'ABR fait réellement varier le débit produit, dans les deux sens.
    let (kbps_sain, _, _) = kbps_par_phase[0];
    let (kbps_effondre, palier_effondre, consigne_effondre) = kbps_par_phase[3];
    let (_, palier_final, consigne_finale) = kbps_par_phase[kbps_par_phase.len() - 1];
    assert_eq!(consigne_effondre, 480, "palier plancher = 12 % de 4 000");
    assert!(
        palier_effondre == 4,
        "l'effondrement doit atteindre le plancher"
    );
    assert!(
        kbps_effondre * 3.0 < kbps_sain,
        "le débit mesuré doit suivre la consigne à la baisse \
         (sain {kbps_sain:.0} kbit/s, effondré {kbps_effondre:.0} kbit/s)"
    );
    assert_eq!(
        palier_final, 0,
        "le retour au calme doit remonter au plein régime"
    );
    assert_eq!(consigne_finale, 4_000);
    assert_eq!(palier_max, 4);
    println!(
        "  OK : débit mesuré {kbps_sain:.0} → {kbps_effondre:.0} kbit/s à l'effondrement, \
         retour palier 0 après rétablissement (hystérésis)."
    );
}

// ---------------------------------------------------------------------------
// Programme
// ---------------------------------------------------------------------------

fn main() {
    println!("Banc nd-codec — encodeur H.264 LOGICIEL openh264 (NVENC : lot ultérieur).");
    println!(
        "Séquence déterministe {LARGEUR}x{HAUTEUR} (graine 0x{GRAINE:08X}), cadence simulée \
         {FPS_SIMULE} fps ; seuls les appels encode() sont chronométrés.\n"
    );

    const N: u32 = 60;

    // --- Scénario A : écran statique --------------------------------------
    println!("=== Scénario A — écran statique ({N} trames identiques, dirty vide) ===");
    let a_off = scenario_statique(false, N);
    a_off.imprime("delta OFF (avant, plein cadre) :");
    let a_on = scenario_statique(true, N);
    a_on.imprime("delta ON  (après, répétitions) :");

    assert_eq!(
        a_on.octets_suite, 0,
        "écran statique + delta : plus un octet après l'image-clé"
    );
    assert_eq!(
        a_on.sautees,
        N - 1,
        "toutes les trames suivantes sont sautées"
    );
    assert!(
        a_off.octets_suite > 0,
        "référence : le plein cadre coûte des octets"
    );
    assert!(
        a_on.ms_par_trame() * 4.0 < a_off.ms_par_trame(),
        "le saut doit être ≥ 4x plus rapide que le ré-encodage plein cadre \
         (ON {:.3} ms, OFF {:.3} ms)",
        a_on.ms_par_trame(),
        a_off.ms_par_trame()
    );
    println!(
        "  OK : {:.1} o/trame et {:.3} ms/trame évités par trame statique (gain CPU x{:.0}).\n",
        a_off.octets_par_trame_suite(),
        a_off.ms_par_trame() - a_on.ms_par_trame(),
        a_off.ms_par_trame() / a_on.ms_par_trame().max(1e-9),
    );

    // --- Scénario B : petit mouvement --------------------------------------
    println!("=== Scénario B — petit mouvement (carré 96x96, dirty = 2 rects exacts) ===");
    let b_off = scenario_petit_mouvement(false, N);
    b_off.imprime("delta OFF (conversion pleine) :");
    let b_on = scenario_petit_mouvement(true, N);
    b_on.imprime("delta ON  (conversion 2 rects) :");
    println!(
        "  Gain de conversion : {:.3} ms/trame ({:.0} % du temps d'encodage plein cadre).\n",
        b_off.ms_par_trame() - b_on.ms_par_trame(),
        100.0 * (b_off.ms_par_trame() - b_on.ms_par_trame()) / b_off.ms_par_trame().max(1e-9),
    );

    // --- Scénario C : mouvement soutenu + qualité --------------------------
    println!("=== Scénario C — mouvement soutenu (moitié basse texturée à chaque trame) ===");
    let (c_8000, paire_8000) = scenario_video(8_000, 40);
    c_8000.imprime("8 000 kbit/s :");
    let (c_1000, paire_1000) = scenario_video(1_000, 40);
    c_1000.imprime("1 000 kbit/s :");

    assert!(
        c_8000.octets_par_trame_suite() > a_off.octets_par_trame_suite(),
        "le mouvement doit coûter plus que l'écran statique"
    );
    assert!(
        c_1000.octets_suite * 2 < c_8000.octets_suite,
        "la consigne de débit doit mordre (1 000 kbit/s → {} o, 8 000 kbit/s → {} o)",
        c_1000.octets_suite,
        c_8000.octets_suite
    );

    let (src_8000, dec_8000) = paire_8000.expect("frame décodée à 8 000 kbit/s");
    let (src_1000, dec_1000) = paire_1000.expect("frame décodée à 1 000 kbit/s");
    let psnr_8000 = psnr_luma(&src_8000, &dec_8000).expect("psnr");
    let psnr_1000 = psnr_luma(&src_1000, &dec_1000).expect("psnr");
    let ssim_8000 = ssim_luma(&src_8000, &dec_8000, LARGEUR, HAUTEUR).expect("ssim");
    let ssim_1000 = ssim_luma(&src_1000, &dec_1000, LARGEUR, HAUTEUR).expect("ssim");
    println!(
        "  Qualité (dernière trame décodée vs source) : \
         8 000 kbit/s → PSNR luma {psnr_8000:.1} dB, SSIM {ssim_8000:.4} ; \
         1 000 kbit/s → PSNR luma {psnr_1000:.1} dB, SSIM {ssim_1000:.4}"
    );
    assert!(
        psnr_8000 > 30.0,
        "à 8 Mbit/s la boucle doit rester fidèle (> 30 dB)"
    );
    assert!(
        psnr_8000 > psnr_1000 && ssim_8000 > ssim_1000,
        "la qualité doit croître avec le débit"
    );

    // --- ABR ----------------------------------------------------------------
    scenario_abr();

    println!("\nBanc terminé : toutes les assertions garde-fou sont passées.");
}
