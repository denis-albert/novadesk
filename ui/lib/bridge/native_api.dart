/// Contrat Dart attendu du pont `flutter_rust_bridge` vers la façade Rust
/// `nd-ffi` (fichier source : `crates/nd-ffi/src/api.rs`).
///
/// Ce fichier est un **miroir écrit à la main** des DTO et fonctions publiques
/// de la façade. Une fois le binding généré (voir `lib/bridge/README.md`),
/// un adaptateur `FrbNativeApi implements NativeApi` déléguera aux fonctions
/// générées ; en attendant, [`MockNativeApi`](mock_api.dart) rend l'UI
/// entièrement navigable sans le cœur Rust.
///
/// Conventions de correspondance avec `nd-ffi` :
///  * `u64` Rust ↔ `int` Dart (64 bits signés — largement suffisant pour les
///    ID NovaDesk à 9 chiffres ; si la configuration FRB expose `u64` en
///    `BigInt`, l'adaptateur fera la conversion).
///  * `Result<_, String>` Rust ↔ `Future` qui **lève** [NovaApiException]
///    portant le message d'erreur français, affichable tel quel.
///  * `enum` Rust à variantes porteuses de données ↔ classe `sealed` Dart.
library;

import 'dart:typed_data';

// ---------------------------------------------------------------------------
// Erreurs
// ---------------------------------------------------------------------------

/// Erreur remontée par la façade `nd-ffi` (`Result<_, String>` côté Rust).
///
/// Le [message] est en français et prêt à afficher (bandeau, snackbar…).
class NovaApiException implements Exception {
  const NovaApiException(this.message);

  final String message;

  @override
  String toString() => 'NovaApiException: $message';
}

// ---------------------------------------------------------------------------
// Informations générales
// ---------------------------------------------------------------------------

/// Informations générales sur l'application (miroir de `nd_ffi::AppInfo`).
class AppInfo {
  const AppInfo({required this.version});

  /// Version du moteur/protocole, ex. `"0.1"`.
  final String version;

  @override
  bool operator ==(Object other) => other is AppInfo && other.version == version;

  @override
  int get hashCode => version.hashCode;

  @override
  String toString() => 'AppInfo(version: $version)';
}

// ---------------------------------------------------------------------------
// Rôle et état de session
// ---------------------------------------------------------------------------

/// Rôle du poste local dans la session (miroir de `nd_ffi::SessionRoleDto`).
enum SessionRoleDto {
  /// Ce poste pilote l'autre.
  controller,

  /// Ce poste est piloté.
  controlled,
}

/// État de session lisible par l'UI (miroir de `nd_ffi::SessionStateDto`).
enum SessionStateDto {
  /// Aucune session active.
  idle,

  /// Résolution de l'ID pair via le rendez-vous.
  resolving,

  /// Établissement du transport (NAT traversal / relais).
  connecting,

  /// Handshake cryptographique en cours.
  handshaking,

  /// Session établie et média en cours.
  active,

  /// Coupure réseau : tentative de reconnexion rapide.
  reconnecting,

  /// Session terminée.
  closed,
}

/// Libellés français, identiques à `SessionStateDto::label()` côté Rust.
extension SessionStateDtoLabel on SessionStateDto {
  /// Libellé court et stable, prêt à afficher tel quel.
  String get label => switch (this) {
        SessionStateDto.idle => 'inactive',
        SessionStateDto.resolving => 'résolution du pair',
        SessionStateDto.connecting => 'connexion',
        SessionStateDto.handshaking => 'authentification',
        SessionStateDto.active => 'active',
        SessionStateDto.reconnecting => 'reconnexion',
        SessionStateDto.closed => 'terminée',
      };
}

/// Photographie de l'état d'une session, prête à afficher
/// (miroir de `nd_ffi::SessionStatusDto`).
class SessionStatusDto {
  const SessionStatusDto({required this.state, this.peer});

  /// Libellé de l'état courant (voir [SessionStateDtoLabel.label]).
  final String state;

  /// ID du pair au format groupé (`"123 456 789"`), si connu.
  final String? peer;

  @override
  bool operator ==(Object other) =>
      other is SessionStatusDto && other.state == state && other.peer == peer;

