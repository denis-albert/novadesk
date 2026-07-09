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

import 'package:flutter/foundation.dart' show listEquals;

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
    this.extendedFeatures = true,
    this.transferDir,
    this.transportReconnect = true,
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

  /// Active les **fonctions étendues** de la session (canaux annexes : chat,
  /// transfert de fichiers, audio, presse-papiers, bascule moniteur, ainsi que
  /// confidentialité, cadre d'écran, tunnels et annotations), chacune gardée
  /// par sa permission. **Vrai par défaut côté UI** pour que le parcours
  /// complet soit démontrable ; `false` = session vidéo + entrées historique
  /// seulement.
  final bool extendedFeatures;

  /// Répertoire de réception des fichiers transférés (canal `Files`) ; `null` =
  /// dossier temporaire du système. Ignoré hors mode étendu.
  final String? transferDir;

  /// Reconnexion transparente **au niveau transport** pour un point de contact
  /// [SessionEndpointDirect] côté contrôleur. **Vrai par défaut côté UI.**
  final bool transportReconnect;

  @override
  bool operator ==(Object other) =>
      other is SessionOptionsDto &&
      other.permissions == permissions &&
      other.recordingPath == recordingPath &&
      other.deltaMode == deltaMode &&
      other.extendedFeatures == extendedFeatures &&
      other.transferDir == transferDir &&
      other.transportReconnect == transportReconnect;

  @override
  int get hashCode => Object.hash(permissions, recordingPath, deltaMode,
      extendedFeatures, transferDir, transportReconnect);

  @override
  String toString() => 'SessionOptionsDto(permissions: $permissions, '
      'recordingPath: $recordingPath, deltaMode: $deltaMode, '
      'extendedFeatures: $extendedFeatures, transferDir: $transferDir, '
      'transportReconnect: $transportReconnect)';
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
// État persistant : identité locale, carnet, réglages, historique,
// enregistrements, accès non surveillé (lot « état persistant »).
//
// Miroirs des DTO de `nd-ffi`. Les identifiants et horodatages sont des `int`
// Dart : l'adaptateur FRB convertit les `u64` (exposés en `BigInt`) et les
// `i64` (exposés en `PlatformInt64`) du pont.
// ---------------------------------------------------------------------------

/// Identité locale de l'appareil, prête à afficher — écran d'accueil « votre
/// ID » (miroir de `nd_ffi::LocalIdentityDto`).
class LocalIdentityDto {
  const LocalIdentityDto({
    required this.id,
    required this.idFormate,
    required this.empreinte,
  });

  /// `NovaId` brut à 9 chiffres, stable et persistant.
  final int id;

  /// ID au format groupé (« 123 456 789 »), prêt à afficher.
  final String idFormate;

  /// Empreinte hexadécimale (BLAKE2s, 64 caractères) de la clé publique
  /// statique — sert à la vérification d'identité (TOFU).
  final String empreinte;

  @override
  bool operator ==(Object other) =>
      other is LocalIdentityDto &&
      other.id == id &&
      other.idFormate == idFormate &&
      other.empreinte == empreinte;

  @override
  int get hashCode => Object.hash(id, idFormate, empreinte);

  @override
  String toString() =>
      'LocalIdentityDto(id: $id, idFormate: $idFormate, empreinte: $empreinte)';
}

/// Entrée du carnet d'adresses / contact enregistré (miroir de
/// `nd_ffi::AddressBookEntryDto`).
class AddressBookEntryDto {
  const AddressBookEntryDto({
    required this.id,
    required this.alias,
    required this.groupe,
    required this.etiquettes,
    required this.favori,
    this.derniereConnexion,
  });

  /// `NovaId` du contact.
  final int id;

  /// Nom lisible donné au contact.
  final String alias;

  /// Groupe de rangement (chaîne vide = non groupé).
  final String groupe;

  /// Étiquettes libres associées au contact.
  final List<String> etiquettes;

  /// Contact marqué comme favori.
  final bool favori;

