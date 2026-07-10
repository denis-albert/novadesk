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

import 'dart:async';
import 'dart:math';

import 'package:flutter/foundation.dart';

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
  Future<int> startSessionWithOptions({
    required SessionConfigDto config,
    required SessionEndpointDto endpoint,
    required SessionOptionsDto options,
  }) {
    // Le mock ignore les options avancées : même comportement que
    // [startSession] (mire animée, flux d'états de synthèse).
    return startSession(config: config, endpoint: endpoint);
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
    final actif = secondes >= 0.5;
    // Valeurs plausibles avec un léger jitter, montant avec la durée.
    final fps = actif ? 29.0 + _alea.nextDouble() * 2.0 : 0.0;
    return SessionStatsDto(
      fps: fps,
      rttUs: 9000 + _alea.nextInt(7000),
      bytesIn: (secondes * 2100000).round(),
      bytesOut: (secondes * 7600).round(),
      frames: (secondes * 30).round(),
      // Statistiques enrichies de synthèse (démontrent le HUD du lot §2b).
      inputsDenied: 0,
      targetBitrateKbps: actif ? 6000 : 0,
      // L'ABR dégrade brièvement toutes les ~20 s (démo du palier).
      abrLevel: actif && (secondes % 20) > 17 ? 1 : 0,
      framesRecorded: 0,
      reconnects: secondes ~/ 45, // une reconnexion simulée toutes les ~45 s
      encoderBackend: actif ? 'NVENC' : null,
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
    unawaited(_chatControleurs.remove(id)?.close());
    unawaited(_transfertControleurs.remove(id)?.close());
    unawaited(_annotationControleurs.remove(id)?.close());
    _confidentialite.remove(id);
    _regions.remove(id);
    _enregistrementsAChaud.remove(id);
    _sourcesAudio.remove(id);
  }

  // -------------------------------------------------------------------------
  // Canaux média annexes — discussion, transfert, audio, moniteurs
  // (flux de synthèse : parcours entièrement démontrable SANS le cœur natif)
  // -------------------------------------------------------------------------

  /// Un contrôleur de discussion par session (créé à la première écoute ou au
  /// premier envoi). Diffusion : robuste à un ré-abonnement de l'UI.
  final Map<int, StreamController<ChatMessageDto>> _chatControleurs = {};

  /// Un contrôleur de transfert par session.
  final Map<int, StreamController<TransferEventDto>> _transfertControleurs = {};

  /// Journaux **observables par les tests** des réglages audio et des bascules
  /// de moniteur (le cœur réel, lui, agit sur la session).
  final List<({int id, bool actif})> reglagesAudio = <({int id, bool actif})>[];
  final List<({int id, int moniteur})> basculesMoniteur =
      <({int id, int moniteur})>[];

  StreamController<ChatMessageDto> _chatControleur(int id) =>
      _chatControleurs.putIfAbsent(
        id,
        () => StreamController<ChatMessageDto>.broadcast(),
      );

  StreamController<TransferEventDto> _transfertControleur(int id) =>
      _transfertControleurs.putIfAbsent(
        id,
        () => StreamController<TransferEventDto>.broadcast(),
      );

  @override
  Stream<ChatMessageDto> sessionChatStream(int id) => _chatControleur(id).stream;

  /// Livre l'écho local immédiatement (comme le cœur réel), puis répond ~1,5 s
  /// plus tard par un message distant de synthèse (« bien reçu »).
  @override
  Future<void> sendChat(int id, String texte) async {
    final controleur = _chatControleur(id);
    if (controleur.isClosed) return;
    controleur.add(ChatMessageDto(fromRemote: false, text: texte));
    Timer(const Duration(milliseconds: 1500), () {
      if (!controleur.isClosed) {
        controleur.add(
          ChatMessageDto(fromRemote: true, text: 'Bien reçu : « $texte »'),
        );
      }
    });
  }

  @override
  Stream<TransferEventDto> sessionTransferStream(int id) =>
      _transfertControleur(id).stream;

  /// Émet une progression synthétique par fichier
  /// (`started` → `progress`×8 → `completed`) puis `finished` en fin de file.
  @override
  Future<void> sendFiles(int id, List<String> chemins) async {
    if (chemins.isEmpty) return;
    final controleur = _transfertControleur(id);
    if (controleur.isClosed) return;
    unawaited(_simulerTransfert(controleur, chemins));
  }

  Future<void> _simulerTransfert(
    StreamController<TransferEventDto> controleur,
    List<String> chemins,
  ) async {
    final tailles = [for (final c in chemins) _tailleSynthetique(c)];
    final total = tailles.fold<int>(0, (somme, t) => somme + t);
    final debut = DateTime.now();
    var faitSession = 0;

    for (var index = 0; index < chemins.length; index++) {
      if (controleur.isClosed) return;
      final nom = _nomFichier(chemins[index]);
      final taille = tailles[index];
      controleur.add(TransferEventDto(
        kind: 'started',
        fileIndex: index,
        fileName: nom,
        bytesDone: 0,
        bytesTotal: taille,
      ));
      const etapes = 8;
      for (var pas = 1; pas <= etapes; pas++) {
        await Future<void>.delayed(const Duration(milliseconds: 180));
        if (controleur.isClosed) return;
        final faitFichier = (taille * pas / etapes).round();
        final faitCourant = faitSession + faitFichier;
        final secondes =
            DateTime.now().difference(debut).inMilliseconds / 1000.0;
        final debit = secondes > 0 ? faitCourant / secondes : 0.0;
        final restant = (total - faitCourant).clamp(0, total);
        controleur.add(TransferEventDto(
          kind: 'progress',
          fileIndex: index,
          fileName: nom,
          bytesDone: faitFichier,
          bytesTotal: taille,
          sessionBytesDone: faitCourant,
          sessionBytesTotal: total,
          percent: total > 0 ? faitCourant / total * 100.0 : 100.0,
          bytesPerSec: debit,
          etaSecs: debit > 0 ? restant / debit : 0.0,
        ));
      }
      faitSession += taille;
      if (controleur.isClosed) return;
      controleur.add(TransferEventDto(
        kind: 'completed',
        fileIndex: index,
        fileName: nom,
        bytesDone: taille,
        bytesTotal: taille,
      ));
    }
    if (controleur.isClosed) return;
    controleur.add(const TransferEventDto(kind: 'finished'));
  }

  @override
  Future<void> setAudioEnabled(int id, bool actif) async {
    reglagesAudio.add((id: id, actif: actif));
    debugPrint('MockNativeApi.setAudioEnabled(session $id) = '
        '${actif ? 'activé' : 'désactivé'}');
  }

  @override
  Future<void> switchMonitor(int id, int moniteur) async {
    basculesMoniteur.add((id: id, moniteur: moniteur));
    debugPrint('MockNativeApi.switchMonitor(session $id) → moniteur $moniteur');
  }

  /// Modes de source audio acceptés par [sessionSetAudioSource] (contrat UI
  /// stable, aligné sur `nd_ffi::flux::source_audio_depuis_mode`).
  static const List<String> _modesSourceAudio = ['systeme', 'micro', 'mixe'];

  /// Source d'émission audio mémorisée par session (absent = défaut du cœur :
  /// audio système seul).
  final Map<int, String> _sourcesAudio = {};

  @override
  Future<void> sessionSetAudioSource(int sessionId, String mode) async {
    // L'analyse du mode précède tout accès à la session, mêmes messages
    // français que le cœur (`source_audio_depuis_mode`).
    if (!_modesSourceAudio.contains(mode)) {
      throw NovaApiException(
          'source audio inconnue : « $mode » (attendu : systeme, micro, mixe)');
    }
    _sourcesAudio[sessionId] = mode;
    debugPrint(
        'MockNativeApi.sessionSetAudioSource(session $sessionId) → $mode');
  }

  /// Source d'émission audio active d'une session (**observable par les
  /// tests**) ; `null` si jamais pilotée (défaut du cœur : audio système seul).
  String? sourceAudio(int sessionId) => _sourcesAudio[sessionId];

  /// Nom de fichier depuis un chemin (séparateurs `/` ou `\`).
  static String _nomFichier(String chemin) {
    final segments = chemin
        .split(RegExp(r'[\\/]+'))
        .where((s) => s.isNotEmpty)
        .toList();
    return segments.isEmpty ? chemin : segments.last;
  }

  /// Taille synthétique déterministe (2–42 Mo) pour une démo reproductible.
  static int _tailleSynthetique(String chemin) {
    final h = chemin.hashCode & 0x7fffffff;
    return 2 * 1024 * 1024 + h % (40 * 1024 * 1024);
  }

  // -------------------------------------------------------------------------
  // Hôte « accès non surveillé » — flux de synthèse (démontre le dialogue
  // d'acceptation SANS le cœur natif)
  // -------------------------------------------------------------------------

  int _prochainIdHote = 1;
  final Set<int> _hotesActifs = <int>{};

  /// Journal **observable par les tests** des décisions transmises à
  /// [approveIncoming] (le cœur réel, lui, débloque/refuse l'appelant).
  final List<({int hostId, int peerId, bool accepter})> approbations =
      <({int hostId, int peerId, bool accepter})>[];

  /// Nombre d'appelants acceptés, pour faire croître les stats de synthèse.
  int _servisParHote = 0;

  @override
  Future<int> startUnattendedHost({
    required int localId,
    required String rendezvous,
    required List<String> stunServers,
    required PermissionsDto permissions,
  }) async {
    final id = _prochainIdHote++;
    _hotesActifs.add(id);
    return id;
  }

  /// Émet une **demande entrante factice après ~2 s** (puis d'autres, espacées),
  /// pour démontrer le dialogue d'acceptation. Le minuteur est annulé à la
  /// résiliation de l'abonnement (aucun timer pendant après `dispose`).
  @override
  Stream<IncomingRequestDto> unattendedIncomingStream(int hostId) {
    // Appelants factices cyclés (le 2ᵉ correspond à un appareil de confiance).
    const idsFactices = <int>[555240173, 730118902, 190774025];
    late final StreamController<IncomingRequestDto> controleur;
    Timer? minuteur;
    var i = 0;

    void programmer(Duration delai) {
      minuteur = Timer(delai, () {
        if (!_hotesActifs.contains(hostId)) {
          unawaited(controleur.close());
          return;
        }
        final pair = idsFactices[i % idsFactices.length];
        i++;
        controleur.add(
          IncomingRequestDto(peerId: pair, peerIdFormate: _formater(pair)),
        );
        programmer(const Duration(seconds: 12));
      });
    }

    controleur = StreamController<IncomingRequestDto>(
      onListen: () => programmer(const Duration(seconds: 2)),
      onCancel: () => minuteur?.cancel(),
    );
    return controleur.stream;
  }

  @override
  Future<void> approveIncoming({
    required int hostId,
    required int peerId,
    required bool accepter,
  }) async {
    approbations.add((hostId: hostId, peerId: peerId, accepter: accepter));
    if (accepter) _servisParHote++;
    // « journalise » (le cœur réel sert ou refuse réellement l'appelant).
    debugPrint('MockNativeApi.approveIncoming(hôte $hostId, pair '
        '${_formater(peerId)}) = ${accepter ? 'accepté' : 'refusé'}');
  }

  @override
  Future<SessionStatsDto> unattendedStats(int hostId) async {
    // Stats cumulées plausibles, croissant avec les sessions servies. Côté
    // hôte : pas de décodage local (fps/frames nuls), encodeur non exposé.
    return SessionStatsDto(
      fps: 0,
      rttUs: 12000 + _alea.nextInt(6000),
      bytesIn: _servisParHote * 4200 + _alea.nextInt(2000),
      bytesOut: _servisParHote * 1850000 + _alea.nextInt(500000),
      frames: 0,
      inputsDenied: _servisParHote * 3,
      targetBitrateKbps: _servisParHote > 0 ? 6000 : 0,
      abrLevel: 0,
      framesRecorded: 0,
      reconnects: 0,
      encoderBackend: null,
    );
  }

  @override
  Future<void> stopUnattendedHost(int hostId) async {
    _hotesActifs.remove(hostId);
  }

  // -------------------------------------------------------------------------
  // État persistant — persistance EN MÉMOIRE (lot « état persistant »).
  //
  // Reproduit fidèlement le comportement observable de `nd-ffi` : le carnet est
  // modifiable, les réglages get/set, l'historique s'enrichit à chaque
  // `recordSession`, l'accès non surveillé mémorise mot de passe / appareils /
  // journal. Tout persiste tant que l'instance vit : le parcours reste
  // entièrement démontrable sans le cœur natif.
  // -------------------------------------------------------------------------

  /// Empreinte locale stable (64 caractères hexadécimaux, façon BLAKE2s).
  static const String _empreinteLocale =
      '3fa97c22e108b4d95f6a1c07e2438d5b9a0f47c1e6d820b53a9f7c14e0d6b28f';

  /// Identité locale persistante : créée une fois, rechargée à l'identique.
  final LocalIdentityDto _identite = LocalIdentityDto(
    id: 936271048,
    idFormate: _formater(936271048),
    empreinte: _empreinteLocale,
  );

  /// Horodatage Unix (secondes) il y a [jours]/[heures]/[minutes].
  static int _ilYa({int jours = 0, int heures = 0, int minutes = 0}) =>
      DateTime.now()
          .subtract(Duration(days: jours, hours: heures, minutes: minutes))
          .millisecondsSinceEpoch ~/
      1000;

  /// Carnet d'adresses en mémoire (calqué sur la maquette `novadesk-app.html`).
  late final List<AddressBookEntryDto> _contacts = [
    AddressBookEntryDto(
      id: 421887330,
      alias: 'poste-bureau',
      groupe: 'Travail',
      etiquettes: const ['bureau', 'windows'],
      favori: true,
      derniereConnexion: _ilYa(heures: 2),
    ),
    AddressBookEntryDto(
      id: 730118902,
      alias: 'serveur-nas',
      groupe: 'Serveurs',
      etiquettes: const ['nas', 'linux'],
      favori: false,
      derniereConnexion: _ilYa(jours: 1),
    ),
    AddressBookEntryDto(
      id: 555240173,
      alias: 'pc-marie',
      groupe: 'Travail',
      etiquettes: const ['support'],
      favori: false,
      derniereConnexion: _ilYa(jours: 3),
    ),
    AddressBookEntryDto(
      id: 190774025,
      alias: 'mobile-atelier',
      groupe: 'Perso',
      etiquettes: const ['android'],
      favori: false,
      derniereConnexion: _ilYa(jours: 5),
    ),
    AddressBookEntryDto(
      id: 308552641,
      alias: 'vm-build-01',
      groupe: 'Serveurs',
      etiquettes: const ['ci'],
      favori: false,
      derniereConnexion: _ilYa(jours: 10),
    ),
  ];

  /// Groupes déclarés (ordre d'apparition).
  final List<String> _groupes = ['Travail', 'Serveurs', 'Perso'];

  /// Réglages effectifs (défauts raisonnables ; surchargés par [setSetting]).
  final Map<String, String> _reglages = {
    'theme': 'systeme',
    'langue': 'fr',
    'serveur_rendezvous': '127.0.0.1:9000',
    'serveur_relais': '',
    'serveurs_stun': '',
    'prereglage_qualite': 'equilibre',
    'dossier_enregistrement': r'C:\Users\Public\Videos\NovaDesk',
    'demarrer_avec_systeme': 'false',
  };

  /// Historique des sessions récentes (le plus récent en tête, borné).
  late final List<RecentSessionDto> _sessionsRecentes = [
    RecentSessionDto(
        id: 421887330, alias: 'poste-bureau', timestamp: _ilYa(heures: 2)),
    RecentSessionDto(
        id: 730118902, alias: 'serveur-nas', timestamp: _ilYa(jours: 1)),
    RecentSessionDto(
        id: 555240173, alias: 'pc-marie', timestamp: _ilYa(jours: 3)),
    RecentSessionDto(
        id: 190774025, alias: 'mobile-atelier', timestamp: _ilYa(jours: 5)),
  ];

  /// Enregistrements de démonstration (métadonnées seules).
  late final List<RecordingDto> _enregistrements = [
    RecordingDto(
      chemin: r'C:\Users\Public\Videos\NovaDesk\poste-bureau_1407.mp4',
      nom: 'poste-bureau_1407.mp4',
      date: _ilYa(heures: 2),
      dureeS: 754,
      tailleOctets: 486 * 1024 * 1024,
    ),
    RecordingDto(
      chemin: r'C:\Users\Public\Videos\NovaDesk\serveur-nas_1840.mp4',
      nom: 'serveur-nas_1840.mp4',
      date: _ilYa(jours: 1),
      dureeS: 242,
      tailleOctets: 158 * 1024 * 1024,
    ),
    RecordingDto(
      chemin: r'C:\Users\Public\Videos\NovaDesk\pc-marie_1407.ndr',
      nom: 'pc-marie_1407.ndr',
      date: _ilYa(jours: 3),
      dureeS: 1275,
      tailleOctets: 921 * 1024 * 1024,
    ),
  ];

  /// Mot de passe permanent d'accès non surveillé (mock : conservé en clair
  /// pour la vérification ; le cœur réel ne stocke qu'un hachage salé).
  String? _motDePasseNonSurveille;

  /// Appareils de confiance (`NovaId`).
  final Set<int> _appareilsConfiance = {421887330, 555240173};

  /// Journal des accès non surveillés (le plus récent en tête).
  late final List<AccessLogEntryDto> _journalAcces = [
    AccessLogEntryDto(
        peerId: 421887330,
        peerIdFormate: _formater(421887330),
        timestamp: _ilYa(heures: 2),
        accepte: true),
    AccessLogEntryDto(
        peerId: 555240173,
        peerIdFormate: _formater(555240173),
        timestamp: _ilYa(jours: 1, heures: 5),
        accepte: false),
    AccessLogEntryDto(
        peerId: 421887330,
        peerIdFormate: _formater(421887330),
        timestamp: _ilYa(jours: 4),
        accepte: true),
  ];

  @override
  Future<LocalIdentityDto> localIdentity() async => _identite;

  @override
  Future<String> generateEphemeralPassword() async {
    // 10 caractères lisibles (alphabet sans symboles ambigus), non persisté.
    const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789';
    return List.generate(
        10, (_) => alphabet[_alea.nextInt(alphabet.length)]).join();
  }

  @override
  Future<List<AddressBookEntryDto>> listContacts() async =>
      List.unmodifiable(_contacts);

  @override
  Future<AddressBookEntryDto> addContact({
    required String alias,
    required int id,
    required String groupe,
    required List<String> etiquettes,
  }) async {
    if (_contacts.any((c) => c.id == id)) {
      throw NovaApiException(
          'un contact possède déjà l\'ID ${_formater(id)}');
    }
    final entree = AddressBookEntryDto(
      id: id,
      alias: alias,
      groupe: groupe,
      etiquettes: List.of(etiquettes),
      favori: false,
    );
    _contacts.add(entree);
    if (groupe.isNotEmpty && !_groupes.contains(groupe)) {
      _groupes.add(groupe);
    }
    return entree;
  }

  @override
  Future<void> updateContact({
    required int id,
    required String alias,
    required String groupe,
    required List<String> etiquettes,
  }) async {
    final i = _contacts.indexWhere((c) => c.id == id);
    if (i < 0) {
      throw NovaApiException('contact inconnu : ${_formater(id)}');
    }
    final ancien = _contacts[i];
    _contacts[i] = AddressBookEntryDto(
      id: id,
      alias: alias,
      groupe: groupe,
      etiquettes: List.of(etiquettes),
      favori: ancien.favori,
      derniereConnexion: ancien.derniereConnexion,
    );
    if (groupe.isNotEmpty && !_groupes.contains(groupe)) {
      _groupes.add(groupe);
    }
  }

  @override
  Future<void> removeContact({required int id}) async {
    final i = _contacts.indexWhere((c) => c.id == id);
    if (i < 0) {
      throw NovaApiException('contact inconnu : ${_formater(id)}');
    }
    _contacts.removeAt(i);
  }

  @override
  Future<void> setFavorite({required int id, required bool favori}) async {
    final i = _contacts.indexWhere((c) => c.id == id);
    if (i < 0) {
      throw NovaApiException('contact inconnu : ${_formater(id)}');
    }
    final ancien = _contacts[i];
    _contacts[i] = AddressBookEntryDto(
      id: ancien.id,
      alias: ancien.alias,
      groupe: ancien.groupe,
      etiquettes: ancien.etiquettes,
      favori: favori,
      derniereConnexion: ancien.derniereConnexion,
    );
  }

  @override
  Future<List<String>> listGroups() async => List.unmodifiable(_groupes);

  @override
  Future<void> addGroup({required String nom}) async {
    if (nom.isEmpty) {
      throw const NovaApiException('le nom du groupe est vide');
    }
    if (_groupes.contains(nom)) {
      throw NovaApiException('le groupe « $nom » existe déjà');
    }
    _groupes.add(nom);
  }

  @override
  Future<List<SettingDto>> getSettings() async {
    final cles = _reglages.keys.toList()..sort();
    return [for (final c in cles) SettingDto(cle: c, valeur: _reglages[c]!)];
  }

  @override
  Future<String?> getSetting({required String cle}) async => _reglages[cle];

  @override
  Future<void> setSetting({required String cle, required String valeur}) async {
    if (cle.isEmpty) {
      throw const NovaApiException('la clé de réglage est vide');
    }
    _reglages[cle] = valeur;
  }

  /// Dernier état **appliqué** du démarrage avec le système ; `null` tant que
  /// [applyAutostart] n'a jamais été appelé.
  bool? _autostart;

  /// État appliqué du démarrage avec le système (**observable par les
  /// tests**) ; le cœur réel, lui, écrit l'entrée de registre `Run`.
  bool? get autostartApplique => _autostart;

  @override
  Future<void> applyAutostart({required bool actif}) async {
    _autostart = actif;
    debugPrint('MockNativeApi.applyAutostart → '
        '${actif ? 'activé' : 'désactivé'}');
  }

  @override
  Future<void> recordSession({required int id, required String alias}) async {
    final maintenant = _ilYa();
    // Dédupliqué par id, remis en tête, borné à 20 entrées.
    _sessionsRecentes.removeWhere((s) => s.id == id);
    _sessionsRecentes.insert(
        0, RecentSessionDto(id: id, alias: alias, timestamp: maintenant));
    if (_sessionsRecentes.length > 20) {
      _sessionsRecentes.removeRange(20, _sessionsRecentes.length);
    }
    // Rafraîchit la dernière connexion du contact correspondant.
    final i = _contacts.indexWhere((c) => c.id == id);
    if (i >= 0) {
      final c = _contacts[i];
      _contacts[i] = AddressBookEntryDto(
        id: c.id,
        alias: c.alias,
        groupe: c.groupe,
        etiquettes: c.etiquettes,
        favori: c.favori,
        derniereConnexion: maintenant,
      );
    }
  }

  @override
  Future<List<RecentSessionDto>> recentSessions() async =>
      List.unmodifiable(_sessionsRecentes);

  @override
  Future<List<RecordingDto>> listRecordings({String? dir}) async {
    // Le mock ignore [dir] (aucun disque) : renvoie la liste triée récent→ancien.
    final copie = List.of(_enregistrements)
      ..sort((a, b) => b.date.compareTo(a.date));
    return copie;
  }

  @override
  Future<UnattendedConfigDto> unattendedConfig() async => UnattendedConfigDto(
        aMotDePasse:
            _motDePasseNonSurveille != null && _motDePasseNonSurveille!.isNotEmpty,
        appareilsDeConfiance: _appareilsConfiance.toList()..sort(),
      );

  @override
  Future<void> setUnattendedPassword({required String pwd}) async {
    // Un mot de passe vide efface la configuration.
    _motDePasseNonSurveille = pwd.isEmpty ? null : pwd;
  }

  @override
  Future<bool> verifyUnattendedPassword({required String pwd}) async {
    final ref = _motDePasseNonSurveille;
    return ref != null && ref.isNotEmpty && ref == pwd;
  }

  @override
  Future<void> addTrustedDevice({required int id}) async {
    _appareilsConfiance.add(id);
  }

  @override
  Future<void> removeTrustedDevice({required int id}) async {
    if (!_appareilsConfiance.remove(id)) {
      throw NovaApiException(
          'appareil non présent dans la liste de confiance : ${_formater(id)}');
    }
  }

  @override
  Future<void> recordAccess(
      {required int peerId, required bool accepte}) async {
    _journalAcces.insert(
      0,
      AccessLogEntryDto(
        peerId: peerId,
        peerIdFormate: _formater(peerId),
        timestamp: _ilYa(),
        accepte: accepte,
      ),
    );
  }

  @override
  Future<List<AccessLogEntryDto>> accessLog() async =>
      List.unmodifiable(_journalAcces);

  // -------------------------------------------------------------------------
  // Admission automatique — liste blanche (ACL) et invitations éphémères EN
  // MÉMOIRE : mêmes validations et messages français que le cœur
  // (`admission_retirer`, `profil_invitation_bits`, `revoquer_invitation`).
  // -------------------------------------------------------------------------

  /// Liste blanche d'admission (ID admis **sans mot de passe** en accès non
  /// surveillé) ; le `Set` ordonné (insertion) reproduit l'« ordre d'ajout »
  /// du cœur. Pré-peuplée d'un appareil hors liste de confiance (l'admission
  /// réunit les deux listes).
  final Set<int> _admissionAutorisee = {730118902};

  /// Profils d'invitation acceptés par [createInvite] (contrat UI stable,
  /// aligné sur `nd_ffi::flux::profil_invitation_bits`).
  static const List<String> _profilsInvitation = [
    'observation',
    'standard',
    'controle_total',
  ];

  /// Alphabet des codes d'invitation : 32 symboles sans caractères ambigus
  /// (ni `I`, `O`, `0`, `1`), comme `nd_features::invite::CODE_ALPHABET`.
  static const String _alphabetInvitation = 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789';

  /// Invitations actives : code → profil accordé + instant d'expiration (le
  /// temps restant [InviteDto.expireDansS] est recalculé à chaque listage,
  /// comme le cœur).
  final Map<String, ({String profil, DateTime expire})> _invitations = {};

  @override
  Future<List<int>> listAdmissionAllowlist() async =>
      List.unmodifiable(_admissionAutorisee);

  @override
  Future<void> addAdmissionAllowed({required int id}) async {
    // Sans effet si l'ID y figure déjà, comme le cœur.
    _admissionAutorisee.add(id);
  }

  @override
  Future<void> removeAdmissionAllowed({required int id}) async {
    if (!_admissionAutorisee.remove(id)) {
      throw NovaApiException(
          "l'appareil ${_formater(id)} n'est pas dans la liste blanche "
          "d'admission");
    }
  }

  @override
  Future<String> createInvite({
    required String profil,
    required int ttlMinutes,
  }) async {
    // La validation du profil précède toute écriture (comme le cœur).
    if (!_profilsInvitation.contains(profil)) {
      throw NovaApiException("profil d'invitation inconnu : « $profil » "
          '(attendu : observation, standard, controle_total)');
    }
    final maintenant = DateTime.now();
    // Purge les invitations expirées au passage (le magasin ne gonfle pas).
    _invitations.removeWhere((_, i) => !i.expire.isAfter(maintenant));
    // Code lisible `XXX-XXX-XXX` (le cœur réel tire du CSPRNG ; unique dans
    // le magasin pour que la démo reste cohérente).
    String code;
    do {
      code = List.generate(
        3,
        (_) => List.generate(
          3,
          (_) => _alphabetInvitation[_alea.nextInt(_alphabetInvitation.length)],
        ).join(),
      ).join('-');
    } while (_invitations.containsKey(code));
    _invitations[code] = (
      profil: profil,
      expire: maintenant.add(Duration(minutes: ttlMinutes)),
    );
    return code;
  }

  @override
  Future<List<InviteDto>> listInvites() async {
    final maintenant = DateTime.now();
    return [
      for (final entree in _invitations.entries)
        if (entree.value.expire.isAfter(maintenant))
          InviteDto(
            code: entree.key,
            profil: entree.value.profil,
            expireDansS: entree.value.expire.difference(maintenant).inSeconds,
          ),
    ];
  }

  @override
  Future<void> revokeInvite({required String code}) async {
    if (_invitations.remove(code) == null) {
      throw NovaApiException(
          'invitation « $code » inconnue (déjà révoquée, consommée ou '
          'expirée)');
    }
  }

  // -------------------------------------------------------------------------
  // Capacités avancées de session — confidentialité, cadre d'écran, tunnels,
  // annotations (état/flux en mémoire : parcours démontrable SANS le cœur natif)
  // -------------------------------------------------------------------------

  /// Mode confidentialité mémorisé par session (rideau actif).
  final Map<int, bool> _confidentialite = {};

  /// Cadre d'écran demandé par session (absent = plein écran).
  final Map<int, RegionDto> _regions = {};

  /// Un contrôleur d'annotations par session (créé à la première écoute ou au
  /// premier envoi).
  final Map<int, StreamController<AnnotationDto>> _annotationControleurs = {};

  StreamController<AnnotationDto> _annotationControleur(int id) =>
      _annotationControleurs.putIfAbsent(
        id,
        () => StreamController<AnnotationDto>.broadcast(),
      );

  @override
  Future<void> setPrivacy(int sessionId, bool actif) async {
    _confidentialite[sessionId] = actif;
    debugPrint('MockNativeApi.setPrivacy(session $sessionId) = '
        '${actif ? 'rideau' : 'écran'}');
  }

  @override
  Future<bool> privacyActive(int sessionId) async =>
      _confidentialite[sessionId] ?? false;

  @override
  Future<void> setSessionRegion(int sessionId, RegionDto? region) async {
    if (region == null) {
      _regions.remove(sessionId);
    } else {
      _regions[sessionId] = region;
    }
  }

  @override
  Future<RegionDto?> sessionRequestedRegion(int sessionId) async =>
      _regions[sessionId];

  @override
  Future<TunnelOuvertDto> openTunnel(
    int sessionId,
    int portLocal,
    String cible,
  ) async {
    // Port éphémère plausible si 0 (comme le cœur réel qui résout un port libre).
    final port = portLocal == 0 ? 49152 + _alea.nextInt(16384) : portLocal;
    return TunnelOuvertDto(adresseLocale: '127.0.0.1:$port', portLocal: port);
  }

  @override
  Future<void> closeTunnels(int sessionId) async {
    // Aucun tunnel réel : no-op.
  }

  @override
  Stream<AnnotationDto> sessionAnnotationStream(int sessionId) =>
      _annotationControleur(sessionId).stream;

  /// Réémet l'annotation sur le flux (écho local immédiat) : le trait dessiné
  /// s'affiche en démo comme s'il revenait du pair.
  @override
  Future<void> sendAnnotation(int sessionId, AnnotationDto annotation) async {
    final controleur = _annotationControleur(sessionId);
    if (!controleur.isClosed) controleur.add(annotation);
  }

  // -------------------------------------------------------------------------
  // Plan de contrôle de session (lot « contrôles de session ») — état en
  // mémoire : permissions à chaud, qualité, enregistrement à chaud, moniteurs
  // factices et infos système du pair, mêmes validations et messages français
  // que `crate::flux` (`capacite_depuis_cle`, `qualite_depuis_preset`).
  // -------------------------------------------------------------------------

  /// Clés de capacité acceptées par [sessionSetPermission] (contrat UI stable,
  /// aligné sur `nd_ffi::flux::capacite_depuis_cle`).
  static const List<String> _capacitesConnues = [
    'voir_ecran',
    'souris',
    'clavier',
    'presse_papiers_lecture',
    'presse_papiers_ecriture',
    'fichiers_envoi',
    'fichiers_reception',
    'audio',
    'redemarrage',
    'enregistrement',
    'confidentialite',
    'tunnel',
  ];

  /// Préréglages de qualité acceptés (avec et sans accent, comme le cœur).
  static const List<String> _presetsQualite = [
    'auto',
    'fluide',
    'equilibre',
    'nettete',
    'netteté',
  ];

  /// Journal **observable par les tests** des permissions renégociées à chaud
  /// (le cœur réel, lui, pousse le nouvel ensemble au filtre d'injection).
  final List<({int id, String capacite, bool autorise})> permissionsRenegociees =
      <({int id, String capacite, bool autorise})>[];

  /// Journal **observable par les tests** des préréglages de qualité appliqués.
  final List<({int id, String preset})> presetsQualiteAppliques =
      <({int id, String preset})>[];

  /// Chemin du MP4 en cours d'écriture par session (absent = pas
  /// d'enregistrement à chaud).
  final Map<int, String> _enregistrementsAChaud = {};

  @override
  Future<void> sessionSetPermission(
    int sessionId,
    String capacite,
    bool autorise,
  ) async {
    // L'analyse de la clé précède tout accès à la session (comme le cœur).
    if (!_capacitesConnues.contains(capacite)) {
      throw NovaApiException(
          'capacité inconnue : « $capacite » (attendu : '
          '${_capacitesConnues.join(', ')})');
    }
    permissionsRenegociees
        .add((id: sessionId, capacite: capacite, autorise: autorise));
    debugPrint('MockNativeApi.sessionSetPermission(session $sessionId) : '
        '$capacite = ${autorise ? 'accordée' : 'retirée'}');
  }

  @override
  Future<void> sessionSetQuality(int sessionId, String preset) async {
    if (!_presetsQualite.contains(preset)) {
      throw NovaApiException('préréglage de qualité inconnu : « $preset » '
          '(attendu : auto, fluide, equilibre, netteté)');
    }
    presetsQualiteAppliques.add((id: sessionId, preset: preset));
    debugPrint('MockNativeApi.sessionSetQuality(session $sessionId) → $preset');
  }

  @override
  Future<void> sessionSetRecording(int sessionId, String? chemin) async {
    if (chemin == null) {
      _enregistrementsAChaud.remove(sessionId);
    } else {
      _enregistrementsAChaud[sessionId] = chemin;
    }
    debugPrint('MockNativeApi.sessionSetRecording(session $sessionId) → '
        '${chemin ?? 'arrêt (fichier clos)'}');
  }

  /// Chemin d'enregistrement à chaud actif d'une session (**observable par les
  /// tests**) ; `null` si aucun enregistrement n'est en cours.
  String? enregistrementActif(int sessionId) => _enregistrementsAChaud[sessionId];

  /// Deux écrans factices (dont le principal), à l'image d'un poste de bureau
  /// classique : remplace l'« Écran 1/2 » codé en dur du sous-menu moniteurs.
  @override
  Future<List<MonitorInfoDto>> sessionMonitors(int sessionId) async => const [
        MonitorInfoDto(index: 0, largeur: 1920, hauteur: 1080, principal: true),
        MonitorInfoDto(
            index: 1, largeur: 1280, hauteur: 1024, principal: false),
      ];

  @override
  Future<PeerInfoDto> sessionPeerInfo(int sessionId) async =>
      const PeerInfoDto(hote: 'MOCK-PC', os: 'Windows 11 Pro (mock)');

  // -------------------------------------------------------------------------
  // Listing de répertoire distant — arborescence factice EN MÉMOIRE : le
  // navigateur de fichiers est démontrable SANS le cœur natif. Chaque dossier
  // listé est navigable (sa clé existe), dossiers d'abord puis fichiers,
  // chaque groupe trié par nom, comme le rendu de l'hôte réel.
  // -------------------------------------------------------------------------

  /// Arborescence du « poste hôte » factice : chemin normalisé
  /// ([_normaliserChemin]) → entrées. La clé vide donne les racines, dont
  /// chaque nom (« C:\ », « D:\ ») est directement utilisable comme chemin de
  /// la demande suivante — même contrat que le cœur réel.
  late final Map<String, List<EntreeFsDto>> _arborescenceDistante = {
    '': const [
      EntreeFsDto(nom: 'C:\\', taille: 0, estDossier: true),
      EntreeFsDto(nom: 'D:\\', taille: 0, estDossier: true),
    ],
    'C:\\': [
      const EntreeFsDto(nom: 'Program Files', taille: 0, estDossier: true),
      const EntreeFsDto(nom: 'Utilisateurs', taille: 0, estDossier: true),
      const EntreeFsDto(nom: 'Windows', taille: 0, estDossier: true),
      EntreeFsDto(
          nom: 'pagefile.sys',
          taille: 3 * 1024 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(heures: 1)),
    ],
    'C:\\Program Files': const [
      EntreeFsDto(nom: 'NovaDesk', taille: 0, estDossier: true),
    ],
    'C:\\Program Files\\NovaDesk': [
      EntreeFsDto(
          nom: 'LISEZMOI.txt',
          taille: 1834,
          estDossier: false,
          modifieLe: _ilYa(jours: 7)),
      EntreeFsDto(
          nom: 'novadesk.exe',
          taille: 18 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 7)),
    ],
    'C:\\Utilisateurs': const [
      EntreeFsDto(nom: 'Public', taille: 0, estDossier: true),
      EntreeFsDto(nom: 'marie', taille: 0, estDossier: true),
    ],
    'C:\\Utilisateurs\\Public': [
      EntreeFsDto(
          nom: 'partage.zip',
          taille: 640 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 4)),
    ],
    'C:\\Utilisateurs\\marie': [
      const EntreeFsDto(nom: 'Documents', taille: 0, estDossier: true),
      EntreeFsDto(
          nom: 'notes.txt',
          taille: 4096,
          estDossier: false,
          modifieLe: _ilYa(heures: 5)),
    ],
    'C:\\Utilisateurs\\marie\\Documents': [
      EntreeFsDto(
          nom: 'budget.xlsx',
          taille: 88 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 2)),
      EntreeFsDto(
          nom: 'rapport.pdf',
          taille: 2 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 1)),
    ],
    'C:\\Windows': [
      const EntreeFsDto(nom: 'System32', taille: 0, estDossier: true),
      EntreeFsDto(
          nom: 'explorer.exe',
          taille: 5 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 30)),
    ],
    'C:\\Windows\\System32': [
      EntreeFsDto(
          nom: 'kernel32.dll',
          taille: 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 30)),
      EntreeFsDto(
          nom: 'ntdll.dll',
          taille: 2 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 30)),
    ],
    'D:\\': [
      const EntreeFsDto(nom: 'Sauvegardes', taille: 0, estDossier: true),
      EntreeFsDto(
          nom: 'archive.zip',
          taille: 250 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 12)),
    ],
    'D:\\Sauvegardes': [
      EntreeFsDto(
          nom: 'poste-bureau_2026-06-30.ndr',
          taille: 921 * 1024 * 1024,
          estDossier: false,
          modifieLe: _ilYa(jours: 8)),
    ],
  };

  /// Normalise un chemin distant pour la recherche dans l'arborescence
  /// factice : sépare sur `/` ou `\`, reconstruit avec `\`, et conserve le
  /// `\` final d'une racine lecteur (« C:\ ») — les chemins bâtis par
  /// concaténation (« C:\ » + « Windows ») retombent ainsi sur leurs clés.
  static String _normaliserChemin(String chemin) {
    final segments = chemin
        .trim()
        .split(RegExp(r'[\\/]+'))
        .where((s) => s.isNotEmpty)
        .toList();
    if (segments.isEmpty) return '';
    if (segments.length == 1 && segments.first.endsWith(':')) {
      return '${segments.first}\\';
    }
    return segments.join('\\');
  }

  @override
  Future<List<EntreeFsDto>> sessionListRemoteDir(
    int sessionId,
    String chemin,
  ) async {
    // Petite latence plausible (aller-retour du canal `Control`).
    await Future<void>.delayed(const Duration(milliseconds: 120));
    final entrees = _arborescenceDistante[_normaliserChemin(chemin)];
    if (entrees == null) {
      // Même esprit que le refus de l'hôte réel, propagé tel quel.
      throw NovaApiException(
          'listing distant impossible : « $chemin » est introuvable ou '
          "n'est pas un dossier");
    }
    return List.unmodifiable(entrees);
  }

  /// Journal **observable par les tests** des téléchargements demandés (le
  /// cœur réel, lui, écrit réellement le fichier tranche par tranche).
  final List<({int id, String cheminDistant, String cheminLocal})>
      telechargements = <({int id, String cheminDistant, String cheminLocal})>[];

  @override
  Future<String> sessionDownloadFile(
    int sessionId,
    String cheminDistant,
    String dossierLocal,
  ) async {
    // Petite latence plausible (tranches sur le canal `Control`).
    await Future<void>.delayed(const Duration(milliseconds: 150));
    // Composant de base sûr du chemin distant (jamais de traversée `..` ni de
    // racine de lecteur), comme le cœur réel.
    final nom = _nomFichier(cheminDistant);
    if (nom.isEmpty || nom == '..' || nom.endsWith(':')) {
      throw NovaApiException(
          'téléchargement distant impossible : « $cheminDistant » '
          "n'est pas un fichier");
    }
    final separateur =
        dossierLocal.endsWith('\\') || dossierLocal.endsWith('/') ? '' : '\\';
    final cheminLocal = '$dossierLocal$separateur$nom';
    telechargements.add(
      (id: sessionId, cheminDistant: cheminDistant, cheminLocal: cheminLocal),
    );
    debugPrint('MockNativeApi.sessionDownloadFile(session $sessionId) : '
        '« $cheminDistant » → $cheminLocal');
    return cheminLocal;
  }

  // -------------------------------------------------------------------------
  // Relecture d'enregistrements — lecteur en mémoire (images de synthèse)
  // -------------------------------------------------------------------------

  static const int _largeurEnregistrementMock = 1280;
  static const int _hauteurEnregistrementMock = 720;
  static const int _fpsEnregistrementMock = 30;
  static const int _nbImagesEnregistrementMock = 60; // ~2 s à 30 fps

  int _prochainIdEnregistrement = 1;

  /// Index de la prochaine image à lire, par enregistrement ouvert.
  final Map<int, int> _positionLecture = {};

  @override
  Future<RecordingInfoDto> openRecording(String chemin) async {
    final id = _prochainIdEnregistrement++;
    _positionLecture[id] = 0;
    return RecordingInfoDto(
      id: id,
      largeur: _largeurEnregistrementMock,
      hauteur: _hauteurEnregistrementMock,
      fps: _fpsEnregistrementMock,
      dureeUs: (_nbImagesEnregistrementMock * 1000000 / _fpsEnregistrementMock)
          .round(),
      nbImages: _nbImagesEnregistrementMock,
    );
  }

  @override
  Future<VideoFrameDto?> recordingNextFrame(int id) async {
    final pos = _positionLecture[id];
    if (pos == null || pos >= _nbImagesEnregistrementMock) return null;
    _positionLecture[id] = pos + 1;
    return _genererFrameEnregistrement(pos);
  }

  @override
  Future<void> recordingSeek(int id, int timestampUs) async {
    if (!_positionLecture.containsKey(id)) return;
    // Convertit le timestamp (µs) en index d'image (30 fps), borné.
    final index = (timestampUs * _fpsEnregistrementMock / 1000000).round();
    _positionLecture[id] = index.clamp(0, _nbImagesEnregistrementMock);
  }

  @override
  Future<void> closeRecording(int id) async {
    _positionLecture.remove(id);
  }

  // -------------------------------------------------------------------------
  // Wake-on-LAN
  // -------------------------------------------------------------------------

  /// Journal **observable par les tests** des réveils Wake-on-LAN demandés (le
  /// cœur réel, lui, émet le paquet magique en UDP).
  final List<({String mac, String? broadcast})> reveilsWol =
      <({String mac, String? broadcast})>[];

  @override
  Future<void> sendWol(String mac, {String? broadcast}) async {
    reveilsWol.add((mac: mac, broadcast: broadcast));
    final cible = (broadcast == null || broadcast.isEmpty)
        ? '255.255.255.255:9'
        : broadcast;
    debugPrint('MockNativeApi.sendWol(mac $mac) → $cible');
  }

  // -------------------------------------------------------------------------
  // Découverte LAN — état EN MÉMOIRE : la liste des voisins est démontrable
  // SANS le cœur natif (aucun socket multicast dans le mock).
  // -------------------------------------------------------------------------

  /// Nom et port annoncés par la découverte active ; `null` = arrêtée.
  ({String nom, int port})? _decouverte;

  /// Instant du démarrage de la découverte (le second voisin factice
  /// n'apparaît qu'après ~2 s : démo d'une liste qui se peuple).
  DateTime? _debutDecouverte;

  @override
  Future<void> discoveryStart(String nom, int port) async {
    // Idempotent, comme le cœur réel : tant qu'une instance vit, les appels
    // suivants sont sans effet, quels que soient leurs arguments (pour
    // changer de nom ou de port : [discoveryStop] puis redémarrer).
    if (_decouverte != null) return;
    _decouverte = (nom: nom, port: port);
    _debutDecouverte = DateTime.now();
    debugPrint('MockNativeApi.discoveryStart(« $nom », port $port)');
  }

  /// Vide tant que la découverte n'est pas démarrée ; sinon un premier voisin
  /// factice répond immédiatement, un second après ~2 s. Triés par id
  /// croissant et le poste local exclu, comme le cœur réel.
  @override
  Future<List<DiscoveredPeerDto>> discoveryPeers() async {
    final debut = _debutDecouverte;
    if (_decouverte == null || debut == null) return const [];
    final pairs = <DiscoveredPeerDto>[
      DiscoveredPeerDto(
        id: 555240173,
        idFormate: _formater(555240173),
        nom: 'pc-marie',
        adresse: '192.168.1.87:52310',
      ),
    ];
    if (DateTime.now().difference(debut) >= const Duration(seconds: 2)) {
      pairs.add(DiscoveredPeerDto(
        id: 730118902,
        idFormate: _formater(730118902),
        nom: 'serveur-nas',
        adresse: '192.168.1.42:49873',
      ));
    }
    return pairs;
  }

  @override
  Future<void> discoveryStop() async {
    // Idempotent : arrêter une découverte déjà arrêtée est sans effet.
    _decouverte = null;
    _debutDecouverte = null;
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

  /// Génère une image de relecture (dimensions de l'enregistrement mock) : un
  /// dégradé diagonal qui se décale selon [index] (mouvement visible à la
  /// lecture), distinct de la mire live.
  VideoFrameDto _genererFrameEnregistrement(int index) {
    const int w = _largeurEnregistrementMock;
    const int h = _hauteurEnregistrementMock;
    final rgba = Uint8List(w * h * 4);
    final int defile = index * 6;
    var i = 0;
    for (var y = 0; y < h; y++) {
      for (var x = 0; x < w; x++) {
        rgba[i++] = ((x + defile) * 255 ~/ w) & 0xFF; // R : défile avec le temps
        rgba[i++] = y * 255 ~/ h; // V : dégradé vertical
        final int b = 200 - ((x + y + defile) & 127) ~/ 2;
        rgba[i++] = b < 0 ? 0 : b; // B
        rgba[i++] = 255; // A
      }
    }
    return VideoFrameDto(width: w, height: h, rgba: rgba);
  }
}