  @override
  int get hashCode => Object.hash(state, peer);

  @override
  String toString() => 'SessionStatusDto(state: $state, peer: $peer)';
}

// ---------------------------------------------------------------------------
// Permissions et configuration de session
// ---------------------------------------------------------------------------

/// Permissions de session sous forme plate (miroir de `nd_ffi::PermissionsDto`).
class PermissionsDto {
  const PermissionsDto({
    required this.keyboard,
    required this.mouse,
    required this.clipboard,
    required this.files,
    required this.audio,
    required this.viewOnly,
  });

  /// Contrôle complet (clavier, souris, presse-papiers, fichiers, audio).
  factory PermissionsDto.full() => const PermissionsDto(
        keyboard: true,
        mouse: true,
        clipboard: true,
        files: true,
        audio: true,
        viewOnly: false,
      );

  /// Observation seule : rien n'est injecté ni transféré.
  /// C'est aussi le défaut prudent du moteur (`Permissions::default`).
  factory PermissionsDto.viewOnly() => const PermissionsDto(
        keyboard: false,
        mouse: false,
        clipboard: false,
        files: false,
        audio: false,
        viewOnly: true,
      );

  final bool keyboard;
  final bool mouse;
  final bool clipboard;
  final bool files;
  final bool audio;

  /// Si vrai, la session est en lecture seule (aucune entrée injectée).
  final bool viewOnly;

  @override
  bool operator ==(Object other) =>
      other is PermissionsDto &&
      other.keyboard == keyboard &&
      other.mouse == mouse &&
      other.clipboard == clipboard &&
      other.files == files &&
      other.audio == audio &&
      other.viewOnly == viewOnly;

  @override
  int get hashCode =>
      Object.hash(keyboard, mouse, clipboard, files, audio, viewOnly);

  @override
  String toString() =>
      'PermissionsDto(keyboard: $keyboard, mouse: $mouse, clipboard: $clipboard, '
      'files: $files, audio: $audio, viewOnly: $viewOnly)';
}

/// Paramètres de démarrage d'une session, sous forme plate
/// (miroir de `nd_ffi::SessionConfigDto`).
class SessionConfigDto {
  const SessionConfigDto({
    required this.role,
    required this.localId,
    this.peerId,
    required this.permissions,
  });

  final SessionRoleDto role;

  /// ID NovaDesk du poste local.
  final int localId;

  /// ID du pair à joindre (requis pour le rôle contrôleur).
  final int? peerId;

  /// Permissions initiales (le poste contrôlé fait foi).
  final PermissionsDto permissions;

  @override
  bool operator ==(Object other) =>
      other is SessionConfigDto &&
      other.role == role &&
      other.localId == localId &&
      other.peerId == peerId &&
      other.permissions == permissions;

  @override
  int get hashCode => Object.hash(role, localId, peerId, permissions);

  @override
  String toString() =>
      'SessionConfigDto(role: $role, localId: $localId, peerId: $peerId, '
      'permissions: $permissions)';
}

// ---------------------------------------------------------------------------
// Événements d'entrée
// ---------------------------------------------------------------------------

/// Événement d'entrée sous forme plate (miroir de `nd_ffi::InputEventDto`).
///
/// La sérialisation binaire reste celle de `nd-proto`
/// ([NativeApi.encodeInputEvent] / [NativeApi.decodeInputEvent]) : ces classes
/// n'existent que pour que l'UI n'importe jamais les types du protocole.
sealed class InputEventDto {
  const InputEventDto();
}

/// Déplacement absolu, coordonnées normalisées 0.0–1.0 sur le moniteur.
final class InputMouseMoveAbs extends InputEventDto {
  const InputMouseMoveAbs({
    required this.x,
    required this.y,
    required this.monitor,
  });

  final double x;
  final double y;
  final int monitor;

  @override
  bool operator ==(Object other) =>
      other is InputMouseMoveAbs &&
      other.x == x &&
      other.y == y &&
      other.monitor == monitor;

  @override
  int get hashCode => Object.hash(x, y, monitor);

  @override
  String toString() => 'InputMouseMoveAbs(x: $x, y: $y, monitor: $monitor)';
}

