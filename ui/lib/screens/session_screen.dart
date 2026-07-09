/// Fenêtre de session — vue « En session » de la maquette `novadesk-app.html` :
/// surface vidéo plein cadre sur fond noir, **barre d'outils flottante sombre**
/// centrée en haut (boutons uniformes groupés + popovers), overlay de connexion
/// (Résolution → Connexion → Authentification), bandeau de reconnexion, tableau
/// blanc, transfert deux volets, discussion, HUD encodeur/débit/latence.
///
/// Rendu vidéo **100 % pur Dart** (aucun `Texture`/plugin natif) : chaque
/// [VideoFrameDto] du flux `session_video_stream` est convertie en `ui.Image`
/// via `decodeImageFromPixels` (RGBA) puis peinte par [_PeintreVideo] en
/// conservant le ratio. Les images précédentes sont libérées (pas de fuite).
///
/// Cycle de vie réel : `start_session` à l'ouverture, abonnement aux flux
/// d'états et de trames, statistiques live via `session_stats`, `stop_session`
/// au `dispose`. Entrées réelles via `send_input`.
///
/// Capacités avancées réelles (lot « capacités moteur ») : mode
/// confidentialité (`set_privacy` / `privacy_active`), annotations
/// bidirectionnelles (`send_annotation` + flux du pair), tunnels TCP
/// (`open_tunnel` / `close_tunnels`) et cadre d'écran
/// (`set_session_region` / `session_requested_region`).
///
/// Plan de contrôle de session réel (lot « contrôles de session ») :
/// permissions renégociées **à chaud** (`session_set_permission`), préréglage
/// de qualité (`session_set_quality`), enregistrement à chaud
/// (`session_set_recording`), liste des **moniteurs réels** de l'hôte
/// (`session_monitors` → sous-menu écrans) et **infos système du pair**
/// (`session_peer_info` → panneau d'infos).
library;

import 'dart:async';
import 'dart:io' show Platform;
import 'dart:math' as math;
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart' show kIsWeb, ValueListenable;
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../bridge/native_api.dart';
import '../platform/window_shim.dart';
import '../state/providers.dart';
import '../theme/motion.dart';
import '../theme/nova_theme.dart';
import '../widgets/app_frame.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';
import '../widgets/session_state_badge.dart';

// Couleurs intrinsèquement sombres de la barre d'outils et des popovers de
// session (maquette : `.tool`, `.pop`, `.pit`…). La surface de session est
// toujours sombre : ces valeurs fixes reproduisent la maquette au pixel près.
const Color _kToolFond = Color(0xFF20242B);
const Color _kToolBordure = Color(0xFF2C313A);
const Color _kToolHover = Color(0xFF2C323B);
const Color _kToolIcone = Color(0xFFC7CDD6);
const Color _kToolInd = Color(0xFF8B94A5);
const Color _kEndIcone = Color(0xFFF0857E);
const Color _kPopFond = Color(0xFF23272E);
const Color _kPopBordure = Color(0xFF333A44);
const Color _kPopTexte = Color(0xFFE6E9EE);
const Color _kPopTexte2 = Color(0xFF79828F);
const Color _kPopHover = Color(0xFF2D333C);
const Color _kPopIcone = Color(0xFFAAB2C0);
const Color _kCkOff = Color(0xFF3A414C);

// Genres d'annotation du pont (voir [AnnotationDto.genre]).
const int _kAnnotationLibre = 0;
const int _kAnnotationRectangle = 1;
const int _kAnnotationEllipse = 2;
const int _kAnnotationFleche = 3;
const int _kAnnotationTexte = 4;

/// Palette du tableau blanc (ARGB, convention `Color` de Flutter — lisible sur
/// une capture sombre comme claire).
const List<int> _kPaletteAnnotation = [
  0xFFEF5350, // rouge
  0xFFFFB35C, // ambre
  0xFF3FB457, // vert
  0xFF5B93F0, // bleu
  0xFFFFFFFF, // blanc
];

/// Épaisseurs de trait proposées par le tableau blanc.
const List<double> _kEpaisseursAnnotation = [2, 4, 7];

/// Accent du mode « cadre d'écran » (rectangle élastique, poignées).
const Color _kCadreAccent = Color(0xFF5B93F0);

/// Arguments de navigation vers la fenêtre de session.
class SessionScreenArgs {
  const SessionScreenArgs({
    required this.config,
    required this.libellePair,
    this.endpoint = const SessionEndpointLoopback(),
    this.options,
  });

  /// Configuration validée par `new_session_config` (façade `nd-ffi`).
  final SessionConfigDto config;

  /// Alias du pair s'il est au carnet, sinon son ID formaté.
  final String libellePair;

  /// Point d'accès réseau de démarrage : [SessionEndpointByRendezvous] pour la
  /// connexion **par ID**, [SessionEndpointLoopback] par défaut.
  final SessionEndpointDto endpoint;

  /// Options avancées éventuelles (permissions granulaires, enregistrement…).
  final SessionOptionsDto? options;
}

class SessionScreen extends ConsumerStatefulWidget {
  const SessionScreen({super.key, required this.args});

  static const String route = NovaRoutes.session;

  final SessionScreenArgs args;

  @override
  ConsumerState<SessionScreen> createState() => _SessionScreenState();
}

class _SessionScreenState extends ConsumerState<SessionScreen> {
  final FocusNode _noeudFocus = FocusNode(debugLabel: 'surface-session');
  final TextEditingController _chatController = TextEditingController();
  final ScrollController _chatScroll = ScrollController();

  /// Champ de saisie du/des chemin(s) à envoyer (transfert sans plugin natif).
  final TextEditingController _cheminController = TextEditingController();

  /// Identifiant de la session ouverte par le cœur (`start_session`).
  int? _sessionId;

  StreamSubscription<SessionStateDto>? _abonnementEtat;
  StreamSubscription<VideoFrameDto>? _abonnementVideo;
  StreamSubscription<ChatMessageDto>? _abonnementChat;
  StreamSubscription<TransferEventDto>? _abonnementTransfert;
  Timer? _minuterieStats;

  /// File de transfert alimentée par `session_transfer_stream` (clé = index du
  /// fichier). Agrégats de session mis à jour à chaque évènement `progress`.
  final Map<int, _TransfertFichier> _transferts = {};
  bool _transfertActif = false;
  double _pourcentTransfert = 0;
  double _debitTransfert = 0;
  double _etaTransfert = 0;

  /// Fichiers de démonstration envoyés quand le champ de chemin est vide
  /// (aucun sélecteur natif disponible : ni admin ni symlinks ici).
  static const List<String> _fichiersDemoATransferer = [
    r'C:\Users\Public\Documents\rapport-Q2.pdf',
    r'C:\Users\Public\Documents\build-9.7.3.zip',
  ];

  /// Trame vidéo courante décodée en `ui.Image`, peinte par [_PeintreVideo].
  final ValueNotifier<ui.Image?> _trameCourante = ValueNotifier<ui.Image?>(null);

  bool _aRecuUneTrame = false;
  bool _decodageEnCours = false;
  SessionStatsDto? _stats;
  bool _sessionArretee = false;

  SessionStateDto _etat = SessionStateDto.idle;
  SessionStatusDto? _statut;
  bool _termine = false;
  bool _toastConnecte = false;

  // Barre d'outils / panneaux.
  int _moniteur = 0;
  String _qualite = 'Équilibré';
  String _modeTravail = 'Efficacité';
  // « Réduire à la fenêtre » par défaut : c'est le comportement effectif du
  // rendu (adapté au cadre) — la coche du popover dit la vérité.
  String _modeAffichage = 'Réduire à la fenêtre';
  String _modeClavier = 'Universel';
  bool _pleinEcran = false;
  bool _enregistre = false;
  bool _favori = false;
  bool _transmettreRaccourcis = true;
  bool _autoResolution = false;
  bool _modeNuit = false;
  bool _suivreCurseur = false;
  bool _curseurDistant = true;
  bool _ftOuvert = false;
  bool _chatOuvert = false;
  bool _wbOuvert = false;

  /// Popover sombre courant (un seul à la fois).
  OverlayEntry? _popover;

  // Permissions commutables en cours de session. Clavier/souris,
  // presse-papiers et transfert sont **renégociés à chaud** via
  // `session_set_permission` (clés du contrat : `clavier`/`souris`,
  // `presse_papiers_lecture`/`presse_papiers_ecriture`,
  // `fichiers_envoi`/`fichiers_reception`) ; l'audio passe par
  // `set_audio_enabled`, la confidentialité par `set_privacy`. « Bloquer les
  // entrées » et « verrouiller à la fin » restent locaux : le cœur n'expose
  // aucune capacité correspondante (cf. `capacite_depuis_cle`, nd-ffi).
  late bool _permAudio = _permissions.audio;
  late bool _permClavierSouris =
      _permissions.keyboard && !_permissions.viewOnly;
  late bool _permPressePapiers = _permissions.clipboard;
  late bool _permTransfert = _permissions.files;
  bool _permBloquerEntree = false;
  bool _permVerrouiller = false;

  /// Mode confidentialité **réel** : reflet local de `privacy_active`, piloté
  /// par `set_privacy` (rideau noir côté hôte).
  bool _permConfidentialite = false;

  // --- Plan de contrôle de session (lot « contrôles de session ») -----------

  /// Moniteurs **réels** de l'hôte (`session_monitors`) ; liste vide tant que
  /// l'annonce n'est pas arrivée → le sous-menu écrans passe en repli statique.
  List<MonitorInfoDto> _moniteurs = const [];

  /// Infos système du pair (`session_peer_info`) ; `null` tant que l'annonce
  /// n'est pas arrivée → le panneau d'infos affiche l'attente.
  PeerInfoDto? _infosPair;

  /// Chemin du MP4 en cours d'écriture (`session_set_recording`) ; `null`
  /// quand aucun enregistrement à chaud n'est actif.
  String? _cheminEnregistrement;

  /// Relance bornée du chargement du plan de contrôle tant que l'annonce de
  /// l'hôte n'est pas arrivée (voir [_chargerPlanControle]).
  int _tentativesPlanControle = 0;
  bool _chargementPlanControle = false;
  static const int _maxTentativesPlanControle = 30;

  // --- Capacités avancées de session (façade « capacités moteur ») ----------

  /// Traits d'annotation affichés : les miens et ceux reçus du pair.
  final List<AnnotationDto> _annotations = [];

  /// Traits envoyés en attente d'écho : le mock réémet chaque envoi sur le
  /// flux — on les reconnaît (égalité de valeur) pour ne pas peindre double.
  final List<AnnotationDto> _echosAttendus = [];

  /// Points (normalisés, à plat `x, y, …`) du trait libre en cours de dessin.
  final List<double> _pointsTraitEnCours = [];

  /// Point de départ (normalisé) du trait ou de la forme en cours.
  Offset? _formeDepart;

  /// Aperçu vivant du trait en cours (peint par [_PeintreAnnotations] sans
  /// reconstruire l'arbre à chaque mouvement).
  final ValueNotifier<AnnotationDto?> _apercuAnnotation =
      ValueNotifier<AnnotationDto?>(null);

  /// Incrémenté à chaque mutation de [_annotations] (repeint sans rebuild).
  final ValueNotifier<int> _revisionAnnotations = ValueNotifier<int>(0);

  StreamSubscription<AnnotationDto>? _abonnementAnnotations;

  /// Outil, couleur (ARGB) et épaisseur courants du tableau blanc.
  int _outilAnnotation = _kAnnotationLibre;
  int _couleurAnnotation = _kPaletteAnnotation.first;
  double _epaisseurAnnotation = 4;

  /// Mode « cadre d'écran » : sélection d'un rectangle sur la surface.
  bool _selectionCadre = false;
  Offset? _cadreDepart;
  Rect? _cadreEnCours;

  /// Cadre effectivement demandé au cœur (`null` = plein écran), reflet de
  /// `session_requested_region`.
  RegionDto? _regionActive;

  /// Tunnels TCP ouverts pendant la session (reflet local des `open_tunnel`).
  final List<_TunnelActif> _tunnels = [];

  int _evenementsEnvoyes = 0;

  DateTime _dernierEnvoiSouris = DateTime.fromMillisecondsSinceEpoch(0);
  static const Duration _intervalleSouris = Duration(milliseconds: 8);
  int _boutonEnfonce = 0;

  final List<_MessageChat> _messages = [
    const _MessageChat(
        texte: 'Session ouverte. Canal de discussion prêt.', deMoi: false),
  ];

  NativeApi get _api => ref.read(nativeApiProvider);

  bool get _estDesktop =>
      !kIsWeb && (Platform.isWindows || Platform.isMacOS || Platform.isLinux);

  PermissionsDto get _permissions => widget.args.config.permissions;

  bool get _sourisActive =>
      _etat == SessionStateDto.active &&
      !_permissions.viewOnly &&
      _permissions.mouse;

  bool get _clavierActif =>
      _etat == SessionStateDto.active &&
      !_permissions.viewOnly &&
      _permissions.keyboard;

  @override
  void initState() {
    super.initState();
    unawaited(_demarrerSession());
  }

