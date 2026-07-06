/// Configuration de l'accès non-surveillé (plan 10 §10.4.5) : mot de passe
/// permanent, appareils de confiance, options de sécurité (TOTP,
/// journalisation, Wake-on-LAN).
///
/// L'activation réelle installera le service système hébergeant le cœur en
/// session détachée (plans 12/15) ; ici l'écran est fonctionnel côté UI et
/// valide les IDs via la façade `nd-ffi` (`parse_nova_id`).
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../widgets/nova_button.dart';
import '../widgets/nova_id_field.dart';

/// Appareil autorisé à se connecter sans présence.
class _AppareilConfiance {
  _AppareilConfiance({
    required this.idFormate,
    required this.alias,
    required this.mode,
  });

  final String idFormate;
  final String alias;
  String mode;
}

class UnattendedScreen extends ConsumerStatefulWidget {
  const UnattendedScreen({super.key});

  static const String route = '/acces-non-surveille';

  @override
  ConsumerState<UnattendedScreen> createState() => _UnattendedScreenState();
}

class _UnattendedScreenState extends ConsumerState<UnattendedScreen> {
  static const List<String> _modes = ['Contrôle', 'Observation'];

  final TextEditingController _motDePasseController = TextEditingController();
  bool _actif = false;
  bool _motDePasseVisible = false;
  bool _totp = true;
  bool _journalisation = true;
  bool _wakeOnLan = false;

  final List<_AppareilConfiance> _appareils = [
    _AppareilConfiance(
      idFormate: '421 887 330',
      alias: 'ce-portable',
      mode: 'Contrôle',
    ),
    _AppareilConfiance(
      idFormate: '730 118 902',
      alias: 'tel-perso',
      mode: 'Observation',
    ),
  ];

  @override
  void dispose() {
    _motDePasseController.dispose();
    super.dispose();
  }

  /// Force du mot de passe entre 0.0 et 1.0 (heuristique simple, indicative).
  double _force(String motDePasse) {
    if (motDePasse.isEmpty) return 0;
    var score = (motDePasse.length / 16).clamp(0.0, 1.0) * 0.5;
    if (RegExp(r'[a-z]').hasMatch(motDePasse)) score += 0.125;
    if (RegExp(r'[A-Z]').hasMatch(motDePasse)) score += 0.125;
    if (RegExp(r'\d').hasMatch(motDePasse)) score += 0.125;
    if (RegExp(r'[^A-Za-z0-9]').hasMatch(motDePasse)) score += 0.125;
    return score.clamp(0.0, 1.0);
  }

  (String, Color) _libelleForce(double force) {
    if (force < 0.4) return ('faible', Colors.red.shade600);
    if (force < 0.7) return ('moyenne', Colors.amber.shade800);
    return ('forte', Colors.green.shade600);
  }

