/// Jeu d'icônes NovaDesk — style « Feather » homogène : viewBox 24×24,
/// trait ~1.7, extrémités et jointures rondes, sans remplissage (sauf
/// pastilles explicites). Les tracés sont ceux de la maquette validée
/// (`anydesk-reference.html`) pour un rendu 1:1.
///
/// Implémentation : chaque icône est une liste de formes (tracé SVG,
/// rectangle arrondi, cercle) interprétées par un [CustomPainter] unique —
/// aucune dépendance, aucune fonte d'icônes hétérogène.
library;

import 'package:flutter/widgets.dart';

// ---------------------------------------------------------------------------
// Modèle : formes d'une icône
// ---------------------------------------------------------------------------

/// Forme élémentaire d'une icône (coordonnées dans le viewBox 24×24).
sealed class NovaForme {
  const NovaForme();
}

/// Tracé SVG (commandes M/L/H/V/C/A/Z, absolues et relatives).
class NovaTrace extends NovaForme {
  const NovaTrace(this.d, {this.plein = false});

  /// Attribut `d` du `<path>` SVG.
  final String d;

  /// Si vrai : remplissage au lieu du trait.
  final bool plein;
}

/// Rectangle arrondi (`<rect x y width height rx>`).
class NovaRect extends NovaForme {
  const NovaRect(this.x, this.y, this.largeur, this.hauteur, this.rayon,
      {this.plein = false});

  final double x, y, largeur, hauteur, rayon;
  final bool plein;
}

/// Cercle (`<circle cx cy r>`).
class NovaCercle extends NovaForme {
  const NovaCercle(this.cx, this.cy, this.rayon, {this.plein = false});

  final double cx, cy, rayon;
  final bool plein;
}

/// Description complète d'une icône.
class NovaIconeData {
  const NovaIconeData(this.formes, {this.graisse = 1.7});

  final List<NovaForme> formes;

  /// Épaisseur du trait en unités viewBox (1.7 par défaut, comme la maquette).
  final double graisse;
}

// ---------------------------------------------------------------------------
// Widget
// ---------------------------------------------------------------------------

/// Icône vectorielle NovaDesk. Couleur héritée de [IconTheme] si absente.
class NovaIcone extends StatelessWidget {
  const NovaIcone(this.icone, {super.key, this.taille = 18, this.couleur});

  final NovaIconeData icone;
  final double taille;
  final Color? couleur;

  @override
  Widget build(BuildContext context) {
    final c = couleur ?? IconTheme.of(context).color ?? const Color(0xFF565C64);
    return SizedBox(
      width: taille,
      height: taille,
      child: CustomPaint(painter: _PeintreIcone(icone: icone, couleur: c)),
    );
  }
}

class _PeintreIcone extends CustomPainter {
  const _PeintreIcone({required this.icone, required this.couleur});

  final NovaIconeData icone;
  final Color couleur;

  @override
  void paint(Canvas canvas, Size size) {
    final echelle = size.shortestSide / 24.0;
    canvas.scale(echelle);

    final trait = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = icone.graisse
      ..strokeCap = StrokeCap.round
      ..strokeJoin = StrokeJoin.round
      ..color = couleur;
    final fond = Paint()
      ..style = PaintingStyle.fill
      ..color = couleur;

    for (final forme in icone.formes) {
      switch (forme) {
        case NovaTrace(:final d, :final plein):
          canvas.drawPath(_CacheTraces.chemin(d), plein ? fond : trait);
        case NovaRect(:final x, :final y, :final largeur, :final hauteur,
              :final rayon, :final plein):
          canvas.drawRRect(
            RRect.fromRectAndRadius(
              Rect.fromLTWH(x, y, largeur, hauteur),
              Radius.circular(rayon),
            ),
            plein ? fond : trait,
          );
        case NovaCercle(:final cx, :final cy, :final rayon, :final plein):
          canvas.drawCircle(Offset(cx, cy), rayon, plein ? fond : trait);
      }
    }
  }

  @override
  bool shouldRepaint(_PeintreIcone ancien) =>
      ancien.icone != icone || ancien.couleur != couleur;
}

// ---------------------------------------------------------------------------
// Interprétation des tracés SVG (avec cache, un parse par `d` unique)
// ---------------------------------------------------------------------------

