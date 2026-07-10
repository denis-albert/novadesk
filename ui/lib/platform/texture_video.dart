/// Pont **dart:ffi** vers le rendu vidéo par **texture GPU** du cœur Rust
/// (`crates/nd-ffi/src/texture.rs`).
///
/// Le cœur expose trois symboles C directs depuis le cdylib `nd_ffi`
/// (`nd_texture_init` / `nd_texture_attach` / `nd_texture_dispose`) : on les
/// résout ici par `DynamicLibrary` — **aucune régénération `flutter_rust_bridge`
/// n'est requise** (ces fonctions n'utilisent aucun `StreamSink`).
///
/// Flux :
///  1. Dart obtient le *handle* du moteur Flutter courant
///     (`EngineContext.getEngineHandle`, plugin `irondash_engine_context`).
///  2. `nd_texture_init(handle)` crée une `Texture` PixelBuffer liée à ce moteur
///     et renvoie son `textureId` (le cœur `irondash_texture`).
///  3. `nd_texture_attach(sessionId, textureId)` relie la session : ses frames
///     décodées alimentent la texture (mise à jour hors thread UI, côté Rust).
///  4. L'UI affiche `Texture(textureId: …)`.
///  5. `nd_texture_dispose(textureId)` libère la texture au `dispose` de l'écran.
///
/// **Repli** : toute défaillance (plugin/DLL absent, symboles manquants, moteur
/// indisponible) renvoie `null`/`false` — l'appelant conserve alors le rendu CPU
/// historique (`decodeImageFromPixels`).
library;

import 'dart:ffi';
import 'dart:io' show Platform;

import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:irondash_engine_context/irondash_engine_context.dart';

// Signatures FFI des trois symboles exportés par `nd_ffi` (voir texture.rs).
typedef _TextureInitC = Int64 Function(Int64 engineHandle);
typedef _TextureInitDart = int Function(int engineHandle);
typedef _TextureAttachC = Int32 Function(Int64 sessionId, Int64 textureId);
typedef _TextureAttachDart = int Function(int sessionId, int textureId);
typedef _TextureDisposeC = Void Function(Int64 textureId);
typedef _TextureDisposeDart = void Function(int textureId);

/// Accès au rendu par texture GPU du cœur. Instancié par [charger] uniquement
/// si la bibliothèque native et ses symboles `nd_texture_*` sont disponibles.
class TextureVideo {
  TextureVideo._(this._init, this._attach, this._dispose);

  final _TextureInitDart _init;
  final _TextureAttachDart _attach;
  final _TextureDisposeDart _dispose;

  /// Plateformes desktop où la texture GPU (irondash) est pertinente.
  static bool get _supporteBureau =>
      !kIsWeb && (Platform.isWindows || Platform.isMacOS || Platform.isLinux);

  /// Charge le pont si possible ; `null` si la plateforme n'est pas desktop, si
  /// la DLL `nd_ffi` n'est pas chargeable, ou si elle n'exporte pas encore les
  /// symboles `nd_texture_*` (ancienne DLL / façade mock) → repli CPU.
  static TextureVideo? charger() {
    if (!_supporteBureau) return null;
    try {
      // La DLL est déjà chargée par `RustLib.init` (main.dart) : l'ouvrir à
      // nouveau renvoie le même module (compteur de références de l'OS).
      final lib = DynamicLibrary.open(_nomBibliotheque());
      return TextureVideo._(
        lib.lookupFunction<_TextureInitC, _TextureInitDart>('nd_texture_init'),
        lib.lookupFunction<_TextureAttachC, _TextureAttachDart>(
            'nd_texture_attach'),
        lib.lookupFunction<_TextureDisposeC, _TextureDisposeDart>(
            'nd_texture_dispose'),
      );
    } catch (_) {
      return null;
    }
  }

  /// Crée une texture liée au moteur Flutter courant et renvoie son `textureId`,
  /// ou `null` en cas d'échec (repli CPU).
  ///
  /// La création doit s'exécuter sur le **thread plateforme** : l'`await` de
  /// [EngineContext.getEngineHandle] rend la main sur l'isolate racine (thread
  /// plateforme sur desktop), d'où l'appel FFI synchrone [_init] est légal.
  Future<int?> creer() async {
    try {
      final handle = await EngineContext.instance.getEngineHandle();
      final id = _init(handle);
      return id < 0 ? null : id;
    } catch (_) {
      return null;
    }
  }

  /// Relie la session [sessionId] à la texture [textureId] (le cœur route alors
  /// ses frames vers la texture). Renvoie `true` si l'attache a réussi.
  bool attacher(int sessionId, int textureId) {
    try {
      return _attach(sessionId, textureId) == 0;
    } catch (_) {
      return false;
    }
  }

  /// Libère la texture [textureId] (idempotent côté cœur).
  void liberer(int textureId) {
    try {
      _dispose(textureId);
    } catch (_) {
      // Libération best-effort.
    }
  }

  /// Nom de la bibliothèque dynamique `nd-ffi` selon la plateforme (identique à
  /// celui chargé par `RustLib.init` dans `main.dart`).
  static String _nomBibliotheque() {
    if (Platform.isWindows) return 'nd_ffi.dll';
    if (Platform.isMacOS) return 'libnd_ffi.dylib';
    return 'libnd_ffi.so';
  }
}
