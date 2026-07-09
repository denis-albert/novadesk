/// Accès non surveillé (maquette `novadesk-app.html`, `#v-unattended`) : volet
/// façon réglages — activation, mot de passe permanent + jauge de force,
/// profils de permissions, appareils de confiance, journal des accès.
///
/// L'écran est une **vue** : le cycle de vie de l'hôte réel (démarrage,
/// demandes entrantes et leur dialogue d'acceptation, statistiques, arrêt)
/// appartient au provider applicatif [hoteNonSurveilleProvider] — quitter
/// l'onglet ne coupe plus la réception, qui survit tant que l'application vit.
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

class UnattendedScreen extends ConsumerStatefulWidget {
  const UnattendedScreen({super.key});

  static const String route = NovaRoutes.nonSurveille;

  @override
  ConsumerState<UnattendedScreen> createState() => _UnattendedScreenState();
}

class _UnattendedScreenState extends ConsumerState<UnattendedScreen> {
  final TextEditingController _motDePasseController = TextEditingController();
  bool _profilControle = true;
  bool _profilObservation = false;

  NativeApi get _api => ref.read(nativeApiProvider);

  @override
  void dispose() {
    // L'hôte non surveillé n'est PLUS arrêté ici : son cycle de vie appartient
    // à [hoteNonSurveilleProvider], au niveau application — la réception
    // continue quand on quitte l'onglet.
    _motDePasseController.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Hôte non surveillé : simple relais vers le provider applicatif
  // ---------------------------------------------------------------------------

  /// Bascule l'hôte via [hoteNonSurveilleProvider] ; l'écran ne fait
  /// qu'afficher le résultat (toast de confirmation ou d'erreur).
  Future<void> _basculerHote(bool activer) async {
    final hote = ref.read(hoteNonSurveilleProvider.notifier);
    try {
      await hote.basculer(activer);
      if (activer && mounted && ref.read(hoteNonSurveilleProvider).actif) {
        NovaToast.montrer(context, 'Accès non surveillé activé');
      }
    } catch (e) {
      if (mounted) NovaToast.montrer(context, messageNova(e), info: true);
    }
  }

  /// Sous-titre du journal : compteurs réels de l'`access_log` persistant, plus,
  /// en session, le résumé des statistiques cumulées de l'hôte
  /// (`unattended_stats`) tenues par [hoteNonSurveilleProvider].
  String _sousTitreJournal() {
    final journal =
        ref.watch(accessLogProvider).valueOrNull ?? const <AccessLogEntryDto>[];
    final acceptes = journal.where((e) => e.accepte).length;
    final refuses = journal.length - acceptes;
    final base = '${journal.length} accès journalisé(s) — '
        '$acceptes acceptée(s), $refuses refusée(s)';
    final hote = ref.watch(hoteNonSurveilleProvider);
    if (!hote.actif) return '$base.';
    final s = hote.stats;
    if (s == null) {
      return '$base · session : ${hote.servies} servie(s), '
          '${hote.refusees} refusée(s).';
    }
    final mo =
        (s.bytesOut / (1024 * 1024)).toStringAsFixed(1).replaceAll('.', ',');
    final ms = (s.rttUs / 1000).toStringAsFixed(0);
    return '$base · ↑ $mo Mo servis · RTT $ms ms.';
  }

  /// Alias d'un pair depuis le carnet, sinon son ID formaté.
  String _aliasOuId(int peerId, String peerIdFormate) {
    final carnet =
        ref.read(carnetProvider).valueOrNull ?? const <EntreeCarnet>[];
    return carnet
            .where((e) => e.id == peerId)
            .map((e) => e.alias)
            .firstOrNull ??
        peerIdFormate;
  }

  /// Alias d'un appareil de confiance depuis le carnet (« sans-alias » sinon).
  String _aliasPour(int id) {
    final carnet =
        ref.read(carnetProvider).valueOrNull ?? const <EntreeCarnet>[];
    return carnet.where((e) => e.id == id).map((e) => e.alias).firstOrNull ??
        'sans-alias';
  }

  /// Formatage local d'un ID (9 chiffres, groupés par 3).
  static String _formaterId(int id) {
    var chiffres = id.toString();
    if (chiffres.length < 9) chiffres = chiffres.padLeft(9, '0');
    final groupes = <String>[];
    for (var fin = chiffres.length; fin > 0; fin -= 3) {
      final debut = fin - 3 < 0 ? 0 : fin - 3;
      groupes.insert(0, chiffres.substring(debut, fin));
    }
    return groupes.join(' ');
  }

  /// Sous-titre de la ligne « Appareils de confiance » (depuis
  /// `unattended_config`).
  String _sousTitreAppareils() {
    final ids = ref.watch(unattendedConfigProvider).valueOrNull
            ?.appareilsDeConfiance ??
        const <int>[];
    if (ids.isEmpty) {
      return 'Aucun appareil de confiance — ajoutez-en pour la connexion '
          'sans mot de passe.';
    }
    return '${ids.map(_aliasPour).join(' · ')} — connexion sans mot de passe.';
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

  // ---------------------------------------------------------------------------
  // Mot de passe permanent (set_unattended_password / verify_unattended_password)
  // ---------------------------------------------------------------------------

  /// Définit (ou efface, si vide) le mot de passe permanent.
  Future<void> _definirMotDePasse() async {
    final pwd = _motDePasseController.text;
    try {
      await _api.setUnattendedPassword(pwd: pwd);
      ref.invalidate(unattendedConfigProvider);
      if (mounted) {
        NovaToast.montrer(
            context,
            pwd.isEmpty
                ? 'Mot de passe permanent effacé'
                : 'Mot de passe permanent défini');
      }
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  /// Vérifie le mot de passe saisi contre le hachage stocké.
  Future<void> _verifierMotDePasse() async {
    final ok =
        await _api.verifyUnattendedPassword(pwd: _motDePasseController.text);
    if (!mounted) return;
    NovaToast.montrer(
      context,
      ok ? 'Mot de passe correct' : 'Mot de passe incorrect',
      info: !ok,
    );
  }

  // ---------------------------------------------------------------------------
  // Appareils de confiance (add_trusted_device / remove_trusted_device)
  // ---------------------------------------------------------------------------

  /// Saisit un ID NovaDesk d'appareil ; renvoie l'ID analysé ou `null`.
  Future<int?> _saisirAppareilId() async {
    final idController = TextEditingController();
    final valide = await montrerDialogueNova<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Ajouter un appareil de confiance'),
        content: NovaIdField(
            controller: idController,
            libelle: "ID de l'appareil",
            autofocus: true),
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
    int? resultat;
    if (valide == true) {
      try {
        resultat = await _api.parseNovaId(texte: idController.text);
      } on NovaApiException catch (e) {
        if (mounted) NovaToast.montrer(context, e.message, info: true);
      }
    }
    idController.dispose();
    return resultat;
  }

  Future<void> _ajouterAppareil() async {
    final id = await _saisirAppareilId();
    if (id == null) return;
    try {
      await _api.addTrustedDevice(id: id);
      ref.invalidate(unattendedConfigProvider);
      if (mounted) {
        NovaToast.montrer(context, 'Appareil ajouté à la liste de confiance');
      }
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  Future<void> _retirerAppareil(int id) async {
    try {
      await _api.removeTrustedDevice(id: id);
      ref.invalidate(unattendedConfigProvider);
      if (mounted) NovaToast.montrer(context, 'Appareil retiré');
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  Future<void> _gererAppareils() async {
    await montrerDialogueNova<void>(
      context: context,
      builder: (context) {
        final t = NovaTokens.of(context);
        return AlertDialog(
          title: const Text('Appareils de confiance'),
          backgroundColor: t.fenetre,
          content: SizedBox(
            width: 380,
            child: Consumer(
              builder: (context, ref, _) {
                final ids = ref
                        .watch(unattendedConfigProvider)
                        .valueOrNull
                        ?.appareilsDeConfiance ??
                    const <int>[];
                return Column(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    if (ids.isEmpty)
                      Padding(
                        padding: const EdgeInsets.symmetric(vertical: 8),
                        child: Align(
                          alignment: Alignment.centerLeft,
                          child: Text('Aucun appareil de confiance.',
                              style:
                                  TextStyle(fontSize: 12.5, color: t.texte3)),
                        ),
                      ),
                    for (final id in ids)
                      ListTile(
                        contentPadding: EdgeInsets.zero,
                        leading: const NovaIcone(NovaIcones.moniteur),
                        title: Text(_formaterId(id)),
                        subtitle: Text(_aliasPour(id)),
                        trailing: IconButton(
                          tooltip: 'Retirer',
                          icon: const NovaIcone(NovaIcones.corbeille,
                              taille: 16),
                          onPressed: () => unawaited(_retirerAppareil(id)),
                        ),
                      ),
                    Align(
                      alignment: Alignment.centerLeft,
                      child: TextButton.icon(
                        onPressed: () => unawaited(_ajouterAppareil()),
                        icon: const NovaIcone(NovaIcones.plus, taille: 14),
                        label: const Text('Ajouter'),
                      ),
                    ),
                  ],
                );
              },
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

  void _voirJournal() {
    montrerDialogueNova<void>(
      context: context,
      builder: (context) {
        final t = NovaTokens.of(context);
        return AlertDialog(
          title: const Text('Journal des accès'),
          content: SizedBox(
            width: 380,
            child: Consumer(
              builder: (context, ref, _) {
                final journal = ref.watch(accessLogProvider).valueOrNull ??
                    const <AccessLogEntryDto>[];
                if (journal.isEmpty) {
                  return Align(
                    alignment: Alignment.centerLeft,
                    child: Text('Aucun accès journalisé pour l’instant.',
                        style: TextStyle(fontSize: 12.5, color: t.texte3)),
                  );
                }
                return Column(
                  mainAxisSize: MainAxisSize.min,
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    for (final e in journal.take(30))
                      Padding(
                        padding: const EdgeInsets.symmetric(vertical: 4),
                        child: Row(
                          children: [
                            NovaIcone(
                              e.accepte ? NovaIcones.coche : NovaIcones.bloquer,
                              taille: 14,
                              couleur: e.accepte ? t.vert : kNovaRouge,
                            ),
                            const SizedBox(width: 8),
                            Expanded(
                              child: Text(
                                '${_aliasOuId(e.peerId, e.peerIdFormate)} · '
                                '${formaterHorodatageRelatif(e.timestamp)} · '
                                '${e.accepte ? 'acceptée' : 'refusée'}',
                                style:
                                    TextStyle(fontSize: 12, color: t.texte3),
                              ),
                            ),
                          ],
                        ),
                      ),
                  ],
                );
              },
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
    // Dépendances réactives : la config d'accès, le carnet et le journal
    // alimentent les sous-titres (appareils, journal) — l'écran se reconstruit
    // à chaque changement persistant.
    ref.watch(unattendedConfigProvider);
    ref.watch(carnetProvider);
    ref.watch(accessLogProvider);
    // État applicatif de l'hôte : interrupteur, stats et compteurs de session.
    final hote = ref.watch(hoteNonSurveilleProvider);
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
            actif: hote.actif,
            onChanged:
                hote.bascule ? null : (v) => unawaited(_basculerHote(v)),
          ),
        ),
        _lignePassword(t),
        _ligneProfils(t),
        _ligne(
          t,
          titre: 'Appareils de confiance',
          sousTitre: _sousTitreAppareils(),
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
    final aMotDePasse =
        ref.watch(unattendedConfigProvider).valueOrNull?.aMotDePasse ?? false;
    return _ligne(
      t,
      alignement: CrossAxisAlignment.start,
      titre: 'Mot de passe permanent',
      sousTitreWidget: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            aMotDePasse
                ? 'Un mot de passe permanent est configuré (haché et salé).'
                : 'Aucun mot de passe permanent — accès refusé par défaut.',
            style: TextStyle(fontSize: 11.5, color: t.texte3),
          ),
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
      controle: Column(
        crossAxisAlignment: CrossAxisAlignment.end,
        mainAxisSize: MainAxisSize.min,
        children: [
          Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              SizedBox(
                width: 180,
                height: 32,
                child: TextField(
                  controller: _motDePasseController,
                  obscureText: true,
                  onChanged: (_) => setState(() {}),
                  decoration:
                      const InputDecoration(hintText: 'Nouveau mot de passe'),
                  style: const TextStyle(fontSize: 12.5),
                ),
              ),
              const SizedBox(width: 8),
              NovaBoutonSecondaire(
                libelle: 'Générer',
                onPressed: () => setState(
                    () => _motDePasseController.text = genererMotDePasse(20)),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              NovaBoutonSecondaire(
                libelle: 'Vérifier',
                onPressed: () => unawaited(_verifierMotDePasse()),
              ),
              const SizedBox(width: 8),
              NovaBoutonPrimaire(
                libelle: 'Définir',
                onPressed: () => unawaited(_definirMotDePasse()),
              ),
            ],
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