  /// Horodatage Unix (secondes) de la dernière connexion, si connue.
  final int? derniereConnexion;

  @override
  bool operator ==(Object other) =>
      other is AddressBookEntryDto &&
      other.id == id &&
      other.alias == alias &&
      other.groupe == groupe &&
      listEquals(other.etiquettes, etiquettes) &&
      other.favori == favori &&
      other.derniereConnexion == derniereConnexion;

  @override
  int get hashCode => Object.hash(id, alias, groupe,
      Object.hashAll(etiquettes), favori, derniereConnexion);

  @override
  String toString() => 'AddressBookEntryDto(id: $id, alias: $alias, '
      'groupe: $groupe, favori: $favori)';
}

/// Réglage clé/valeur (les deux en texte : l'UI interprète selon la clé).
/// Miroir de `nd_ffi::SettingDto`.
class SettingDto {
  const SettingDto({required this.cle, required this.valeur});

  /// Clé du réglage (ex. `theme`, `langue`, `dossier_enregistrement`,
  /// `serveur_rendezvous`, `serveur_relais`, `serveurs_stun`,
  /// `prereglage_qualite`, `demarrer_avec_systeme`).
  final String cle;

  /// Valeur textuelle courante (surcharge persistée ou défaut).
  final String valeur;

  @override
  bool operator ==(Object other) =>
      other is SettingDto && other.cle == cle && other.valeur == valeur;

  @override
  int get hashCode => Object.hash(cle, valeur);

  @override
  String toString() => 'SettingDto(cle: $cle, valeur: $valeur)';
}

/// Une session récente (historique borné, le plus récent en tête). Miroir de
/// `nd_ffi::RecentSessionDto`.
class RecentSessionDto {
  const RecentSessionDto({
    required this.id,
    required this.alias,
    required this.timestamp,
  });

  /// `NovaId` du pair joint.
  final int id;

  /// Alias affiché au moment de la session.
  final String alias;

  /// Horodatage Unix (secondes) du démarrage de la session.
  final int timestamp;

  @override
  bool operator ==(Object other) =>
      other is RecentSessionDto &&
      other.id == id &&
      other.alias == alias &&
      other.timestamp == timestamp;

  @override
  int get hashCode => Object.hash(id, alias, timestamp);

  @override
  String toString() =>
      'RecentSessionDto(id: $id, alias: $alias, timestamp: $timestamp)';
}

/// Description d'un fichier d'enregistrement présent sur le disque (miroir de
/// `nd_ffi::RecordingDto`).
class RecordingDto {
  const RecordingDto({
    required this.chemin,
    required this.nom,
    required this.date,
    required this.dureeS,
    required this.tailleOctets,
  });

  /// Chemin absolu du fichier.
  final String chemin;

  /// Nom de fichier seul.
  final String nom;

  /// Date de modification (horodatage Unix, secondes).
  final int date;

  /// Durée en secondes (`0.0` si inconnue).
  final double dureeS;

  /// Taille du fichier en octets.
  final int tailleOctets;

  @override
  bool operator ==(Object other) =>
      other is RecordingDto &&
      other.chemin == chemin &&
      other.nom == nom &&
      other.date == date &&
      other.dureeS == dureeS &&
      other.tailleOctets == tailleOctets;

  @override
  int get hashCode => Object.hash(chemin, nom, date, dureeS, tailleOctets);

  @override
  String toString() =>
      'RecordingDto(nom: $nom, dureeS: $dureeS, tailleOctets: $tailleOctets)';
}

/// Configuration d'accès non surveillé, sans jamais exposer le secret (miroir
/// de `nd_ffi::UnattendedConfigDto`).
class UnattendedConfigDto {
  const UnattendedConfigDto({
    required this.aMotDePasse,
    required this.appareilsDeConfiance,
  });

  /// Un mot de passe permanent est configuré (seul un hachage salé est stocké).
  final bool aMotDePasse;

  /// `NovaId` des appareils de confiance.
  final List<int> appareilsDeConfiance;

