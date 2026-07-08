/// Lecteur d'enregistrements NovaDesk (maquette `novadesk-app.html`, vue
/// `#v-enreg` / `.player`) : liste latérale des sessions enregistrées à gauche
/// (`.reclist`) et scène de lecture à droite (`.stage`) — canevas d'aperçu
/// sombre (`.canvas`) surmontant la barre de contrôles (`.pctrl`).
///
/// La sélection d'un enregistrement met à jour le titre et le détail du canevas
/// ainsi que la durée totale. La lecture est purement présentationnelle
/// (remplissage figé à 38 %, pas de lecture réelle) — fidèle à la maquette.
///
/// Les surfaces d'aperçu (vignettes `.th`, canevas dégradé) sont volontairement
/// sombres : comme l'écran de session, elles représentent une surface vidéo et
/// utilisent des valeurs fixes issues de la maquette. Tout le reste (filets,
/// liste, contrôles) est piloté par le thème [NovaTokens].
library;

import 'package:flutter/material.dart';

import '../app_routes.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';

// ===========================================================================
// Données (maquette : tableau `REC`)
// ===========================================================================

/// Un enregistrement de session (une entrée du tableau `REC` de la maquette).
class _Enregistrement {
  const _Enregistrement({
    required this.nom,
    required this.date,
    required this.heure,
    required this.duree,
  });

  /// Alias du poste enregistré (ex. « poste-bureau »).
  final String nom;

  /// Date de l'enregistrement (ex. « 8 juil. »).
  final String date;

  /// Heure de début (ex. « 14:07 »).
  final String heure;

  /// Durée totale (ex. « 12:34 »).
  final String duree;

  /// Ligne méta de la liste (maquette `.mt`) : « 8 juil. 14:07 · 12:34 ».
  String get meta => '$date $heure · $duree';

  /// Détail affiché sous le titre du canevas : « 8 juil. 2026 · 12:34 · H.264 ».
  String get detailCanevas => '$date 2026 · $duree · H.264';
}

/// Enregistrements de démonstration — reprise exacte du tableau `REC`.
const List<_Enregistrement> _enregistrements = [
  _Enregistrement(
      nom: 'poste-bureau', date: '8 juil.', heure: '14:07', duree: '12:34'),
  _Enregistrement(
      nom: 'serveur-nas', date: '7 juil.', heure: '18:40', duree: '04:02'),
  _Enregistrement(
      nom: 'pc-marie', date: '5 juil.', heure: '14:07', duree: '21:15'),
];

// --- Surfaces d'aperçu sombres (valeurs fixes de la maquette) --------------

/// Fond des vignettes `.th` (`#1c2430`).
const Color _fondVignette = Color(0xFF1C2430);

/// Icône de lecture dans les vignettes `.th` (`#5b6472`).
const Color _icoVignette = Color(0xFF5B6472);

/// Dégradé du canevas `.canvas` (`linear-gradient(160deg,#20293a,#12182550)`).
const Color _canevasHaut = Color(0xFF20293A);
const Color _canevasBas = Color(0xFF12182D);

/// Texte de base du canevas (`#8a94b4`).
const Color _canevasTexte = Color(0xFF8A94B4);

/// Nom en surbrillance dans le canevas (`#cdd6f2`).
const Color _canevasTitre = Color(0xFFCDD6F2);

// ===========================================================================
// Écran
// ===========================================================================

/// Écran « Lecteur d'enregistrements » (vue `enregistrements` du rail).
class RecordingsScreen extends StatefulWidget {
  const RecordingsScreen({super.key});

  /// Route nommée de l'écran.
  static const String route = NovaRoutes.enregistrements;

  @override
  State<RecordingsScreen> createState() => _RecordingsScreenState();
}

