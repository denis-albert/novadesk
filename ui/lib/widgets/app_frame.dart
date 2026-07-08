/// Chrome applicatif NovaDesk (maquette `novadesk-app.html`) : barre de titre
/// 38 px à onglets (logo pastille rouge + « NovaDesk » + onglet Accueil +
/// onglet de session + « + » + « Compte » + contrôles fenêtre ─ ▢ ✕), **rail
/// de navigation** 50 px (Accueil, Carnet, Enregistrements, Accès non
/// surveillé, Réglages) et barre d'état 26 px.
///
/// Contrainte no-admin : sans plugin natif de fenêtrage, cette barre est
/// applicative (dessinée sous le chrome OS) ; ─ ▢ passent par le shim no-op
/// `window_shim.dart`, ✕ ferme l'application.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show SystemNavigator;
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import 'nova_icons.dart';

/// Vue active — pilote la mise en évidence du rail et de l'onglet.
enum NovaVue { accueil, carnet, enregistrements, nonSurveille, reglages, session }

/// Habillage commun des écrans : barre de titre + rail + contenu + barre d'état.
class NovaAppFrame extends StatelessWidget {
  const NovaAppFrame({
    super.key,
    required this.corps,
    this.vue = NovaVue.accueil,
    this.libelleSession,
    this.masquerChrome = false,
    this.afficherRail = true,
    this.etatGauche,
  });

  /// Contenu principal de l'écran.
  final Widget corps;

  /// Vue active (rail + onglet).
  final NovaVue vue;

  /// Si non nul : un onglet de session (pastille verte + alias) est affiché.
  final String? libelleSession;

  /// Plein écran : masque tout le chrome, ne laisse que le corps.
  final bool masquerChrome;

  /// Affiche le rail de navigation (masqué en session : la surface le couvre).
  final bool afficherRail;

  /// Contenu additionnel à gauche de la barre d'état (stats de session…).
  final Widget? etatGauche;

  @override
  Widget build(BuildContext context) {
    if (masquerChrome) return corps;
    return Column(
      children: [
        _BarreTitre(vue: vue, libelleSession: libelleSession),
        Expanded(
          child: afficherRail
              ? Row(
                  children: [
                    _RailNavigation(vue: vue),
                    Expanded(child: corps),
                  ],
                )
              : corps,
        ),
        _BarreEtat(gauche: etatGauche),
      ],
    );
  }
}

// ---------------------------------------------------------------------------
// Navigation partagée : pile ≤ 2 (accueil = base, une vue au-dessus au plus).
// ---------------------------------------------------------------------------

void naviguerVersVue(BuildContext context, String route) {
  final nav = Navigator.of(context);
  nav.popUntil((r) => r.isFirst);
  if (route != NovaRoutes.accueil) {
    nav.pushNamed(route);
  }
}

// ---------------------------------------------------------------------------
// Barre de titre + onglets
// ---------------------------------------------------------------------------

class _BarreTitre extends StatelessWidget {
  const _BarreTitre({required this.vue, this.libelleSession});

  final NovaVue vue;
  final String? libelleSession;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final enSession = vue == NovaVue.session;
    return Container(
      height: 38,
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
                fontSize: 13.5, fontWeight: FontWeight.w700, color: t.logo),
          ),
          const SizedBox(width: 14),
          _OngletTitre(
            libelle: 'Accueil',
            icone: NovaIcones.accueil,
            actif: !enSession,
            onTap: () => naviguerVersVue(context, NovaRoutes.accueil),
          ),
          if (libelleSession != null)
            _OngletSession(
              libelle: libelleSession!,
              actif: enSession,
              onFermer: () => Navigator.of(context).maybePop(),
            ),
          _BoutonPlus(
            onTap: () => ScaffoldMessenger.of(context).showSnackBar(
              const SnackBar(
                  content: Text('Connexions multiples en onglets — à venir.')),
            ),
          ),
          const Spacer(),
          _BoutonCompte(
            onTap: () => ScaffoldMessenger.of(context).showSnackBar(
              const SnackBar(content: Text('Gestion du compte — à venir.')),
            ),
          ),
          const _ControlesFenetre(),
        ],
      ),
    );
  }
}

/// Onglet de la barre de titre (hauteur pleine, filet actif rouge 2 px bas).
class _OngletTitre extends StatefulWidget {
  const _OngletTitre({
    required this.libelle,
    required this.icone,
    required this.actif,
    required this.onTap,
  });

  final String libelle;
  final IconData icone;
  final bool actif;
  final VoidCallback onTap;

  @override
  State<_OngletTitre> createState() => _OngletTitreState();
}

