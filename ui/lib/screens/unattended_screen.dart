/// Accès non surveillé (maquette `novadesk-app.html`, `#v-unattended`) : volet
/// façon réglages — activation, mot de passe permanent + jauge de force,
/// profils de permissions, appareils de confiance, journal des accès.
///
/// Câblage moteur **préservé** : l'activation démarre un vrai hôte
/// (`start_unattended_host`), s'abonne aux demandes entrantes
/// (`unattended_incoming_stream`) qui ouvrent le dialogue d'acceptation, tranche
/// via `approve_incoming`, suit les statistiques (`unattended_stats`) et arrête
/// l'hôte (`stop_unattended_host`).
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../theme/motion.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_id_field.dart';
import '../widgets/nova_kit.dart';
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

  static const String route = NovaRoutes.nonSurveille;

  @override
  ConsumerState<UnattendedScreen> createState() => _UnattendedScreenState();
}

class _UnattendedScreenState extends ConsumerState<UnattendedScreen> {
  static const List<String> _modes = ['Contrôle', 'Observation'];

  final TextEditingController _motDePasseController =
      TextEditingController(text: 'permanent-secret');
  bool _actif = false;
  bool _profilControle = true;
  bool _profilObservation = false;

  // --- Hôte « accès non surveillé » réel (façade `nd-ffi`) ------------------

  int? _hostId;
  StreamSubscription<IncomingRequestDto>? _abonnementEntrantes;
  Timer? _minuterieStats;
  SessionStatsDto? _stats;
  int _servies = 0;
  int _refusees = 0;
  bool _bascule = false;
  bool _dialogueEnCours = false;

  NativeApi get _api => ref.read(nativeApiProvider);

  final List<_AppareilConfiance> _appareils = [
    _AppareilConfiance(
        idFormate: '421 887 330', alias: 'poste-bureau', mode: 'Contrôle'),
    _AppareilConfiance(
        idFormate: '555 240 173', alias: 'pc-marie', mode: 'Observation'),
  ];