  @override
  bool operator ==(Object other) =>
      other is UnattendedConfigDto &&
      other.aMotDePasse == aMotDePasse &&
      listEquals(other.appareilsDeConfiance, appareilsDeConfiance);

  @override
  int get hashCode =>
      Object.hash(aMotDePasse, Object.hashAll(appareilsDeConfiance));

  @override
  String toString() => 'UnattendedConfigDto(aMotDePasse: $aMotDePasse, '
      'appareilsDeConfiance: $appareilsDeConfiance)';
}

/// Une entrée du journal des accès non surveillés (miroir de
/// `nd_ffi::AccessLogEntryDto`).
class AccessLogEntryDto {
  const AccessLogEntryDto({
    required this.peerId,
    required this.peerIdFormate,
    required this.timestamp,
    required this.accepte,
  });

  /// `NovaId` brut de l'appelant.
  final int peerId;

  /// ID de l'appelant au format groupé, prêt à afficher.
  final String peerIdFormate;

  /// Horodatage Unix (secondes) de l'accès.
  final int timestamp;

  /// Vrai si l'accès a été accepté, faux s'il a été refusé.
  final bool accepte;

  @override
  bool operator ==(Object other) =>
      other is AccessLogEntryDto &&
      other.peerId == peerId &&
      other.peerIdFormate == peerIdFormate &&
      other.timestamp == timestamp &&
      other.accepte == accepte;

  @override
  int get hashCode => Object.hash(peerId, peerIdFormate, timestamp, accepte);

  @override
  String toString() => 'AccessLogEntryDto(peerId: $peerId, '
      'timestamp: $timestamp, accepte: $accepte)';
}

// ---------------------------------------------------------------------------
// Capacités avancées de session : confidentialité, cadre d'écran, tunnels,
// annotations et relecture d'enregistrements (lot « capacités moteur »).
//
// Miroirs des DTO de `nd-ffi` fraîchement exposés. Les identifiants et
// dimensions sont des `int` Dart : l'adaptateur FRB convertit les `u64`
// (exposés en `BigInt`) du pont.
// ---------------------------------------------------------------------------

/// Zone rectangulaire d'écran à partager (« cadre d'écran »), en **pixels du
/// moniteur** de l'hôte (miroir de `nd_ffi::RegionDto`).
class RegionDto {
  const RegionDto({
    required this.x,
    required this.y,
    required this.largeur,
    required this.hauteur,
  });

  /// Abscisse du coin supérieur gauche.
  final int x;

  /// Ordonnée du coin supérieur gauche.
  final int y;

  /// Largeur de la zone, en pixels.
  final int largeur;

  /// Hauteur de la zone, en pixels.
  final int hauteur;

  @override
  bool operator ==(Object other) =>
      other is RegionDto &&
      other.x == x &&
      other.y == y &&
      other.largeur == largeur &&
      other.hauteur == hauteur;

  @override
  int get hashCode => Object.hash(x, y, largeur, hauteur);

  @override
  String toString() =>
      'RegionDto(x: $x, y: $y, largeur: $largeur, hauteur: $hauteur)';
}

/// Tunnel TCP de session ouvert : coordonnées de l'écouteur local à utiliser
/// côté contrôleur (miroir de `nd_ffi::TunnelOuvertDto`, renvoyé par
/// [NativeApi.openTunnel]).
class TunnelOuvertDto {
  const TunnelOuvertDto({
    required this.adresseLocale,
    required this.portLocal,
  });

  /// Adresse locale réellement écoutée (« 127.0.0.1:port », port résolu si le
  /// port demandé était `0`).
  final String adresseLocale;

  /// Port local réellement écouté (pratique pour l'UI sans reparser l'adresse).
  final int portLocal;

  @override
  bool operator ==(Object other) =>
      other is TunnelOuvertDto &&
      other.adresseLocale == adresseLocale &&
      other.portLocal == portLocal;

  @override
  int get hashCode => Object.hash(adresseLocale, portLocal);