/// Déplacement relatif en pixels.
final class InputMouseMoveRel extends InputEventDto {
  const InputMouseMoveRel({required this.dx, required this.dy});

  final double dx;
  final double dy;

  @override
  bool operator ==(Object other) =>
      other is InputMouseMoveRel && other.dx == dx && other.dy == dy;

  @override
  int get hashCode => Object.hash(dx, dy);

  @override
  String toString() => 'InputMouseMoveRel(dx: $dx, dy: $dy)';
}

/// Bouton souris (0 = gauche, 1 = droit, 2 = milieu, 3 = X1, 4 = X2).
final class InputMouseButton extends InputEventDto {
  const InputMouseButton({required this.button, required this.down});

  final int button;
  final bool down;

  @override
  bool operator ==(Object other) =>
      other is InputMouseButton && other.button == button && other.down == down;

  @override
  int get hashCode => Object.hash(button, down);

  @override
  String toString() => 'InputMouseButton(button: $button, down: $down)';
}

/// Molette (crans ; positif = haut/droite).
final class InputScroll extends InputEventDto {
  const InputScroll({required this.dx, required this.dy});

  final double dx;
  final double dy;

  @override
  bool operator ==(Object other) =>
      other is InputScroll && other.dx == dx && other.dy == dy;

  @override
  int get hashCode => Object.hash(dx, dy);

  @override
  String toString() => 'InputScroll(dx: $dx, dy: $dy)';
}

/// Touche par scancode physique.
final class InputKey extends InputEventDto {
  const InputKey({required this.scancode, required this.down});

  final int scancode;
  final bool down;

  @override
  bool operator ==(Object other) =>
      other is InputKey && other.scancode == scancode && other.down == down;

  @override
  int get hashCode => Object.hash(scancode, down);

  @override
  String toString() => 'InputKey(scancode: $scancode, down: $down)';
}

/// Caractère Unicode (point de code).
final class InputUnicode extends InputEventDto {
  const InputUnicode({required this.codepoint});

  final int codepoint;

  @override
  bool operator ==(Object other) =>
      other is InputUnicode && other.codepoint == codepoint;

  @override
  int get hashCode => codepoint.hashCode;

  @override
  String toString() => 'InputUnicode(codepoint: $codepoint)';
}

// ---------------------------------------------------------------------------
// Session live : trame vidéo, statistiques, endpoint
// ---------------------------------------------------------------------------

/// Image décodée prête à afficher, poussée par [NativeApi.sessionVideoStream]
/// (miroir de `nd_ffi::VideoFrameDto`, lui-même miroir de
/// `nd_codec::DecodedFrame`).
///
/// C'est l'unité du **rendu 100 % Dart** : l'UI convertit chaque trame en
/// `ui.Image` via `decodeImageFromPixels` puis la peint (aucun plugin natif).
class VideoFrameDto {
  const VideoFrameDto({
    required this.width,
    required this.height,
    required this.rgba,
  });

  /// Largeur en pixels.
  final int width;

  /// Hauteur en pixels.
  final int height;

  /// Pixels RGBA (largeur × hauteur × 4 octets), ordre R, G, B, A.
  final Uint8List rgba;

  @override
  String toString() => 'VideoFrameDto(${width}x$height, ${rgba.length} o)';
}

/// Instantané des statistiques d'une session, rafraîchies en continu par le
/// moteur (miroir de `nd_ffi::SessionStatsDto`).
///
/// Les cinq premiers champs sont historiques (lot 04) ; les suivants exposent
/// les statistiques **enrichies** du moteur (lot §2 : permissions, ABR,
/// enregistrement, reconnexion) et le backend d'encodage réellement à l'œuvre.
/// Certains champs sont propres à un rôle (ex. `targetBitrateKbps`, `abrLevel`,
/// `framesRecorded`, `encoderBackend` côté hôte) : ils valent `0`/`null` quand
/// ils ne s'appliquent pas au poste local, et l'UI les masque alors.
class SessionStatsDto {
  const SessionStatsDto({
    required this.fps,
    required this.rttUs,
    required this.bytesIn,
    required this.bytesOut,
    required this.frames,
    this.inputsDenied = 0,
    this.targetBitrateKbps = 0,
    this.abrLevel = 0,
    this.framesRecorded = 0,
    this.reconnects = 0,
    this.encoderBackend,
  });

