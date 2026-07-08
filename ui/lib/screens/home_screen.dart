/// Écran d'accueil — fidèle à la maquette `novadesk-app.html` (`#v-accueil`) :
/// deux colonnes séparées par un filet (« Poste distant » : champ d'adresse +
/// bouton rouge « Se connecter » + modes ; « Ce poste » : adresse à 9 chiffres,
/// alias, liens Copier/Inviter/Accès non surveillé), onglets Sessions récentes
/// / Favoris / Découverts, **liste d'appareils** avec squelette de chargement,
/// état vide sur Découverts, menu contextuel au clic droit et toasts.
///
/// Câblage moteur **préservé** : la connexion par ID valide la saisie via la
/// façade (`parse_nova_id` + `new_session_config`) puis ouvre la session en
/// **mise en relation par rendez-vous** ([SessionEndpointByRendezvous]).
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../theme/motion.dart';
import '../theme/nova_theme.dart';
import '../widgets/app_frame.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_id_field.dart';
import '../widgets/nova_kit.dart';
import 'session_screen.dart';

/// Mode de connexion, traduit en [PermissionsDto] avant `new_session_config`.
enum ModeConnexion { controle, observation, fichiers }

extension _ModeConnexionX on ModeConnexion {
  String get libelle => switch (this) {
        ModeConnexion.controle => 'Contrôle',
        ModeConnexion.observation => 'Observation',
        ModeConnexion.fichiers => 'Fichiers seul',
      };

  IconData get icone => switch (this) {
        ModeConnexion.controle => NovaIcones.controle,
        ModeConnexion.observation => NovaIcones.observation,
        ModeConnexion.fichiers => NovaIcones.fichiers,
      };

  PermissionsDto get permissions => switch (this) {
        ModeConnexion.controle => PermissionsDto.full(),
        ModeConnexion.observation => PermissionsDto.viewOnly(),
        ModeConnexion.fichiers => const PermissionsDto(
            keyboard: false,
            mouse: false,
            clipboard: false,
            files: true,
            audio: false,
            viewOnly: true,
          ),
      };
}

/// Onglet de la liste d'appareils.
enum _OngletAccueil { recentes, favoris, decouverts }

class HomeScreen extends ConsumerStatefulWidget {
  const HomeScreen({super.key});

  static const String route = NovaRoutes.accueil;

