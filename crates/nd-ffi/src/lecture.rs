//! Relecture d'enregistrement de la façade FFI (voir [`crate::api`]).
//!
//! Comme pour les sessions live ([`crate::flux`]), `flutter_rust_bridge` ne
//! portant pas bien un objet mutable partagé, la relecture travaille **par
//! identifiant opaque** : chaque enregistrement ouvert vit dans une table
//! statique (`OnceLock<Mutex<HashMap<u64, EntreeLecteur>>>`) et l'UI ne manipule
//! que son `u64`. Ce module est **privé** : il n'est pas scanné par le codegen
//! (`rust_input: crate::api`) et ne fait pas partie du contrat.
//!
//! # Décodage
//!
//! À l'ouverture, [`nd_features::RecordingPlayer`] détecte le format (`.mp4` ou
//! `.ndr`), expose les métadonnées et rend **tous** les échantillons encodés
//! (H.264 Annex B, [`nd_features::EncodedSample`]). On les met en cache, avec un
//! curseur et un décodeur [`nd_codec`] frais. [`image_suivante`] enveloppe
//! l'échantillon courant dans un [`nd_codec::EncodedChunk`] et le décode en RGBA
//! — exactement comme le fait la session live pour produire une
//! [`VideoFrameDto`]. [`chercher`] repositionne le curseur sur l'image-clé la
//! plus proche ≤ horodatage (via [`RecordingPlayer::sample_at`]) et réinitialise
//! le décodeur (le flux doit reprendre sur une image-clé).
//!
//! Deux accès **sans identifiant** complètent la table (lot « extras session &
//! relecture ») : [`infos`] lit les métadonnées puis referme aussitôt le
//! fichier, et [`flux_images`] relit tout l'enregistrement en **flux poussé**
//! vers un `StreamSink` Dart (thread de drainage dédié, même motif que
//! [`crate::flux`]).

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

use nd_codec::{create_decoder, CodecKind, EncodedChunk, VideoDecoder};
use nd_features::{EncodedSample, RecordingPlayer};
use nd_proto::MonitorId;

use crate::api::{RecordingInfoDto, VideoFrameDto};
use crate::frb_generated::StreamSink;

/// Un enregistrement ouvert : le lecteur (pour la recherche par temps), le
/// décodeur courant, les échantillons encodés en cache et le curseur de lecture.
struct EntreeLecteur {
    /// Lecteur de relecture ouvert sur le fichier (sert à [`RecordingPlayer::sample_at`]).
    lecteur: RecordingPlayer<File>,
    /// Décodeur H.264 courant ; réinitialisé à chaque recherche.
    decodeur: Box<dyn VideoDecoder>,
    /// Tous les échantillons encodés (Annex B), dans l'ordre de présentation.
    echantillons: Vec<EncodedSample>,
    /// Index du prochain échantillon à décoder.
    curseur: usize,
}

/// Table des enregistrements ouverts, indexée par identifiant opaque.
type TableLecteurs = Mutex<HashMap<u64, EntreeLecteur>>;

/// Prochain identifiant de lecteur (compteur monotone : 1, 2, 3…).
static PROCHAIN_ID: AtomicU64 = AtomicU64::new(1);

/// Table statique unique du processus.
static LECTEURS: OnceLock<TableLecteurs> = OnceLock::new();

/// Verrouille la table (empoisonnement absorbé, cf. [`crate::flux`]).
fn verrou() -> MutexGuard<'static, HashMap<u64, EntreeLecteur>> {
    LECTEURS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Ouvre l'enregistrement `chemin` (`.mp4` **ou** `.ndr`, format auto-détecté),