  /// Images décodées par seconde (fenêtre glissante d'une seconde).
  final double fps;

  /// RTT du chemin réseau en microsecondes.
  final int rttUs;

  /// Octets utiles reçus (après déchiffrement, hors handshake).
  final int bytesIn;

  /// Octets utiles émis (avant chiffrement, hors handshake).
  final int bytesOut;

  /// Trames décodées livrées depuis le début de la session.
  final int frames;

  /// Entrées reçues mais **refusées par les permissions** (côté contrôlé).
  final int inputsDenied;

  /// Débit cible actuellement appliqué à l'encodeur par l'ABR (hôte), kbit/s.
  final int targetBitrateKbps;

  /// Palier ABR courant (hôte) : 0 = plein régime, croît en dégradant.
  final int abrLevel;

  /// Images écrites dans l'enregistrement local (hôte), toutes époques confondues.
  final int framesRecorded;

  /// Reconnexions **réussies** depuis le début de la session.
  final int reconnects;

  /// Nom du backend d'encodage réellement à l'œuvre côté hôte (« NVENC »,
  /// repli logiciel…) ; `null` tant que l'encodeur n'est pas créé ou côté
  /// contrôleur.
  final String? encoderBackend;

  @override
  bool operator ==(Object other) =>
      other is SessionStatsDto &&
      other.fps == fps &&
      other.rttUs == rttUs &&
      other.bytesIn == bytesIn &&
      other.bytesOut == bytesOut &&
      other.frames == frames &&
      other.inputsDenied == inputsDenied &&
      other.targetBitrateKbps == targetBitrateKbps &&
      other.abrLevel == abrLevel &&
      other.framesRecorded == framesRecorded &&
      other.reconnects == reconnects &&
      other.encoderBackend == encoderBackend;

  @override
  int get hashCode => Object.hash(fps, rttUs, bytesIn, bytesOut, frames,
      inputsDenied, targetBitrateKbps, abrLevel, framesRecorded, reconnects,
      encoderBackend);

  @override
  String toString() =>
      'SessionStatsDto(fps: $fps, rttUs: $rttUs, bytesIn: $bytesIn, '
      'bytesOut: $bytesOut, frames: $frames, inputsDenied: $inputsDenied, '
      'targetBitrateKbps: $targetBitrateKbps, abrLevel: $abrLevel, '
      'framesRecorded: $framesRecorded, reconnects: $reconnects, '
      'encoderBackend: $encoderBackend)';
}

/// Point d'accès réseau au démarrage d'une session
/// (miroir de `nd_ffi::SessionEndpointDto`).
sealed class SessionEndpointDto {
  const SessionEndpointDto();
}

/// La session lie un écouteur QUIC local (`127.0.0.1`, port éphémère) et
/// **accepte** la connexion entrante (rôle hôte typique). L'adresse et le
/// certificat à transmettre au pair se relisent via
/// [NativeApi.sessionListenInfo].
final class SessionEndpointLoopback extends SessionEndpointDto {
  const SessionEndpointLoopback();
}

/// La session **se connecte** directement à [addr] (« ip:port ») avec le
/// certificat auto-signé (DER) épinglé [certDer] du pair (rôle contrôleur).
final class SessionEndpointDirect extends SessionEndpointDto {
  const SessionEndpointDirect({required this.addr, required this.certDer});

  /// Adresse QUIC (UDP) du pair, ex. « 127.0.0.1:53211 ».
  final String addr;

  /// Certificat DER du pair, épinglé à la connexion.
  final Uint8List certDer;
}

/// La session se met en relation **par ID** via un serveur de rendez-vous :
/// STUN → hole punching → QUIC sur la socket percée, avec repli relais
/// optionnel. C'est le seul point de contact **reconnectable** (miroir de
/// `nd_ffi::SessionEndpointDto::ByRendezvous`). Toutes les adresses sont en
/// texte (« ip:port »).
final class SessionEndpointByRendezvous extends SessionEndpointDto {
  const SessionEndpointByRendezvous({
    required this.server,
    this.stunServers = const [],
    this.relay,
  });

