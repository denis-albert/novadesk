//! Exemple de bout en bout : enregistre une session synthétique dans un
//! fichier MP4 **réellement relisible**, puis le rouvre et re-décode chaque
//! image pour le prouver.
//!
//! ```text
//! cargo run -p nd-features --example record_and_replay
//! ```
//!
//! Chaîne exercée : images BGRA synthétiques → encodeur H.264 réel (openh264,
//! via `nd-codec`) → [`Mp4Muxer`] → fichier `.mp4` (ouvrable dans VLC/ffmpeg)
//! → [`Mp4Reader::validate`] (structure + BLAKE3) → démux → décodeur H.264
//! réel → comptage des images re-décodées. Le chemin d'archive `.ndr` +
//! conversion [`ndr_to_mp4`] est démontré dans la foulée.

use std::fs::{File, OpenOptions};
use std::io::Cursor;

use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
use nd_codec::{create_decoder, create_encoder, CodecKind, EncodedChunk, EncoderConfig};
use nd_features::{ndr_to_mp4, IndexedRecorder, Mp4Muxer, Mp4Reader, RecordingMetadata};
use nd_features::{Capability, PermissionBroker, PermissionSet, SessionReader};
use nd_proto::{MonitorId, Result};

const LARGEUR: u32 = 320;
const HAUTEUR: u32 = 240;
const IMAGES: u64 = 60;
const PAS_US: u64 = 40_000; // 25 i/s

/// Image synthétique BGRA : dégradé animé + pavé blanc qui traverse.
fn image_synthetique(rang: u64) -> CapturedFrame {
    let (w, h) = (LARGEUR as usize, HAUTEUR as usize);
    let mut data = vec![0u8; w * h * 4];
    for (indice, pixel) in data.chunks_exact_mut(4).enumerate() {
        let (x, y) = (indice % w, indice / w);
        pixel[0] = (x + rang as usize * 2) as u8;
        pixel[1] = y as u8;
        pixel[2] = (rang * 4) as u8;
        pixel[3] = 255;
    }
    let gauche = (rang as usize * 5) % (w - 32);
    let haut = (rang as usize * 3) % (h - 32);
    for y in haut..haut + 32 {
        data[(y * w + gauche) * 4..(y * w + gauche + 32) * 4].fill(255);
    }
    CapturedFrame {
        width: LARGEUR,
        height: HAUTEUR,
        monitor: MonitorId(0),
        format: PixelFormat::Bgra8,
        dirty: vec![],
        cursor: None,
        timestamp_us: rang * PAS_US,
        image: Some(FrameImage::Cpu {
            data,
            stride: w * 4,
        }),
    }
}

/// Re-décode toutes les images d'un MP4 avec un décodeur H.264 réel.
fn redecoder<R: std::io::Read + std::io::Seek>(
    lecteur: &mut Mp4Reader<R>,
) -> Result<(u64, u32, u32)> {
    let mut decodeur = create_decoder(CodecKind::H264)?;
    let (mut decodees, mut largeur, mut hauteur) = (0u64, 0u32, 0u32);
    lecteur.rewind();
    while let Some(echantillon) = lecteur.next_sample()? {
        let unite = EncodedChunk {
            data: lecteur.sample_annexb(&echantillon)?,
            is_keyframe: echantillon.keyframe,
            monitor: MonitorId(0),
            timestamp_us: echantillon.timestamp_us,
        };
        if let Some(image) = decodeur.decode(&unite)? {
            decodees += 1;
            largeur = image.width;
            hauteur = image.height;
        }
    }
    Ok((decodees, largeur, hauteur))
}