/// met ses échantillons en cache, crée un décodeur H.264 et renvoie ses
/// métadonnées + l'identifiant opaque attribué.
pub(crate) fn ouvrir(chemin: String) -> Result<RecordingInfoDto, String> {
    let mut lecteur = RecordingPlayer::open_path(&chemin)
        .map_err(|e| format!("ouverture de l'enregistrement « {chemin} » impossible : {e}"))?;
    let echantillons = lecteur
        .samples()
        .map_err(|e| format!("extraction des échantillons de « {chemin} » impossible : {e}"))?;
    let decodeur = create_decoder(CodecKind::H264)
        .map_err(|e| format!("création du décodeur H.264 impossible : {e}"))?;

    // Métadonnées lues avant de déplacer le lecteur dans la table.
    let (largeur, hauteur, fps, duree_us, nb_images) = (
        lecteur.width(),
        lecteur.height(),
        lecteur.fps(),
        lecteur.duration_us(),
        lecteur.frames(),
    );

    let id = PROCHAIN_ID.fetch_add(1, Ordering::Relaxed);
    verrou().insert(
        id,
        EntreeLecteur {
            lecteur,
            decodeur,
            echantillons,
            curseur: 0,
        },
    );
    Ok(RecordingInfoDto {
        id,
        largeur,
        hauteur,
        fps,
        duree_us,
        nb_images,
    })
}

/// Décode et renvoie la prochaine image de l'enregistrement `id`, ou `Ok(None)`
/// en fin de flux. Les échantillons qui ne produisent pas d'image (trames de
/// répétition en encodage delta) sont sautés jusqu'à la prochaine image
/// affichable.
pub(crate) fn image_suivante(id: u64) -> Result<Option<VideoFrameDto>, String> {
    let mut table = verrou();
    let entree = table.get_mut(&id).ok_or_else(|| {
        format!("lecteur d'enregistrement {id} inconnu (jamais ouvert ou déjà fermé)")
    })?;
    while entree.curseur < entree.echantillons.len() {
        // Emprunt court de l'échantillon (données clonées) pour libérer le
        // vecteur avant l'emprunt mutable du curseur et du décodeur.
        let chunk = {
            let echantillon = &entree.echantillons[entree.curseur];
            EncodedChunk {
                data: echantillon.data.clone(),
                is_keyframe: echantillon.is_keyframe,
                monitor: MonitorId(0),
                timestamp_us: echantillon.timestamp_us,
            }
        };
        entree.curseur += 1;
        match entree
            .decodeur
            .decode(&chunk)
            .map_err(|e| format!("décodage de l'échantillon impossible : {e}"))?
        {
            Some(frame) => return Ok(Some(frame.into())),
            None => continue,
        }
    }
    Ok(None)
}

/// Repositionne la lecture de l'enregistrement `id` sur l'image-clé la plus
/// proche **avant** (ou à) `timestamp_us` et réinitialise le décodeur (le flux
/// reprend sur une image-clé). Sans image-clé, retombe au début.
pub(crate) fn chercher(id: u64, timestamp_us: u64) -> Result<(), String> {
    let mut table = verrou();
    let entree = table.get_mut(&id).ok_or_else(|| {
        format!("lecteur d'enregistrement {id} inconnu (jamais ouvert ou déjà fermé)")
    })?;
    let cible = entree
        .lecteur
        .sample_at(timestamp_us)
        .map_err(|e| format!("recherche dans l'enregistrement impossible : {e}"))?;
    // Position de l'image-clé cible dans le cache : l'échantillon rendu par
    // `sample_at` est identifié par son horodatage (image-clé unique par ts).
    entree.curseur = match cible {
        Some(echantillon) => entree
            .echantillons
            .iter()
            .position(|e| e.is_keyframe && e.timestamp_us == echantillon.timestamp_us)
            .unwrap_or(0),
        None => 0,
    };
    // Un décodeur ne peut reprendre qu'à une image-clé : on repart à neuf.
    entree.decodeur = create_decoder(CodecKind::H264)
        .map_err(|e| format!("réinitialisation du décodeur impossible : {e}"))?;
    Ok(())
}

/// Ferme l'enregistrement `id` et le retire de la table. Erreur s'il est inconnu.
pub(crate) fn fermer(id: u64) -> Result<(), String> {
    verrou().remove(&id).map(|_| ()).ok_or_else(|| {
        format!("lecteur d'enregistrement {id} inconnu (jamais ouvert ou déjà fermé)")
    })
}

// ---------------------------------------------------------------------------
// Accès sans identifiant : métadonnées seules et relecture en flux poussé
// ---------------------------------------------------------------------------