  /// Adresse du serveur de rendez-vous (`nd-signaling`), ex. « 203.0.113.7:9000 ».
  final String server;

  /// Serveurs STUN interrogés pour le candidat réflexif. Liste vide =
  /// candidats locaux seulement (LAN / boucle locale).
  final List<String> stunServers;

  /// Relais de repli (`nd-relay`) quand le punch échoue ; `null` = pas de repli.
  final String? relay;
}

/// Coordonnées d'écoute d'une session hôte démarrée en
/// [SessionEndpointLoopback] (miroir de `nd_ffi::ListenInfoDto`).
class ListenInfoDto {
  const ListenInfoDto({required this.addr, required this.certDer});

  /// Adresse d'écoute effective (« 127.0.0.1:port »).
  final String addr;

  /// Certificat auto-signé (DER) à épingler côté pair.
  final Uint8List certDer;
}

/// Options avancées de démarrage d'une session (miroir de
/// `nd_ffi::SessionOptionsDto`).
///
/// Complète [SessionConfigDto] : celui-ci porte le rôle, les ID et les
/// permissions ; celui-là affine le comportement côté **contrôlé** (filtre de
/// permissions granulaire faisant autorité, enregistrement local, encodage
/// delta). [NativeApi.startSession] équivaut à un démarrage avec les options
/// par défaut du moteur.
class SessionOptionsDto {
  const SessionOptionsDto({
    required this.permissions,
    this.recordingPath,
    this.deltaMode = false,
  });

  /// Permissions granulaires appliquées avant chaque injection d'entrée
  /// (contrôlé). Fait autorité sur les permissions de [SessionConfigDto].
  final PermissionsDto permissions;

  /// Chemin du MP4 à écrire pour l'enregistrement local (hôte) ; `null` =
  /// pas d'enregistrement.
  final String? recordingPath;

  /// Encodage delta **opt-in** : à n'activer que si la capture renseigne
  /// fidèlement les régions modifiées.
  final bool deltaMode;

  @override
  bool operator ==(Object other) =>
      other is SessionOptionsDto &&
      other.permissions == permissions &&
      other.recordingPath == recordingPath &&
      other.deltaMode == deltaMode;

  @override
  int get hashCode => Object.hash(permissions, recordingPath, deltaMode);

  @override
  String toString() => 'SessionOptionsDto(permissions: $permissions, '
      'recordingPath: $recordingPath, deltaMode: $deltaMode)';
}

/// Demande d'accès entrante vers un hôte « accès non surveillé », poussée par
/// [NativeApi.unattendedIncomingStream] pour chaque appelant à approuver
/// (miroir de `nd_ffi::IncomingRequestDto`).
///
/// L'UI présente la demande (dialogue d'acceptation) puis tranche via
/// [NativeApi.approveIncoming] avec le même [peerId].
class IncomingRequestDto {
  const IncomingRequestDto({required this.peerId, required this.peerIdFormate});

  /// ID NovaDesk brut de l'appelant (à repasser à [NativeApi.approveIncoming]).
  final int peerId;

  /// ID de l'appelant au format groupé (« 123 456 789 »), prêt à afficher.
  final String peerIdFormate;

  @override
  bool operator ==(Object other) =>
      other is IncomingRequestDto &&
      other.peerId == peerId &&
      other.peerIdFormate == peerIdFormate;

  @override
  int get hashCode => Object.hash(peerId, peerIdFormate);

  @override
  String toString() =>
      'IncomingRequestDto(peerId: $peerId, peerIdFormate: $peerIdFormate)';
}

// ---------------------------------------------------------------------------
// Canaux média annexes : discussion et transfert de fichiers
// (lot « session media »)
// ---------------------------------------------------------------------------

/// Message de discussion poussé par [NativeApi.sessionChatStream] (miroir de
/// `nd_ffi::ChatMessageDto`, lui-même miroir de `nd_core::ChatMessage`).
class ChatMessageDto {
  const ChatMessageDto({required this.fromRemote, required this.text});

