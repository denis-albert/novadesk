/// Boîte à composants NovaDesk — briques réutilisables fidèles à la maquette
/// (`novadesk-app.html`) : étiquette de section, en-tête de panneau, boutons
/// (primaire rouge / fantôme / danger / action carrée), interrupteur vert,
/// segmenté rouge, étiquettes bleues, pastille d'état, squelette shimmer, état
/// vide, toasts et menu contextuel.
///
/// Tout est piloté par le thème ([NovaTokens]) — aucune couleur codée en dur
/// (hors surfaces intrinsèquement sombres ou blanc/noir de contraste).
library;

import 'dart:async';

import 'package:flutter/material.dart';

import '../theme/nova_theme.dart';
import 'nova_icons.dart';

// ===========================================================================
// Étiquettes et en-têtes
// ===========================================================================

/// Étiquette de section en capitales espacées (maquette `.lbl`).
class NovaSectionLabel extends StatelessWidget {
  const NovaSectionLabel(this.texte, {super.key, this.padding});

  final String texte;
  final EdgeInsetsGeometry? padding;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final child = Text(
      texte.toUpperCase(),
      style: TextStyle(
        fontSize: 10.5,
        fontWeight: FontWeight.w700,
        letterSpacing: 0.6,
        color: t.texte3,
      ),
    );
    return padding == null ? child : Padding(padding: padding!, child: child);
  }
}

/// En-tête de colonne/panneau : icône rouge 16 px + libellé 13 px (maquette `.h`).
class NovaPanelHeader extends StatelessWidget {
  const NovaPanelHeader(this.icone, this.titre, {super.key});

  final IconData icone;
  final String titre;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Row(
      children: [
        NovaIcone(icone, taille: 16, couleur: kNovaRouge),
        const SizedBox(width: 8),
        Text(
          titre,
          style: TextStyle(
              fontSize: 13, fontWeight: FontWeight.w600, color: t.texte),
        ),
      ],
    );
  }
}

// ===========================================================================
// Boutons
// ===========================================================================

/// Bouton primaire rouge (maquette `.btn.pri` / `.go`) — usage réservé.
class NovaBoutonPrimaire extends StatefulWidget {
  const NovaBoutonPrimaire({
    super.key,
    required this.libelle,
    this.icone,
    this.onPressed,
    this.hauteur = 32,
    this.enCours = false,
  });

  final String libelle;
  final IconData? icone;
  final VoidCallback? onPressed;
  final double hauteur;
  final bool enCours;

  @override
  State<NovaBoutonPrimaire> createState() => _NovaBoutonPrimaireState();
}

class _NovaBoutonPrimaireState extends State<NovaBoutonPrimaire> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final desactive = widget.onPressed == null && !widget.enCours;
    final fond = desactive
        ? t.champBordure
        : (_survole ? kNovaRougePresse : kNovaRouge);
    return MouseRegion(
      cursor: desactive ? MouseCursor.defer : SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.enCours ? null : widget.onPressed,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 120),
          height: widget.hauteur,
          padding: const EdgeInsets.symmetric(horizontal: 16),
          decoration: BoxDecoration(
            color: fond,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                widget.libelle,
                style: const TextStyle(
                  fontSize: 13,
                  fontWeight: FontWeight.w600,
                  color: Colors.white,
                ),
              ),
              if (widget.enCours) ...[
                const SizedBox(width: 8),
                const SizedBox(
                  width: 15,
                  height: 15,
                  child: CircularProgressIndicator(
                      strokeWidth: 2, color: Colors.white),
                ),
              ] else if (widget.icone != null) ...[
                const SizedBox(width: 8),
                NovaIcone(widget.icone!, taille: 15, couleur: Colors.white),
              ],
            ],
          ),
        ),
      ),
    );
  }
}

/// Bouton fantôme / secondaire (maquette `.btn`) : fond fenêtre, bordure filet,
/// texte primaire, icône optionnelle.
class NovaBoutonSecondaire extends StatefulWidget {
  const NovaBoutonSecondaire({
    super.key,
    required this.libelle,
    this.icone,
    this.onPressed,
    this.hauteur = 32,
    this.danger = false,
  });

  final String libelle;
  final IconData? icone;
  final VoidCallback? onPressed;
  final double hauteur;
  final bool danger;

  @override
  State<NovaBoutonSecondaire> createState() => _NovaBoutonSecondaireState();
}

