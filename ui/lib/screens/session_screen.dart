/// Fenêtre de session (plan 10 §10.4.2) : surface vidéo distante, barre
/// d'outils (moniteurs, qualité, plein écran, Ctrl+Alt+Suppr, chat,
/// fichiers, fin de session) et barre d'état.
///
/// Rendu vidéo (plan 10 §10.3) : la trame décodée reste en mémoire GPU ; le
/// cœur Rust enregistre une **texture externe** auprès de l'embedder Flutter
/// (crate `irondash_texture`) et publiera un `textureId` entier. Ici, tant
/// que le pont réel n'est pas branché, `_textureId` reste `null` et un
/// panneau d'attente est affiché à la place du widget [Texture].
///
/// Capture des entrées : la surface écoute souris ([Listener]) et clavier
/// ([Focus]) ; chaque geste devient un `InputEventDto` sérialisé par
/// `encode_input_event` (façade `nd-ffi`) — les octets produits partiront
/// sur le canal `Input` une fois le transport branché.
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
  /// `null` tant que le pont réel n'est pas branché (plan 10 §10.3) ;
  /// il sera alors assigné à réception de l'événement `Streaming`.
  // ignore: prefer_final_fields
  int? _textureId;

  SessionStateDto _etat = SessionStateDto.idle;
  SessionStatusDto? _statut;
  bool _termine = false;

  // Barre d'outils.
  int _moniteur = 0;
  String _qualite = 'Auto';
  bool _pleinEcran = false;

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
      appBar: _pleinEcran ? null : _barreOutils(theme),
      endDrawer: _panneauChat(theme),
      body: Column(
        children: [
          Expanded(child: _surfaceDistante(theme)),
          _barreEtat(theme),
        ],
      ),
    );
  }

  PreferredSizeWidget _barreOutils(ThemeData theme) {
    return AppBar(
      title: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          const Icon(Icons.cast_connected, size: 20),
          const SizedBox(width: 10),
          Text(widget.args.libellePair),
        ],
      ),
      actions: [
        // Sélecteur de moniteur distant (plan 13 : multi-moniteur).
        PopupMenuButton<int>(
          tooltip: 'Moniteur distant',
          icon: const Icon(Icons.monitor_outlined),
          initialValue: _moniteur,
          onSelected: (valeur) {
            setState(() => _moniteur = valeur);
            _informer('Affichage de l’écran distant ${valeur + 1}.');
          },
          itemBuilder: (context) => [
            for (var i = 0; i < 2; i++)
              PopupMenuItem(value: i, child: Text('Écran ${i + 1}')),
          ],
        ),
        // Politique de qualité : la boucle d'adaptation reste côté cœur
        // (plans 03/04) ; l'UI n'envoie qu'une intention.
        PopupMenuButton<String>(
          tooltip: 'Qualité',
          icon: const Icon(Icons.tune),
          initialValue: _qualite,
          onSelected: (valeur) {
            setState(() => _qualite = valeur);
            _informer('Politique de qualité : $valeur.');
          },
          itemBuilder: (context) => const [
            PopupMenuItem(value: 'Auto', child: Text('Auto (adaptatif)')),
            PopupMenuItem(value: 'Fluidité', child: Text('Fluidité')),
            PopupMenuItem(value: 'Netteté', child: Text('Netteté')),
          ],
        ),
        IconButton(
          tooltip: _pleinEcran ? 'Quitter le plein écran (F11)'
              : 'Plein écran (F11)',
          icon: Icon(
            _pleinEcran ? Icons.fullscreen_exit : Icons.fullscreen,
          ),
          onPressed: _basculerPleinEcran,
        ),
        IconButton(
          tooltip: 'Envoyer Ctrl+Alt+Suppr',
          icon: const Icon(Icons.keyboard_command_key),
          onPressed:
              _etat == SessionStateDto.active ? _envoyerCtrlAltSuppr : null,
        ),
        IconButton(
          tooltip: 'Discussion',
          icon: const Icon(Icons.chat_bubble_outline),
          onPressed: () => _cleScaffold.currentState?.openEndDrawer(),
        ),
        IconButton(
          tooltip: 'Transfert de fichiers',
          icon: const Icon(Icons.folder_open_outlined),
          onPressed: _permissions.files ? _ouvrirTransferts : null,
        ),
        const SizedBox(width: 8),
        IconButton(
          tooltip: 'Terminer la session',
          icon: Icon(Icons.call_end, color: theme.colorScheme.error),
          onPressed: _terminerSession,
        ),
        const SizedBox(width: 4),
      ],
    );
  }

  /// Surface distante : capture souris/clavier + rendu vidéo.
  Widget _surfaceDistante(ThemeData theme) {
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
                  color: const Color(0xFF101014),
                  alignment: Alignment.center,
                  child: _textureId != null
                      // Composition zéro-copie de la trame GPU décodée
                      // (plan 10 §10.3.1).
                      ? Texture(
                          textureId: _textureId!,
                          filterQuality: FilterQuality.medium,
                        )
                      : _panneauAttente(theme),
                ),
              ),
            ),
          ),
        );
      },
    );
  }

  /// Panneau affiché tant qu'aucune texture n'est publiée par le cœur.
  Widget _panneauAttente(ThemeData theme) {
    final enEtablissement = switch (_etat) {
      SessionStateDto.resolving ||
      SessionStateDto.connecting ||
      SessionStateDto.handshaking ||
      SessionStateDto.reconnecting =>
        true,
      _ => false,
    };
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        if (enEtablissement)
          const SizedBox(
            width: 36,
            height: 36,
            child: CircularProgressIndicator(strokeWidth: 3),
          )
        else
          Icon(
            _etat == SessionStateDto.closed
                ? Icons.link_off
                : Icons.desktop_windows_outlined,
            size: 56,
            color: Colors.white38,
          ),
        const SizedBox(height: 16),
        Text(
          switch (_etat) {
            SessionStateDto.active => 'Surface vidéo distante',
            SessionStateDto.closed => 'Session terminée',
            _ => 'Session ${_etat.label}…',
          },
          style: theme.textTheme.titleMedium?.copyWith(color: Colors.white70),
        ),
        if (_etat == SessionStateDto.active) ...[
          const SizedBox(height: 8),
          const Text(
            'La trame décodée sera composée ici via une texture GPU externe\n'
            'publiée par le cœur Rust (widget Texture, zéro copie — plan 10 §10.3).',
            textAlign: TextAlign.center,
            style: TextStyle(color: Colors.white38, fontSize: 12),
          ),
        ],
      ],
    );
  }

  /// Barre d'état : badge d'état, sécurité, pair, compteurs d'entrées.
  Widget _barreEtat(ThemeData theme) {
    final peer = _statut?.peer;
    return Material(
      color: theme.colorScheme.surfaceContainerHighest,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
        child: Row(
          children: [
            SessionStateBadge(etat: _etat, dense: true),
            const SizedBox(width: 12),
            Icon(Icons.lock_outline,
                size: 14, color: theme.colorScheme.outline),
            const SizedBox(width: 4),
            Text(
              'Chiffré — TLS 1.3 + Noise_IK',
              style: theme.textTheme.bodySmall,
            ),
            if (_etat == SessionStateDto.active) ...[
              const SizedBox(width: 12),
              Text('SAS 47-19-83', style: theme.textTheme.bodySmall),
            ],
            if (peer != null) ...[
              const SizedBox(width: 12),
              Text('Pair : $peer', style: theme.textTheme.bodySmall),
            ],
            const Spacer(),
            Text(
              'Entrées : $_evenementsEnvoyes évt ($_octetsEnvoyes o) · '
              'Qualité : $_qualite · Écran ${_moniteur + 1}',
              style: theme.textTheme.bodySmall,
            ),
          ],
        ),
      ),
    );
  }

  /// Panneau latéral de discussion (contenu local ; le canal réel viendra
  /// du cœur via un Stream FRB).
  Widget _panneauChat(ThemeData theme) {
    return Drawer(
      child: SafeArea(
        child: Column(
          children: [
            Padding(
              padding: const EdgeInsets.all(16),
              child: Row(
                children: [
                  const Icon(Icons.chat_bubble_outline),
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
                        color: message.deMoi
                            ? theme.colorScheme.primaryContainer
                            : theme.colorScheme.surfaceContainerHighest,
                        borderRadius: BorderRadius.circular(12),
                      ),
                      child: Text(message.texte),
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
                        border: OutlineInputBorder(),
                        isDense: true,
                      ),
                      onSubmitted: (_) => _envoyerMessageChat(),
                    ),
                  ),
                  const SizedBox(width: 8),
                  IconButton.filled(
                    tooltip: 'Envoyer',
                    icon: const Icon(Icons.send),
                    onPressed: _envoyerMessageChat,
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
              color: theme.colorScheme.outline,
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
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Row(
        children: [
          const Icon(Icons.insert_drive_file_outlined, size: 20),
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
