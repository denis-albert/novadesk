/// Écran d'accueil — fidèle à la maquette `anydesk-reference.html` :
/// deux colonnes séparées par un filet 1px. À gauche « Poste distant »
/// (grand champ d'adresse + bouton rouge « Se connecter » + vignettes des
/// sessions récentes) ; à droite « Ce poste » (adresse à 9 chiffres en gros,
/// Copier/Partager, alias, accès non surveillé, note).
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/app_frame.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_id_field.dart';
import '../widgets/session_thumbnail.dart';
import 'session_screen.dart';
import 'unattended_screen.dart';

/// Mode de connexion, traduit en [PermissionsDto] avant
/// `new_session_config` (façade `nd-ffi`). Présenté dans un menu discret
/// sous le champ d'adresse (AnyDesk le règle côté acceptation, doc 03 §5.2).
enum ModeConnexion { controle, observation, transfertSeul }

extension _ModeConnexionX on ModeConnexion {
  String get libelle => switch (this) {
        ModeConnexion.controle => 'Contrôle',
        ModeConnexion.observation => 'Observation',
        ModeConnexion.transfertSeul => 'Transfert seul',
      };

  PermissionsDto get permissions => switch (this) {
        ModeConnexion.controle => PermissionsDto.full(),
        ModeConnexion.observation => PermissionsDto.viewOnly(),
        // Transfert seul : aucun contrôle à distance, uniquement les fichiers.
        ModeConnexion.transfertSeul => const PermissionsDto(
            keyboard: false,
            mouse: false,
            clipboard: false,
            files: true,
            audio: false,
            viewOnly: true,
          ),
      };
}

class HomeScreen extends ConsumerStatefulWidget {
  const HomeScreen({super.key});

  static const String route = '/';

  @override
  ConsumerState<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends ConsumerState<HomeScreen> {
  final TextEditingController _adresseController = TextEditingController();
  final FocusNode _adresseFocus = FocusNode(debugLabel: 'champ-adresse');

  ModeConnexion _mode = ModeConnexion.controle;
  bool _connexionEnCours = false;
  bool _adresseEnFocus = false;
  bool _filtreFavoris = false;
  bool _accesNonSurveille = true;
  bool _idCopie = false;
  Timer? _minuteurCopie;

  @override
  void initState() {
    super.initState();
    _adresseFocus.addListener(() {
      setState(() => _adresseEnFocus = _adresseFocus.hasFocus);
    });
  }

  @override
  void dispose() {
    _minuteurCopie?.cancel();
    _adresseController.dispose();
    _adresseFocus.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Connexion
  // ---------------------------------------------------------------------------

  /// Valide la saisie via la façade (`parse_nova_id` + `new_session_config`)
  /// puis ouvre la fenêtre de session. Un alias du carnet est accepté à la
  /// place de l'adresse (« Adresse à 9 chiffres, ou un alias enregistré »).
  Future<void> _seConnecter() async {
    final api = ref.read(nativeApiProvider);
    final idLocal = ref.read(idLocalProvider);
    final saisie = _adresseController.text.trim();
    final carnet = ref.read(carnetProvider);

    setState(() => _connexionEnCours = true);
    try {
      // Alias enregistré ? Sinon, l'adresse chiffrée fait foi.
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
      // Connexion **par ID** : mise en relation via le serveur de rendez-vous
      // (STUN → hole punching → QUIC), adresses issues des réglages réseau.
      final endpoint = SessionEndpointByRendezvous(
        server: ref.read(rendezvousProvider),
        stunServers: ref.read(stunServersProvider),
        relay: ref.read(relayProvider),
      );
      // Les permissions du mode retenu deviennent les permissions granulaires
      // de la session (démarrage via `start_session_with_options`).
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
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text(e.message)),
      );
    } finally {
      if (mounted) {
        setState(() => _connexionEnCours = false);
      }
    }
  }

