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
library;

import 'dart:async';
import 'dart:io' show Platform;
import 'dart:math' as math;
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

  /// Identifiant de la session ouverte par le cœur (`start_session`).
  int? _sessionId;

  StreamSubscription<SessionStateDto>? _abonnementEtat;
  StreamSubscription<VideoFrameDto>? _abonnementVideo;
  Timer? _minuterieStats;

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
  String _modeAffichage = 'Original';
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

  // Permissions commutables en cours de session (habillage : l'application
  // réelle passera par le canal contrôle du cœur, lot 04).
  late bool _permAudio = _permissions.audio;
  late bool _permClavierSouris =
      _permissions.keyboard && !_permissions.viewOnly;
  late bool _permPressePapiers = _permissions.clipboard;
  late bool _permTransfert = _permissions.files;
  bool _permBloquerEntree = false;
  bool _permVerrouiller = false;
  bool _permConfidentialite = false;

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
    _minuterieStats?.cancel();
    unawaited(_arreterMoteur());
    _trameCourante.value?.dispose();
    _trameCourante.dispose();
    if (_pleinEcran && _estDesktop) {
      unawaited(windowManager.setFullScreen(false));
    }
    _chatController.dispose();
    _noeudFocus.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Cycle de vie réel de la session (piloté par le cœur Rust via NativeApi)
  // ---------------------------------------------------------------------------

  Future<void> _demarrerSession() async {
    try {
      final options = widget.args.options;
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
    } catch (e) {
      _signalerErreurFatale(_messageErreur(e));
    }
  }

  Future<void> _surEtat(SessionStateDto etat) async {
    await _appliquerEtat(etat);
    if (!mounted) return;
    if (etat == SessionStateDto.active) {
      _demarrerStats();
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
    try {
      final stats = await _api.sessionStats(id);
      if (!mounted) return;
      setState(() => _stats = stats);
    } catch (_) {
      // Statistiques indisponibles : le HUD conserve la dernière valeur.
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

  void _informer(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(content: Text(message)),
    );
  }

  void _aVenir(String fonction) => _informer('$fonction — à venir (lot 04).');

  void _envoyerMessageChat() {
    final texte = _chatController.text.trim();
    if (texte.isEmpty) return;
    setState(() {
      _messages.add(_MessageChat(texte: texte, deMoi: true));
      _chatController.clear();
    });
  }

  // ---------------------------------------------------------------------------
  // Popovers sombres (un seul à la fois, ancré sous le bouton)
  // ---------------------------------------------------------------------------

  void _fermerPopover() {
    _popover?.remove();
    _popover = null;
  }

  void _rafraichirPopover() => _popover?.markNeedsBuild();

  void _basculerPopover(BuildContext ancre, WidgetBuilder contenu,
      {double largeur = 236}) {
    if (_popover != null) {
      _fermerPopover();
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
              onTap: _fermerPopover,
            ),
          ),
          Positioned(
            left: gauche,
            top: haut,
            width: largeur,
            child: _CadrePopover(child: contenu(ctx)),
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
    return Scaffold(
      body: NovaAppFrame(
        vue: NovaVue.session,
        libelleSession: widget.args.libellePair,
        masquerChrome: _pleinEcran,
        afficherRail: false,
        etatGauche: _etatSession(),
        corps: Row(
          children: [
            Expanded(
              child: Stack(
                children: [
                  Positioned.fill(child: _surfaceDistante()),
                  _sinfo(),
                  if (_wbOuvert)
                    Positioned(top: 52, left: 14, child: _barreWhiteboard()),
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
              _moniteur == 2 ? 'Tous les écrans' : 'Écran ${_moniteur + 1}',
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

  Widget _contenuSurface() {
    final montrerVideo = _aRecuUneTrame && _etat != SessionStateDto.closed;
    if (!montrerVideo) return _apercuSimule();
    return SizedBox.expand(
      child: RepaintBoundary(
        child: CustomPaint(
          painter: _PeintreVideo(_trameCourante),
          size: Size.infinite,
        ),
      ),
    );
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
              _etat == SessionStateDto.active
                  ? '1920 × 1080 · 60 IPS · NVENC'
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
              onTap: () {
                setState(() => _enregistre = !_enregistre);
                _informer(_enregistre
                    ? 'Enregistrement démarré (démo — moteur au lot 04).'
                    : 'Enregistrement arrêté.');
              },
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
    return Container(
      decoration: const BoxDecoration(
        border: Border(right: BorderSide(color: _kToolBordure)),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          _indicateur(NovaIcones.bouclierCoche, 'Chiffré', pastilleVerte: true),
          _indicateur(NovaIcones.image, null),
          _indicateur(NovaIcones.disque, null),
        ],
      ),
    );
  }

  Widget _indicateur(IconData icone, String? libelle,
      {bool pastilleVerte = false}) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 11),
      height: 40,
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

  Widget _contenuInfo(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: const [
        _PopHeader('Poste distant'),
        _PopItem(texte: 'Windows 11 Pro · 24H2'),
        _PopItem(texte: 'Intel i7-13700 · 32 Go'),
        _PopItem(texte: '2 écrans · 1920×1080'),
        _PopItem(texte: 'Session chiffrée TLS 1.3'),
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
          onTap: () => _basculerPerm(() => _permClavierSouris = !_permClavierSouris),
        ),
        _PopItem(
          icone: NovaIcones.pressePapiers,
          texte: 'Synchroniser le presse-papiers',
          coche: _permPressePapiers,
          onTap: () => _basculerPerm(() => _permPressePapiers = !_permPressePapiers),
        ),
        _PopItem(
          icone: NovaIcones.audio,
          texte: 'Entendre le son distant',
          coche: _permAudio,
          onTap: () => _basculerPerm(() => _permAudio = !_permAudio),
        ),
        _PopItem(
          icone: NovaIcones.dossier,
          texte: 'Autoriser le transfert de fichiers',
          coche: _permTransfert,
          onTap: () => _basculerPerm(() => _permTransfert = !_permTransfert),
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
          onTap: () =>
              _basculerPerm(() => _permConfidentialite = !_permConfidentialite),
        ),
      ],
    );
  }

  void _basculerPerm(VoidCallback modif) {
    setState(modif);
    _rafraichirPopover();
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
              _fermerPopover();
              _aVenir("Demande d'élévation (UAC)");
            }),
        _PopItem(
            icone: NovaIcones.changerCote,
            texte: 'Changer de côté',
            onTap: () {
              _fermerPopover();
              _aVenir('Changement de côté');
            }),
        _PopItem(
            icone: NovaIcones.capture,
            texte: "Capture d'écran",
            onTap: () {
              _fermerPopover();
              _aVenir("Capture d'écran");
            }),
        _PopItem(
            icone: NovaIcones.redemarrer,
            texte: 'Redémarrer le poste distant',
            onTap: () {
              _fermerPopover();
              _aVenir('Redémarrage du poste distant');
            }),
        _PopItem(
            icone: NovaIcones.terminal,
            texte: 'Configurer un tunnel TCP',
            onTap: () {
              _fermerPopover();
              _aVenir('Tunnel TCP');
            }),
      ],
    );
  }

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
        for (final q in const [
          'Meilleure qualité',
          'Équilibré',
          'Meilleures performances'
        ])
          _PopItem(
            texte: q,
            selectionne: _qualite == q,
            onTap: () => _choisir(() => _qualite = q),
          ),
      ],
    );
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
            onTap: () {
              _fermerPopover();
              _aVenir("Cadre d'écran");
            }),
      ],
    );
  }

  Widget _contenuMoniteurs(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const _PopHeader('Moniteurs distants'),
        _PopItem(
          icone: NovaIcones.moniteur,
          texte: 'Écran 1 (principal)',
          selectionne: _moniteur == 0,
          onTap: () => _choisir(() => _moniteur = 0),
        ),
        _PopItem(
          icone: NovaIcones.moniteur,
          texte: 'Écran 2',
          selectionne: _moniteur == 1,
          onTap: () => _choisir(() => _moniteur = 1),
        ),
        _PopItem(
          icone: NovaIcones.tousEcrans,
          texte: 'Afficher tous les écrans',
          selectionne: _moniteur == 2,
          onTap: () => _choisir(() => _moniteur = 2),
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
              _fermerPopover();
              _aVenir('Clavier virtuel');
            }),
      ],
    );
  }

  void _choisir(VoidCallback modif) {
    setState(modif);
    _fermerPopover();
  }

  // ---------------------------------------------------------------------------
  // Tableau blanc (maquette `.wbtb`)
  // ---------------------------------------------------------------------------

  Widget _barreWhiteboard() {
    const outils = [
      NovaIcones.tableauBlanc,
      NovaIcones.carre,
      NovaIcones.flecheDiagonale,
      NovaIcones.cercle,
      NovaIcones.gomme,
    ];
    return Container(
      padding: const EdgeInsets.all(4),
      decoration: BoxDecoration(
        color: _kPopFond,
        borderRadius: BorderRadius.circular(6),
        border: Border.all(color: _kPopBordure),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final o in outils)
            _BoutonWhiteboard(icone: o, onTap: () => _aVenir('Tableau blanc')),
        ],
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Transfert de fichiers (maquette `.ft`)
  // ---------------------------------------------------------------------------

  Widget _transfert() {
    final t = NovaTokens.of(context);
    return Container(
      height: 248,
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
          _fileTransfert(t),
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

  Widget _fileTransfert(NovaTokens t) {
    return Container(
      height: 58,
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      decoration: BoxDecoration(
        border: Border(top: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          NovaIcone(NovaIcones.telecharger, taille: 14, couleur: t.vert),
          const SizedBox(width: 9),
          Text('build-9.7.3.zip', style: TextStyle(fontSize: 11.5, color: t.texte)),
          const SizedBox(width: 12),
          Expanded(
            child: ClipRRect(
              borderRadius: BorderRadius.circular(2),
              child: LinearProgressIndicator(
                value: 0.64,
                minHeight: 4,
                backgroundColor: t.filetFort,
                color: t.vert,
              ),
            ),
          ),
          const SizedBox(width: 12),
          Text('64% · 5,2 Mo/s',
              style: TextStyle(
                fontSize: 11.5,
                color: t.texte2,
                fontFeatures: const [ui.FontFeature.tabularFigures()],
              )),
        ],
      ),
    );
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

/// Bouton du mini-tableau blanc (maquette `.wbtb .tbtn`) : 32×32.
class _BoutonWhiteboard extends StatefulWidget {
  const _BoutonWhiteboard({required this.icone, required this.onTap});

  final IconData icone;
  final VoidCallback onTap;

  @override
  State<_BoutonWhiteboard> createState() => _BoutonWhiteboardState();
}

class _BoutonWhiteboardState extends State<_BoutonWhiteboard> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    return MouseRegion(
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
            color: _survole ? _kPopHover : Colors.transparent,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: NovaIcone(widget.icone,
              taille: 16, couleur: _survole ? Colors.white : _kToolIcone),
        ),
      ),
    );
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

/// Peintre de la surface vidéo : dessine la trame `ui.Image` courante en
/// conservant le ratio (letterbox), **sans aucun plugin natif**.
class _PeintreVideo extends CustomPainter {
  _PeintreVideo(this.trame) : super(repaint: trame);

  final ValueListenable<ui.Image?> trame;

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
    final double echelle = math.min(size.width / iw, size.height / ih);
    final double dw = iw * echelle;
    final double dh = ih * echelle;
    final double dx = (size.width - dw) / 2;
    final double dy = (size.height - dh) / 2;
    canvas.drawImageRect(
      image,
      Rect.fromLTWH(0, 0, iw, ih),
      Rect.fromLTWH(dx, dy, dw, dh),
      _peinture,
    );
  }

  @override
  bool shouldRepaint(covariant _PeintreVideo old) => old.trame != trame;
}

/// Message du panneau de discussion.
class _MessageChat {
  const _MessageChat({required this.texte, required this.deMoi});

  final String texte;
  final bool deMoi;
}