fn main() -> Result<()> {
    // 0. Garde d'intégration : l'orchestrateur vérifie la permission AVANT
    //    d'ouvrir le moindre enregistreur (contrat du module permissions).
    let mut broker =
        PermissionBroker::with_permissions([Capability::SessionRecording].into_iter().collect());
    assert!(broker.authorize("exemple", Capability::SessionRecording));

    let metadata = RecordingMetadata {
        width: LARGEUR,
        height: HAUTEUR,
        fps: 25,
        codec: "nova-h264".into(),
        start_unix_ms: 1_750_000_000_000,
    };

    // 1. Encodage H.264 réel de 60 images synthétiques (clé toutes les 20).
    let mut encodeur = create_encoder(CodecKind::H264)?;
    encodeur.configure(EncoderConfig {
        kind: CodecKind::H264,
        width: LARGEUR,
        height: HAUTEUR,
        target_bitrate_kbps: 2_000,
        max_fps: 25,
    })?;
    let unites: Vec<EncodedChunk> = (0..IMAGES)
        .map(|rang| encodeur.encode(&image_synthetique(rang), rang % 20 == 0))
        .collect::<Result<_>>()?;
    let cles = unites.iter().filter(|u| u.is_keyframe).count() as u64;
    println!(
        "[1] {IMAGES} images {LARGEUR}×{HAUTEUR} encodées en H.264 (openh264), dont {cles} images-clés"
    );

    // 2. Mux MP4 en direct, vers un vrai fichier (ouvrable dans VLC/ffmpeg).
    let chemin = std::env::temp_dir().join("novadesk_record_and_replay.mp4");
    let fichier = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&chemin)?;
    let mut muxeur = Mp4Muxer::new(fichier, metadata.clone())?;
    for unite in &unites {
        muxeur.record_video_chunk(unite)?;
    }
    let fichier = muxeur.finish()?;
    let taille = fichier.metadata()?.len();
    drop(fichier);
    println!("[2] MP4 écrit : {} ({taille} octets)", chemin.display());

    // 3. Réouverture + validation complète (structure, index, BLAKE3).
    let mut lecteur = Mp4Reader::new(File::open(&chemin)?)?;
    let rapport = lecteur.validate()?;
    println!(
        "[3] Validation : {} trames, {} images-clés, durée {} µs, {}×{}, {}, BLAKE3 {}",
        rapport.frames,
        rapport.keyframes,
        rapport.duration_us,
        rapport.width,
        rapport.height,
        rapport.codec,
        if rapport.hash_verified {
            "vérifié"
        } else {
            "absent"
        }
    );
    assert_eq!(rapport.frames, IMAGES);
    assert_eq!(rapport.keyframes, cles);
    assert!(rapport.hash_verified);

    // 4. Relecture : chaque image re-décodée par un décodeur H.264 réel.
    let (decodees, largeur, hauteur) = redecoder(&mut lecteur)?;
    println!("[4] Relecture : {decodees}/{IMAGES} trames re-décodées en {largeur}×{hauteur}");
    assert_eq!(decodees, IMAGES);
    assert_eq!((largeur, hauteur), (LARGEUR, HAUTEUR));

    // 5. Chemin d'archive : .ndr v2 (BLAKE3) puis conversion en MP4 rejouable.
    let mut archive = IndexedRecorder::new(Vec::new(), metadata, true)?;
    for unite in &unites {
        archive.record_video_chunk(unite)?;
    }
    let ndr = archive.finish()?;
    let mut lecteur_ndr = SessionReader::new(Cursor::new(ndr))?;
    let mp4 = ndr_to_mp4(&mut lecteur_ndr, Cursor::new(Vec::new()))?.into_inner();
    let mut lecteur_conv = Mp4Reader::new(Cursor::new(mp4))?;
    let rapport_conv = lecteur_conv.validate()?;
    let (redecodees, _, _) = redecoder(&mut lecteur_conv)?;
    println!(
        "[5] Archive .ndr convertie : MP4 valide ({} trames, BLAKE3 {}), {redecodees}/{IMAGES} re-décodées",
        rapport_conv.frames,
        if rapport_conv.hash_verified {
            "vérifié"
        } else {
            "absent"
        }
    );
    assert_eq!(rapport_conv.frames, IMAGES);
    assert_eq!(redecodees, IMAGES);

    // Refus par défaut : sans la capacité, l'orchestrateur n'enregistre pas.
    let mut sans_droit = PermissionBroker::with_permissions(PermissionSet::view_only());
    assert!(!sans_droit.authorize("exemple", Capability::SessionRecording));
    println!("[6] Garde permissions : enregistrement refusé sans SessionRecording (journalisé)");
    Ok(())
}