class _CacheTraces {
  static final Map<String, Path> _cache = {};

  static Path chemin(String d) => _cache.putIfAbsent(d, () => _parser(d));

  /// Mini-interpréteur du langage de tracé SVG : M/m, L/l, H/h, V/v, C/c,
  /// A/a, Z/z, avec répétition implicite de la dernière commande.
  static Path _parser(String d) {
    final chemin = Path();
    final jetons = RegExp(r'[MmLlHhVvCcAaZz]|-?\d*\.?\d+')
        .allMatches(d)
        .map((m) => m.group(0)!)
        .toList();

    var i = 0;
    var commande = '';
    double x = 0, y = 0; // position courante
    double dx = 0, dy = 0; // point de départ du sous-tracé (pour Z)

    double lire() => double.parse(jetons[i++]);

    while (i < jetons.length) {
      final jeton = jetons[i];
      if (RegExp(r'^[A-Za-z]$').hasMatch(jeton)) {
        commande = jeton;
        i++;
        if (commande == 'Z' || commande == 'z') {
          chemin.close();
          x = dx;
          y = dy;
          continue;
        }
      } else if (commande == 'M') {
        commande = 'L'; // paires supplémentaires après M = LineTo implicite
      } else if (commande == 'm') {
        commande = 'l';
      }

      switch (commande) {
        case 'M':
          x = lire();
          y = lire();
          chemin.moveTo(x, y);
          dx = x;
          dy = y;
        case 'm':
          x += lire();
          y += lire();
          chemin.moveTo(x, y);
          dx = x;
          dy = y;
        case 'L':
          x = lire();
          y = lire();
          chemin.lineTo(x, y);
        case 'l':
          x += lire();
          y += lire();
          chemin.lineTo(x, y);
        case 'H':
          x = lire();
          chemin.lineTo(x, y);
        case 'h':
          x += lire();
          chemin.lineTo(x, y);
        case 'V':
          y = lire();
          chemin.lineTo(x, y);
        case 'v':
          y += lire();
          chemin.lineTo(x, y);
        case 'C':
          final x1 = lire(), y1 = lire(), x2 = lire(), y2 = lire();
          x = lire();
          y = lire();
          chemin.cubicTo(x1, y1, x2, y2, x, y);
        case 'c':
          final x1 = x + lire(), y1 = y + lire();
          final x2 = x + lire(), y2 = y + lire();
          x += lire();
          y += lire();
          chemin.cubicTo(x1, y1, x2, y2, x, y);
        case 'A':
        case 'a':
          final rx = lire(), ry = lire(), rotation = lire();
          final grandArc = lire() != 0, sens = lire() != 0;
          final relatif = commande == 'a';
          final fx = relatif ? x + lire() : lire();
          final fy = relatif ? y + lire() : lire();
          chemin.arcToPoint(
            Offset(fx, fy),
            radius: Radius.elliptical(rx, ry),
            rotation: rotation,
            largeArc: grandArc,
            clockwise: sens,
          );
          x = fx;
          y = fy;
        default:
          // Commande inconnue : on saute le jeton pour ne jamais boucler.
          i++;
      }
    }
    return chemin;
  }
}

// ---------------------------------------------------------------------------
// Catalogue (tracés de la maquette + compléments Feather)
// ---------------------------------------------------------------------------

abstract final class NovaIcones {
  /// Fenêtre : réduire.
  static const reduire =
      NovaIconeData([NovaTrace('M5 12h14')], graisse: 2);

  /// Fenêtre : agrandir / restaurer.
  static const agrandir =
      NovaIconeData([NovaRect(5, 5, 14, 14, 1.5)], graisse: 2);

  /// Fenêtre : fermer — et « Terminer » en session.
  static const fermer =
      NovaIconeData([NovaTrace('M6 6l12 12M18 6L6 18')], graisse: 2);

  /// Onglet « + » (nouvelle connexion).
  static const plus =
      NovaIconeData([NovaTrace('M12 5v14M5 12h14')], graisse: 2);

  /// Enveloppe du champ adresse.
  static const adresse = NovaIconeData([
    NovaRect(3, 5, 18, 14, 2),
    NovaTrace('M3 7l9 6 9-6'),
  ], graisse: 1.8);