  @override
  String toString() =>
      'TunnelOuvertDto(adresseLocale: $adresseLocale, portLocal: $portLocal)';
}

/// Un **trait d'annotation** (« tableau blanc ») dessiné par-dessus l'image,
/// sous forme plate (miroir de `nd_ffi::AnnotationDto`).
///
///  * [genre] sélectionne la forme : `0` = trait libre / polyligne, `1` =
///    rectangle, `2` = ellipse, `3` = flèche, `4` = texte.
///  * [points] est une liste plate de coordonnées `[x0, y0, x1, y1, …]`,
///    **normalisées** dans `0.0..=1.0`. Le nombre de points attendu dépend du
///    [genre] : trait libre = 1 point ou plus ; rectangle = 2 points (coins
///    opposés) ; ellipse = 2 points (centre puis demi-axes) ; flèche =
///    2 points (origine, pointe) ; texte = 1 point (position).
///  * [couleurArgb] est empaquetée **ARGB** (`0xAARRGGBB`, convention `Color`
///    de Flutter).
///  * [epaisseur] est l'épaisseur du tracé ; pour le texte, la hauteur de
///    police.
///  * [texte] ne concerne que le genre texte (`4`) : requis pour lui, `null`
///    sinon.
///
/// À la réception ([NativeApi.sessionAnnotationStream]), une couche de `n`
/// traits est livrée comme `n` [AnnotationDto] successifs.
class AnnotationDto {
  const AnnotationDto({
    required this.genre,
    required this.points,
    required this.couleurArgb,
    required this.epaisseur,
    this.texte,
  });

  /// Forme du trait (voir la doc du type).
  final int genre;

  /// Coordonnées plates `[x0, y0, x1, y1, …]`, normalisées `0.0..=1.0`.
  final Float32List points;

  /// Couleur ARGB empaquetée (`0xAARRGGBB`).
  final int couleurArgb;

  /// Épaisseur du tracé (ou hauteur de police pour le texte).
  final double epaisseur;

  /// Contenu textuel (genre texte uniquement ; requis pour lui, sinon `null`).
  final String? texte;

  @override
  bool operator ==(Object other) =>
      other is AnnotationDto &&
      other.genre == genre &&
      listEquals(other.points, points) &&
      other.couleurArgb == couleurArgb &&
      other.epaisseur == epaisseur &&
      other.texte == texte;

  @override
  int get hashCode => Object.hash(
      genre, Object.hashAll(points), couleurArgb, epaisseur, texte);

  @override
  String toString() =>
      'AnnotationDto(genre: $genre, points: ${points.length}, '
      'couleurArgb: $couleurArgb, epaisseur: $epaisseur, texte: $texte)';
}

/// Métadonnées d'un enregistrement ouvert pour relecture (miroir de
/// `nd_ffi::RecordingInfoDto`, renvoyé par [NativeApi.openRecording]). [id]
/// indexe le lecteur pour [NativeApi.recordingNextFrame],
/// [NativeApi.recordingSeek] et [NativeApi.closeRecording].
class RecordingInfoDto {
  const RecordingInfoDto({
    required this.id,
    required this.largeur,
    required this.hauteur,
    required this.fps,
    required this.dureeUs,
    required this.nbImages,
  });

  /// Identifiant opaque du lecteur (à repasser aux fonctions `recording*`).
  final int id;

  /// Largeur des images, en pixels.
  final int largeur;

  /// Hauteur des images, en pixels.
  final int hauteur;

  /// Cadence nominale, en images par seconde.
  final int fps;

  /// Durée de l'enregistrement, en microsecondes.
  final int dureeUs;

  /// Nombre d'images de l'enregistrement.
  final int nbImages;

  @override
  bool operator ==(Object other) =>
      other is RecordingInfoDto &&
      other.id == id &&
      other.largeur == largeur &&
      other.hauteur == hauteur &&
      other.fps == fps &&
      other.dureeUs == dureeUs &&
      other.nbImages == nbImages;