  /// `true` si le message vient du pair distant ; `false` pour l'écho local
  /// d'un message que ce poste vient d'émettre via [NativeApi.sendChat].
  final bool fromRemote;

  /// Texte du message (UTF-8).
  final String text;

  @override
  bool operator ==(Object other) =>
      other is ChatMessageDto &&
      other.fromRemote == fromRemote &&
      other.text == text;

  @override
  int get hashCode => Object.hash(fromRemote, text);

  @override
  String toString() => 'ChatMessageDto(fromRemote: $fromRemote, text: $text)';
}

/// Évènement de progression d'un transfert de fichiers, poussé par
/// [NativeApi.sessionTransferStream] (miroir de `nd_ffi::TransferEventDto`).
///
/// Structure **plate** : l'UI branche sur [kind] ; les champs non pertinents
/// pour un `kind` donné valent `null`. Les compteurs d'octets sont des `int`
/// Dart (l'adaptateur FRB convertit les `u64`/`BigInt` du pont).
class TransferEventDto {
  const TransferEventDto({
    required this.kind,
    this.fileIndex,
    this.fileName,
    this.bytesDone,
    this.bytesTotal,
    this.sessionBytesDone,
    this.sessionBytesTotal,
    this.percent,
    this.bytesPerSec,
    this.etaSecs,
  });

  /// Nature de l'évènement : `"started"` (début d'un fichier), `"progress"`
  /// (avancement), `"completed"` (fichier terminé), `"finished"` (file
  /// entièrement transférée) ou `"cancelled"` (annulation).
  final String kind;

  /// Index (0-basé) du fichier concerné (`started`/`progress`/`completed`).
  final int? fileIndex;

  /// Nom du fichier concerné (`started`/`progress`/`completed`).
  final String? fileName;

  /// Octets du **fichier courant** déjà transférés.
  final int? bytesDone;

  /// Taille totale du **fichier courant**.
  final int? bytesTotal;

  /// Octets déjà transférés pour l'ensemble de la file (`progress`).
  final int? sessionBytesDone;

  /// Taille totale connue de la file (`progress`).
  final int? sessionBytesTotal;

  /// Pourcentage accompli de la **session** dans `[0, 100]` (`progress`).
  final double? percent;

  /// Débit instantané moyen de la session en octets/seconde (`progress`).
  final double? bytesPerSec;

  /// Temps estimé avant la fin de la session en secondes (`progress`).
  final double? etaSecs;

  @override
  String toString() =>
      'TransferEventDto(kind: $kind, fileName: $fileName, '
      'bytesDone: $bytesDone, bytesTotal: $bytesTotal, percent: $percent)';
}

// ---------------------------------------------------------------------------
// Interface de la façade
// ---------------------------------------------------------------------------

/// Surface d'appel vers le cœur Rust, telle qu'exposée par `nd-ffi`.
///
/// Toutes les méthodes sont asynchrones : `flutter_rust_bridge` exécute le
/// Rust dans un isolate travailleur et renvoie des `Future`, l'isolate UI
/// reste libre pour le rendu 60 fps (plan 10 §10.2).
abstract interface class NativeApi {
  /// Renvoie les informations générales de l'application
  /// (miroir de `nd_ffi::app_info`).
  Future<AppInfo> appInfo();

  /// Version du moteur, ex. écran « À propos »
  /// (miroir de `nd_ffi::engine_version_string`).
  Future<String> engineVersionString();

  /// Formate un ID NovaDesk pour affichage : 9 chiffres groupés par 3,
  /// ex. `123 456 789` (miroir de `nd_ffi::format_nova_id`).
  Future<String> formatNovaId({required int id});

  /// Analyse un ID NovaDesk saisi par l'utilisateur. Tolère le format groupé
  /// et tout espacement parasite ; lève [NovaApiException] sinon
  /// (miroir de `nd_ffi::parse_nova_id`).
  Future<int> parseNovaId({required String texte});

  /// Construit un statut de session affichable à partir d'un état et d'un
  /// pair éventuel (miroir de `nd_ffi::session_status`).
  Future<SessionStatusDto> sessionStatus({
    required SessionStateDto state,
    int? peerId,
  });