class _RecordingsScreenState extends State<RecordingsScreen> {
  /// Index de l'enregistrement sélectionné (premier par défaut).
  int _indexSelectionne = 0;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _construireListe(t),
        Expanded(child: _construireScene(t)),
      ],
    );
  }

  // -------------------------------------------------------------------------
  // Liste latérale (maquette `.reclist`)
  // -------------------------------------------------------------------------

  Widget _construireListe(NovaTokens t) {
    return Container(
      width: 228,
      decoration: BoxDecoration(
        border: Border(right: BorderSide(color: t.filet)),
      ),
      child: ListView.builder(
        padding: const EdgeInsets.all(8),
        itemCount: _enregistrements.length,
        itemBuilder: (context, i) => _LigneEnregistrement(
          enregistrement: _enregistrements[i],
          selectionne: i == _indexSelectionne,
          onTap: () => setState(() => _indexSelectionne = i),
        ),
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Scène de lecture (maquette `.stage`)
  // -------------------------------------------------------------------------

  Widget _construireScene(NovaTokens t) {
    final enr = _enregistrements[_indexSelectionne];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(child: _construireCanevas(enr)),
        _construireControles(t, enr),
      ],
    );
  }

  /// Canevas d'aperçu sombre (maquette `.canvas`).
  Widget _construireCanevas(_Enregistrement enr) {
    return Container(
      alignment: Alignment.center,
      decoration: const BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
          colors: [_canevasHaut, _canevasBas],
        ),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.center,
        children: [
          Text.rich(
            TextSpan(
              text: 'Enregistrement — ',
              style: const TextStyle(fontSize: 13, color: _canevasTexte),
              children: [
                TextSpan(
                  text: enr.nom,
                  style: const TextStyle(
                    color: _canevasTitre,
                    fontWeight: FontWeight.w700,
                  ),
                ),
              ],
            ),
            textAlign: TextAlign.center,
          ),
          const SizedBox(height: 4),
          Text(
            enr.detailCanevas,
            textAlign: TextAlign.center,
            style: TextStyle(
              fontSize: 11,
              color: _canevasTexte.withValues(alpha: 0.7),
            ),
          ),
        ],
      ),
    );
  }

  /// Barre de contrôles de lecture (maquette `.pctrl`).
  Widget _construireControles(NovaTokens t, _Enregistrement enr) {
    return Container(
      height: 50,
      padding: const EdgeInsets.symmetric(horizontal: 16),
      decoration: BoxDecoration(
        border: Border(top: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          const _BoutonLecture(),
          const SizedBox(width: 12),
          Text('04:41', style: _styleTemps(t.texte2)),
          const SizedBox(width: 12),
          Expanded(child: _construireTimeline(t)),
          const SizedBox(width: 12),
          Text(enr.duree, style: _styleTemps(t.texte3)),
          const SizedBox(width: 12),
          NovaBoutonAction(
            icone: NovaIcones.agrandirCadre,
            onTap: () {},
          ),
        ],
      ),
    );
  }

  /// Style commun des étiquettes de temps (12 px, chiffres tabulaires).
  TextStyle _styleTemps(Color couleur) => TextStyle(
        fontSize: 12,
        color: couleur,
        fontFeatures: const [FontFeature.tabularFigures()],
      );

  /// Ligne de temps présentationnelle (maquette `.timeline`) : fond neutre,
  /// remplissage rouge figé à 38 % et bouton rond à son extrémité.
  Widget _construireTimeline(NovaTokens t) {
    const fraction = 0.38;
    return SizedBox(
      height: 11,
      child: LayoutBuilder(
        builder: (context, contraintes) {
          final largeur = contraintes.maxWidth;
          return Stack(
            clipBehavior: Clip.none,
            alignment: Alignment.centerLeft,
            children: [
              // Barre de fond (4 px).
              Container(
                height: 4,
                width: largeur,
                decoration: BoxDecoration(
                  color: t.filetFort,
                  borderRadius: BorderRadius.circular(2),
                ),
              ),
              // Remplissage (38 %).
              Container(
                height: 4,
                width: largeur * fraction,
                decoration: BoxDecoration(
                  color: kNovaRouge,
                  borderRadius: BorderRadius.circular(2),
                ),
              ),
              // Bouton (knob) centré sur l'extrémité du remplissage.
              Positioned(
                left: largeur * fraction - 5.5,
                top: 0,
                bottom: 0,
                child: Center(
                  child: Container(
                    width: 11,
                    height: 11,
                    decoration: const BoxDecoration(
                      color: kNovaRouge,
                      shape: BoxShape.circle,
                    ),
                  ),
                ),
              ),
            ],
          );
        },
      ),
    );
  }
}

// ===========================================================================
// Ligne d'enregistrement (maquette `.rec`)
// ===========================================================================

/// Une entrée de la liste latérale : vignette + nom + méta, avec survol
/// (`.rec:hover`) et état sélectionné (`.rec.on`).
class _LigneEnregistrement extends StatefulWidget {
  const _LigneEnregistrement({
    required this.enregistrement,
    required this.selectionne,
    required this.onTap,
  });

  final _Enregistrement enregistrement;
  final bool selectionne;
  final VoidCallback onTap;

  @override
  State<_LigneEnregistrement> createState() => _LigneEnregistrementState();
}

class _LigneEnregistrementState extends State<_LigneEnregistrement> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    // `.rec.on` l'emporte sur `.rec:hover` (même spécificité, règle plus tardive).
    final Color fond = widget.selectionne
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
          padding: const EdgeInsets.all(8),
          decoration: BoxDecoration(
            color: fond,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            children: [
              // Vignette `.th` (surface d'aperçu sombre).
              Container(
                width: 54,
                height: 32,
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: _fondVignette,
                  borderRadius: BorderRadius.circular(3),
                ),
                child: const NovaIcone(NovaIcones.lecture,
                    taille: 16, couleur: _icoVignette),
              ),
              const SizedBox(width: 10),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(
                      widget.enregistrement.nom,
                      style: TextStyle(
                        fontSize: 12.5,
                        fontWeight: FontWeight.w500,
                        color: t.texte,
                      ),
                    ),
                    const SizedBox(height: 2),
                    Text(
                      widget.enregistrement.meta,
                      style: TextStyle(
                        fontSize: 11,
                        color: t.texte3,
                        fontFeatures: const [FontFeature.tabularFigures()],
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
}

// ===========================================================================
// Bouton de lecture (maquette `.pbtn`)
// ===========================================================================

/// Bouton rond rouge de lecture — présentationnel (pas de lecture réelle),
/// curseur cliquable pour l'affordance (maquette `.pbtn`).
class _BoutonLecture extends StatelessWidget {
  const _BoutonLecture();

  @override
  Widget build(BuildContext context) {
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      child: GestureDetector(
        onTap: () {},
        behavior: HitTestBehavior.opaque,
        child: Container(
          width: 32,
          height: 32,
          alignment: Alignment.center,
          decoration: const BoxDecoration(
            color: kNovaRouge,
            shape: BoxShape.circle,
          ),
          child: const NovaIcone(NovaIcones.lecture,
              taille: 15, couleur: Colors.white),
        ),
      ),
    );
  }
}
