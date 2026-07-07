/// Implémentation **fictive** de [NativeApi], en Dart pur, pour développer et
/// naviguer dans l'UI sans le pont `flutter_rust_bridge` ni le cœur Rust.
///
/// La logique (formatage/analyse d'ID, validations, messages d'erreur
/// français) reproduit fidèlement `crates/nd-ffi/src/api.rs` afin que le
/// remplacement par le binding réel soit transparent pour les écrans.
///
/// ATTENTION : l'encodage binaire des événements d'entrée est un format
/// **factice** propre à ce mock (auto-cohérent : `decode(encode(e)) == e`),
/// il ne correspond PAS à l'encodage réel de `nd-proto`. Seul le binding
/// généré produit les octets attendus par le canal `Input`.
library;

import 'dart:math';
import 'dart:typed_data';

import 'native_api.dart';

/// Façade fictive : mêmes signatures, mêmes messages, zéro FFI.
class MockNativeApi implements NativeApi {
  /// Valeur factice alignée sur `nd_core::engine_version()` au moment de
  /// l'écriture de ce mock.
  static const String _versionMoteur = '0.1';

  // -------------------------------------------------------------------------
  // Informations générales
  // -------------------------------------------------------------------------

  @override
  Future<AppInfo> appInfo() async => const AppInfo(version: _versionMoteur);

  @override
  Future<String> engineVersionString() async => _versionMoteur;

  // -------------------------------------------------------------------------
  // ID NovaDesk
  // -------------------------------------------------------------------------

  @override
  Future<String> formatNovaId({required int id}) async => _formater(id);

  /// 9 chiffres minimum (complétés par des zéros de tête), groupés par 3
  /// depuis la droite — même rendu que `NovaId::to_string()`.
  static String _formater(int id) {
    var chiffres = id.toString();
    if (chiffres.length < 9) {
      chiffres = chiffres.padLeft(9, '0');
    }
    final groupes = <String>[];
    for (var fin = chiffres.length; fin > 0; fin -= 3) {
      final debut = fin - 3 < 0 ? 0 : fin - 3;
      groupes.insert(0, chiffres.substring(debut, fin));
    }
    return groupes.join(' ');
  }

  @override
  Future<int> parseNovaId({required String texte}) async {
    // Retire tout espacement, y compris les espaces insécables d'un
    // copier-coller (comme `parse_nova_id` côté Rust).
    final chiffres = texte.replaceAll(RegExp(r'\s+'), '');
    if (chiffres.isEmpty) {
      throw const NovaApiException("l'ID NovaDesk est vide");
    }
    for (final rune in chiffres.runes) {
      if (rune < 0x30 || rune > 0x39) {
        final c = String.fromCharCode(rune);
        throw NovaApiException("caractère invalide dans l'ID NovaDesk : « $c »");
      }
    }
    // Nuance vs Rust : `int` Dart est signé 64 bits (u64 côté Rust) ; sans
    // incidence pour les ID à 9 chiffres. Au-delà, on refuse proprement.
    final valeur = int.tryParse(chiffres);
    if (valeur == null || valeur < 0) {
      throw NovaApiException('ID NovaDesk trop long : « $chiffres »');
    }
    return valeur;
  }

  // -------------------------------------------------------------------------
  // Statut, permissions, configuration
  // -------------------------------------------------------------------------

  @override
  Future<SessionStatusDto> sessionStatus({
    required SessionStateDto state,
    int? peerId,
  }) async {
    return SessionStatusDto(
      state: state.label,
      peer: peerId == null ? null : _formater(peerId),
    );
  }

  @override
  Future<SessionConfigDto> newSessionConfig({
    required SessionRoleDto role,
    required int localId,
    int? peerId,
    required PermissionsDto permissions,
  }) async {
    if (role == SessionRoleDto.controller && peerId == null) {
      throw const NovaApiException(
          "le rôle contrôleur nécessite l'ID du pair à joindre");
    }
    if (peerId != null && peerId == localId) {
      throw NovaApiException(
        "l'ID du pair (${_formater(localId)}) est identique à l'ID local : "
        'impossible de se connecter à soi-même',
      );
    }
    return SessionConfigDto(
      role: role,
      localId: localId,
      peerId: peerId,
      permissions: permissions,
    );
  }

  // -------------------------------------------------------------------------
  // Événements d'entrée — encodage FACTICE (étiquette 1 octet + charge
  // petit-boutiste), uniquement pour l'aller-retour du mock.
  // -------------------------------------------------------------------------

