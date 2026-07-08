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
