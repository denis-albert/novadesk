//! Rendu vidéo par **texture GPU** : pont entre le cœur (frames décodées) et le
//! **registre de textures Flutter**, via `irondash_texture` /
//! `irondash_engine_context`.
//!
//! # Principe
//!
//! Le chemin historique pousse chaque [`nd_codec::DecodedFrame`] (RGBA) vers le
//! Dart par le flux `session_video_stream`, où elle est décodée en `ui.Image`
//! (`decodeImageFromPixels`) puis peinte au CPU. Ce module ajoute un chemin
//! **texture** : le Dart obtient le *handle* du moteur Flutter
//! (`IrondashEngineContext.getEngineHandle`) et le passe ici ; on crée alors une
//! `Texture` PixelBuffer liée à ce moteur et on renvoie son **identifiant de
//! texture**. À chaque trame de la session attachée, on remplit le tampon de la
//! texture et on signale « image disponible » — Flutter téléverse les pixels
//! (CPU→GPU) et affiche `Texture(textureId: …)`. Aucun pixel ne transite alors
//! par le pont FRB (le flux Dart ne reçoit qu'un « tick » de dimensions).
//!
//! # Honnêteté PixelBuffer vs zéro-copie
//!
//! C'est une texture **PixelBuffer** : l'upload CPU→GPU est fait par Flutter à
//! partir d'un `Vec<u8>` RGBA. Ce n'est **pas** du zéro-copie D3D11 partagé (qui
//! exigerait `irondash_texture` en mode `TextureDescriptor`/`ID3D11Texture2D` et
//! que l'encodeur/décodeur expose une surface GPU partagée). Le gain par rapport
//! au chemin CPU : plus de `decodeImageFromPixels` côté Dart ni de marshalling
//! des pixels par le pont ; l'affichage passe par le compositeur GPU de Flutter.
//!
//! # Threads
//!
//! [`creer_texture`] doit s'exécuter sur le **thread plateforme** (l'appel
//! `nd_texture_init` via `dart:ffi` depuis l'isolate racine y satisfait :
//! l'isolate racine tourne sur ce thread). [`SendableTexture::mark_frame_available`]
//! est en revanche appelable depuis **n'importe quel thread** — ici le thread de
//! drainage vidéo (`crate::flux`).
//!
//! # Contrat FFI (dart:ffi direct — aucune régénération `frb` requise)
//!
//! * `nd_texture_init(engine_handle: i64) -> i64` — crée la texture, renvoie son
//!   `textureId` (`>= 0`) ou `-1` en cas d'échec.
//! * `nd_texture_attach(session_id: i64, texture_id: i64) -> i32` — relie une
//!   session à une texture (`0` si succès, `-1` si la texture est inconnue).
//! * `nd_texture_dispose(texture_id: i64)` — libère la texture.
//!
//! Ces trois entrées sont des symboles C **exportés par le cdylib** (`nd_ffi`),
//! appelés directement par `ui/lib/platform/texture_video.dart`. Elles
//! n'utilisent aucun `StreamSink` et **ne passent pas par le codegen FRB**.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use irondash_texture::{
    BoxedPixelData, PayloadProvider, SendableTexture, SimplePixelData, Texture,
};
use nd_codec::DecodedFrame;

/// Dernière trame RGBA prête à être téléversée dans la texture.
struct TrameRgba {
    largeur: i32,
    hauteur: i32,
    /// Pixels RGBA (le format attendu par `irondash_texture` sous Windows est
    /// `RGBA`, identique à [`DecodedFrame::rgba`] — aucun échange R/B).
    rgba: Vec<u8>,
}

/// Fournisseur de pixels interrogé par Flutter : renvoie la dernière trame
/// poussée. Partage son tampon avec le [`HolderTexture`] (écriture côté cœur,
/// lecture côté Flutter).
struct FournisseurPixels {
    tampon: Arc<Mutex<Option<TrameRgba>>>,
}

impl PayloadProvider<BoxedPixelData> for FournisseurPixels {
    fn get_payload(&self) -> BoxedPixelData {
        let garde = self.tampon.lock().unwrap_or_else(PoisonError::into_inner);
        match garde.as_ref() {
            Some(t) => SimplePixelData::new_boxed(t.largeur, t.hauteur, t.rgba.clone()),
            // Aucune trame encore reçue : 1×1 transparent (jamais de panique).
            None => SimplePixelData::new_boxed(1, 1, vec![0, 0, 0, 0]),
        }
    }
}

/// Une texture vivante : sa poignée « envoyable » (signal inter-thread) et le
/// tampon partagé avec son fournisseur de pixels.
struct HolderTexture {
    sendable: Arc<SendableTexture<BoxedPixelData>>,
    tampon: Arc<Mutex<Option<TrameRgba>>>,
}

/// Table des textures vivantes, indexée par `textureId`.
type TableTextures = Mutex<HashMap<i64, HolderTexture>>;

/// Table statique des textures du processus.
static TEXTURES: OnceLock<TableTextures> = OnceLock::new();

/// Verrouille la table des textures (empoisonnement absorbé, cf. `crate::flux`).
fn verrou_textures() -> MutexGuard<'static, HashMap<i64, HolderTexture>> {
    TEXTURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Association `session → texture` (au plus une texture par session).