  static const int _etqMouseMoveAbs = 0;
  static const int _etqMouseMoveRel = 1;
  static const int _etqMouseButton = 2;
  static const int _etqScroll = 3;
  static const int _etqKey = 4;
  static const int _etqUnicode = 5;

  @override
  Future<Uint8List> encodeInputEvent({required InputEventDto event}) async {
    switch (event) {
      case InputMouseMoveAbs(:final x, :final y, :final monitor):
        final d = ByteData(21)
          ..setUint8(0, _etqMouseMoveAbs)
          ..setFloat64(1, x, Endian.little)
          ..setFloat64(9, y, Endian.little)
          ..setUint32(17, monitor, Endian.little);
        return d.buffer.asUint8List();
      case InputMouseMoveRel(:final dx, :final dy):
        final d = ByteData(17)
          ..setUint8(0, _etqMouseMoveRel)
          ..setFloat64(1, dx, Endian.little)
          ..setFloat64(9, dy, Endian.little);
        return d.buffer.asUint8List();
      case InputMouseButton(:final button, :final down):
        final d = ByteData(3)
          ..setUint8(0, _etqMouseButton)
          ..setUint8(1, button)
          ..setUint8(2, down ? 1 : 0);
        return d.buffer.asUint8List();
      case InputScroll(:final dx, :final dy):
        final d = ByteData(17)
          ..setUint8(0, _etqScroll)
          ..setFloat64(1, dx, Endian.little)
          ..setFloat64(9, dy, Endian.little);
        return d.buffer.asUint8List();
      case InputKey(:final scancode, :final down):
        final d = ByteData(6)
          ..setUint8(0, _etqKey)
          ..setUint32(1, scancode, Endian.little)
          ..setUint8(5, down ? 1 : 0);
        return d.buffer.asUint8List();
      case InputUnicode(:final codepoint):
        final d = ByteData(5)
          ..setUint8(0, _etqUnicode)
          ..setUint32(1, codepoint, Endian.little);
        return d.buffer.asUint8List();
    }
  }

  @override
  Future<InputEventDto> decodeInputEvent({required Uint8List data}) async {
    InputEventDto? evenement;
    if (data.isNotEmpty) {
      final d = ByteData.sublistView(data);
      switch (data[0]) {
        case _etqMouseMoveAbs:
          if (data.length == 21) {
            evenement = InputMouseMoveAbs(
              x: d.getFloat64(1, Endian.little),
              y: d.getFloat64(9, Endian.little),
              monitor: d.getUint32(17, Endian.little),
            );
          }
        case _etqMouseMoveRel:
          if (data.length == 17) {
            evenement = InputMouseMoveRel(
              dx: d.getFloat64(1, Endian.little),
              dy: d.getFloat64(9, Endian.little),
            );
          }
        case _etqMouseButton:
          if (data.length == 3) {
            evenement = InputMouseButton(
              button: d.getUint8(1),
              down: d.getUint8(2) != 0,
            );
          }
        case _etqScroll:
          if (data.length == 17) {
            evenement = InputScroll(
              dx: d.getFloat64(1, Endian.little),
              dy: d.getFloat64(9, Endian.little),
            );
          }
        case _etqKey:
          if (data.length == 6) {
            evenement = InputKey(
              scancode: d.getUint32(1, Endian.little),
              down: d.getUint8(5) != 0,
            );
          }
        case _etqUnicode:
          if (data.length == 5) {
            evenement = InputUnicode(codepoint: d.getUint32(1, Endian.little));
          }
      }
    }
    if (evenement == null) {
      throw NovaApiException(
          "événement d'entrée illisible (${data.length} octet(s) reçus)");
    }
    return evenement;
  }

  // -------------------------------------------------------------------------
  // Session live — flux de synthèse (démontre le rendu SANS le cœur natif)
  // -------------------------------------------------------------------------

  int _prochainIdSession = 1;
  final Map<int, DateTime> _debutSession = {};
  final Random _alea = Random();

  @override
  Future<int> startSession({
    required SessionConfigDto config,
    required SessionEndpointDto endpoint,
  }) async {
    final id = _prochainIdSession++;
    _debutSession[id] = DateTime.now();
    return id;
  }

  @override
  Future<ListenInfoDto> sessionListenInfo(int id) async {
    // Hôte fictif en écoute loopback : adresse plausible, certificat vide.
    return ListenInfoDto(addr: '127.0.0.1:53211', certDer: Uint8List(0));
  }