  @override
  int get hashCode => Object.hash(id, largeur, hauteur, fps, dureeUs, nbImages);

  @override
  String toString() => 'RecordingInfoDto(id: $id, ${largeur}x$hauteur, '
      'fps: $fps, dureeUs: $dureeUs, nbImages: $nbImages)';
}

// ---------------------------------------------------------------------------
// Plan de contrôle de session : moniteurs réels et infos système du pair
// (lot « contrôles de session »).
//
// Miroirs des DTO de `nd-ffi`. Comme pour [RegionDto], l'adaptateur FRB
// convertit les types générés vers ces miroirs écrits à la main.
// ---------------------------------------------------------------------------

/// Un écran de l'hôte publié sur le plan de contrôle (miroir de
/// `nd_ffi::MonitorInfoDto`, lui-même miroir plat de `nd_core::RemoteMonitor`) :
/// remplace l'« Écran 1/2 » codé en dur du sous-menu moniteurs.
class MonitorInfoDto {
  const MonitorInfoDto({
    required this.index,
    required this.largeur,
    required this.hauteur,
    required this.principal,
  });

  /// Index du moniteur — l'argument attendu par [NativeApi.switchMonitor].
  final int index;

  /// Largeur en pixels.
  final int largeur;

  /// Hauteur en pixels.
  final int hauteur;

  /// Vrai pour le moniteur principal.
  final bool principal;

  @override
  bool operator ==(Object other) =>
      other is MonitorInfoDto &&
      other.index == index &&
      other.largeur == largeur &&
      other.hauteur == hauteur &&
      other.principal == principal;

  @override
  int get hashCode => Object.hash(index, largeur, hauteur, principal);

  @override
  String toString() => 'MonitorInfoDto(index: $index, ${largeur}x$hauteur, '
      'principal: $principal)';
}

/// Infos système du pair (miroir de `nd_ffi::PeerInfoDto`, lui-même miroir
/// plat de `nd_core::PeerInfo`) : remplace le contenu inventé du panneau
/// « Infos système » de la session.
class PeerInfoDto {
  const PeerInfoDto({required this.hote, required this.os});

  /// Nom d'hôte de la machine distante.
  final String hote;

  /// Système d'exploitation (chaîne libre, ex. « windows (x86_64) »).
  final String os;

  @override
  bool operator ==(Object other) =>
      other is PeerInfoDto && other.hote == hote && other.os == os;

  @override
  int get hashCode => Object.hash(hote, os);

  @override
  String toString() => 'PeerInfoDto(hote: $hote, os: $os)';
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

  // -------------------------------------------------------------------------
  // État persistant (lot « état persistant »)
  // -------------------------------------------------------------------------

  /// Identité locale, créée et persistée au premier appel puis rechargée à
  /// l'identique ensuite. Miroir de `nd_ffi::local_identity`.
  Future<LocalIdentityDto> localIdentity();

  /// Génère un mot de passe éphémère **lisible** (session ponctuelle),
  /// non persisté. Miroir de `nd_ffi::generate_ephemeral_password`.
  Future<String> generateEphemeralPassword();

  /// Liste tous les contacts du carnet. Miroir de `nd_ffi::list_contacts`.
  Future<List<AddressBookEntryDto>> listContacts();

  /// Ajoute un contact et renvoie l'entrée créée ; lève [NovaApiException] si
  /// l'`id` existe déjà. Un `groupe` non vide est ajouté à la liste des
  /// groupes. Miroir de `nd_ffi::add_contact`.
  Future<AddressBookEntryDto> addContact({
    required String alias,
    required int id,
    required String groupe,
    required List<String> etiquettes,
  });

  /// Met à jour l'alias, le groupe et les étiquettes d'un contact ; lève
  /// [NovaApiException] si l'`id` est inconnu. Le favori et la dernière
  /// connexion ne sont pas touchés. Miroir de `nd_ffi::update_contact`.
  Future<void> updateContact({
    required int id,
    required String alias,
    required String groupe,
    required List<String> etiquettes,
  });