type TableAssoc = Mutex<HashMap<u64, i64>>;

/// Table statique des associations session→texture.
static ASSOC: OnceLock<TableAssoc> = OnceLock::new();

/// Verrouille la table des associations (empoisonnement absorbé).
fn verrou_assoc() -> MutexGuard<'static, HashMap<u64, i64>> {
    ASSOC
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Crée une texture PixelBuffer liée au moteur `engine_handle`, l'enregistre et
/// renvoie son `textureId` (`>= 0`), ou `-1` en cas d'échec (handle inconnu,
/// registre de textures indisponible…).
///
/// **À appeler sur le thread plateforme** (cf. doc du module).
pub(crate) fn creer_texture(engine_handle: i64) -> i64 {
    let tampon: Arc<Mutex<Option<TrameRgba>>> = Arc::new(Mutex::new(None));
    let fournisseur = Arc::new(FournisseurPixels {
        tampon: Arc::clone(&tampon),
    });
    let texture = match Texture::new_with_provider(engine_handle, fournisseur) {
        Ok(t) => t,
        Err(_) => return -1,
    };
    let id = texture.id();
    let sendable = texture.into_sendable_texture();
    verrou_textures().insert(id, HolderTexture { sendable, tampon });
    id
}

/// Relie la session `session_id` à la texture `texture_id` : les trames de la
/// session alimenteront désormais cette texture. Renvoie `0` si la texture
/// existe, `-1` sinon.
pub(crate) fn attacher(session_id: u64, texture_id: i64) -> i32 {
    if !verrou_textures().contains_key(&texture_id) {
        return -1;
    }
    verrou_assoc().insert(session_id, texture_id);
    0
}

/// Libère la texture `texture_id` (et retire toute association pointant vers
/// elle). Idempotent.
pub(crate) fn liberer_texture(texture_id: i64) {
    verrou_textures().remove(&texture_id);
    verrou_assoc().retain(|_, tid| *tid != texture_id);
}

/// Détache toute texture de la session `id` (appelé à l'arrêt de la session).
/// La texture elle-même reste vivante jusqu'à [`liberer_texture`] (pilotée par
/// le Dart au `dispose` de l'écran).
pub(crate) fn detacher_session(id: u64) {
    verrou_assoc().remove(&id);
}

/// Achemine une trame décodée vers la texture de la session, si une texture y
/// est attachée.
///
/// Renvoie `Ok((largeur, hauteur))` quand la trame a été **consommée par la
/// texture** — le cœur enverra alors un simple « tick » (dimensions, sans
/// pixels) au flux Dart —, ou `Err(frame)` en **repli** (aucune texture : le
/// flux Dart reçoit les pixels RGBA comme historiquement).
pub(crate) fn consommer_frame(
    session_id: u64,
    frame: DecodedFrame,
) -> Result<(u32, u32), DecodedFrame> {
    let Some(texture_id) = verrou_assoc().get(&session_id).copied() else {
        return Err(frame);
    };
    // Clone les `Arc` sous verrou puis relâche la table avant l'upload : le
    // verrou des textures n'est jamais tenu pendant `mark_frame_available`.
    let (sendable, tampon) = {
        let table = verrou_textures();
        match table.get(&texture_id) {
            Some(h) => (Arc::clone(&h.sendable), Arc::clone(&h.tampon)),
            None => return Err(frame),
        }
    };
    let (largeur, hauteur) = (frame.width, frame.height);
    {
        let mut garde = tampon.lock().unwrap_or_else(PoisonError::into_inner);
        *garde = Some(TrameRgba {
            largeur: largeur as i32,
            hauteur: hauteur as i32,
            rgba: frame.rgba,
        });
    }
    sendable.mark_frame_available();
    Ok((largeur, hauteur))
}

// ---------------------------------------------------------------------------
// Points d'entrée FFI directs (dart:ffi) — symboles C exportés par le cdylib.
// Aucune régénération `frb` requise : le Dart les résout par `DynamicLibrary`.
//
// `#[no_mangle]` déclenche le lint `unsafe_code` (la table des symboles exportés
// échappe aux garanties du compilateur) : on l'`allow` localement, comme le fait
// le pont généré `frb_generated` en tête de `lib.rs`. Ces fonctions ne
// déréférencent aucun pointeur et ne peuvent pas paniquer (verrous absorbés,
// `Result` traité) — pas d'`unwind` à travers la frontière C.
// ---------------------------------------------------------------------------

/// Crée une texture liée au moteur `engine_handle` et renvoie son `textureId`
/// (`>= 0`) ou `-1`. Voir [`creer_texture`].
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn nd_texture_init(engine_handle: i64) -> i64 {
    creer_texture(engine_handle)
}

/// Relie la session `session_id` à la texture `texture_id` (`0` si succès, `-1`
/// si la texture est inconnue). Voir [`attacher`].
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn nd_texture_attach(session_id: i64, texture_id: i64) -> i32 {
    attacher(session_id as u64, texture_id)
}

/// Libère la texture `texture_id`. Voir [`liberer_texture`].
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn nd_texture_dispose(texture_id: i64) {
    liberer_texture(texture_id);
}
