/// Écran « Carnet d'adresses » — fidèle à la maquette `novadesk-app.html`
/// (vue `#v-carnet`). À gauche : un rail de groupes 184 px (Tous / Favoris puis
/// les groupes distincts du carnet, plus « Nouveau groupe » et Importer /
/// Exporter). À droite : une barre de recherche + « Ajouter » surmontant un
/// tableau dense (étoile · alias+OS · adresse · étiquettes · dernière connexion
/// · état · actions révélées au survol).
///
/// Écran de présentation : aucune session réelle n'est ouverte ici. Le carnet
/// est un [carnetProvider] fictif ; les actions se contentent d'un toast
/// NovaDesk, hormis « favori » (bascule) et « Supprimer » (retrait), persistés
/// dans le provider. Chargement initial simulé par des squelettes shimmer
/// (~780 ms), comme `skTrs` dans la maquette.
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/app_frame.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';

/// Clés de groupe réservées (les autres clés sont des noms de groupe libres).
const String _cleTous = 'Tous';
const String _cleFavoris = 'Favoris';

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
  List<_Groupe> _calculerGroupes(List<EntreeCarnet> entrees, NovaTokens t) {
    final favoris = entrees.where((e) => e.favori).length;
    final noms = <String>[];
    final comptes = <String, int>{};
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
  // Actions (toasts + mutations du provider fictif)
  // ---------------------------------------------------------------------------

  void _seConnecter(EntreeCarnet e) {
    // Écran de présentation : aucune session réelle n'est ouverte ici.
    NovaToast.montrer(context, 'Connexion à ${e.alias}…');
  }

  void _observer(EntreeCarnet e) {
    NovaToast.montrer(context, 'Observation de ${e.alias}…');
  }

  void _transfertFichiers(EntreeCarnet e) {
    NovaToast.montrer(context, 'Transfert de fichiers — ${e.alias}', info: true);
  }

  void _renommer(EntreeCarnet e) {
    NovaToast.montrer(context, 'Renommer « ${e.alias} » — à venir.', info: true);
  }

  void _wakeOnLan(EntreeCarnet e) {
    NovaToast.montrer(context, 'Paquet Wake-on-LAN envoyé à ${e.alias}',
        info: true);
  }

  void _basculerFavori(EntreeCarnet e) {
    final carnet = ref.read(carnetProvider.notifier);
    final devientFavori = !e.favori;
    carnet.state = [
      for (final x in carnet.state)
        x.id == e.id ? x.copyWith(favori: devientFavori) : x,
    ];
    NovaToast.montrer(
      context,
      devientFavori
          ? '${e.alias} ajouté aux favoris'
          : '${e.alias} retiré des favoris',
    );
  }

  void _supprimer(EntreeCarnet e) {
    final carnet = ref.read(carnetProvider.notifier);
    carnet.state = carnet.state.where((x) => x.id != e.id).toList();
    if (_idSelectionne == e.id) {
      setState(() => _idSelectionne = null);
    }
    NovaToast.montrer(context, '${e.alias} supprimé du carnet');
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
        _basculerFavori(e);
        break;
      case 'ren':
        _renommer(e);
        break;
      case 'wol':
        _wakeOnLan(e);
        break;
      case 'del':
        _supprimer(e);
        break;
    }
  }

  // ---------------------------------------------------------------------------
  // Construction
  // ---------------------------------------------------------------------------

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final entrees = ref.watch(carnetProvider);
    final groupes = _calculerGroupes(entrees, t);
    final filtrees = _filtrer(entrees);
    // Pré-résolution des adresses ici : `ref.watch` doit rester dans `build`
    // (jamais dans un `itemBuilder` paresseux de ListView).
    final lignes = [for (final e in filtrees) (e, _adresse(e))];

    return Scaffold(
      body: NovaAppFrame(
        vue: NovaVue.carnet,
        corps: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _railGroupes(t, groupes),
            Expanded(child: _panneauPrincipal(t, lignes, filtrees.isEmpty)),
          ],
        ),
      ),
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
                    onTap: () => NovaToast.montrer(
                      context,
                      'Nouveau groupe — à venir.',
                      info: true,
                    ),
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
                    onPressed: () => NovaToast.montrer(
                      context,
                      'Importation du carnet — à venir.',
                      info: true,
                    ),
                  ),
                ),
                const SizedBox(width: 6),
                Expanded(
                  child: NovaBoutonSecondaire(
                    libelle: 'Exporter',
                    icone: NovaIcones.exporter,
                    hauteur: 28,
                    onPressed: () => NovaToast.montrer(
                      context,
                      'Exportation du carnet — à venir.',
                      info: true,
                    ),
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

  Widget _panneauPrincipal(
      NovaTokens t, List<(EntreeCarnet, String)> lignes, bool vide) {
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
                onPressed: () => NovaToast.montrer(
                  context,
                  "Ajout d'un appareil au carnet — à venir.",
                  info: true,
                ),
              ),
            ],
          ),
        ),
        // Tableau : en-tête figé + corps défilant.
        Expanded(
          child: Column(
            children: [
              _entete(t),
              Expanded(child: _corps(t, lignes, vide)),
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
  Widget _corps(NovaTokens t, List<(EntreeCarnet, String)> lignes, bool vide) {
    if (_chargement) {
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
          onWakeOnLan: () => _wakeOnLan(entree),
          onMenu: (pos) => _ouvrirMenu(entree, pos),
        );
      },
    );
  }
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

class _LigneGroupe extends StatefulWidget {
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
  State<_LigneGroupe> createState() => _LigneGroupeState();
}

class _LigneGroupeState extends State<_LigneGroupe> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleurTexte =
        widget.couleurTexte ?? (widget.selectionne ? t.texte : t.texte2);
    final couleurIcone = widget.couleurIcone ?? couleurTexte;
    final fond = widget.selectionne
        ? t.selection
        : (_survole ? t.survol : Colors.transparent);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        behavior: HitTestBehavior.opaque,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 7),
          decoration: BoxDecoration(
            color: fond,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            children: [
              NovaIcone(widget.icone, taille: 15, couleur: couleurIcone),
              const SizedBox(width: 9),
              Expanded(
                child: Text(
                  widget.libelle,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    fontSize: 12.5,
                    fontWeight:
                        widget.selectionne ? FontWeight.w500 : FontWeight.w400,
                    color: couleurTexte,
                  ),
                ),
              ),
              if (widget.compte != null) ...[
                const SizedBox(width: 8),
                Text(
                  '${widget.compte}',
                  style: TextStyle(fontSize: 11, color: t.texte3),
                ),
              ],
            ],
          ),
        ),
      ),
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