  /// Retire un contact du carnet ; lève [NovaApiException] si l'`id` est
  /// inconnu. Miroir de `nd_ffi::remove_contact`.
  Future<void> removeContact({required int id});

  /// Marque (ou démarque) un contact comme favori ; lève [NovaApiException] si
  /// l'`id` est inconnu. Miroir de `nd_ffi::set_favorite`.
  Future<void> setFavorite({required int id, required bool favori});

  /// Liste les groupes déclarés du carnet. Miroir de `nd_ffi::list_groups`.
  Future<List<String>> listGroups();

  /// Ajoute un groupe (éventuellement vide de contacts) ; lève
  /// [NovaApiException] si le nom est vide ou déjà présent. Miroir de
  /// `nd_ffi::add_group`.
  Future<void> addGroup({required String nom});

  /// Renvoie tous les réglages effectifs (défauts fusionnés avec les surcharges
  /// persistées), triés par clé. Miroir de `nd_ffi::get_settings`.
  Future<List<SettingDto>> getSettings();

  /// Valeur effective d'un réglage (`null` si la clé est inconnue). Miroir de
  /// `nd_ffi::get_setting`.
  Future<String?> getSetting({required String cle});

  /// Définit (persiste) la valeur d'un réglage ; lève [NovaApiException] si la
  /// clé est vide. Miroir de `nd_ffi::set_setting`.
  Future<void> setSetting({required String cle, required String valeur});

  /// Journalise le démarrage d'une session (à appeler au moment de se
  /// connecter) : entrée en tête de l'historique (dédupliquée par `id`, bornée)
  /// et dernière connexion du contact rafraîchie. Miroir de
  /// `nd_ffi::record_session`.
  Future<void> recordSession({required int id, required String alias});

  /// Sessions récentes, de la plus récente à la plus ancienne. Miroir de
  /// `nd_ffi::recent_sessions`.
  Future<List<RecentSessionDto>> recentSessions();

  /// Liste les enregistrements (`.mp4`/`.ndr`) d'un dossier — [dir] s'il est
  /// fourni, sinon le réglage `dossier_enregistrement`, sinon le dossier par
  /// défaut. Un dossier absent renvoie une liste vide, triée du plus récent au
  /// plus ancien. Miroir de `nd_ffi::list_recordings`.
  Future<List<RecordingDto>> listRecordings({String? dir});

  /// Configuration d'accès non surveillé. Miroir de `nd_ffi::unattended_config`.
  Future<UnattendedConfigDto> unattendedConfig();

  /// Définit le mot de passe permanent d'accès non surveillé (stocké **haché et
  /// salé**). Un mot de passe vide efface la configuration. Miroir de
  /// `nd_ffi::set_unattended_password`.
  Future<void> setUnattendedPassword({required String pwd});

  /// Vérifie un mot de passe candidat contre le hachage stocké (`false` si
  /// aucun mot de passe n'est configuré). Miroir de
  /// `nd_ffi::verify_unattended_password`.
  Future<bool> verifyUnattendedPassword({required String pwd});

  /// Ajoute un appareil à la liste de confiance (sans effet s'il y figure
  /// déjà). Miroir de `nd_ffi::add_trusted_device`.
  Future<void> addTrustedDevice({required int id});

  /// Retire un appareil de la liste de confiance ; lève [NovaApiException] s'il
  /// n'y figure pas. Miroir de `nd_ffi::remove_trusted_device`.
  Future<void> removeTrustedDevice({required int id});

  /// Ajoute une entrée au journal des accès (append) : à appeler quand une
  /// demande d'accès non surveillé est tranchée. Miroir de
  /// `nd_ffi::record_access`.
  Future<void> recordAccess({required int peerId, required bool accepte});

  /// Renvoie le journal des accès, du plus récent au plus ancien. Miroir de
  /// `nd_ffi::access_log`.
  Future<List<AccessLogEntryDto>> accessLog();

