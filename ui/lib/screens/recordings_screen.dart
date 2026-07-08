/// Lecteur d'enregistrements NovaDesk (maquette `novadesk-app.html`, vue
/// `#v-enreg` / `.player`) : liste latérale des sessions enregistrées à gauche
/// (`.reclist`) et scène de lecture à droite (`.stage`) — canevas d'aperçu
/// sombre (`.canvas`) surmontant la barre de contrôles (`.pctrl`).
///
/// La liste est désormais alimentée par l'état persistant (`list_recordings`) :
/// nom, date, durée et taille **réels** des fichiers présents. État vide si
/// aucun enregistrement. La lecture reste présentationnelle (remplissage figé à
/// 38 %, pas de lecture réelle) — fidèle à la maquette.
///
/// Les surfaces d'aperçu (vignettes `.th`, canevas dégradé) sont volontairement
/// sombres : comme l'écran de session, elles représentent une surface vidéo et
/// utilisent des valeurs fixes issues de la maquette. Tout le reste (filets,
/// liste, contrôles) est piloté par le thème [NovaTokens].
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';

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
// Formatage des métadonnées d'enregistrement
// ===========================================================================

const List<String> _moisCourts = [
  'janv.', 'févr.', 'mars', 'avr.', 'mai', 'juin', //
  'juil.', 'août', 'sept.', 'oct.', 'nov.', 'déc.'
];

/// Date courte française d'un horodatage Unix (secondes) : « 8 juil. ».
String _formaterDateCourte(int unixSecondes) {
  final d = DateTime.fromMillisecondsSinceEpoch(unixSecondes * 1000);
  return '${d.day} ${_moisCourts[d.month - 1]}';
}

/// Heure d'un horodatage Unix (secondes) : « 14:07 ».
String _formaterHeure(int unixSecondes) {
  final d = DateTime.fromMillisecondsSinceEpoch(unixSecondes * 1000);
  return '${d.hour.toString().padLeft(2, '0')}:'
      '${d.minute.toString().padLeft(2, '0')}';
}

/// Durée « mm:ss » (ou « h:mm:ss ») depuis un nombre de secondes.
String _formaterDuree(double secondes) {
  final total = secondes.round();
  final h = total ~/ 3600;
  final m = (total % 3600) ~/ 60;
  final s = total % 60;
  final mm = m.toString().padLeft(2, '0');
  final ss = s.toString().padLeft(2, '0');
  return h > 0 ? '$h:$mm:$ss' : '$mm:$ss';
}

/// Taille de fichier lisible (Ko / Mo / Go).
String _formaterTaille(int octets) {
  if (octets < 1024) return '$octets o';
  if (octets < 1024 * 1024) return '${(octets / 1024).toStringAsFixed(0)} Ko';
  if (octets < 1024 * 1024 * 1024) {
    return '${(octets / (1024 * 1024)).toStringAsFixed(0)} Mo';
  }
  return '${(octets / (1024 * 1024 * 1024)).toStringAsFixed(1).replaceAll('.', ',')} Go';
}

/// Ligne méta d'une entrée : « 8 juil. 14:07 · 12:34 · 486 Mo ».
String _meta(RecordingDto r) =>
    '${_formaterDateCourte(r.date)} ${_formaterHeure(r.date)} · '
    '${_formaterDuree(r.dureeS)} · ${_formaterTaille(r.tailleOctets)}';

/// Détail sous le titre du canevas : « poste-bureau_1407.mp4 · 12:34 ».
String _detailCanevas(RecordingDto r) =>
    '${_formaterDateCourte(r.date)} · ${_formaterDuree(r.dureeS)} · '
    '${_extension(r.nom)}';

/// Extension en capitales (ex. « MP4 »), ou « ENR » à défaut.
String _extension(String nom) {
  final point = nom.lastIndexOf('.');
  if (point < 0 || point == nom.length - 1) return 'ENR';
  return nom.substring(point + 1).toUpperCase();
}

// ===========================================================================
// Écran
// ===========================================================================

/// Écran « Lecteur d'enregistrements » (vue `enregistrements` du rail).
class RecordingsScreen extends ConsumerStatefulWidget {
  const RecordingsScreen({super.key});

  /// Route nommée de l'écran.
  static const String route = NovaRoutes.enregistrements;

  @override
  ConsumerState<RecordingsScreen> createState() => _RecordingsScreenState();
}

class _RecordingsScreenState extends ConsumerState<RecordingsScreen> {
  /// Index de l'enregistrement sélectionné (premier par défaut).
  int _indexSelectionne = 0;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final asyncRec = ref.watch(recordingsProvider);
    final enregistrements = asyncRec.valueOrNull ?? const <RecordingDto>[];

    if (asyncRec.isLoading && enregistrements.isEmpty) {
      return const Center(
        child: SizedBox(
          width: 22,
          height: 22,
          child: CircularProgressIndicator(strokeWidth: 2),
        ),
      );
    }
    if (enregistrements.isEmpty) {
      return const NovaEmptyState(
        icone: NovaIcones.enregistrements,
        titre: 'Aucun enregistrement',
        sousTitre: 'Les sessions enregistrées apparaîtront ici.',
      );
    }

    final index = _indexSelectionne.clamp(0, enregistrements.length - 1);
    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _construireListe(t, enregistrements, index),
        Expanded(child: _construireScene(t, enregistrements[index])),
      ],
    );
  }

  // -------------------------------------------------------------------------
  // Liste latérale (maquette `.reclist`)
  // -------------------------------------------------------------------------

  Widget _construireListe(
      NovaTokens t, List<RecordingDto> enregistrements, int index) {
    return Container(
      width: 228,
      decoration: BoxDecoration(
        border: Border(right: BorderSide(color: t.filet)),
      ),
      child: ListView.builder(
        padding: const EdgeInsets.all(8),
        itemCount: enregistrements.length,
        itemBuilder: (context, i) => _LigneEnregistrement(
          enregistrement: enregistrements[i],
          selectionne: i == index,
          onTap: () => setState(() => _indexSelectionne = i),
        ),
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Scène de lecture (maquette `.stage`)
  // -------------------------------------------------------------------------

  Widget _construireScene(NovaTokens t, RecordingDto enr) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(child: _construireCanevas(enr)),
        _construireControles(t, enr),
      ],
    );
  }

  /// Canevas d'aperçu sombre (maquette `.canvas`).
  Widget _construireCanevas(RecordingDto enr) {
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
            _detailCanevas(enr),
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
  Widget _construireControles(NovaTokens t, RecordingDto enr) {
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
          Text(_formaterDuree(enr.dureeS * 0.38), style: _styleTemps(t.texte2)),
          const SizedBox(width: 12),
          Expanded(child: _construireTimeline(t)),
          const SizedBox(width: 12),
          Text(_formaterDuree(enr.dureeS), style: _styleTemps(t.texte3)),
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

  final RecordingDto enregistrement;
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
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: 12.5,
                        fontWeight: FontWeight.w500,
                        color: t.texte,
                      ),
                    ),
                    const SizedBox(height: 2),
                    Text(
                      _meta(widget.enregistrement),
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
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
