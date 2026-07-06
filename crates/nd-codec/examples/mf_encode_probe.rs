//! Sonde d'encodage **Media Foundation** : encode quelques frames BGRA synthétiques
//! (dégradé 1280x720 animé) via `create_hardware_encoder(H264)`, affiche taille et
//! type (clé/delta), puis re-décode chaque unité via `create_decoder(H264)`
//! (openh264) pour prouver la validité du flux produit par le MFT.
//!
//! Lancer : `cargo run --example mf_encode_probe -p nd-codec`

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
    use nd_codec::{create_decoder, create_hardware_encoder, CodecKind, EncoderConfig};
    use nd_proto::MonitorId;

    const LARGEUR: u32 = 1280;
    const HAUTEUR: u32 = 720;
    const FRAMES: u32 = 8;

    /// Frame BGRA synthétique : dégradé diagonal animé (décalé de `phase` pixels).
    fn frame_synthetique(phase: u32) -> CapturedFrame {
        let stride = LARGEUR as usize * 4;
        let mut data = vec![0u8; stride * HAUTEUR as usize];
        for y in 0..HAUTEUR as usize {
            for x in 0..LARGEUR as usize {
                let o = y * stride + x * 4;
                data[o] = ((x as u32 + phase * 4) % 256) as u8; // B : dégradé animé
                data[o + 1] = ((y as u32) % 256) as u8; // G : dégradé vertical
                data[o + 2] = (((x + y) as u32 / 8) % 256) as u8; // R : diagonale
                data[o + 3] = 255; // A opaque
            }
        }
        CapturedFrame {
            width: LARGEUR,
            height: HAUTEUR,
            monitor: MonitorId(0),
            format: PixelFormat::Bgra8,
            dirty: Vec::new(),
            cursor: None,
            timestamp_us: u64::from(phase) * 16_667, // ~60 fps
            image: Some(FrameImage::Cpu { data, stride }),
        }
    }

    let mut enc = create_hardware_encoder(CodecKind::H264)?;
    enc.configure(EncoderConfig {
        kind: CodecKind::H264,
        width: LARGEUR,
        height: HAUTEUR,
        target_bitrate_kbps: 8_000,
        max_fps: 60,
    })?;
    let mut dec = create_decoder(CodecKind::H264)?;

    println!("Encodage H.264 via Media Foundation ({LARGEUR}x{HAUTEUR}, {FRAMES} frames)…");

    let mut total_brut: u64 = 0;
    let mut total_h264: u64 = 0;
    let mut cle_vue = false;
    let mut decodees = 0u32;

    for i in 0..FRAMES {
        let frame = frame_synthetique(i);
        let force_cle = i == 0; // première frame en image-clé
        let chunk = enc.encode(&frame, force_cle)?;

        let brut = u64::from(LARGEUR) * u64::from(HAUTEUR) * 4;
        let h264 = chunk.data.len() as u64;
        total_brut += brut;
        total_h264 += h264;
        cle_vue |= chunk.is_keyframe;

        // Validation : le flux du MFT doit se re-décoder avec openh264.
        let redecodee = dec.decode(&chunk)?;
        if let Some(img) = &redecodee {
            assert_eq!((img.width, img.height), (LARGEUR, HAUTEUR));
            decodees += 1;
        }

        println!(
            "frame {} : H.264 {h264} o | clé={} | ratio={:.1}x | redécodée={}",
            i + 1,
            chunk.is_keyframe,
            brut as f64 / h264.max(1) as f64,
            redecodee.is_some(),
        );
    }

    let ratio_moyen = total_brut as f64 / total_h264.max(1) as f64;
    println!(
        "Terminé : {FRAMES} frame(s), keyframe vue={cle_vue}, {decodees} re-décodée(s), \
         compression moyenne={ratio_moyen:.1}x ({total_brut} o bruts -> {total_h264} o H.264)."
    );
    assert!(cle_vue, "aucune image-clé produite");
    assert!(decodees > 0, "aucune frame re-décodée : flux invalide");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!("mf_encode_probe : exemple Windows uniquement (Media Foundation).");
}