  @override
  ConsumerState<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends ConsumerState<HomeScreen> {
  final TextEditingController _adresseController = TextEditingController();
  final FocusNode _adresseFocus = FocusNode(debugLabel: 'champ-adresse');

  ModeConnexion _mode = ModeConnexion.controle;
  bool _connexionEnCours = false;
  bool _adresseEnFocus = false;
  _OngletAccueil _onglet = _OngletAccueil.recentes;
  bool _idCopie = false;
  Timer? _minuteurCopie;

  /// Squelette de chargement de la liste (~780 ms comme la maquette).
  bool _chargement = true;
  Timer? _minuteurChargement;

  @override
  void initState() {
    super.initState();
    _adresseFocus.addListener(
      () => setState(() => _adresseEnFocus = _adresseFocus.hasFocus),
    );
    _minuteurChargement = Timer(const Duration(milliseconds: 780), () {
      if (mounted) setState(() => _chargement = false);
    });
  }

  @override
  void dispose() {
    _minuteurCopie?.cancel();
    _minuteurChargement?.cancel();
    _adresseController.dispose();
    _adresseFocus.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Connexion (câblage moteur préservé)
  // ---------------------------------------------------------------------------

  /// Valide la saisie via la façade puis ouvre la fenêtre de session en mise en
  /// relation **par ID** ([SessionEndpointByRendezvous]).
  Future<void> _seConnecter([String? saisieExplicite]) async {
    final api = ref.read(nativeApiProvider);
    final idLocal = ref.read(idLocalProvider);
    final saisie = (saisieExplicite ?? _adresseController.text).trim();
    final carnet = ref.read(carnetProvider);

    setState(() => _connexionEnCours = true);
    try {
      final correspondance = carnet
          .where((e) => e.alias.toLowerCase() == saisie.toLowerCase())
          .firstOrNull;
      final idPair = correspondance?.id ?? await api.parseNovaId(texte: saisie);
      final config = await api.newSessionConfig(
        role: SessionRoleDto.controller,
        localId: idLocal,
        peerId: idPair,
        permissions: _mode.permissions,
      );
      final idFormate = await api.formatNovaId(id: idPair);
      final alias = correspondance?.alias ??
          carnet.where((e) => e.id == idPair).map((e) => e.alias).firstOrNull;
      // Connexion par ID : mise en relation via le serveur de rendez-vous
      // (STUN → hole punching → QUIC), adresses issues des réglages réseau.
      final endpoint = SessionEndpointByRendezvous(
        server: ref.read(rendezvousProvider),
        stunServers: ref.read(stunServersProvider),
        relay: ref.read(relayProvider),
      );
      final options = SessionOptionsDto(permissions: _mode.permissions);
      if (!mounted) return;
      await Navigator.of(context).pushNamed(
        SessionScreen.route,
        arguments: SessionScreenArgs(
          config: config,
          libellePair: alias ?? idFormate,
          endpoint: endpoint,
          options: options,
        ),
      );
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    } finally {
      if (mounted) setState(() => _connexionEnCours = false);
    }
  }

  Future<void> _connecterEntree(EntreeCarnet entree) async {
    final idFormate =
        await ref.read(nativeApiProvider).formatNovaId(id: entree.id);
    _adresseController.text = idFormate;
    await _seConnecter(idFormate);
  }

  // ---------------------------------------------------------------------------
  // Actions du carnet (état local)
  // ---------------------------------------------------------------------------

  void _basculerFavori(EntreeCarnet entree) {
    final carnet = ref.read(carnetProvider.notifier);
    carnet.state = [
      for (final e in carnet.state)
        e.id == entree.id ? e.copyWith(favori: !e.favori) : e,
    ];
    NovaToast.montrer(
      context,
      entree.favori
          ? '${entree.alias} retiré des favoris'
          : '${entree.alias} ajouté aux favoris',
    );
  }

  Future<void> _menuContextuel(EntreeCarnet entree, Offset position) async {
    final choix = await showNovaContextMenu(context, position, const [
      NovaMenuAction('conn', 'Se connecter', NovaIcones.flecheDroite),
      NovaMenuAction('obs', 'Observer', NovaIcones.observation),
      NovaMenuAction('ft', 'Transfert de fichiers', NovaIcones.dossier),
      NovaMenuAction('fav', 'Ajouter aux favoris', NovaIcones.etoile,
          separateurAvant: true),
      NovaMenuAction('ren', 'Renommer', NovaIcones.crayon),
      NovaMenuAction('wol', 'Wake-on-LAN', NovaIcones.alimentation),
      NovaMenuAction('del', 'Supprimer', NovaIcones.corbeille,
          danger: true, separateurAvant: true),
    ]);
    if (!mounted || choix == null) return;
    switch (choix) {
      case 'conn':
      case 'obs':
        unawaited(_connecterEntree(entree));
      case 'fav':
        _basculerFavori(entree);
      case 'ren':
        unawaited(_renommer(entree));
      case 'wol':
        NovaToast.montrer(
            context, 'Paquet Wake-on-LAN envoyé à ${entree.alias}',
            info: true);
      case 'del':
        ref.read(carnetProvider.notifier).state = ref
            .read(carnetProvider)
            .where((e) => e.id != entree.id)
            .toList();
        NovaToast.montrer(context, '${entree.alias} supprimé du carnet');
      case 'ft':
        NovaToast.montrer(context, 'Transfert de fichiers — ${entree.alias}',
            info: true);
    }
  }

  Future<void> _renommer(EntreeCarnet entree) async {
    final controller = TextEditingController(text: entree.alias);
    final nouvelAlias = await montrerDialogueNova<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Renommer'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(labelText: 'Alias'),
          onSubmitted: (v) => Navigator.of(context).pop(v),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Annuler'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(controller.text),
            child: const Text('Renommer'),
          ),
        ],
      ),
    );
    controller.dispose();
    final alias = nouvelAlias?.trim();
    if (alias == null || alias.isEmpty) return;
    ref.read(carnetProvider.notifier).state = [
      for (final e in ref.read(carnetProvider))
        e.id == entree.id ? e.copyWith(alias: alias) : e,
    ];
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    // Corps seul : l'habillage (barre de titre, rail, barre d'état) est fourni
    // une seule fois par la coquille persistante (NovaCoquille).
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _entete(),
        _barreOnglets(),
        Expanded(child: _liste()),
      ],
    );
  }

  // --- En-tête deux colonnes ------------------------------------------------

  Widget _entete() {
    final t = NovaTokens.of(context);
    return Container(
      decoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      padding: const EdgeInsets.fromLTRB(0, 20, 0, 16),
      child: IntrinsicHeight(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Expanded(
              child: Padding(
                padding: const EdgeInsets.symmetric(horizontal: 26),
                child: _colonnePosteDistant(t),
              ),
            ),
            Container(width: 1, color: t.filet),
            Expanded(
              child: Padding(
                padding: const EdgeInsets.symmetric(horizontal: 26),
                child: _colonneCePoste(t),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _colonnePosteDistant(NovaTokens t) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const NovaPanelHeader(NovaIcones.moniteur, 'Poste distant'),
        const SizedBox(height: 11),
        Row(
          children: [
            Expanded(child: _champAdresse(t)),
            const SizedBox(width: 8),
            NovaBoutonPrimaire(
              libelle: 'Se connecter',
              icone: NovaIcones.flecheDroite,
              hauteur: 38,
              enCours: _connexionEnCours,
              onPressed: _connexionEnCours ? null : () => _seConnecter(),
            ),
          ],
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 6,
          runSpacing: 6,
          children: [
            for (final mode in ModeConnexion.values) _puceMode(t, mode),
          ],
        ),
      ],
    );
  }

  Widget _champAdresse(NovaTokens t) {
    return AnimatedContainer(
      duration: const Duration(milliseconds: 120),
      height: 38,
      padding: const EdgeInsets.symmetric(horizontal: 11),
      decoration: BoxDecoration(
        color: t.fenetre,
        borderRadius: BorderRadius.circular(kNovaRayon),
        border: Border.all(
            color: _adresseEnFocus ? kNovaRouge : t.champBordure),
      ),
      child: Row(
        children: [
          NovaIcone(NovaIcones.adresse, taille: 16, couleur: t.texte3),
          const SizedBox(width: 9),
          Expanded(
            child: TextField(
              controller: _adresseController,
              focusNode: _adresseFocus,
              inputFormatters: const [_FormateurAdresse()],
              style: TextStyle(
                fontSize: 15,
                color: t.texte,
                fontFeatures: const [FontFeature.tabularFigures()],
              ),
              decoration: InputDecoration(
                isCollapsed: true,
                filled: false,
                border: InputBorder.none,
                enabledBorder: InputBorder.none,
                focusedBorder: InputBorder.none,
                hintText: "Saisir l'adresse du poste distant",
                hintStyle: TextStyle(fontSize: 13.5, color: t.texte3),
              ),
              onSubmitted: (_) => unawaited(_seConnecter()),
            ),
          ),
        ],
      ),
    );
  }

  Widget _puceMode(NovaTokens t, ModeConnexion mode) {
    final actif = _mode == mode;
    return GestureDetector(
      onTap: () => setState(() => _mode = mode),
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 11, vertical: 5),
          decoration: BoxDecoration(
            color: actif ? t.selection : Colors.transparent,
            border: Border.all(color: actif ? t.bleu : t.champBordure),
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              NovaIcone(mode.icone,
                  taille: 13, couleur: actif ? t.bleu : t.texte2),
              const SizedBox(width: 6),
              Text(
                mode.libelle,
                style: TextStyle(
                    fontSize: 11.5, color: actif ? t.bleu : t.texte2),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _colonneCePoste(NovaTokens t) {
    final idFormate = ref.watch(idLocalFormateProvider);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const NovaPanelHeader(NovaIcones.cast, 'Ce poste'),
        const SizedBox(height: 11),
        idFormate.when(
          data: (id) => SelectableText(
            id,
            style: TextStyle(
              fontSize: 24,
              fontWeight: FontWeight.w600,
              letterSpacing: 0.3,
              color: t.texte,
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
          loading: () => NovaSkeleton(largeur: 160, hauteur: 26),
          error: (e, _) => const Text('—'),
        ),
        const SizedBox(height: 3),
        Row(
          children: [
            NovaIcone(NovaIcones.tag, taille: 13, couleur: t.texte2),
            const SizedBox(width: 6),
            Text('poste-bureau-01@ad',
                style: TextStyle(fontSize: 12, color: t.texte2)),
          ],
        ),
        const SizedBox(height: 12),
        Wrap(
          spacing: 16,
          runSpacing: 10,
          children: [
            _lien(t, _idCopie ? NovaIcones.coche : NovaIcones.copier,
                _idCopie ? 'Copié' : 'Copier', () async {
              if (!idFormate.hasValue) return;
              await Clipboard.setData(
                  ClipboardData(text: idFormate.requireValue));
              if (!mounted) return;
              setState(() => _idCopie = true);
              NovaToast.montrer(context, 'Adresse copiée');
              _minuteurCopie?.cancel();
              _minuteurCopie = Timer(const Duration(milliseconds: 1100), () {
                if (mounted) setState(() => _idCopie = false);
              });
            }),
            _lien(t, NovaIcones.inviter, 'Inviter', _montrerInvitation),
            _lien(t, NovaIcones.cadenas, 'Accès non surveillé',
                () => naviguerVersVue(ref, context, NovaVue.nonSurveille)),
          ],
        ),
      ],
    );
  }

  Widget _lien(NovaTokens t, IconData icone, String libelle, VoidCallback onTap) {
    return _LienBleu(icone: icone, libelle: libelle, onTap: onTap);
  }

  // --- Onglets --------------------------------------------------------------

  Widget _barreOnglets() {
    final t = NovaTokens.of(context);
    final favoris = ref.watch(carnetProvider).where((e) => e.favori).length;
    return Container(
      decoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      padding: const EdgeInsets.fromLTRB(20, 9, 20, 0),
      child: Row(
        children: [
          _onglets(t, _OngletAccueil.recentes, 'Sessions récentes', null),
          _onglets(t, _OngletAccueil.favoris, 'Favoris', favoris),
          _onglets(t, _OngletAccueil.decouverts, 'Découverts', 0),
        ],
      ),
    );
  }

  Widget _onglets(NovaTokens t, _OngletAccueil onglet, String libelle, int? n) {
    final actif = _onglet == onglet;
    return GestureDetector(
      onTap: () => setState(() => _onglet = onglet),
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        child: Container(
          padding: const EdgeInsets.fromLTRB(13, 8, 13, 8),
          decoration: BoxDecoration(
            border: Border(
              bottom: BorderSide(
                width: 2,
                color: actif ? kNovaRouge : Colors.transparent,
              ),
            ),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                libelle,
                style: TextStyle(
                  fontSize: 12.5,
                  fontWeight: actif ? FontWeight.w600 : FontWeight.w400,
                  color: actif ? t.texte : t.texte2,
                ),
              ),
              if (n != null && n > 0) ...[
                const SizedBox(width: 7),
                Container(
                  padding:
                      const EdgeInsets.symmetric(horizontal: 6, vertical: 1),
                  decoration: BoxDecoration(
                    color: t.survol,
                    borderRadius: BorderRadius.circular(9),
                  ),
                  child: Text('$n',
                      style: TextStyle(fontSize: 11, color: t.texte3)),
                ),
              ],
            ],
          ),
        ),
      ),
    );
  }

  // --- Liste d'appareils ----------------------------------------------------

  Widget _liste() {
    final carnet = ref.watch(carnetProvider);
    if (_onglet == _OngletAccueil.decouverts) {
      return const NovaEmptyState(
        icone: NovaIcones.radar,
        titre: 'Aucun appareil découvert',
        sousTitre:
            'Aucun poste NovaDesk détecté sur votre réseau local.',
      );
    }
    if (_chargement) {
      return ListView(
        children: [for (var i = 0; i < 4; i++) const _LigneSquelette()],
      );
    }
    final entrees = _onglet == _OngletAccueil.favoris
        ? carnet.where((e) => e.favori).toList()
        : carnet;
    if (entrees.isEmpty) {
      return const NovaEmptyState(
        icone: NovaIcones.etoile,
        titre: 'Aucun favori',
        sousTitre: 'Ajoutez des favoris via le menu contextuel d’un appareil.',
      );
    }
    return ListView.builder(
      itemCount: entrees.length,
      itemBuilder: (context, i) => _LigneAppareil(
        entree: entrees[i],
        onConnecter: () => unawaited(_connecterEntree(entrees[i])),
        onFavori: () => _basculerFavori(entrees[i]),
        onMenu: (pos) => unawaited(_menuContextuel(entrees[i], pos)),
      ),
    );
  }

  // --- Modale d'invitation --------------------------------------------------

  void _montrerInvitation() {
    montrerDialogueNova<void>(
        context: context, builder: (context) => const _InviteDialog());
  }
}

// ===========================================================================
// Composants privés
// ===========================================================================

/// Lien bleu avec icône (maquette `.lnk`), souligné au survol.
class _LienBleu extends StatefulWidget {
  const _LienBleu(
      {required this.icone, required this.libelle, required this.onTap});

  final IconData icone;
  final String libelle;
  final VoidCallback onTap;

  @override
  State<_LienBleu> createState() => _LienBleuState();
}

class _LienBleuState extends State<_LienBleu> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIcone(widget.icone, taille: 13, couleur: t.bleu),
            const SizedBox(width: 6),
            Text(
              widget.libelle,
              style: TextStyle(
                fontSize: 12,
                color: t.bleu,
                decoration:
                    _survole ? TextDecoration.underline : TextDecoration.none,
                decorationColor: t.bleu,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Ligne d'appareil de la liste (maquette `.row`).
class _LigneAppareil extends ConsumerStatefulWidget {
  const _LigneAppareil({
    required this.entree,
    required this.onConnecter,
    required this.onFavori,
    required this.onMenu,
  });

  final EntreeCarnet entree;
  final VoidCallback onConnecter;
  final VoidCallback onFavori;
  final ValueChanged<Offset> onMenu;

  @override
  ConsumerState<_LigneAppareil> createState() => _LigneAppareilState();
}

class _LigneAppareilState extends ConsumerState<_LigneAppareil> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final e = widget.entree;
    final idFormate = ref.watch(idFormateProvider(e.id));
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onConnecter,
        onSecondaryTapDown: (d) => widget.onMenu(d.globalPosition),
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 8),
          decoration: BoxDecoration(
            color: _survole ? t.panneau : null,
            border: Border(bottom: BorderSide(color: t.filet)),
          ),
          child: Row(
            children: [
              _vignette(t, e),
              const SizedBox(width: 12),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(e.alias,
                        style: TextStyle(
                            fontSize: 13,
                            fontWeight: FontWeight.w500,
                            color: t.texte)),
                    Text(
                      idFormate.maybeWhen(data: (v) => v, orElse: () => '…'),
                      style: TextStyle(
                        fontSize: 11.5,
                        color: t.texte3,
                        fontFeatures: const [FontFeature.tabularFigures()],
                      ),
                    ),
                  ],
                ),
              ),
              const SizedBox(width: 12),
              Text(e.derniereConnexion,
                  style: TextStyle(fontSize: 11.5, color: t.texte3)),
              const SizedBox(width: 12),
              _etoile(t, e),
              const SizedBox(width: 4),
              // Actions révélées au survol.
              Opacity(
                opacity: _survole ? 1 : 0,
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    NovaBoutonAction(
                      icone: NovaIcones.flecheDroite,
                      accent: true,
                      infobulle: 'Se connecter',
                      onTap: _survole ? widget.onConnecter : null,
                    ),
                    Builder(
                      builder: (ctx) => NovaBoutonAction(
                        icone: NovaIcones.troisPoints,
                        onTap: _survole
                            ? () {
                                final box =
                                    ctx.findRenderObject() as RenderBox?;
                                final pos = box?.localToGlobal(
                                        box.size.center(Offset.zero)) ??
                                    Offset.zero;
                                widget.onMenu(pos);
                              }
                            : null,
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _vignette(NovaTokens t, EntreeCarnet e) {
    return Container(
      width: 40,
      height: 28,
      decoration: BoxDecoration(
        color: t.vignette1,
        borderRadius: BorderRadius.circular(3),
        border: Border.all(color: t.champBordure),
      ),
      child: Stack(
        children: [
          Positioned(
            left: 5,
            right: 5,
            top: 4,
            bottom: 6,
            child: DecoratedBox(
              decoration: BoxDecoration(
                color: Colors.black.withValues(alpha: 0.10),
                borderRadius: BorderRadius.circular(1),
              ),
            ),
          ),
          Positioned(
            top: 2,
            right: 2,
            child: Container(
              width: 8,
              height: 8,
              decoration: BoxDecoration(
                color: e.enLigne ? t.vert : t.texte3,
                shape: BoxShape.circle,
                border: Border.all(color: t.fenetre, width: 1.5),
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _etoile(NovaTokens t, EntreeCarnet e) {
    final visible = e.favori || _survole;
    return Opacity(
      opacity: visible ? (e.favori ? 1 : 0.6) : 0,
      child: IgnorePointer(
        ignoring: !visible,
        child: NovaBoutonAction(
          icone: NovaIcones.etoile,
          tailleIcone: 15,
          taille: 24,
          couleurActive: e.favori ? t.ambre : null,
          onTap: widget.onFavori,
        ),
      ),
    );
  }
}

/// Ligne de squelette de chargement (maquette `.skrow`).
class _LigneSquelette extends StatelessWidget {
  const _LigneSquelette();

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 9),
      decoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          const NovaSkeleton(largeur: 40, hauteur: 28),
          const SizedBox(width: 12),
          Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: const [
              NovaSkeleton(largeur: 130, hauteur: 11),
              SizedBox(height: 7),
              NovaSkeleton(largeur: 82, hauteur: 9),
            ],
          ),
          const Spacer(),
          const NovaSkeleton(largeur: 52, hauteur: 9),
        ],
      ),
    );
  }
}

/// Formateur du champ adresse : chiffres regroupés par 3, alias littéral laissé
/// libre.
class _FormateurAdresse extends TextInputFormatter {
  const _FormateurAdresse();

  static const _formateurId = NovaIdInputFormatter();

  @override
  TextEditingValue formatEditUpdate(
    TextEditingValue oldValue,
    TextEditingValue newValue,
  ) {
    if (RegExp(r'[^\d\s]').hasMatch(newValue.text)) {
      return newValue;
    }
    return _formateurId.formatEditUpdate(oldValue, newValue);
  }
}

/// Modale « Inviter à se connecter » (maquette `.modal.invite`).
class _InviteDialog extends StatelessWidget {
  const _InviteDialog();

  static const String _lien = 'https://novadesk.io/i/9x7-Kd2-mQ';

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Dialog(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 400),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 17, 20, 17),
              child: Row(
                children: [
                  Container(
                    width: 42,
                    height: 42,
                    alignment: Alignment.center,
                    decoration: BoxDecoration(
                      color: kNovaRouge,
                      borderRadius: BorderRadius.circular(kNovaRayon),
                    ),
                    child: const NovaIcone(NovaIcones.inviter,
                        taille: 20, couleur: Colors.white),
                  ),
                  const SizedBox(width: 13),
                  Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text('Inviter à se connecter',
                          style: TextStyle(
                              fontSize: 15,
                              fontWeight: FontWeight.w600,
                              color: t.texte)),
                      const SizedBox(height: 1),
                      Text('Lien à usage unique',
                          style: TextStyle(fontSize: 12, color: t.texte3)),
                    ],
                  ),
                ],
              ),
            ),
            Divider(height: 1, color: t.filet),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 16, 20, 16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const NovaSectionLabel("Lien d'invitation"),
                  const SizedBox(height: 6),
                  Row(
                    children: [
                      Expanded(
                        child: Container(
                          height: 34,
                          padding: const EdgeInsets.symmetric(horizontal: 10),
                          alignment: Alignment.centerLeft,
                          decoration: BoxDecoration(
                            color: t.panneau,
                            borderRadius: BorderRadius.circular(kNovaRayon),
                            border: Border.all(color: t.champBordure),
                          ),
                          child: Text(_lien,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(fontSize: 12, color: t.texte)),
                        ),
                      ),
                      const SizedBox(width: 8),
                      NovaBoutonPrimaire(
                        libelle: 'Copier',
                        onPressed: () async {
                          await Clipboard.setData(
                              const ClipboardData(text: _lien));
                          if (context.mounted) {
                            NovaToast.montrer(context, "Lien d'invitation copié");
                          }
                        },
                      ),
                    ],
                  ),
                  const SizedBox(height: 12),
                  Text(
                    'Expire dans 10 min · une seule connexion · profil '
                    '« Observation ».',
                    style: TextStyle(fontSize: 11.5, color: t.texte3),
                  ),
                ],
              ),
            ),
            Divider(height: 1, color: t.filet),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 14, 20, 14),
              child: Center(
                child: NovaBoutonSecondaire(
                  libelle: 'Fermer',
                  hauteur: 38,
                  onPressed: () => Navigator.of(context).pop(),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}
