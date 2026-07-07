/// Fenêtre de session — vue « En session » de la maquette
/// `anydesk-reference.html` : surface vidéo plein cadre sur fond noir,
/// **barre d'outils flottante sombre** centrée en haut (pair, sécurité,
/// affichage, entrées, outils, permissions, actions, « Terminer » rouge),
/// chrome (onglets + barre d'état) masqué en plein écran.
///
/// Rendu vidéo (plan 10 §10.3) : la trame décodée reste en mémoire GPU ; le
/// cœur Rust publiera un `textureId` entier. Tant que le pont réel n'est pas
/// branché, `_textureId` reste `null` et un aperçu simulé est affiché.
///
/// Capture des entrées : la surface écoute souris ([Listener]) et clavier
/// ([Focus]) ; chaque geste devient un `InputEventDto` sérialisé par
/// `encode_input_event` (façade `nd-ffi`).
library;

import 'dart:async';
import 'dart:io' show Platform;
import 'dart:math' as math;

import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/native_api.dart';
import '../platform/window_shim.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/app_frame.dart';
import '../widgets/nova_icons.dart';
import '../widgets/session_state_badge.dart';

/// Arguments de navigation vers la fenêtre de session.
class SessionScreenArgs {
  const SessionScreenArgs({required this.config, required this.libellePair});

  /// Configuration validée par `new_session_config` (façade `nd-ffi`).
  final SessionConfigDto config;

  /// Alias du pair s'il est au carnet, sinon son ID formaté.
  final String libellePair;
}

class SessionScreen extends ConsumerStatefulWidget {
  const SessionScreen({super.key, required this.args});

  static const String route = '/session';

  final SessionScreenArgs args;

  @override
  ConsumerState<SessionScreen> createState() => _SessionScreenState();
}

class _SessionScreenState extends ConsumerState<SessionScreen> {
  final GlobalKey<ScaffoldState> _cleScaffold = GlobalKey<ScaffoldState>();
  final FocusNode _noeudFocus = FocusNode(debugLabel: 'surface-session');
  final TextEditingController _chatController = TextEditingController();

  /// Identifiant de texture GPU externe fourni par le cœur Rust.
  /// `null` tant que le pont réel n'est pas branché (plan 10 §10.3).
  // ignore: prefer_final_fields
  int? _textureId;

  SessionStateDto _etat = SessionStateDto.idle;
  SessionStatusDto? _statut;
  bool _termine = false;

  // Barre d'outils.
  int _moniteur = 0;
  String _qualite = 'Équilibré';
  bool _pleinEcran = false;
  bool _enregistre = false;
  bool _favori = false;
  String _dispositionClavier = 'Auto';
  bool _transmettreRaccourcis = true;

  // Permissions commutables en cours de session (habillage : l'application
  // réelle des bascules passera par le canal contrôle du cœur, lot 04).
  late bool _permAudio = _permissions.audio;
  late bool _permClavierSouris =
      _permissions.keyboard && !_permissions.viewOnly;
  late bool _permPressePapiers = _permissions.clipboard;
  bool _permBloquerEntree = false;
  bool _permConfidentialite = false;

  // Compteurs d'entrées encodées (HUD honnête, plan 10 §10.6.2).
  int _evenementsEnvoyes = 0;
  int _octetsEnvoyes = 0;

  // Regroupement des mouvements souris pour ne pas saturer le pont
  // (plan 10 §10.2.2) : envoi au plus toutes les 8 ms.
  DateTime _dernierEnvoiSouris = DateTime.fromMillisecondsSinceEpoch(0);
  static const Duration _intervalleSouris = Duration(milliseconds: 8);
  int _boutonEnfonce = 0;