class _NovaBoutonSecondaireState extends State<NovaBoutonSecondaire> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleur = widget.danger ? kNovaRouge : t.texte;
    return MouseRegion(
      cursor: widget.onPressed == null
          ? MouseCursor.defer
          : SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onPressed,
        child: Container(
          height: widget.hauteur,
          padding: const EdgeInsets.symmetric(horizontal: 12),
          decoration: BoxDecoration(
            color: _survole ? t.survol : t.fenetre,
            border: Border.all(color: t.filetFort),
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              if (widget.icone != null) ...[
                NovaIcone(widget.icone!, taille: 14, couleur: couleur),
                const SizedBox(width: 7),
              ],
              Text(
                widget.libelle,
                style: TextStyle(
                    fontSize: 12.5, fontWeight: FontWeight.w500, color: couleur),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Bouton d'action carré révélé au survol d'une ligne (maquette `.ra`).
/// [accent] passe l'icône au rouge au survol (flèche « se connecter »).
class NovaBoutonAction extends StatefulWidget {
  const NovaBoutonAction({
    super.key,
    required this.icone,
    this.onTap,
    this.infobulle,
    this.accent = false,
    this.taille = 28,
    this.tailleIcone = 15,
    this.couleurActive,
  });

  final IconData icone;
  final VoidCallback? onTap;
  final String? infobulle;
  final bool accent;
  final double taille;
  final double tailleIcone;

  /// Couleur permanente de l'icône (ex. ambre pour un favori actif). Prime sur
  /// la logique de survol quand elle est fournie.
  final Color? couleurActive;

  @override
  State<NovaBoutonAction> createState() => _NovaBoutonActionState();
}

class _NovaBoutonActionState extends State<NovaBoutonAction> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleur = widget.couleurActive ??
        (_survole ? (widget.accent ? kNovaRouge : t.texte) : t.texte2);
    Widget bouton = MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        behavior: HitTestBehavior.opaque,
        child: Container(
          width: widget.taille,
          height: widget.taille,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: _survole ? t.survol : Colors.transparent,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: NovaIcone(widget.icone, taille: widget.tailleIcone, couleur: couleur),
        ),
      ),
    );
    if (widget.infobulle != null) {
      bouton = Tooltip(message: widget.infobulle!, child: bouton);
    }
    return bouton;
  }
}

// ===========================================================================
// Interrupteur (maquette `.sw`)
// ===========================================================================

/// Interrupteur compact 36×20, vert quand actif (maquette `.sw.on`).
class NovaSwitch extends StatelessWidget {
  const NovaSwitch({super.key, required this.actif, this.onChanged, this.label});

  final bool actif;
  final ValueChanged<bool>? onChanged;
  final String? label;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final desactive = onChanged == null;
    return Semantics(
      toggled: actif,
      button: true,
      label: label,
      child: MouseRegion(
        cursor: desactive ? MouseCursor.defer : SystemMouseCursors.click,
        child: GestureDetector(
          onTap: desactive ? null : () => onChanged!(!actif),
          child: AnimatedContainer(
            duration: const Duration(milliseconds: 150),
            width: 36,
            height: 20,
            padding: const EdgeInsets.all(2),
            alignment: actif ? Alignment.centerRight : Alignment.centerLeft,
            decoration: BoxDecoration(
              color: actif
                  ? (desactive ? t.vert.withValues(alpha: 0.5) : t.vert)
                  : t.filetFort,
              borderRadius: BorderRadius.circular(10),
            ),
            child: Container(
              width: 16,
              height: 16,
              decoration: BoxDecoration(
                color: Colors.white,
                shape: BoxShape.circle,
                boxShadow: [
                  BoxShadow(
                    color: Colors.black.withValues(alpha: 0.3),
                    blurRadius: 2,
                    offset: const Offset(0, 1),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}

// ===========================================================================
// Segmenté (maquette `.segb`)
// ===========================================================================

/// Contrôle segmenté compact, segment actif rouge (maquette `.segb span.on`).
class NovaSegmented<T> extends StatelessWidget {
  const NovaSegmented({
    super.key,
    required this.valeurs,
    required this.selection,
    required this.onChanged,
  });

  /// Ordre des segments : (valeur, libellé).
  final List<(T, String)> valeurs;
  final T selection;
  final ValueChanged<T> onChanged;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      decoration: BoxDecoration(
        border: Border.all(color: t.filetFort),
        borderRadius: BorderRadius.circular(kNovaRayon),
      ),
      clipBehavior: Clip.antiAlias,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (var i = 0; i < valeurs.length; i++)
            _segment(context, t, valeurs[i].$1, valeurs[i].$2,
                dernier: i == valeurs.length - 1),
        ],
      ),
    );
  }

  Widget _segment(BuildContext context, NovaTokens t, T valeur, String libelle,
      {required bool dernier}) {
    final actif = valeur == selection;
    return GestureDetector(
      onTap: () => onChanged(valeur),
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 13, vertical: 6),
          decoration: BoxDecoration(
            color: actif ? kNovaRouge : Colors.transparent,
            border: dernier
                ? null
                : Border(right: BorderSide(color: t.filetFort)),
          ),
          child: Text(
            libelle,
            style: TextStyle(
              fontSize: 12,
              color: actif ? Colors.white : t.texte2,
            ),
          ),
        ),
      ),
    );
  }
}

// ===========================================================================
// Étiquette (maquette `.tag`) et pastille d'état (maquette `.st2`)
// ===========================================================================

/// Étiquette bleue compacte (maquette `.tag`).
class NovaTag extends StatelessWidget {
  const NovaTag(this.texte, {super.key});

