/// Chrome applicatif NovaDesk (maquette `anydesk-reference.html`) :
/// barre de titre 40px à onglets (logo pastille rouge + « Accueil » + onglet
/// de session + bouton « + » + contrôles fenêtre ─ ▢ ✕) et barre d'état 28px
/// (« En ligne · Chiffrement de bout en bout · NovaDesk x.y »).
///
/// Contrainte no-admin (doc 03 §5.6) : sans plugin natif de fenêtrage, cette
/// barre est **applicative** (dessinée sous le chrome OS) ; les boutons ─ ▢
/// passent par le shim no-op `window_shim.dart`, ✕ ferme l'application.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show SystemNavigator;
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../state/providers.dart';
import '../theme/nova_theme.dart';
import 'nova_icons.dart';

/// Onglet actif de la barre de titre.
enum NovaOnglet { accueil, session, reglages }

/// Habillage commun des écrans : titre + onglets en haut, contenu au centre,
/// barre d'état en bas. `masquerChrome` (plein écran) ne laisse que le corps.
class NovaAppFrame extends StatelessWidget {
  const NovaAppFrame({
    super.key,
    required this.corps,
    this.ongletActif = NovaOnglet.accueil,
    this.libelleSession,
    this.masquerChrome = false,
    this.etatGauche,
  });

  /// Contenu principal de l'écran.
  final Widget corps;

  /// Onglet mis en évidence (soulignement rouge — usage réservé autorisé).
  final NovaOnglet ongletActif;

  /// Si non nul : un onglet de session (pastille verte + alias) est affiché.
  final String? libelleSession;

  /// Plein écran : masque barre de titre et barre d'état.
  final bool masquerChrome;

  /// Contenu additionnel à gauche de la barre d'état (état de session…).
  final Widget? etatGauche;

  @override
  Widget build(BuildContext context) {
    if (masquerChrome) return corps;
    return Column(
      children: [
        _BarreTitre(
          ongletActif: ongletActif,
          libelleSession: libelleSession,
        ),
        Expanded(child: corps),
        _BarreEtat(gauche: etatGauche),
      ],
    );
  }
}

// ---------------------------------------------------------------------------
// Barre de titre + onglets
// ---------------------------------------------------------------------------

class _BarreTitre extends StatelessWidget {
  const _BarreTitre({required this.ongletActif, this.libelleSession});

  final NovaOnglet ongletActif;
  final String? libelleSession;

  /// Revenir à l'accueil = dépiler jusqu'à la première route.
  void _versAccueil(BuildContext context) {
    if (ongletActif != NovaOnglet.accueil) {
      Navigator.of(context).popUntil((route) => route.isFirst);
    }
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      height: 40,
      decoration: BoxDecoration(
        color: t.barre,
        border: Border(bottom: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          const SizedBox(width: 12),
          const NovaLogo(),
          const SizedBox(width: 8),
          Text(
            'NovaDesk',
            style: TextStyle(
              fontSize: 13.5,
              fontWeight: FontWeight.w700,
              color: t.logo,
            ),
          ),
          const SizedBox(width: 20),
          _Onglet(
            libelle: 'Accueil',
            actif: ongletActif == NovaOnglet.accueil,
            onTap: () => _versAccueil(context),
          ),
          if (libelleSession != null)
            _Onglet(
              libelle: libelleSession!,
              actif: ongletActif == NovaOnglet.session,
              pastilleVerte: true,
              onTap: () {},
            ),
          if (ongletActif == NovaOnglet.reglages)
            _Onglet(libelle: 'Réglages', actif: true, onTap: () {}),
          _BoutonOngletPlus(
            onTap: () {
              // Multi-onglets de session : à venir (une session à la fois).
              ScaffoldMessenger.of(context).showSnackBar(
                const SnackBar(
                  content: Text('Connexions multiples en onglets — à venir.'),
                ),
              );
            },
          ),
          const Spacer(),
          _BoutonFenetre(
            icone: NovaIcones.reglages,
            taille: 15,
            infobulle: 'Réglages',
            onTap: () {
              if (ongletActif != NovaOnglet.reglages) {
                Navigator.of(context).pushNamed('/parametres');
              }
            },
          ),
          const _ControlesFenetre(),
        ],
      ),
    );
  }
}

/// Onglet de la barre de titre : hauteur pleine, filet actif rouge 2px.
class _Onglet extends StatefulWidget {
  const _Onglet({
    required this.libelle,
    required this.actif,
    required this.onTap,
    this.pastilleVerte = false,
  });

  final String libelle;
  final bool actif;
  final bool pastilleVerte;
  final VoidCallback onTap;

  @override
  State<_Onglet> createState() => _OngletState();
}

class _OngletState extends State<_Onglet> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        behavior: HitTestBehavior.opaque,
        child: Container(
          height: 40,
          padding: const EdgeInsets.symmetric(horizontal: 16),
          decoration: BoxDecoration(
            border: Border(
              bottom: BorderSide(
                width: 2,
                color: widget.actif ? kNovaRouge : Colors.transparent,
              ),
            ),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              if (widget.pastilleVerte) ...[
                Container(
                  width: 6,
                  height: 6,
                  decoration: const BoxDecoration(
                    color: kNovaVert,
                    shape: BoxShape.circle,
                  ),
                ),
                const SizedBox(width: 8),
              ],
              Text(
                widget.libelle,
                style: TextStyle(
                  fontSize: 12.5,
                  fontWeight:
                      widget.actif ? FontWeight.w600 : FontWeight.w400,
                  color: widget.actif || _survole ? t.texte : t.texte2,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Bouton « + » : nouvelle connexion en onglet.
class _BoutonOngletPlus extends StatefulWidget {
  const _BoutonOngletPlus({required this.onTap});

  final VoidCallback onTap;

  @override
  State<_BoutonOngletPlus> createState() => _BoutonOngletPlusState();
}

class _BoutonOngletPlusState extends State<_BoutonOngletPlus> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        behavior: HitTestBehavior.opaque,
        child: Container(
          height: 40,
          padding: const EdgeInsets.symmetric(horizontal: 12),
          alignment: Alignment.center,
          child: NovaIcone(
            NovaIcones.plus,
            taille: 15,
            couleur: _survole ? t.texte : t.texte3,
          ),
        ),
      ),
    );
  }
}

