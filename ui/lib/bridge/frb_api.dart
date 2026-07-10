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
  // Canaux média annexes — délégation aux fonctions générées (u64 ⇄ BigInt)
  // ---------------------------------------------------------------------------

  @override
  Stream<ChatMessageDto> sessionChatStream(int id) =>
      frb.sessionChatStream(id: BigInt.from(id)).map(_chatDepuis);

  @override
  Future<void> sendChat(int id, String texte) =>
      frb.sendChat(id: BigInt.from(id), texte: texte);

  @override
  Stream<TransferEventDto> sessionTransferStream(int id) =>
      frb.sessionTransferStream(id: BigInt.from(id)).map(_transfertDepuis);

  @override
  Future<void> sendFiles(int id, List<String> chemins) =>
      frb.sendFiles(id: BigInt.from(id), chemins: chemins);

  @override
  Future<void> setAudioEnabled(int id, bool actif) =>
      frb.setAudioEnabled(id: BigInt.from(id), actif: actif);

  @override
  Future<void> sessionSetAudioSource(int sessionId, String mode) async {
    try {
      await frb.sessionSetAudioSource(
        sessionId: BigInt.from(sessionId),
        mode: mode,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> switchMonitor(int id, int moniteur) =>
      frb.switchMonitor(id: BigInt.from(id), moniteur: moniteur);

  // ---------------------------------------------------------------------------
  // Capacités avancées de session, relecture et Wake-on-LAN — délégation aux
  // fonctions générées (u64 ⇄ BigInt)
  // ---------------------------------------------------------------------------

  @override
  Future<void> setPrivacy(int sessionId, bool actif) =>
      frb.setPrivacy(sessionId: BigInt.from(sessionId), actif: actif);

  @override
  Future<bool> privacyActive(int sessionId) =>
      frb.privacyActive(sessionId: BigInt.from(sessionId));

  @override
  Future<void> setSessionRegion(int sessionId, RegionDto? region) =>
      frb.setSessionRegion(
        sessionId: BigInt.from(sessionId),
        region: region == null ? null : _regionVers(region),
      );

  @override
  Future<RegionDto?> sessionRequestedRegion(int sessionId) async {
    final r =
        await frb.sessionRequestedRegion(sessionId: BigInt.from(sessionId));
    return r == null ? null : _regionDepuis(r);
  }

  @override
  Future<TunnelOuvertDto> openTunnel(
    int sessionId,
    int portLocal,
    String cible,
  ) async {
    try {
      final t = await frb.openTunnel(
        sessionId: BigInt.from(sessionId),
        portLocal: portLocal,
        cible: cible,
      );
      return _tunnelDepuis(t);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> closeTunnels(int sessionId) =>
      frb.closeTunnels(sessionId: BigInt.from(sessionId));

  @override
  Future<void> sendAnnotation(int sessionId, AnnotationDto annotation) async {
    try {
      await frb.sendAnnotation(
        sessionId: BigInt.from(sessionId),
        annotation: _annotationVers(annotation),
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Stream<AnnotationDto> sessionAnnotationStream(int sessionId) => frb
      .sessionAnnotationStream(sessionId: BigInt.from(sessionId))
      .map(_annotationDepuis);

  @override
  Future<RecordingInfoDto> openRecording(String chemin) async {
    try {
      final info = await frb.openRecording(chemin: chemin);
      return _recordingInfoDepuis(info);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<VideoFrameDto?> recordingNextFrame(int id) async {
    final f = await frb.recordingNextFrame(id: BigInt.from(id));
    return f == null ? null : _frameDepuis(f);
  }

  @override
  Future<void> recordingSeek(int id, int timestampUs) => frb.recordingSeek(
        id: BigInt.from(id),
        timestampUs: BigInt.from(timestampUs),
      );

  @override
  Future<void> closeRecording(int id) =>
      frb.closeRecording(id: BigInt.from(id));

  // ---------------------------------------------------------------------------
  // Plan de contrôle de session (lot « contrôles de session ») — délégation aux
  // fonctions générées (u64 ⇄ BigInt) ; les `Result<_, String>` du cœur (clé de
  // capacité ou préréglage inconnus, session inconnue, annonce absente) sont
  // retransformés en [NovaApiException] au message français affichable.
  // ---------------------------------------------------------------------------

  @override
  Future<void> sessionSetPermission(
    int sessionId,
    String capacite,
    bool autorise,
  ) async {
    try {
      await frb.sessionSetPermission(
        sessionId: BigInt.from(sessionId),
        capacite: capacite,
        autorise: autorise,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> sessionSetQuality(int sessionId, String preset) async {
    try {
      await frb.sessionSetQuality(
        sessionId: BigInt.from(sessionId),
        preset: preset,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> sessionSetRecording(int sessionId, String? chemin) async {
    try {
      await frb.sessionSetRecording(
        sessionId: BigInt.from(sessionId),
        chemin: chemin,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<List<MonitorInfoDto>> sessionMonitors(int sessionId) async {
    try {
      final moniteurs =
          await frb.sessionMonitors(sessionId: BigInt.from(sessionId));
      return moniteurs.map(_moniteurDepuis).toList();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<PeerInfoDto> sessionPeerInfo(int sessionId) async {
    try {
      final infos =
          await frb.sessionPeerInfo(sessionId: BigInt.from(sessionId));
      return PeerInfoDto(hote: infos.hote, os: infos.os);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<List<EntreeFsDto>> sessionListRemoteDir(
    int sessionId,
    String chemin,
  ) async {
    try {
      final entrees = await frb.sessionListRemoteDir(
        sessionId: BigInt.from(sessionId),
        chemin: chemin,
      );
      return entrees.map(_entreeFsDepuis).toList();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<String> sessionDownloadFile(
    int sessionId,
    String cheminDistant,
    String dossierLocal,
  ) async {
    try {
      return await frb.sessionDownloadFile(
        sessionId: BigInt.from(sessionId),
        cheminDistant: cheminDistant,
        dossierLocal: dossierLocal,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> sendWol(String mac, {String? broadcast}) async {
    try {
      await frb.sendWol(mac: mac, broadcast: broadcast);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  // ---------------------------------------------------------------------------
  // Découverte LAN — délégation aux fonctions générées ; les `Result<_, String>`
  // du cœur (identité indisponible, port d'écoute occupé…) sont retransformés
  // en [NovaApiException] au message français affichable.
  // ---------------------------------------------------------------------------

  @override
  Future<void> discoveryStart(String nom, int port) async {
    try {
      await frb.discoveryStart(nom: nom, port: port);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<List<DiscoveredPeerDto>> discoveryPeers() async {
    // Jamais d'erreur côté cœur : liste vide si la découverte est arrêtée.
    final pairs = await frb.discoveryPeers();
    return pairs.map(_pairDecouvertDepuis).toList();
  }

  @override
  Future<void> discoveryStop() async {
    try {
      await frb.discoveryStop();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

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
  // État persistant — délégation aux fonctions générées (u64 ⇄ BigInt,
  // i64 ⇄ PlatformInt64)
  // ---------------------------------------------------------------------------

  @override
  Future<LocalIdentityDto> localIdentity() async {
    final i = await frb.localIdentity();
    return LocalIdentityDto(
      id: i.id.toInt(),
      idFormate: i.idFormate,
      empreinte: i.empreinte,
    );
  }

  @override
  Future<String> generateEphemeralPassword() =>
      frb.generateEphemeralPassword();

  @override
  Future<List<AddressBookEntryDto>> listContacts() async {
    final contacts = await frb.listContacts();
    return contacts.map(_contactDepuis).toList();
  }

  @override
  Future<AddressBookEntryDto> addContact({
    required String alias,
    required int id,
    required String groupe,
    required List<String> etiquettes,
  }) async {
    try {
      final entree = await frb.addContact(
        alias: alias,
        id: BigInt.from(id),
        groupe: groupe,
        etiquettes: etiquettes,
      );
      return _contactDepuis(entree);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> updateContact({
    required int id,
    required String alias,
    required String groupe,
    required List<String> etiquettes,
  }) async {
    try {
      await frb.updateContact(
        id: BigInt.from(id),
        alias: alias,
        groupe: groupe,
        etiquettes: etiquettes,
      );
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> removeContact({required int id}) async {
    try {
      await frb.removeContact(id: BigInt.from(id));
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> setFavorite({required int id, required bool favori}) async {
    try {
      await frb.setFavorite(id: BigInt.from(id), favori: favori);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<List<String>> listGroups() => frb.listGroups();

  @override
  Future<void> addGroup({required String nom}) async {
    try {
      await frb.addGroup(nom: nom);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<List<SettingDto>> getSettings() async {
    final settings = await frb.getSettings();
    return [for (final s in settings) SettingDto(cle: s.cle, valeur: s.valeur)];
  }

  @override
  Future<String?> getSetting({required String cle}) =>
      frb.getSetting(cle: cle);

  @override
  Future<void> setSetting({required String cle, required String valeur}) async {
    try {
      await frb.setSetting(cle: cle, valeur: valeur);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> applyAutostart({required bool actif}) async {
    try {
      await frb.applyAutostart(actif: actif);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> recordSession({required int id, required String alias}) =>
      frb.recordSession(id: BigInt.from(id), alias: alias);

  @override
  Future<List<RecentSessionDto>> recentSessions() async {
    final sessions = await frb.recentSessions();
    return [
      for (final s in sessions)
        RecentSessionDto(
          id: s.id.toInt(),
          alias: s.alias,
          timestamp: s.timestamp.toInt(),
        ),
    ];
  }

  @override
  Future<List<RecordingDto>> listRecordings({String? dir}) async {
    final recordings = await frb.listRecordings(dir: dir);
    return [
      for (final r in recordings)
        RecordingDto(
          chemin: r.chemin,
          nom: r.nom,
          date: r.date.toInt(),
          dureeS: r.dureeS,
          tailleOctets: r.tailleOctets.toInt(),
        ),
    ];
  }

  @override
  Future<UnattendedConfigDto> unattendedConfig() async {
    final c = await frb.unattendedConfig();
    return UnattendedConfigDto(
      aMotDePasse: c.aMotDePasse,
      appareilsDeConfiance: [
        for (final id in c.appareilsDeConfiance) id.toInt(),
      ],
    );
  }

  @override
  Future<void> setUnattendedPassword({required String pwd}) async {
    try {
      await frb.setUnattendedPassword(pwd: pwd);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<bool> verifyUnattendedPassword({required String pwd}) =>
      frb.verifyUnattendedPassword(pwd: pwd);

  @override
  Future<void> addTrustedDevice({required int id}) =>
      frb.addTrustedDevice(id: BigInt.from(id));

  @override
  Future<void> removeTrustedDevice({required int id}) async {
    try {
      await frb.removeTrustedDevice(id: BigInt.from(id));
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> recordAccess({required int peerId, required bool accepte}) =>
      frb.recordAccess(peerId: BigInt.from(peerId), accepte: accepte);

  @override
  Future<List<AccessLogEntryDto>> accessLog() async {
    final journal = await frb.accessLog();
    return [
      for (final e in journal)
        AccessLogEntryDto(
          peerId: e.peerId.toInt(),
          peerIdFormate: e.peerIdFormate,
          timestamp: e.timestamp.toInt(),
          accepte: e.accepte,
        ),
    ];
  }

  // ---------------------------------------------------------------------------
  // Admission automatique : liste blanche (ACL) et invitations éphémères —
  // délégation aux fonctions générées (u64 ⇄ BigInt, Uint64List ⇄ List<int>) ;
  // les `Result<_, String>` du cœur (ID absent de la liste, profil ou code
  // inconnu, persistance impossible) sont retransformés en [NovaApiException]
  // au message français affichable.
  // ---------------------------------------------------------------------------

  @override
  Future<List<int>> listAdmissionAllowlist() async {
    try {
      final ids = await frb.listAdmissionAllowlist();
      return [for (final id in ids) id.toInt()];
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> addAdmissionAllowed({required int id}) async {
    try {
      await frb.addAdmissionAllowed(id: BigInt.from(id));
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> removeAdmissionAllowed({required int id}) async {
    try {
      await frb.removeAdmissionAllowed(id: BigInt.from(id));
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<String> createInvite({
    required String profil,
    required int ttlMinutes,
  }) async {
    try {
      return await frb.createInvite(profil: profil, ttlMinutes: ttlMinutes);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<List<InviteDto>> listInvites() async {
    try {
      final invitations = await frb.listInvites();
      return invitations.map(_inviteDepuis).toList();
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  @override
  Future<void> revokeInvite({required String code}) async {
    try {
      await frb.revokeInvite(code: code);
    } catch (e) {
      throw NovaApiException(_message(e));
    }
  }

  // ---------------------------------------------------------------------------
  // Conversions internes
  // ---------------------------------------------------------------------------

  /// Aplatit une entrée de carnet générée (id `u64` ⇄ `BigInt`, dernière
  /// connexion `i64` ⇄ `PlatformInt64`).
  static AddressBookEntryDto _contactDepuis(frb.AddressBookEntryDto c) =>
      AddressBookEntryDto(
        id: c.id.toInt(),
        alias: c.alias,
        groupe: c.groupe,
        etiquettes: c.etiquettes,
        favori: c.favori,
        derniereConnexion: c.derniereConnexion?.toInt(),
      );

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

  static frb.RegionDto _regionVers(RegionDto r) => frb.RegionDto(
        x: r.x,
        y: r.y,
        largeur: r.largeur,
        hauteur: r.hauteur,
      );

  static RegionDto _regionDepuis(frb.RegionDto r) => RegionDto(
        x: r.x,
        y: r.y,
        largeur: r.largeur,
        hauteur: r.hauteur,
      );

  static TunnelOuvertDto _tunnelDepuis(frb.TunnelOuvertDto t) => TunnelOuvertDto(
        adresseLocale: t.adresseLocale,
        portLocal: t.portLocal,
      );

  /// Aplatit un moniteur généré (index et dimensions déjà des `int` côté
  /// généré : `u32` Rust ⇄ `int` Dart).
  static MonitorInfoDto _moniteurDepuis(frb.MonitorInfoDto m) => MonitorInfoDto(
        index: m.index,
        largeur: m.largeur,
        hauteur: m.hauteur,
        principal: m.principal,
      );

  /// Aplatit une entrée de listing distant générée (taille et horodatage de
  /// modification `u64` ⇄ `BigInt`).
  static EntreeFsDto _entreeFsDepuis(frb.EntreeFsDto e) => EntreeFsDto(
        nom: e.nom,
        taille: e.taille.toInt(),
        estDossier: e.estDossier,
        modifieLe: e.modifieLe?.toInt(),
      );

  /// Aplatit un pair découvert généré (id `u64` ⇄ `BigInt`).
  static DiscoveredPeerDto _pairDecouvertDepuis(frb.DiscoveredPeerDto p) =>
      DiscoveredPeerDto(
        id: p.id.toInt(),
        idFormate: p.idFormate,
        nom: p.nom,
        adresse: p.adresse,
      );

  /// Aplatit une invitation générée (temps restant `u64` ⇄ `BigInt`).
  static InviteDto _inviteDepuis(frb.InviteDto i) => InviteDto(
        code: i.code,
        profil: i.profil,
        expireDansS: i.expireDansS.toInt(),
      );

  static frb.AnnotationDto _annotationVers(AnnotationDto a) => frb.AnnotationDto(
        genre: a.genre,
        points: a.points,
        couleurArgb: a.couleurArgb,
        epaisseur: a.epaisseur,
        texte: a.texte,
      );

  static AnnotationDto _annotationDepuis(frb.AnnotationDto a) => AnnotationDto(
        genre: a.genre,
        points: a.points,
        couleurArgb: a.couleurArgb,
        epaisseur: a.epaisseur,
        texte: a.texte,
      );

  /// Aplatit les métadonnées d'enregistrement (id, durée et nombre d'images
  /// `u64` ⇄ `BigInt` ; dimensions et fps déjà des `int` côté généré).
  static RecordingInfoDto _recordingInfoDepuis(frb.RecordingInfoDto i) =>
      RecordingInfoDto(
        id: i.id.toInt(),
        largeur: i.largeur,
        hauteur: i.hauteur,
        fps: i.fps,
        dureeUs: i.dureeUs.toInt(),
        nbImages: i.nbImages.toInt(),
      );

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
        // Le contrat porte désormais ces axes (défauts : mode étendu + audio /
        // chat / fichiers / presse-papiers, reconnexion transparente activée).
        extendedFeatures: o.extendedFeatures,
        transferDir: o.transferDir,
        transportReconnect: o.transportReconnect,
        // Mot de passe d'admission automatique (accès non surveillé) : relayé
        // tel quel, `null` = dialogue d'approbation manuel côté hôte.
        motDePasse: o.motDePasse,
        // Code d'invitation éphémère (usage unique) : relayé tel quel,
        // `null` = pas d'invitation présentée.
        invitation: o.invitation,
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

  static ChatMessageDto _chatDepuis(frb.ChatMessageDto m) =>
      ChatMessageDto(fromRemote: m.fromRemote, text: m.text);

  /// Aplatit l'évènement de transfert généré (compteurs `u64` ⇄ `BigInt`).
  static TransferEventDto _transfertDepuis(frb.TransferEventDto e) =>
      TransferEventDto(
        kind: e.kind,
        fileIndex: e.fileIndex?.toInt(),
        fileName: e.fileName,
        bytesDone: e.bytesDone?.toInt(),
        bytesTotal: e.bytesTotal?.toInt(),
        sessionBytesDone: e.sessionBytesDone?.toInt(),
        sessionBytesTotal: e.sessionBytesTotal?.toInt(),
        percent: e.percent,
        bytesPerSec: e.bytesPerSec,
        etaSecs: e.etaSecs,
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