  final String texte;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 7, vertical: 2),
      decoration: BoxDecoration(
        color: t.selection,
        borderRadius: BorderRadius.circular(3),
      ),
      child: Text(
        texte,
        style: TextStyle(fontSize: 10.5, color: t.bleu),
      ),
    );
  }
}

/// Pastille d'état « En ligne / Hors ligne » (maquette `.st2`).
class NovaStatePill extends StatelessWidget {
  const NovaStatePill({super.key, required this.enLigne});

  final bool enLigne;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Container(
          width: 8,
          height: 8,
          decoration: BoxDecoration(
            color: enLigne ? t.vert : t.texte3,
            shape: BoxShape.circle,
          ),
        ),
        const SizedBox(width: 6),
        Text(
          enLigne ? 'En ligne' : 'Hors ligne',
          style: TextStyle(
              fontSize: 11.5, color: enLigne ? t.texte2 : t.texte3),
        ),
      ],
    );
  }
}

// ===========================================================================
// Squelette shimmer (maquette `.sk`)
// ===========================================================================

/// Bloc « squelette » animé (dégradé qui défile) pour l'état de chargement.
class NovaSkeleton extends StatefulWidget {
  const NovaSkeleton({
    super.key,
    required this.largeur,
    required this.hauteur,
    this.rayon = 3,
  });

  final double largeur;
  final double hauteur;
  final double rayon;

  @override
  State<NovaSkeleton> createState() => _NovaSkeletonState();
}

class _NovaSkeletonState extends State<NovaSkeleton>
    with SingleTickerProviderStateMixin {
  late final AnimationController _controller = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 1300),
  )..repeat();

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return SizedBox(
      width: widget.largeur,
      height: widget.hauteur,
      child: AnimatedBuilder(
        animation: _controller,
        builder: (context, _) {
          final dx = (_controller.value * 2 - 1) * 2;
          return DecoratedBox(
            decoration: BoxDecoration(
              borderRadius: BorderRadius.circular(widget.rayon),
              gradient: LinearGradient(
                begin: Alignment(-1 - dx, 0),
                end: Alignment(1 - dx, 0),
                colors: [t.panneau2, t.survol, t.panneau2],
                stops: const [0.25, 0.5, 0.75],
              ),
            ),
          );
        },
      ),
    );
  }
}

// ===========================================================================
// État vide (maquette `.empty`)
// ===========================================================================

/// État vide centré : icône discrète + titre + sous-titre (maquette `.empty`).
class NovaEmptyState extends StatelessWidget {
  const NovaEmptyState({
    super.key,
    required this.icone,
    required this.titre,
    required this.sousTitre,
  });

  final IconData icone;
  final String titre;
  final String sousTitre;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(44),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            NovaIcone(icone, taille: 32, couleur: t.texte3.withValues(alpha: 0.5)),
            const SizedBox(height: 10),
            Text(
              titre,
              style: TextStyle(
                  fontSize: 13.5, fontWeight: FontWeight.w600, color: t.texte2),
            ),
            const SizedBox(height: 5),
            Text(
              sousTitre,
              textAlign: TextAlign.center,
              style: TextStyle(fontSize: 12, height: 1.5, color: t.texte3),
            ),
          ],
        ),
      ),
    );
  }
}

// ===========================================================================
// Toasts (maquette `.toasts` / `.toast`)
// ===========================================================================

/// Toast NovaDesk : notification bas-droite auto-résorbée (~3 s), filet gauche
/// vert (succès) ou bleu (info).
class NovaToast {
  NovaToast._();