  /// Construit et valide une configuration de session côté UI ; lève
  /// [NovaApiException] avec un message français en cas de saisie invalide
  /// (miroir de `nd_ffi::new_session_config`).
  Future<SessionConfigDto> newSessionConfig({
    required SessionRoleDto role,
    required int localId,
    int? peerId,
    required PermissionsDto permissions,
  });

  /// Sérialise un événement d'entrée au format binaire du canal `Input`
  /// (miroir de `nd_ffi::encode_input_event`).
  Future<Uint8List> encodeInputEvent({required InputEventDto event});

  /// Désérialise un événement d'entrée ; lève [NovaApiException] si les
  /// octets sont illisibles (miroir de `nd_ffi::decode_input_event`).
  Future<InputEventDto> decodeInputEvent({required Uint8List data});

  // -------------------------------------------------------------------------
  // Session live (dépend du lot 03 : streaming FFI)
  // -------------------------------------------------------------------------

  /// Démarre une session réelle (QUIC → Noise → capture/codec/entrées) et
  /// renvoie son **identifiant opaque** ; lève [NovaApiException] en cas
  /// d'échec (miroir de `nd_ffi::start_session`).
  Future<int> startSession({
    required SessionConfigDto config,
    required SessionEndpointDto endpoint,
  });

  /// Démarre une session comme [startSession], mais avec des options avancées
  /// ([SessionOptionsDto] : permissions granulaires, enregistrement local,
  /// encodage delta). Miroir de `nd_ffi::start_session_with_options`.
  Future<int> startSessionWithOptions({
    required SessionConfigDto config,
    required SessionEndpointDto endpoint,
    required SessionOptionsDto options,
  });

  /// Adresse et certificat d'écoute d'une session [SessionEndpointLoopback]
  /// (miroir de `nd_ffi::session_listen_info`).
  Future<ListenInfoDto> sessionListenInfo(int id);

  /// Flux des transitions d'état de la session
  /// (`resolving → … → active → … → closed`), miroir de
  /// `nd_ffi::session_state_stream`.
  Stream<SessionStateDto> sessionStateStream(int id);

  /// Flux des trames vidéo décodées (rôle contrôleur). **Fonction clé du
  /// rendu** : l'UI peint chaque [VideoFrameDto] (miroir de
  /// `nd_ffi::session_video_stream`).
  Stream<VideoFrameDto> sessionVideoStream(int id);

  /// Attend (au plus [timeoutMs]) la prochaine transition d'état ; `null` si
  /// aucune n'arrive ou si la session est terminée. Repli synchrone,
  /// mutuellement exclusif avec [sessionStateStream]
  /// (miroir de `nd_ffi::wait_session_state`).
  Future<SessionStateDto?> waitSessionState(int id, {required int timeoutMs});

  /// Collecte jusqu'à [maxFrames] trames décodées (au plus [timeoutMs]).
  /// Repli synchrone, mutuellement exclusif avec [sessionVideoStream]
  /// (miroir de `nd_ffi::collect_video_frames`).
  Future<List<VideoFrameDto>> collectVideoFrames(
    int id, {
    required int maxFrames,
    required int timeoutMs,
  });

  /// Instantané des statistiques de la session (fps, RTT, octets, trames),
  /// miroir de `nd_ffi::session_stats`.
  Future<SessionStatsDto> sessionStats(int id);

  /// Dernière erreur d'exécution du moteur, `null` tant que la session vit ou
  /// si elle s'est close proprement. À afficher quand l'état passe à `closed`
  /// (miroir de `nd_ffi::session_last_error`).
  Future<String?> sessionLastError(int id);

  /// Pousse un événement d'entrée vers le pair (rôle contrôleur) : les octets
  /// partent sur le canal `Input` chiffré (miroir de `nd_ffi::send_input`).
  Future<void> sendInput(int id, InputEventDto event);

  /// Arrête la session et invalide son identifiant
  /// (miroir de `nd_ffi::stop_session`).
  Future<void> stopSession(int id);