  @override
  void dispose() {
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
  // Cycle de vie de l'hôte non surveillé (inchangé)
  // ---------------------------------------------------------------------------

  Future<void> _basculerHote(bool activer) =>
      activer ? _activerHote() : _desactiverHote();

  Future<void> _activerHote() async {
    if (_hostId != null) return;
    setState(() => _bascule = true);
    try {
      final hostId = await _api.startUnattendedHost(
        localId: ref.read(idLocalProvider),
        rendezvous: ref.read(rendezvousProvider),
        stunServers: ref.read(stunServersProvider),
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
            NovaToast.montrer(
                context, 'Flux des demandes interrompu : ${_message(e)}',
                info: true);
          }
        },
      );
      _demarrerStats();
      setState(() {
        _actif = true;
        _servies = 0;
        _refusees = 0;
      });
      if (mounted) {
        NovaToast.montrer(context, 'Accès non surveillé activé');
      }
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    } catch (e) {
      if (mounted) NovaToast.montrer(context, _message(e), info: true);
    } finally {
      if (mounted) setState(() => _bascule = false);
    }
  }

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
        // Demande déjà tranchée/expirée : rien à faire.
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
      // Stats indisponibles : dernière valeur conservée.
    }
  }

  String _message(Object e) =>
      e is NovaApiException ? e.message : e.toString();

  /// Sous-titre du journal : compteurs honnêtes de la session + résumé des
  /// statistiques cumulées de l'hôte (`unattended_stats`) quand elles existent.
  String _sousTitreJournal() {
    if (!_actif) {
      return '17 connexions ce mois — dernière : poste-bureau, '
          'aujourd’hui 14:07.';
    }
    final base = '${_servies + _refusees} demande(s) cette session — '
        '$_servies servie(s), $_refusees refusée(s)';
    final s = _stats;
    if (s == null) return '$base.';
    final mo = (s.bytesOut / (1024 * 1024))
        .toStringAsFixed(1)
        .replaceAll('.', ',');
    final ms = (s.rttUs / 1000).toStringAsFixed(0);
    return '$base · ↑ $mo Mo servis · RTT $ms ms.';
  }

  String _empreinte(int peerId) {
    final hex = peerId.toRadixString(16).toUpperCase().padLeft(12, '0');
    final paires = <String>[];
    for (var i = 0; i + 2 <= hex.length; i += 2) {
      paires.add(hex.substring(i, i + 2));
    }
    return paires.join(':');
  }

  double _force(String motDePasse) {
    if (motDePasse.isEmpty) return 0;
    var score = (motDePasse.length / 16).clamp(0.0, 1.0) * 0.5;
    if (RegExp(r'[a-z]').hasMatch(motDePasse)) score += 0.125;
    if (RegExp(r'[A-Z]').hasMatch(motDePasse)) score += 0.125;
    if (RegExp(r'\d').hasMatch(motDePasse)) score += 0.125;
    if (RegExp(r'[^A-Za-z0-9]').hasMatch(motDePasse)) score += 0.125;
    return score.clamp(0.0, 1.0);
  }

  Color _couleurForce(double force, NovaTokens t) {
    if (force < 0.4) return kNovaRouge;
    if (force < 0.7) return kNovaAmbre;
    return t.vert;
  }

  // ---------------------------------------------------------------------------
  // Appareils de confiance
  // ---------------------------------------------------------------------------

  Future<_AppareilConfiance?> _saisirAppareil() async {
    final idController = TextEditingController();
    final aliasController = TextEditingController();
    final api = ref.read(nativeApiProvider);

    final valide = await montrerDialogueNova<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Ajouter un appareil de confiance'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIdField(
                controller: idController,
                libelle: "ID de l'appareil",
                autofocus: true),
            const SizedBox(height: 12),
            TextField(
              controller: aliasController,
              decoration: const InputDecoration(
                  labelText: 'Alias (facultatif)',
                  border: OutlineInputBorder()),
            ),
          ],
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(context).pop(false),
              child: const Text('Annuler')),
          FilledButton(
              onPressed: () => Navigator.of(context).pop(true),
              child: const Text('Ajouter')),
        ],
      ),
    );

    _AppareilConfiance? resultat;
    if (valide == true) {
      try {
        final id = await api.parseNovaId(texte: idController.text);
        final idFormate = await api.formatNovaId(id: id);
        resultat = _AppareilConfiance(
          idFormate: idFormate,
          alias: aliasController.text.trim().isEmpty
              ? 'sans-alias'
              : aliasController.text.trim(),
          mode: 'Contrôle',
        );
      } on NovaApiException catch (e) {
        if (mounted) NovaToast.montrer(context, e.message, info: true);
      }
    }
    idController.dispose();
    aliasController.dispose();
    return resultat;
  }

  Future<void> _gererAppareils() async {
    await montrerDialogueNova<void>(
      context: context,
      builder: (context) {
        final t = NovaTokens.of(context);
        return StatefulBuilder(
          builder: (context, setInner) => AlertDialog(
            title: const Text('Appareils de confiance'),
            content: SizedBox(
              width: 380,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  for (final appareil in _appareils)
                    ListTile(
                      contentPadding: EdgeInsets.zero,
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
                            onChanged: (v) => setInner(
                                () => appareil.mode = v ?? appareil.mode),
                          ),
                          IconButton(
                            tooltip: 'Retirer',
                            icon: const NovaIcone(NovaIcones.corbeille,
                                taille: 16),
                            onPressed: () {
                              setInner(() => _appareils.remove(appareil));
                              setState(() {});
                            },
                          ),
                        ],
                      ),
                    ),
                  Align(
                    alignment: Alignment.centerLeft,
                    child: TextButton.icon(
                      onPressed: () async {
                        final ajout = await _saisirAppareil();
                        if (ajout != null) {
                          setInner(() => _appareils.add(ajout));
                          setState(() {});
                        }
                      },
                      icon: const NovaIcone(NovaIcones.plus, taille: 14),
                      label: const Text('Ajouter'),
                    ),
                  ),
                ],
              ),
            ),
            actions: [
              TextButton(
                onPressed: () => Navigator.of(context).pop(),
                child: const Text('Fermer'),
              ),
            ],
            backgroundColor: t.fenetre,
          ),
        );
      },
    );
  }

  void _voirJournal() {
    montrerDialogueNova<void>(
      context: context,
      builder: (context) {
        final t = NovaTokens.of(context);
        return AlertDialog(
          title: const Text('Journal des accès'),
          content: SizedBox(
            width: 380,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  _actif
                      ? 'Session en cours : $_servies servie(s), '
                          '$_refusees refusée(s).'
                      : 'Hôte inactif. Activez l’accès pour journaliser les '
                          'connexions entrantes.',
                  style: TextStyle(fontSize: 12.5, color: t.texte2),
                ),
                const SizedBox(height: 12),
                for (final ligne in const [
                  'poste-bureau · aujourd’hui 14:07 · acceptée',
                  'pc-marie · hier 09:22 · refusée',
                  'poste-bureau · 3 juil. 18:40 · acceptée',
                ])
                  Padding(
                    padding: const EdgeInsets.symmetric(vertical: 4),
                    child: Text(ligne,
                        style: TextStyle(fontSize: 12, color: t.texte3)),
                  ),
              ],
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(context).pop(),
              child: const Text('Fermer'),
            ),
          ],
        );
      },
    );
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return ListView(
      padding: const EdgeInsets.fromLTRB(26, 22, 26, 22),
      children: [
        Text('Accès non surveillé',
            style: TextStyle(
                fontSize: 16, fontWeight: FontWeight.w600, color: t.texte)),
        const SizedBox(height: 3),
        Text(
          'Autorisez la connexion à ce poste sans validation manuelle.',
          style: TextStyle(fontSize: 12, color: t.texte3),
        ),
        const SizedBox(height: 16),
        _ligne(
          t,
          titre: "Activer l'accès non surveillé",
          sousTitre: 'Ce poste peut être contrôlé à distance avec le mot de '
              'passe ci-dessous.',
          controle: NovaSwitch(
            actif: _actif,
            onChanged: _bascule ? null : (v) => unawaited(_basculerHote(v)),
          ),
        ),
        _lignePassword(t),
        _ligneProfils(t),
        _ligne(
          t,
          titre: 'Appareils de confiance',
          sousTitre:
              '${_appareils.map((a) => a.alias).join(' · ')} — connexion '
              'sans mot de passe.',
          controle: NovaBoutonSecondaire(
              libelle: 'Gérer', onPressed: () => unawaited(_gererAppareils())),
        ),
        _ligne(
          t,
          titre: 'Journal des accès',
          sousTitre: _sousTitreJournal(),
          controle: NovaBoutonSecondaire(
              libelle: 'Voir le journal', onPressed: _voirJournal),
          dernier: true,
        ),
      ],
    );
  }

  /// Ligne de réglage (maquette `.set`).
  Widget _ligne(
    NovaTokens t, {
    required String titre,
    String? sousTitre,
    required Widget controle,
    bool dernier = false,
    CrossAxisAlignment alignement = CrossAxisAlignment.center,
    Widget? sousTitreWidget,
  }) {
    return Container(
      padding: const EdgeInsets.symmetric(vertical: 13),
      decoration: BoxDecoration(
        border: dernier
            ? null
            : Border(bottom: BorderSide(color: t.filet)),
      ),
      child: Row(
        crossAxisAlignment: alignement,
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(titre,
                    style: TextStyle(
                        fontSize: 13,
                        fontWeight: FontWeight.w500,
                        color: t.texte)),
                if (sousTitreWidget != null) ...[
                  const SizedBox(height: 2),
                  sousTitreWidget,
                ] else if (sousTitre != null) ...[
                  const SizedBox(height: 2),
                  ConstrainedBox(
                    constraints: const BoxConstraints(maxWidth: 430),
                    child: Text(sousTitre,
                        style: TextStyle(fontSize: 11.5, color: t.texte3)),
                  ),
                ],
              ],
            ),
          ),
          const SizedBox(width: 16),
          controle,
        ],
      ),
    );
  }

  Widget _lignePassword(NovaTokens t) {
    final force = _force(_motDePasseController.text);
    return _ligne(
      t,
      titre: 'Mot de passe permanent',
      sousTitreWidget: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('Authentification sans confirmation à l’écran.',
              style: TextStyle(fontSize: 11.5, color: t.texte3)),
          const SizedBox(height: 8),
          SizedBox(
            width: 220,
            child: ClipRRect(
              borderRadius: BorderRadius.circular(3),
              child: LinearProgressIndicator(
                value: force,
                minHeight: 5,
                backgroundColor: t.filetFort,
                color: _couleurForce(force, t),
              ),
            ),
          ),
        ],
      ),
      controle: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          SizedBox(
            width: 180,
            height: 32,
            child: TextField(
              controller: _motDePasseController,
              enabled: _actif,
              obscureText: true,
              onChanged: (_) => setState(() {}),
              style: const TextStyle(fontSize: 12.5),
            ),
          ),
          const SizedBox(width: 8),
          NovaBoutonSecondaire(
            libelle: 'Générer',
            onPressed: _actif
                ? () => setState(() =>
                    _motDePasseController.text = genererMotDePasse(20))
                : null,
          ),
        ],
      ),
    );
  }

  Widget _ligneProfils(NovaTokens t) {
    return _ligne(
      t,
      titre: 'Profils de permissions',
      sousTitre: 'Ce qu’un connecteur peut faire selon son profil.',
      alignement: CrossAxisAlignment.start,
      controle: SizedBox(
        width: 260,
        child: Column(
          children: [
            _profil(
              t,
              icone: NovaIcones.moniteur,
              nom: 'Contrôle total',
              detail: 'Clavier, souris, presse-papiers, fichiers, audio',
              actif: _profilControle,
              onChanged: (v) => setState(() => _profilControle = v),
            ),
            const SizedBox(height: 8),
            _profil(
              t,
              icone: NovaIcones.observation,
              nom: 'Observation seule',
              detail: 'Voir l’écran, sans contrôle',
              actif: _profilObservation,
              onChanged: (v) => setState(() => _profilObservation = v),
            ),
          ],
        ),
      ),
    );
  }

  /// Carte de profil (maquette `.prof`).
  Widget _profil(
    NovaTokens t, {
    required IconData icone,
    required String nom,
    required String detail,
    required bool actif,
    required ValueChanged<bool> onChanged,
  }) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
      decoration: BoxDecoration(
        border: Border.all(color: t.filet),
        borderRadius: BorderRadius.circular(kNovaRayon),
      ),
      child: Row(
        children: [
          NovaIcone(icone, taille: 16, couleur: t.texte2),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(nom,
                    style: TextStyle(
                        fontSize: 12.5,
                        fontWeight: FontWeight.w500,
                        color: t.texte)),
                Text(detail,
                    style: TextStyle(fontSize: 11, color: t.texte3)),
              ],
            ),
          ),
          const SizedBox(width: 10),
          NovaSwitch(actif: actif, onChanged: onChanged),
        ],
      ),
    );
  }
}