  static final List<_ToastItem> _actifs = [];
  static OverlayEntry? _hote;

  /// Affiche un toast au-dessus de l'[Overlay] courant.
  static void montrer(BuildContext context, String message,
      {bool info = false}) {
    final overlay = Overlay.of(context, rootOverlay: true);
    final item = _ToastItem(message: message, info: info);
    _actifs.add(item);
    if (_hote == null) {
      _hote = OverlayEntry(builder: _construireHote);
      overlay.insert(_hote!);
    } else {
      _hote!.markNeedsBuild();
    }
    Timer(const Duration(seconds: 3), () {
      _actifs.remove(item);
      if (_actifs.isEmpty) {
        _hote?.remove();
        _hote = null;
      } else {
        _hote?.markNeedsBuild();
      }
    });
  }

  static Widget _construireHote(BuildContext context) {
    return Positioned(
      right: 14,
      bottom: 34,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.end,
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final item in _actifs)
            Padding(
              padding: const EdgeInsets.only(top: 8),
              child: _NovaToastCarte(item: item),
            ),
        ],
      ),
    );
  }
}

class _ToastItem {
  _ToastItem({required this.message, required this.info});
  final String message;
  final bool info;
}

class _NovaToastCarte extends StatelessWidget {
  const _NovaToastCarte({required this.item});

  final _ToastItem item;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final accent = item.info ? t.bleu : t.vert;
    return Material(
      color: Colors.transparent,
      child: Container(
        constraints: const BoxConstraints(minWidth: 210, maxWidth: 320),
        clipBehavior: Clip.antiAlias,
        decoration: BoxDecoration(
          color: t.fenetre,
          borderRadius: BorderRadius.circular(6),
          border: Border.all(color: t.filetFort),
          boxShadow: [
            BoxShadow(
              color: Colors.black.withValues(alpha: 0.18),
              blurRadius: 24,
              offset: const Offset(0, 8),
            ),
          ],
        ),
        child: IntrinsicHeight(
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            mainAxisSize: MainAxisSize.min,
            children: [
              // Filet gauche coloré (succès vert / info bleu).
              Container(width: 3, color: accent),
              Padding(
                padding: const EdgeInsets.fromLTRB(11, 9, 12, 9),
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    NovaIcone(item.info ? NovaIcones.info : NovaIcones.coche,
                        taille: 16, couleur: accent),
                    const SizedBox(width: 10),
                    ConstrainedBox(
                      constraints: const BoxConstraints(maxWidth: 250),
                      child: Text(
                        item.message,
                        style: TextStyle(fontSize: 12.5, color: t.texte),
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
// Menu contextuel (maquette `.ctx`)
// ===========================================================================

/// Entrée d'un menu contextuel NovaDesk.
class NovaMenuAction {
  const NovaMenuAction(
    this.cle,
    this.libelle,
    this.icone, {
    this.danger = false,
    this.separateurAvant = false,
  });

  final String cle;
  final String libelle;
  final IconData icone;
  final bool danger;

  /// Trait de séparation inséré au-dessus de cette entrée.
  final bool separateurAvant;
}

/// Ouvre un menu contextuel à la position écran [position] et renvoie la clé de
/// l'entrée choisie (`null` si écarté). Style fidèle à `.ctx` de la maquette.
Future<String?> showNovaContextMenu(
  BuildContext context,
  Offset position,
  List<NovaMenuAction> actions,
) {
  final t = NovaTokens.of(context);
  final overlay =
      Overlay.of(context, rootOverlay: true).context.findRenderObject()
          as RenderBox;
  final entries = <PopupMenuEntry<String>>[];
  for (final action in actions) {
    if (action.separateurAvant) {
      entries.add(const PopupMenuDivider(height: 9));
    }
    final couleur = action.danger ? kNovaRouge : t.texte;
    entries.add(
      PopupMenuItem<String>(
        value: action.cle,
        height: 34,
        child: Row(
          children: [
            NovaIcone(action.icone,
                taille: 15, couleur: action.danger ? kNovaRouge : t.texte2),
            const SizedBox(width: 10),
            Text(action.libelle,
                style: TextStyle(fontSize: 12.5, color: couleur)),
          ],
        ),
      ),
    );
  }
  return showMenu<String>(
    context: context,
    position: RelativeRect.fromRect(
      Rect.fromLTWH(position.dx, position.dy, 1, 1),
      Offset.zero & overlay.size,
    ),
    items: entries,
  );
}