  // -------------------------------------------------------------------------
  // Capacités avancées de session (lot « capacités moteur ») — chacune gardée
  // par le mode étendu ([SessionOptionsDto.extendedFeatures]) côté cœur. Le
  // [sessionId] est du même type (`int`) que celui de [sendChat]/[switchMonitor].
  // -------------------------------------------------------------------------

  /// Active ou lève le **mode confidentialité** de la session : l'hôte cesse de
  /// diffuser son écran réel (rideau noir). L'état effectif se relit via
  /// [privacyActive]. Sans effet hors mode étendu. Miroir de
  /// `nd_ffi::set_privacy`.
  Future<void> setPrivacy(int sessionId, bool actif);

  /// État du mode confidentialité connu localement (rideau actif à afficher).
  /// Miroir de `nd_ffi::privacy_active`.
  Future<bool> privacyActive(int sessionId);

  /// Restreint la zone d'écran partagée au [region] fourni, ou **rétablit le
  /// plein écran** avec `null`. Sans effet hors mode étendu. Miroir de
  /// `nd_ffi::set_session_region`.
  Future<void> setSessionRegion(int sessionId, RegionDto? region);

  /// Cadre d'écran actuellement demandé (`null` = plein écran). Miroir de
  /// `nd_ffi::session_requested_region`.
  Future<RegionDto?> sessionRequestedRegion(int sessionId);

  /// Ouvre un **tunnel TCP de session** : écoute sur `127.0.0.1:portLocal`
  /// ([portLocal] `== 0` → port éphémère) et relaie chaque connexion locale
  /// vers [cible] (« ip:port ») à travers la session. Renvoie l'adresse locale
  /// écoutée ; lève [NovaApiException] si [cible] est mal formée ou si
  /// l'écouteur local ne peut être lié. Miroir de `nd_ffi::open_tunnel`.
  Future<TunnelOuvertDto> openTunnel(
    int sessionId,
    int portLocal,
    String cible,
  );

  /// Ferme **tous** les tunnels TCP ouverts pour la session (idempotent).
  /// Miroir de `nd_ffi::close_tunnels`.
  Future<void> closeTunnels(int sessionId);

  /// Envoie une [annotation] au pair (un trait). Les annotations reçues du pair
  /// arrivent sur [sessionAnnotationStream]. Sans effet hors mode étendu ; lève
  /// [NovaApiException] si le DTO est mal formé (genre inconnu, points
  /// incohérents, texte manquant). Miroir de `nd_ffi::send_annotation`.
  Future<void> sendAnnotation(int sessionId, AnnotationDto annotation);

  /// Flux des annotations **reçues** du pair, à raison d'un [AnnotationDto] par
  /// trait de la couche reçue. Miroir de `nd_ffi::session_annotation_stream`.
  Stream<AnnotationDto> sessionAnnotationStream(int sessionId);

  /// Ouvre un enregistrement (`.mp4` ou `.ndr`, format auto-détecté) pour
  /// relecture et renvoie ses métadonnées + un identifiant opaque ; lève
  /// [NovaApiException] en cas d'échec. L'enregistrement vit jusqu'à
  /// [closeRecording]. Miroir de `nd_ffi::open_recording`.
  Future<RecordingInfoDto> openRecording(String chemin);

  /// Décode et renvoie la **prochaine image** de l'enregistrement [id] (même
  /// [VideoFrameDto] que [sessionVideoStream]), ou `null` en fin de flux.
  /// Miroir de `nd_ffi::recording_next_frame`.
  Future<VideoFrameDto?> recordingNextFrame(int id);

  /// Repositionne la lecture sur l'image-clé la plus proche avant (ou à)
  /// [timestampUs] (microsecondes). Le prochain [recordingNextFrame] repart de
  /// cette image-clé. Miroir de `nd_ffi::recording_seek`.
  Future<void> recordingSeek(int id, int timestampUs);