/// Contrôles fenêtre ─ ▢ ✕ (44×40, survol discret, fermer = rouge).
class _ControlesFenetre extends StatelessWidget {
  const _ControlesFenetre();

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        _BoutonFenetre(
          icone: NovaIcones.reduire,
          taille: 12,
          infobulle: 'Réduire',
          // Shim no-op : le contrôle réel de fenêtre est un chantier
          // packaging (doc 03 §5.6) — aucun plugin natif ici.
          onTap: () {},
        ),
        _BoutonFenetre(
          icone: NovaIcones.agrandir,
          taille: 11,
          infobulle: 'Agrandir',
          onTap: () {},
        ),
        _BoutonFenetre(
          icone: NovaIcones.fermer,
          taille: 12,
          infobulle: 'Fermer',
          fermeture: true,
          onTap: () => SystemNavigator.pop(),
        ),
      ],
    );
  }
}

class _BoutonFenetre extends StatefulWidget {
  const _BoutonFenetre({
    required this.icone,
    required this.taille,
    required this.infobulle,
    required this.onTap,
    this.fermeture = false,
  });

  final NovaIconeData icone;
  final double taille;
  final String infobulle;
  final VoidCallback onTap;

  /// Le bouton fermer vire au rouge au survol (seul usage autorisé ici).
  final bool fermeture;

  @override
  State<_BoutonFenetre> createState() => _BoutonFenetreState();
}

class _BoutonFenetreState extends State<_BoutonFenetre> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final Color fond = !_survole
        ? Colors.transparent
        : widget.fermeture
            ? kNovaRouge
            : t.survol;
    final Color couleur = _survole
        ? (widget.fermeture ? Colors.white : t.texte)
        : t.texte2;
    return Tooltip(
      message: widget.infobulle,
      child: MouseRegion(
        onEnter: (_) => setState(() => _survole = true),
        onExit: (_) => setState(() => _survole = false),
        child: GestureDetector(
          onTap: widget.onTap,
          behavior: HitTestBehavior.opaque,
          child: Container(
            width: 44,
            height: 40,
            color: fond,
            alignment: Alignment.center,
            child: NovaIcone(widget.icone,
                taille: widget.taille, couleur: couleur),
          ),
        ),
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Logo
// ---------------------------------------------------------------------------

/// Pastille de marque : disque rouge + arc blanc ouvert (maquette `.brand .m`).
class NovaLogo extends StatelessWidget {
  const NovaLogo({super.key, this.taille = 18});

  final double taille;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: taille,
      height: taille,
      child: const CustomPaint(painter: _PeintreLogo()),
    );
  }
}

class _PeintreLogo extends CustomPainter {
  const _PeintreLogo();

  @override
  void paint(Canvas canvas, Size size) {
    final centre = Offset(size.width / 2, size.height / 2);
    canvas.drawCircle(centre, size.width / 2, Paint()..color = kNovaRouge);
    // Arc blanc : anneau inset 5/18, ouvert sur ~90° (bord droit),
    // pivoté de 40° comme dans la maquette.
    final rayon = size.width / 2 - size.width * (5 / 18) + 1;
    final trait = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = size.width * (2 / 18)
      ..strokeCap = StrokeCap.round
      ..color = Colors.white;
    const depart = (40 - 45) * 3.14159265 / 180; // rotation 40° − ouverture
    canvas.drawArc(
      Rect.fromCircle(center: centre, radius: rayon),
      depart,
      3.14159265 * 1.5, // 270° dessinés, 90° ouverts
      false,
      trait,
    );
  }

  @override
  bool shouldRepaint(_PeintreLogo ancien) => false;
}

// ---------------------------------------------------------------------------
// Barre d'état
// ---------------------------------------------------------------------------

class _BarreEtat extends ConsumerWidget {
  const _BarreEtat({this.gauche});

  final Widget? gauche;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final t = NovaTokens.of(context);
    final version = ref.watch(appInfoProvider).maybeWhen(
          data: (info) => info.version,
          orElse: () => '…',
        );
    final styleDiscret = TextStyle(fontSize: 11, color: t.texte3);
    return Container(
      height: 28,
      padding: const EdgeInsets.symmetric(horizontal: 14),
      decoration: BoxDecoration(
        color: t.barre,
        border: Border(top: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          Container(
            width: 7,
            height: 7,
            decoration: const BoxDecoration(
              color: kNovaVert,
              shape: BoxShape.circle,
            ),
          ),
          const SizedBox(width: 6),
          Text('En ligne',
              style: TextStyle(fontSize: 11, color: t.texte2)),
          if (gauche != null) ...[
            const SizedBox(width: 14),
            Expanded(child: gauche!),
          ] else
            const Spacer(),
          const SizedBox(width: 8),
          Text('Chiffrement de bout en bout', style: styleDiscret),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 4),
            child: Text('·',
                style: TextStyle(
                    fontSize: 11,
                    color: t.texte3.withValues(alpha: 0.5))),
          ),
          Text('NovaDesk $version', style: styleDiscret),
        ],
      ),
    );
  }
}
