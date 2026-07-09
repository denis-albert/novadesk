//! Test d'intégration de la **relecture d'enregistrement** de la façade UI
//! (`nd_ffi::open_recording` / `recording_next_frame` / `recording_seek` /
//! `close_recording`, plus `recording_info` sans lecteur durable), de bout en
//! bout sur un vrai MP4 H.264 fabriqué à la volée : image synthétique
//! `CapturedFrame` → encodeur `nd-codec` → muxeur `nd-features`. Prouve qu'un
//! enregistrement se rouvre, se décode en RGBA aux bonnes dimensions, se
//! cherche par horodatage et se ferme.

use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
use nd_codec::{create_encoder, CodecKind, EncoderConfig};
use nd_features::{Mp4Muxer, RecordingMetadata};
use nd_ffi::{
    close_recording, open_recording, recording_info, recording_next_frame, recording_seek,
};
use nd_proto::MonitorId;

const LARGEUR: u32 = 128;
const HAUTEUR: u32 = 96;
const IMAGES: u64 = 20;
const PAS_US: u64 = 40_000; // 25 i/s
const PERIODE_CLE: u64 = 10; // image-clé forcée toutes les 10 images

/// Image synthétique BGRA : dégradé animé + pavé blanc qui traverse l'écran (du
/// mouvement réel, pour que l'encodeur produise clés et deltas).
fn image_synthetique(rang: u64) -> CapturedFrame {
    let (w, h) = (LARGEUR as usize, HAUTEUR as usize);
    let mut data = vec![0u8; w * h * 4];
    for (indice, pixel) in data.chunks_exact_mut(4).enumerate() {
        let (x, y) = (indice % w, indice / w);
        pixel[0] = (x * 2 + rang as usize * 3) as u8; // B
        pixel[1] = (y * 2) as u8; // G
        pixel[2] = (rang * 9) as u8; // R
        pixel[3] = 255;
    }
    let gauche = (rang as usize * 4) % (w - 16);
    let haut = (rang as usize * 2) % (h - 16);
    for y in haut..haut + 16 {
        let ligne = &mut data[(y * w + gauche) * 4..(y * w + gauche + 16) * 4];
        ligne.fill(255);
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

/// Fabrique les octets d'un vrai MP4 H.264 relisible (encodeur logiciel réel).
fn fabriquer_mp4() -> Vec<u8> {
    let mut encodeur = create_encoder(CodecKind::H264).expect("encodeur H.264 logiciel");
    encodeur
        .configure(EncoderConfig {
            kind: CodecKind::H264,
            width: LARGEUR,
            height: HAUTEUR,
            target_bitrate_kbps: 1_000,
            max_fps: 25,
        })
        .expect("configuration de l'encodeur");
    let metadonnees = RecordingMetadata {
        width: LARGEUR,
        height: HAUTEUR,
        fps: 25,
        codec: "nova-h264".into(),
        start_unix_ms: 1_750_000_000_000,
    };
    let mut muxeur = Mp4Muxer::new(std::io::Cursor::new(Vec::new()), metadonnees).unwrap();
    for rang in 0..IMAGES {
        let unite = encodeur
            .encode(&image_synthetique(rang), rang % PERIODE_CLE == 0)
            .expect("encodage d'une image");
        muxeur.record_video_chunk(&unite).unwrap();
    }
    muxeur.finish().unwrap().into_inner()
}

/// Écrit le MP4 fabriqué dans un dossier temporaire et rend son chemin (le
/// dossier reste vivant tant que le `TempDir` rendu n'est pas lâché).
fn enregistrement_temporaire() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dir.path().join("session.mp4");
    std::fs::write(&chemin, fabriquer_mp4()).expect("écriture du MP4");
    let texte = chemin.to_string_lossy().into_owned();
    (dir, texte)
}

#[test]
fn open_recording_et_next_frame_decodent_les_images() {
    let (_dir, chemin) = enregistrement_temporaire();

    let info = open_recording(chemin).expect("ouverture de l'enregistrement");
    assert_eq!((info.largeur, info.hauteur), (LARGEUR, HAUTEUR));
    assert_eq!(info.fps, 25);
    assert_eq!(info.nb_images, IMAGES);
    assert_eq!(info.duree_us, IMAGES * PAS_US);

    // Au moins une image se décode, aux bonnes dimensions et au bon format RGBA.
    let premiere = recording_next_frame(info.id)
        .expect("décodage sans erreur")
        .expect("au moins une image");
    assert_eq!((premiere.width, premiere.height), (LARGEUR, HAUTEUR));
    assert_eq!(
        premiere.rgba.len(),
        (LARGEUR * HAUTEUR * 4) as usize,
        "tampon RGBA complet attendu"
    );

    // La suite se décode jusqu'à épuisement (1 image par échantillon).
    let mut total = 1u64;
    while let Some(frame) = recording_next_frame(info.id).expect("décodage") {
        assert_eq!((frame.width, frame.height), (LARGEUR, HAUTEUR));
        total += 1;
    }
    assert_eq!(total, IMAGES, "toutes les images doivent se décoder");
    // Fin de flux stable.
    assert!(recording_next_frame(info.id).expect("décodage").is_none());

    close_recording(info.id).expect("fermeture");
    // Après fermeture, l'identifiant est invalide.
    assert!(recording_next_frame(info.id).is_err());
    assert!(close_recording(info.id).is_err());
}

#[test]
fn recording_seek_repart_d_une_image_cle() {
    let (_dir, chemin) = enregistrement_temporaire();
    let info = open_recording(chemin).expect("ouverture");

    // Recherche à 450 000 µs : l'image-clé précédente est à 400 000 µs
    // (image 10, image-clé forcée toutes les 10 images).
    recording_seek(info.id, 450_000).expect("recherche");
    let mut apres_seek = 0u64;
    while let Some(frame) = recording_next_frame(info.id).expect("décodage") {
        assert_eq!((frame.width, frame.height), (LARGEUR, HAUTEUR));
        apres_seek += 1;
    }
    // De l'image 10 à la 19 incluses = 10 images.
    assert_eq!(apres_seek, IMAGES - PERIODE_CLE);

    // Recherche au début : redonne toutes les images.
    recording_seek(info.id, 0).expect("recherche début");
    let mut depuis_debut = 0u64;
    while recording_next_frame(info.id).expect("décodage").is_some() {
        depuis_debut += 1;
    }
    assert_eq!(depuis_debut, IMAGES);

    close_recording(info.id).expect("fermeture");
}

#[test]
fn open_recording_fichier_absent_echoue() {
    let err = open_recording("chemin/inexistant/enregistrement.mp4".to_owned())
        .expect_err("un fichier absent doit être refusé");
    assert!(err.contains("impossible"), "message peu utile : {err}");
}

#[test]
fn recording_info_rend_les_metadonnees_sans_lecteur() {
    let (_dir, chemin) = enregistrement_temporaire();

    let info = recording_info(chemin).expect("métadonnées de l'enregistrement");
    // Dimensions non nulles et conformes au MP4 fabriqué.
    assert!(
        info.largeur > 0 && info.hauteur > 0,
        "dimensions nulles : {} × {}",
        info.largeur,
        info.hauteur
    );
    assert_eq!((info.largeur, info.hauteur), (LARGEUR, HAUTEUR));
    assert_eq!(info.fps, 25);
    assert_eq!(info.nb_images, IMAGES);
    assert_eq!(info.duree_us, IMAGES * PAS_US);

    // Aucun lecteur n'est resté ouvert : l'`id` sentinelle 0 n'est pas
    // utilisable avec les fonctions `recording_*` à identifiant.
    assert_eq!(info.id, 0);
    assert!(recording_next_frame(info.id).is_err());
    assert!(close_recording(info.id).is_err());
}

#[test]
fn recording_info_fichier_absent_echoue() {
    let err = recording_info("chemin/inexistant/enregistrement.mp4".to_owned())
        .expect_err("un fichier absent doit être refusé");
    assert!(err.contains("impossible"), "message peu utile : {err}");
}