  /// Clic sur une vignette : préremplit l'adresse puis lance la connexion.
  Future<void> _connecterEntree(EntreeCarnet entree) async {
    final idFormate =
        await ref.read(nativeApiProvider).formatNovaId(id: entree.id);
    _adresseController.text = idFormate;
    await _seConnecter();
  }

  // ---------------------------------------------------------------------------
  // Actions des vignettes (état local du carnet fictif)
  // ---------------------------------------------------------------------------

  void _surActionVignette(EntreeCarnet entree, ActionVignette action) {
    final carnet = ref.read(carnetProvider.notifier);
    switch (action) {
      case ActionVignette.connecter:
        unawaited(_connecterEntree(entree));
      case ActionVignette.favori:
        carnet.state = [
          for (final e in carnet.state)
            e.id == entree.id ? e.copyWith(favori: !e.favori) : e,
        ];
      case ActionVignette.renommer:
        unawaited(_renommer(entree));
      case ActionVignette.supprimer:
        carnet.state =
            carnet.state.where((e) => e.id != entree.id).toList();
    }
  }

  Future<void> _renommer(EntreeCarnet entree) async {
    final controller = TextEditingController(text: entree.alias);
    final nouvelAlias = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Renommer'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(labelText: 'Alias'),
          onSubmitted: (valeur) => Navigator.of(context).pop(valeur),
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
    final carnet = ref.read(carnetProvider.notifier);
    carnet.state = [
      for (final e in carnet.state)
        e.id == entree.id ? e.copyWith(alias: alias) : e,
    ];
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Scaffold(
      body: NovaAppFrame(
        corps: LayoutBuilder(
          builder: (context, contraintes) {
            final large = contraintes.maxWidth >= 780;
            if (large) {
              return Row(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Expanded(
                    child: SingleChildScrollView(
                      padding: const EdgeInsets.all(30),
                      child: _colonnePosteDistant(t),
                    ),
                  ),
                  Container(width: 1, color: t.filet),
                  SizedBox(
                    width: 320,
                    child: SingleChildScrollView(
                      padding: const EdgeInsets.symmetric(
                          horizontal: 28, vertical: 30),
                      child: _colonneCePoste(t),
                    ),
                  ),
                ],
              );
            }
            // Repli étroit : une seule colonne, sections séparées d'un filet.
            return ListView(
              padding: const EdgeInsets.all(20),
              children: [
                _colonnePosteDistant(t),
                const SizedBox(height: 24),
                Divider(color: t.filet),
                const SizedBox(height: 24),
                _colonneCePoste(t),
              ],
            );
          },
        ),
      ),
    );
  }

  Text _label(NovaTokens t, String texte) {
    return Text(
      texte,
      style: TextStyle(
        fontSize: 11,
        fontWeight: FontWeight.w700,
        letterSpacing: 1.1,
        color: t.texte3,
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Colonne gauche : « Poste distant »
  // ---------------------------------------------------------------------------

  Widget _colonnePosteDistant(NovaTokens t) {
    final carnet = ref.watch(carnetProvider);
    final entrees =
        _filtreFavoris ? carnet.where((e) => e.favori).toList() : carnet;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        _label(t, 'POSTE DISTANT'),
        const SizedBox(height: 14),
        _barreAdresse(t),
        const SizedBox(height: 10),
        _ligneAstuceEtMode(t),
        const SizedBox(height: 34),
        Row(
          children: [
            Expanded(
              child: Wrap(
                spacing: 16,
                runSpacing: 4,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  _ongletSection(t, 'SESSIONS RÉCENTES', !_filtreFavoris,
                      () => setState(() => _filtreFavoris = false)),
                  _ongletSection(t, 'FAVORIS', _filtreFavoris,
                      () => setState(() => _filtreFavoris = true)),
                ],
              ),
            ),
            const SizedBox(width: 12),
            _lien(t, 'Tout afficher', () {
              ScaffoldMessenger.of(context).showSnackBar(
                const SnackBar(
                    content: Text('Historique complet — à venir.')),
              );
            }),
          ],
        ),
        const SizedBox(height: 14),
        if (entrees.isEmpty)
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 24),
            child: Text(
              _filtreFavoris
                  ? 'Aucun favori pour l’instant — étoile via le menu ⋯ '
                      'd’une vignette.'
                  : 'Aucune session récente.',
              style: TextStyle(fontSize: 12, color: t.texte3),
            ),
          )
        else
          GridView.builder(
            shrinkWrap: true,
            physics: const NeverScrollableScrollPhysics(),
            gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
              maxCrossAxisExtent: 190,
              mainAxisExtent: 138,
              mainAxisSpacing: 14,
              crossAxisSpacing: 14,
            ),
            itemCount: entrees.length,
            itemBuilder: (context, index) {
              final entree = entrees[index];
              return SessionThumbnail(
                entree: entree,
                onConnecter: () => unawaited(_connecterEntree(entree)),
                onAction: (action) => _surActionVignette(entree, action),
              );
            },
          ),
      ],
    );
  }

  /// Grand champ d'adresse (46px) + bouton rouge « Se connecter » (maquette
  /// `.addrbar`) : fond champ, bordure 1px, liseré rouge au focus.
  Widget _barreAdresse(NovaTokens t) {
    return Row(
      children: [
        Expanded(
          child: AnimatedContainer(
            duration: const Duration(milliseconds: 120),
            height: 46,
            padding: const EdgeInsets.symmetric(horizontal: 14),
            decoration: BoxDecoration(
              color: _adresseEnFocus ? t.fenetre : t.champ,
              borderRadius: BorderRadius.circular(8),
              border: Border.all(
                color: _adresseEnFocus ? kNovaRouge : t.champBordure,
              ),
            ),
            child: Row(
              children: [
                NovaIcone(NovaIcones.adresse, taille: 18, couleur: t.texte3),
                const SizedBox(width: 10),
                Expanded(
                  child: TextField(
                    controller: _adresseController,
                    focusNode: _adresseFocus,
                    inputFormatters: const [_FormateurAdresse()],
                    style: TextStyle(
                      fontSize: 16,
                      letterSpacing: 0.5,
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
                      hintStyle: TextStyle(
                        fontSize: 14,
                        letterSpacing: 0,
                        color: t.texte3,
                      ),
                    ),
                    onSubmitted: (_) => unawaited(_seConnecter()),
                  ),
                ),
              ],
            ),
          ),
        ),
        const SizedBox(width: 10),
        SizedBox(
          height: 46,
          child: FilledButton(
            onPressed: _connexionEnCours ? null : _seConnecter,
            // SEUL bouton rouge de l'application (usage réservé, maquette
            // `.btn-red`) : rouge marque, enfoncé/survolé plus sombre.
            style: ButtonStyle(
              backgroundColor: WidgetStateProperty.resolveWith(
                (etats) => etats.contains(WidgetState.disabled)
                    ? t.champBordure
                    : etats.contains(WidgetState.hovered) ||
                            etats.contains(WidgetState.pressed)
                        ? kNovaRougePresse
                        : kNovaRouge,
              ),
              foregroundColor: const WidgetStatePropertyAll(Colors.white),
              padding: const WidgetStatePropertyAll(
                EdgeInsets.symmetric(horizontal: 18),
              ),
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Text('Se connecter'),
                const SizedBox(width: 8),
                if (_connexionEnCours)
                  const SizedBox(
                    width: 15,
                    height: 15,
                    child: CircularProgressIndicator(
                      strokeWidth: 2,
                      color: Colors.white,
                    ),
                  )
                else
                  const NovaIcone(NovaIcones.flecheDroite,
                      taille: 17, couleur: Colors.white),
              ],
            ),
          ),
        ),
      ],
    );
  }

  /// Astuce sous le champ + menu discret du mode de connexion.
  Widget _ligneAstuceEtMode(NovaTokens t) {
    return Row(
      children: [
        NovaIcone(NovaIcones.info, taille: 14, couleur: t.texte3),
        const SizedBox(width: 6),
        Expanded(
          child: Text(
            'Adresse à 9 chiffres, ou un alias enregistré.',
            style: TextStyle(fontSize: 12, color: t.texte3),
          ),
        ),
        PopupMenuButton<ModeConnexion>(
          tooltip: 'Mode de connexion',
          initialValue: _mode,
          onSelected: (mode) => setState(() => _mode = mode),
          itemBuilder: (context) => [
            for (final mode in ModeConnexion.values)
              PopupMenuItem(
                value: mode,
                height: 34,
                child: Text(mode.libelle),
              ),
          ],
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                'Mode : ${_mode.libelle}',
                style: TextStyle(fontSize: 12, color: t.texte2),
              ),
              const SizedBox(width: 3),
              NovaIcone(NovaIcones.chevronBas, taille: 12, couleur: t.texte3),
            ],
          ),
        ),
      ],
    );
  }

  Widget _ongletSection(
      NovaTokens t, String libelle, bool actif, VoidCallback onTap) {
    return InkWell(
      onTap: onTap,
      child: Text(
        libelle,
        style: TextStyle(
          fontSize: 11,
          fontWeight: FontWeight.w700,
          letterSpacing: 1.1,
          color: actif ? t.texte2 : t.texte3.withValues(alpha: 0.75),
        ),
      ),
    );
  }

  Widget _lien(NovaTokens t, String libelle, VoidCallback onTap) {
    return InkWell(
      onTap: onTap,
      child: Text(
        libelle,
        style: TextStyle(fontSize: 12, color: t.texte2),
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Colonne droite : « Ce poste »
  // ---------------------------------------------------------------------------

  Widget _colonneCePoste(NovaTokens t) {
    final idFormate = ref.watch(idLocalFormateProvider);

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        _label(t, 'CE POSTE'),
        const SizedBox(height: 14),
        idFormate.when(
          data: (id) => SelectableText(
            id,
            style: TextStyle(
              fontSize: 29,
              fontWeight: FontWeight.w700,
              letterSpacing: 1,
              color: t.texte,
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
          loading: () => const SizedBox(
            height: 40,
            width: 40,
            child: Padding(
              padding: EdgeInsets.all(8),
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
          ),
          error: (e, _) => const Text('—'),
        ),
        const SizedBox(height: 8),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            _BoutonFantome(
              icone: _idCopie ? NovaIcones.coche : NovaIcones.copier,
              libelle: _idCopie ? 'Copié' : 'Copier',
              onTap: idFormate.hasValue
                  ? () async {
                      await Clipboard.setData(
                        ClipboardData(text: idFormate.requireValue),
                      );
                      if (!mounted) return;
                      setState(() => _idCopie = true);
                      _minuteurCopie?.cancel();
                      _minuteurCopie =
                          Timer(const Duration(milliseconds: 1100), () {
                        if (mounted) setState(() => _idCopie = false);
                      });
                    }
                  : null,
            ),
            _BoutonFantome(
              icone: NovaIcones.partager,
              libelle: 'Partager',
              onTap: () {
                ScaffoldMessenger.of(context).showSnackBar(
                  const SnackBar(
                      content: Text("Partage de l'adresse — à venir.")),
                );
              },
            ),
          ],
        ),
        const SizedBox(height: 20),
        Row(
          children: [
            NovaIcone(NovaIcones.moniteur, taille: 16, couleur: t.texte3),
            const SizedBox(width: 9),
            Text(
              'POSTE-BUREAU-01',
              style: TextStyle(fontSize: 13, color: t.texte2),
            ),
          ],
        ),
        Padding(
          padding: const EdgeInsets.symmetric(vertical: 20),
          child: Divider(color: t.filet),
        ),
        Row(
          children: [
            Expanded(
              child: InkWell(
                onTap: () => Navigator.of(context)
                    .pushNamed(UnattendedScreen.route),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      'Accès non surveillé',
                      style: TextStyle(
                        fontSize: 13,
                        fontWeight: FontWeight.w600,
                        color: t.texte,
                      ),
                    ),
                    const SizedBox(height: 1),
                    Text(
                      'Contrôle sans validation manuelle',
                      style: TextStyle(fontSize: 11.5, color: t.texte3),
                    ),
                  ],
                ),
              ),
            ),
            const SizedBox(width: 12),
            _InterrupteurNova(
              actif: _accesNonSurveille,
              onChanged: (valeur) =>
                  setState(() => _accesNonSurveille = valeur),
            ),
          ],
        ),
        const SizedBox(height: 22),
        Text(
          'Votre adresse identifie ce poste sur le réseau NovaDesk. '
          'Communiquez-la à la personne qui doit vous contrôler. '
          'Toutes les sessions sont chiffrées de bout en bout.',
          style: TextStyle(fontSize: 12, height: 1.55, color: t.texte3),
        ),
      ],
    );
  }
}