/// Métadonnées de l'enregistrement `chemin` **sans lecteur durable** : le
/// fichier est ouvert, lu, puis refermé aussitôt — rien n'entre dans la table,
/// rien à fermer ensuite. Le champ `id` du DTO rendu vaut `0`, valeur jamais
/// attribuée à un lecteur réel ([`PROCHAIN_ID`] démarre à 1) : pour décoder des
/// images, passer par [`ouvrir`] ou [`flux_images`].
pub(crate) fn infos(chemin: String) -> Result<RecordingInfoDto, String> {
    let lecteur = RecordingPlayer::open_path(&chemin)
        .map_err(|e| format!("ouverture de l'enregistrement « {chemin} » impossible : {e}"))?;
    Ok(RecordingInfoDto {
        id: 0,
        largeur: lecteur.width(),
        hauteur: lecteur.height(),
        fps: lecteur.fps(),
        duree_us: lecteur.duration_us(),
        nb_images: lecteur.frames(),
    })
}

/// Prochain numéro de thread de relecture en flux (nommage unique, sans lien
/// avec les identifiants de lecteur).
static PROCHAIN_FLUX: AtomicU64 = AtomicU64::new(1);

/// Relit l'enregistrement `chemin` en **flux poussé** : ouvre le fichier, met
/// tous les échantillons encodés en cache, puis un thread dédié les décode en
/// RGBA et pousse chaque image dans `sink` (même [`VideoFrameDto`] que la
/// session live), dans l'ordre de présentation.
///
/// L'ouverture, l'extraction des échantillons et la création du décodeur sont
/// **synchrones** : leurs erreurs sont renvoyées immédiatement, avant tout
/// démarrage de thread. Ensuite, même contrat que les drains de
/// [`crate::flux`] : le drain s'arrête à l'annulation du `Stream` côté Dart
/// (`add` en échec) ; une erreur de décodage est signalée au `Stream`
/// (`add_error`) puis clôt la relecture ; en fin d'enregistrement, lâcher le
/// sink clôt le `Stream` Dart.
pub(crate) fn flux_images(chemin: String, sink: StreamSink<VideoFrameDto>) -> Result<(), String> {
    let mut lecteur = RecordingPlayer::open_path(&chemin)
        .map_err(|e| format!("ouverture de l'enregistrement « {chemin} » impossible : {e}"))?;
    let echantillons = lecteur
        .samples()
        .map_err(|e| format!("extraction des échantillons de « {chemin} » impossible : {e}"))?;
    let mut decodeur = create_decoder(CodecKind::H264)
        .map_err(|e| format!("création du décodeur H.264 impossible : {e}"))?;
    // Tout est en cache : le descripteur de fichier peut être refermé tout de suite.
    drop(lecteur);

    let nom = format!(
        "nd-ffi-relecture-{}",
        PROCHAIN_FLUX.fetch_add(1, Ordering::Relaxed)
    );
    thread::Builder::new()
        .name(nom.clone())
        .spawn(move || {
            let issue = decoder_vers(echantillons, decodeur.as_mut(), |image| {
                sink.add(image).is_ok()
            });
            if let Err(erreur) = issue {
                // Erreur de décodage : signalée au Stream Dart (reçue comme
                // erreur de flux) avant de le clore en lâchant le sink.
                let _ = sink.add_error(erreur);
            }
        })
        .map(|_poignee| ())
        .map_err(|e| format!("création du thread « {nom} » impossible : {e}"))
}

