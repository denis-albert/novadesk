/// Écran « Carnet d'adresses » — fidèle à la maquette `novadesk-app.html`
/// (vue `#v-carnet`). À gauche : un rail de groupes 184 px (Tous / Favoris puis
/// les groupes distincts du carnet, plus « Nouveau groupe » et Importer /
/// Exporter). À droite : une barre de recherche + « Ajouter » surmontant un
/// tableau dense (étoile · alias+OS · adresse · étiquettes · dernière connexion
/// · état · actions révélées au survol).
///
/// « Se connecter », « Observer » et « Transfert de fichiers » ouvrent une
/// **vraie session** (mêmes appels de façade que l'accueil : `new_session_config`
/// → fenêtre de session par rendez-vous), avec les permissions du mode choisi.
/// « Wake-on-LAN » demande l'adresse MAC (le carnet n'en stocke pas) puis émet
/// le paquet magique **réel** via la façade (`send_wol`).
/// « Importer » / « Exporter » échangent le carnet au format **JSON** par un
/// simple fichier local (`dart:io`, aucun plugin ni sélecteur natif) : l'export
/// écrit contacts + groupes vers `Documents`, l'import ajoute les contacts
/// absents (dédoublonnés par ID) et crée les groupes manquants.
/// Le carnet est persistant ([carnetProvider]) ; favori, renommage, retrait et
/// groupes sont persistés. Chargement initial simulé par des squelettes shimmer
/// (~780 ms), comme `skTrs` dans la maquette.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';

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
import 'session_screen.dart';

/// Clés de groupe réservées (les autres clés sont des noms de groupe libres).
const String _cleTous = 'Tous';
const String _cleFavoris = 'Favoris';

/// Nom d'affichage des contacts sans groupe (voir `EntreeCarnet.depuisContact`) ;
/// à l'export on restitue un groupe vide pour un aller-retour fidèle.
const String _groupeSansGroupe = 'Sans groupe';

/// Durée du faux chargement initial (squelettes), calquée sur la maquette.
const Duration _dureeChargement = Duration(milliseconds: 780);

class AddressBookScreen extends ConsumerStatefulWidget {
  const AddressBookScreen({super.key});

  static const String route = NovaRoutes.carnet;

  @override
  ConsumerState<AddressBookScreen> createState() => _AddressBookScreenState();
}

class _AddressBookScreenState extends ConsumerState<AddressBookScreen> {
  final TextEditingController _controleurRecherche = TextEditingController();

  /// Groupe filtrant le tableau (`Tous` par défaut).
  String _groupeSelectionne = _cleTous;

  /// Identifiant de la ligne sélectionnée (`null` si aucune).
  int? _idSelectionne;

  /// Terme de recherche courant (alias ou adresse).
  String _recherche = '';

  /// Vrai pendant le faux chargement initial (squelettes).
  bool _chargement = true;
  Timer? _minuteurChargement;

  /// Verrou : une seule ouverture de session à la fois.
  bool _connexionEnCours = false;

  /// Adresses MAC saisies pour le Wake-on-LAN pendant la session, par ID de
  /// contact : le carnet ne stocke pas de MAC, on pré-remplit donc le dialogue
  /// avec la dernière saisie (mémoire d'écran, best-effort).
  final Map<int, String> _macWolMemorisees = <int, String>{};

  @override
  void initState() {
    super.initState();
    // Révélation différée des lignes réelles, comme la maquette (~780 ms).
    _minuteurChargement = Timer(_dureeChargement, () {
      if (mounted) setState(() => _chargement = false);
    });
  }