  @override
  void dispose() {
    _fermerPopover();
    unawaited(_abonnementEtat?.cancel());
    unawaited(_abonnementVideo?.cancel());
    unawaited(_abonnementChat?.cancel());
    unawaited(_abonnementTransfert?.cancel());
    unawaited(_abonnementAnnotations?.cancel());
    _minuterieStats?.cancel();
    unawaited(_arreterMoteur());
    _trameCourante.value?.dispose();
    _trameCourante.dispose();
    _apercuAnnotation.dispose();
    _revisionAnnotations.dispose();
    if (_pleinEcran && _estDesktop) {
      unawaited(windowManager.setFullScreen(false));
    }
    _chatController.dispose();
    _chatScroll.dispose();
    _cheminController.dispose();
    _noeudFocus.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Cycle de vie réel de la session (piloté par le cœur Rust via NativeApi)
  // ---------------------------------------------------------------------------

  Future<void> _demarrerSession() async {
    try {
      // Mode delta (dirty-rects) activé d'office au démarrage : seuls les
      // rectangles modifiés sont ré-encodés — économie de bande passante, le
      // moteur fusionnant déjà les move/dirty-rects DXGI. Les options du
      // parcours appelant sont conservées telles quelles ; seul `deltaMode`
      // (faux par défaut dans la façade) est forcé à vrai si besoin.
      final optionsDemandees = widget.args.options;
      final options = optionsDemandees == null || optionsDemandees.deltaMode
          ? optionsDemandees
          : SessionOptionsDto(
              permissions: optionsDemandees.permissions,
              recordingPath: optionsDemandees.recordingPath,
              deltaMode: true,
              extendedFeatures: optionsDemandees.extendedFeatures,
              transferDir: optionsDemandees.transferDir,
              transportReconnect: optionsDemandees.transportReconnect,
            );
      final id = options == null
          ? await _api.startSession(
              config: widget.args.config,
              endpoint: widget.args.endpoint,
            )
          : await _api.startSessionWithOptions(
              config: widget.args.config,
              endpoint: widget.args.endpoint,
              options: options,
            );
      if (!mounted) {
        unawaited(_api.stopSession(id));
        return;
      }
      _sessionId = id;
      _abonnementEtat = _api.sessionStateStream(id).listen(
        (SessionStateDto etat) {
          unawaited(_surEtat(etat));
        },
        onError: (Object e) {
          _signalerErreurFatale(_messageErreur(e));
        },
      );
      _abonnementVideo = _api.sessionVideoStream(id).listen(
        _surTrameVideo,
        onError: (Object _) {
          // Trame corrompue : ignorée, le flux continue.
        },
      );
      _abonnementChat = _api.sessionChatStream(id).listen(
        _surMessageChat,
        onError: (Object _) {
          // Message illisible : ignoré, le flux continue.
        },
      );
      _abonnementTransfert = _api.sessionTransferStream(id).listen(
        _surEvenementTransfert,
        onError: (Object _) {
          // Évènement de transfert illisible : ignoré, le flux continue.
        },
      );
      _abonnementAnnotations = _api.sessionAnnotationStream(id).listen(
        _surAnnotationRecue,
        onError: (Object _) {
          // Annotation illisible : ignorée, le flux continue.
        },
      );
    } catch (e) {
      _signalerErreurFatale(_messageErreur(e));
    }
  }

  Future<void> _surEtat(SessionStateDto etat) async {
    await _appliquerEtat(etat);
    if (!mounted) return;
    if (etat == SessionStateDto.active) {
      _demarrerStats();
      unawaited(_synchroniserCapacites());
      // (Re)charge le plan de contrôle publié par l'hôte — compteur remis à
      // zéro pour couvrir aussi le retour d'une reconnexion.
      _tentativesPlanControle = 0;
      unawaited(_chargerPlanControle());
      if (!_toastConnecte) {
        _toastConnecte = true;
        NovaToast.montrer(context, 'Connecté à ${widget.args.libellePair}');
      }
    } else if (etat == SessionStateDto.closed) {
      await _surFermeture();
    }
  }

  Future<void> _appliquerEtat(SessionStateDto etat) async {
    final statut = await _api.sessionStatus(
      state: etat,
      peerId: widget.args.config.peerId,
    );
    if (!mounted) return;
    setState(() {
      _etat = etat;
      _statut = statut;
    });
  }

  Future<void> _surFermeture() async {
    if (_termine) return;
    String? erreur;
    final id = _sessionId;
    if (id != null) {
      try {
        erreur = await _api.sessionLastError(id);
      } catch (_) {
        // Erreur ignorée : on retourne quand même à l'accueil.
      }
    }
    if (!mounted) return;
    if (erreur != null) {
      _informer('Session terminée : $erreur');
    }
    await Future<void>.delayed(const Duration(milliseconds: 300));
    if (mounted) Navigator.of(context).pop();
  }

  void _surTrameVideo(VideoFrameDto trame) {
    if (_decodageEnCours) return;
    _decodageEnCours = true;
    ui.decodeImageFromPixels(
      trame.rgba,
      trame.width,
      trame.height,
      ui.PixelFormat.rgba8888,
      (ui.Image image) {
        _decodageEnCours = false;
        if (!mounted) {
          image.dispose();
          return;
        }
        final ancienne = _trameCourante.value;
        _trameCourante.value = image;
        ancienne?.dispose();
        if (!_aRecuUneTrame) {
          setState(() => _aRecuUneTrame = true);
        }
      },
    );
  }

  void _demarrerStats() {
    if (_minuterieStats != null) return;
    unawaited(_rafraichirStats());
    _minuterieStats = Timer.periodic(
      const Duration(seconds: 1),
      (_) => unawaited(_rafraichirStats()),
    );
  }

  Future<void> _rafraichirStats() async {
    final id = _sessionId;
    if (id == null) return;
    // L'annonce du plan de contrôle (moniteurs, infos pair) peut arriver un
    // peu après le passage à l'état actif : on retente au rythme des
    // statistiques tant qu'elle manque (borné).
    if (_moniteurs.isEmpty || _infosPair == null) {
      unawaited(_chargerPlanControle());
    }
    try {
      final stats = await _api.sessionStats(id);
      if (!mounted) return;
      setState(() => _stats = stats);
    } catch (_) {
      // Statistiques indisponibles : le HUD conserve la dernière valeur.
    }
  }

  /// Charge le plan de contrôle publié par l'hôte à l'établissement : liste
  /// des **moniteurs réels** (`session_monitors`) et **infos système du pair**
  /// (`session_peer_info`). Chaque volet est indépendant (une annonce absente
  /// n'empêche pas l'autre) ; relancé par [_rafraichirStats] tant qu'il manque
  /// des données, dans la limite de [_maxTentativesPlanControle] essais.
  Future<void> _chargerPlanControle() async {
    final id = _sessionId;
    if (id == null || _chargementPlanControle) return;
    if (_tentativesPlanControle >= _maxTentativesPlanControle) return;
    _tentativesPlanControle++;
    _chargementPlanControle = true;
    try {
      if (_moniteurs.isEmpty) {
        try {
          final moniteurs = await _api.sessionMonitors(id);
          if (!mounted) return;
          if (moniteurs.isNotEmpty) {
            setState(() => _moniteurs = moniteurs);
            _rafraichirPopover();
          }
        } catch (_) {
          // Liste non annoncée : le sous-menu écrans garde son repli statique.
        }
      }
      if (_infosPair == null) {
        try {
          final infos = await _api.sessionPeerInfo(id);
          if (!mounted) return;
          setState(() => _infosPair = infos);
          _rafraichirPopover();
        } catch (_) {
          // Annonce pas encore arrivée : le panneau d'infos garde l'attente.
        }
      }
    } finally {
      _chargementPlanControle = false;
    }
  }

  Future<void> _arreterMoteur() async {
    final id = _sessionId;
    if (id == null || _sessionArretee) return;
    _sessionArretee = true;
    try {
      await _api.stopSession(id);
    } catch (_) {
      // Arrêt best-effort.
    }
  }

  void _signalerErreurFatale(String message) {
    if (!mounted) return;
    _informer('Session interrompue : $message');
    Future<void>.delayed(const Duration(milliseconds: 350)).then((_) {
      if (mounted) Navigator.of(context).pop();
    });
  }

  String _messageErreur(Object e) =>
      e is NovaApiException ? e.message : e.toString();

  Future<void> _terminerSession() async {
    if (_termine) return;
    _termine = true;
    _fermerPopover();
    await _arreterMoteur();
    if (mounted) {
      await _appliquerEtat(SessionStateDto.closed);
    }
    if (_pleinEcran && _estDesktop) {
      _pleinEcran = false;
      await windowManager.setFullScreen(false);
    }
    await Future<void>.delayed(const Duration(milliseconds: 300));
    if (mounted) {
      Navigator.of(context).pop();
    }
  }

  // ---------------------------------------------------------------------------
  // Envoi des entrées (souris / clavier) vers le cœur
  // ---------------------------------------------------------------------------

  Future<void> _envoyer(InputEventDto evenement) async {
    final id = _sessionId;
    if (id == null) return;
    final estSouris = switch (evenement) {
      InputMouseMoveAbs() ||
      InputMouseMoveRel() ||
      InputMouseButton() ||
      InputScroll() =>
        true,
      InputKey() || InputUnicode() => false,
    };
    if (estSouris && !_sourisActive) return;
    if (!estSouris && !_clavierActif) return;

    try {
      await _api.sendInput(id, evenement);
    } catch (_) {
      return;
    }
    if (!mounted) return;
    setState(() => _evenementsEnvoyes++);
  }

  double _normaliser(double valeur, double maximum) =>
      maximum <= 0 ? 0 : math.min(1.0, math.max(0.0, valeur / maximum));

  void _surMouvement(PointerEvent evenement, BoxConstraints contraintes) {
    final maintenant = DateTime.now();
    if (maintenant.difference(_dernierEnvoiSouris) < _intervalleSouris) {
      return;
    }
    _dernierEnvoiSouris = maintenant;
    unawaited(_envoyer(InputMouseMoveAbs(
      x: _normaliser(evenement.localPosition.dx, contraintes.maxWidth),
      y: _normaliser(evenement.localPosition.dy, contraintes.maxHeight),
      monitor: _moniteur,
    )));
  }

  int _boutonDepuisMasque(int boutons) {
    if (boutons & kPrimaryMouseButton != 0) return 0;
    if (boutons & kSecondaryMouseButton != 0) return 1;
    if (boutons & kMiddleMouseButton != 0) return 2;
    if (boutons & kBackMouseButton != 0) return 3;
    if (boutons & kForwardMouseButton != 0) return 4;
    return 0;
  }

  void _surBoutonEnfonce(PointerDownEvent evenement) {
    _noeudFocus.requestFocus();
    _boutonEnfonce = _boutonDepuisMasque(evenement.buttons);
    unawaited(_envoyer(InputMouseButton(button: _boutonEnfonce, down: true)));
  }

  void _surBoutonRelache(PointerUpEvent evenement) {
    unawaited(_envoyer(InputMouseButton(button: _boutonEnfonce, down: false)));
  }

  void _surMolette(PointerSignalEvent evenement) {
    if (evenement is! PointerScrollEvent) return;
    unawaited(_envoyer(InputScroll(
      dx: evenement.scrollDelta.dx / 120.0,
      dy: -evenement.scrollDelta.dy / 120.0,
    )));
  }

  KeyEventResult _surTouche(FocusNode noeud, KeyEvent evenement) {
    if (evenement is KeyDownEvent &&
        evenement.logicalKey == LogicalKeyboardKey.f11) {
      unawaited(_basculerPleinEcran());
      return KeyEventResult.handled;
    }
    // Échap pendant l'établissement : équivaut au lien « Annuler » de l'overlay.
    if (evenement is KeyDownEvent &&
        evenement.logicalKey == LogicalKeyboardKey.escape &&
        _montrerOverlayConnexion) {
      unawaited(_terminerSession());
      return KeyEventResult.handled;
    }
    // Échap en mode « cadre d'écran » : annule la sélection en cours.
    if (evenement is KeyDownEvent &&
        evenement.logicalKey == LogicalKeyboardKey.escape &&
        _selectionCadre) {
      setState(() {
        _selectionCadre = false;
        _cadreDepart = null;
        _cadreEnCours = null;
      });
      _informer('Sélection du cadre annulée.');
      return KeyEventResult.handled;
    }
    if (evenement is KeyDownEvent &&
        evenement.logicalKey == LogicalKeyboardKey.escape &&
        _pleinEcran) {
      unawaited(_basculerPleinEcran());
      return KeyEventResult.handled;
    }
    if (!_clavierActif) return KeyEventResult.ignored;

    final enfoncee = evenement is! KeyUpEvent;
    unawaited(_envoyer(InputKey(
      scancode: evenement.physicalKey.usbHidUsage,
      down: enfoncee,
    )));
    final caractere = evenement.character;
    if (evenement is KeyDownEvent && caractere != null && caractere.isNotEmpty) {
      unawaited(_envoyer(InputUnicode(codepoint: caractere.runes.first)));
    }
    return KeyEventResult.handled;
  }

  // ---------------------------------------------------------------------------
  // Actions de la barre d'outils
  // ---------------------------------------------------------------------------

  Future<void> _basculerPleinEcran() async {
    if (!_estDesktop) {
      _informer('Plein écran indisponible sur cette plateforme.');
      return;
    }
    _pleinEcran = !_pleinEcran;
    await windowManager.setFullScreen(_pleinEcran);
    if (mounted) setState(() {});
  }

  Future<void> _envoyerCtrlAltSuppr() async {
    const touches = [
      PhysicalKeyboardKey.controlLeft,
      PhysicalKeyboardKey.altLeft,
      PhysicalKeyboardKey.delete,
    ];
    for (final touche in touches) {
      await _envoyer(InputKey(scancode: touche.usbHidUsage, down: true));
    }
    for (final touche in touches.reversed) {
      await _envoyer(InputKey(scancode: touche.usbHidUsage, down: false));
    }
    _informer('Ctrl+Alt+Suppr envoyé au poste distant.');
  }

  // ---------------------------------------------------------------------------
  // Enregistrement à chaud (`session_set_recording`)
  // ---------------------------------------------------------------------------

  /// Démarre ou arrête l'**enregistrement local à chaud** : démarrer compose
  /// le chemin depuis le réglage `dossier_enregistrement` + un nom horodaté
  /// puis appelle `session_set_recording(id, chemin)` ; arrêter appelle
  /// `session_set_recording(id, null)` (fichier clos proprement). La pastille
  /// rouge ne reflète que l'état **accepté par le cœur** (rien n'est basculé
  /// en cas d'erreur).
  Future<void> _basculerEnregistrement() async {
    final id = _sessionId;
    if (id == null) {
      _informer('Enregistrement : session non démarrée.');
      return;
    }
    if (_enregistre) {
      final chemin = _cheminEnregistrement;
      try {
        await _api.sessionSetRecording(id, null);
        if (!mounted) return;
        setState(() {
          _enregistre = false;
          _cheminEnregistrement = null;
        });
        NovaToast.montrer(
          context,
          chemin == null
              ? 'Enregistrement arrêté — fichier clos.'
              : 'Enregistrement arrêté — fichier clos : $chemin',
        );
      } catch (e) {
        if (mounted) _informer('Enregistrement : ${_messageErreur(e)}');
      }
      return;
    }
    final chemin = await _cheminEnregistrementHorodate();
    if (!mounted) return;
    try {
      await _api.sessionSetRecording(id, chemin);
      if (!mounted) return;
      setState(() {
        _enregistre = true;
        _cheminEnregistrement = chemin;
      });
      NovaToast.montrer(context, 'Enregistrement démarré : $chemin');
    } catch (e) {
      if (mounted) _informer('Enregistrement : ${_messageErreur(e)}');
    }
  }

  /// Chemin du MP4 à écrire : dossier du réglage `dossier_enregistrement`
  /// (relu via le provider des réglages) + `novadesk-<horodatage>.mp4`.
  /// Réglage vide ou indisponible → nom seul (le cœur écrit alors dans son
  /// dossier de travail par défaut).
  Future<String> _cheminEnregistrementHorodate() async {
    String dossier = '';
    try {
      final reglages = await ref.read(settingsProvider.future);
      dossier = reglages['dossier_enregistrement']?.trim() ?? '';
    } catch (_) {
      // Réglages indisponibles : repli sur le nom seul.
    }
    final maintenant = DateTime.now();
    String deux(int v) => v.toString().padLeft(2, '0');
    final horodatage =
        '${maintenant.year}${deux(maintenant.month)}${deux(maintenant.day)}'
        '-${deux(maintenant.hour)}${deux(maintenant.minute)}'
        '${deux(maintenant.second)}';
    final nom = 'novadesk-$horodatage.mp4';
    if (dossier.isEmpty) return nom;
    // Respecte le style de séparateur du dossier configuré (« C:\… » ou
    // « /home/… ») sans dépendre de la plateforme d'exécution.
    final separateur = dossier.contains('\\') ? '\\' : '/';
    final base = dossier.endsWith('\\') || dossier.endsWith('/')
        ? dossier.substring(0, dossier.length - 1)
        : dossier;
    return '$base$separateur$nom';
  }

  /// Toast d'information NovaDesk (remplace les SnackBar Material : un seul
  /// système de notifications, fidèle à la maquette `.toast`).
  void _informer(String message) {
    if (!mounted) return;
    NovaToast.montrer(context, message, info: true);
  }

  void _aVenir(String fonction) => _informer('$fonction — à venir.');

  // ---------------------------------------------------------------------------
  // Discussion réelle (canal `Control` du cœur via `send_chat` / chat stream)
  // ---------------------------------------------------------------------------

  /// Envoi d'un message : appelle `send_chat`. L'écho local (fromRemote faux) et
  /// la réponse distante arrivent tous deux par `session_chat_stream` : on ne
  /// duplique donc rien localement.
  void _envoyerMessageChat() {
    final texte = _chatController.text.trim();
    if (texte.isEmpty) return;
    final id = _sessionId;
    if (id == null) {
      _informer('Discussion indisponible : session non démarrée.');
      return;
    }
    unawaited(_api.sendChat(id, texte));
    _chatController.clear();
  }

  void _surMessageChat(ChatMessageDto message) {
    if (!mounted) return;
    setState(() {
      // `fromRemote` vrai = message du pair (à gauche) ; faux = mon écho local.
      _messages
          .add(_MessageChat(texte: message.text, deMoi: !message.fromRemote));
    });
    _defilerChatEnBas();
  }

  void _defilerChatEnBas() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_chatScroll.hasClients) {
        unawaited(_chatScroll.animateTo(
          _chatScroll.position.maxScrollExtent,
          duration: const Duration(milliseconds: 200),
          curve: Curves.easeOut,
        ));
      }
    });
  }

  // ---------------------------------------------------------------------------
  // Transfert réel (canal `Files` du cœur via `send_files` / transfer stream)
  // ---------------------------------------------------------------------------

  /// Lance l'envoi. Faute de sélecteur natif (aucun plugin ici), on lit un
  /// simple champ de chemins (séparés par « ; ») ; à vide, on envoie les
  /// entrées de démonstration pour garder le parcours démontrable sous mock.
  void _envoyerFichiers() {
    final id = _sessionId;
    if (id == null) {
      _informer('Transfert indisponible : session non démarrée.');
      return;
    }
    final saisie = _cheminController.text.trim();
    final chemins = saisie.isEmpty
        ? _fichiersDemoATransferer
        : saisie
            .split(RegExp(r'[;\n]+'))
            .map((s) => s.trim())
            .where((s) => s.isNotEmpty)
            .toList();
    if (chemins.isEmpty) return;
    _cheminController.clear();
    unawaited(_api.sendFiles(id, chemins));
    _informer('Envoi de ${chemins.length} fichier(s) au poste distant…');
  }

  void _surEvenementTransfert(TransferEventDto e) {
    if (!mounted) return;
    setState(() {
      switch (e.kind) {
        case 'started':
          _transfertActif = true;
          _ftOuvert = true; // révèle le panneau si un transfert débute
          final index = e.fileIndex ?? _transferts.length;
          _transferts[index] = _TransfertFichier(
            index: index,
            nom: e.fileName ?? 'fichier',
            bytesTotal: e.bytesTotal ?? 0,
            bytesDone: e.bytesDone ?? 0,
          );
        case 'progress':
          _transfertActif = true;
          final index = e.fileIndex ?? 0;
          final f = _transferts.putIfAbsent(
            index,
            () => _TransfertFichier(
              index: index,
              nom: e.fileName ?? 'fichier',
              bytesTotal: e.bytesTotal ?? 0,
            ),
          );
          f.bytesDone = e.bytesDone ?? f.bytesDone;
          if (e.bytesTotal != null) f.bytesTotal = e.bytesTotal!;
          if (e.fileName != null) f.nom = e.fileName!;
          f.termine = false;
          _pourcentTransfert = e.percent ?? _pourcentTransfert;
          _debitTransfert = e.bytesPerSec ?? _debitTransfert;
          _etaTransfert = e.etaSecs ?? _etaTransfert;
        case 'completed':
          final f = _transferts[e.fileIndex ?? 0];
          if (f != null) {
            if (e.bytesTotal != null) f.bytesTotal = e.bytesTotal!;
            f.bytesDone = f.bytesTotal;
            f.termine = true;
          }
        case 'finished':
          _transfertActif = false;
          _pourcentTransfert = 100;
          _etaTransfert = 0;
          for (final f in _transferts.values) {
            f.termine = true;
            f.bytesDone = f.bytesTotal;
          }
        case 'cancelled':
          _transfertActif = false;
      }
    });
    if (e.kind == 'finished') {
      NovaToast.montrer(context, 'Transfert terminé.');
    }
  }

  // ---------------------------------------------------------------------------
  // Popovers sombres (un seul à la fois, ancré sous le bouton)
  // ---------------------------------------------------------------------------

  /// Ferme le popover courant. [rendreFocus] restitue le focus clavier à la
  /// surface distante (fermeture explicite : Échap, clic à côté, sélection).
  void _fermerPopover({bool rendreFocus = false}) {
    _popover?.remove();
    _popover = null;
    if (rendreFocus && mounted) _noeudFocus.requestFocus();
  }

  void _rafraichirPopover() => _popover?.markNeedsBuild();

  void _basculerPopover(BuildContext ancre, WidgetBuilder contenu,
      {double largeur = 236}) {
    if (_popover != null) {
      _fermerPopover(rendreFocus: true);
      return;
    }
    final box = ancre.findRenderObject() as RenderBox?;
    final overlayBox =
        Overlay.of(context).context.findRenderObject() as RenderBox?;
    if (box == null || overlayBox == null) return;
    final pos = box.localToGlobal(Offset.zero, ancestor: overlayBox);
    final largeurEcran = overlayBox.size.width;
    var gauche = pos.dx + box.size.width / 2 - largeur / 2;
    gauche = gauche.clamp(8.0, largeurEcran - largeur - 8);
    final haut = pos.dy + box.size.height + 4;
    _popover = OverlayEntry(
      builder: (ctx) => Stack(
        children: [
          Positioned.fill(
            child: GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTap: () => _fermerPopover(rendreFocus: true),
            ),
          ),
          Positioned(
            left: gauche,
            top: haut,
            width: largeur,
            // Échap referme le popover (le focus clavier lui est confié le
            // temps de son affichage, puis rendu à la surface).
            child: Focus(
              autofocus: true,
              onKeyEvent: (noeud, evenement) {
                if (evenement is KeyDownEvent &&
                    evenement.logicalKey == LogicalKeyboardKey.escape) {
                  _fermerPopover(rendreFocus: true);
                  return KeyEventResult.handled;
                }
                return KeyEventResult.ignored;
              },
              child: _CadrePopover(child: contenu(ctx)),
            ),
          ),
        ],
      ),
    );
    Overlay.of(context).insert(_popover!);
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    final stats = _stats;
    return Scaffold(
      body: NovaAppFrame(
        vue: NovaVue.session,
        libelleSession: widget.args.libellePair,
        // Latence réelle dans l'onglet de session (maquette `.stab2 .lt`).
        latenceSessionMs: stats == null || _etat != SessionStateDto.active
            ? null
            : (stats.rttUs / 1000).round(),
        masquerChrome: _pleinEcran,
        afficherRail: false,
        etatGauche: _etatSession(),
        corps: Row(
          children: [
            Expanded(
              child: Stack(
                children: [
                  Positioned.fill(child: _surfaceDistante()),
                  // Couche d'annotations — toujours peinte (les traits du pair
                  // restent visibles tableau blanc replié) mais transparente
                  // aux clics : le pointeur atteint la surface distante.
                  Positioned.fill(
                    child: IgnorePointer(
                      child: RepaintBoundary(
                        child: CustomPaint(
                          painter: _PeintreAnnotations(
                            trame: _trameCourante,
                            annotations: _annotations,
                            apercu: _apercuAnnotation,
                            revision: _revisionAnnotations,
                            mode: _modeVideo,
                          ),
                          size: Size.infinite,
                        ),
                      ),
                    ),
                  ),
                  // Couche de dessin du tableau blanc : capture les glissers
                  // (le temps du mode, les entrées ne vont plus au distant).
                  if (_wbOuvert) Positioned.fill(child: _coucheDessin()),
                  // Couche de sélection du cadre d'écran (glisser-rectangle).
                  if (_selectionCadre)
                    Positioned.fill(child: _coucheSelectionCadre()),
                  _sinfo(),
                  if (_permConfidentialite) _chipConfidentialite(),
                  if (_selectionCadre) _chipSelectionCadre(),
                  if (_wbOuvert)
                    Positioned(
                      top: 52,
                      left: 14,
                      bottom: 60,
                      child: Align(
                        alignment: Alignment.topLeft,
                        child: _barreWhiteboard(),
                      ),
                    ),
                  if (_ftOuvert)
                    Positioned(
                        left: 0, right: 0, bottom: 0, child: _transfert()),
                  if (_etat == SessionStateDto.reconnecting)
                    Positioned(
                        top: 0, left: 0, right: 0, child: _bandeauReconnexion()),
                  if (_montrerOverlayConnexion)
                    Positioned.fill(child: _overlayConnexion()),
                  Positioned(
                    top: 0,
                    left: 0,
                    right: 0,
                    child: Align(
                      alignment: Alignment.topCenter,
                      child: _barreOutils(),
                    ),
                  ),
                ],
              ),
            ),
            if (_chatOuvert) _panneauChat(),
          ],
        ),
      ),
    );
  }

  bool get _montrerOverlayConnexion {
    final etablissement = switch (_etat) {
      SessionStateDto.resolving ||
      SessionStateDto.connecting ||
      SessionStateDto.handshaking =>
        true,
      _ => false,
    };
    return etablissement && !_aRecuUneTrame;
  }

  /// Contenu session de la barre d'état basse, alimentée par `session_stats`.
  Widget _etatSession() {
    final t = NovaTokens.of(context);
    final peer = _statut?.peer;
    final stats = _stats;
    return Row(
      children: [
        Flexible(child: SessionStateBadge(etat: _etat, dense: true)),
        const SizedBox(width: 10),
        Expanded(
          child: Text(
            [
              if (peer != null) 'Pair : $peer',
              'Qualité : $_qualite',
              _libelleMoniteur,
              if (_regionActive != null)
                'Cadre : ${_regionActive!.largeur}×${_regionActive!.hauteur} px',
              if (_tunnels.isNotEmpty) 'Tunnels : ${_tunnels.length}',
              if (stats != null) '${stats.fps.toStringAsFixed(0)} IPS',
              if (stats != null) '${(stats.rttUs / 1000).toStringAsFixed(0)} ms',
              if (stats != null) '↓ ${_formaterOctets(stats.bytesIn)}',
              if (stats?.encoderBackend != null)
                'Encodeur : ${stats!.encoderBackend}',
              if (stats != null && stats.targetBitrateKbps > 0)
                'ABR N${stats.abrLevel} · ${_formaterDebit(stats.targetBitrateKbps)}',
              if (stats != null && stats.reconnects > 0)
                'Reconnexions : ${stats.reconnects}',
              if (stats != null && stats.inputsDenied > 0)
                'Entrées refusées : ${stats.inputsDenied}',
              'Entrées : $_evenementsEnvoyes',
            ].join(' · '),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(fontSize: 11, color: t.texte3),
          ),
        ),
      ],
    );
  }

  /// Libellé du moniteur diffusé : indexé sur la liste réelle quand l'hôte
  /// l'a annoncée, sinon convention de repli du sous-menu statique
  /// (index 2 = « tous les écrans »).
  String get _libelleMoniteur => _moniteurs.isEmpty && _moniteur == 2
      ? 'Tous les écrans'
      : 'Écran ${_moniteur + 1}';

  String _formaterOctets(int octets) {
    if (octets < 1024) return '$octets o';
    if (octets < 1024 * 1024) {
      return '${(octets / 1024).toStringAsFixed(0)} Ko';
    }
    if (octets < 1024 * 1024 * 1024) {
      return '${(octets / (1024 * 1024)).toStringAsFixed(1).replaceAll('.', ',')} Mo';
    }
    return '${(octets / (1024 * 1024 * 1024)).toStringAsFixed(1).replaceAll('.', ',')} Go';
  }

  String _formaterDebit(int kbps) {
    if (kbps >= 1000) {
      return '${(kbps / 1000).toStringAsFixed(1).replaceAll('.', ',')} Mb/s';
    }
    return '$kbps kb/s';
  }

  // ---------------------------------------------------------------------------
  // Surface distante (rendu vidéo pur Dart — inchangé)
  // ---------------------------------------------------------------------------

  Widget _surfaceDistante() {
    return LayoutBuilder(
      builder: (context, contraintes) {
        return Listener(
          onPointerHover: (e) => _surMouvement(e, contraintes),
          onPointerMove: (e) => _surMouvement(e, contraintes),
          onPointerDown: _surBoutonEnfonce,
          onPointerUp: _surBoutonRelache,
          onPointerSignal: _surMolette,
          child: Focus(
            focusNode: _noeudFocus,
            autofocus: true,
            onKeyEvent: _surTouche,
            child: MouseRegion(
              cursor: _sourisActive
                  ? SystemMouseCursors.basic
                  : SystemMouseCursors.forbidden,
              child: RepaintBoundary(
                child: Container(
                  color: const Color(0xFF0C0F14),
                  alignment: Alignment.center,
                  child: _contenuSurface(),
                ),
              ),
            ),
          ),
        );
      },
    );
  }

  /// Mode d'affichage effectif appliqué au peintre vidéo.
  _ModeAffichageVideo get _modeVideo => switch (_modeAffichage) {
        'Étirer' => _ModeAffichageVideo.etirer,
        'Réduire à la fenêtre' => _ModeAffichageVideo.adapter,
        _ => _ModeAffichageVideo.original,
      };

  Widget _contenuSurface() {
    final montrerVideo = _aRecuUneTrame && _etat != SessionStateDto.closed;
    if (!montrerVideo) return _apercuSimule();
    Widget surface = SizedBox.expand(
      child: RepaintBoundary(
        child: CustomPaint(
          painter: _PeintreVideo(_trameCourante, _modeVideo),
          size: Size.infinite,
        ),
      ),
    );
    // « Mode nuit (inverser) » : inversion des couleurs côté visionneuse.
    if (_modeNuit) {
      surface = ColorFiltered(
        colorFilter: const ColorFilter.matrix(<double>[
          -1, 0, 0, 0, 255, //
          0, -1, 0, 0, 255, //
          0, 0, -1, 0, 255, //
          0, 0, 0, 1, 0,
        ]),
        child: surface,
      );
    }
    return surface;
  }

  /// Aperçu du bureau distant tant qu'aucune trame n'est décodée : dégradé
  /// sombre + résumé de session (maquette `.scr`).
  Widget _apercuSimule() {
    final peer = _statut?.peer;
    return Container(
      decoration: const BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
          colors: [Color(0xFF1F2A3F), Color(0xFF0E1420)],
        ),
      ),
      child: Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIcone(
              _etat == SessionStateDto.closed
                  ? NovaIcones.lienCoupe
                  : NovaIcones.moniteur,
              taille: 44,
              couleur: const Color(0xFF828DA6),
            ),
            const SizedBox(height: 12),
            Text.rich(
              TextSpan(
                text: switch (_etat) {
                  SessionStateDto.active => 'Écran distant — ',
                  SessionStateDto.closed => 'Session terminée — ',
                  _ => 'Session ${_etat.label}… — ',
                },
                children: [
                  TextSpan(
                    text: peer == null
                        ? widget.args.libellePair
                        : '${widget.args.libellePair} · $peer',
                    style: const TextStyle(
                      color: Color(0xFFCBD4EC),
                      fontWeight: FontWeight.w600,
                    ),
                  ),
                ],
              ),
              style: const TextStyle(fontSize: 13, color: Color(0xFF828DA6)),
            ),
            const SizedBox(height: 5),
            Text(
              // Rien d'inventé : en session active on attend la première
              // trame ; sinon on rappelle la protection du canal.
              _etat == SessionStateDto.active
                  ? 'En attente de la première image…'
                  : 'Chiffrement TLS 1.3 + Noise_IK',
              style: TextStyle(
                fontSize: 11,
                color: const Color(0xFF828DA6).withValues(alpha: 0.7),
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// HUD bas-gauche : encodeur · débit · latence · IPS (maquette `.sinfo`).
  Widget _sinfo() {
    final stats = _stats;
    final texte = stats == null
        ? 'Établissement de la session…'
        : [
            stats.encoderBackend ?? 'Décodage',
            _formaterDebit(stats.targetBitrateKbps > 0
                ? stats.targetBitrateKbps
                : (stats.bytesIn ~/ 125).clamp(0, 99999).toInt()),
            '${(stats.rttUs / 1000).toStringAsFixed(0)} ms',
            '${stats.fps.toStringAsFixed(0)} IPS',
          ].join(' · ');
    return Positioned(
      left: 12,
      bottom: 12,
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
        decoration: BoxDecoration(
          color: const Color(0xFF0A0D12).withValues(alpha: 0.72),
          borderRadius: BorderRadius.circular(kNovaRayon),
          border: Border.all(color: Colors.white.withValues(alpha: 0.08)),
        ),
        child: Text(
          texte,
          style: const TextStyle(
            fontSize: 11,
            color: Color(0xFF9AA3B7),
            fontFeatures: [ui.FontFeature.tabularFigures()],
          ),
        ),
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Bandeau de reconnexion (maquette : bandeau haut)
  // ---------------------------------------------------------------------------

  Widget _bandeauReconnexion() {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
      color: kNovaAmbre.withValues(alpha: 0.94),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: const [
          SizedBox(
            width: 14,
            height: 14,
            child: CircularProgressIndicator(strokeWidth: 2, color: Colors.white),
          ),
          SizedBox(width: 10),
          Text(
            'Connexion perdue — reconnexion en cours…',
            style: TextStyle(
                fontSize: 12, color: Colors.white, fontWeight: FontWeight.w600),
          ),
        ],
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Overlay de connexion (Résolution → Connexion → Authentification)
  // ---------------------------------------------------------------------------

  int get _indexEtape => switch (_etat) {
        SessionStateDto.resolving => 0,
        SessionStateDto.connecting => 1,
        SessionStateDto.handshaking => 2,
        _ => 3,
      };

  Widget _overlayConnexion() {
    final peer = _statut?.peer ?? '';
    return Container(
      color: const Color(0xFF0A0D12).withValues(alpha: 0.72),
      alignment: Alignment.center,
      child: Container(
        width: 328,
        padding: const EdgeInsets.all(22),
        decoration: BoxDecoration(
          color: const Color(0xFF181B21),
          borderRadius: BorderRadius.circular(8),
          border: Border.all(color: const Color(0xFF343B45)),
          boxShadow: [
            BoxShadow(
              color: Colors.black.withValues(alpha: 0.42),
              blurRadius: 50,
              offset: const Offset(0, 20),
            ),
          ],
        ),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              widget.args.libellePair,
              style: const TextStyle(
                  fontSize: 14,
                  fontWeight: FontWeight.w600,
                  color: Color(0xFFE6E9ED)),
            ),
            const SizedBox(height: 2),
            Text(
              peer,
              style: const TextStyle(
                fontSize: 12,
                color: Color(0xFF69727C),
                fontFeatures: [ui.FontFeature.tabularFigures()],
              ),
            ),
            const SizedBox(height: 16),
            _etapeConnexion("Résolution de l'adresse", 0),
            _etapeConnexion('Connexion (NAT / relais)', 1),
            _etapeConnexion('Authentification chiffrée', 2),
            const SizedBox(height: 15),
            Center(
              child: GestureDetector(
                onTap: () => unawaited(_terminerSession()),
                child: MouseRegion(
                  cursor: SystemMouseCursors.click,
                  child: const Text(
                    'Annuler',
                    style: TextStyle(fontSize: 12.5, color: Color(0xFF9DA5AF)),
                  ),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _etapeConnexion(String libelle, int index) {
    final fait = _indexEtape > index;
    final courant = _indexEtape == index;
    final Color couleurTexte = fait
        ? const Color(0xFF9DA5AF)
        : courant
            ? const Color(0xFFE6E9ED)
            : const Color(0xFF69727C);
    Widget cercle;
    if (fait) {
      cercle = Container(
        width: 20,
        height: 20,
        decoration: const BoxDecoration(
          color: Color(0xFF3FB457),
          shape: BoxShape.circle,
        ),
        child: const NovaIcone(NovaIcones.coche, taille: 11, couleur: Colors.white),
      );
    } else if (courant) {
      cercle = const SizedBox(
        width: 20,
        height: 20,
        child: CircularProgressIndicator(
            strokeWidth: 2, color: Color(0xFF5B93F0)),
      );
    } else {
      cercle = Container(
        width: 20,
        height: 20,
        decoration: BoxDecoration(
          shape: BoxShape.circle,
          border: Border.all(color: const Color(0xFF343B45), width: 2),
        ),
      );
    }
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Row(
        children: [
          cercle,
          const SizedBox(width: 11),
          Expanded(
            child: Text(libelle,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(fontSize: 12.5, color: couleurTexte)),
          ),
        ],
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Barre d'outils flottante (boutons uniformes groupés)
  // ---------------------------------------------------------------------------

  Widget _barreOutils() {
    final actif = _etat == SessionStateDto.active;
    return Container(
      height: 40,
      decoration: BoxDecoration(
        color: _kToolFond,
        border: Border.all(color: _kToolBordure),
        borderRadius:
            const BorderRadius.vertical(bottom: Radius.circular(6)),
        boxShadow: [
          BoxShadow(
            color: Colors.black.withValues(alpha: 0.35),
            blurRadius: 14,
            offset: const Offset(0, 4),
          ),
        ],
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          _groupeIndicateurs(),
          _groupe([
            _boutonPopover(NovaIcones.info, 'Infos système', _contenuInfo),
            _boutonPopover(NovaIcones.cadenas, 'Permissions', _contenuPerm),
            _BoutonBarre(
              icone: _favori ? NovaIcones.etoilePleine : NovaIcones.etoile,
              infobulle: _favori ? 'Retirer des favoris' : 'Favori',
              couleurForcee: _favori ? kNovaAmbre : null,
              onTap: () => setState(() => _favori = !_favori),
            ),
          ]),
          _groupe([
            _boutonPopover(NovaIcones.moniteurs, 'Moniteurs', _contenuMoniteurs),
            _boutonPopover(
                NovaIcones.qualite, 'Qualité / mode', _contenuQualite),
            _boutonPopover(NovaIcones.affichage, 'Affichage', _contenuAffichage),
            _BoutonBarre(
              icone: _pleinEcran
                  ? NovaIcones.quitterPleinEcran
                  : NovaIcones.pleinEcran,
              infobulle: _pleinEcran ? 'Quitter le plein écran' : 'Plein écran',
              onTap: () => unawaited(_basculerPleinEcran()),
            ),
          ]),
          _groupe([
            _boutonPopover(NovaIcones.clavier, 'Clavier', _contenuClavier),
            _BoutonBarre(
              icone: NovaIcones.ctrlAltSuppr,
              infobulle: 'Ctrl+Alt+Suppr',
              onTap: actif ? () => unawaited(_envoyerCtrlAltSuppr()) : null,
            ),
            _BoutonBarre(
              icone: NovaIcones.pressePapiers,
              infobulle: 'Presse-papiers',
              onTap: () => _aVenir('Presse-papiers'),
            ),
          ]),
          _groupe([
            _BoutonBarre(
              icone: NovaIcones.dossierSync,
              infobulle: 'Transfert de fichiers',
              actif: _ftOuvert,
              onTap: _permissions.files
                  ? () => setState(() => _ftOuvert = !_ftOuvert)
                  : null,
            ),
            _BoutonBarre(
              icone: NovaIcones.discussion,
              infobulle: 'Discussion',
              actif: _chatOuvert,
              onTap: () => setState(() => _chatOuvert = !_chatOuvert),
            ),
            _BoutonBarre(
              icone: NovaIcones.tableauBlanc,
              infobulle: 'Tableau blanc',
              actif: _wbOuvert,
              onTap: () => setState(() => _wbOuvert = !_wbOuvert),
            ),
            _BoutonBarre(
              icone: NovaIcones.enregistrer,
              infobulle: _enregistre ? "Arrêter l'enregistrement" : 'Enregistrer',
              actif: _enregistre,
              pastilleRouge: _enregistre,
              onTap: () => unawaited(_basculerEnregistrement()),
            ),
          ]),
          _groupe([
            _boutonPopover(NovaIcones.troisPoints, 'Actions', _contenuActions),
            _BoutonBarre(
              icone: NovaIcones.alimentation,
              infobulle: 'Terminer',
              fermeture: true,
              onTap: () => unawaited(_terminerSession()),
            ),
          ], dernier: true),
        ],
      ),
    );
  }

  Widget _groupe(List<Widget> enfants, {bool dernier = false}) {
    return Container(
      decoration: BoxDecoration(
        border: dernier
            ? null
            : Border(right: BorderSide(color: _kToolBordure)),
      ),
      padding: const EdgeInsets.symmetric(horizontal: 2),
      child: Row(mainAxisSize: MainAxisSize.min, children: enfants),
    );
  }

  Widget _groupeIndicateurs() {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        _indicateur(NovaIcones.bouclierCoche, 'Chiffré', pastilleVerte: true),
        _indicateur(NovaIcones.image, null),
        _indicateur(NovaIcones.disque, null),
      ],
    );
  }

  /// Cellule indicatrice (maquette `.tind`) : filet séparateur à droite.
  Widget _indicateur(IconData icone, String? libelle,
      {bool pastilleVerte = false}) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 11),
      height: 40,
      decoration: const BoxDecoration(
        border: Border(right: BorderSide(color: _kToolBordure)),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (pastilleVerte) ...[
            Container(
              width: 7,
              height: 7,
              decoration: const BoxDecoration(
                  color: Color(0xFF3FB457), shape: BoxShape.circle),
            ),
            const SizedBox(width: 6),
          ],
          NovaIcone(icone, taille: 15, couleur: _kToolInd),
          if (libelle != null) ...[
            const SizedBox(width: 6),
            Text(libelle,
                style: const TextStyle(fontSize: 11, color: _kToolInd)),
          ],
        ],
      ),
    );
  }

  Widget _boutonPopover(IconData icone, String infobulle, WidgetBuilder contenu,
      {double largeur = 236}) {
    return Builder(
      builder: (ancre) => _BoutonBarre(
        icone: icone,
        infobulle: infobulle,
        onTap: () => _basculerPopover(ancre, contenu, largeur: largeur),
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Contenus des popovers
  // ---------------------------------------------------------------------------

  /// Panneau « Infos système » : nom d'hôte et OS **réels** du pair
  /// (`session_peer_info`) + décompte des moniteurs annoncés
  /// (`session_monitors`). Rien d'inventé : tant que l'annonce n'est pas
  /// arrivée, le panneau dit l'attente.
  Widget _contenuInfo(BuildContext context) {
    final infos = _infosPair;
    final moniteurs = _moniteurs;
    final MonitorInfoDto? principal = moniteurs.isEmpty
        ? null
        : moniteurs.firstWhere((m) => m.principal,
            orElse: () => moniteurs.first);
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Poste distant'),
        if (infos == null)
          const _PopItem(texte: "Infos système en attente de l'hôte…")
        else ...[
          _PopItem(icone: NovaIcones.moniteur, texte: infos.hote),
          _PopItem(texte: infos.os),
        ],
        if (principal != null)
          _PopItem(
            texte: moniteurs.length == 1
                ? '1 écran · ${principal.largeur}×${principal.hauteur}'
                : '${moniteurs.length} écrans · '
                    '${principal.largeur}×${principal.hauteur}',
          ),
        const _PopItem(texte: 'Session chiffrée TLS 1.3'),
      ],
    );
  }

  Widget _contenuPerm(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Permissions accordées'),
        _PopItem(
          icone: NovaIcones.clavier,
          texte: 'Contrôler clavier & souris',
          coche: _permClavierSouris,
          onTap: () => unawaited(_basculerPermissionAChaud(
            capacites: const ['clavier', 'souris'],
            libelle: 'Contrôle clavier & souris',
            lire: () => _permClavierSouris,
            ecrire: (v) => _permClavierSouris = v,
          )),
        ),
        _PopItem(
          icone: NovaIcones.pressePapiers,
          texte: 'Synchroniser le presse-papiers',
          coche: _permPressePapiers,
          onTap: () => unawaited(_basculerPermissionAChaud(
            capacites: const [
              'presse_papiers_lecture',
              'presse_papiers_ecriture',
            ],
            libelle: 'Synchronisation du presse-papiers',
            lire: () => _permPressePapiers,
            ecrire: (v) => _permPressePapiers = v,
          )),
        ),
        _PopItem(
          icone: NovaIcones.audio,
          texte: 'Entendre le son distant',
          coche: _permAudio,
          onTap: _basculerAudio,
        ),
        _PopItem(
          icone: NovaIcones.dossier,
          texte: 'Autoriser le transfert de fichiers',
          coche: _permTransfert,
          onTap: () => unawaited(_basculerPermissionAChaud(
            capacites: const ['fichiers_envoi', 'fichiers_reception'],
            libelle: 'Transfert de fichiers',
            lire: () => _permTransfert,
            ecrire: (v) => _permTransfert = v,
          )),
        ),
        _PopItem(
          icone: NovaIcones.bloquer,
          texte: 'Bloquer les entrées du distant',
          coche: _permBloquerEntree,
          onTap: () => _basculerPerm(() => _permBloquerEntree = !_permBloquerEntree),
        ),
        _PopItem(
          icone: NovaIcones.cadenas,
          texte: 'Verrouiller le compte à la fin',
          coche: _permVerrouiller,
          onTap: () => _basculerPerm(() => _permVerrouiller = !_permVerrouiller),
        ),
        _PopItem(
          icone: NovaIcones.confidentialite,
          texte: 'Mode confidentialité',
          coche: _permConfidentialite,
          onTap: () => unawaited(_basculerConfidentialite()),
        ),
      ],
    );
  }

  void _basculerPerm(VoidCallback modif) {
    setState(modif);
    _rafraichirPopover();
  }

  /// Renégocie **à chaud** une bascule du popover Permissions : bascule
  /// optimiste immédiate (la case répond au clic), envoi de chaque clé de
  /// [capacites] au cœur via `session_set_permission`, toast de confirmation ;
  /// si le cœur refuse, **retour en arrière** de la case (revert optimiste),
  /// message français du cœur en toast et réalignement best-effort des clés
  /// déjà appliquées.
  Future<void> _basculerPermissionAChaud({
    required List<String> capacites,
    required String libelle,
    required bool Function() lire,
    required void Function(bool) ecrire,
  }) async {
    final id = _sessionId;
    if (id == null) {
      _informer('Permissions : session non démarrée.');
      return;
    }
    final vise = !lire();
    setState(() => ecrire(vise));
    _rafraichirPopover();
    try {
      for (final capacite in capacites) {
        await _api.sessionSetPermission(id, capacite, vise);
      }
      if (!mounted) return;
      NovaToast.montrer(
        context,
        vise ? '$libelle — autorisé.' : '$libelle — retiré.',
      );
    } catch (e) {
      if (!mounted) return;
      setState(() => ecrire(!vise));
      _rafraichirPopover();
      _informer('Permissions : ${_messageErreur(e)}');
      // Réaligne (best-effort) le cœur sur l'état affiché : les clés déjà
      // appliquées avant l'échec sont reposées à leur valeur d'origine.
      unawaited(() async {
        for (final capacite in capacites) {
          try {
            await _api.sessionSetPermission(id, capacite, !vise);
          } catch (_) {
            // Réalignement best-effort : l'erreur est déjà affichée.
          }
        }
      }());
    }
  }

  /// Bascule l'audio de la session : met à jour l'UI puis pilote le cœur via
  /// `set_audio_enabled` (sans effet si la permission audio n'est pas accordée).
  void _basculerAudio() {
    setState(() => _permAudio = !_permAudio);
    _rafraichirPopover();
    final id = _sessionId;
    if (id != null) {
      unawaited(_api.setAudioEnabled(id, _permAudio));
    }
  }

  /// Bascule le **mode confidentialité** (rideau noir côté hôte) : bascule
  /// optimiste immédiate, puis réconciliation sur l'état réel relu via
  /// `privacy_active` — la coche et la pastille disent la vérité du cœur.
  Future<void> _basculerConfidentialite() async {
    final id = _sessionId;
    if (id == null) {
      _informer('Mode confidentialité : session non démarrée.');
      return;
    }
    final vise = !_permConfidentialite;
    setState(() => _permConfidentialite = vise);
    _rafraichirPopover();
    try {
      await _api.setPrivacy(id, vise);
      final reel = await _api.privacyActive(id);
      if (!mounted) return;
      setState(() => _permConfidentialite = reel);
      _rafraichirPopover();
      NovaToast.montrer(
        context,
        reel
            ? "Mode confidentialité activé — l'écran de l'hôte est masqué."
            : 'Mode confidentialité levé.',
      );
    } catch (e) {
      if (!mounted) return;
      setState(() => _permConfidentialite = !vise);
      _rafraichirPopover();
      _informer('Mode confidentialité : ${_messageErreur(e)}');
    }
  }

  /// Relit les états réels des capacités étendues (confidentialité, cadre) au
  /// passage en session active : l'UI reflète le cœur, jamais l'inverse.
  Future<void> _synchroniserCapacites() async {
    final id = _sessionId;
    if (id == null) return;
    try {
      final rideau = await _api.privacyActive(id);
      final region = await _api.sessionRequestedRegion(id);
      if (!mounted) return;
      setState(() {
        _permConfidentialite = rideau;
        _regionActive = region;
      });
    } catch (_) {
      // Capacités étendues indisponibles : les états locaux font foi.
    }
  }

  Widget _contenuActions(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Actions'),
        _PopItem(
            icone: NovaIcones.bouclier,
            texte: "Demander l'élévation (UAC)",
            onTap: () {
              _fermerPopover(rendreFocus: true);
              _aVenir("Demande d'élévation (UAC)");
            }),
        _PopItem(
            icone: NovaIcones.changerCote,
            texte: 'Changer de côté',
            onTap: () {
              _fermerPopover(rendreFocus: true);
              _aVenir('Changement de côté');
            }),
        _PopItem(
            icone: NovaIcones.capture,
            texte: "Capture d'écran",
            onTap: () {
              _fermerPopover(rendreFocus: true);
              _aVenir("Capture d'écran");
            }),
        _PopItem(
            icone: NovaIcones.redemarrer,
            texte: 'Redémarrer le poste distant',
            onTap: () {
              _fermerPopover(rendreFocus: true);
              _aVenir('Redémarrage du poste distant');
            }),
        _PopItem(
            icone: NovaIcones.terminal,
            texte: 'Configurer un tunnel TCP',
            onTap: () => unawaited(_configurerTunnel())),
      ],
    );
  }

  /// Préréglages du cœur (`session_set_quality` : `auto`, `fluide`,
  /// `equilibre`, `netteté`) par libellé du sélecteur de qualité.
  static const Map<String, String> _presetQualiteParLibelle = {
    'Automatique': 'auto',
    'Meilleure qualité': 'netteté',
    'Équilibré': 'equilibre',
    'Meilleures performances': 'fluide',
  };

  Widget _contenuQualite(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Mode de travail'),
        for (final m in const ['Efficacité', 'Vidéo', 'Jeux (capture souris)'])
          _PopItem(
            texte: m,
            selectionne: _modeTravail == m,
            onTap: () => _choisir(() => _modeTravail = m),
          ),
        const _PopHeader('Qualité'),
        for (final q in _presetQualiteParLibelle.keys)
          _PopItem(
            texte: q,
            selectionne: _qualite == q,
            onTap: () => unawaited(_choisirQualite(q)),
          ),
      ],
    );
  }

  /// Applique un **préréglage de qualité** : le choix est reflété tout de
  /// suite (sélecteur + barre d'état), puis `session_set_quality` reconfigure
  /// l'encodeur hôte (profil ABR + plafond de débit). Si le cœur refuse,
  /// retour au choix précédent et message français en toast.
  Future<void> _choisirQualite(String libelle) async {
    final precedent = _qualite;
    setState(() => _qualite = libelle);
    _fermerPopover(rendreFocus: true);
    final id = _sessionId;
    final preset = _presetQualiteParLibelle[libelle];
    if (id == null || preset == null) return;
    try {
      await _api.sessionSetQuality(id, preset);
      if (!mounted) return;
      NovaToast.montrer(context, 'Qualité « $libelle » appliquée.');
    } catch (e) {
      if (!mounted) return;
      setState(() => _qualite = precedent);
      _informer('Qualité : ${_messageErreur(e)}');
    }
  }

  Widget _contenuAffichage(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader("Mode d'affichage"),
        for (final m in const ['Original', 'Réduire à la fenêtre', 'Étirer'])
          _PopItem(
            texte: m,
            selectionne: _modeAffichage == m,
            onTap: () => _choisir(() => _modeAffichage = m),
          ),
        const _PopHeader('Options'),
        _PopItem(
            texte: 'Auto-adapter la résolution',
            coche: _autoResolution,
            onTap: () => _basculerPerm(() => _autoResolution = !_autoResolution)),
        _PopItem(
            texte: 'Mode nuit (inverser)',
            coche: _modeNuit,
            onTap: () => _basculerPerm(() => _modeNuit = !_modeNuit)),
        _PopItem(
            texte: 'Suivre le curseur',
            coche: _suivreCurseur,
            onTap: () => _basculerPerm(() => _suivreCurseur = !_suivreCurseur)),
        _PopItem(
            texte: 'Afficher le curseur distant',
            coche: _curseurDistant,
            onTap: () => _basculerPerm(() => _curseurDistant = !_curseurDistant)),
        _PopItem(
            icone: NovaIcones.cadre,
            texte: "Cadre d'écran",
            selectionne: _regionActive != null,
            onTap: _demarrerSelectionCadre),
        if (_regionActive != null)
          _PopItem(
              icone: NovaIcones.agrandirCadre,
              texte: 'Plein écran (lever le cadre)',
              onTap: () => unawaited(_annulerCadre())),
      ],
    );
  }

  /// Sous-menu écrans : la liste **réelle** annoncée par l'hôte
  /// (`session_monitors`) quand elle est arrivée — chaque entrée porte l'index
  /// attendu par `switch_monitor` — sinon le repli statique historique.
  Widget _contenuMoniteurs(BuildContext context) {
    final reels = _moniteurs;
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Moniteurs distants'),
        if (reels.isEmpty) ...[
          // Repli : l'hôte n'a pas (encore) annoncé ses écrans — entrées
          // statiques, la bascule reste appliquée au mieux par le cœur.
          _PopItem(
            icone: NovaIcones.moniteur,
            texte: 'Écran 1 (principal)',
            selectionne: _moniteur == 0,
            onTap: () => _choisirMoniteur(0),
          ),
          _PopItem(
            icone: NovaIcones.moniteur,
            texte: 'Écran 2',
            selectionne: _moniteur == 1,
            onTap: () => _choisirMoniteur(1),
          ),
          _PopItem(
            icone: NovaIcones.tousEcrans,
            texte: 'Afficher tous les écrans',
            selectionne: _moniteur == 2,
            onTap: () => _choisirMoniteur(2),
          ),
        ] else
          for (final m in reels)
            _PopItem(
              icone: NovaIcones.moniteur,
              texte: 'Écran ${m.index + 1}'
                  '${m.principal ? ' (principal)' : ''}'
                  ' · ${m.largeur}×${m.hauteur}',
              selectionne: _moniteur == m.index,
              onTap: () => _choisirMoniteur(m.index),
            ),
      ],
    );
  }

  Widget _contenuClavier(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Clavier'),
        _PopItem(
          texte: 'Transmettre les raccourcis',
          coche: _transmettreRaccourcis,
          onTap: () =>
              _basculerPerm(() => _transmettreRaccourcis = !_transmettreRaccourcis),
        ),
        _PopItem(
          texte: 'Mode universel (Unicode)',
          selectionne: _modeClavier == 'Universel',
          onTap: () => _choisir(() => _modeClavier = 'Universel'),
        ),
        _PopItem(
          texte: 'Mode national (scancodes)',
          selectionne: _modeClavier == 'National',
          onTap: () => _choisir(() => _modeClavier = 'National'),
        ),
        _PopItem(
            icone: NovaIcones.clavier,
            texte: 'Clavier virtuel',
            onTap: () {
              _fermerPopover(rendreFocus: true);
              _aVenir('Clavier virtuel');
            }),
      ],
    );
  }

  void _choisir(VoidCallback modif) {
    setState(modif);
    _fermerPopover(rendreFocus: true);
  }

  /// Sélection d'un moniteur distant : met à jour l'UI puis demande la bascule
  /// multi-écran au cœur via `switch_monitor` (index 2 = « tous les écrans »,
  /// appliqué au mieux par l'hôte).
  void _choisirMoniteur(int moniteur) {
    setState(() => _moniteur = moniteur);
    _fermerPopover(rendreFocus: true);
    final id = _sessionId;
    if (id != null) {
      unawaited(_api.switchMonitor(id, moniteur));
    }
  }

  // ---------------------------------------------------------------------------
  // Tableau blanc / annotations réelles (`send_annotation` + flux du pair)
  // ---------------------------------------------------------------------------

  /// Annotation reçue du pair : peinte telle quelle, sauf s'il s'agit de
  /// l'écho d'un de nos propres envois (mock) déjà affiché localement.
  void _surAnnotationRecue(AnnotationDto annotation) {
    if (!mounted) return;
    final echo = _echosAttendus.indexOf(annotation);
    if (echo >= 0) {
      _echosAttendus.removeAt(echo);
      return;
    }
    _annotations.add(annotation);
    _revisionAnnotations.value++;
  }

  /// Projette une position locale de la surface dans les coordonnées
  /// normalisées `0..1` de l'image distante affichée (letterbox déduit).
  Offset _normaliserPoint(Offset local, Size taille) {
    final rect = _rectAffichageVideo(taille, _trameCourante.value, _modeVideo);
    if (rect.width <= 0 || rect.height <= 0) return Offset.zero;
    return Offset(
      math.min(1.0, math.max(0.0, (local.dx - rect.left) / rect.width)),
      math.min(1.0, math.max(0.0, (local.dy - rect.top) / rect.height)),
    );
  }

  void _surDebutTrait(Offset local, Size taille) {
    final p = _normaliserPoint(local, taille);
    _formeDepart = p;
    _pointsTraitEnCours
      ..clear()
      ..add(p.dx)
      ..add(p.dy);
    _majApercuAnnotation(p);
  }

  void _surPointTrait(Offset local, Size taille) {
    if (_formeDepart == null) return;
    final p = _normaliserPoint(local, taille);
    if (_outilAnnotation == _kAnnotationLibre) {
      final dx = p.dx - _pointsTraitEnCours[_pointsTraitEnCours.length - 2];
      final dy = p.dy - _pointsTraitEnCours[_pointsTraitEnCours.length - 1];
      // Lissage : on n'ajoute un sommet qu'au-delà de ~0,2 % de la surface.
      if (dx * dx + dy * dy < 0.000004) return;
      _pointsTraitEnCours
        ..add(p.dx)
        ..add(p.dy);
    }
    _majApercuAnnotation(p);
  }

  /// Reconstruit l'aperçu du trait en cours selon l'outil : polyligne (libre),
  /// coins opposés (rectangle, flèche) ou centre + demi-axes (ellipse) — la
  /// forme plate attendue par [AnnotationDto].
  void _majApercuAnnotation(Offset courant) {
    final depart = _formeDepart;
    if (depart == null) return;
    final Float32List points;
    switch (_outilAnnotation) {
      case _kAnnotationRectangle:
      case _kAnnotationFleche:
        points = Float32List.fromList(
            [depart.dx, depart.dy, courant.dx, courant.dy]);
      case _kAnnotationEllipse:
        points = Float32List.fromList([
          (depart.dx + courant.dx) / 2,
          (depart.dy + courant.dy) / 2,
          (courant.dx - depart.dx).abs() / 2,
          (courant.dy - depart.dy).abs() / 2,
        ]);
      default:
        points = Float32List.fromList(_pointsTraitEnCours);
    }
    _apercuAnnotation.value = AnnotationDto(
      genre: _outilAnnotation,
      points: points,
      couleurArgb: _couleurAnnotation,
      epaisseur: _epaisseurAnnotation,
    );
  }

  /// Fin du geste : le trait devient définitif localement puis part au pair
  /// via `send_annotation` (les formes dégénérées d'un simple clic sont
  /// ignorées, sauf le point du trait libre).
  void _surFinTrait() {
    final annotation = _apercuAnnotation.value;
    _apercuAnnotation.value = null;
    _pointsTraitEnCours.clear();
    _formeDepart = null;
    if (annotation == null) return;
    if (annotation.genre != _kAnnotationLibre) {
      final p = annotation.points;
      final degenere = annotation.genre == _kAnnotationEllipse
          ? p[2] < 0.001 && p[3] < 0.001
          : (p[0] - p[2]).abs() < 0.002 && (p[1] - p[3]).abs() < 0.002;
      if (degenere) return;
    }
    _annotations.add(annotation);
    _echosAttendus.add(annotation);
    _revisionAnnotations.value++;
    unawaited(_envoyerAnnotation(annotation));
  }

  Future<void> _envoyerAnnotation(AnnotationDto annotation) async {
    final id = _sessionId;
    if (id == null) return;
    try {
      await _api.sendAnnotation(id, annotation);
    } catch (e) {
      if (mounted) _informer('Annotation non transmise : ${_messageErreur(e)}');
    }
  }

  /// Efface la couche d'annotations côté visionneuse (le pont n'expose pas
  /// d'effacement distant : chaque côté gère sa couche).
  void _effacerAnnotations() {
    if (_annotations.isEmpty && _apercuAnnotation.value == null) return;
    _annotations.clear();
    _echosAttendus.clear();
    _pointsTraitEnCours.clear();
    _formeDepart = null;
    _apercuAnnotation.value = null;
    _revisionAnnotations.value++;
    _informer('Annotations effacées.');
  }

  /// Couche de capture du dessin (au-dessus de la vidéo, sous la barre
  /// d'outils) : chaque glisser trace un trait, envoyé au pair au relâcher.
  Widget _coucheDessin() {
    return LayoutBuilder(
      builder: (context, contraintes) {
        final taille = contraintes.biggest;
        return MouseRegion(
          cursor: SystemMouseCursors.precise,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onPanStart: (d) => _surDebutTrait(d.localPosition, taille),
            onPanUpdate: (d) => _surPointTrait(d.localPosition, taille),
            onPanEnd: (_) => _surFinTrait(),
            onPanCancel: _surFinTrait,
            child: const SizedBox.expand(),
          ),
        );
      },
    );
  }

  // ---------------------------------------------------------------------------
  // Barre du tableau blanc (maquette `.wbtb`) : outils, couleur, épaisseur
  // ---------------------------------------------------------------------------

  Widget _barreWhiteboard() {
    const outils = [
      (NovaIcones.crayonOutil, _kAnnotationLibre, 'Trait libre'),
      (NovaIcones.carre, _kAnnotationRectangle, 'Rectangle'),
      (NovaIcones.cercle, _kAnnotationEllipse, 'Ellipse'),
      (NovaIcones.flecheDiagonale, _kAnnotationFleche, 'Flèche'),
    ];
    return Container(
      padding: const EdgeInsets.all(4),
      decoration: BoxDecoration(
        color: _kPopFond,
        borderRadius: BorderRadius.circular(6),
        border: Border.all(color: _kPopBordure),
      ),
      child: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            for (final (icone, genre, libelle) in outils)
              _BoutonWhiteboard(
                icone: icone,
                infobulle: libelle,
                actif: _outilAnnotation == genre,
                onTap: () => setState(() => _outilAnnotation = genre),
              ),
            _separateurWhiteboard(),
            for (final couleur in _kPaletteAnnotation)
              _BoutonWhiteboard(
                infobulle: 'Couleur du trait',
                actif: _couleurAnnotation == couleur,
                onTap: () => setState(() => _couleurAnnotation = couleur),
                enfant: Container(
                  width: 14,
                  height: 14,
                  decoration: BoxDecoration(
                    color: Color(couleur),
                    shape: BoxShape.circle,
                    border:
                        Border.all(color: Colors.white.withValues(alpha: 0.25)),
                  ),
                ),
              ),
            _separateurWhiteboard(),
            for (final epaisseur in _kEpaisseursAnnotation)
              _BoutonWhiteboard(
                infobulle: 'Épaisseur ${epaisseur.toStringAsFixed(0)}',
                actif: _epaisseurAnnotation == epaisseur,
                onTap: () => setState(() => _epaisseurAnnotation = epaisseur),
                enfant: Container(
                  width: 6 + epaisseur * 1.2,
                  height: 6 + epaisseur * 1.2,
                  decoration: const BoxDecoration(
                      color: _kToolIcone, shape: BoxShape.circle),
                ),
              ),
            _separateurWhiteboard(),
            _BoutonWhiteboard(
              icone: NovaIcones.gomme,
              infobulle: 'Tout effacer',
              onTap: _effacerAnnotations,
            ),
          ],
        ),
      ),
    );
  }

  Widget _separateurWhiteboard() => Container(
        width: 24,
        height: 1,
        margin: const EdgeInsets.symmetric(vertical: 4),
        color: _kPopBordure,
      );

  // ---------------------------------------------------------------------------
  // Cadre d'écran (région partagée) — `set_session_region`
  // ---------------------------------------------------------------------------

  /// Entre en mode sélection : le prochain glisser sur la surface délimite la
  /// zone d'écran demandée à l'hôte (Échap annule).
  void _demarrerSelectionCadre() {
    _fermerPopover(rendreFocus: true);
    if (_sessionId == null) {
      _informer("Cadre d'écran : session non démarrée.");
      return;
    }
    if (_trameCourante.value == null) {
      _informer("Cadre d'écran : attendez la première image distante.");
      return;
    }
    setState(() {
      _selectionCadre = true;
      _cadreDepart = null;
      _cadreEnCours = null;
    });
  }

  Widget _coucheSelectionCadre() {
    return LayoutBuilder(
      builder: (context, contraintes) {
        final taille = contraintes.biggest;
        return MouseRegion(
          cursor: SystemMouseCursors.precise,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onPanStart: (d) => setState(() {
              _cadreDepart = d.localPosition;
              _cadreEnCours =
                  Rect.fromPoints(d.localPosition, d.localPosition);
            }),
            onPanUpdate: (d) {
              final depart = _cadreDepart;
              if (depart == null) return;
              setState(
                  () => _cadreEnCours = Rect.fromPoints(depart, d.localPosition));
            },
            onPanEnd: (_) => unawaited(_validerCadre(taille)),
            onPanCancel: () => setState(() {
              _cadreDepart = null;
              _cadreEnCours = null;
            }),
            child: CustomPaint(
              painter: _PeintreSelectionCadre(
                rect: _cadreEnCours,
                libelle: _libelleCadre(taille),
              ),
              size: Size.infinite,
            ),
          ),
        );
      },
    );
  }

  /// Dimensions vivantes de la sélection, exprimées en pixels distants.
  String? _libelleCadre(Size taille) {
    final rect = _cadreEnCours;
    if (rect == null) return null;
    final region = _regionDepuisRect(rect, taille);
    return region == null ? null : '${region.largeur} × ${region.hauteur} px';
  }

  /// Convertit un rectangle local (pixels de la surface) en [RegionDto] dans
  /// les **pixels de l'écran distant**, borné à l'image courante ; `null` si
  /// la sélection est trop petite ou hors de l'image.
  RegionDto? _regionDepuisRect(Rect local, Size taille) {
    final image = _trameCourante.value;
    if (image == null) return null;
    final rectVideo = _rectAffichageVideo(taille, image, _modeVideo);
    if (rectVideo.width <= 0 || rectVideo.height <= 0) return null;
    final zone = local.intersect(rectVideo);
    if (zone.width < 4 || zone.height < 4) return null;
    int borner(int v, int minimum, int maximum) =>
        v < minimum ? minimum : (v > maximum ? maximum : v);
    final sx = image.width / rectVideo.width;
    final sy = image.height / rectVideo.height;
    final x =
        borner(((zone.left - rectVideo.left) * sx).round(), 0, image.width - 1);
    final y =
        borner(((zone.top - rectVideo.top) * sy).round(), 0, image.height - 1);
    final largeur = borner((zone.width * sx).round(), 1, image.width - x);
    final hauteur = borner((zone.height * sy).round(), 1, image.height - y);
    return RegionDto(x: x, y: y, largeur: largeur, hauteur: hauteur);
  }

  /// Applique la sélection : `set_session_region` puis relecture du cadre
  /// effectif (`session_requested_region`) pour l'afficher tel que demandé.
  Future<void> _validerCadre(Size taille) async {
    final rect = _cadreEnCours;
    _cadreDepart = null;
    if (rect == null) return;
    final region = _regionDepuisRect(rect, taille);
    if (region == null) {
      setState(() => _cadreEnCours = null);
      _informer('Sélection trop petite — cadre inchangé (Échap pour quitter).');
      return;
    }
    final id = _sessionId;
    if (id == null) {
      setState(() {
        _selectionCadre = false;
        _cadreEnCours = null;
      });
      return;
    }
    try {
      await _api.setSessionRegion(id, region);
      final effectif = await _api.sessionRequestedRegion(id);
      if (!mounted) return;
      setState(() {
        _regionActive = effectif;
        _selectionCadre = false;
        _cadreEnCours = null;
      });
      NovaToast.montrer(
        context,
        effectif == null
            ? 'Cadre appliqué.'
            : 'Cadre appliqué : ${effectif.largeur}×${effectif.hauteur} px '
                'à (${effectif.x}, ${effectif.y}).',
      );
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _selectionCadre = false;
        _cadreEnCours = null;
      });
      _informer("Cadre d'écran : ${_messageErreur(e)}");
    }
  }

  /// Rétablit le plein écran (`set_session_region(null)`).
  Future<void> _annulerCadre() async {
    _fermerPopover(rendreFocus: true);
    final id = _sessionId;
    if (id == null) return;
    try {
      await _api.setSessionRegion(id, null);
      final effectif = await _api.sessionRequestedRegion(id);
      if (!mounted) return;
      setState(() => _regionActive = effectif);
      NovaToast.montrer(context, 'Plein écran rétabli — cadre levé.');
    } catch (e) {
      if (mounted) _informer("Cadre d'écran : ${_messageErreur(e)}");
    }
  }

  // ---------------------------------------------------------------------------
  // Tunnel TCP de session — `open_tunnel` / `close_tunnels`
  // ---------------------------------------------------------------------------

  /// Ouvre le dialogue « Tunnel TCP » (formulaire + liste des tunnels).
  Future<void> _configurerTunnel() async {
    _fermerPopover();
    final id = _sessionId;
    if (id == null) {
      _informer('Tunnel TCP : session non démarrée.');
      return;
    }
    await montrerDialogueNova<void>(
      context: context,
      builder: (_) => _DialogueTunnel(
        initiaux: List.of(_tunnels),
        onOuvrir: _ouvrirTunnel,
        onFermerTout: _fermerTousTunnels,
      ),
    );
    if (mounted) _noeudFocus.requestFocus();
  }

  /// Ouvre un tunnel via `open_tunnel` (`portLocal` 0 = port éphémère) et
  /// l'ajoute au reflet local ; `null` en cas d'échec (erreur en toast).
  Future<_TunnelActif?> _ouvrirTunnel(
      int portLocal, String hote, int portDistant) async {
    final id = _sessionId;
    if (id == null) {
      _informer('Tunnel TCP : session non démarrée.');
      return null;
    }
    final cible = '$hote:$portDistant';
    try {
      final ouvert = await _api.openTunnel(id, portLocal, cible);
      final tunnel =
          _TunnelActif(adresseLocale: ouvert.adresseLocale, cible: cible);
      if (!mounted) return tunnel;
      setState(() => _tunnels.add(tunnel));
      NovaToast.montrer(
          context, 'Tunnel ouvert : ${ouvert.adresseLocale} → $cible');
      return tunnel;
    } catch (e) {
      if (mounted) _informer('Tunnel TCP : ${_messageErreur(e)}');
      return null;
    }
  }

  /// Ferme **tous** les tunnels de la session (`close_tunnels` est global côté
  /// cœur) ; vrai si la fermeture a abouti.
  Future<bool> _fermerTousTunnels() async {
    final id = _sessionId;
    if (id == null) return false;
    try {
      await _api.closeTunnels(id);
      if (!mounted) return true;
      setState(() => _tunnels.clear());
      NovaToast.montrer(context, 'Tous les tunnels de la session sont fermés.');
      return true;
    } catch (e) {
      if (mounted) _informer('Tunnel TCP : ${_messageErreur(e)}');
      return false;
    }
  }

  // ---------------------------------------------------------------------------
  // Pastilles flottantes (HUD) : confidentialité, mode cadre
  // ---------------------------------------------------------------------------

  /// Pastille HUD commune : fond sombre translucide, filet doux (comme `.sinfo`).
  Widget _chipHud({
    required IconData icone,
    required Color couleurIcone,
    required String texte,
  }) {
    return IgnorePointer(
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
        decoration: BoxDecoration(
          color: const Color(0xFF0A0D12).withValues(alpha: 0.72),
          borderRadius: BorderRadius.circular(kNovaRayon),
          border: Border.all(color: Colors.white.withValues(alpha: 0.08)),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIcone(icone, taille: 13, couleur: couleurIcone),
            const SizedBox(width: 7),
            Flexible(
              child: Text(
                texte,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(fontSize: 11, color: Color(0xFFD5DAE3)),
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// Indicateur « écran masqué » : visible tant que `privacy_active` est vrai.
  Widget _chipConfidentialite() => Positioned(
        top: 48,
        right: 12,
        child: _chipHud(
          icone: NovaIcones.oeilBarre,
          couleurIcone: kNovaAmbre,
          texte: 'Écran distant masqué (confidentialité)',
        ),
      );

  /// Consigne du mode « cadre d'écran », centrée sous la barre d'outils.
  Widget _chipSelectionCadre() => Positioned(
        top: 48,
        left: 12,
        right: 12,
        child: Center(
          child: _chipHud(
            icone: NovaIcones.cadre,
            couleurIcone: _kCadreAccent,
            texte: "Tracez la zone d'écran à partager — Échap pour annuler",
          ),
        ),
      );

  // ---------------------------------------------------------------------------
  // Transfert de fichiers (maquette `.ft`)
  // ---------------------------------------------------------------------------

  Widget _transfert() {
    final t = NovaTokens.of(context);
    return Container(
      height: 300,
      decoration: BoxDecoration(
        color: t.fenetre,
        border: Border(top: BorderSide(color: t.filetFort)),
      ),
      child: Column(
        children: [
          Container(
            height: 38,
            padding: const EdgeInsets.symmetric(horizontal: 12),
            decoration: BoxDecoration(
              border: Border(bottom: BorderSide(color: t.filet)),
            ),
            child: Row(
              children: [
                NovaIcone(NovaIcones.dossierSync, taille: 15, couleur: t.texte2),
                const SizedBox(width: 8),
                Text('Transfert de fichiers',
                    style: TextStyle(
                        fontSize: 12.5,
                        fontWeight: FontWeight.w600,
                        color: t.texte)),
                const Spacer(),
                NovaBoutonAction(
                  icone: NovaIcones.fermer,
                  tailleIcone: 14,
                  taille: 26,
                  onTap: () => setState(() => _ftOuvert = false),
                ),
              ],
            ),
          ),
          Expanded(
            child: Row(
              children: [
                Expanded(
                  child: _volet(t, NovaIcones.moniteur, 'Ce poste — Documents', [
                    (NovaIcones.dossier, 'Projets', '—'),
                    (NovaIcones.fichierTexte, 'rapport-Q2.pdf', '1,2 Mo'),
                  ]),
                ),
                Container(width: 1, color: t.filet),
                Expanded(
                  child: _volet(t, NovaIcones.serveur, 'poste-bureau — Bureau', [
                    (NovaIcones.dossier, 'Livraison', '—'),
                    (NovaIcones.fichierArchive, 'build-9.7.3.zip', '58 Mo'),
                  ]),
                ),
              ],
            ),
          ),
          _zoneEnvoi(t),
          if (_transferts.isNotEmpty) _fileTransfert(t),
        ],
      ),
    );
  }

  /// Zone d'envoi : champ de chemin (pas de sélecteur natif) + bouton
  /// « Envoyer » qui appelle `send_files`.
  Widget _zoneEnvoi(NovaTokens t) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      decoration: BoxDecoration(
        border: Border(top: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          NovaIcone(NovaIcones.exporter, taille: 15, couleur: t.texte3),
          const SizedBox(width: 8),
          Expanded(
            child: SizedBox(
              height: 32,
              child: TextField(
                controller: _cheminController,
                style: const TextStyle(fontSize: 12.5),
                decoration: const InputDecoration(
                  hintText: 'Chemin(s) local(aux) à envoyer, séparés par « ; »…',
                ),
                onSubmitted: (_) => _envoyerFichiers(),
              ),
            ),
          ),
          const SizedBox(width: 8),
          NovaBoutonPrimaire(
            libelle: 'Envoyer',
            onPressed: _permTransfert ? _envoyerFichiers : null,
          ),
        ],
      ),
    );
  }

  Widget _volet(NovaTokens t, IconData icone, String titre,
      List<(IconData, String, String)> fichiers) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Container(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 7),
          decoration: BoxDecoration(
            border: Border(bottom: BorderSide(color: t.filet)),
          ),
          child: Row(
            children: [
              NovaIcone(icone, taille: 13, couleur: t.texte3),
              const SizedBox(width: 7),
              Text(titre.toUpperCase(),
                  style: TextStyle(
                      fontSize: 11, letterSpacing: 0.4, color: t.texte3)),
            ],
          ),
        ),
        Expanded(
          child: ListView(
            padding: EdgeInsets.zero,
            children: [
              for (final (ic, nom, taille) in fichiers)
                _ElementFichier(icone: ic, nom: nom, taille: taille),
            ],
          ),
        ),
      ],
    );
  }

  /// File de progression réelle, alimentée par `session_transfer_stream` :
  /// résumé de session (%, débit, ETA) + une ligne par fichier.
  Widget _fileTransfert(NovaTokens t) {
    final items = _transferts.values.toList()
      ..sort((a, b) => a.index.compareTo(b.index));
    return Container(
      constraints: const BoxConstraints(maxHeight: 108),
      decoration: BoxDecoration(
        border: Border(top: BorderSide(color: t.filet)),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 8, 12, 4),
            child: Row(
              children: [
                NovaIcone(NovaIcones.dossierSync, taille: 13, couleur: t.texte3),
                const SizedBox(width: 8),
                Text(
                  _transfertActif
                      ? 'Transfert · ${_pourcentTransfert.toStringAsFixed(0)} %'
                      : 'File de transfert',
                  style: TextStyle(fontSize: 11.5, color: t.texte2),
                ),
                const Spacer(),
                if (_transfertActif && _debitTransfert > 0)
                  Text(
                    '${_formaterOctets(_debitTransfert.round())}/s · '
                    '${_formaterDuree(_etaTransfert)}',
                    style: TextStyle(
                      fontSize: 11,
                      color: t.texte3,
                      fontFeatures: const [ui.FontFeature.tabularFigures()],
                    ),
                  ),
              ],
            ),
          ),
          Flexible(
            child: ListView(
              padding: const EdgeInsets.only(bottom: 6),
              shrinkWrap: true,
              children: [for (final f in items) _ligneTransfert(t, f)],
            ),
          ),
        ],
      ),
    );
  }

  Widget _ligneTransfert(NovaTokens t, _TransfertFichier f) {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 5),
      child: Row(
        children: [
          NovaIcone(
            f.termine ? NovaIcones.coche : NovaIcones.telecharger,
            taille: 14,
            couleur: f.termine ? t.vert : t.texte2,
          ),
          const SizedBox(width: 9),
          SizedBox(
            width: 116,
            child: Text(
              f.nom,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(fontSize: 11.5, color: t.texte),
            ),
          ),
          const SizedBox(width: 10),
          Expanded(
            child: ClipRRect(
              borderRadius: BorderRadius.circular(2),
              child: LinearProgressIndicator(
                value: f.fraction,
                minHeight: 4,
                backgroundColor: t.filetFort,
                color: f.termine ? t.vert : kNovaRouge,
              ),
            ),
          ),
          const SizedBox(width: 10),
          Text(
            f.termine ? 'Terminé' : '${(f.fraction * 100).toStringAsFixed(0)} %',
            style: TextStyle(
              fontSize: 11,
              color: t.texte2,
              fontFeatures: const [ui.FontFeature.tabularFigures()],
            ),
          ),
        ],
      ),
    );
  }

  /// Durée lisible pour l'ETA (« 12 s », « 1 min 05 s »).
  String _formaterDuree(double secondes) {
    if (secondes <= 0) return '0 s';
    if (secondes < 60) return '${secondes.toStringAsFixed(0)} s';
    final m = secondes ~/ 60;
    final s = (secondes % 60).round();
    return '$m min ${s.toString().padLeft(2, '0')} s';
  }

  // ---------------------------------------------------------------------------
  // Panneau de discussion (maquette `.chat`)
  // ---------------------------------------------------------------------------

  Widget _panneauChat() {
    final t = NovaTokens.of(context);
    return Container(
      width: 274,
      decoration: BoxDecoration(
        color: t.fenetre,
        border: Border(left: BorderSide(color: t.filetFort)),
      ),
      child: Column(
        children: [
          Container(
            padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 11),
            decoration: BoxDecoration(
              border: Border(bottom: BorderSide(color: t.filet)),
            ),
            child: Row(
              children: [
                NovaIcone(NovaIcones.discussion, taille: 16, couleur: t.texte2),
                const SizedBox(width: 8),
                Text('Discussion',
                    style: TextStyle(
                        fontSize: 12.5,
                        fontWeight: FontWeight.w600,
                        color: t.texte)),
                const Spacer(),
                NovaBoutonAction(
                  icone: NovaIcones.fermer,
                  tailleIcone: 14,
                  taille: 26,
                  onTap: () => setState(() => _chatOuvert = false),
                ),
              ],
            ),
          ),
          Expanded(
            child: ListView.builder(
              controller: _chatScroll,
              padding: const EdgeInsets.all(12),
              itemCount: _messages.length,
              itemBuilder: (context, i) {
                final m = _messages[i];
                return Align(
                  alignment:
                      m.deMoi ? Alignment.centerRight : Alignment.centerLeft,
                  child: Container(
                    margin: const EdgeInsets.symmetric(vertical: 4),
                    padding:
                        const EdgeInsets.symmetric(horizontal: 10, vertical: 7),
                    constraints: const BoxConstraints(maxWidth: 210),
                    decoration: BoxDecoration(
                      color: m.deMoi ? kNovaRouge : t.panneau2,
                      borderRadius: BorderRadius.circular(8),
                    ),
                    child: Text(
                      m.texte,
                      style: TextStyle(
                        fontSize: 12,
                        height: 1.4,
                        color: m.deMoi ? Colors.white : t.texte,
                      ),
                    ),
                  ),
                );
              },
            ),
          ),
          Container(
            padding: const EdgeInsets.all(10),
            decoration: BoxDecoration(
              border: Border(top: BorderSide(color: t.filet)),
            ),
            child: Row(
              children: [
                Expanded(
                  child: SizedBox(
                    height: 32,
                    child: TextField(
                      controller: _chatController,
                      style: const TextStyle(fontSize: 12.5),
                      decoration: const InputDecoration(hintText: 'Message…'),
                      onSubmitted: (_) => _envoyerMessageChat(),
                    ),
                  ),
                ),
                const SizedBox(width: 8),
                NovaBoutonPrimaire(
                  libelle: 'Envoyer',
                  onPressed: _envoyerMessageChat,
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

// ===========================================================================
// Boutons et sous-composants de session
// ===========================================================================

/// Bouton uniforme de la barre d'outils (maquette `.tbtn`) : 38×40, survol
/// blanc translucide, « Terminer » vire au rouge, pastille rouge « REC ».
class _BoutonBarre extends StatefulWidget {
  const _BoutonBarre({
    required this.icone,
    required this.infobulle,
    this.onTap,
    this.fermeture = false,
    this.actif = false,
    this.pastilleRouge = false,
    this.couleurForcee,
  });

  final IconData icone;
  final String infobulle;
  final VoidCallback? onTap;
  final bool fermeture;
  final bool actif;
  final bool pastilleRouge;
  final Color? couleurForcee;

  @override
  State<_BoutonBarre> createState() => _BoutonBarreState();
}

class _BoutonBarreState extends State<_BoutonBarre> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final desactive = widget.onTap == null;
    final Color fond = widget.fermeture && _survole
        ? kNovaRouge
        : (_survole || widget.actif) && !desactive
            ? _kToolHover
            : Colors.transparent;
    final Color couleur = widget.couleurForcee ??
        (desactive
            ? _kToolIcone.withValues(alpha: 0.35)
            : widget.fermeture && !_survole
                ? _kEndIcone
                : _survole || widget.actif
                    ? Colors.white
                    : _kToolIcone);

    return Tooltip(
      message: widget.infobulle,
      child: MouseRegion(
        cursor: desactive ? MouseCursor.defer : SystemMouseCursors.click,
        onEnter: (_) => setState(() => _survole = true),
        onExit: (_) => setState(() => _survole = false),
        child: GestureDetector(
          onTap: widget.onTap,
          child: Container(
            width: 38,
            height: 40,
            alignment: Alignment.center,
            color: fond,
            child: Stack(
              alignment: Alignment.center,
              clipBehavior: Clip.none,
              children: [
                NovaIcone(widget.icone, taille: 18, couleur: couleur),
                if (widget.pastilleRouge)
                  Positioned(
                    top: 6,
                    right: 6,
                    child: Container(
                      width: 7,
                      height: 7,
                      decoration: const BoxDecoration(
                          color: kNovaRouge, shape: BoxShape.circle),
                    ),
                  ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

/// Bouton du mini-tableau blanc (maquette `.wbtb .tbtn`) : 32×32. [enfant]
/// remplace l'icône (pastilles de couleur / d'épaisseur) ; [actif] marque
/// l'outil, la couleur ou l'épaisseur sélectionnés.
class _BoutonWhiteboard extends StatefulWidget {
  const _BoutonWhiteboard({
    this.icone,
    this.enfant,
    required this.onTap,
    this.actif = false,
    this.infobulle,
  }) : assert(icone != null || enfant != null,
            'Icône ou contenu personnalisé requis');

  final IconData? icone;
  final Widget? enfant;
  final VoidCallback onTap;
  final bool actif;
  final String? infobulle;

  @override
  State<_BoutonWhiteboard> createState() => _BoutonWhiteboardState();
}

class _BoutonWhiteboardState extends State<_BoutonWhiteboard> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    Widget bouton = MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        child: Container(
          width: 32,
          height: 32,
          margin: const EdgeInsets.symmetric(vertical: 1),
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: _survole || widget.actif ? _kPopHover : Colors.transparent,
            borderRadius: BorderRadius.circular(kNovaRayon),
            border: widget.actif ? Border.all(color: _kPopBordure) : null,
          ),
          child: widget.enfant ??
              NovaIcone(
                widget.icone!,
                taille: 16,
                couleur:
                    _survole || widget.actif ? Colors.white : _kToolIcone,
              ),
        ),
      ),
    );
    final infobulle = widget.infobulle;
    if (infobulle != null) {
      bouton = Tooltip(message: infobulle, child: bouton);
    }
    return bouton;
  }
}

/// Cadre sombre d'un popover (maquette `.pop`).
class _CadrePopover extends StatelessWidget {
  const _CadrePopover({required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Material(
      color: Colors.transparent,
      child: Container(
        padding: const EdgeInsets.all(5),
        decoration: BoxDecoration(
          color: _kPopFond,
          borderRadius: BorderRadius.circular(6),
          border: Border.all(color: _kPopBordure),
          boxShadow: [
            BoxShadow(
              color: Colors.black.withValues(alpha: 0.5),
              blurRadius: 28,
              offset: const Offset(0, 12),
            ),
          ],
        ),
        child: child,
      ),
    );
  }
}

/// En-tête de section d'un popover (maquette `.pop h6`).
class _PopHeader extends StatelessWidget {
  const _PopHeader(this.texte);

  final String texte;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(10, 7, 10, 4),
      child: Text(
        texte.toUpperCase(),
        style: const TextStyle(
          fontSize: 9.5,
          letterSpacing: 0.6,
          fontWeight: FontWeight.w600,
          color: _kPopTexte2,
        ),
      ),
    );
  }
}

/// Entrée d'un popover (maquette `.pit`) : icône optionnelle, texte, et
/// trailing case à cocher (`coche`) ou coche de sélection (`selectionne`).
class _PopItem extends StatefulWidget {
  const _PopItem({
    this.icone,
    required this.texte,
    this.coche,
    this.selectionne = false,
    this.onTap,
  });

  final IconData? icone;
  final String texte;

  /// Case à cocher (style `.ck`) : `null` = pas de case ; `true`/`false` = état.
  final bool? coche;

  /// Coche de sélection (style `.rc`) : élément d'un choix unique.
  final bool selectionne;

  final VoidCallback? onTap;

  @override
  State<_PopItem> createState() => _PopItemState();
}

class _PopItemState extends State<_PopItem> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    return MouseRegion(
      cursor:
          widget.onTap == null ? MouseCursor.defer : SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
          decoration: BoxDecoration(
            color: _survole || widget.selectionne
                ? _kPopHover
                : Colors.transparent,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            children: [
              if (widget.icone != null) ...[
                NovaIcone(widget.icone!, taille: 15, couleur: _kPopIcone),
                const SizedBox(width: 10),
              ],
              Expanded(
                child: Text(
                  widget.texte,
                  style: const TextStyle(fontSize: 12.5, color: _kPopTexte),
                ),
              ),
              if (widget.coche != null)
                Container(
                  width: 16,
                  height: 16,
                  alignment: Alignment.center,
                  decoration: BoxDecoration(
                    color: widget.coche! ? const Color(0xFF3FB457) : _kCkOff,
                    borderRadius: BorderRadius.circular(3),
                  ),
                  child: widget.coche!
                      ? const NovaIcone(NovaIcones.coche,
                          taille: 11, couleur: Colors.white)
                      : null,
                )
              else if (widget.selectionne)
                const NovaIcone(NovaIcones.coche,
                    taille: 15, couleur: Color(0xFF3FB457)),
            ],
          ),
        ),
      ),
    );
  }
}

/// Élément de fichier dans un volet de transfert (maquette `.fitem`).
class _ElementFichier extends StatefulWidget {
  const _ElementFichier(
      {required this.icone, required this.nom, required this.taille});

  final IconData icone;
  final String nom;
  final String taille;

  @override
  State<_ElementFichier> createState() => _ElementFichierState();
}

class _ElementFichierState extends State<_ElementFichier> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: Container(
        color: _survole ? t.panneau : Colors.transparent,
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
        child: Row(
          children: [
            NovaIcone(widget.icone, taille: 15, couleur: t.texte3),
            const SizedBox(width: 9),
            Expanded(
              child: Text(widget.nom,
                  style: TextStyle(fontSize: 12.5, color: t.texte)),
            ),
            Text(widget.taille,
                style: TextStyle(fontSize: 11, color: t.texte3)),
          ],
        ),
      ),
    );
  }
}

/// Mode d'affichage de la surface distante (popover « Mode d'affichage »).
enum _ModeAffichageVideo {
  /// Taille réelle (1:1), réduite seulement si l'écran distant dépasse la
  /// fenêtre — jamais agrandie.
  original,

  /// Adaptée à la fenêtre en conservant le ratio (letterbox).
  adapter,

  /// Étirée pour remplir la fenêtre (le ratio n'est pas conservé).
  etirer,
}

/// Rectangle d'affichage effectif de la trame [image] dans une surface de
/// [taille] donnée pour le [mode] choisi — partagé par le peintre vidéo, la
/// couche d'annotations et la sélection du cadre, afin que projections et
/// conversions restent alignées au pixel près. Sans image : toute la surface.
Rect _rectAffichageVideo(
    Size taille, ui.Image? image, _ModeAffichageVideo mode) {
  if (image == null || taille.isEmpty) return Offset.zero & taille;
  final double iw = image.width.toDouble();
  final double ih = image.height.toDouble();
  if (iw <= 0 || ih <= 0) return Offset.zero & taille;
  final double dw;
  final double dh;
  switch (mode) {
    case _ModeAffichageVideo.etirer:
      dw = taille.width;
      dh = taille.height;
    case _ModeAffichageVideo.adapter:
      final echelle = math.min(taille.width / iw, taille.height / ih);
      dw = iw * echelle;
      dh = ih * echelle;
    case _ModeAffichageVideo.original:
      // 1:1, plafonné à la fenêtre (pas de panoramique : on réduit si
      // l'écran distant est plus grand que la surface).
      final echelle =
          math.min(1.0, math.min(taille.width / iw, taille.height / ih));
      dw = iw * echelle;
      dh = ih * echelle;
  }
  return Rect.fromLTWH(
      (taille.width - dw) / 2, (taille.height - dh) / 2, dw, dh);
}

/// Peintre de la surface vidéo : dessine la trame `ui.Image` courante selon le
/// mode d'affichage choisi, **sans aucun plugin natif**.
class _PeintreVideo extends CustomPainter {
  _PeintreVideo(this.trame, this.mode) : super(repaint: trame);

  final ValueListenable<ui.Image?> trame;
  final _ModeAffichageVideo mode;

  static final Paint _peinture = Paint()
    ..filterQuality = FilterQuality.medium
    ..isAntiAlias = false;

  @override
  void paint(Canvas canvas, Size size) {
    final image = trame.value;
    if (image == null || size.isEmpty) return;
    final double iw = image.width.toDouble();
    final double ih = image.height.toDouble();
    if (iw <= 0 || ih <= 0) return;
    final destination = _rectAffichageVideo(size, image, mode);
    if (destination.isEmpty) return;
    canvas.drawImageRect(
      image,
      Rect.fromLTWH(0, 0, iw, ih),
      destination,
      _peinture,
    );
  }

  @override
  bool shouldRepaint(covariant _PeintreVideo old) =>
      old.trame != trame || old.mode != mode;
}

/// Peintre de la couche d'annotations : traits posés (les miens + ceux du
/// pair) et aperçu du trait en cours, exprimés en coordonnées normalisées de
/// l'image distante puis projetés sur le rectangle vidéo réellement affiché.
/// Repeint via [revision] (mutations de la liste), [apercu] (geste vivant) et
/// [trame] (letterbox recalculé à chaque nouvelle définition d'image).
class _PeintreAnnotations extends CustomPainter {
  _PeintreAnnotations({
    required this.trame,
    required this.annotations,
    required this.apercu,
    required ValueListenable<int> revision,
    required this.mode,
  }) : super(repaint: Listenable.merge([trame, apercu, revision]));

  final ValueListenable<ui.Image?> trame;
  final List<AnnotationDto> annotations;
  final ValueListenable<AnnotationDto?> apercu;
  final _ModeAffichageVideo mode;

  @override
  void paint(Canvas canvas, Size size) {
    if (size.isEmpty) return;
    final rect = _rectAffichageVideo(size, trame.value, mode);
    if (rect.width <= 0 || rect.height <= 0) return;
    for (final annotation in annotations) {
      _peindreAnnotation(canvas, rect, annotation);
    }
    final enCours = apercu.value;
    if (enCours != null) _peindreAnnotation(canvas, rect, enCours);
  }

  void _peindreAnnotation(Canvas canvas, Rect rect, AnnotationDto a) {
    final points = a.points;
    if (points.length < 2) return;
    Offset projeter(int i) => Offset(
          rect.left + points[i * 2] * rect.width,
          rect.top + points[i * 2 + 1] * rect.height,
        );
    final couleur = Color(a.couleurArgb);
    final trait = Paint()
      ..color = couleur
      ..style = PaintingStyle.stroke
      ..strokeWidth = math.max(1.0, a.epaisseur)
      ..strokeCap = StrokeCap.round
      ..strokeJoin = StrokeJoin.round
      ..isAntiAlias = true;
    switch (a.genre) {
      case _kAnnotationLibre:
        final n = points.length ~/ 2;
        if (n == 1) {
          // Simple clic : un point plein.
          canvas.drawCircle(
            projeter(0),
            math.max(1.5, a.epaisseur / 2 + 0.5),
            Paint()
              ..color = couleur
              ..isAntiAlias = true,
          );
          return;
        }
        final premier = projeter(0);
        final chemin = Path()..moveTo(premier.dx, premier.dy);
        for (var i = 1; i < n; i++) {
          final p = projeter(i);
          chemin.lineTo(p.dx, p.dy);
        }
        canvas.drawPath(chemin, trait);
      case _kAnnotationRectangle:
        if (points.length < 4) return;
        canvas.drawRect(Rect.fromPoints(projeter(0), projeter(1)), trait);
      case _kAnnotationEllipse:
        if (points.length < 4) return;
        // Centre puis demi-axes (les demi-axes sont des longueurs : ils se
        // projettent sans décalage d'origine).
        final centre = projeter(0);
        final rx = points[2] * rect.width;
        final ry = points[3] * rect.height;
        canvas.drawOval(
          Rect.fromCenter(center: centre, width: rx * 2, height: ry * 2),
          trait,
        );
      case _kAnnotationFleche:
        if (points.length < 4) return;
        final origine = projeter(0);
        final pointe = projeter(1);
        canvas.drawLine(origine, pointe, trait);
        final direction = pointe - origine;
        if (direction.distance < 0.5) return;
        final angle = math.atan2(direction.dy, direction.dx);
        final longueur = math.max(9.0, a.epaisseur * 3);
        for (final ecart in const [0.48, -0.48]) {
          canvas.drawLine(
            pointe,
            pointe -
                Offset(math.cos(angle + ecart), math.sin(angle + ecart)) *
                    longueur,
            trait,
          );
        }
      case _kAnnotationTexte:
        final texte = a.texte;
        if (texte == null || texte.isEmpty) return;
        final position = projeter(0);
        final peintreTexte = TextPainter(
          text: TextSpan(
            text: texte,
            style: TextStyle(
              color: couleur,
              fontSize: math.max(10.0, a.epaisseur),
              fontWeight: FontWeight.w600,
              shadows: const [Shadow(color: Colors.black54, blurRadius: 3)],
            ),
          ),
          textDirection: TextDirection.ltr,
        )..layout(maxWidth: math.max(40.0, rect.right - position.dx));
        peintreTexte.paint(canvas, position);
        peintreTexte.dispose();
    }
  }

  @override
  bool shouldRepaint(covariant _PeintreAnnotations old) =>
      old.trame != trame ||
      old.annotations != annotations ||
      old.apercu != apercu ||
      old.mode != mode;
}

/// Peintre du mode « cadre d'écran » : voile sombre, rectangle élastique,
/// poignées d'angle et dimensions (en pixels distants) pendant le glisser.
class _PeintreSelectionCadre extends CustomPainter {
  _PeintreSelectionCadre({required this.rect, required this.libelle});

  final Rect? rect;
  final String? libelle;

  @override
  void paint(Canvas canvas, Size size) {
    if (size.isEmpty) return;
    final voile = Paint()..color = const Color(0x66060A10);
    final zone = rect?.intersect(Offset.zero & size);
    if (zone == null || zone.isEmpty) {
      canvas.drawRect(Offset.zero & size, voile);
      return;
    }
    // Voile en quatre bandes : la zone choisie reste parfaitement lisible.
    canvas.drawRect(Rect.fromLTRB(0, 0, size.width, zone.top), voile);
    canvas.drawRect(
        Rect.fromLTRB(0, zone.bottom, size.width, size.height), voile);
    canvas.drawRect(Rect.fromLTRB(0, zone.top, zone.left, zone.bottom), voile);
    canvas.drawRect(
        Rect.fromLTRB(zone.right, zone.top, size.width, zone.bottom), voile);
    canvas.drawRect(
      zone,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1.5
        ..color = _kCadreAccent,
    );
    final poignee = Paint()..color = _kCadreAccent;
    for (final coin in [
      zone.topLeft,
      zone.topRight,
      zone.bottomLeft,
      zone.bottomRight,
    ]) {
      canvas.drawRect(
          Rect.fromCenter(center: coin, width: 6, height: 6), poignee);
    }
    final texte = libelle;
    if (texte == null || zone.width < 48) return;
    final peintreTexte = TextPainter(
      text: TextSpan(
        text: texte,
        style: const TextStyle(
          fontSize: 11,
          color: Colors.white,
          fontWeight: FontWeight.w600,
          fontFeatures: [ui.FontFeature.tabularFigures()],
        ),
      ),
      textDirection: TextDirection.ltr,
    )..layout();
    final position = Offset(
      math.max(8.0, zone.right - peintreTexte.width - 8),
      zone.bottom + peintreTexte.height + 12 > size.height
          ? zone.bottom - peintreTexte.height - 8
          : zone.bottom + 6,
    );
    final fondTexte = Rect.fromLTWH(
      position.dx - 5,
      position.dy - 3,
      peintreTexte.width + 10,
      peintreTexte.height + 6,
    );
    canvas.drawRRect(
      RRect.fromRectAndRadius(fondTexte, const Radius.circular(4)),
      Paint()..color = const Color(0xCC10151D),
    );
    peintreTexte.paint(canvas, position);
    peintreTexte.dispose();
  }

  @override
  bool shouldRepaint(covariant _PeintreSelectionCadre old) =>
      old.rect != rect || old.libelle != libelle;
}

/// Message du panneau de discussion.
class _MessageChat {
  const _MessageChat({required this.texte, required this.deMoi});

  final String texte;
  final bool deMoi;
}

/// Fichier suivi dans la file de transfert (alimenté par `session_transfer_stream`).
class _TransfertFichier {
  _TransfertFichier({
    required this.index,
    required this.nom,
    required this.bytesTotal,
    this.bytesDone = 0,
  });

  final int index;
  String nom;
  int bytesTotal;
  int bytesDone;
  bool termine = false;

  /// Fraction accomplie du fichier dans `[0, 1]`.
  double get fraction => bytesTotal > 0
      ? (bytesDone / bytesTotal).clamp(0.0, 1.0)
      : (termine ? 1.0 : 0.0);
}

/// Tunnel TCP ouvert pendant la session (reflet local d'`open_tunnel`).
class _TunnelActif {
  const _TunnelActif({required this.adresseLocale, required this.cible});

  /// Adresse réellement écoutée sur ce poste (« 127.0.0.1:port »).
  final String adresseLocale;

  /// Cible atteinte depuis le poste distant (« hôte:port »).
  final String cible;
}

/// Dialogue « Tunnel TCP » : formulaire port local → hôte:port distant, liste
/// des tunnels ouverts et fermeture globale (`close_tunnels` ferme tous les
/// tunnels de la session côté cœur). Les appels réels sont délégués à l'écran
/// via [onOuvrir] / [onFermerTout], qui y tiennent aussi le reflet partagé.
class _DialogueTunnel extends StatefulWidget {
  const _DialogueTunnel({
    required this.initiaux,
    required this.onOuvrir,
    required this.onFermerTout,
  });

  /// Tunnels déjà ouverts à l'ouverture du dialogue.
  final List<_TunnelActif> initiaux;

  /// Ouvre un tunnel (`portLocal` 0 = automatique) ; `null` si l'appel échoue.
  final Future<_TunnelActif?> Function(
      int portLocal, String hote, int portDistant) onOuvrir;

  /// Ferme tous les tunnels de la session ; vrai si la fermeture a abouti.
  final Future<bool> Function() onFermerTout;

  @override
  State<_DialogueTunnel> createState() => _DialogueTunnelState();
}

class _DialogueTunnelState extends State<_DialogueTunnel> {
  late final List<_TunnelActif> _tunnels = List.of(widget.initiaux);
  final TextEditingController _portLocalController = TextEditingController();
  final TextEditingController _hoteController = TextEditingController();
  final TextEditingController _portDistantController = TextEditingController();
  bool _enCours = false;

  @override
  void dispose() {
    _portLocalController.dispose();
    _hoteController.dispose();
    _portDistantController.dispose();
    super.dispose();
  }

  Future<void> _ouvrir() async {
    final brutLocal = _portLocalController.text.trim();
    final portLocal = brutLocal.isEmpty ? 0 : int.tryParse(brutLocal);
    final hote = _hoteController.text.trim();
    final portDistant = int.tryParse(_portDistantController.text.trim());
    if (portLocal == null || portLocal > 65535) {
      NovaToast.montrer(
          context, 'Port local invalide (0 à 65535, 0 = automatique).',
          info: true);
      return;
    }
    if (hote.isEmpty || hote.contains(' ') || hote.contains(':')) {
      NovaToast.montrer(context, 'Hôte distant invalide (IP ou nom, sans « : »).',
          info: true);
      return;
    }
    if (portDistant == null || portDistant < 1 || portDistant > 65535) {
      NovaToast.montrer(context, 'Port distant invalide (1 à 65535).',
          info: true);
      return;
    }
    setState(() => _enCours = true);
    final ouvert = await widget.onOuvrir(portLocal, hote, portDistant);
    if (!mounted) return;
    setState(() {
      _enCours = false;
      if (ouvert != null) {
        _tunnels.add(ouvert);
        _portLocalController.clear();
        _portDistantController.clear();
      }
    });
  }

  Future<void> _toutFermer() async {
    setState(() => _enCours = true);
    final ferme = await widget.onFermerTout();
    if (!mounted) return;
    setState(() {
      _enCours = false;
      if (ferme) _tunnels.clear();
    });
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return AlertDialog(
      title: const Text('Tunnel TCP'),
      content: SizedBox(
        width: 400,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(
              'Relaye un port local de ce poste vers un hôte joignable depuis '
              'le poste distant, à travers la session chiffrée '
              '(port local 0 = attribué automatiquement).',
              style: TextStyle(fontSize: 12, height: 1.45, color: t.texte2),
            ),
            const SizedBox(height: 14),
            Row(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                Expanded(
                  flex: 5,
                  child: TextField(
                    controller: _portLocalController,
                    autofocus: true,
                    keyboardType: TextInputType.number,
                    inputFormatters: [FilteringTextInputFormatter.digitsOnly],
                    style: const TextStyle(fontSize: 13),
                    decoration: const InputDecoration(
                        labelText: 'Port local', hintText: '0 = auto'),
                  ),
                ),
                Padding(
                  padding: const EdgeInsets.only(left: 8, right: 8, bottom: 10),
                  child: NovaIcone(NovaIcones.flecheDroite,
                      taille: 13, couleur: t.texte3),
                ),
                Expanded(
                  flex: 8,
                  child: TextField(
                    controller: _hoteController,
                    style: const TextStyle(fontSize: 13),
                    decoration: const InputDecoration(
                        labelText: 'Hôte distant',
                        hintText: 'ex. 192.168.1.10'),
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  flex: 5,
                  child: TextField(
                    controller: _portDistantController,
                    keyboardType: TextInputType.number,
                    inputFormatters: [FilteringTextInputFormatter.digitsOnly],
                    style: const TextStyle(fontSize: 13),
                    decoration: const InputDecoration(
                        labelText: 'Port distant', hintText: 'ex. 3389'),
                    onSubmitted: (_) => unawaited(_ouvrir()),
                  ),
                ),
              ],
            ),
            if (_tunnels.isNotEmpty) ...[
              const SizedBox(height: 16),
              Text(
                'TUNNELS OUVERTS (${_tunnels.length})',
                style: TextStyle(
                  fontSize: 10.5,
                  letterSpacing: 0.5,
                  fontWeight: FontWeight.w600,
                  color: t.texte3,
                ),
              ),
              const SizedBox(height: 4),
              ConstrainedBox(
                constraints: const BoxConstraints(maxHeight: 132),
                child: ListView.builder(
                  shrinkWrap: true,
                  itemCount: _tunnels.length,
                  itemBuilder: (context, i) {
                    final tunnel = _tunnels[i];
                    return Padding(
                      padding: const EdgeInsets.symmetric(vertical: 4),
                      child: Row(
                        children: [
                          Container(
                            width: 7,
                            height: 7,
                            decoration: BoxDecoration(
                                color: t.vert, shape: BoxShape.circle),
                          ),
                          const SizedBox(width: 9),
                          Expanded(
                            child: Text(
                              '${tunnel.adresseLocale} → ${tunnel.cible}',
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(
                                fontSize: 12,
                                color: t.texte,
                                fontFeatures: const [
                                  ui.FontFeature.tabularFigures()
                                ],
                              ),
                            ),
                          ),
                        ],
                      ),
                    );
                  },
                ),
              ),
            ],
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Fermer'),
        ),
        if (_tunnels.isNotEmpty)
          TextButton(
            onPressed: _enCours ? null : () => unawaited(_toutFermer()),
            child: const Text('Tout fermer'),
          ),
        FilledButton(
          onPressed: _enCours ? null : () => unawaited(_ouvrir()),
          child: Text(_enCours ? 'Ouverture…' : 'Ouvrir le tunnel'),
        ),
      ],
    );
  }
}