  /// Ajoute un appareil de confiance : l'ID saisi est validé puis reformaté
  /// par la façade (`parse_nova_id` + `format_nova_id`).
  Future<void> _ajouterAppareil() async {
    final idController = TextEditingController();
    final aliasController = TextEditingController();
    final api = ref.read(nativeApiProvider);

    final valide = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Ajouter un appareil de confiance'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIdField(
              controller: idController,
              libelle: "ID de l'appareil",
              autofocus: true,
            ),
            const SizedBox(height: 12),
            TextField(
              controller: aliasController,
              decoration: const InputDecoration(
                labelText: 'Alias (facultatif)',
                border: OutlineInputBorder(),
              ),
            ),
          ],
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Annuler'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Ajouter'),
          ),
        ],
      ),
    );

    if (valide != true) {
      idController.dispose();
      aliasController.dispose();
      return;
    }
    try {
      final id = await api.parseNovaId(texte: idController.text);
      final idFormate = await api.formatNovaId(id: id);
      if (!mounted) return;
      setState(() {
        _appareils.add(_AppareilConfiance(
          idFormate: idFormate,
          alias: aliasController.text.trim().isEmpty
              ? 'sans-alias'
              : aliasController.text.trim(),
          mode: 'Contrôle',
        ));
      });
    } on NovaApiException catch (e) {
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text(e.message)),
      );
    } finally {
      idController.dispose();
      aliasController.dispose();
    }
  }

  void _enregistrer() {
    ScaffoldMessenger.of(context).showSnackBar(
      const SnackBar(
        content: Text(
          'Configuration enregistrée (simulation — elle sera persistée par '
          'le cœur Rust et le service système, plans 12/15).',
        ),
      ),
    );
    Navigator.of(context).pop();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final force = _force(_motDePasseController.text);
    final (libelleForce, couleurForce) = _libelleForce(force);

    return Scaffold(
      appBar: AppBar(title: const Text('Accès non-surveillé')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          Text(
            'Configurez cet appareil pour un accès permanent, sans présence '
            "d'un utilisateur devant l'écran.",
            style: theme.textTheme.bodyMedium,
          ),
          const SizedBox(height: 12),
          Card(
            child: SwitchListTile(
              secondary: const Icon(Icons.shield_outlined),
              title: const Text("Autoriser l'accès non-surveillé"),
              subtitle: Text(
                _actif
                    ? 'Activé — service : novadesk-svc (simulation)'
                    : 'Désactivé',
              ),
              value: _actif,
              onChanged: (valeur) => setState(() => _actif = valeur),
            ),
          ),
          const SizedBox(height: 16),

          // 1. Mot de passe permanent -------------------------------------
          Text('1. Mot de passe permanent', style: theme.textTheme.titleSmall),
          const SizedBox(height: 8),
          TextField(
            controller: _motDePasseController,
            enabled: _actif,
            obscureText: !_motDePasseVisible,
            onChanged: (_) => setState(() {}),
            decoration: InputDecoration(
              labelText: 'Mot de passe',
              border: const OutlineInputBorder(),
              prefixIcon: const Icon(Icons.password_outlined),
              suffixIcon: IconButton(
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
            ),
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              Expanded(
                child: ClipRRect(
                  borderRadius: BorderRadius.circular(4),
                  child: LinearProgressIndicator(
                    value: force,
                    minHeight: 6,
                    color: couleurForce,
                  ),
                ),
              ),
              const SizedBox(width: 10),
              Text('Force : $libelleForce',
                  style: theme.textTheme.bodySmall
                      ?.copyWith(color: couleurForce)),
            ],
          ),
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerLeft,
            child: FilledButton.tonalIcon(
              onPressed: _actif
                  ? () => setState(() {
                        _motDePasseController.text = genererMotDePasse(32);
                        _motDePasseVisible = true;
                      })
                  : null,
              icon: const Icon(Icons.casino_outlined),
              label: const Text('Générer un mot de passe aléatoire (32 c.)'),
            ),
          ),
          const SizedBox(height: 20),

          // 2. Appareils autorisés -----------------------------------------
          Text(
            '2. Appareils autorisés (carnet de confiance)',
            style: theme.textTheme.titleSmall,
          ),
          const SizedBox(height: 8),
          Card(
            child: Column(
              children: [
                for (final appareil in _appareils)
                  ListTile(
                    leading: const Icon(Icons.devices_outlined),
                    title: Text(appareil.idFormate),
                    subtitle: Text(appareil.alias),
                    trailing: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        DropdownButton<String>(
                          value: appareil.mode,
                          underline: const SizedBox.shrink(),
                          items: [
                            for (final mode in _modes)
                              DropdownMenuItem(
                                  value: mode, child: Text(mode)),
                          ],
                          onChanged: _actif
                              ? (valeur) => setState(
                                  () => appareil.mode = valeur ?? appareil.mode)
                              : null,
                        ),
                        IconButton(
                          tooltip: 'Retirer',
                          icon: const Icon(Icons.delete_outline),
                          onPressed: _actif
                              ? () =>
                                  setState(() => _appareils.remove(appareil))
                              : null,
                        ),
                      ],
                    ),
                  ),
                Align(
                  alignment: Alignment.centerRight,
                  child: Padding(
                    padding: const EdgeInsets.all(8),
                    child: TextButton.icon(
                      onPressed: _actif ? _ajouterAppareil : null,
                      icon: const Icon(Icons.add),
                      label: const Text('Ajouter'),
                    ),
                  ),
                ),
              ],
            ),
          ),
          const SizedBox(height: 20),

          // 3. Sécurité ----------------------------------------------------
          Text('3. Sécurité', style: theme.textTheme.titleSmall),
          const SizedBox(height: 8),
          SwitchListTile(
            secondary: const Icon(Icons.phonelink_lock),
            title: const Text('Double authentification (TOTP)'),
            value: _totp,
            onChanged:
                _actif ? (valeur) => setState(() => _totp = valeur) : null,
          ),
          SwitchListTile(
            secondary: const Icon(Icons.receipt_long_outlined),
            title: const Text('Journaliser toutes les sessions'),
            value: _journalisation,
            onChanged: _actif
                ? (valeur) => setState(() => _journalisation = valeur)
                : null,
          ),
          SwitchListTile(
            secondary: const Icon(Icons.power_settings_new_outlined),
            title: const Text('Autoriser le Wake-on-LAN'),
            subtitle: const Text('Réveiller ce poste à distance (plan 13)'),
            value: _wakeOnLan,
            onChanged: _actif
                ? (valeur) => setState(() => _wakeOnLan = valeur)
                : null,
          ),
          if (_actif && (!_totp || !_journalisation)) ...[
            const SizedBox(height: 8),
            Card(
              color: theme.colorScheme.errorContainer,
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Row(
                  children: [
                    Icon(Icons.warning_amber_rounded,
                        color: theme.colorScheme.onErrorContainer),
                    const SizedBox(width: 10),
                    Expanded(
                      child: Text(
                        'La double authentification et la journalisation sont '
                        'fortement recommandées pour un accès permanent.',
                        style: TextStyle(
                            color: theme.colorScheme.onErrorContainer),
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ],
          const SizedBox(height: 24),

          // Actions ---------------------------------------------------------
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              TextButton(
                onPressed: () => Navigator.of(context).pop(),
                child: const Text('Annuler'),
              ),
              const SizedBox(width: 12),
              NovaButton(
                libelle: 'Enregistrer',
                icone: Icons.save_outlined,
                onPressed: _enregistrer,
              ),
            ],
          ),
        ],
      ),
    );
  }
}
