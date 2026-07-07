/// Configuration de l'accès non-surveillé (plan 10 §10.4.5) : mot de passe
/// permanent, appareils de confiance, options de sécurité (TOTP,
/// journalisation, Wake-on-LAN).
///
/// L'activation réelle installera le service système hébergeant le cœur en
/// session détachée (plans 12/15) ; ici l'écran est fonctionnel côté UI et
/// valide les IDs via la façade `nd-ffi` (`parse_nova_id`).
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_button.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_id_field.dart';
import 'incoming_request_dialog.dart';

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

  // --- Hôte « accès non surveillé » réel (façade `nd-ffi`) ------------------

  /// Identifiant opaque de l'hôte tant qu'il est actif (`start_unattended_host`).
  int? _hostId;

  /// Abonnement au flux des demandes entrantes (`unattended_incoming_stream`).
  StreamSubscription<IncomingRequestDto>? _abonnementEntrantes;

  /// Minuterie de rafraîchissement des statistiques d'hôte (~2 s).
  Timer? _minuterieStats;

  /// Dernières statistiques cumulées de l'hôte (`unattended_stats`).
  SessionStatsDto? _stats;

  /// Décisions prises **par cette UI** depuis l'activation (compteurs honnêtes).
  int _servies = 0;
  int _refusees = 0;

  /// Activation/désactivation en cours (désactive le bouton, montre le spinner).
  bool _bascule = false;

  /// Un dialogue d'acceptation est déjà ouvert (évite l'empilement).
  bool _dialogueEnCours = false;

  NativeApi get _api => ref.read(nativeApiProvider);

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
    // Coupe le flux (annule le minuteur du mock) puis arrête l'hôte.
    unawaited(_abonnementEntrantes?.cancel());
    _minuterieStats?.cancel();
    final id = _hostId;
    if (id != null) {
      unawaited(_api.stopUnattendedHost(id));
    }
    _motDePasseController.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Cycle de vie de l'hôte non surveillé
  // ---------------------------------------------------------------------------

  Future<void> _basculerHote(bool activer) =>
      activer ? _activerHote() : _desactiverHote();

  /// Démarre l'hôte (`start_unattended_host`) et s'abonne aux demandes
  /// entrantes ; publie l'ID local au serveur de rendez-vous.
  Future<void> _activerHote() async {
    if (_hostId != null) return;
    setState(() => _bascule = true);
    try {
      final hostId = await _api.startUnattendedHost(
        localId: ref.read(idLocalProvider),
        rendezvous: ref.read(rendezvousProvider),
        stunServers: ref.read(stunServersProvider),
        // L'hôte autorise le contrôle ; le dialogue d'acceptation affine le
        // profil de chaque session servie.
        permissions: PermissionsDto.full(),
      );
      if (!mounted) {
        unawaited(_api.stopUnattendedHost(hostId));
        return;
      }
      _hostId = hostId;
      _abonnementEntrantes = _api.unattendedIncomingStream(hostId).listen(
        (demande) => unawaited(_surDemandeEntrante(demande)),
        onError: (Object e) {
          if (mounted) {
            ScaffoldMessenger.of(context).showSnackBar(
              SnackBar(content: Text('Flux des demandes interrompu : '
                  '${_message(e)}')),
            );
          }
        },
      );
      _demarrerStats();
      setState(() {
        _actif = true;
        _servies = 0;
        _refusees = 0;
      });
    } on NovaApiException catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text(e.message)));
      }
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text(_message(e))));
      }
    } finally {
      if (mounted) setState(() => _bascule = false);
    }
  }

  /// Arrête l'hôte (`stop_unattended_host`), annule l'abonnement et le polling.
  Future<void> _desactiverHote() async {
    final id = _hostId;
    if (id == null) return;
    setState(() => _bascule = true);
    unawaited(_abonnementEntrantes?.cancel());
    _abonnementEntrantes = null;
    _minuterieStats?.cancel();
    _minuterieStats = null;
    try {
      await _api.stopUnattendedHost(id);
    } catch (_) {
      // Arrêt best-effort.
    }
    _hostId = null;
    if (mounted) {
      setState(() {
        _actif = false;
        _bascule = false;
        _stats = null;
      });
    }
  }

  /// À chaque demande entrante : ouvre le dialogue d'acceptation puis tranche
  /// via `approve_incoming` (Accepter → sert la session, Refuser → la refuse ;
  /// un dialogue écarté vaut refus, jamais de blocage).
  Future<void> _surDemandeEntrante(IncomingRequestDto demande) async {
    if (!mounted || _hostId == null || _dialogueEnCours) return;
    _dialogueEnCours = true;
    final alias = _appareils
        .where((a) => a.idFormate == demande.peerIdFormate)
        .map((a) => a.alias)
        .firstOrNull;
    final reponse = await IncomingRequestDialog.montrer(
      context,
      alias: alias ?? 'Appareil non répertorié',
      idFormate: demande.peerIdFormate,
      empreinte: _empreinte(demande.peerId),
    );
    final accepter = reponse?.acceptee ?? false;
    final id = _hostId;
    if (id != null) {
      try {
        await _api.approveIncoming(
          hostId: id,
          peerId: demande.peerId,
          accepter: accepter,
        );
      } catch (_) {
        // Demande déjà tranchée/expirée : rien de plus à faire côté UI.
      }
    }
    if (mounted) {
      setState(() {
        if (accepter) {
          _servies++;
        } else {
          _refusees++;
        }
      });
    }
    _dialogueEnCours = false;
  }

  void _demarrerStats() {
    _minuterieStats?.cancel();
    unawaited(_rafraichirStats());
    _minuterieStats = Timer.periodic(
      const Duration(seconds: 2),
      (_) => unawaited(_rafraichirStats()),
    );
  }

  Future<void> _rafraichirStats() async {
    final id = _hostId;
    if (id == null) return;
    try {
      final stats = await _api.unattendedStats(id);
      if (mounted) setState(() => _stats = stats);
    } catch (_) {
      // Stats indisponibles : on conserve la dernière valeur connue.
    }
  }

  String _message(Object e) =>
      e is NovaApiException ? e.message : e.toString();

  /// Empreinte lisible dérivée de l'ID pour l'affichage du dialogue (démo :
  /// l'empreinte réelle viendra du certificat épinglé du pair, plan 06).
  String _empreinte(int peerId) {
    final hex = peerId.toRadixString(16).toUpperCase().padLeft(12, '0');
    final paires = <String>[];
    for (var i = 0; i + 2 <= hex.length; i += 2) {
      paires.add(hex.substring(i, i + 2));
    }
    return paires.join(':');
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
    // Couleurs sémantiques du doc 03 §1.1 (avertissement #F0A020, vert accès).
    if (force < 0.4) return ('faible', kNovaRouge);
    if (force < 0.7) return ('moyenne', const Color(0xFFF0A020));
    return ('forte', kNovaVert);
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

  // ---------------------------------------------------------------------------
  // Carte d'état de l'hôte (activation + statistiques live)
  // ---------------------------------------------------------------------------

  Widget _carteHote(ThemeData theme, NovaTokens t) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(14),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                NovaIcone(NovaIcones.bouclier,
                    couleur: _actif ? kNovaVert : t.texte2),
                const SizedBox(width: 10),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text('Accès non surveillé',
                          style: theme.textTheme.titleSmall),
                      const SizedBox(height: 1),
                      Text(
                        _actif
                            ? 'Actif — ce poste écoute et fait valider les '
                                'demandes entrantes'
                            : 'Inactif — les connexions non surveillées sont '
                                'refusées',
                        style: TextStyle(fontSize: 11.5, color: t.texte3),
                      ),
                    ],
                  ),
                ),
                _pastilleEtat(t),
              ],
            ),
            const SizedBox(height: 14),
            Row(
              children: [
                if (!_actif)
                  NovaButton(
                    libelle: "Activer l'accès non surveillé",
                    icone: NovaIcones.bouclierCoche,
                    enCours: _bascule,
                    onPressed:
                        _bascule ? null : () => unawaited(_basculerHote(true)),
                  )
                else
                  OutlinedButton.icon(
                    onPressed:
                        _bascule ? null : () => unawaited(_basculerHote(false)),
                    icon: const NovaIcone(NovaIcones.fermer, taille: 14),
                    label: const Text('Désactiver'),
                  ),
                const Spacer(),
                if (_actif) ...[
                  _compteur(t, 'Servies', _servies, kNovaVert),
                  const SizedBox(width: 16),
                  _compteur(t, 'Refusées', _refusees, kNovaRouge),
                ],
              ],
            ),
            if (_actif && _stats != null) ...[
              const SizedBox(height: 12),
              Divider(color: t.filet, height: 1),
              const SizedBox(height: 10),
              _ligneStatsHote(t, _stats!),
            ],
          ],
        ),
      ),
    );
  }

  Widget _pastilleEtat(NovaTokens t) {
    final couleur = _actif ? kNovaVert : t.texte3;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 9, vertical: 4),
      decoration: BoxDecoration(
        color: couleur.withValues(alpha: 0.14),
        borderRadius: BorderRadius.circular(20),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: 7,
            height: 7,
            decoration: BoxDecoration(color: couleur, shape: BoxShape.circle),
          ),
          const SizedBox(width: 6),
          Text(
            _actif ? 'Actif' : 'Inactif',
            style: TextStyle(
                fontSize: 11, fontWeight: FontWeight.w600, color: couleur),
          ),
        ],
      ),
    );
  }

  Widget _compteur(NovaTokens t, String libelle, int valeur, Color couleur) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.end,
      children: [
        Text('$valeur',
            style: TextStyle(
                fontSize: 17, fontWeight: FontWeight.w700, color: couleur)),
        Text(libelle, style: TextStyle(fontSize: 10.5, color: t.texte3)),
      ],
    );
  }

  /// Résumé honnête des statistiques cumulées de l'hôte (`unattended_stats`).
  Widget _ligneStatsHote(NovaTokens t, SessionStatsDto s) {
    final parts = <String>[
      '↑ ${_formaterOctets(s.bytesOut)}',
      if (s.targetBitrateKbps > 0)
        'ABR N${s.abrLevel} · ${_formaterDebit(s.targetBitrateKbps)}',
      if (s.inputsDenied > 0) 'Entrées refusées : ${s.inputsDenied}',
      'RTT : ${(s.rttUs / 1000).toStringAsFixed(0)} ms',
    ];
    return Row(
      children: [
        NovaIcone(NovaIcones.qualite, taille: 13, couleur: t.texte3),
        const SizedBox(width: 8),
        Expanded(
          child: Text(
            parts.join('  ·  '),
            style: TextStyle(fontSize: 11.5, color: t.texte3),
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

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final t = NovaTokens.of(context);
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
          _carteHote(theme, t),
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
              prefixIcon: const Padding(
                padding: EdgeInsets.symmetric(horizontal: 11),
                child: NovaIcone(NovaIcones.cle, taille: 16),
              ),
              prefixIconConstraints:
                  const BoxConstraints(minWidth: 38, minHeight: 38),
              suffixIcon: IconButton(
                tooltip: _motDePasseVisible ? 'Masquer' : 'Afficher',
                icon: NovaIcone(
                  _motDePasseVisible
                      ? NovaIcones.oeilBarre
                      : NovaIcones.oeil,
                  taille: 16,
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
            child: OutlinedButton.icon(
              onPressed: _actif
                  ? () => setState(() {
                        _motDePasseController.text = genererMotDePasse(32);
                        _motDePasseVisible = true;
                      })
                  : null,
              icon: const NovaIcone(NovaIcones.recharger, taille: 14),
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
                    leading: const NovaIcone(NovaIcones.moniteur),
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
                          icon: const NovaIcone(NovaIcones.corbeille,
                              taille: 16),
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
                      icon: const NovaIcone(NovaIcones.plus, taille: 14),
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
            secondary: const NovaIcone(NovaIcones.cadenas),
            title: const Text('Double authentification (TOTP)'),
            value: _totp,
            onChanged:
                _actif ? (valeur) => setState(() => _totp = valeur) : null,
          ),
          SwitchListTile(
            secondary: const NovaIcone(NovaIcones.horloge),
            title: const Text('Journaliser toutes les sessions'),
            value: _journalisation,
            onChanged: _actif
                ? (valeur) => setState(() => _journalisation = valeur)
                : null,
          ),
          SwitchListTile(
            secondary: const NovaIcone(NovaIcones.alimentation),
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
                    NovaIcone(NovaIcones.avertissement,
                        couleur: theme.colorScheme.onErrorContainer),
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
                icone: NovaIcones.coche,
                onPressed: _enregistrer,
              ),
            ],
          ),
        ],
      ),
    );
  }
}
