/// Écran d'accueil (plan 10 §10.4.1) : mon ID + mot de passe éphémère,
/// saisie de l'ID distant avec choix du mode, sessions récentes / carnet.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../widgets/nova_button.dart';
import '../widgets/nova_id_field.dart';
import 'session_screen.dart';
import 'settings_screen.dart';

/// Mode de connexion proposé sur l'accueil, traduit en [PermissionsDto]
/// avant l'appel à `new_session_config` (façade `nd-ffi`).
enum ModeConnexion { controle, observation, transfertSeul }

extension _ModeConnexionX on ModeConnexion {
  String get libelle => switch (this) {
        ModeConnexion.controle => 'Contrôle',
        ModeConnexion.observation => 'Observation',
        ModeConnexion.transfertSeul => 'Transfert seul',
      };

  IconData get icone => switch (this) {
        ModeConnexion.controle => Icons.mouse_outlined,
        ModeConnexion.observation => Icons.visibility_outlined,
        ModeConnexion.transfertSeul => Icons.folder_copy_outlined,
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
  final TextEditingController _idDistantController = TextEditingController();
  ModeConnexion _mode = ModeConnexion.controle;
  bool _connexionEnCours = false;
  bool _motDePasseVisible = false;

  @override
  void dispose() {
    _idDistantController.dispose();
    super.dispose();
  }

  /// Valide la saisie via la façade (`parse_nova_id` + `new_session_config`)
  /// puis ouvre la fenêtre de session. Les erreurs de la façade sont des
  /// messages français prêts à afficher.
  Future<void> _seConnecter() async {
    final api = ref.read(nativeApiProvider);
    final idLocal = ref.read(idLocalProvider);
    setState(() => _connexionEnCours = true);
    try {
      final idPair = await api.parseNovaId(texte: _idDistantController.text);
      final config = await api.newSessionConfig(
        role: SessionRoleDto.controller,
        localId: idLocal,
        peerId: idPair,
        permissions: _mode.permissions,
      );
      final idFormate = await api.formatNovaId(id: idPair);
      final alias = ref
          .read(carnetProvider)
          .where((e) => e.id == idPair)
          .map((e) => e.alias)
          .firstOrNull;
      if (!mounted) return;
      await Navigator.of(context).pushNamed(
        SessionScreen.route,
        arguments: SessionScreenArgs(
          config: config,
          libellePair: alias ?? idFormate,
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

  /// Préremplit le champ ID depuis une entrée du carnet.
  Future<void> _preremplir(EntreeCarnet entree) async {
    final idFormate =
        await ref.read(nativeApiProvider).formatNovaId(id: entree.id);
    _idDistantController.text = idFormate;
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(Icons.screen_share_outlined),
            SizedBox(width: 10),
            Text('NovaDesk'),
          ],
        ),
        actions: [
          IconButton(
            tooltip: 'Paramètres',
            icon: const Icon(Icons.settings_outlined),
            onPressed: () =>
                Navigator.of(context).pushNamed(SettingsScreen.route),
          ),
          const SizedBox(width: 4),
        ],
      ),
      body: LayoutBuilder(
        builder: (context, contraintes) {
          final large = contraintes.maxWidth >= 920;
          if (large) {
            return SingleChildScrollView(
              padding: const EdgeInsets.all(24),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Expanded(
                    flex: 5,
                    child: Column(
                      children: [
                        _carteCePoste(),
                        const SizedBox(height: 16),
                        _carteConnexion(),
                      ],
                    ),
                  ),
                  const SizedBox(width: 16),
                  Expanded(flex: 4, child: _carteCarnet()),
                ],
              ),
            );
          }
          return ListView(
            padding: const EdgeInsets.all(16),
            children: [
              _carteCePoste(),
              const SizedBox(height: 16),
              _carteConnexion(),
              const SizedBox(height: 16),
              _carteCarnet(),
            ],
          );
        },
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Carte « Ce poste » : mon ID, mot de passe éphémère, alias.
  // -------------------------------------------------------------------------
  Widget _carteCePoste() {
    final theme = Theme.of(context);
    final idFormate = ref.watch(idLocalFormateProvider);
    final motDePasse = ref.watch(motDePasseEphemereProvider);

    return Card(
      child: Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Ce poste', style: theme.textTheme.titleMedium),
            const SizedBox(height: 16),
            Text('Votre ID', style: theme.textTheme.labelMedium),
            Row(
              children: [
                Expanded(
                  child: idFormate.when(
                    data: (id) => SelectableText(
                      id,
                      style: theme.textTheme.headlineMedium?.copyWith(
                        fontFeatures: const [FontFeature.tabularFigures()],
                        letterSpacing: 2,
                      ),
                    ),
                    loading: () => const LinearProgressIndicator(),
                    error: (e, _) => const Text('—'),
                  ),
                ),
                IconButton(
                  tooltip: "Copier l'ID",
                  icon: const Icon(Icons.copy_outlined),
                  onPressed: idFormate.hasValue
                      ? () async {
                          await Clipboard.setData(
                            ClipboardData(text: idFormate.requireValue),
                          );
                          if (!mounted) return;
                          ScaffoldMessenger.of(context).showSnackBar(
                            const SnackBar(
                              content: Text('ID copié dans le presse-papiers.'),
                            ),
                          );
                        }
                      : null,
                ),
              ],
            ),
            const SizedBox(height: 8),
            Text('Mot de passe éphémère', style: theme.textTheme.labelMedium),
            Row(
              children: [
                Expanded(
                  child: Text(
                    _motDePasseVisible ? motDePasse : '● ● ● ● ● ●',
                    style: theme.textTheme.titleMedium,
                  ),
                ),
                IconButton(
                  tooltip: _motDePasseVisible ? 'Masquer' : 'Afficher',
                  icon: Icon(
                    _motDePasseVisible
                        ? Icons.visibility_off_outlined
                        : Icons.visibility_outlined,
                  ),
                  onPressed: () => setState(
                    () => _motDePasseVisible = !_motDePasseVisible,
                  ),
                ),
                IconButton(
                  tooltip: 'Régénérer le mot de passe',
                  icon: const Icon(Icons.refresh),
                  onPressed: () {
                    ref.read(motDePasseEphemereProvider.notifier).state =
                        genererMotDePasse(10);
                  },
                ),
              ],
            ),
            const SizedBox(height: 8),
            Row(
              children: [
                Icon(
                  Icons.badge_outlined,
                  size: 18,
                  color: theme.colorScheme.outline,
                ),
                const SizedBox(width: 6),
                Text('Alias : poste-atelier',
                    style: theme.textTheme.bodyMedium),
                const Spacer(),
                Icon(Icons.circle, size: 10, color: Colors.green.shade600),
                const SizedBox(width: 6),
                Text('En ligne — relais eu-w-3',
                    style: theme.textTheme.bodySmall),
              ],
            ),
          ],
        ),
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Carte « Se connecter » : ID distant + mode + bouton.
  // -------------------------------------------------------------------------
  Widget _carteConnexion() {
    final theme = Theme.of(context);
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              'Se connecter à un poste distant',
              style: theme.textTheme.titleMedium,
            ),
            const SizedBox(height: 16),
            NovaIdField(
              controller: _idDistantController,
              libelle: "Entrez l'ID distant",
              onSubmitted: (_) => _seConnecter(),
            ),
            const SizedBox(height: 16),
            SegmentedButton<ModeConnexion>(
              segments: [
                for (final mode in ModeConnexion.values)
                  ButtonSegment(
                    value: mode,
                    label: Text(mode.libelle),
                    icon: Icon(mode.icone),
                  ),
              ],
              selected: {_mode},
              onSelectionChanged: (selection) =>
                  setState(() => _mode = selection.first),
            ),
            const SizedBox(height: 16),
            Align(
              alignment: Alignment.centerRight,
              child: NovaButton(
                libelle: 'Se connecter',
                icone: Icons.link,
                enCours: _connexionEnCours,
                onPressed: _seConnecter,
              ),
            ),
          ],
        ),
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Carte « Sessions récentes & carnet ».
  // -------------------------------------------------------------------------
  Widget _carteCarnet() {
    final theme = Theme.of(context);
    final carnet = ref.watch(carnetProvider);
    return Card(
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 8, 20, 8),
              child: Text(
                'Sessions récentes & carnet',
                style: theme.textTheme.titleMedium,
              ),
            ),
            for (final entree in carnet)
              ListTile(
                leading: Icon(
                  entree.favori ? Icons.star : Icons.star_border,
                  color: entree.favori
                      ? Colors.amber.shade700
                      : theme.colorScheme.outline,
                ),
                title: Text(entree.alias),
                subtitle: ref.watch(idFormateProvider(entree.id)).when(
                      data: (id) => Text(id),
                      loading: () => const Text('…'),
                      error: (e, _) => const Text('—'),
                    ),
                trailing: Text(
                  entree.derniereConnexion,
                  style: theme.textTheme.bodySmall,
                ),
                onTap: () => _preremplir(entree),
              ),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 8, 20, 4),
              child: Text(
                'Cliquez sur une entrée pour préremplir l’ID distant.',
                style: theme.textTheme.bodySmall?.copyWith(
                  color: theme.colorScheme.outline,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}