class _OngletTitreState extends State<_OngletTitre> {
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
          height: 38,
          padding: const EdgeInsets.symmetric(horizontal: 12),
          decoration: BoxDecoration(
            color: widget.actif ? t.fenetre : Colors.transparent,
            border: Border(
              right: BorderSide(color: t.filet),
              bottom: BorderSide(
                width: 2,
                color: widget.actif ? kNovaRouge : Colors.transparent,
              ),
            ),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              NovaIcone(widget.icone,
                  taille: 14,
                  couleur: widget.actif || _survole ? t.texte : t.texte2),
              const SizedBox(width: 8),
              Text(
                widget.libelle,
                style: TextStyle(
                  fontSize: 12.5,
                  fontWeight: widget.actif ? FontWeight.w600 : FontWeight.w400,
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

/// Onglet de session : pastille verte + alias + croix de fermeture.
class _OngletSession extends StatelessWidget {
  const _OngletSession({
    required this.libelle,
    required this.actif,
    required this.onFermer,
  });

  final String libelle;
  final bool actif;
  final VoidCallback onFermer;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      height: 38,
      padding: const EdgeInsets.symmetric(horizontal: 12),
      decoration: BoxDecoration(
        color: actif ? t.fenetre : Colors.transparent,
        border: Border(
          right: BorderSide(color: t.filet),
          bottom: BorderSide(
            width: 2,
            color: actif ? kNovaRouge : Colors.transparent,
          ),
        ),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: 6,
            height: 6,
            decoration: BoxDecoration(color: t.vert, shape: BoxShape.circle),
          ),
          const SizedBox(width: 8),
          Text(
            libelle,
            style: TextStyle(
              fontSize: 12.5,
              fontWeight: actif ? FontWeight.w600 : FontWeight.w400,
              color: actif ? t.texte : t.texte2,
            ),
          ),
          const SizedBox(width: 8),
          _CroixOnglet(onTap: onFermer),
        ],
      ),
    );
  }
}

class _CroixOnglet extends StatefulWidget {
  const _CroixOnglet({required this.onTap});
  final VoidCallback onTap;

  @override
  State<_CroixOnglet> createState() => _CroixOngletState();
}

class _CroixOngletState extends State<_CroixOnglet> {
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
        child: Container(
          width: 16,
          height: 16,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: _survole ? t.survol : Colors.transparent,
            borderRadius: BorderRadius.circular(3),
          ),
          child: NovaIcone(NovaIcones.fermer,
              taille: 11, couleur: _survole ? t.texte : t.texte3),
        ),
      ),
    );
  }
}

/// Bouton « + » (nouvelle connexion en onglet).
class _BoutonPlus extends StatefulWidget {
  const _BoutonPlus({required this.onTap});
  final VoidCallback onTap;

  @override
  State<_BoutonPlus> createState() => _BoutonPlusState();
}

class _BoutonPlusState extends State<_BoutonPlus> {
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
          height: 38,
          padding: const EdgeInsets.symmetric(horizontal: 10),
          alignment: Alignment.center,
          child: NovaIcone(NovaIcones.plus,
              taille: 15, couleur: _survole ? t.texte : t.texte3),
        ),
      ),
    );
  }
}

/// « Compte » à droite de la barre de titre (maquette `.acct`).
class _BoutonCompte extends StatefulWidget {
  const _BoutonCompte({required this.onTap});
  final VoidCallback onTap;

  @override
  State<_BoutonCompte> createState() => _BoutonCompteState();
}

class _BoutonCompteState extends State<_BoutonCompte> {
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
          height: 38,
          padding: const EdgeInsets.symmetric(horizontal: 11),
          color: _survole ? t.survol : Colors.transparent,
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              NovaIcone(NovaIcones.utilisateur, taille: 15, couleur: t.texte2),
              const SizedBox(width: 7),
              Text('Compte',
                  style: TextStyle(fontSize: 12, color: t.texte2)),
            ],
          ),
        ),
      ),
    );
  }
}

/// Contrôles fenêtre ─ ▢ ✕ (38×38, survol discret, fermer = rouge).
class _ControlesFenetre extends StatelessWidget {
  const _ControlesFenetre();

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        _BoutonFenetre(
          icone: NovaIcones.reduire,
          infobulle: 'Réduire',
          onTap: () {},
        ),
        _BoutonFenetre(
          icone: NovaIcones.agrandir,
          infobulle: 'Agrandir',
          onTap: () {},
        ),
        _BoutonFenetre(
          icone: NovaIcones.fermer,
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
    required this.infobulle,
    required this.onTap,
    this.fermeture = false,
  });

  final IconData icone;
  final String infobulle;
  final VoidCallback onTap;
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
            width: 38,
            height: 38,
            color: fond,
            alignment: Alignment.center,
            child: NovaIcone(widget.icone, taille: 15, couleur: couleur),
          ),
        ),
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Rail de navigation
// ---------------------------------------------------------------------------

