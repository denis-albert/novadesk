/// Façade locale et neutre en remplacement du plugin `window_manager`.
///
/// Le plugin natif `window_manager` (et son transitif `screen_retriever`)
/// exige le support des liens symboliques — donc le « mode développeur »
/// Windows — au moment de la compilation. Sur un poste sans droits
/// administrateur, cette contrainte empêche `flutter build windows`.
///
/// Cette façade sans dépendance expose la même surface d'API que celle
/// utilisée dans l'app (fenêtrage/plein écran) sous forme de no-ops : l'app se
/// compile et s'exécute avec le fenêtrage par défaut de l'OS. Pour retrouver le
/// contrôle programmatique de la fenêtre, réintroduire `window_manager` dans
/// `pubspec.yaml` et remplacer les imports de ce fichier par le paquet.
library;

import 'dart:async';
import 'dart:ui' show Size;

/// Miroir de `WindowOptions` du paquet `window_manager`.
class WindowOptions {
  const WindowOptions({
    this.size,
    this.minimumSize,
    this.center,
    this.title,
  });

  final Size? size;
  final Size? minimumSize;
  final bool? center;
  final String? title;
}

/// Implémentation neutre : chaque appel est un no-op qui préserve la sémantique
/// asynchrone attendue par les appelants.
class _WindowManagerStub {
  const _WindowManagerStub();

  Future<void> ensureInitialized() async {}

  Future<void> waitUntilReadyToShow(
    WindowOptions options, [
    Future<void> Function()? callback,
  ]) async {
    if (callback != null) {
      await callback();
    }
  }

  Future<void> show() async {}

  Future<void> focus() async {}

  Future<void> setFullScreen(bool fullScreen) async {}

  Future<bool> isFullScreen() async => false;
}

/// Singleton calqué sur `windowManager` du paquet d'origine.
const windowManager = _WindowManagerStub();