  /// Ferme l'enregistrement [id] et libère ses ressources (identifiant
  /// invalidé). Miroir de `nd_ffi::close_recording`.
  Future<void> closeRecording(int id);

  // -------------------------------------------------------------------------
  // Plan de contrôle de session (lot « contrôles de session ») : permissions à
  // chaud, préréglage de qualité, enregistrement à chaud, moniteurs réels,
  // infos système du pair. Même [sessionId] que [sendChat]/[switchMonitor].
  // -------------------------------------------------------------------------

  /// Renégocie **une** permission de la session **en cours** : accorde
  /// (`autorise` vrai) ou retire (faux) la capacité [capacite] de l'ensemble
  /// vivant — l'hôte l'applique au vol à son filtre d'injection. [capacite]
  /// est une **clé stable** parmi : `voir_ecran`, `souris`, `clavier`,
  /// `presse_papiers_lecture`, `presse_papiers_ecriture`, `fichiers_envoi`,
  /// `fichiers_reception`, `audio`, `redemarrage`, `enregistrement`,
  /// `confidentialite`, `tunnel` ; toute autre clé lève [NovaApiException]
  /// (sans toucher à la session). Sans effet hors mode étendu. Miroir de
  /// `nd_ffi::session_set_permission`.
  Future<void> sessionSetPermission(
    int sessionId,
    String capacite,
    bool autorise,
  );

  /// Applique un **préréglage de qualité** à l'encodeur hôte : [preset] parmi
  /// `auto`, `fluide`, `equilibre`, `netteté` (mappé vers un profil ABR et un
  /// plafond de débit ; l'ABR continue de dégrader **sous** le plafond). Un
  /// préréglage inconnu lève [NovaApiException]. Sans effet hors mode étendu.
  /// Miroir de `nd_ffi::session_set_quality`.
  Future<void> sessionSetQuality(int sessionId, String preset);

  /// Démarre (avec un [chemin] MP4) ou arrête (`null`) l'**enregistrement
  /// local** de l'hôte **en cours de session** : démarrer ouvre une nouvelle
  /// époque MP4, arrêter clôt proprement le fichier (relisible). Côté hôte
  /// uniquement (sans effet côté contrôleur) ; lève [NovaApiException] si la
  /// session est inconnue. Miroir de `nd_ffi::session_set_recording`.
  Future<void> sessionSetRecording(int sessionId, String? chemin);

  /// Liste des **moniteurs réels** de l'hôte, publiée par lui sur le canal
  /// `Control` à l'établissement (rôle contrôleur). Liste **vide** tant que
  /// l'annonce n'est pas arrivée ou si l'hôte n'a aucun écran énumérable ;
  /// l'index de chaque entrée est celui qu'attend [switchMonitor]. Miroir de
  /// `nd_ffi::session_monitors`.
  Future<List<MonitorInfoDto>> sessionMonitors(int sessionId);

  /// **Infos système du pair** (nom d'hôte + OS) publiées par l'hôte à
  /// l'établissement (rôle contrôleur) ; lève [NovaApiException] tant que
  /// l'annonce n'est pas arrivée (ou si la session est inconnue). Miroir de
  /// `nd_ffi::session_peer_info`.
  Future<PeerInfoDto> sessionPeerInfo(int sessionId);

  // -------------------------------------------------------------------------
  // Réseau annexe : Wake-on-LAN
  // -------------------------------------------------------------------------

  /// Réveille un appareil par **Wake-on-LAN** : émet le « paquet magique » pour
  /// l'adresse [mac] (« 01:23:45:67:89:AB », séparateurs `:` ou `-`, casse
  /// indifférente) vers [broadcast] (« ip:port ») ou, si `null`/vide, vers
  /// `255.255.255.255:9` (diffusion limitée au sous-réseau local, port 9). Lève
  /// [NovaApiException] si la MAC ou l'adresse de diffusion est invalide, ou si
  /// l'émission UDP échoue. Miroir de `nd_ffi::send_wol`.
  Future<void> sendWol(String mac, {String? broadcast});
}