  @override
  void dispose() {
    _minuteurChargement?.cancel();
    _controleurRecherche.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Données dérivées
  // ---------------------------------------------------------------------------

  /// Formatage local d'un ID (repli synchrone strictement identique à
  /// `MockNativeApi._formater` : 9 chiffres complétés à gauche, groupés par 3
  /// depuis la droite → « 421887330 » devient « 421 887 330 »). Utilisé pour la
  /// recherche et comme affichage d'attente avant résolution du
  /// [idFormateProvider].
  static String _formaterIdLocal(int id) {
    var chiffres = id.toString();
    if (chiffres.length < 9) {
      chiffres = chiffres.padLeft(9, '0');
    }
    final groupes = <String>[];
    for (var fin = chiffres.length; fin > 0; fin -= 3) {
      final debut = fin - 3 < 0 ? 0 : fin - 3;
      groupes.insert(0, chiffres.substring(debut, fin));
    }
    return groupes.join(' ');
  }

  /// Adresse formatée d'une entrée, via la façade (repli local en attendant).
  String _adresse(EntreeCarnet e) {
    return ref
        .watch(idFormateProvider(e.id))
        .maybeWhen(data: (v) => v, orElse: () => _formaterIdLocal(e.id));
  }

  /// Construit la liste des groupes : « Tous », « Favoris », puis un groupe par
  /// valeur distincte de `groupe` (comptes calculés dynamiquement).
  List<_Groupe> _calculerGroupes(
      List<EntreeCarnet> entrees, List<String> groupesDeclares, NovaTokens t) {
    final favoris = entrees.where((e) => e.favori).length;
    final noms = <String>[];
    final comptes = <String, int>{};
    // Groupes déclarés (via `list_groups`) d'abord, même vides de contacts.
    for (final nom in groupesDeclares) {
      if (!noms.contains(nom)) noms.add(nom);
      comptes.putIfAbsent(nom, () => 0);
    }
    for (final e in entrees) {
      comptes[e.groupe] = (comptes[e.groupe] ?? 0) + 1;
      if (!noms.contains(e.groupe)) noms.add(e.groupe);
    }
    return [
      _Groupe(_cleTous, 'Tous', NovaIcones.liste, entrees.length),
      _Groupe(_cleFavoris, 'Favoris', NovaIcones.etoile, favoris,
          couleurIcone: t.ambre),
      for (final nom in noms)
        _Groupe(nom, nom, NovaIcones.fichiers, comptes[nom] ?? 0),
    ];
  }

  /// Applique le filtre de groupe puis la recherche (alias / adresse / ID brut).
  List<EntreeCarnet> _filtrer(List<EntreeCarnet> entrees) {
    Iterable<EntreeCarnet> res = entrees;
    if (_groupeSelectionne == _cleFavoris) {
      res = res.where((e) => e.favori);
    } else if (_groupeSelectionne != _cleTous) {
      res = res.where((e) => e.groupe == _groupeSelectionne);
    }
    final q = _recherche.trim().toLowerCase();
    if (q.isNotEmpty) {
      res = res.where((e) {
        return e.alias.toLowerCase().contains(q) ||
            _formaterIdLocal(e.id).contains(q) ||
            e.id.toString().contains(q);
      });
    }
    return res.toList();
  }

  // ---------------------------------------------------------------------------
  // Actions (sessions réelles + mutations persistantes du carnet)
  // ---------------------------------------------------------------------------

  /// Ouvre une vraie session vers [e] avec les [permissions] données — même
  /// parcours que l'accueil : `new_session_config` puis fenêtre de session en
  /// mise en relation par rendez-vous ([SessionEndpointByRendezvous]).
  Future<void> _ouvrirSession(EntreeCarnet e, PermissionsDto permissions) async {
    if (_connexionEnCours) return;
    setState(() => _connexionEnCours = true);
    try {
      final api = ref.read(nativeApiProvider);
      final config = await api.newSessionConfig(
        role: SessionRoleDto.controller,
        localId: ref.read(idLocalProvider),
        peerId: e.id,
        permissions: permissions,
      );
      final endpoint = SessionEndpointByRendezvous(
        server: ref.read(rendezvousProvider),
        stunServers: ref.read(stunServersProvider),
        relay: ref.read(relayProvider),
      );
      // Journalise la session (historique + dernière connexion du contact).
      await api.recordSession(id: e.id, alias: e.alias);
      ref.invalidate(recentSessionsProvider);
      ref.invalidate(carnetProvider);
      if (!mounted) return;
      await Navigator.of(context).pushNamed(
        SessionScreen.route,
        arguments: SessionScreenArgs(
          config: config,
          libellePair: e.alias,
          endpoint: endpoint,
          options: SessionOptionsDto(permissions: permissions),
        ),
      );
    } on NovaApiException catch (ex) {
      if (mounted) NovaToast.montrer(context, ex.message, info: true);
    } finally {
      if (mounted) setState(() => _connexionEnCours = false);
    }
  }

  void _seConnecter(EntreeCarnet e) =>
      unawaited(_ouvrirSession(e, PermissionsDto.full()));

  void _observer(EntreeCarnet e) =>
      unawaited(_ouvrirSession(e, PermissionsDto.viewOnly()));

  void _transfertFichiers(EntreeCarnet e) => unawaited(_ouvrirSession(
        e,
        const PermissionsDto(
          keyboard: false,
          mouse: false,
          clipboard: false,
          files: true,
          audio: false,
          viewOnly: true,
        ),
      ));

  Future<void> _renommer(EntreeCarnet e) async {
    final controller = TextEditingController(text: e.alias);
    final nouveau = await montrerDialogueNova<String>(
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
              child: const Text('Annuler')),
          FilledButton(
              onPressed: () => Navigator.of(context).pop(controller.text),
              child: const Text('Renommer')),
        ],
      ),
    );
    controller.dispose();
    final alias = nouveau?.trim();
    if (alias == null || alias.isEmpty) return;
    try {
      await ref.read(carnetProvider.notifier).modifier(
            id: e.id,
            alias: alias,
            groupe: e.groupe,
            etiquettes: e.etiquettes,
          );
      if (mounted) {
        NovaToast.montrer(context, '${e.alias} renommé en « $alias »');
      }
    } on NovaApiException catch (ex) {
      if (mounted) NovaToast.montrer(context, ex.message, info: true);
    }
  }

  /// Réveille [e] par **Wake-on-LAN** : demande l'adresse MAC via un dialogue
  /// (le carnet n'en stocke pas) puis émet le paquet magique via la façade
  /// (`send_wol`). Diffusion vide → globale (`255.255.255.255:9`).
  Future<void> _wakeOnLan(EntreeCarnet e) async {
    final parametres = await _demanderParametresWol(e);
    if (parametres == null || !mounted) return;
    // Mémorise la MAC (normalisée) pour pré-remplir le prochain réveil.
    _macWolMemorisees[e.id] = parametres.mac;
    try {
      await ref
          .read(nativeApiProvider)
          .sendWol(parametres.mac, broadcast: parametres.broadcast);
      if (!mounted) return;
      NovaToast.montrer(context, 'Paquet de réveil envoyé à ${e.alias}');
    } on NovaApiException catch (ex) {
      if (mounted) NovaToast.montrer(context, ex.message, info: true);
    }
  }

  /// Dialogue « Réveiller {alias} » : champ **Adresse MAC** obligatoire
  /// (formats `AA:BB:CC:DD:EE:FF`, `AA-BB-…` ou `AABB…` tolérés, pré-rempli si
  /// déjà saisie pendant la session) et champ **Broadcast** facultatif
  /// (« ip:port », vide → diffusion globale). Renvoie la MAC normalisée et la
  /// diffusion (`null` si vide), ou `null` si l'utilisateur annule.
  Future<({String mac, String? broadcast})?> _demanderParametresWol(
      EntreeCarnet e) async {
    final macController =
        TextEditingController(text: _macWolMemorisees[e.id] ?? '');
    final broadcastController = TextEditingController();
    String? erreurMac;
    final parametres =
        await montrerDialogueNova<({String mac, String? broadcast})>(
      context: context,
      builder: (context) => StatefulBuilder(
        builder: (context, setEtat) {
          // Valide la MAC ; en cas d'échec, affiche l'erreur sans fermer.
          void valider() {
            final mac = _normaliserMac(macController.text);
            if (mac == null) {
              setEtat(() => erreurMac =
                  'Adresse MAC invalide — attendu AA:BB:CC:DD:EE:FF.');
              return;
            }
            final broadcast = broadcastController.text.trim();
            Navigator.of(context).pop(
                (mac: mac, broadcast: broadcast.isEmpty ? null : broadcast));
          }

          return AlertDialog(
            title: Text('Réveiller ${e.alias}'),
            content: SizedBox(
              width: 360,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  TextField(
                    controller: macController,
                    autofocus: true,
                    decoration: InputDecoration(
                      labelText: 'Adresse MAC',
                      hintText: 'AA:BB:CC:DD:EE:FF',
                      errorText: erreurMac,
                      errorMaxLines: 2,
                    ),
                    onChanged: (_) {
                      if (erreurMac != null) setEtat(() => erreurMac = null);
                    },
                    onSubmitted: (_) => valider(),
                  ),
                  const SizedBox(height: 10),
                  TextField(
                    controller: broadcastController,
                    decoration: const InputDecoration(
                      labelText: 'Broadcast (facultatif)',
                      hintText: '255.255.255.255:9',
                    ),
                    onSubmitted: (_) => valider(),
                  ),
                ],
              ),
            ),
            actions: [
              TextButton(
                onPressed: () => Navigator.of(context).pop(),
                child: const Text('Annuler'),
              ),
              FilledButton(
                onPressed: valider,
                child: const Text('Réveiller'),
              ),
            ],
          );
        },
      ),
    );
    macController.dispose();
    broadcastController.dispose();
    return parametres;
  }

  Future<void> _basculerFavori(EntreeCarnet e) async {
    final devientFavori = !e.favori;
    try {
      await ref.read(carnetProvider.notifier).basculerFavori(e.id, devientFavori);
      if (mounted) {
        NovaToast.montrer(
          context,
          devientFavori
              ? '${e.alias} ajouté aux favoris'
              : '${e.alias} retiré des favoris',
        );
      }
    } on NovaApiException catch (ex) {
      if (mounted) NovaToast.montrer(context, ex.message, info: true);
    }
  }

  Future<void> _supprimer(EntreeCarnet e) async {
    try {
      await ref.read(carnetProvider.notifier).supprimer(e.id);
      if (!mounted) return;
      if (_idSelectionne == e.id) {
        setState(() => _idSelectionne = null);
      }
      NovaToast.montrer(context, '${e.alias} supprimé du carnet');
    } on NovaApiException catch (ex) {
      if (mounted) NovaToast.montrer(context, ex.message, info: true);
    }
  }

  /// Ajoute un appareil au carnet (`add_contact`) via un dialogue de saisie.
  Future<void> _ajouterContact() async {
    final aliasController = TextEditingController();
    final idController = TextEditingController();
    final groupeInitial =
        (_groupeSelectionne != _cleTous && _groupeSelectionne != _cleFavoris)
            ? _groupeSelectionne
            : '';
    final groupeController = TextEditingController(text: groupeInitial);
    final etiquettesController = TextEditingController();

    final valide = await montrerDialogueNova<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Ajouter un appareil'),
        content: SizedBox(
          width: 360,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextField(
                controller: aliasController,
                autofocus: true,
                decoration: const InputDecoration(labelText: 'Alias'),
              ),
              const SizedBox(height: 10),
              NovaIdField(controller: idController, libelle: 'ID NovaDesk'),
              const SizedBox(height: 10),
              TextField(
                controller: groupeController,
                decoration:
                    const InputDecoration(labelText: 'Groupe (facultatif)'),
              ),
              const SizedBox(height: 10),
              TextField(
                controller: etiquettesController,
                decoration: const InputDecoration(
                    labelText: 'Étiquettes (séparées par des virgules)'),
              ),
            ],
          ),
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

    if (valide == true) {
      try {
        final api = ref.read(nativeApiProvider);
        final id = await api.parseNovaId(texte: idController.text);
        final alias = aliasController.text.trim().isEmpty
            ? _formaterIdLocal(id)
            : aliasController.text.trim();
        final etiquettes = etiquettesController.text
            .split(',')
            .map((s) => s.trim())
            .where((s) => s.isNotEmpty)
            .toList();
        await ref.read(carnetProvider.notifier).ajouter(
              alias: alias,
              id: id,
              groupe: groupeController.text.trim(),
              etiquettes: etiquettes,
            );
        if (mounted) NovaToast.montrer(context, '$alias ajouté au carnet');
      } on NovaApiException catch (e) {
        if (mounted) NovaToast.montrer(context, e.message, info: true);
      }
    }
    aliasController.dispose();
    idController.dispose();
    groupeController.dispose();
    etiquettesController.dispose();
  }

  /// Crée un nouveau groupe (`add_group`) via un dialogue de saisie.
  Future<void> _nouveauGroupe() async {
    final controller = TextEditingController();
    final nom = await montrerDialogueNova<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Nouveau groupe'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(labelText: 'Nom du groupe'),
          onSubmitted: (v) => Navigator.of(context).pop(v),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(context).pop(),
              child: const Text('Annuler')),
          FilledButton(
              onPressed: () => Navigator.of(context).pop(controller.text),
              child: const Text('Créer')),
        ],
      ),
    );
    controller.dispose();
    final nomGroupe = nom?.trim();
    if (nomGroupe == null || nomGroupe.isEmpty) return;
    try {
      await ref.read(carnetProvider.notifier).ajouterGroupe(nomGroupe);
      if (!mounted) return;
      setState(() => _groupeSelectionne = nomGroupe);
      NovaToast.montrer(context, 'Groupe « $nomGroupe » créé');
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  // ---------------------------------------------------------------------------
  // Import / Export du carnet (fichier JSON local via dart:io — aucun plugin)
  // ---------------------------------------------------------------------------

  /// Dossier proposé pour l'échange : `Documents` du profil utilisateur s'il
  /// existe, sinon le Bureau, sinon le profil lui-même (repli : dossier
  /// courant). Aucun sélecteur natif ici (ni admin ni plugins) : le dialogue
  /// se contente d'un champ de chemin pré-rempli.
  static String _dossierEchangeParDefaut() {
    final profil = Platform.environment['USERPROFILE'] ??
        Platform.environment['HOME'] ??
        '';
    if (profil.isEmpty) return Directory.current.path;
    for (final nom in const ['Documents', 'Desktop']) {
      final dossier = Directory('$profil${Platform.pathSeparator}$nom');
      if (dossier.existsSync()) return dossier.path;
    }
    return profil;
  }

  /// Nom de fichier proposé : `carnet-novadesk-AAAA-MM-JJ.json` (date du jour).
  static String _nomExportDuJour() {
    final d = DateTime.now();
    return 'carnet-novadesk-${d.year}-${d.month.toString().padLeft(2, '0')}-'
        '${d.day.toString().padLeft(2, '0')}.json';
  }

  /// Chemin complet proposé par défaut dans les dialogues Import/Export.
  static String _cheminEchangeParDefaut() =>
      '${_dossierEchangeParDefaut()}${Platform.pathSeparator}'
      '${_nomExportDuJour()}';

  /// Dialogue commun Import/Export : un champ **chemin de fichier** pré-rempli
  /// (fidèle au style des autres dialogues du carnet). Renvoie le chemin
  /// saisi, ou `null` si l'utilisateur annule ou laisse le champ vide.
  Future<String?> _demanderCheminFichier({
    required String titre,
    required String libelleAction,
    required String aide,
  }) async {
    final controller = TextEditingController(text: _cheminEchangeParDefaut());
    final chemin = await montrerDialogueNova<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(titre),
        content: SizedBox(
          width: 430,
          child: TextField(
            controller: controller,
            autofocus: true,
            decoration: InputDecoration(
              labelText: 'Chemin du fichier JSON',
              helperText: aide,
              helperMaxLines: 3,
            ),
            onSubmitted: (v) => Navigator.of(context).pop(v),
          ),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(context).pop(),
              child: const Text('Annuler')),
          FilledButton(
              onPressed: () => Navigator.of(context).pop(controller.text),
              child: Text(libelleAction)),
        ],
      ),
    );
    controller.dispose();
    final nettoye = chemin?.trim();
    return (nettoye == null || nettoye.isEmpty) ? null : nettoye;
  }

  /// Exporte le carnet en **JSON** (contacts + groupes déclarés) vers un
  /// fichier écrit via `dart:io`. Un chemin de dossier saisi tel quel reçoit
  /// le nom du jour ; un export existant est écrasé. Toast de succès avec le
  /// chemin, erreurs d'écriture en toast d'information.
  Future<void> _exporterCarnet() async {
    final entrees =
        ref.read(carnetProvider).valueOrNull ?? const <EntreeCarnet>[];
    final groupesDeclares =
        ref.read(groupesProvider).valueOrNull ?? const <String>[];
    if (entrees.isEmpty && groupesDeclares.isEmpty) {
      NovaToast.montrer(context, 'Le carnet est vide — rien à exporter.',
          info: true);
      return;
    }
    final chemin = await _demanderCheminFichier(
      titre: 'Exporter le carnet',
      libelleAction: 'Exporter',
      aide: '${entrees.length} contact(s) et ${groupesDeclares.length} '
          'groupe(s) seront écrits dans ce fichier.',
    );
    if (chemin == null || !mounted) return;
    var cible = chemin;
    if (FileSystemEntity.isDirectorySync(cible)) {
      cible = '$cible${Platform.pathSeparator}${_nomExportDuJour()}';
    }
    final donnees = <String, Object?>{
      'format': 'carnet-novadesk',
      'version': 1,
      'exporteLe': DateTime.now().toIso8601String(),
      'groupes': groupesDeclares,
      'contacts': [
        for (final e in entrees)
          <String, Object?>{
            'id': e.id,
            'alias': e.alias,
            'groupe': e.groupe == _groupeSansGroupe ? '' : e.groupe,
            'etiquettes': e.etiquettes,
            'favori': e.favori,
          },
      ],
    };
    try {
      final fichier = File(cible);
      await fichier.parent.create(recursive: true);
      await fichier
          .writeAsString(const JsonEncoder.withIndent('  ').convert(donnees));
    } on FileSystemException catch (e) {
      if (mounted) {
        NovaToast.montrer(context, 'Export impossible vers $cible — ${e.message}',
            info: true);
      }
      return;
    }
    if (mounted) {
      NovaToast.montrer(
          context, 'Carnet exporté (${entrees.length} contact(s)) → $cible');
    }
  }

  /// Importe un carnet **JSON** (export NovaDesk `{groupes, contacts}` ; une
  /// liste nue de contacts est aussi acceptée). Les contacts sont ajoutés via
  /// la façade ([CarnetNotifier.ajouter]) en dédoublonnant par ID, les favoris
  /// restaurés et les groupes manquants créés. Fichier introuvable ou JSON
  /// invalide → toast d'information, rien n'est modifié.
  Future<void> _importerCarnet() async {
    final chemin = await _demanderCheminFichier(
      titre: 'Importer un carnet',
      libelleAction: 'Importer',
      aide: 'Fichier JSON produit par « Exporter » — contacts ajoutés sans '
          'doublon, groupes créés au besoin.',
    );
    if (chemin == null || !mounted) return;
    String texte;
    try {
      texte = await File(chemin).readAsString();
    } on FileSystemException {
      if (mounted) {
        NovaToast.montrer(context, 'Fichier introuvable ou illisible : $chemin',
            info: true);
      }
      return;
    }
    Object? racine;
    try {
      racine = jsonDecode(texte);
    } on FormatException {
      if (mounted) {
        NovaToast.montrer(
            context, 'JSON invalide — fichier d\'export NovaDesk attendu.',
            info: true);
      }
      return;
    }
    if (!mounted) return;
    final List<dynamic>? contactsBruts = racine is Map
        ? (racine['contacts'] is List
            ? racine['contacts'] as List
            : const <dynamic>[])
        : (racine is List ? racine : null);
    final List<dynamic> groupesBruts =
        racine is Map && racine['groupes'] is List
            ? racine['groupes'] as List
            : const <dynamic>[];
    if (contactsBruts == null) {
      NovaToast.montrer(context,
          'Format inattendu — objet {"contacts": […]} ou liste attendus.',
          info: true);
      return;
    }

    final notifier = ref.read(carnetProvider.notifier);
    final existants =
        ref.read(carnetProvider).valueOrNull ?? const <EntreeCarnet>[];
    final idsConnus = {for (final e in existants) e.id};
    final groupesConnus = <String>{
      ...ref.read(groupesProvider).valueOrNull ?? const <String>[],
      for (final e in existants) e.groupe,
    };

    // Groupes déclarés d'abord (même vides de contacts), sans doublon —
    // `add_group` lèverait sur un nom déjà présent côté cœur.
    for (final brut in groupesBruts) {
      final nom = brut is String ? brut.trim() : '';
      if (nom.isEmpty ||
          nom == _groupeSansGroupe ||
          groupesConnus.contains(nom)) {
        continue;
      }
      try {
        await notifier.ajouterGroupe(nom);
      } on NovaApiException {
        // Déjà présent côté cœur : rien à faire.
      }
      groupesConnus.add(nom);
    }

    var importes = 0;
    var ignores = 0; // doublons d'ID (fichier ou carnet) et entrées invalides
    for (final brut in contactsBruts) {
      if (brut is! Map) {
        ignores++;
        continue;
      }
      final idBrut = brut['id'];
      final id = idBrut is int ? idBrut : int.tryParse('$idBrut');
      if (id == null || id <= 0 || idsConnus.contains(id)) {
        ignores++;
        continue;
      }
      final aliasBrut = brut['alias'];
      final alias = aliasBrut is String && aliasBrut.trim().isNotEmpty
          ? aliasBrut.trim()
          : _formaterIdLocal(id);
      final groupeBrut = brut['groupe'];
      final groupe =
          groupeBrut is String && groupeBrut.trim() != _groupeSansGroupe
              ? groupeBrut.trim()
              : '';
      final etiquettesBrutes = brut['etiquettes'];
      final etiquettes = [
        if (etiquettesBrutes is List)
          for (final e in etiquettesBrutes)
            if (e is String && e.trim().isNotEmpty) e.trim(),
      ];
      try {
        await notifier.ajouter(
            alias: alias, id: id, groupe: groupe, etiquettes: etiquettes);
        if (brut['favori'] == true) {
          await notifier.basculerFavori(id, true);
        }
        idsConnus.add(id);
        if (groupe.isNotEmpty) groupesConnus.add(groupe);
        importes++;
      } on NovaApiException {
        ignores++; // ID déjà présent côté cœur (état plus frais que l'écran).
      }
    }
    if (!mounted) return;
    if (importes == 0) {
      NovaToast.montrer(
        context,
        ignores > 0
            ? 'Aucun contact importé — $ignores déjà présent(s) ou invalide(s).'
            : 'Aucun contact à importer dans ce fichier.',
        info: true,
      );
      return;
    }
    NovaToast.montrer(
      context,
      '$importes contact(s) importé(s)'
      '${ignores > 0 ? ' · $ignores ignoré(s) (doublons/invalides)' : ''}',
    );
  }

  /// Ouvre le menu contextuel d'une entrée à la position écran [position]
  /// (clic droit sur la ligne ou bouton « ⋯ »), puis traite le choix.
  Future<void> _ouvrirMenu(EntreeCarnet e, Offset position) async {
    setState(() => _idSelectionne = e.id);
    final cle = await showNovaContextMenu(context, position, const [
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
    if (!mounted || cle == null) return;
    switch (cle) {
      case 'conn':
        _seConnecter(e);
        break;
      case 'obs':
        _observer(e);
        break;
      case 'ft':
        _transfertFichiers(e);
        break;
      case 'fav':
        unawaited(_basculerFavori(e));
        break;
      case 'ren':
        unawaited(_renommer(e));
        break;
      case 'wol':
        unawaited(_wakeOnLan(e));
        break;
      case 'del':
        unawaited(_supprimer(e));
        break;
    }
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final asyncCarnet = ref.watch(carnetProvider);
    final entrees = asyncCarnet.valueOrNull ?? const <EntreeCarnet>[];
    final groupesDeclares =
        ref.watch(groupesProvider).valueOrNull ?? const <String>[];
    final groupes = _calculerGroupes(entrees, groupesDeclares, t);
    final filtrees = _filtrer(entrees);
    // Pré-résolution des adresses ici : `ref.watch` doit rester dans `build`
    // (jamais dans un `itemBuilder` paresseux de ListView).
    final lignes = [for (final e in filtrees) (e, _adresse(e))];
    final chargement =
        _chargement || (asyncCarnet.isLoading && !asyncCarnet.hasValue);

    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _railGroupes(t, groupes),
        Expanded(
            child: _panneauPrincipal(t, lignes, filtrees.isEmpty, chargement)),
      ],
    );
  }

  // ---- Rail de groupes (gauche, maquette `.groups`) -------------------------

  Widget _railGroupes(NovaTokens t, List<_Groupe> groupes) {
    return Container(
      width: 184,
      decoration: BoxDecoration(
        border: Border(right: BorderSide(color: t.filet)),
      ),
      padding: const EdgeInsets.symmetric(vertical: 12, horizontal: 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const NovaSectionLabel('Groupes',
              padding: EdgeInsets.fromLTRB(8, 0, 8, 8)),
          Expanded(
            child: SingleChildScrollView(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  for (final g in groupes)
                    _LigneGroupe(
                      icone: g.icone,
                      libelle: g.libelle,
                      compte: g.compte,
                      selectionne: _groupeSelectionne == g.cle,
                      couleurIcone: g.couleurIcone,
                      onTap: () =>
                          setState(() => _groupeSelectionne = g.cle),
                    ),
                  // Ligne bleue « + Nouveau groupe » (maquette `.grp.add`).
                  _LigneGroupe(
                    icone: NovaIcones.plus,
                    libelle: 'Nouveau groupe',
                    couleurTexte: t.bleu,
                    onTap: () => unawaited(_nouveauGroupe()),
                  ),
                ],
              ),
            ),
          ),
          // Bas de rail (maquette `.grpx`) : Importer / Exporter.
          Padding(
            padding: const EdgeInsets.only(top: 10),
            child: Row(
              children: [
                Expanded(
                  child: NovaBoutonSecondaire(
                    libelle: 'Importer',
                    icone: NovaIcones.importer,
                    hauteur: 28,
                    onPressed: () => unawaited(_importerCarnet()),
                  ),
                ),
                const SizedBox(width: 6),
                Expanded(
                  child: NovaBoutonSecondaire(
                    libelle: 'Exporter',
                    icone: NovaIcones.exporter,
                    hauteur: 28,
                    onPressed: () => unawaited(_exporterCarnet()),
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  // ---- Panneau principal (droite, maquette `.bookmain`) ---------------------

  Widget _panneauPrincipal(NovaTokens t, List<(EntreeCarnet, String)> lignes,
      bool vide, bool chargement) {
    return Column(
      children: [
        // Barre : recherche + bouton primaire « Ajouter » (maquette `.bookbar`).
        Container(
          padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
          decoration: BoxDecoration(
            border: Border(bottom: BorderSide(color: t.filet)),
          ),
          child: Row(
            children: [
              _champRecherche(t),
              const Spacer(),
              NovaBoutonPrimaire(
                libelle: 'Ajouter',
                icone: NovaIcones.plus,
                onPressed: () => unawaited(_ajouterContact()),
              ),
            ],
          ),
        ),
        // Tableau : en-tête figé + corps défilant.
        Expanded(
          child: Column(
            children: [
              _entete(t),
              Expanded(child: _corps(t, lignes, vide, chargement)),
            ],
          ),
        ),
      ],
    );
  }

  /// Champ de recherche (maquette `.search`) : fond panneau, filet, icône loupe.
  Widget _champRecherche(NovaTokens t) {
    return Container(
      width: 210,
      height: 32,
      padding: const EdgeInsets.symmetric(horizontal: 10),
      decoration: BoxDecoration(
        color: t.panneau,
        border: Border.all(color: t.filetFort),
        borderRadius: BorderRadius.circular(kNovaRayon),
      ),
      child: Row(
        children: [
          NovaIcone(NovaIcones.rechercher, taille: 14, couleur: t.texte3),
          const SizedBox(width: 8),
          Expanded(
            child: TextField(
              controller: _controleurRecherche,
              onChanged: (v) => setState(() => _recherche = v),
              style: TextStyle(fontSize: 12.5, color: t.texte),
              decoration: InputDecoration(
                isCollapsed: true,
                filled: false,
                border: InputBorder.none,
                enabledBorder: InputBorder.none,
                focusedBorder: InputBorder.none,
                hintText: 'Rechercher…',
                hintStyle: TextStyle(fontSize: 12.5, color: t.texte3),
              ),
            ),
          ),
        ],
      ),
    );
  }

  /// En-tête du tableau (maquette `thead th`) : capitales espacées, filet bas.
  Widget _entete(NovaTokens t) {
    Widget titre(String s) => Text(
          s.toUpperCase(),
          maxLines: 1,
          overflow: TextOverflow.clip,
          style: TextStyle(
            fontSize: 10.5,
            fontWeight: FontWeight.w600,
            letterSpacing: 0.3,
            color: t.texte3,
          ),
        );
    return Container(
      height: 34,
      padding: const EdgeInsets.symmetric(horizontal: 14),
      decoration: BoxDecoration(
        color: t.fenetre,
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      child: _rangeeColonnes(
        etoile: const SizedBox.shrink(),
        alias: titre('Alias'),
        adresse: titre('Adresse'),
        etiquettes: titre('Étiquettes'),
        derniere: titre('Dernière connexion'),
        etat: titre('État'),
        actions: const SizedBox.shrink(),
      ),
    );
  }

  /// Corps du tableau : squelettes (chargement), état vide, ou lignes réelles.
  Widget _corps(NovaTokens t, List<(EntreeCarnet, String)> lignes, bool vide,
      bool chargement) {
    if (chargement) {
      return ListView.builder(
        padding: EdgeInsets.zero,
        itemCount: 6,
        itemBuilder: (context, index) => const _LigneSquelette(),
      );
    }
    if (vide) {
      final enRecherche = _recherche.trim().isNotEmpty;
      return NovaEmptyState(
        icone: enRecherche ? NovaIcones.rechercher : NovaIcones.carnet,
        titre: enRecherche ? 'Aucun résultat' : 'Aucun appareil',
        sousTitre: enRecherche
            ? 'Aucun appareil ne correspond à « ${_recherche.trim()} ».'
            : 'Ce groupe ne contient aucun appareil pour l’instant.',
      );
    }
    return ListView.builder(
      padding: EdgeInsets.zero,
      itemCount: lignes.length,
      itemBuilder: (context, i) {
        final (entree, adresse) = lignes[i];
        return _LigneCarnet(
          entree: entree,
          adresse: adresse,
          selectionne: _idSelectionne == entree.id,
          onTap: () => setState(() => _idSelectionne = entree.id),
          onConnecter: () => _seConnecter(entree),
          onWakeOnLan: () => unawaited(_wakeOnLan(entree)),
          onMenu: (pos) => _ouvrirMenu(entree, pos),
        );
      },
    );
  }
}

// ===========================================================================
// Wake-on-LAN — validation / normalisation de l'adresse MAC saisie
// ===========================================================================

/// Normalise une adresse MAC saisie librement — séparateurs `:` ou `-` ou
/// aucun, casse indifférente — vers la forme canonique « AA:BB:CC:DD:EE:FF ».
/// Renvoie `null` si la saisie ne compte pas exactement 12 chiffres
/// hexadécimaux.
String? _normaliserMac(String saisie) {
  final hex = saisie.trim().replaceAll(RegExp(r'[:-]'), '').toUpperCase();
  if (!RegExp(r'^[0-9A-F]{12}$').hasMatch(hex)) return null;
  return [
    for (var i = 0; i < hex.length; i += 2) hex.substring(i, i + 2),
  ].join(':');
}

// ===========================================================================
// Gabarit de colonnes — partagé par l'en-tête, les lignes et les squelettes
// pour que les largeurs restent rigoureusement alignées (maquette : étoile 30,
// alias flexible, adresse 130, étiquettes flexible, dernière 150, état 110,
// actions 104). Le padding horizontal 14 est porté par chaque conteneur appelant.
// ===========================================================================

Widget _rangeeColonnes({
  required Widget etoile,
  required Widget alias,
  required Widget adresse,
  required Widget etiquettes,
  required Widget derniere,
  required Widget etat,
  required Widget actions,
}) {
  return Row(
    children: [
      SizedBox(width: 30, child: etoile),
      Expanded(flex: 3, child: alias),
      SizedBox(width: 130, child: adresse),
      Expanded(flex: 2, child: etiquettes),
      SizedBox(width: 150, child: derniere),
      SizedBox(width: 110, child: etat),
      SizedBox(width: 104, child: actions),
    ],
  );
}

// ===========================================================================
// Modèle interne d'un groupe du rail
// ===========================================================================

class _Groupe {
  const _Groupe(this.cle, this.libelle, this.icone, this.compte,
      {this.couleurIcone});

  /// Clé de sélection (`Tous`, `Favoris` ou un nom de groupe).
  final String cle;
  final String libelle;
  final IconData icone;
  final int compte;

  /// Teinte forcée de l'icône (ambre pour « Favoris »), sinon couleur du texte.
  final Color? couleurIcone;
}

// ===========================================================================
// Ligne de groupe (rail gauche) — survol / sélection (maquette `.grp`)
// ===========================================================================

class _LigneGroupe extends StatelessWidget {
  const _LigneGroupe({
    required this.icone,
    required this.libelle,
    required this.onTap,
    this.compte,
    this.selectionne = false,
    this.couleurIcone,
    this.couleurTexte,
  });

  final IconData icone;
  final String libelle;
  final VoidCallback onTap;
  final int? compte;
  final bool selectionne;
  final Color? couleurIcone;
  final Color? couleurTexte;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleurLibelle =
        couleurTexte ?? (selectionne ? t.texte : t.texte2);
    final couleurGlyphe = couleurIcone ?? couleurLibelle;
    return NovaActivable(
      onTap: onTap,
      label: libelle,
      builder: (context, survole, focus) {
        final fond = selectionne
            ? t.selection
            : (survole ? t.survol : Colors.transparent);
        return Container(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 7),
          decoration: BoxDecoration(
            color: fond,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            children: [
              NovaIcone(icone, taille: 15, couleur: couleurGlyphe),
              const SizedBox(width: 9),
              Expanded(
                child: Text(
                  libelle,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    fontSize: 12.5,
                    fontWeight:
                        selectionne ? FontWeight.w500 : FontWeight.w400,
                    color: couleurLibelle,
                  ),
                ),
              ),
              if (compte != null) ...[
                const SizedBox(width: 8),
                Text(
                  '$compte',
                  style: TextStyle(fontSize: 11, color: t.texte3),
                ),
              ],
            ],
          ),
        );
      },
    );
  }
}

// ===========================================================================
// Ligne du tableau (maquette `tbody tr`) — survol, sélection, actions, menu
// ===========================================================================

class _LigneCarnet extends StatefulWidget {
  const _LigneCarnet({
    required this.entree,
    required this.adresse,
    required this.selectionne,
    required this.onTap,
    required this.onConnecter,
    required this.onWakeOnLan,
    required this.onMenu,
  });

  final EntreeCarnet entree;
  final String adresse;
  final bool selectionne;
  final VoidCallback onTap;
  final VoidCallback onConnecter;
  final VoidCallback onWakeOnLan;
  final void Function(Offset position) onMenu;

  @override
  State<_LigneCarnet> createState() => _LigneCarnetState();
}

class _LigneCarnetState extends State<_LigneCarnet> {
  bool _survole = false;

  /// Icône OS (maquette : windows / macos / android → moniteur, linux →
  /// serveur).
  IconData get _iconeOs => switch (widget.entree.os) {
        OsAppareil.linux => NovaIcones.serveur,
        _ => NovaIcones.moniteur,
      };

  /// Ouvre le menu contextuel juste sous le bouton « ⋯ ».
  void _menuDepuisBouton(BuildContext boutonContext) {
    final rendu = boutonContext.findRenderObject();
    if (rendu is RenderBox) {
      widget.onMenu(rendu.localToGlobal(Offset(0, rendu.size.height)));
    }
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final e = widget.entree;
    final montrerActions = _survole || widget.selectionne;
    final fond = widget.selectionne
        ? t.selection
        : (_survole ? t.panneau : Colors.transparent);

    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        onSecondaryTapDown: (d) => widget.onMenu(d.globalPosition),
        behavior: HitTestBehavior.opaque,
        child: Stack(
          children: [
            Container(
              height: 42,
              padding: const EdgeInsets.symmetric(horizontal: 14),
              decoration: BoxDecoration(
                color: fond,
                border: Border(bottom: BorderSide(color: t.filet)),
              ),
              child: _rangeeColonnes(
                etoile: Align(
                  alignment: Alignment.centerLeft,
                  child: e.favori
                      ? NovaIcone(NovaIcones.etoilePleine,
                          taille: 14, couleur: t.ambre)
                      : const SizedBox.shrink(),
                ),
                alias: Row(
                  children: [
                    // Pastille OS (maquette `.osic`).
                    Container(
                      width: 26,
                      height: 26,
                      alignment: Alignment.center,
                      decoration: BoxDecoration(
                        color: t.panneau2,
                        borderRadius: BorderRadius.circular(kNovaRayon),
                      ),
                      child:
                          NovaIcone(_iconeOs, taille: 15, couleur: t.texte2),
                    ),
                    const SizedBox(width: 10),
                    Flexible(
                      child: Text(
                        e.alias,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                          fontSize: 12.5,
                          fontWeight: FontWeight.w500,
                          color: t.texte,
                        ),
                      ),
                    ),
                  ],
                ),
                adresse: Text(
                  widget.adresse,
                  maxLines: 1,
                  overflow: TextOverflow.clip,
                  style: TextStyle(
                    fontSize: 12.5,
                    color: t.texte2,
                    fontFeatures: const [FontFeature.tabularFigures()],
                  ),
                ),
                etiquettes: _etiquettes(e.etiquettes),
                derniere: Text(
                  e.derniereConnexion,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(fontSize: 12.5, color: t.texte2),
                ),
                etat: Align(
                  alignment: Alignment.centerLeft,
                  child: NovaStatePill(enLigne: e.enLigne),
                ),
                // Actions révélées au survol ou sur la ligne sélectionnée
                // (maquette `.cact`).
                actions:
                    montrerActions ? _actions() : const SizedBox.shrink(),
              ),
            ),
            // Liseré bleu 2 px de sélection (maquette `tr.sel` : inset shadow).
            if (widget.selectionne)
              Positioned(
                left: 0,
                top: 0,
                bottom: 0,
                child: Container(width: 2, color: t.bleu),
              ),
          ],
        ),
      ),
    );
  }

  Widget _etiquettes(List<String> etiquettes) {
    if (etiquettes.isEmpty) return const SizedBox.shrink();
    // Défilement horizontal désactivé : sert seulement à rogner proprement le
    // dépassement éventuel sur les fenêtres étroites (jamais d'overflow).
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      physics: const NeverScrollableScrollPhysics(),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (var i = 0; i < etiquettes.length; i++) ...[
            if (i > 0) const SizedBox(width: 4),
            NovaTag(etiquettes[i]),
          ],
        ],
      ),
    );
  }

  Widget _actions() {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        NovaBoutonAction(
          icone: NovaIcones.flecheDroite,
          accent: true,
          infobulle: 'Se connecter',
          onTap: widget.onConnecter,
        ),
        NovaBoutonAction(
          icone: NovaIcones.alimentation,
          infobulle: 'Wake-on-LAN',
          onTap: widget.onWakeOnLan,
        ),
        Builder(
          builder: (boutonContext) => NovaBoutonAction(
            icone: NovaIcones.troisPoints,
            onTap: () => _menuDepuisBouton(boutonContext),
          ),
        ),
      ],
    );
  }
}