  /// Flèche « Se connecter ».
  static const flecheDroite =
      NovaIconeData([NovaTrace('M5 12h13M13 6l6 6-6 6')], graisse: 2.2);

  /// Information (astuce sous le champ).
  static const info = NovaIconeData([
    NovaCercle(12, 12, 9),
    NovaTrace('M12 11v5M12 8h.01'),
  ], graisse: 1.8);

  /// Copier.
  static const copier = NovaIconeData([
    NovaRect(9, 9, 12, 12, 2),
    NovaTrace('M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1'),
  ], graisse: 1.8);

  /// Partager.
  static const partager = NovaIconeData([
    NovaTrace('M4 12v7a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-7'),
    NovaTrace('M16 6l-4-4-4 4M12 2v13'),
  ], graisse: 1.8);

  /// Moniteur avec pied (alias du poste, écran distant).
  static const moniteur = NovaIconeData([
    NovaRect(2, 3, 20, 14, 2),
    NovaTrace('M8 21h8M12 17v4'),
  ]);

  /// Deux moniteurs superposés (sélecteur d'écran).
  static const moniteurs = NovaIconeData([
    NovaRect(2, 3, 13, 9, 1.5),
    NovaRect(9, 10, 13, 9, 1.5),
  ]);

  /// Curseurs qualité / vitesse.
  static const qualite = NovaIconeData([
    NovaTrace('M5 8h14M5 16h14'),
    NovaCercle(9, 8, 2.3, plein: true),
    NovaCercle(15, 16, 2.3, plein: true),
  ]);

  /// Plein écran.
  static const pleinEcran =
      NovaIconeData([NovaTrace('M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5')]);

  /// Quitter le plein écran.
  static const quitterPleinEcran =
      NovaIconeData([NovaTrace('M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5')]);

  /// Clavier.
  static const clavier = NovaIconeData([
    NovaRect(2, 6, 20, 12, 2),
    NovaTrace('M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10'),
  ]);

  /// Ctrl+Alt+Suppr (gerbe + arc).
  static const ctrlAltSuppr =
      NovaIconeData([NovaTrace('M12 4v5M6 6l3 3M18 6l-3 3M4 15a8 8 0 0 0 16 0')]);

  /// Presse-papiers.
  static const pressePapiers = NovaIconeData([
    NovaRect(8, 3, 8, 4, 1.2),
    NovaTrace('M9 5H6v16h12V5h-3'),
  ]);

  /// Dossier (transfert de fichiers).
  static const dossier = NovaIconeData([
    NovaTrace('M4 20a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2z'),
  ]);

  /// Bulle de discussion.
  static const discussion = NovaIconeData([
    NovaTrace('M21 15a2 2 0 0 1-2 2H8l-4 4V5a2 2 0 0 1 2-2h13a2 2 0 0 1 2 2z'),
  ]);

  /// Enregistrement (cercle + point).
  static const enregistrer = NovaIconeData([
    NovaCercle(12, 12, 8),
    NovaCercle(12, 12, 3.2, plein: true),
  ]);

  /// Menu « plus » (trois points).
  static const troisPoints = NovaIconeData([
    NovaCercle(5, 12, 1.6, plein: true),
    NovaCercle(12, 12, 1.6, plein: true),
    NovaCercle(19, 12, 1.6, plein: true),
  ]);

  /// Cadenas (sécurité / chiffrement).
  static const cadenas = NovaIconeData([
    NovaRect(3, 11, 18, 10, 2),
    NovaTrace('M7 11V7a5 5 0 0 1 10 0v4'),
  ]);

  /// Bouclier (accès non surveillé, profils).
  static const bouclier = NovaIconeData([
    NovaTrace('M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z'),
  ]);

  /// Bouclier coché (connexion vérifiée).
  static const bouclierCoche = NovaIconeData([
    NovaTrace('M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z'),
    NovaTrace('M9 12l2 2 4-4'),
  ]);

  /// Étoile (favori).
  static const etoile = NovaIconeData([
    NovaTrace(
        'M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z'),
  ]);

  /// Étoile pleine (favori actif).
  static const etoilePleine = NovaIconeData([
    NovaTrace(
        'M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z',
        plein: true),
  ]);

