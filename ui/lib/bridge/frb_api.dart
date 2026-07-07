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
  // Session live — délégation aux fonctions générées (u64 ⇄ BigInt)
  // ---------------------------------------------------------------------------

  @override
  Future<int> startSession({
    required SessionConfigDto config,
    required SessionEndpointDto endpoint,
  }) async {
    try {
      final id = await frb.startSession(
        config: _configVers(config),
        endpoint: _endpointVers(endpoint),
      );
      return id.toInt();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<int> startSessionWithOptions({
    required SessionConfigDto config,
    required SessionEndpointDto endpoint,
    required SessionOptionsDto options,
  }) async {
    try {
      final id = await frb.startSessionWithOptions(
        config: _configVers(config),
        endpoint: _endpointVers(endpoint),
        options: _optionsVers(options),
      );
      return id.toInt();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<ListenInfoDto> sessionListenInfo(int id) async {
    try {
      final info = await frb.sessionListenInfo(id: BigInt.from(id));
      return ListenInfoDto(addr: info.addr, certDer: info.certDer);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Stream<SessionStateDto> sessionStateStream(int id) =>
      frb.sessionStateStream(id: BigInt.from(id)).map(_stateDepuis);

  @override
  Stream<VideoFrameDto> sessionVideoStream(int id) =>
      frb.sessionVideoStream(id: BigInt.from(id)).map(_frameDepuis);

  @override
  Future<SessionStateDto?> waitSessionState(
    int id, {
    required int timeoutMs,
  }) async {
    final s = await frb.waitSessionState(
      id: BigInt.from(id),
      timeoutMs: BigInt.from(timeoutMs),
    );
    return s == null ? null : _stateDepuis(s);
  }

  @override
  Future<List<VideoFrameDto>> collectVideoFrames(
    int id, {
    required int maxFrames,
    required int timeoutMs,
  }) async {
    final frames = await frb.collectVideoFrames(
      id: BigInt.from(id),
      maxFrames: maxFrames,
      timeoutMs: BigInt.from(timeoutMs),
    );
    return frames.map(_frameDepuis).toList();
  }

  @override
  Future<SessionStatsDto> sessionStats(int id) async {
    final s = await frb.sessionStats(id: BigInt.from(id));
    return _statsDepuis(s);
  }

  @override
  Future<String?> sessionLastError(int id) =>
      frb.sessionLastError(id: BigInt.from(id));

  @override
  Future<void> sendInput(int id, InputEventDto event) =>
      frb.sendInput(id: BigInt.from(id), event: _inputVers(event));

  @override
  Future<void> stopSession(int id) => frb.stopSession(id: BigInt.from(id));

  // ---------------------------------------------------------------------------
  // Hôte « accès non surveillé » — délégation aux fonctions générées
  // ---------------------------------------------------------------------------

  @override
  Future<int> startUnattendedHost({
    required int localId,
    required String rendezvous,
    required List<String> stunServers,
    required PermissionsDto permissions,
  }) async {
    try {
      final id = await frb.startUnattendedHost(
        localId: BigInt.from(localId),
        rendezvous: rendezvous,
        stunServers: stunServers,
        permissions: _permsVers(permissions),
      );
      return id.toInt();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Stream<IncomingRequestDto> unattendedIncomingStream(int hostId) =>
      frb
          .unattendedIncomingStream(hostId: BigInt.from(hostId))
          .map(_incomingDepuis);

  @override
  Future<void> approveIncoming({
    required int hostId,
    required int peerId,
    required bool accepter,
  }) async {
    try {
      await frb.approveIncoming(
        hostId: BigInt.from(hostId),
        peerId: BigInt.from(peerId),
        accepter: accepter,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<SessionStatsDto> unattendedStats(int hostId) async {
    final s = await frb.unattendedStats(hostId: BigInt.from(hostId));
    return _statsDepuis(s);
  }

  @override
  Future<void> stopUnattendedHost(int hostId) =>
      frb.stopUnattendedHost(hostId: BigInt.from(hostId));

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

  static SessionStateDto _stateDepuis(frb.SessionStateDto s) => switch (s) {
        frb.SessionStateDto.idle => SessionStateDto.idle,
        frb.SessionStateDto.resolving => SessionStateDto.resolving,
        frb.SessionStateDto.connecting => SessionStateDto.connecting,
        frb.SessionStateDto.handshaking => SessionStateDto.handshaking,
        frb.SessionStateDto.active => SessionStateDto.active,
        frb.SessionStateDto.reconnecting => SessionStateDto.reconnecting,
        frb.SessionStateDto.closed => SessionStateDto.closed,
      };

  static VideoFrameDto _frameDepuis(frb.VideoFrameDto f) =>
      VideoFrameDto(width: f.width, height: f.height, rgba: f.rgba);

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

  static frb.SessionConfigDto _configVers(SessionConfigDto c) =>
      frb.SessionConfigDto(
        role: _roleVers(c.role),
        localId: BigInt.from(c.localId),
        peerId: c.peerId == null ? null : BigInt.from(c.peerId!),
        permissions: _permsVers(c.permissions),
      );

  static frb.SessionEndpointDto _endpointVers(SessionEndpointDto e) =>
      switch (e) {
        SessionEndpointLoopback() => const frb.SessionEndpointDto.loopback(),
        SessionEndpointDirect(:final addr, :final certDer) =>
          frb.SessionEndpointDto.direct(addr: addr, certDer: certDer),
        SessionEndpointByRendezvous(
          :final server,
          :final stunServers,
          :final relay
        ) =>
          frb.SessionEndpointDto.byRendezvous(
            server: server,
            stunServers: stunServers,
            relay: relay,
          ),
      };

  static frb.SessionOptionsDto _optionsVers(SessionOptionsDto o) =>
      frb.SessionOptionsDto(
        permissions: _permsVers(o.permissions),
        recordingPath: o.recordingPath,
        deltaMode: o.deltaMode,
      );

  /// Conversion des statistiques (u64 ⇄ BigInt ; `targetBitrateKbps`,
  /// `abrLevel` et `reconnects` sont déjà des `int` côté généré).
  static SessionStatsDto _statsDepuis(frb.SessionStatsDto s) => SessionStatsDto(
        fps: s.fps,
        rttUs: s.rttUs.toInt(),
        bytesIn: s.bytesIn.toInt(),
        bytesOut: s.bytesOut.toInt(),
        frames: s.frames.toInt(),
        inputsDenied: s.inputsDenied.toInt(),
        targetBitrateKbps: s.targetBitrateKbps,
        abrLevel: s.abrLevel,
        framesRecorded: s.framesRecorded.toInt(),
        reconnects: s.reconnects,
        encoderBackend: s.encoderBackend,
      );

  static IncomingRequestDto _incomingDepuis(frb.IncomingRequestDto r) =>
      IncomingRequestDto(
        peerId: r.peerId.toInt(),
        peerIdFormate: r.peerIdFormate,
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
