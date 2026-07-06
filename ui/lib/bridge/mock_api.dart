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
}