  /// Éclair (actions à distance).
  static const eclair =
      NovaIconeData([NovaTrace('M13 2L3 14h9l-1 8 10-12h-9l1-8z')]);

  /// Œil / œil barré (visibilité du mot de passe).
  static const oeil = NovaIconeData([
    NovaTrace('M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z'),
    NovaCercle(12, 12, 3),
  ]);
  static const oeilBarre = NovaIconeData([
    NovaTrace(
        'M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24'),
    NovaTrace('M1 1l22 22'),
  ]);

  /// Corbeille (supprimer).
  static const corbeille = NovaIconeData([
    NovaTrace('M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2'),
    NovaTrace('M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6'),
  ]);

  /// Crayon (renommer).
  static const crayon = NovaIconeData([
    NovaTrace('M17 3a2.83 2.83 0 0 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z'),
  ]);

  /// Coche (valider).
  static const coche = NovaIconeData([NovaTrace('M20 6L9 17l-5-5')], graisse: 2);

  /// Recharger / régénérer.
  static const recharger = NovaIconeData([
    NovaTrace('M23 4v6h-6M1 20v-6h6'),
    NovaTrace('M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15'),
  ]);

  /// Réglages (curseurs verticaux, Feather « sliders »).
  static const reglages = NovaIconeData([
    NovaTrace(
        'M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6'),
  ]);

  /// Utilisateur (compte, identité entrante).
  static const utilisateur = NovaIconeData([
    NovaTrace('M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2'),
    NovaCercle(12, 7, 4),
  ]);

  /// Horloge (sessions récentes).
  static const horloge = NovaIconeData([
    NovaCercle(12, 12, 9),
    NovaTrace('M12 7v5l3 3'),
  ]);

  /// Clé (empreinte / secret).
  static const cle = NovaIconeData([
    NovaTrace(
        'M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4'),
  ]);

  /// Globe (réseau).
  static const globe = NovaIconeData([
    NovaCercle(12, 12, 10),
    NovaTrace('M2 12h20'),
    NovaTrace('M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z'),
  ]);

  /// Appareil photo (capture d'écran).
  static const capture = NovaIconeData([
    NovaTrace(
        'M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z'),
    NovaCercle(12, 13, 4),
  ]);

  /// Marche/arrêt (redémarrer, verrouiller la session).
  static const alimentation = NovaIconeData([
    NovaTrace('M18.36 6.64a9 9 0 1 1-12.73 0M12 2v10'),
  ]);

  /// Terminal (tunnel TCP).
  static const terminal =
      NovaIconeData([NovaTrace('M4 17l6-5-6-5M12 19h8')]);

  /// Haut-parleur (audio).
  static const audio = NovaIconeData([
    NovaTrace('M11 5L6 9H2v6h4l5 4V5z'),
    NovaTrace('M15.54 8.46a5 5 0 0 1 0 7.07'),
  ]);

  /// Souris.
  static const souris = NovaIconeData([
    NovaRect(7, 2, 10, 20, 5),
    NovaTrace('M12 6v4'),
  ]);

  /// Triangle d'avertissement.
  static const avertissement = NovaIconeData([
    NovaTrace(
        'M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z'),
    NovaTrace('M12 9v4M12 17h.01'),
  ]);

  /// Chevrons.
  static const chevronBas = NovaIconeData([NovaTrace('M6 9l6 6 6-6')]);
  static const chevronDroit = NovaIconeData([NovaTrace('M9 6l6 6-6 6')]);

  /// Lien coupé (session terminée).
  static const lienCoupe = NovaIconeData([
    NovaTrace('M18.84 12.25l1.72-1.71a5 5 0 0 0-7.07-7.07l-1.72 1.71'),
    NovaTrace('M5.17 11.75l-1.72 1.71a5 5 0 0 0 7.07 7.07l1.71-1.71'),
    NovaTrace('M2 2l20 20'),
  ]);

  /// Lune (thème sombre).
  static const lune = NovaIconeData([
    NovaTrace('M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z'),
  ], graisse: 1.9);

  /// Liste (ACL, liste blanche).
  static const liste = NovaIconeData([
    NovaTrace('M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01'),
  ]);
}
