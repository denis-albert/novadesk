/// Écran des réglages en **onglets** (doc 03 §5.4) : Interface, Sécurité,
/// Connexion, Affichage, Enregistrement, À propos — avec profils de
/// permissions (Par défaut / Partage d'écran / Contrôle total / Non
/// surveillé) et liste blanche ACL à joker (`*@espace`).
///
/// État local volatil : la persistance réelle appartiendra au cœur Rust
/// (source de vérité, fichier chiffré — plans 06/11) via la façade.
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/app_frame.dart';
import '../widgets/nova_icons.dart';
import 'incoming_request_dialog.dart';
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
  String _bandePassante = 'Illimitée';
  bool _decouverteLan = true;
  String _qualiteDefaut = 'Équilibré';
  bool _adapterResolution = true;
  bool _curseurDistant = true;
  bool _enregistrementAuto = false;
  String _formatEnregistrement = 'NDR (natif)';

  /// Liste blanche ACL — joker `*@espace` accepté.
  final List<String> _acl = ['*@atelier', '421 887 330'];
  final TextEditingController _aclController = TextEditingController();

  /// Profils de permissions : nom → (permission → accordée).
  final Map<String, Map<String, bool>> _profils = {
    'Par défaut': {
      "Afficher l'écran": true,
      'Clavier et souris': true,
      'Presse-papiers': true,
      'Transfert de fichiers': false,
      'Transmettre le son': false,
    },
    "Partage d'écran": {
      "Afficher l'écran": true,
      'Clavier et souris': false,
      'Presse-papiers': false,
      'Transfert de fichiers': false,
      'Transmettre le son': false,
    },
    'Contrôle total': {
      "Afficher l'écran": true,
      'Clavier et souris': true,
      'Presse-papiers': true,
      'Transfert de fichiers': true,
      'Transmettre le son': true,
    },
    'Non surveillé': {
      "Afficher l'écran": true,
      'Clavier et souris': true,
      'Presse-papiers': true,
      'Transfert de fichiers': true,
      'Transmettre le son': false,
    },
  };

  @override
  void dispose() {
    _aclController.dispose();
    super.dispose();
  }

  void _ajouterAcl() {
    final entree = _aclController.text.trim();
    if (entree.isEmpty) return;
    if (_acl.contains(entree)) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('Cette entrée figure déjà dans la liste.')),
      );
      return;
    }
    setState(() {
      _acl.add(entree);
      _aclController.clear();
    });
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Scaffold(
      body: NovaAppFrame(
        ongletActif: NovaOnglet.reglages,
        corps: DefaultTabController(
          length: 6,
          child: Column(
            children: [
              Container(
                decoration: BoxDecoration(
                  color: t.barre,
                  border: Border(bottom: BorderSide(color: t.filet)),
                ),
                child: const TabBar(
                  isScrollable: true,
                  tabAlignment: TabAlignment.start,
                  dividerColor: Colors.transparent,
                  tabs: [
                    Tab(height: 40, text: 'Interface'),
                    Tab(height: 40, text: 'Sécurité'),
                    Tab(height: 40, text: 'Connexion'),
                    Tab(height: 40, text: 'Affichage'),
                    Tab(height: 40, text: 'Enregistrement'),
                    Tab(height: 40, text: 'À propos'),
                  ],
                ),
              ),
              Expanded(
                child: TabBarView(
                  children: [
                    _ongletInterface(t),
                    _ongletSecurite(t),
                    _ongletConnexion(t),
                    _ongletAffichage(t),
                    _ongletEnregistrement(t),
                    _ongletAPropos(t),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Briques de mise en page
  // ---------------------------------------------------------------------------

  Widget _page(List<Widget> enfants) {
    return ListView(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 12),
      children: enfants,
    );
  }

  Widget _titreSection(NovaTokens t, String titre) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 14, 16, 6),
      child: Text(
        titre.toUpperCase(),
        style: TextStyle(
          fontSize: 10.5,
          fontWeight: FontWeight.w700,
          letterSpacing: 1.1,
          color: t.texte3,
        ),
      ),
    );
  }

  // ---------------------------------------------------------------------------
  // Onglet Interface
  // ---------------------------------------------------------------------------

  Widget _ongletInterface(NovaTokens t) {
    final modeTheme = ref.watch(themeModeProvider);
    return _page([
      _titreSection(t, 'Apparence'),
      ListTile(
        leading: const NovaIcone(NovaIcones.lune),
        title: const Text('Thème'),
        subtitle: const Text('Clair, sombre ou selon le système'),
        trailing: SegmentedButton<ThemeMode>(
          showSelectedIcon: false,
          segments: const [
            ButtonSegment(value: ThemeMode.system, label: Text('Système')),
            ButtonSegment(value: ThemeMode.light, label: Text('Clair')),
            ButtonSegment(value: ThemeMode.dark, label: Text('Sombre')),
          ],
          selected: {modeTheme},
          onSelectionChanged: (selection) =>
              ref.read(themeModeProvider.notifier).state = selection.first,
        ),
      ),
      ListTile(
        leading: const NovaIcone(NovaIcones.globe),
        title: const Text('Langue'),
        subtitle:
            const Text('Catalogues ARB multilingues à venir (plan 10 §10.7.2)'),
        trailing: DropdownButton<String>(
          value: 'fr',
          underline: const SizedBox.shrink(),
          items: const [
            DropdownMenuItem(value: 'fr', child: Text('Français')),
          ],
          onChanged: (_) {},
        ),
      ),
    ]);
  }

  // ---------------------------------------------------------------------------
  // Onglet Sécurité
  // ---------------------------------------------------------------------------

  Widget _ongletSecurite(NovaTokens t) {
    return _page([
      _titreSection(t, 'Connexions entrantes'),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.bouclierCoche),
        title: const Text("Demander confirmation à l'utilisateur"),
        subtitle: const Text(
            'Chaque connexion entrante requiert une autorisation explicite'),
        value: _confirmationRequise,
        onChanged: (valeur) => setState(() => _confirmationRequise = valeur),
      ),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.cadenas),
        title: const Text("Verrouiller l'écran en fin de session"),
        value: _verrouillerEnFin,
        onChanged: (valeur) => setState(() => _verrouillerEnFin = valeur),
      ),
      ListTile(
        leading: const NovaIcone(NovaIcones.bouclier),
        title: const Text('Accès non surveillé'),
        subtitle:
            const Text('Mot de passe permanent, appareils de confiance, TOTP…'),
        trailing: const NovaIcone(NovaIcones.chevronDroit, taille: 15),
        onTap: () => Navigator.of(context).pushNamed(UnattendedScreen.route),
      ),
      ListTile(
        leading: const NovaIcone(NovaIcones.utilisateur),
        title: const Text("Tester le dialogue d'acceptation"),
        subtitle: const Text(
            'Aperçu de la fenêtre présentée lors d’une connexion entrante'),
        trailing: const NovaIcone(NovaIcones.chevronDroit, taille: 15),
        onTap: () async {
          final reponse = await IncomingRequestDialog.montrer(context);
          if (reponse == null || !mounted) return;
          ScaffoldMessenger.of(context).showSnackBar(
            SnackBar(
              content: Text(reponse.acceptee
                  ? 'Session entrante acceptée (démo) — profil '
                      '« ${reponse.profil.libelleDemo} ».'
                  : 'Session entrante refusée (démo).'),
            ),
          );
        },
      ),
      _titreSection(t, 'Liste blanche (ACL)'),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.liste),
        title: const Text('Liste blanche uniquement'),
        subtitle:
            const Text('Refuser tout poste absent de la liste ci-dessous'),
        value: _listeBlancheSeule,
        onChanged: (valeur) => setState(() => _listeBlancheSeule = valeur),
      ),
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 6, 16, 4),
        child: Row(
          children: [
            Expanded(
              child: TextField(
                controller: _aclController,
                decoration: const InputDecoration(
                  hintText: 'Adresse, alias ou joker (*@espace)',
                ),
                onSubmitted: (_) => _ajouterAcl(),
              ),
            ),
            const SizedBox(width: 8),
            OutlinedButton(
              onPressed: _ajouterAcl,
              child: const Text('Ajouter'),
            ),
          ],
        ),
      ),
      for (final entree in _acl)
        ListTile(
          leading: NovaIcone(
            entree.startsWith('*') ? NovaIcones.etoile : NovaIcones.moniteur,
            taille: 15,
          ),
          title: Text(entree),
          subtitle: entree.startsWith('*@')
              ? Text('Joker : tout l’espace « ${entree.substring(2)} »')
              : null,
          trailing: IconButton(
            tooltip: 'Retirer',
            icon: const NovaIcone(NovaIcones.corbeille, taille: 15),
            onPressed: () => setState(() => _acl.remove(entree)),
          ),
        ),
      _titreSection(t, 'Profils de permissions'),
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 6),
        child: Text(
          'Chaque profil précoche les permissions proposées à l’acceptation '
          'd’une connexion entrante.',
          style: TextStyle(fontSize: 11.5, color: t.texte3),
        ),
      ),
      for (final profil in _profils.entries)
        ExpansionTile(
          leading: const NovaIcone(NovaIcones.bouclier, taille: 16),
          title: Text(profil.key),
          subtitle: Text(
            '${profil.value.values.where((v) => v).length} permission(s) '
            'accordée(s)',
            style: TextStyle(fontSize: 11, color: t.texte3),
          ),
          childrenPadding: const EdgeInsets.only(left: 34, bottom: 6),
          children: [
            for (final permission in profil.value.entries)
              SizedBox(
                height: 32,
                child: Row(
                  children: [
                    SizedBox(
                      width: 28,
                      child: Checkbox(
                        value: permission.value,
                        onChanged: (valeur) => setState(() =>
                            profil.value[permission.key] = valeur ?? false),
                      ),
                    ),
                    const SizedBox(width: 4),
                    Text(permission.key,
                        style: const TextStyle(fontSize: 12.5)),
                  ],
                ),
              ),
          ],
        ),
    ]);
  }

  // ---------------------------------------------------------------------------
  // Onglet Connexion
  // ---------------------------------------------------------------------------

  Widget _ongletConnexion(NovaTokens t) {
    return _page([
      _titreSection(t, 'Réseau'),
      ListTile(
        leading: const NovaIcone(NovaIcones.globe),
        title: const Text('Mode de connexion'),
        subtitle: const Text('P2P direct quand possible, sinon relais'),
        trailing: DropdownButton<String>(
          value: _modeReseau,
          underline: const SizedBox.shrink(),
          items: const [
            DropdownMenuItem(value: 'Automatique', child: Text('Automatique')),
            DropdownMenuItem(value: 'P2P direct', child: Text('P2P direct')),
            DropdownMenuItem(
                value: 'Relais uniquement', child: Text('Relais uniquement')),
          ],
          onChanged: (valeur) =>
              setState(() => _modeReseau = valeur ?? _modeReseau),
        ),
      ),
      ListTile(
        leading: const NovaIcone(NovaIcones.qualite),
        title: const Text('Limite de bande passante'),
        trailing: DropdownButton<String>(
          value: _bandePassante,
          underline: const SizedBox.shrink(),
          items: const [
            DropdownMenuItem(value: 'Illimitée', child: Text('Illimitée')),
            DropdownMenuItem(value: '10 Mo/s', child: Text('10 Mo/s')),
            DropdownMenuItem(value: '2 Mo/s', child: Text('2 Mo/s')),
          ],
          onChanged: (valeur) =>
              setState(() => _bandePassante = valeur ?? _bandePassante),
        ),
      ),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.moniteurs),
        title: const Text('Découverte du réseau local'),
        subtitle: const Text('Annoncer ce poste aux pairs du LAN (plan 13)'),
        value: _decouverteLan,
        onChanged: (valeur) => setState(() => _decouverteLan = valeur),
      ),
    ]);
  }

  // ---------------------------------------------------------------------------
  // Onglet Affichage
  // ---------------------------------------------------------------------------

  Widget _ongletAffichage(NovaTokens t) {
    return _page([
      _titreSection(t, 'Qualité par défaut'),
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 4, 16, 8),
        child: SegmentedButton<String>(
          showSelectedIcon: false,
          segments: const [
            ButtonSegment(
                value: 'Meilleure qualité', label: Text('Meilleure qualité')),
            ButtonSegment(value: 'Équilibré', label: Text('Équilibré')),
            ButtonSegment(
                value: 'Meilleures performances',
                label: Text('Meilleures perfs')),
          ],
          selected: {_qualiteDefaut},
          onSelectionChanged: (selection) =>
              setState(() => _qualiteDefaut = selection.first),
        ),
      ),
      _titreSection(t, 'Rendu'),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.pleinEcran),
        title: const Text('Adapter la résolution à la fenêtre'),
        value: _adapterResolution,
        onChanged: (valeur) => setState(() => _adapterResolution = valeur),
      ),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.souris),
        title: const Text('Afficher le curseur distant'),
        value: _curseurDistant,
        onChanged: (valeur) => setState(() => _curseurDistant = valeur),
      ),
    ]);
  }

  // ---------------------------------------------------------------------------
  // Onglet Enregistrement
  // ---------------------------------------------------------------------------

  Widget _ongletEnregistrement(NovaTokens t) {
    return _page([
      _titreSection(t, 'Sessions'),
      SwitchListTile(
        secondary: const NovaIcone(NovaIcones.enregistrer),
        title: const Text('Enregistrer automatiquement les sessions'),
        subtitle: const Text('Démarre l’enregistrement à chaque connexion'),
        value: _enregistrementAuto,
        onChanged: (valeur) => setState(() => _enregistrementAuto = valeur),
      ),
      ListTile(
        leading: const NovaIcone(NovaIcones.dossier),
        title: const Text('Dossier des enregistrements'),
        subtitle: const Text(r'C:\Users\…\Vidéos\NovaDesk'),
        trailing: OutlinedButton(
          onPressed: () {
            ScaffoldMessenger.of(context).showSnackBar(
              const SnackBar(
                  content: Text('Choix du dossier — à venir (lot 04).')),
            );
          },
          child: const Text('Modifier'),
        ),
      ),
      ListTile(
        leading: const NovaIcone(NovaIcones.capture),
        title: const Text('Format'),
        trailing: DropdownButton<String>(
          value: _formatEnregistrement,
          underline: const SizedBox.shrink(),
          items: const [
            DropdownMenuItem(value: 'NDR (natif)', child: Text('NDR (natif)')),
            DropdownMenuItem(value: 'MP4', child: Text('MP4')),
          ],
          onChanged: (valeur) => setState(
              () => _formatEnregistrement = valeur ?? _formatEnregistrement),
        ),
      ),
    ]);
  }

  // ---------------------------------------------------------------------------
  // Onglet À propos
  // ---------------------------------------------------------------------------

  Widget _ongletAPropos(NovaTokens t) {
    final appInfo = ref.watch(appInfoProvider);
    return _page([
      _titreSection(t, 'Versions'),
      ListTile(
        leading: const NovaIcone(NovaIcones.terminal),
        title: const Text('Moteur (cœur Rust)'),
        subtitle: appInfo.when(
          data: (info) => Text('NovaDesk ${info.version} — '
              'chiffrement TLS 1.3 + Noise_IK'),
          loading: () => const Text('…'),
          error: (e, _) => const Text('indisponible'),
        ),
      ),
      const ListTile(
        leading: NovaIcone(NovaIcones.moniteur),
        title: Text('Interface'),
        subtitle: Text('novadesk_ui 0.1.0 — Flutter, Material 3'),
      ),
      _titreSection(t, 'Identité de ce poste'),
      ListTile(
        leading: const NovaIcone(NovaIcones.cle),
        title: const Text('Empreinte de ce poste'),
        // FICTIF : l'empreinte réelle viendra du cœur (plan 06).
        subtitle: const Text('9A:F2:04:6B:D8:33:71:CE:…:E1'),
        trailing: OutlinedButton(
          onPressed: () => _montrerEmpreinte(context),
          child: const Text('Afficher'),
        ),
      ),
    ]);
  }

  void _montrerEmpreinte(BuildContext context) {
    showDialog<void>(
      context: context,
      builder: (context) {
        final t = NovaTokens.of(context);
        return AlertDialog(
          title: const Text('Empreinte de ce poste'),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Container(
                padding: const EdgeInsets.all(12),
                decoration: BoxDecoration(
                  color: t.panneau,
                  borderRadius: BorderRadius.circular(7),
                  border: Border.all(color: t.filet),
                ),
                child: Text(
                  '9A:F2:04:6B:D8:33:71:CE:5D:0B:A4:E1',
                  style: TextStyle(
                    fontSize: 13,
                    color: t.texte,
                    fontFeatures: const [FontFeature.tabularFigures()],
                  ),
                ),
              ),
              const SizedBox(height: 10),
              Text(
                'Comparez cette empreinte hors bande (téléphone, message) '
                'avant d’accorder un accès permanent. Le QR de vérification '
                'sera généré par le cœur (plan 06).',
                style: TextStyle(fontSize: 11.5, color: t.texte3),
              ),
            ],
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
}

/// Libellé du profil pour le message de démonstration.
extension on ProfilPermissions {
  String get libelleDemo => switch (this) {
        ProfilPermissions.parDefaut => 'Par défaut',
        ProfilPermissions.partageEcran => "Partage d'écran",
        ProfilPermissions.controleTotal => 'Contrôle total',
        ProfilPermissions.nonSurveille => 'Non surveillé',
      };
}
