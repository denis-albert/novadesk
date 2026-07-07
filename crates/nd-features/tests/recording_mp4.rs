//! Preuve de bout en bout que l'enregistrement produit une vidéo
//! **réellement relisible** : images synthétiques → encodeur H.264 réel
//! (openh264 via `nd-codec`) → mux MP4 (`nd-features`) → validation du
//! conteneur → démux → **re-décodage réel** de chaque image.
//!
//! Si le MP4 était un simple sac d'octets, le décodeur échouerait : ces tests
//! garantissent que le fichier est exploitable par un décodeur H.264 standard.

use std::io::Cursor;

use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
use nd_codec::{create_decoder, create_encoder, CodecKind, EncodedChunk, EncoderConfig};
use nd_features::{ndr_to_mp4, IndexedRecorder, Mp4Muxer, Mp4Reader, RecordingMetadata};
use nd_features::{SessionReader, ValidationReport};
use nd_proto::MonitorId;

const LARGEUR: u32 = 128;
const HAUTEUR: u32 = 96;
const IMAGES: u64 = 25;
const PAS_US: u64 = 40_000; // 25 i/s

/// Image synthétique BGRA : dégradé animé + pavé qui traverse l'écran
/// (du mouvement réel, pour que l'encodeur produise clés et deltas).
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
    // Pavé blanc 16×16 qui avance à chaque image.
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

fn metadonnees() -> RecordingMetadata {
    RecordingMetadata {
        width: LARGEUR,
        height: HAUTEUR,
        fps: 25,
        codec: "nova-h264".into(),
        start_unix_ms: 1_750_000_000_000,
    }
}

/// Encode `IMAGES` images H.264 réelles (image-clé forcée toutes les 10).
fn encoder_session() -> Vec<EncodedChunk> {
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
    (0..IMAGES)
        .map(|rang| {
            encodeur
                .encode(&image_synthetique(rang), rang % 10 == 0)
                .expect("encodage d'une image")
        })
        .collect()
}

/// Re-décode toutes les images d'un MP4 et rend (nombre, dimensions vues).
fn redecoder_tout(lecteur: &mut Mp4Reader<Cursor<Vec<u8>>>) -> (u64, u32, u32) {
    let mut decodeur = create_decoder(CodecKind::H264).expect("décodeur H.264 logiciel");
    let (mut decodees, mut largeur, mut hauteur) = (0u64, 0u32, 0u32);
    lecteur.rewind();
    while let Some(echantillon) = lecteur.next_sample().expect("échantillon lisible") {
        let annexb = lecteur
            .sample_annexb(&echantillon)
            .expect("conversion Annex B");
        let unite = EncodedChunk {
            data: annexb,
            is_keyframe: echantillon.keyframe,
            monitor: MonitorId(0),
            timestamp_us: echantillon.timestamp_us,
        };
        if let Some(image) = decodeur.decode(&unite).expect("décodage H.264") {
            decodees += 1;
            largeur = image.width;
            hauteur = image.height;
            assert_eq!(image.rgba.len(), (image.width * image.height * 4) as usize);
        }
    }
    (decodees, largeur, hauteur)
}

#[test]
fn mp4_direct_valide_et_redecodable() {
    let unites = encoder_session();
    assert_eq!(unites.len() as u64, IMAGES);
    assert!(unites[0].is_keyframe, "la première unité doit être une clé");
    let cles_encodees = unites.iter().filter(|u| u.is_keyframe).count() as u64;
    assert!(cles_encodees >= 3, "images-clés forcées en 0, 10 et 20");

    // Mux en direct : l'API d'intégration que nd-core appellera.
    let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), metadonnees()).unwrap();
    for unite in &unites {
        muxeur.record_video_chunk(unite).unwrap();
    }
    assert_eq!(muxeur.frames_written(), IMAGES);
    let octets = muxeur.finish().unwrap().into_inner();
    assert_eq!(&octets[4..8], b"ftyp", "en-tête MP4 standard attendu");

    // Validation structurelle complète + intégrité BLAKE3.
    let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
    let rapport = lecteur.validate().unwrap();
    assert_eq!(rapport.frames, IMAGES);
    assert_eq!(rapport.keyframes, cles_encodees);
    assert_eq!(rapport.width, LARGEUR);
    assert_eq!(rapport.height, HAUTEUR);
    assert_eq!(rapport.duration_us, IMAGES * PAS_US);
    assert!(rapport.codec.starts_with("avc1."), "{}", rapport.codec);
    assert!(rapport.hash_verified);

    // Preuve finale : chaque image se RE-DÉCODE avec un décodeur H.264 réel.
    let (decodees, largeur, hauteur) = redecoder_tout(&mut lecteur);
    assert_eq!(decodees, IMAGES, "toutes les images doivent se décoder");
    assert_eq!((largeur, hauteur), (LARGEUR, HAUTEUR));
}

#[test]
fn ndr_puis_conversion_mp4_redecodable() {
    let unites = encoder_session();

    // 1. Session archivée au fil de l'eau dans le conteneur interne .ndr v2.
    let mut archive = IndexedRecorder::new(Vec::new(), metadonnees(), true).unwrap();
    for unite in &unites {
        archive.record_video_chunk(unite).unwrap();
    }
    let ndr = archive.finish().unwrap();

    // L'archive elle-même est saine (index + hachage BLAKE3).
    let mut lecteur_ndr = SessionReader::new(Cursor::new(ndr.clone())).unwrap();
    let rapport_ndr: ValidationReport = lecteur_ndr.validate().unwrap();
    assert_eq!(rapport_ndr.frames, IMAGES);
    assert!(rapport_ndr.hash_verified);

    // 2. Conversion .ndr → MP4 rejouable.
    let mut lecteur_ndr = SessionReader::new(Cursor::new(ndr)).unwrap();
    let mp4 = ndr_to_mp4(&mut lecteur_ndr, Cursor::new(Vec::new()))
        .unwrap()
        .into_inner();

    let mut lecteur = Mp4Reader::new(Cursor::new(mp4)).unwrap();
    let rapport = lecteur.validate().unwrap();
    assert_eq!(rapport.frames, IMAGES);
    assert!(rapport.hash_verified);

    let (decodees, largeur, hauteur) = redecoder_tout(&mut lecteur);
    assert_eq!(decodees, IMAGES);
    assert_eq!((largeur, hauteur), (LARGEUR, HAUTEUR));
}

#[test]
fn mp4_corrompu_detecte_avant_relecture() {
    let unites = encoder_session();
    let mut muxeur = Mp4Muxer::new(Cursor::new(Vec::new()), metadonnees()).unwrap();
    for unite in &unites {
        muxeur.record_video_chunk(unite).unwrap();
    }
    let mut octets = muxeur.finish().unwrap().into_inner();

    // Corruption d'un octet au début des données vidéo (la charge utile de
    // mdat commence à 48 = ftyp 32 + en-tête long 16 ; l'octet 60 est dans
    // la première NAL du premier échantillon) : la structure du conteneur
    // reste analysable, c'est la vérification qui doit la révéler.
    octets[60] ^= 0xFF;
    let mut lecteur = Mp4Reader::new(Cursor::new(octets)).unwrap();
    assert!(
        lecteur.validate().is_err(),
        "la corruption des données doit invalider le BLAKE3"
    );
}