/// Décode `echantillons` un à un et livre chaque image produite via `livrer` ;
/// s'arrête sans erreur dès que `livrer` rend `false` (consommateur parti). Les
/// échantillons qui ne produisent pas d'image (trames de répétition en encodage
/// delta) sont sautés. Une erreur de décodage interrompt la relecture.
fn decoder_vers(
    echantillons: Vec<EncodedSample>,
    decodeur: &mut dyn VideoDecoder,
    mut livrer: impl FnMut(VideoFrameDto) -> bool,
) -> Result<(), String> {
    for echantillon in echantillons {
        // Les échantillons sont consommés : les données partent au décodeur
        // sans copie (contrairement à `image_suivante`, dont le cache survit).
        let chunk = EncodedChunk {
            data: echantillon.data,
            is_keyframe: echantillon.is_keyframe,
            monitor: MonitorId(0),
            timestamp_us: echantillon.timestamp_us,
        };
        if let Some(frame) = decodeur
            .decode(&chunk)
            .map_err(|e| format!("décodage de l'échantillon impossible : {e}"))?
        {
            if !livrer(frame.into()) {
                return Ok(());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests unitaires : cœur du décodage en flux (échantillons réels encodés en
// H.264 logiciel — aucun fichier ni StreamSink requis).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use nd_capture::{CapturedFrame, FrameImage, PixelFormat};
    use nd_codec::{create_encoder, EncoderConfig};

    use super::*;

    const LARGEUR: u32 = 64;
    const HAUTEUR: u32 = 48;
    const PAS_US: u64 = 40_000; // 25 i/s

    /// Image synthétique BGRA animée (du mouvement réel pour l'encodeur).
    fn image_synthetique(rang: u64) -> CapturedFrame {
        let (w, h) = (LARGEUR as usize, HAUTEUR as usize);
        let mut data = vec![0u8; w * h * 4];
        for (indice, pixel) in data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (indice % w, indice / w);
            pixel[0] = (x * 3 + rang as usize * 5) as u8; // B
            pixel[1] = (y * 4) as u8; // G
            pixel[2] = (rang * 11) as u8; // R
            pixel[3] = 255;
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

    /// Encode `n` images synthétiques (image-clé en tête) en échantillons prêts
    /// à décoder — le même matériau que rend [`RecordingPlayer::samples`].
    fn echantillons_synthetiques(n: u64) -> Vec<EncodedSample> {
        let mut encodeur = create_encoder(CodecKind::H264).expect("encodeur H.264 logiciel");
        encodeur
            .configure(EncoderConfig {
                kind: CodecKind::H264,
                width: LARGEUR,
                height: HAUTEUR,
                target_bitrate_kbps: 500,
                max_fps: 25,
            })
            .expect("configuration de l'encodeur");
        (0..n)
            .map(|rang| {
                let unite = encodeur
                    .encode(&image_synthetique(rang), rang == 0)
                    .expect("encodage d'une image");
                EncodedSample {
                    timestamp_us: unite.timestamp_us,
                    is_keyframe: unite.is_keyframe,
                    data: unite.data,
                }
            })
            .collect()
    }

    #[test]
    fn decoder_vers_livre_toutes_les_images() {
        let echantillons = echantillons_synthetiques(5);
        let mut decodeur = create_decoder(CodecKind::H264).expect("décodeur");
        let mut images = Vec::new();
        decoder_vers(echantillons, decodeur.as_mut(), |image| {
            images.push(image);
            true
        })
        .expect("décodage complet");
        assert_eq!(images.len(), 5, "une image décodée par échantillon");
        for image in &images {
            assert_eq!((image.width, image.height), (LARGEUR, HAUTEUR));
            assert_eq!(
                image.rgba.len(),
                (LARGEUR * HAUTEUR * 4) as usize,
                "tampon RGBA complet attendu"
            );
        }
    }

    #[test]
    fn decoder_vers_s_arrete_quand_le_consommateur_part() {
        // `livrer` refuse dès la première image (Stream Dart annulé) : la
        // relecture s'arrête proprement, sans erreur ni image excédentaire.
        let echantillons = echantillons_synthetiques(5);
        let mut decodeur = create_decoder(CodecKind::H264).expect("décodeur");
        let mut livrees = 0u32;
        decoder_vers(echantillons, decodeur.as_mut(), |_image| {
            livrees += 1;
            false
        })
        .expect("arrêt propre");
        assert_eq!(livrees, 1, "plus rien n'est livré après l'annulation");
    }

    #[test]
    fn infos_fichier_absent_echoue() {
        let err = infos("dossier/inexistant/enregistrement.mp4".to_owned())
            .expect_err("un fichier absent doit être refusé");
        assert!(err.contains("impossible"), "message peu utile : {err}");
    }
}