class _RailNavigation extends StatelessWidget {
  const _RailNavigation({required this.vue});

  final NovaVue vue;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      width: 50,
      decoration: BoxDecoration(
        color: t.barre,
        border: Border(right: BorderSide(color: t.filet)),
      ),
      padding: const EdgeInsets.symmetric(vertical: 7),
      child: Column(
        children: [
          _RailItem(
            icone: NovaIcones.accueil,
            infobulle: 'Accueil',
            actif: vue == NovaVue.accueil,
            onTap: () => naviguerVersVue(context, NovaRoutes.accueil),
          ),
          _RailItem(
            icone: NovaIcones.carnet,
            infobulle: 'Carnet',
            actif: vue == NovaVue.carnet,
            onTap: () => naviguerVersVue(context, NovaRoutes.carnet),
          ),
          _RailItem(
            icone: NovaIcones.enregistrements,
            infobulle: 'Enregistrements',
            actif: vue == NovaVue.enregistrements,
            onTap: () => naviguerVersVue(context, NovaRoutes.enregistrements),
          ),
          _RailItem(
            icone: NovaIcones.cadenas,
            infobulle: 'Accès non surveillé',
            actif: vue == NovaVue.nonSurveille,
            onTap: () => naviguerVersVue(context, NovaRoutes.nonSurveille),
          ),
          const Spacer(),
          _RailItem(
            icone: NovaIcones.reglages,
            infobulle: 'Réglages',
            actif: vue == NovaVue.reglages,
            onTap: () => naviguerVersVue(context, NovaRoutes.reglages),
          ),
        ],
      ),
    );
  }
}

class _RailItem extends StatefulWidget {
  const _RailItem({
    required this.icone,
    required this.infobulle,
    required this.actif,
    required this.onTap,
  });

  final IconData icone;
  final String infobulle;
  final bool actif;
  final VoidCallback onTap;

  @override
  State<_RailItem> createState() => _RailItemState();
}

class _RailItemState extends State<_RailItem> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleur = widget.actif
        ? kNovaRouge
        : (_survole ? t.texte : t.texte3);
    return Tooltip(
      message: widget.infobulle,
      preferBelow: false,
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        onEnter: (_) => setState(() => _survole = true),
        onExit: (_) => setState(() => _survole = false),
        child: GestureDetector(
          onTap: widget.onTap,
          behavior: HitTestBehavior.opaque,
          child: SizedBox(
            width: 50,
            height: 40,
            child: Stack(
              alignment: Alignment.center,
              children: [
                if (widget.actif)
                  Positioned(
                    left: 0,
                    top: 8,
                    bottom: 8,
                    child: Container(
                      width: 3,
                      decoration: const BoxDecoration(
                        color: kNovaRouge,
                        borderRadius:
                            BorderRadius.horizontal(right: Radius.circular(2)),
                      ),
                    ),
                  ),
                Container(
                  width: 36,
                  height: 36,
                  alignment: Alignment.center,
                  decoration: BoxDecoration(
                    color: widget.actif
                        ? t.fenetre
                        : (_survole ? t.survol : Colors.transparent),
                    borderRadius: BorderRadius.circular(kNovaRayon),
                  ),
                  child: NovaIcone(widget.icone, taille: 19, couleur: couleur),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Logo (pastille de marque : disque rouge + arc blanc ouvert)
// ---------------------------------------------------------------------------

class NovaLogo extends StatelessWidget {
  const NovaLogo({super.key, this.taille = 17});

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
    final rayon = size.width / 2 - size.width * (4.5 / 17) + 1;
    final trait = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = size.width * (2 / 17)
      ..strokeCap = StrokeCap.round
      ..color = Colors.white;
    const depart = (42 - 45) * 3.14159265 / 180;
    canvas.drawArc(
      Rect.fromCircle(center: centre, radius: rayon),
      depart,
      3.14159265 * 1.5,
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
      height: 26,
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
            decoration: BoxDecoration(color: t.vert, shape: BoxShape.circle),
          ),
          const SizedBox(width: 6),
          if (gauche != null)
            Expanded(child: gauche!)
          else
            Expanded(
              child: Row(
                children: [
                  Flexible(
                    child: Text('Prêt · connecté au rendez-vous',
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(fontSize: 11, color: t.texte2)),
                  ),
                  const SizedBox(width: 8),
                  Text('·', style: styleDiscret),
                  const SizedBox(width: 8),
                  Flexible(
                    child: Text('Chiffrement de bout en bout',
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: styleDiscret),
                  ),
                ],
              ),
            ),
          const SizedBox(width: 8),
          Text('NovaDesk $version', style: styleDiscret),
        ],
      ),
    );
  }
}
