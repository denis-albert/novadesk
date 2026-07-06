/// Écran des paramètres (plan 10 §10.4.4) : Interface, Réseau, Sécurité,
/// À propos. La persistance réelle appartiendra au cœur Rust (source de
/// vérité, fichier chiffré — plans 06/11) ; l'UI lira/écrira via la façade.
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../state/providers.dart';
import 'unattended_screen.dart';

class SettingsScreen extends ConsumerStatefulWidget {
  const SettingsScreen({super.key});

  static const String route = '/parametres';

  @override
  ConsumerState<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends ConsumerState<SettingsScreen> {
  // Réglages fictifs, en mémoire : seront lus/écrits via la façade `nd-ffi`
  // quand le cœur exposera son magasin de configuration.
  bool _confirmationRequise = true;
  bool _listeBlancheSeule = false;
  bool _verrouillerEnFin = false;
  String _modeReseau = 'Automatique';

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final modeTheme = ref.watch(themeModeProvider);
    final appInfo = ref.watch(appInfoProvider);

    return Scaffold(
      appBar: AppBar(title: const Text('Paramètres')),
      body: ListView(
        padding: const EdgeInsets.symmetric(vertical: 8),
        children: [
          _titreSection(theme, 'Interface'),
          ListTile(
            leading: const Icon(Icons.brightness_6_outlined),
            title: const Text('Thème'),
            subtitle: const Text('Clair, sombre ou selon le système'),
            trailing: SegmentedButton<ThemeMode>(
              showSelectedIcon: false,
              segments: const [
                ButtonSegment(
                  value: ThemeMode.system,
                  label: Text('Système'),
                ),
                ButtonSegment(value: ThemeMode.light, label: Text('Clair')),
                ButtonSegment(value: ThemeMode.dark, label: Text('Sombre')),
              ],
              selected: {modeTheme},
              onSelectionChanged: (selection) =>
                  ref.read(themeModeProvider.notifier).state = selection.first,
            ),
          ),
          ListTile(
            leading: const Icon(Icons.language_outlined),
            title: const Text('Langue'),
            subtitle:
                const Text('Catalogues ARB multilingues à venir (plan 10 §10.7.2)'),
            trailing: DropdownButton<String>(
              value: 'fr',
              items: const [
                DropdownMenuItem(value: 'fr', child: Text('Français')),
              ],
              onChanged: (_) {},
            ),
          ),
          const Divider(),
          _titreSection(theme, 'Réseau'),
          ListTile(
            leading: const Icon(Icons.lan_outlined),
            title: const Text('Mode de connexion'),
            subtitle: const Text('P2P direct quand possible, sinon relais'),
            trailing: DropdownButton<String>(
              value: _modeReseau,
              items: const [
                DropdownMenuItem(
                    value: 'Automatique', child: Text('Automatique')),
                DropdownMenuItem(value: 'P2P direct', child: Text('P2P direct')),
                DropdownMenuItem(
                    value: 'Relais uniquement', child: Text('Relais uniquement')),
              ],
              onChanged: (valeur) =>
                  setState(() => _modeReseau = valeur ?? _modeReseau),
            ),
          ),
          const ListTile(
            leading: Icon(Icons.speed_outlined),
            title: Text('Limite de bande passante'),
            subtitle: Text('Illimitée'),
            enabled: false,
          ),
          const Divider(),
          _titreSection(theme, 'Sécurité'),
          SwitchListTile(
            secondary: const Icon(Icons.verified_user_outlined),
            title: const Text("Demander confirmation à l'utilisateur"),
            subtitle: const Text(
                'Chaque connexion entrante requiert une autorisation explicite'),
            value: _confirmationRequise,
            onChanged: (valeur) =>
                setState(() => _confirmationRequise = valeur),
          ),
          SwitchListTile(
            secondary: const Icon(Icons.playlist_add_check),
            title: const Text("Liste blanche d'IDs uniquement"),
            subtitle:
                const Text('Refuser tout poste absent du carnet de confiance'),
            value: _listeBlancheSeule,
            onChanged: (valeur) => setState(() => _listeBlancheSeule = valeur),
          ),
          SwitchListTile(
            secondary: const Icon(Icons.lock_outline),
            title: const Text("Verrouiller l'écran en fin de session"),
            value: _verrouillerEnFin,
            onChanged: (valeur) => setState(() => _verrouillerEnFin = valeur),
          ),
          ListTile(
            leading: const Icon(Icons.shield_outlined),
            title: const Text('Accès non-surveillé'),
            subtitle: const Text(
                'Mot de passe permanent, appareils de confiance, TOTP…'),
            trailing: const Icon(Icons.chevron_right),
            onTap: () =>
                Navigator.of(context).pushNamed(UnattendedScreen.route),
          ),
          ListTile(
            leading: const Icon(Icons.fingerprint),
            title: const Text('Empreinte de ce poste'),
            // FICTIF : l'empreinte réelle viendra du cœur (plan 06).
            subtitle: const Text('9A:F2:04:6B:D8:33:71:CE:…:E1'),
            trailing: TextButton(
              onPressed: () {},
              child: const Text('Afficher le QR'),
            ),
          ),
          const Divider(),
          _titreSection(theme, 'À propos'),
          ListTile(
            leading: const Icon(Icons.memory_outlined),
            title: const Text('Version du moteur (cœur Rust)'),
            subtitle: appInfo.when(
              data: (info) => Text('NovaDesk ${info.version} — '
                  'chiffrement TLS 1.3 + Noise_IK'),
              loading: () => const Text('…'),
              error: (e, _) => const Text('indisponible'),
            ),
          ),
          const ListTile(
            leading: Icon(Icons.flutter_dash),
            title: Text('Interface'),
            subtitle: Text('novadesk_ui 0.1.0 — Flutter, Material 3'),
          ),
        ],
      ),
    );
  }

  Widget _titreSection(ThemeData theme, String titre) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 16, 16, 4),
      child: Text(
        titre,
        style: theme.textTheme.titleSmall?.copyWith(
          color: theme.colorScheme.primary,
          fontWeight: FontWeight.w600,
        ),
      ),
    );
  }
}