// ---------------------------------------------------------------------------
// Petits composants fidèles à la maquette
// ---------------------------------------------------------------------------

/// Formateur du champ adresse : chiffres regroupés par 3 (comme
/// [NovaIdInputFormatter]) mais laisse passer un **alias** littéral.
class _FormateurAdresse extends TextInputFormatter {
  const _FormateurAdresse();

  static const _formateurId = NovaIdInputFormatter();

  @override
  TextEditingValue formatEditUpdate(
    TextEditingValue oldValue,
    TextEditingValue newValue,
  ) {
    if (RegExp(r'[^\d\s]').hasMatch(newValue.text)) {
      return newValue; // alias : saisie libre
    }
    return _formateurId.formatEditUpdate(oldValue, newValue);
  }
}

/// Bouton « fantôme » 30px (maquette `.ghost`) : bordure filet, texte
/// secondaire, survol discret.
class _BoutonFantome extends StatefulWidget {
  const _BoutonFantome({
    required this.icone,
    required this.libelle,
    this.onTap,
  });

  final NovaIconeData icone;
  final String libelle;
  final VoidCallback? onTap;

  @override
  State<_BoutonFantome> createState() => _BoutonFantomeState();
}

class _BoutonFantomeState extends State<_BoutonFantome> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleur = _survole ? t.texte : t.texte2;
    return MouseRegion(
      cursor: widget.onTap == null
          ? MouseCursor.defer
          : SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        child: Container(
          height: 30,
          padding: const EdgeInsets.symmetric(horizontal: 10),
          decoration: BoxDecoration(
            color: _survole ? t.survol : Colors.transparent,
            border: Border.all(color: t.filet),
            borderRadius: BorderRadius.circular(7),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              NovaIcone(widget.icone, taille: 14, couleur: couleur),
              const SizedBox(width: 6),
              Text(
                widget.libelle,
                style: TextStyle(fontSize: 12, color: couleur),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Interrupteur compact 38×22 (maquette `.switch`) : vert = accordé.
class _InterrupteurNova extends StatelessWidget {
  const _InterrupteurNova({required this.actif, required this.onChanged});

  final bool actif;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Semantics(
      toggled: actif,
      button: true,
      label: 'Accès non surveillé',
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        child: GestureDetector(
          onTap: () => onChanged(!actif),
          child: AnimatedContainer(
            duration: const Duration(milliseconds: 140),
            width: 38,
            height: 22,
            padding: const EdgeInsets.all(2),
            alignment:
                actif ? Alignment.centerRight : Alignment.centerLeft,
            decoration: BoxDecoration(
              color: actif ? kNovaVert : t.champBordure,
              borderRadius: BorderRadius.circular(11),
            ),
            child: Container(
              width: 18,
              height: 18,
              decoration: BoxDecoration(
                color: Colors.white,
                shape: BoxShape.circle,
                boxShadow: [
                  BoxShadow(
                    color: Colors.black.withValues(alpha: 0.35),
                    blurRadius: 2,
                    offset: const Offset(0, 1),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