  // Chat (panneau latéral, contenu local en attendant le canal du cœur).
  final List<_MessageChat> _messages = [
    const _MessageChat(texte: 'Session ouverte. Canal de discussion prêt.',
        deMoi: false),
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

  /// Initiales du pair pour l'avatar de la barre d'outils (« PB »).
  String get _initialesPair {
    final mots = widget.args.libellePair
        .split(RegExp(r'[\s\-_.]+'))
        .where((m) => m.isNotEmpty)
        .toList();
    if (mots.isEmpty) return '?';
    if (mots.length == 1) {
      return mots.first.substring(0, math.min(2, mots.first.length))
          .toUpperCase();
    }
    return (mots[0][0] + mots[1][0]).toUpperCase();
  }

  @override
  void initState() {
    super.initState();
    unawaited(_deroulerConnexion());
  }

  @override
  void dispose() {
    if (_pleinEcran && _estDesktop) {
      // Restaure la fenêtre si la session se ferme en plein écran.
      unawaited(windowManager.setFullScreen(false));
    }
    _chatController.dispose();
    _noeudFocus.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Cycle de vie simulé de la session
  // ---------------------------------------------------------------------------

  /// SIMULATION : déroule la machine à états `nd_core::SessionState`
  /// (résolution → connexion → authentification → active). Le vrai pont
  /// poussera ces transitions via un `Stream` FRB (plan 10 §10.2.2).
  Future<void> _deroulerConnexion() async {
    const etapes = <(SessionStateDto, Duration)>[
      (SessionStateDto.resolving, Duration(milliseconds: 450)),
      (SessionStateDto.connecting, Duration(milliseconds: 700)),
      (SessionStateDto.handshaking, Duration(milliseconds: 600)),
      (SessionStateDto.active, Duration.zero),
    ];
    for (final (etat, duree) in etapes) {
      if (!mounted || _termine) return;
      await _changerEtat(etat);
      await Future<void>.delayed(duree);
    }
  }

  /// Change l'état et rafraîchit le statut affichable via la façade
  /// (`session_status` fournit le libellé + l'ID pair formaté).
  Future<void> _changerEtat(SessionStateDto etat) async {
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

  Future<void> _terminerSession() async {
    _termine = true;
    await _changerEtat(SessionStateDto.closed);
    if (_pleinEcran && _estDesktop) {
      _pleinEcran = false;
      await windowManager.setFullScreen(false);
    }
    await Future<void>.delayed(const Duration(milliseconds: 350));
    if (mounted) {
      Navigator.of(context).pop();
    }
  }

  // ---------------------------------------------------------------------------
  // Envoi des entrées (souris / clavier) vers le cœur
  // ---------------------------------------------------------------------------

  /// Sérialise l'événement via `encode_input_event`. Une fois le transport
  /// branché, les octets partiront sur le canal `Input` (nd-proto) ; ici on
  /// tient des compteurs pour le HUD.
  Future<void> _envoyer(InputEventDto evenement) async {
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

    final octets = await _api.encodeInputEvent(event: evenement);
    if (!mounted) return;
    setState(() {
      _evenementsEnvoyes++;
      _octetsEnvoyes += octets.length;
    });
  }

  double _normaliser(double valeur, double maximum) =>
      maximum <= 0 ? 0 : math.min(1.0, math.max(0.0, valeur / maximum));

  /// Mouvements (survol + glissé) : coordonnées absolues normalisées
  /// 0.0–1.0 sur le moniteur distant sélectionné, regroupées à 8 ms.
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

  /// Convention `nd-proto` : 0 = gauche, 1 = droit, 2 = milieu, 3 = X1, 4 = X2.
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

  /// Molette : pixels Flutter → crans (~120 px/cran) ; `nd-proto` compte
  /// positif = haut/droite, d'où l'inversion de l'axe vertical.
  void _surMolette(PointerSignalEvent evenement) {
    if (evenement is! PointerScrollEvent) return;
    unawaited(_envoyer(InputScroll(
      dx: evenement.scrollDelta.dx / 120.0,
      dy: -evenement.scrollDelta.dy / 120.0,
    )));
  }

  /// Clavier : scancode physique (usage USB HID) + point de code Unicode
  /// pour les touches productrices de texte (mapping détaillé au plan 07).
  KeyEventResult _surTouche(FocusNode noeud, KeyEvent evenement) {
    // Raccourcis locaux prioritaires (plan 10 §10.5.4).
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

    final enfoncee = evenement is! KeyUpEvent; // down et repeat
    unawaited(_envoyer(InputKey(
      scancode: evenement.physicalKey.usbHidUsage,
      down: enfoncee,
    )));
    final caractere = evenement.character;
    if (evenement is KeyDownEvent && caractere != null && caractere.isNotEmpty) {
      unawaited(
          _envoyer(InputUnicode(codepoint: caractere.runes.first)));
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

  /// Ctrl+Alt+Suppr : séquence appui/relâche envoyée touche par touche ;
  /// côté hôte, elle passera par le canal privilégié (plan 07).
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
    _informer('Ctrl+Alt+Suppr envoyé au poste distant (simulation).');
  }

  void _informer(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(content: Text(message)),
    );
  }

  void _aVenir(String fonction) => _informer('$fonction — à venir (lot 04).');

  void _ouvrirTransferts() {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      builder: (context) => const _FeuilleTransferts(),
    );
  }

  void _envoyerMessageChat() {
    final texte = _chatController.text.trim();
    if (texte.isEmpty) return;
    setState(() {
      _messages.add(_MessageChat(texte: texte, deMoi: true));
      _chatController.clear();
    });
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      key: _cleScaffold,
      endDrawer: _panneauChat(theme),
      body: NovaAppFrame(
        ongletActif: NovaOnglet.session,
        libelleSession: widget.args.libellePair,
        masquerChrome: _pleinEcran,
        etatGauche: _etatSession(),
        corps: Stack(
          children: [
            Positioned.fill(child: _surfaceDistante()),
            // Barre d'outils flottante, centrée en haut (maquette .toolbar).
            Positioned(
              top: 14,
              left: 12,
              right: 12,
              child: Center(child: _barreOutils()),
            ),
          ],
        ),
      ),
    );
  }

  /// Contenu session de la barre d'état basse (discrète, doc 03 §3).
  Widget _etatSession() {
    final t = NovaTokens.of(context);
    final peer = _statut?.peer;
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
              'Entrées : $_evenementsEnvoyes évt ($_octetsEnvoyes o)',
            ].join(' · '),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(fontSize: 11, color: t.texte3),
          ),
        ),
      ],
    );
  }

  // ---------------------------------------------------------------------------
  // Surface distante
  // ---------------------------------------------------------------------------

  /// Surface distante : capture souris/clavier + rendu vidéo sur fond noir.
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
              // RepaintBoundary : la vidéo se rafraîchit sans invalider le
              // chrome, et inversement (plan 10 §10.6.3).
              child: RepaintBoundary(
                child: Container(
                  color: const Color(0xFF000000),
                  alignment: Alignment.center,
                  child: _textureId != null
                      // Composition zéro-copie de la trame GPU décodée
                      // (plan 10 §10.3.1).
                      ? Texture(
                          textureId: _textureId!,
                          filterQuality: FilterQuality.medium,
                        )
                      : _apercuSimule(),
                ),
              ),
            ),
          ),
        );
      },
    );
  }

  /// Aperçu simulé du bureau distant (maquette `.screen`) tant qu'aucune
  /// texture n'est publiée par le cœur : dégradé sombre, résumé de session,
  /// barre des tâches esquissée.
  Widget _apercuSimule() {
    final enEtablissement = switch (_etat) {
      SessionStateDto.resolving ||
      SessionStateDto.connecting ||
      SessionStateDto.handshaking ||
      SessionStateDto.reconnecting =>
        true,
      _ => false,
    };
    final peer = _statut?.peer;

    return Container(
      decoration: const BoxDecoration(
        gradient: RadialGradient(
          center: Alignment(-0.36, -0.48),
          radius: 1.25,
          colors: [Color(0xFF223052), Color(0xFF16182A), Color(0xFF0C0E16)],
          stops: [0.0, 0.62, 1.0],
        ),
      ),
      child: Stack(
        children: [
          Center(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                if (enEtablissement)
                  const SizedBox(
                    width: 34,
                    height: 34,
                    child: CircularProgressIndicator(
                      strokeWidth: 2.5,
                      color: Color(0xFF7F88B3),
                    ),
                  )
                else
                  NovaIcone(
                    _etat == SessionStateDto.closed
                        ? NovaIcones.lienCoupe
                        : NovaIcones.moniteur,
                    taille: 54,
                    couleur: const Color(0xFF7F88B3),
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
                          color: Color(0xFFCFD6F2),
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                    ],
                  ),
                  style: const TextStyle(
                      fontSize: 13, color: Color(0xFF9AA3C8)),
                ),
                const SizedBox(height: 5),
                Text(
                  _etat == SessionStateDto.active
                      ? '1920 × 1080 · 60 IPS · latence 12 ms · '
                          'chiffré (Noise XX) — la trame décodée sera '
                          'composée ici (texture GPU, plan 10 §10.3)'
                      : 'Chiffrement TLS 1.3 + Noise_IK',
                  textAlign: TextAlign.center,
                  style: TextStyle(
                    fontSize: 11.5,
                    color: const Color(0xFF9AA3C8).withValues(alpha: 0.72),
                  ),
                ),
              ],
            ),
          ),
          // Barre des tâches esquissée (maquette `.taskbar`).
          Positioned(
            left: 0,
            right: 0,
            bottom: 0,
            height: 40,
            child: Container(
              color: const Color(0xFF0A0C14).withValues(alpha: 0.66),
              padding: const EdgeInsets.symmetric(horizontal: 12),
              child: Row(
                children: [
                  _pastilleTache(const Color(0xFF3A63D0)),
                  const SizedBox(width: 9),
                  _pastilleTache(Colors.white.withValues(alpha: 0.10)),
                  const SizedBox(width: 9),
                  _pastilleTache(Colors.white.withValues(alpha: 0.10)),
                  const SizedBox(width: 9),
                  _pastilleTache(Colors.white.withValues(alpha: 0.10)),
                  const Spacer(),
                  const Column(
                    mainAxisAlignment: MainAxisAlignment.center,
                    crossAxisAlignment: CrossAxisAlignment.end,
                    children: [
                      Text('14:07',
                          style: TextStyle(
                              fontSize: 11.5, color: Color(0xFFC3C9DF))),
                      Text('lun. 7 juil.',
                          style: TextStyle(
                              fontSize: 10, color: Color(0x99C3C9DF))),
                    ],
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _pastilleTache(Color couleur) {
    return Container(
      width: 24,
      height: 24,
      decoration: BoxDecoration(
        color: couleur,
        borderRadius: BorderRadius.circular(5),
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Barre d'outils flottante
  // ---------------------------------------------------------------------------

  static const Color _iconeBarre = Color(0xFFCFD3DA);

  Widget _barreOutils() {
    final actif = _etat == SessionStateDto.active;
    return Container(
      padding: const EdgeInsets.all(5),
      decoration: BoxDecoration(
        color: const Color(0xFF1A1C21).withValues(alpha: 0.94),
        borderRadius: BorderRadius.circular(11),
        border: Border.all(color: Colors.white.withValues(alpha: 0.09)),
        boxShadow: [
          BoxShadow(
            color: Colors.black.withValues(alpha: 0.5),
            blurRadius: 34,
            offset: const Offset(0, 12),
          ),
        ],
      ),
      child: SingleChildScrollView(
        scrollDirection: Axis.horizontal,
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            _blocPair(),
            _separateur(),
            // Sécurité / empreinte (indicateur en barre d'outils, doc 03 §3).
            _menuSecurite(),
            _separateur(),
            // Affichage.
            _menuMoniteurs(),
            _menuQualite(),
            _BoutonBarre(
              icone: _pleinEcran
                  ? NovaIcones.quitterPleinEcran
                  : NovaIcones.pleinEcran,
              infobulle: _pleinEcran
                  ? 'Quitter le plein écran (F11)'
                  : 'Plein écran (F11)',
              onTap: () => unawaited(_basculerPleinEcran()),
            ),
            _separateur(),
            // Entrées.
            _menuClavier(),
            _BoutonBarre(
              icone: NovaIcones.ctrlAltSuppr,
              infobulle: 'Ctrl+Alt+Suppr',
              onTap: actif ? () => unawaited(_envoyerCtrlAltSuppr()) : null,
            ),
            _menuPressePapiers(),
            _separateur(),
            // Outils.
            _BoutonBarre(
              icone: NovaIcones.dossier,
              infobulle: 'Transfert de fichiers',
              onTap: _permissions.files ? _ouvrirTransferts : null,
            ),
            _BoutonBarre(
              icone: NovaIcones.discussion,
              infobulle: 'Discussion',
              onTap: () => _cleScaffold.currentState?.openEndDrawer(),
            ),
            _BoutonBarre(
              icone: NovaIcones.enregistrer,
              infobulle: _enregistre
                  ? "Arrêter l'enregistrement"
                  : 'Enregistrer la session',
              actif: _enregistre,
              pastilleRouge: _enregistre,
              onTap: () {
                setState(() => _enregistre = !_enregistre);
                _informer(_enregistre
                    ? 'Enregistrement démarré (démo — moteur au lot 04).'
                    : 'Enregistrement arrêté.');
              },
            ),
            _separateur(),
            // Permissions, favori, actions.
            _menuPermissions(),
            _BoutonBarre(
              icone: _favori ? NovaIcones.etoilePleine : NovaIcones.etoile,
              infobulle:
                  _favori ? 'Retirer des favoris' : 'Ajouter aux favoris',
              onTap: () => setState(() => _favori = !_favori),
            ),
            _menuActions(),
            _separateur(),
            _BoutonBarre(
              icone: NovaIcones.fermer,
              infobulle: 'Terminer',
              fermeture: true,
              onTap: () => unawaited(_terminerSession()),
            ),
          ],
        ),
      ),
    );
  }

  /// Identité du pair : avatar rouge (maquette `.peer .av`) + nom + latence.
  Widget _blocPair() {
    final sousTitre = _etat == SessionStateDto.active
        ? 'connecté · 12 ms'
        : _etat.label;
    return Padding(
      padding: const EdgeInsets.only(left: 3, right: 12),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: 26,
            height: 26,
            alignment: Alignment.center,
            decoration: BoxDecoration(
              color: kNovaRouge,
              borderRadius: BorderRadius.circular(7),
            ),
            child: Text(
              _initialesPair,
              style: const TextStyle(
                fontSize: 11,
                fontWeight: FontWeight.w700,
                color: Colors.white,
              ),
            ),
          ),
          const SizedBox(width: 9),
          Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                widget.args.libellePair,
                style: const TextStyle(
                  fontSize: 12,
                  fontWeight: FontWeight.w600,
                  color: Colors.white,
                ),
              ),
              Text(
                sousTitre,
                style:
                    const TextStyle(fontSize: 10.5, color: Color(0xFF8B929C)),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _separateur() {
    return Container(
      width: 1,
      height: 24,
      margin: const EdgeInsets.symmetric(horizontal: 5),
      color: Colors.white.withValues(alpha: 0.12),
    );
  }

  /// Item informatif (non cliquable mais lisible) pour les menus.
  PopupMenuItem<T> _itemInfo<T>(String texte, {NovaIconeData? icone}) {
    return PopupMenuItem<T>(
      enabled: false,
      height: 32,
      child: Row(
        children: [
          if (icone != null) ...[
            NovaIcone(icone, taille: 14),
            const SizedBox(width: 8),
          ],
          Expanded(
            child: Text(
              texte,
              style: TextStyle(
                fontSize: 12,
                color: Theme.of(context).colorScheme.onSurface,
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _menuSecurite() {
    return PopupMenuButton<void>(
      tooltip: 'Sécurité de la connexion',
      offset: const Offset(0, 42),
      itemBuilder: (context) => [
        PopupMenuItem<void>(
          enabled: false,
          height: 34,
          child: Row(
            children: [
              const NovaIcone(NovaIcones.bouclierCoche,
                  taille: 15, couleur: kNovaVert),
              const SizedBox(width: 8),
              Text(
                'Connexion vérifiée',
                style: TextStyle(
                  fontSize: 12.5,
                  fontWeight: FontWeight.w600,
                  color: Theme.of(context).colorScheme.onSurface,
                ),
              ),
            ],
          ),
        ),
        const PopupMenuDivider(),
        _itemInfo<void>('Transport : P2P direct (QUIC)',
            icone: NovaIcones.globe),
        _itemInfo<void>('Chiffrement : TLS 1.3 + Noise_IK',
            icone: NovaIcones.cadenas),
        _itemInfo<void>('Empreinte : 9A:F2:04:6B:D8:33:71:CE',
            icone: NovaIcones.cle),
        _itemInfo<void>('SAS : 47-19-83', icone: NovaIcones.coche),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.cadenas,
        onTapGere: true,
      ),
    );
  }

  Widget _menuMoniteurs() {
    return PopupMenuButton<int>(
      tooltip: 'Moniteurs',
      offset: const Offset(0, 42),
      initialValue: _moniteur,
      onSelected: (valeur) {
        setState(() => _moniteur = valeur);
        _informer(valeur == 2
            ? 'Affichage de tous les écrans distants.'
            : "Affichage de l'écran distant ${valeur + 1}.");
      },
      itemBuilder: (context) => [
        for (var i = 0; i < 2; i++)
          PopupMenuItem(value: i, height: 34, child: Text('Écran ${i + 1}')),
        const PopupMenuItem(
            value: 2, height: 34, child: Text('Tous les écrans')),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.moniteurs,
        onTapGere: true,
      ),
    );
  }

  Widget _menuQualite() {
    const politiques = [
      'Meilleure qualité',
      'Équilibré',
      'Meilleures performances',
    ];
    return PopupMenuButton<String>(
      tooltip: 'Qualité / vitesse',
      offset: const Offset(0, 42),
      initialValue: _qualite,
      onSelected: (valeur) {
        setState(() => _qualite = valeur);
        _informer('Politique de qualité : $valeur.');
      },
      itemBuilder: (context) => [
        for (final politique in politiques)
          PopupMenuItem(
            value: politique,
            height: 34,
            child: Row(
              children: [
                SizedBox(
                  width: 20,
                  child: politique == _qualite
                      ? const NovaIcone(NovaIcones.coche, taille: 13)
                      : null,
                ),
                Text(politique),
              ],
            ),
          ),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.qualite,
        onTapGere: true,
      ),
    );
  }

  Widget _menuClavier() {
    return PopupMenuButton<String>(
      tooltip: 'Clavier et saisie',
      offset: const Offset(0, 42),
      onSelected: (valeur) {
        if (valeur == 'raccourcis') {
          setState(() => _transmettreRaccourcis = !_transmettreRaccourcis);
        } else {
          setState(() => _dispositionClavier = valeur);
          _informer('Disposition clavier : $valeur.');
        }
      },
      itemBuilder: (context) => [
        _itemInfo<String>('Mode de transmission des touches'),
        for (final disposition in const ['Auto', 'AZERTY (fr)', 'QWERTY (us)'])
          PopupMenuItem(
            value: disposition,
            height: 34,
            child: Row(
              children: [
                SizedBox(
                  width: 20,
                  child: disposition == _dispositionClavier
                      ? const NovaIcone(NovaIcones.coche, taille: 13)
                      : null,
                ),
                Text(disposition),
              ],
            ),
          ),
        const PopupMenuDivider(),
        CheckedPopupMenuItem(
          value: 'raccourcis',
          height: 34,
          checked: _transmettreRaccourcis,
          child: const Text('Transmettre les raccourcis système'),
        ),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.clavier,
        onTapGere: true,
      ),
    );
  }

  Widget _menuPressePapiers() {
    return PopupMenuButton<String>(
      tooltip: 'Presse-papiers',
      offset: const Offset(0, 42),
      onSelected: (valeur) => _aVenir(valeur == 'envoyer'
          ? 'Envoi du presse-papiers'
          : 'Récupération du presse-papiers distant'),
      itemBuilder: (context) => const [
        PopupMenuItem(
            value: 'envoyer',
            height: 34,
            child: Text('Envoyer le presse-papiers')),
        PopupMenuItem(
            value: 'recuperer',
            height: 34,
            child: Text('Récupérer le presse-papiers distant')),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.pressePapiers,
        onTapGere: true,
      ),
    );
  }

  /// Permissions commutables (menu à cases — doc 03 §3 item 10).
  Widget _menuPermissions() {
    return PopupMenuButton<String>(
      tooltip: 'Permissions',
      offset: const Offset(0, 42),
      onSelected: (valeur) => setState(() {
        switch (valeur) {
          case 'audio':
            _permAudio = !_permAudio;
          case 'entrees':
            _permClavierSouris = !_permClavierSouris;
          case 'presse':
            _permPressePapiers = !_permPressePapiers;
          case 'bloquer':
            _permBloquerEntree = !_permBloquerEntree;
          case 'confidentialite':
            _permConfidentialite = !_permConfidentialite;
        }
      }),
      itemBuilder: (context) => [
        _itemInfo<String>('Permissions de la session'),
        CheckedPopupMenuItem(
          value: 'audio',
          height: 34,
          checked: _permAudio,
          child: const Text('Transmettre le son'),
        ),
        CheckedPopupMenuItem(
          value: 'entrees',
          height: 34,
          checked: _permClavierSouris,
          child: const Text('Clavier et souris'),
        ),
        CheckedPopupMenuItem(
          value: 'presse',
          height: 34,
          checked: _permPressePapiers,
          child: const Text('Presse-papiers'),
        ),
        const PopupMenuDivider(),
        CheckedPopupMenuItem(
          value: 'bloquer',
          height: 34,
          checked: _permBloquerEntree,
          child: const Text("Bloquer l'entrée distante"),
        ),
        CheckedPopupMenuItem(
          value: 'confidentialite',
          height: 34,
          checked: _permConfidentialite,
          child: const Text('Mode confidentialité'),
        ),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.bouclier,
        onTapGere: true,
      ),
    );
  }

  /// Actions à distance (menu groupé — doc 03 §3 item 8).
  Widget _menuActions() {
    return PopupMenuButton<String>(
      tooltip: 'Actions',
      offset: const Offset(0, 42),
      onSelected: (valeur) {
        switch (valeur) {
          case 'cad':
            unawaited(_envoyerCtrlAltSuppr());
          case 'elevation':
            _aVenir("Demande d'élévation (UAC)");
          case 'verrouiller':
            _aVenir('Verrouillage du poste distant');
          case 'redemarrer':
            _aVenir('Redémarrage du poste distant');
          case 'capture':
            _aVenir("Capture d'écran");
          case 'tunnel':
            _aVenir('Tunnel TCP');
        }
      },
      itemBuilder: (context) => [
        _itemInfo<String>('Actions à distance'),
        const PopupMenuItem(
          value: 'elevation',
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.eclair, taille: 14),
            SizedBox(width: 8),
            Text("Demander l'élévation (UAC)"),
          ]),
        ),
        const PopupMenuItem(
          value: 'cad',
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.ctrlAltSuppr, taille: 14),
            SizedBox(width: 8),
            Text('Envoyer Ctrl+Alt+Suppr'),
          ]),
        ),
        const PopupMenuItem(
          value: 'verrouiller',
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.cadenas, taille: 14),
            SizedBox(width: 8),
            Text('Verrouiller le poste distant'),
          ]),
        ),
        const PopupMenuItem(
          value: 'redemarrer',
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.alimentation, taille: 14),
            SizedBox(width: 8),
            Text('Redémarrer le poste distant'),
          ]),
        ),
        const PopupMenuItem(
          value: 'capture',
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.capture, taille: 14),
            SizedBox(width: 8),
            Text("Capture d'écran"),
          ]),
        ),
        const PopupMenuItem(
          value: 'tunnel',
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.terminal, taille: 14),
            SizedBox(width: 8),
            Text('Tunnel TCP…'),
          ]),
        ),
      ],
      child: const _CorpsBoutonBarre(
        icone: NovaIcones.troisPoints,
        onTapGere: true,
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Panneau de discussion (contenu local ; canal réel via Stream FRB, lot 04)
  // ---------------------------------------------------------------------------

  Widget _panneauChat(ThemeData theme) {
    final t = theme.extension<NovaTokens>()!;
    return Drawer(
      child: SafeArea(
        child: Column(
          children: [
            Padding(
              padding: const EdgeInsets.all(16),
              child: Row(
                children: [
                  NovaIcone(NovaIcones.discussion,
                      taille: 16, couleur: t.texte2),
                  const SizedBox(width: 8),
                  Text('Discussion', style: theme.textTheme.titleMedium),
                ],
              ),
            ),
            const Divider(height: 1),
            Expanded(
              child: ListView.builder(
                padding: const EdgeInsets.all(12),
                itemCount: _messages.length,
                itemBuilder: (context, index) {
                  final message = _messages[index];
                  return Align(
                    alignment: message.deMoi
                        ? Alignment.centerRight
                        : Alignment.centerLeft,
                    child: Container(
                      margin: const EdgeInsets.symmetric(vertical: 4),
                      padding: const EdgeInsets.symmetric(
                          horizontal: 12, vertical: 8),
                      decoration: BoxDecoration(
                        color: message.deMoi ? t.champ : t.panneau,
                        border: Border.all(color: t.filet),
                        borderRadius: BorderRadius.circular(9),
                      ),
                      child: Text(message.texte,
                          style: const TextStyle(fontSize: 12.5)),
                    ),
                  );
                },
              ),
            ),
            const Divider(height: 1),
            Padding(
              padding: const EdgeInsets.all(12),
              child: Row(
                children: [
                  Expanded(
                    child: TextField(
                      controller: _chatController,
                      decoration: const InputDecoration(
                        hintText: 'Écrire un message…',
                      ),
                      onSubmitted: (_) => _envoyerMessageChat(),
                    ),
                  ),
                  const SizedBox(width: 8),
                  SizedBox(
                    height: 38,
                    child: FilledButton(
                      onPressed: _envoyerMessageChat,
                      style: FilledButton.styleFrom(
                        backgroundColor: t.texte,
                        foregroundColor: t.fenetre,
                        padding:
                            const EdgeInsets.symmetric(horizontal: 12),
                      ),
                      child: const Text('Envoyer'),
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Boutons de la barre d'outils
// ---------------------------------------------------------------------------

/// Bouton 38×36 de la barre flottante : survol blanc translucide,
/// « Terminer » vire au rouge (usage réservé autorisé).
class _BoutonBarre extends StatefulWidget {
  const _BoutonBarre({
    required this.icone,
    required this.infobulle,
    this.onTap,
    this.fermeture = false,
    this.actif = false,
    this.pastilleRouge = false,
  });

  final NovaIconeData icone;
  final String infobulle;
  final VoidCallback? onTap;
  final bool fermeture;

  /// État enclenché (fond marqué en continu, ex. enregistrement).
  final bool actif;

  /// Point rouge « REC » en surimpression.
  final bool pastilleRouge;

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
            ? Colors.white.withValues(alpha: 0.10)
            : Colors.transparent;
    final Color couleur = desactive
        ? _SessionScreenState._iconeBarre.withValues(alpha: 0.35)
        : _survole
            ? Colors.white
            : _SessionScreenState._iconeBarre;

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
            height: 36,
            decoration: BoxDecoration(
              color: fond,
              borderRadius: BorderRadius.circular(8),
            ),
            child: Stack(
              alignment: Alignment.center,
              children: [
                NovaIcone(widget.icone, taille: 19, couleur: couleur),
                if (widget.pastilleRouge)
                  Positioned(
                    top: 5,
                    right: 6,
                    child: Container(
                      width: 7,
                      height: 7,
                      decoration: const BoxDecoration(
                        color: kNovaRouge,
                        shape: BoxShape.circle,
                      ),
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

/// Corps visuel d'un bouton de barre destiné à être enveloppé dans un
/// [PopupMenuButton] (qui gère lui-même le tap et l'infobulle).
class _CorpsBoutonBarre extends StatefulWidget {
  const _CorpsBoutonBarre({required this.icone, this.onTapGere = false});

  final NovaIconeData icone;

  /// Présent uniquement pour documenter que le parent gère le tap.
  final bool onTapGere;

  @override
  State<_CorpsBoutonBarre> createState() => _CorpsBoutonBarreState();
}

class _CorpsBoutonBarreState extends State<_CorpsBoutonBarre> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: Container(
        width: 38,
        height: 36,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: _survole
              ? Colors.white.withValues(alpha: 0.10)
              : Colors.transparent,
          borderRadius: BorderRadius.circular(8),
        ),
        child: NovaIcone(
          widget.icone,
          taille: 19,
          couleur:
              _survole ? Colors.white : _SessionScreenState._iconeBarre,
        ),
      ),
    );
  }
}

/// Message du panneau de discussion.
class _MessageChat {
  const _MessageChat({required this.texte, required this.deMoi});

  final String texte;
  final bool deMoi;
}

/// Feuille « Transfert de fichiers » : aperçu de la file d'attente.
/// Le moteur réel (plan 09) alimentera cette vue via un `Stream` FRB de
/// progression ; le gestionnaire double-panneau complet suivra.
class _FeuilleTransferts extends StatelessWidget {
  const _FeuilleTransferts();

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(20, 4, 20, 24),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Transfert de fichiers', style: theme.textTheme.titleMedium),
          const SizedBox(height: 12),
          const _LigneTransfert(
            nom: 'plan.png',
            direction: '→ distant',
            progression: 0.68,
            debit: '0,9 Mo/s',
          ),
          const _LigneTransfert(
            nom: 'build.zip',
            direction: '← local',
            progression: 0.22,
            debit: '1,4 Mo/s',
          ),
          const SizedBox(height: 12),
          Text(
            'File d’attente fictive : le moteur de transfert (plan 09) '
            'alimentera cette vue via un flux de progression FRB.',
            style: theme.textTheme.bodySmall?.copyWith(
              color: theme.extension<NovaTokens>()!.texte3,
            ),
          ),
        ],
      ),
    );
  }
}

class _LigneTransfert extends StatelessWidget {
  const _LigneTransfert({
    required this.nom,
    required this.direction,
    required this.progression,
    required this.debit,
  });

  final String nom;
  final String direction;
  final double progression;
  final String debit;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final t = theme.extension<NovaTokens>()!;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Row(
        children: [
          NovaIcone(NovaIcones.dossier, taille: 18, couleur: t.texte2),
          const SizedBox(width: 10),
          SizedBox(width: 110, child: Text(nom, overflow: TextOverflow.ellipsis)),
          SizedBox(width: 70, child: Text(direction,
              style: theme.textTheme.bodySmall)),
          Expanded(
            child: ClipRRect(
              borderRadius: BorderRadius.circular(4),
              child: LinearProgressIndicator(value: progression, minHeight: 6),
            ),
          ),
          const SizedBox(width: 10),
          Text('${(progression * 100).round()} % · $debit',
              style: theme.textTheme.bodySmall),
        ],
      ),
    );
  }
}