// ===========================================================================
// Ligne « squelette » de chargement (maquette `skTrs`)
// ===========================================================================

class _LigneSquelette extends StatelessWidget {
  const _LigneSquelette();

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      height: 42,
      padding: const EdgeInsets.symmetric(horizontal: 14),
      decoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      child: _rangeeColonnes(
        etoile: const SizedBox.shrink(),
        alias: const SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          physics: NeverScrollableScrollPhysics(),
          child: Row(
            children: [
              NovaSkeleton(largeur: 26, hauteur: 26),
              SizedBox(width: 10),
              NovaSkeleton(largeur: 120, hauteur: 11),
            ],
          ),
        ),
        adresse: const Align(
          alignment: Alignment.centerLeft,
          child: NovaSkeleton(largeur: 88, hauteur: 10),
        ),
        etiquettes: const Align(
          alignment: Alignment.centerLeft,
          child: NovaSkeleton(largeur: 70, hauteur: 10),
        ),
        derniere: const Align(
          alignment: Alignment.centerLeft,
          child: NovaSkeleton(largeur: 60, hauteur: 10),
        ),
        etat: const Align(
          alignment: Alignment.centerLeft,
          child: NovaSkeleton(largeur: 64, hauteur: 10),
        ),
        actions: const SizedBox.shrink(),
      ),
    );
  }
}
