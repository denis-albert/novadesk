//! Sonde d'encodage **matériel** (NVENC) : prouve que `create_hardware_encoder`
//! passe par un MFT H.264 **matériel** — sur ce poste, l'encodeur du GPU NVIDIA —
//! et pas par le MFT logiciel de Microsoft.
//!
//! La sonde :
//! 1. imprime le **nom exact** de l'encodeur sélectionné ([`VideoEncoder::nom_backend`]),
//!    et échoue honnêtement (code 1 + message de repli) s'il n'est pas NVIDIA ;
//! 2. encode 60 frames 1920×1080 synthétiques (scène type bureau : dégradé +
//!    fenêtre en déplacement) en mesurant **taille et temps par trame** (le temps
//!    inclut la conversion BGRA→NV12 CPU, comme en production) ;
//! 3. calcule le débit effectif et le compare à la consigne CBR ;
//! 4. **re-décode** tout le flux avec openh264 pour prouver que la sortie NVENC
//!    est un H.264 valide. Assertions garde-fou sur chaque étape.
//!
//! Lancer : `cargo run --example nvenc_probe -p nd-codec --release`

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
    use nd_codec::{create_decoder, create_hardware_encoder, CodecKind, EncoderConfig};
    use nd_proto::MonitorId;

    const LARGEUR: u32 = 1920;
    const HAUTEUR: u32 = 1080;
    const FRAMES: u32 = 60;
    const FPS: u32 = 60;
    const DEBIT_KBPS: u32 = 8_000;

    /// Scène synthétique type bureau : fond en dégradé fixe (texte/fenêtres
    /// immobiles) + « fenêtre » claire de 480×270 qui se déplace en diagonale
    /// (l'élément mobile typique d'une session de bureau à distance).
    fn frame_synthetique(phase: u32) -> CapturedFrame {
        let stride = LARGEUR as usize * 4;
        let mut data = vec![0u8; stride * HAUTEUR as usize];
        for y in 0..HAUTEUR as usize {
            for x in 0..LARGEUR as usize {
                let o = y * stride + x * 4;
                data[o] = (x % 256) as u8; // B : dégradé horizontal
                data[o + 1] = (y % 256) as u8; // G : dégradé vertical
                data[o + 2] = ((x + y) / 12 % 256) as u8; // R : diagonale
                data[o + 3] = 255;
            }
        }
        // Fenêtre mobile (8 px de déplacement par frame, bornée à l'image).
        let (fw, fh) = (480usize, 270usize);
        let fx = (phase as usize * 8) % (LARGEUR as usize - fw);
        let fy = (phase as usize * 4) % (HAUTEUR as usize - fh);
        for y in fy..fy + fh {
            for x in fx..fx + fw {
                let o = y * stride + x * 4;
                data[o] = 235; // fenêtre gris clair, bord implicite
                data[o + 1] = 235;
                data[o + 2] = 235;
            }
        }
        CapturedFrame {
            width: LARGEUR,
            height: HAUTEUR,
            monitor: MonitorId(0),
            format: PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: u64::from(phase) * 1_000_000 / u64::from(FPS),
            image: Some(FrameImage::Cpu { data, stride }),
        }
    }

    // --- 1. Sélection : le chemin matériel doit remonter un nom NVIDIA. ---------
    let t_creation = Instant::now();
    let mut enc = create_hardware_encoder(CodecKind::H264)?;
    let creation_ms = t_creation.elapsed().as_secs_f64() * 1e3;
    let nom = enc.nom_backend().to_string();
    println!("Encodeur sélectionné : « {nom} » (création {creation_ms:.1} ms)");

    let nom_bas = nom.to_lowercase();
    if !nom_bas.contains("nvidia") {
        eprintln!(
            "ÉCHEC DE PREUVE GPU : l'encodeur sélectionné n'est pas le MFT NVIDIA.\n\
             Le MFT matériel est absent ou non instanciable sur cette machine ; le\n\
             repli documenté a pris le relais : « {nom} ». Rien n'a planté, mais la\n\
             sonde refuse d'annoncer « GPU » sans la preuve du nom."
        );
        std::process::exit(1);
    }
    assert!(
        !nom_bas.contains("microsoft"),
        "garde-fou : le MFT logiciel Microsoft ne doit pas passer pour du matériel"
    );

    // --- 2. Configuration (négociation des types + démarrage du flux GPU). ------
    let t_cfg = Instant::now();
    enc.configure(EncoderConfig {
        kind: CodecKind::H264,
        width: LARGEUR,
        height: HAUTEUR,
        target_bitrate_kbps: DEBIT_KBPS,
        max_fps: FPS,
    })?;
    println!(
        "configure() : {:.1} ms ({LARGEUR}x{HAUTEUR}, CBR {DEBIT_KBPS} kbit/s, {FPS} fps, NV12)",
        t_cfg.elapsed().as_secs_f64() * 1e3
    );

    // --- 3. Encodage mesuré + re-décodage openh264 (preuve de validité). --------
    let mut dec = create_decoder(CodecKind::H264)?;
    let brut_par_frame = u64::from(LARGEUR) * u64::from(HAUTEUR) * 4;
    let mut temps_ms: Vec<f64> = Vec::with_capacity(FRAMES as usize);
    let mut total_h264: u64 = 0;
    let mut cle_vue = false;
    let mut decodees = 0u32;

    println!("\ntrame |  H.264 (o) | clé | encode (ms) | redécodée");
    for i in 0..FRAMES {
        let frame = frame_synthetique(i);
        let t = Instant::now();
        let chunk = enc.encode(&frame, i == 0)?;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        temps_ms.push(ms);

        assert!(!chunk.data.is_empty(), "trame {i} : chunk vide");
        if i == 0 {
            assert!(
                chunk.is_keyframe,
                "la première trame doit être une image-clé"
            );
        }
        cle_vue |= chunk.is_keyframe;
        total_h264 += chunk.data.len() as u64;

        let redecodee = dec.decode(&chunk)?;
        if let Some(img) = &redecodee {
            assert_eq!(
                (img.width, img.height),
                (LARGEUR, HAUTEUR),
                "dimensions décodées incohérentes"
            );
            decodees += 1;
        }
        println!(
            "{:5} | {:10} | {:^3} | {:11.2} | {}",
            i + 1,
            chunk.data.len(),
            if chunk.is_keyframe { "oui" } else { "non" },
            ms,
            if redecodee.is_some() { "oui" } else { "non" },
        );
    }

    // --- 4. Synthèse chiffrée + garde-fous. --------------------------------------
    // La première trame paie l'initialisation de la session GPU : statistiques de
    // régime permanent calculées sur les trames suivantes.
    let mut tri = temps_ms[1..].to_vec();
    tri.sort_by(|a, b| a.partial_cmp(b).expect("temps finis"));
    let moyenne = tri.iter().sum::<f64>() / tri.len() as f64;
    let p95 = tri[(tri.len() * 95 / 100).min(tri.len() - 1)];
    let duree_video_s = f64::from(FRAMES) / f64::from(FPS);
    let debit_effectif_kbps = (total_h264 as f64 * 8.0 / 1000.0) / duree_video_s;
    let ratio = (brut_par_frame * u64::from(FRAMES)) as f64 / total_h264 as f64;

    println!(
        "\nSynthèse : {FRAMES} trames {LARGEUR}x{HAUTEUR} via « {nom} »\n\
         - temps d'encodage : 1re trame {premiere:.2} ms (init session GPU incluse), \
         puis moy {moyenne:.2} ms / p95 {p95:.2} ms / max {max:.2} ms\n\
         - taille totale H.264 : {total_h264} o (compression {ratio:.0}x vs BGRA brut)\n\
         - débit effectif : {debit_effectif_kbps:.0} kbit/s pour une consigne CBR \
         de {DEBIT_KBPS} kbit/s sur {duree_video_s:.1} s de vidéo\n\
         - re-décodage openh264 : {decodees}/{FRAMES} images reconstruites, \
         image-clé vue = {cle_vue}",
        premiere = temps_ms[0],
        max = tri.last().copied().unwrap_or(0.0),
    );

    assert!(cle_vue, "aucune image-clé produite");
    assert!(
        decodees >= FRAMES - 2,
        "flux suspect : seulement {decodees}/{FRAMES} images re-décodées"
    );
    assert!(
        moyenne < 100.0,
        "temps moyen d'encodage anormal ({moyenne:.1} ms/trame)"
    );
    // CBR : tolérance large (l'amorçage et la VBV font varier le début de flux).
    assert!(
        debit_effectif_kbps < f64::from(DEBIT_KBPS) * 3.0,
        "débit effectif très au-dessus de la consigne ({debit_effectif_kbps:.0} kbit/s)"
    );

    println!(
        "\nPREUVE GPU : l'encodage est passé par « {nom} » (MFT matériel NVIDIA/NVENC), \
         et son flux H.264 se re-décode intégralement avec openh264."
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!("nvenc_probe : exemple Windows uniquement (MFT matériel Media Foundation).");
}