  // -------------------------------------------------------------------------
  // Canaux média annexes : discussion, transfert, audio, moniteurs
  // (lot « session media » — chaque canal gardé par sa permission, mode étendu)
  // -------------------------------------------------------------------------

  /// Flux des messages de discussion de la session : messages **reçus** du pair
  /// ([ChatMessageDto.fromRemote] vrai) et **échos locaux** des messages émis
  /// via [sendChat] (faux). Miroir de `nd_ffi::session_chat_stream`.
  Stream<ChatMessageDto> sessionChatStream(int id);

  /// Envoie un message de discussion au pair ; l'écho local est livré sur
  /// [sessionChatStream] une fois le message émis. Miroir de
  /// `nd_ffi::send_chat`.
  Future<void> sendChat(int id, String texte);

  /// Flux des évènements de progression du transfert de fichiers, tant côté
  /// **émetteur** que **récepteur**. Miroir de `nd_ffi::session_transfer_stream`.
  Stream<TransferEventDto> sessionTransferStream(int id);

  /// Démarre l'**envoi** d'une file de fichiers ([chemins] locaux) vers le
  /// pair ; la progression est observable sur [sessionTransferStream]. Gardé
  /// par la permission « fichiers ». Miroir de `nd_ffi::send_files`.
  Future<void> sendFiles(int id, List<String> chemins);

  /// Active ou désactive l'audio de la session (émission côté hôte, lecture
  /// côté contrôleur). Sans effet si la permission audio n'est pas accordée.
  /// Miroir de `nd_ffi::set_audio_enabled`.
  Future<void> setAudioEnabled(int id, bool actif);

  /// Demande à l'hôte de diffuser le **moniteur** d'index [moniteur] (bascule
  /// multi-écran ; un index hors bornes est ignoré au mieux). Miroir de
  /// `nd_ffi::switch_monitor`.
  Future<void> switchMonitor(int id, int moniteur);

  // -------------------------------------------------------------------------
  // Hôte « accès non surveillé » (lot §2b)
  // -------------------------------------------------------------------------

  /// Démarre un hôte « accès non surveillé » : publie [localId] au serveur de
  /// [rendezvous] (« ip:port »), génère une identité TLS et attend les
  /// appelants. Renvoie un **identifiant opaque d'hôte** (distinct des
  /// identifiants de session). Miroir de `nd_ffi::start_unattended_host`.
  ///
  /// Chaque appelant est soumis à **approbation pilotée par l'UI** : abonnez-
  /// vous à [unattendedIncomingStream] puis tranchez via [approveIncoming].
  /// [stunServers] (« ip:port », liste éventuellement vide) alimente le hole
  /// punching ; [permissions] filtre les entrées reçues (côté contrôlé).
  Future<int> startUnattendedHost({
    required int localId,
    required String rendezvous,
    required List<String> stunServers,
    required PermissionsDto permissions,
  });

  /// Flux des demandes d'accès entrantes de l'hôte [hostId]. À brancher juste
  /// après [startUnattendedHost] : une demande arrivée sans abonné n'est pas
  /// livrée et expirera (refus par défaut). Miroir de
  /// `nd_ffi::unattended_incoming_stream`.
  Stream<IncomingRequestDto> unattendedIncomingStream(int hostId);

  /// Tranche une demande entrante de l'hôte [hostId] : `accepter == true`
  /// débloque et sert la session, `false` la refuse. [peerId] est celui de la
  /// [IncomingRequestDto] reçue. Miroir de `nd_ffi::approve_incoming`.
  Future<void> approveIncoming({
    required int hostId,
    required int peerId,
    required bool accepter,
  });

  /// Instantané des statistiques cumulées des sessions servies par l'hôte
  /// [hostId] (entrées appliquées/refusées, débit ABR, octets…).
  /// `encoderBackend` reste `null` (non exposé par la poignée d'hôte).
  /// Miroir de `nd_ffi::unattended_stats`.
  Future<SessionStatsDto> unattendedStats(int hostId);

  /// Arrête l'hôte [hostId] et invalide son identifiant : réveille toute
  /// approbation en attente (refus). Miroir de `nd_ffi::stop_unattended_host`.
  Future<void> stopUnattendedHost(int hostId);
}
