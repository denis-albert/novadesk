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
/// « Wake-on-LAN » demande l'adresse MAC (le carnet n'en stocke pas) puis émet
/// le paquet magique **réel** via la façade (`send_wol`).
///
/// L'onglet « Découverts » affiche les **pairs LAN réels** : `discovery_peers`
/// est sondé (~2 s) tant que l'onglet est actif — la découverte elle-même est
/// démarrée au niveau application (coquille, `main.dart`). Un **secret
/// optionnel** (hôte en accès non surveillé), déplié par la puce cadenas sous
/// le champ d'adresse, est transmis via [SessionOptionsDto.motDePasse] — ou,
/// si la saisie a la forme d'un code d'invitation « XXX-XXX-XXX », via
/// [SessionOptionsDto.invitation] (routage par `_estCodeInvitation`).
///
/// Le dialogue « Inviter » crée des **invitations éphémères réelles** via la
/// façade : profil accordé + durée de validité → `create_invite` renvoie un
/// code copiable ; la modale liste aussi les invitations actives
/// (`list_invites`, « expire dans … ») et les révoque (`revoke_invite`).
library;

import 'dart:async';

import 'package:flutter/foundation.dart' show listEquals;
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

  /// Secret **optionnel** présenté à un hôte en accès non surveillé : mot de
  /// passe d'admission automatique **ou** code d'invitation « XXX-XXX-XXX »
  /// — le routage se fait à la connexion selon la forme de la saisie (voir
  /// [_seConnecter]) ; champ déplié à la demande par la puce cadenas.
  final TextEditingController _mdpController = TextEditingController();
  final FocusNode _mdpFocus = FocusNode(debugLabel: 'champ-mot-de-passe');
  bool _mdpDeplie = false;
  bool _mdpEnFocus = false;
  bool _mdpRenseigne = false;

  ModeConnexion _mode = ModeConnexion.controle;
  bool _connexionEnCours = false;
  bool _adresseEnFocus = false;
  _OngletAccueil _onglet = _OngletAccueil.recentes;
  bool _idCopie = false;
  Timer? _minuteurCopie;

  /// Squelette de chargement de la liste (~780 ms comme la maquette).
  bool _chargement = true;
  Timer? _minuteurChargement;

  /// Instantané des pairs découverts sur le réseau local (`discovery_peers`),
  /// sondé par [_minuteurDecouverte] tant que l'onglet « Découverts » est actif.
  List<DiscoveredPeerDto> _pairsDecouverts = const [];
  Timer? _minuteurDecouverte;
  bool _sondeDecouverteEnCours = false;

  /// Adresses MAC saisies pour le Wake-on-LAN pendant la session, par ID de
  /// contact : le carnet ne stocke pas de MAC, on pré-remplit donc le dialogue
  /// avec la dernière saisie (mémoire d'écran, best-effort).
  final Map<int, String> _macWolMemorisees = <int, String>{};

  @override
  void initState() {
    super.initState();
    _adresseFocus.addListener(
      () => setState(() => _adresseEnFocus = _adresseFocus.hasFocus),
    );
    _mdpFocus.addListener(
      () => setState(() => _mdpEnFocus = _mdpFocus.hasFocus),
    );
    // La puce cadenas reste « active » tant qu'un mot de passe est saisi (même
    // champ replié) : on ne rebâtit que sur la transition vide ↔ renseigné.
    _mdpController.addListener(() {
      final renseigne = _mdpController.text.isNotEmpty;
      if (renseigne != _mdpRenseigne && mounted) {
        setState(() => _mdpRenseigne = renseigne);
      }
    });
    _minuteurChargement = Timer(const Duration(milliseconds: 780), () {
      if (mounted) setState(() => _chargement = false);
    });
  }

  @override
  void dispose() {
    _minuteurCopie?.cancel();
    _minuteurChargement?.cancel();
    _minuteurDecouverte?.cancel();
    _adresseController.dispose();
    _adresseFocus.dispose();
    _mdpController.dispose();
    _mdpFocus.dispose();
    super.dispose();
  }

  // ---------------------------------------------------------------------------
  // Découverte LAN (onglet « Découverts ») — sonde périodique de l'instantané
  // ---------------------------------------------------------------------------

  /// Bascule d'onglet : (dés)arme la sonde de découverte selon l'onglet visé —
  /// le minuteur ne vit que tant que « Découverts » est actif.
  void _changerOnglet(_OngletAccueil onglet) {
    if (_onglet == onglet) return;
    setState(() => _onglet = onglet);
    if (onglet == _OngletAccueil.decouverts) {
      _demarrerSondeDecouverte();
    } else {
      _arreterSondeDecouverte();
    }
  }

  /// Arme la sonde périodique (~2 s) des pairs découverts, avec un relevé
  /// immédiat pour peupler la liste sans attendre le premier tic. La découverte
  /// elle-même tourne au niveau application (démarrée par la coquille au
  /// lancement) : ici on ne fait que **lire** l'instantané des voisins.
  void _demarrerSondeDecouverte() {
    if (_minuteurDecouverte != null) return;
    unawaited(_sonderPairsDecouverts());
    _minuteurDecouverte = Timer.periodic(
      const Duration(seconds: 2),
      (_) => unawaited(_sonderPairsDecouverts()),
    );
  }

  void _arreterSondeDecouverte() {
    _minuteurDecouverte?.cancel();
    _minuteurDecouverte = null;
  }

  /// Relève l'instantané `discovery_peers` et rafraîchit la liste si elle a
  /// changé. Silencieux en cas d'échec (le prochain tic retentera) : une sonde
  /// de fond ne produit jamais de toast.
  Future<void> _sonderPairsDecouverts() async {
    if (_sondeDecouverteEnCours) return;
    _sondeDecouverteEnCours = true;
    try {
      final pairs = await ref.read(nativeApiProvider).discoveryPeers();
      if (!mounted) return;
      if (!listEquals(pairs, _pairsDecouverts)) {
        setState(() => _pairsDecouverts = pairs);
      }
    } catch (_) {
      // Best-effort : liste inchangée, nouvelle tentative au prochain tic.
    } finally {
      _sondeDecouverteEnCours = false;
    }
  }

  // ---------------------------------------------------------------------------
  // Connexion (câblage moteur préservé)
  // ---------------------------------------------------------------------------

  /// Valide la saisie via la façade puis ouvre la fenêtre de session en mise en
  /// relation **par ID** ([SessionEndpointByRendezvous]). [modeForce] impose un
  /// mode (menu contextuel « Observer » / « Transfert de fichiers ») ; sinon le
  /// mode choisi sous le champ d'adresse s'applique.
  Future<void> _seConnecter(
      [String? saisieExplicite, ModeConnexion? modeForce]) async {
    final api = ref.read(nativeApiProvider);
    final idLocal = ref.read(idLocalProvider);
    final saisie = (saisieExplicite ?? _adresseController.text).trim();
    final carnet = ref.read(carnetProvider).valueOrNull ?? const <EntreeCarnet>[];
    final mode = modeForce ?? _mode;

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
        permissions: mode.permissions,
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
      // Secret optionnel (hôte en accès non surveillé) : le champ unique
      // accepte **mot de passe ou code d'invitation**. Une saisie à la forme
      // d'un code « XXX-XXX-XXX » ([_estCodeInvitation]) part dans
      // [SessionOptionsDto.invitation] — normalisée en majuscules, l'hôte
      // comparant le code exact tel que généré — sinon elle reste un mot de
      // passe transmis tel quel. Vide → `null` pour les deux (comportement
      // inchangé : l'hôte garde son dialogue d'approbation manuel).
      final secret = _mdpController.text;
      final estInvitation = _estCodeInvitation(secret.trim());
      final options = SessionOptionsDto(
        permissions: mode.permissions,
        motDePasse: secret.isEmpty || estInvitation ? null : secret,
        invitation: estInvitation ? secret.trim().toUpperCase() : null,
      );
      // Journalise la session au démarrage (historique + dernière connexion du
      // contact) puis rafraîchit les vues concernées.
      await api.recordSession(id: idPair, alias: alias ?? idFormate);
      ref.invalidate(recentSessionsProvider);
      ref.invalidate(carnetProvider);
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

  Future<void> _connecterEntree(EntreeCarnet entree,
      {ModeConnexion? mode}) async {
    final idFormate =
        await ref.read(nativeApiProvider).formatNovaId(id: entree.id);
    _adresseController.text = idFormate;
    await _seConnecter(idFormate, mode);
  }

  // ---------------------------------------------------------------------------
  // Actions du carnet (état local)
  // ---------------------------------------------------------------------------

  Future<void> _basculerFavori(EntreeCarnet entree) async {
    try {
      await ref
          .read(carnetProvider.notifier)
          .basculerFavori(entree.id, !entree.favori);
      if (!mounted) return;
      NovaToast.montrer(
        context,
        entree.favori
            ? '${entree.alias} retiré des favoris'
            : '${entree.alias} ajouté aux favoris',
      );
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  Future<void> _supprimer(EntreeCarnet entree) async {
    try {
      await ref.read(carnetProvider.notifier).supprimer(entree.id);
      if (!mounted) return;
      NovaToast.montrer(context, '${entree.alias} supprimé du carnet');
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
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
        unawaited(_connecterEntree(entree));
      case 'obs':
        // Session réelle en observation seule, quel que soit le mode choisi.
        unawaited(
            _connecterEntree(entree, mode: ModeConnexion.observation));
      case 'ft':
        // Session réelle limitée au transfert de fichiers.
        unawaited(_connecterEntree(entree, mode: ModeConnexion.fichiers));
      case 'fav':
        unawaited(_basculerFavori(entree));
      case 'ren':
        unawaited(_renommer(entree));
      case 'wol':
        unawaited(_wakeOnLan(entree));
      case 'del':
        unawaited(_supprimer(entree));
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
    try {
      await ref.read(carnetProvider.notifier).modifier(
            id: entree.id,
            alias: alias,
            groupe: entree.groupe,
            etiquettes: entree.etiquettes,
          );
      if (!mounted) return;
      NovaToast.montrer(context, '${entree.alias} renommé en « $alias »');
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  /// Réveille [entree] par **Wake-on-LAN** : demande l'adresse MAC via un
  /// dialogue (le carnet n'en stocke pas) puis émet le paquet magique via la
  /// façade (`send_wol`). Diffusion vide → globale (`255.255.255.255:9`).
  Future<void> _wakeOnLan(EntreeCarnet entree) async {
    final parametres = await _demanderParametresWol(entree);
    if (parametres == null || !mounted) return;
    // Mémorise la MAC (normalisée) pour pré-remplir le prochain réveil.
    _macWolMemorisees[entree.id] = parametres.mac;
    try {
      await ref
          .read(nativeApiProvider)
          .sendWol(parametres.mac, broadcast: parametres.broadcast);
      if (!mounted) return;
      NovaToast.montrer(context, 'Paquet de réveil envoyé à ${entree.alias}');
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    }
  }

  /// Dialogue « Réveiller {alias} » : champ **Adresse MAC** obligatoire
  /// (formats `AA:BB:CC:DD:EE:FF`, `AA-BB-…` ou `AABB…` tolérés, pré-rempli si
  /// déjà saisie pendant la session) et champ **Broadcast** facultatif
  /// (« ip:port », vide → diffusion globale). Renvoie la MAC normalisée et la
  /// diffusion (`null` si vide), ou `null` si l'utilisateur annule.
  Future<({String mac, String? broadcast})?> _demanderParametresWol(
      EntreeCarnet entree) async {
    final macController =
        TextEditingController(text: _macWolMemorisees[entree.id] ?? '');
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
            title: Text('Réveiller ${entree.alias}'),
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
            _puceMotDePasse(t),
          ],
        ),
        if (_mdpDeplie) ...[
          const SizedBox(height: 10),
          _champMotDePasse(t),
        ],
      ],
    );
  }

  /// Puce « Mot de passe / code » (cadenas) : déplie/replie le champ de secret
  /// d'accès non surveillé — mot de passe **ou** code d'invitation — sous le
  /// champ d'adresse. Reste « active » (bleue) tant qu'une saisie est présente
  /// — la saisie est transmise même champ replié.
  Widget _puceMotDePasse(NovaTokens t) {
    final actif = _mdpDeplie || _mdpRenseigne;
    return NovaActivable(
      onTap: () {
        setState(() => _mdpDeplie = !_mdpDeplie);
        // Saisie immédiate au dépliage (la demande de focus est honorée quand
        // le champ s'attache à l'arbre, au frame suivant).
        if (_mdpDeplie) _mdpFocus.requestFocus();
      },
      label: "Mot de passe ou code d'invitation (hôte en accès non surveillé)",
      builder: (context, survole, focus) => Container(
        padding: const EdgeInsets.symmetric(horizontal: 11, vertical: 5),
        decoration: BoxDecoration(
          color: actif ? t.selection : Colors.transparent,
          border: Border.all(color: actif ? t.bleu : t.champBordure),
          borderRadius: BorderRadius.circular(kNovaRayon),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIcone(NovaIcones.cadenas,
                taille: 13, couleur: actif ? t.bleu : t.texte2),
            const SizedBox(width: 6),
            Text(
              'Mot de passe / code',
              style:
                  TextStyle(fontSize: 11.5, color: actif ? t.bleu : t.texte2),
            ),
          ],
        ),
      ),
    );
  }

  /// Champ **secret optionnel** (accès non surveillé), déplié sous le champ
  /// d'adresse par la puce cadenas. À la connexion ([_seConnecter]), une
  /// saisie à la forme d'un code d'invitation « XXX-XXX-XXX » part dans
  /// [SessionOptionsDto.invitation], toute autre saisie non vide dans
  /// [SessionOptionsDto.motDePasse] ; vide → `null` (l'hôte se replie sur son
  /// dialogue d'approbation manuel).
  Widget _champMotDePasse(NovaTokens t) {
    return AnimatedContainer(
      duration: const Duration(milliseconds: 120),
      height: 34,
      padding: const EdgeInsets.symmetric(horizontal: 11),
      decoration: BoxDecoration(
        color: t.fenetre,
        borderRadius: BorderRadius.circular(kNovaRayon),
        border: Border.all(color: _mdpEnFocus ? kNovaRouge : t.champBordure),
        boxShadow: _mdpEnFocus
            ? [
                BoxShadow(
                  color: t.bleu.withValues(alpha: 0.13),
                  spreadRadius: 3,
                ),
              ]
            : null,
      ),
      child: Row(
        children: [
          NovaIcone(NovaIcones.cadenas, taille: 14, couleur: t.texte3),
          const SizedBox(width: 9),
          Expanded(
            child: TextField(
              controller: _mdpController,
              focusNode: _mdpFocus,
              obscureText: true,
              autocorrect: false,
              enableSuggestions: false,
              style: TextStyle(fontSize: 13.5, color: t.texte),
              decoration: InputDecoration(
                isCollapsed: true,
                filled: false,
                border: InputBorder.none,
                enabledBorder: InputBorder.none,
                focusedBorder: InputBorder.none,
                hintText: "Optionnel — mot de passe ou code d'invitation",
                hintStyle: TextStyle(fontSize: 12.5, color: t.texte3),
              ),
              onSubmitted: (_) => unawaited(_seConnecter()),
            ),
          ),
        ],
      ),
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
        // Halo doux au focus (maquette `.inp:focus-within` :
        // `box-shadow:0 0 0 3px rgba(47,111,224,.13)`).
        boxShadow: _adresseEnFocus
            ? [
                BoxShadow(
                  color: t.bleu.withValues(alpha: 0.13),
                  spreadRadius: 3,
                ),
              ]
            : null,
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
    return NovaActivable(
      onTap: () => setState(() => _mode = mode),
      label: mode.libelle,
      builder: (context, survole, focus) => Container(
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
    );
  }

  Widget _colonneCePoste(NovaTokens t) {
    final idFormate = ref.watch(idLocalFormateProvider);
    final identite = ref.watch(localIdentityProvider);
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
          loading: () => const NovaSkeleton(largeur: 160, hauteur: 26),
          error: (e, _) => const Text('—'),
        ),
        const SizedBox(height: 5),
        // Empreinte d'identité (vérification TOFU) — issue de local_identity.
        Row(
          children: [
            NovaIcone(NovaIcones.bouclierCoche, taille: 13, couleur: t.texte2),
            const SizedBox(width: 6),
            identite.when(
              data: (i) => Text(
                'Empreinte : ${_empreinteCourte(i.empreinte)}',
                style: TextStyle(
                  fontSize: 12,
                  color: t.texte2,
                  fontFeatures: const [FontFeature.tabularFigures()],
                ),
              ),
              loading: () => const NovaSkeleton(largeur: 150, hauteur: 11),
              error: (e, _) => Text('Empreinte indisponible',
                  style: TextStyle(fontSize: 12, color: t.texte3)),
            ),
          ],
        ),
        const SizedBox(height: 8),
        _motDePasseEphemere(t),
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

  /// Empreinte compacte : 6 premières paires hexadécimales (« 3F·A9·7C·… »).
  static String _empreinteCourte(String empreinte) {
    final hex = empreinte.toUpperCase();
    final n = hex.length < 12 ? hex.length : 12;
    final paires = <String>[];
    for (var i = 0; i + 2 <= n; i += 2) {
      paires.add(hex.substring(i, i + 2));
    }
    return paires.join('·');
  }

  /// Mot de passe éphémère (session ponctuelle) issu de
  /// `generate_ephemeral_password`, avec copie et régénération.
  Widget _motDePasseEphemere(NovaTokens t) {
    final motDePasse = ref.watch(motDePasseEphemereProvider);
    return Row(
      children: [
        NovaIcone(NovaIcones.cadenas, taille: 13, couleur: t.texte2),
        const SizedBox(width: 6),
        Text('Mot de passe : ',
            style: TextStyle(fontSize: 12, color: t.texte2)),
        Flexible(
          child: motDePasse.when(
            data: (mdp) => SelectableText(
              mdp,
              maxLines: 1,
              style: TextStyle(
                fontSize: 12.5,
                color: t.texte,
                letterSpacing: 0.5,
                fontFamily: 'Cascadia Code',
                fontFamilyFallback: const ['Consolas', 'monospace'],
              ),
            ),
            loading: () => const NovaSkeleton(largeur: 84, hauteur: 12),
            error: (e, _) => Text('—',
                style: TextStyle(fontSize: 12, color: t.texte3)),
          ),
        ),
        const SizedBox(width: 6),
        NovaBoutonAction(
          icone: NovaIcones.copier,
          tailleIcone: 13,
          taille: 22,
          infobulle: 'Copier le mot de passe',
          onTap: motDePasse.hasValue
              ? () async {
                  await Clipboard.setData(
                      ClipboardData(text: motDePasse.requireValue));
                  if (mounted) {
                    NovaToast.montrer(context, 'Mot de passe copié');
                  }
                }
              : null,
        ),
        NovaBoutonAction(
          icone: NovaIcones.recharger,
          tailleIcone: 13,
          taille: 22,
          infobulle: 'Régénérer',
          onTap: () => unawaited(
              ref.read(motDePasseEphemereProvider.notifier).regenerer()),
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
    final favoris = (ref.watch(carnetProvider).valueOrNull ??
            const <EntreeCarnet>[])
        .where((e) => e.favori)
        .length;
    return Container(
      decoration: BoxDecoration(
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      padding: const EdgeInsets.fromLTRB(20, 9, 20, 0),
      child: Row(
        children: [
          _onglets(t, _OngletAccueil.recentes, 'Sessions récentes', null),
          _onglets(t, _OngletAccueil.favoris, 'Favoris', favoris),
          _onglets(
              t, _OngletAccueil.decouverts, 'Découverts', _pairsDecouverts.length),
        ],
      ),
    );
  }

  Widget _onglets(NovaTokens t, _OngletAccueil onglet, String libelle, int? n) {
    final actif = _onglet == onglet;
    return NovaActivable(
      onTap: () => _changerOnglet(onglet),
      rayonFocus: 3,
      label: libelle,
      builder: (context, survole, focus) => Container(
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
                color: actif || survole ? t.texte : t.texte2,
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
    );
  }

  // --- Liste d'appareils ----------------------------------------------------

  Widget _liste() {
    final carnet =
        ref.watch(carnetProvider).valueOrNull ?? const <EntreeCarnet>[];
    final List<EntreeCarnet> entrees;
    if (_onglet == _OngletAccueil.decouverts) {
      // Liste **vivante** des pairs LAN (instantané `discovery_peers`, sondé
      // par minuteur tant que l'onglet est actif) — pas de squelette : une
      // liste vide est un résultat honnête, pas un chargement.
      if (_pairsDecouverts.isEmpty) {
        return const NovaEmptyState(
          icone: NovaIcones.radar,
          titre: 'Aucun appareil découvert',
          sousTitre:
              'Aucun appareil NovaDesk détecté sur le réseau local.',
        );
      }
      entrees = [
        for (final pair in _pairsDecouverts)
          _entreeDepuisPairDecouvert(pair, carnet),
      ];
    } else if (_chargement) {
      return ListView(
        children: [for (var i = 0; i < 4; i++) const _LigneSquelette()],
      );
    } else if (_onglet == _OngletAccueil.favoris) {
      entrees = carnet.where((e) => e.favori).toList();
      if (entrees.isEmpty) {
        return const NovaEmptyState(
          icone: NovaIcones.etoile,
          titre: 'Aucun favori',
          sousTitre:
              'Ajoutez des favoris via le menu contextuel d’un appareil.',
        );
      }
    } else {
      // Sessions récentes (historique persistant), enrichies depuis le carnet.
      final recentes = ref.watch(recentSessionsProvider).valueOrNull ??
          const <RecentSessionDto>[];
      if (recentes.isEmpty) {
        return const NovaEmptyState(
          icone: NovaIcones.horloge,
          titre: 'Aucune session récente',
          sousTitre: 'Vos connexions récentes apparaîtront ici.',
        );
      }
      entrees = [for (final s in recentes) _entreeDepuisRecente(s, carnet)];
    }
    return ListView.builder(
      itemCount: entrees.length,
      itemBuilder: (context, i) => _LigneAppareil(
        entree: entrees[i],
        onConnecter: () => unawaited(_connecterEntree(entrees[i])),
        onFavori: () => unawaited(_basculerFavori(entrees[i])),
        onMenu: (pos) => unawaited(_menuContextuel(entrees[i], pos)),
      ),
    );
  }

  /// Convertit un pair découvert sur le LAN en entrée d'affichage : nom
  /// annoncé, adresse « ip:port » en libellé de droite et pastille **verte**
  /// (il vient d'annoncer sa présence, il est en ligne). L'étoile reflète le
  /// carnet si le pair y figure.
  ///
  /// ⚠️ Les annonces ne sont ni signées ni chiffrées : nom et ID sont purement
  /// indicatifs — l'authentification passe par la poignée de main de session.
  EntreeCarnet _entreeDepuisPairDecouvert(
      DiscoveredPeerDto pair, List<EntreeCarnet> carnet) {
    final corr = carnet.where((e) => e.id == pair.id).firstOrNull;
    return EntreeCarnet(
      id: pair.id,
      alias: pair.nom,
      derniereConnexion: pair.adresse,
      favori: corr?.favori ?? false,
      enLigne: true,
      groupe: 'Découverts',
    );
  }

  /// Convertit une session récente en entrée d'affichage (enrichie depuis le
  /// carnet si le pair y figure).
  EntreeCarnet _entreeDepuisRecente(
      RecentSessionDto s, List<EntreeCarnet> carnet) {
    final corr = carnet.where((e) => e.id == s.id).firstOrNull;
    if (corr != null) return corr;
    return EntreeCarnet(
      id: s.id,
      alias: s.alias,
      derniereConnexion: formaterHorodatageRelatif(s.timestamp),
      groupe: 'Récent',
    );
  }

  // --- Modale d'invitation --------------------------------------------------

  /// Ouvre la modale « Inviter à se connecter » : génération d'invitations
  /// éphémères **réelles** (profil accordé + durée → code), liste des
  /// invitations actives et révocation unitaire.
  void _montrerInvitation() {
    montrerDialogueNova<void>(
        context: context, builder: (context) => const _InviteDialog());
  }
}

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

/// Vrai si [saisie] a la forme d'un **code d'invitation** « XXX-XXX-XXX » :
/// trois groupes de trois symboles de l'alphabet des codes (lettres sans `I`
/// ni `O`, chiffres 2–9 — jamais `0` ni `1`), casse tolérée à la saisie. Sert
/// à router le champ optionnel « mot de passe / code » vers
/// [SessionOptionsDto.invitation] plutôt que [SessionOptionsDto.motDePasse] :
/// l'alphabet restreint rend improbable la collision avec un vrai mot de
/// passe de cette forme exacte.
bool _estCodeInvitation(String saisie) => RegExp(
      r'^[A-HJ-NP-Z2-9]{3}-[A-HJ-NP-Z2-9]{3}-[A-HJ-NP-Z2-9]{3}$',
      caseSensitive: false,
    ).hasMatch(saisie);

// ===========================================================================
// Composants privés
// ===========================================================================

/// Lien bleu avec icône (maquette `.lnk`), souligné au survol.
class _LienBleu extends StatelessWidget {
  const _LienBleu(
      {required this.icone, required this.libelle, required this.onTap});

  final IconData icone;
  final String libelle;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return NovaActivable(
      onTap: onTap,
      rayonFocus: 3,
      label: libelle,
      builder: (context, survole, focus) => Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          NovaIcone(icone, taille: 13, couleur: t.bleu),
          const SizedBox(width: 6),
          Text(
            libelle,
            style: TextStyle(
              fontSize: 12,
              color: t.bleu,
              decoration:
                  survole ? TextDecoration.underline : TextDecoration.none,
              decorationColor: t.bleu,
            ),
          ),
        ],
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
    // Invisible : exclue du parcours clavier ET des interactions souris.
    return ExcludeFocus(
      excluding: !visible,
      child: Opacity(
        opacity: visible ? (e.favori ? 1 : 0.6) : 0,
        child: IgnorePointer(
          ignoring: !visible,
          child: NovaBoutonAction(
            icone: NovaIcones.etoile,
            tailleIcone: 15,
            taille: 24,
            infobulle: e.favori ? 'Retirer des favoris' : 'Ajouter aux favoris',
            couleurActive: e.favori ? t.ambre : null,
            onTap: widget.onFavori,
          ),
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

// ===========================================================================
// Invitations éphémères (modale « Inviter »)
// ===========================================================================

/// Profil de permissions accordé à l'admission par une **invitation
/// éphémère** — les trois valeurs acceptées par [NativeApi.createInvite],
/// avec libellé et icône d'affichage.
enum _ProfilInvitation {
  observation('observation', 'Observation', NovaIcones.observation),
  standard('standard', 'Standard', NovaIcones.controle),
  controleTotal('controle_total', 'Contrôle total', NovaIcones.eclair);

  const _ProfilInvitation(this.valeurApi, this.libelle, this.icone);

  /// Valeur attendue par la façade ([NativeApi.createInvite],
  /// [InviteDto.profil]).
  final String valeurApi;

  /// Libellé d'affichage.
  final String libelle;

  /// Icône de la puce de choix.
  final IconData icone;

  /// Libellé du profil [valeurApi] tel que rendu par la façade ; la valeur
  /// brute est restituée telle quelle si elle est inconnue (robustesse
  /// d'affichage : la liste ne doit jamais casser).
  static String libellePour(String valeurApi) =>
      values.where((p) => p.valeurApi == valeurApi).firstOrNull?.libelle ??
      valeurApi;
}

/// Durées de validité proposées à la création d'une invitation.
const List<({int minutes, String libelle})> _dureesInvitation = [
  (minutes: 10, libelle: '10 min'),
  (minutes: 60, libelle: '1 h'),
  (minutes: 1440, libelle: '24 h'),
];

/// Temps restant lisible d'une invitation active (« expire dans 9 min ») —
/// arrondi vers le bas : pour un compte à rebours, sous-estimer est honnête.
String _formaterExpiration(int secondes) {
  final s = secondes < 0 ? 0 : secondes;
  if (s < 60) return 'expire dans $s s';
  final minutes = s ~/ 60;
  if (minutes < 60) return 'expire dans $minutes min';
  return 'expire dans ${minutes ~/ 60} h';
}

/// Modale « Inviter à se connecter » (maquette `.modal.invite`), branchée sur
/// les **invitations éphémères réelles** de la façade : choix du profil
/// accordé et de la durée de validité → [NativeApi.createInvite] renvoie un
/// code « XXX-XXX-XXX » copiable à dicter au technicien. En dessous, la liste
/// des invitations **actives** ([NativeApi.listInvites], relevée à l'ouverture
/// puis toutes les ~30 s pour un « expire dans … » honnête) avec révocation
/// unitaire ([NativeApi.revokeInvite]).
class _InviteDialog extends ConsumerStatefulWidget {
  const _InviteDialog();

  @override
  ConsumerState<_InviteDialog> createState() => _InviteDialogState();
}

class _InviteDialogState extends ConsumerState<_InviteDialog> {
  /// Profil accordé à la prochaine invitation (moindre privilège par défaut).
  _ProfilInvitation _profil = _ProfilInvitation.observation;

  /// Durée de validité de la prochaine invitation, en minutes.
  int _ttlMinutes = 10;

  bool _creationEnCours = false;

  /// Dernier code généré par la modale, affiché dans l'encart copiable.
  String? _codeGenere;

  /// Invitations actives — `null` tant que le premier relevé n'est pas rendu
  /// (squelettes de chargement).
  List<InviteDto>? _invitations;

  /// Code dont la révocation est en cours (désactive les corbeilles le temps
  /// de l'aller-retour).
  String? _revocationEnCours;

  /// Rafraîchissement périodique de la liste : les « expire dans … » restent
  /// honnêtes et les invitations expirées disparaissent d'elles-mêmes.
  Timer? _minuteurRafraichissement;

  @override
  void initState() {
    super.initState();
    unawaited(_rafraichirInvitations(premierReleve: true));
    _minuteurRafraichissement = Timer.periodic(
      const Duration(seconds: 30),
      (_) => unawaited(_rafraichirInvitations()),
    );
  }

  @override
  void dispose() {
    _minuteurRafraichissement?.cancel();
    super.dispose();
  }

  /// Relève la liste des invitations actives. Un tic périodique échoue en
  /// silence (liste précédente conservée, prochain tic retentera) ; le
  /// **premier** relevé signale l'erreur par un toast et se replie sur une
  /// liste vide pour ne pas laisser des squelettes permanents.
  Future<void> _rafraichirInvitations({bool premierReleve = false}) async {
    try {
      final invitations = await ref.read(nativeApiProvider).listInvites();
      if (mounted) setState(() => _invitations = invitations);
    } on NovaApiException catch (e) {
      if (premierReleve && mounted) {
        setState(() => _invitations = const []);
        NovaToast.montrer(context, e.message, info: true);
      }
    }
  }

  /// Crée l'invitation (profil + durée choisis) via la façade, affiche le
  /// code renvoyé dans l'encart copiable et rafraîchit la liste des actives.
  Future<void> _genererCode() async {
    setState(() => _creationEnCours = true);
    try {
      final code = await ref.read(nativeApiProvider).createInvite(
            profil: _profil.valeurApi,
            ttlMinutes: _ttlMinutes,
          );
      if (!mounted) return;
      setState(() => _codeGenere = code);
      NovaToast.montrer(context, 'Invitation créée');
      await _rafraichirInvitations();
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    } finally {
      if (mounted) setState(() => _creationEnCours = false);
    }
  }

  /// Révoque [code] puis rafraîchit la liste — y compris après une erreur, le
  /// code ayant pu expirer ou être consommé entre l'affichage et le clic.
  Future<void> _revoquer(String code) async {
    setState(() => _revocationEnCours = code);
    try {
      await ref.read(nativeApiProvider).revokeInvite(code: code);
      if (!mounted) return;
      // Un code révoqué n'est plus présentable : retire l'encart s'il l'affiche.
      setState(() {
        if (_codeGenere == code) _codeGenere = null;
      });
      NovaToast.montrer(context, 'Invitation $code révoquée');
    } on NovaApiException catch (e) {
      if (mounted) NovaToast.montrer(context, e.message, info: true);
    } finally {
      if (mounted) {
        setState(() => _revocationEnCours = null);
        await _rafraichirInvitations();
      }
    }
  }

  /// Copie [code] dans le presse-papiers système.
  Future<void> _copierCode(String code) async {
    await Clipboard.setData(ClipboardData(text: code));
    if (mounted) NovaToast.montrer(context, "Code d'invitation copié");
  }

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
            _entete(t),
            Divider(height: 1, color: t.filet),
            // Corps déroulant : la modale reste utilisable sur une fenêtre
            // basse ou avec une longue liste d'invitations, sans déborder.
            Flexible(
              child: SingleChildScrollView(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Padding(
                      padding: const EdgeInsets.fromLTRB(20, 16, 20, 16),
                      child: _sectionCreation(t),
                    ),
                    Divider(height: 1, color: t.filet),
                    Padding(
                      padding: const EdgeInsets.fromLTRB(20, 14, 20, 16),
                      child: _sectionActives(t),
                    ),
                  ],
                ),
              ),
            ),
            Divider(height: 1, color: t.filet),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 14, 20, 14),
              // Pied de modale : bouton pleine largeur (maquette `.foot .btn`).
              child: Row(
                children: [
                  Expanded(
                    child: NovaBoutonSecondaire(
                      libelle: 'Fermer',
                      hauteur: 38,
                      onPressed: () => Navigator.of(context).pop(),
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// En-tête de la modale (pastille rouge + titre), fidèle à la maquette.
  Widget _entete(NovaTokens t) {
    return Padding(
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
              Text('Code éphémère à usage unique',
                  style: TextStyle(fontSize: 12, color: t.texte3)),
            ],
          ),
        ],
      ),
    );
  }

  /// Générateur d'invitation : puces **profil accordé** et **durée de
  /// validité**, résumé dynamique (reprend la formulation de la maquette),
  /// bouton « Générer le code » puis encart du code renvoyé (sélectionnable,
  /// bouton Copier).
  Widget _sectionCreation(NovaTokens t) {
    final libelleDuree =
        _dureesInvitation.firstWhere((d) => d.minutes == _ttlMinutes).libelle;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const NovaSectionLabel('Profil accordé'),
        const SizedBox(height: 6),
        Wrap(
          spacing: 6,
          runSpacing: 6,
          children: [
            for (final profil in _ProfilInvitation.values)
              _PuceChoix(
                libelle: profil.libelle,
                icone: profil.icone,
                actif: _profil == profil,
                onTap: () => setState(() => _profil = profil),
              ),
          ],
        ),
        const SizedBox(height: 12),
        const NovaSectionLabel('Durée de validité'),
        const SizedBox(height: 6),
        Wrap(
          spacing: 6,
          runSpacing: 6,
          children: [
            for (final duree in _dureesInvitation)
              _PuceChoix(
                libelle: duree.libelle,
                actif: _ttlMinutes == duree.minutes,
                onTap: () => setState(() => _ttlMinutes = duree.minutes),
              ),
          ],
        ),
        const SizedBox(height: 12),
        Text(
          'Expire dans $libelleDuree · une seule connexion · profil '
          '« ${_profil.libelle} ».',
          style: TextStyle(fontSize: 11.5, color: t.texte3),
        ),
        const SizedBox(height: 12),
        Row(
          children: [
            Expanded(
              child: NovaBoutonPrimaire(
                libelle: 'Générer le code',
                icone: NovaIcones.inviter,
                hauteur: 34,
                enCours: _creationEnCours,
                onPressed:
                    _creationEnCours ? null : () => unawaited(_genererCode()),
              ),
            ),
          ],
        ),
        if (_codeGenere != null) ...[
          const SizedBox(height: 12),
          const NovaSectionLabel("Code d'invitation"),
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
                  child: SelectableText(
                    _codeGenere!,
                    maxLines: 1,
                    style: TextStyle(
                      fontSize: 13,
                      color: t.texte,
                      letterSpacing: 1,
                      fontFamily: 'Cascadia Code',
                      fontFamilyFallback: const ['Consolas', 'monospace'],
                    ),
                  ),
                ),
              ),
              const SizedBox(width: 8),
              NovaBoutonPrimaire(
                libelle: 'Copier',
                onPressed: () => unawaited(_copierCode(_codeGenere!)),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Text(
            'Code à dicter au technicien — il le saisit dans le champ '
            '« Mot de passe / code » avant de se connecter.',
            style: TextStyle(fontSize: 11.5, color: t.texte3),
          ),
        ],
      ],
    );
  }

  /// Liste des invitations **actives** (non expirées, non consommées) : code
  /// en chasse fixe, profil + « expire dans … », corbeille de révocation.
  /// Squelettes tant que le premier relevé n'est pas rendu.
  Widget _sectionActives(NovaTokens t) {
    final invitations = _invitations;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const NovaSectionLabel('Invitations actives'),
        const SizedBox(height: 8),
        if (invitations == null) ...[
          const Padding(
            padding: EdgeInsets.only(bottom: 8),
            child: NovaSkeleton(largeur: 210, hauteur: 12),
          ),
          const NovaSkeleton(largeur: 150, hauteur: 12),
        ] else if (invitations.isEmpty)
          Text('Aucune invitation active.',
              style: TextStyle(fontSize: 12, color: t.texte3))
        else
          for (var i = 0; i < invitations.length; i++)
            _ligneInvitation(t, invitations[i],
                derniere: i == invitations.length - 1),
      ],
    );
  }

  /// Une ligne d'invitation active — filet sous chaque ligne sauf la dernière.
  Widget _ligneInvitation(NovaTokens t, InviteDto invitation,
      {required bool derniere}) {
    return Container(
      padding: const EdgeInsets.symmetric(vertical: 6),
      decoration: derniere
          ? null
          : BoxDecoration(
              border: Border(bottom: BorderSide(color: t.filet)),
            ),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  invitation.code,
                  style: TextStyle(
                    fontSize: 12.5,
                    color: t.texte,
                    letterSpacing: 0.5,
                    fontFamily: 'Cascadia Code',
                    fontFamilyFallback: const ['Consolas', 'monospace'],
                  ),
                ),
                const SizedBox(height: 2),
                Text(
                  '${_ProfilInvitation.libellePour(invitation.profil)} · '
                  '${_formaterExpiration(invitation.expireDansS)}',
                  style: TextStyle(fontSize: 11.5, color: t.texte3),
                ),
              ],
            ),
          ),
          const SizedBox(width: 8),
          NovaBoutonAction(
            icone: NovaIcones.corbeille,
            tailleIcone: 14,
            taille: 24,
            infobulle: 'Révoquer',
            onTap: _revocationEnCours == null
                ? () => unawaited(_revoquer(invitation.code))
                : null,
          ),
        ],
      ),
    );
  }
}

/// Puce de choix de la modale d'invitation (profil / durée) — même visuel que
/// les puces de mode de l'accueil (maquette `.chip`) : fond sélection et
/// bordure bleue quand active.
class _PuceChoix extends StatelessWidget {
  const _PuceChoix({
    required this.libelle,
    required this.actif,
    required this.onTap,
    this.icone,
  });

  final String libelle;
  final bool actif;
  final VoidCallback onTap;
  final IconData? icone;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return NovaActivable(
      onTap: onTap,
      label: libelle,
      builder: (context, survole, focus) => Container(
        padding: const EdgeInsets.symmetric(horizontal: 11, vertical: 5),
        decoration: BoxDecoration(
          color: actif ? t.selection : Colors.transparent,
          border: Border.all(color: actif ? t.bleu : t.champBordure),
          borderRadius: BorderRadius.circular(kNovaRayon),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (icone != null) ...[
              NovaIcone(icone!, taille: 13, couleur: actif ? t.bleu : t.texte2),
              const SizedBox(width: 6),
            ],
            Text(
              libelle,
              style:
                  TextStyle(fontSize: 11.5, color: actif ? t.bleu : t.texte2),
            ),
          ],
        ),
      ),
    );
  }
}
