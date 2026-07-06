/// Adaptateur reliant l'interface [NativeApi] (types de `native_api.dart`,
/// utilisés partout dans l'UI) au binding généré par `flutter_rust_bridge`
/// (cœur Rust `nd-ffi`, fonctions de `generated/api.dart`).
///
/// Rôle : traduire les types **générés** (préfixe `frb`) vers/depuis les types
/// **de l'UI**, convertir `u64` (exposé en `BigInt` par FRB) ↔ `int`, et
/// retransformer les erreurs `Result<_, String>` (que FRB lève comme `String`)
/// en [NovaApiException]. Le basculement mock ↔ réel se fait dans `main()`
/// (override du `nativeApiProvider`), sans toucher aux écrans.
library;

import 'dart:typed_data';

import 'generated/api.dart' as frb;
import 'native_api.dart';

/// Implémentation réelle de [NativeApi] : délègue au cœur Rust via FRB.
class FrbNativeApi implements NativeApi {
  const FrbNativeApi();

  // ---------------------------------------------------------------------------
  // Informations générales
  // ---------------------------------------------------------------------------

  @override
  Future<AppInfo> appInfo() async {
    final info = await frb.appInfo();
    return AppInfo(version: info.version);
  }

  @override
  Future<String> engineVersionString() async {
    // `engine_version_string()` vit à la racine de `nd-ffi` (hors module `api`)
    // et n'est donc pas exposé par le binding ; la version vient d'`app_info()`.
    final info = await frb.appInfo();
    return info.version;
  }

  // ---------------------------------------------------------------------------
  // ID NovaDesk
  // ---------------------------------------------------------------------------

  @override
  Future<String> formatNovaId({required int id}) =>
      frb.formatNovaId(id: BigInt.from(id));

  @override
  Future<int> parseNovaId({required String texte}) async {
    try {
      final valeur = await frb.parseNovaId(texte: texte);
      return valeur.toInt();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  // ---------------------------------------------------------------------------
  // Statut, permissions, configuration
  // ---------------------------------------------------------------------------

  @override
  Future<SessionStatusDto> sessionStatus({
    required SessionStateDto state,
    int? peerId,
  }) async {
    final s = await frb.sessionStatus(
      state: _stateVers(state),
      peerId: peerId == null ? null : BigInt.from(peerId),
    );
    return SessionStatusDto(state: s.state, peer: s.peer);
  }

  @override
  Future<SessionConfigDto> newSessionConfig({
    required SessionRoleDto role,
    required int localId,
    int? peerId,
    required PermissionsDto permissions,
  }) async {
    try {
      final c = await frb.newSessionConfig(
        role: _roleVers(role),
        localId: BigInt.from(localId),
        peerId: peerId == null ? null : BigInt.from(peerId),
        permissions: _permsVers(permissions),
      );
      return SessionConfigDto(
        role: _roleDepuis(c.role),
        localId: c.localId.toInt(),
        peerId: c.peerId?.toInt(),
        permissions: _permsDepuis(c.permissions),
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  // ---------------------------------------------------------------------------
  // Événements d'entrée
  // ---------------------------------------------------------------------------

  @override
  Future<Uint8List> encodeInputEvent({required InputEventDto event}) =>
      frb.encodeInputEvent(event: _inputVers(event));

  @override
  Future<InputEventDto> decodeInputEvent({required Uint8List data}) async {
    try {
      final e = await frb.decodeInputEvent(data: data);
      return _inputDepuis(e);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  // ---------------------------------------------------------------------------
  // Conversions internes
  // ---------------------------------------------------------------------------

  /// FRB lève la `String` d'erreur telle quelle pour un `Result<_, String>`.
  static String _message(Object e) {
    if (e is NovaApiException) return e.message;
    if (e is String) return e;
    return e.toString();
  }

  static frb.SessionRoleDto _roleVers(SessionRoleDto r) => switch (r) {
        SessionRoleDto.controller => frb.SessionRoleDto.controller,
        SessionRoleDto.controlled => frb.SessionRoleDto.controlled,
      };

  static SessionRoleDto _roleDepuis(frb.SessionRoleDto r) => switch (r) {
        frb.SessionRoleDto.controller => SessionRoleDto.controller,
        frb.SessionRoleDto.controlled => SessionRoleDto.controlled,
      };

  static frb.SessionStateDto _stateVers(SessionStateDto s) => switch (s) {
        SessionStateDto.idle => frb.SessionStateDto.idle,
        SessionStateDto.resolving => frb.SessionStateDto.resolving,
        SessionStateDto.connecting => frb.SessionStateDto.connecting,
        SessionStateDto.handshaking => frb.SessionStateDto.handshaking,
        SessionStateDto.active => frb.SessionStateDto.active,
        SessionStateDto.reconnecting => frb.SessionStateDto.reconnecting,
        SessionStateDto.closed => frb.SessionStateDto.closed,
      };

  static frb.PermissionsDto _permsVers(PermissionsDto p) => frb.PermissionsDto(
        keyboard: p.keyboard,
        mouse: p.mouse,
        clipboard: p.clipboard,
        files: p.files,
        audio: p.audio,
        viewOnly: p.viewOnly,
      );

  static PermissionsDto _permsDepuis(frb.PermissionsDto p) => PermissionsDto(
        keyboard: p.keyboard,
        mouse: p.mouse,
        clipboard: p.clipboard,
        files: p.files,
        audio: p.audio,
        viewOnly: p.viewOnly,
      );

  static frb.InputEventDto _inputVers(InputEventDto e) => switch (e) {
        InputMouseMoveAbs(:final x, :final y, :final monitor) =>
          frb.InputEventDto.mouseMoveAbs(x: x, y: y, monitor: monitor),
        InputMouseMoveRel(:final dx, :final dy) =>
          frb.InputEventDto.mouseMoveRel(dx: dx, dy: dy),
        InputMouseButton(:final button, :final down) =>
          frb.InputEventDto.mouseButton(button: button, down: down),
        InputScroll(:final dx, :final dy) =>
          frb.InputEventDto.scroll(dx: dx, dy: dy),
        InputKey(:final scancode, :final down) =>
          frb.InputEventDto.key(scancode: scancode, down: down),
        InputUnicode(:final codepoint) =>
          frb.InputEventDto.unicode(codepoint: codepoint),
      };

  static InputEventDto _inputDepuis(frb.InputEventDto e) => switch (e) {
        frb.InputEventDto_MouseMoveAbs(:final x, :final y, :final monitor) =>
          InputMouseMoveAbs(x: x, y: y, monitor: monitor),
        frb.InputEventDto_MouseMoveRel(:final dx, :final dy) =>
          InputMouseMoveRel(dx: dx, dy: dy),
        frb.InputEventDto_MouseButton(:final button, :final down) =>
          InputMouseButton(button: button, down: down),
        frb.InputEventDto_Scroll(:final dx, :final dy) =>
          InputScroll(dx: dx, dy: dy),
        frb.InputEventDto_Key(:final scancode, :final down) =>
          InputKey(scancode: scancode, down: down),
        frb.InputEventDto_Unicode(:final codepoint) =>
          InputUnicode(codepoint: codepoint),
      };
}