  /// Progression d'état de synthèse : `resolving → connecting → handshaking →
  /// active`, avec des délais réalistes, puis reste actif jusqu'à l'annulation.
  @override
  Stream<SessionStateDto> sessionStateStream(int id) async* {
    const etapes = <(SessionStateDto, int)>[
      (SessionStateDto.resolving, 350),
      (SessionStateDto.connecting, 500),
      (SessionStateDto.handshaking, 450),
      (SessionStateDto.active, 0),
    ];
    for (final (etat, ms) in etapes) {
      if (ms > 0) {
        await Future<void>.delayed(Duration(milliseconds: ms));
      }
      yield etat;
    }
  }

  /// ~30 trames/s d'une mire animée 320×180 (barres de couleur + réticule de
  /// balayage rouge NovaDesk) : le rendu `VideoFrameDto → ui.Image → CustomPaint`
  /// est ainsi **démontrable sans DLL native**. S'arrête à l'annulation.
  @override
  Stream<VideoFrameDto> sessionVideoStream(int id) {
    return Stream<VideoFrameDto>.periodic(
      const Duration(milliseconds: 33),
      (tick) => _genererFrameMire(tick),
    );
  }

  @override
  Future<SessionStateDto?> waitSessionState(
    int id, {
    required int timeoutMs,
  }) async {
    // Repli synchrone (l'UI consomme le flux) : renvoie l'état actif après un
    // court délai borné par [timeoutMs].
    await Future<void>.delayed(
      Duration(milliseconds: min(timeoutMs, 50)),
    );
    return SessionStateDto.active;
  }

  @override
  Future<List<VideoFrameDto>> collectVideoFrames(
    int id, {
    required int maxFrames,
    required int timeoutMs,
  }) async {
    // Repli synchrone : renvoie quelques trames de synthèse (borné).
    final n = max(0, min(maxFrames, 8));
    return List<VideoFrameDto>.generate(n, _genererFrameMire);
  }

  @override
  Future<SessionStatsDto> sessionStats(int id) async {
    final debut = _debutSession[id];
    final secondes = debut == null
        ? 0.0
        : DateTime.now().difference(debut).inMilliseconds / 1000.0;
    // Valeurs plausibles avec un léger jitter, montant avec la durée.
    final fps = secondes < 0.5 ? 0.0 : 29.0 + _alea.nextDouble() * 2.0;
    return SessionStatsDto(
      fps: fps,
      rttUs: 9000 + _alea.nextInt(7000),
      bytesIn: (secondes * 2100000).round(),
      bytesOut: (secondes * 7600).round(),
      frames: (secondes * 30).round(),
    );
  }

  @override
  Future<String?> sessionLastError(int id) async => null;

  @override
  Future<void> sendInput(int id, InputEventDto event) async {
    // Le mock accepte et ignore l'entrée (aucun pair réel à piloter).
  }

  @override
  Future<void> stopSession(int id) async {
    _debutSession.remove(id);
  }

  // Barres facon mire télé : blanc, jaune, cyan, vert, magenta, rouge, bleu, noir.
  static const List<List<int>> _barresMire = [
    [236, 239, 244],
    [232, 205, 74],
    [86, 199, 214],
    [96, 194, 120],
    [206, 106, 197],
    [231, 86, 76],
    [86, 118, 224],
    [26, 28, 34],
  ];

  /// Génère une trame 320×180 : 8 barres de couleur, un ombrage diagonal qui
  /// défile (animation visible) et un réticule de balayage rouge #EF443B.
  VideoFrameDto _genererFrameMire(int tick) {
    const int w = 320;
    const int h = 180;
    final rgba = Uint8List(w * h * 4);
    final int balayageX = tick % w;
    final int balayageY = tick % h;
    final int defile = tick * 3;
    var i = 0;
    for (var y = 0; y < h; y++) {
      final bool ligneH = (y - balayageY).abs() < 1;
      for (var x = 0; x < w; x++) {
        final barre = _barresMire[(x * 8) ~/ w];
        // Bandes sombres diagonales qui défilent avec le temps.
        final int onde = ((x + y + defile) & 63) < 32 ? 0 : 20;
        int r = barre[0] - onde;
        int g = barre[1] - onde;
        int b = barre[2] - onde;
        // Réticule de balayage aux couleurs NovaDesk (#EF443B).
        if ((x - balayageX).abs() < 1 || ligneH) {
          r = 0xEF;
          g = 0x44;
          b = 0x3B;
        }
        rgba[i++] = r < 0 ? 0 : (r > 255 ? 255 : r);
        rgba[i++] = g < 0 ? 0 : (g > 255 ? 255 : g);
        rgba[i++] = b < 0 ? 0 : (b > 255 ? 255 : b);
        rgba[i++] = 255;
      }
    }
    return VideoFrameDto(width: w, height: h, rgba: rgba);
  }
}
