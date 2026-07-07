/// Vignette « speed-dial » d'une session récente (maquette `.tile`) :
/// aperçu de bureau désaturé réaliste (fenêtre esquissée + barre des tâches),
/// pastille de présence, alias + ID, menu contextuel
/// (Connecter / Favori / Renommer / Supprimer).
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../state/providers.dart';
import '../theme/nova_theme.dart';
import 'nova_icons.dart';

/// Action choisie dans le menu `⋯` d'une vignette.
enum ActionVignette { connecter, favori, renommer, supprimer }

class SessionThumbnail extends ConsumerStatefulWidget {
  const SessionThumbnail({
    super.key,
    required this.entree,
    required this.onConnecter,
    required this.onAction,
  });

  final EntreeCarnet entree;
  final VoidCallback onConnecter;
  final ValueChanged<ActionVignette> onAction;

  @override
  ConsumerState<SessionThumbnail> createState() => _SessionThumbnailState();
}

class _SessionThumbnailState extends ConsumerState<SessionThumbnail> {
  bool _survole = false;

  /// Fond désaturé de l'aperçu, stable par ID (pas de dégradé arc-en-ciel).
  Color _fondApercu(NovaTokens t) {
    return switch (widget.entree.id % 3) {
      0 => t.vignette1,
      1 => t.vignette2,
      _ => t.vignette3,
    };
  }

  Future<void> _ouvrirMenu(Offset position) async {
    final ecran = Overlay.of(context).context.findRenderObject()! as RenderBox;
    final action = await showMenu<ActionVignette>(
      context: context,
      position: RelativeRect.fromRect(
        Rect.fromLTWH(position.dx, position.dy, 1, 1),
        Offset.zero & ecran.size,
      ),
      items: [
        const PopupMenuItem(
          value: ActionVignette.connecter,
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.flecheDroite, taille: 14),
            SizedBox(width: 8),
            Text('Connecter'),
          ]),
        ),
        PopupMenuItem(
          value: ActionVignette.favori,
          height: 34,
          child: Row(children: [
            NovaIcone(
              widget.entree.favori
                  ? NovaIcones.etoilePleine
                  : NovaIcones.etoile,
              taille: 14,
            ),
            const SizedBox(width: 8),
            Text(widget.entree.favori
                ? 'Retirer des favoris'
                : 'Ajouter aux favoris'),
          ]),
        ),
        const PopupMenuItem(
          value: ActionVignette.renommer,
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.crayon, taille: 14),
            SizedBox(width: 8),
            Text('Renommer'),
          ]),
        ),
        const PopupMenuItem(
          value: ActionVignette.supprimer,
          height: 34,
          child: Row(children: [
            NovaIcone(NovaIcones.corbeille, taille: 14),
            SizedBox(width: 8),
            Text('Supprimer'),
          ]),
        ),
      ],
    );
    if (action != null) widget.onAction(action);
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final idFormate = ref.watch(idFormateProvider(widget.entree.id));

    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onConnecter,
        onSecondaryTapDown: (d) => _ouvrirMenu(d.globalPosition),
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 120),
          clipBehavior: Clip.antiAlias,
          decoration: BoxDecoration(
            color: t.fenetre,
            borderRadius: BorderRadius.circular(9),
            border: Border.all(color: _survole ? t.filetFort : t.filet),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Expanded(child: _apercu(t)),
              _meta(t, idFormate),
            ],
          ),
        ),
      ),
    );
  }

  /// Aperçu de bureau : fenêtre translucide esquissée + barre des tâches
  /// (bande sombre basse + pastille « démarrer ») + pastille de présence.
  Widget _apercu(NovaTokens t) {
    return Container(
      color: _fondApercu(t),
      child: LayoutBuilder(
        builder: (context, contraintes) {
          final l = contraintes.maxWidth;
          final h = contraintes.maxHeight;
          return Stack(
            children: [
              // Fenêtre au centre (comme `.thumb::before`).
              Positioned(
                left: l * 0.22,
                top: h * 0.20,
                right: l * 0.22,
                bottom: h * 0.34,
                child: Container(
                  decoration: BoxDecoration(
                    color: Colors.white.withValues(alpha: 0.10),
                    border: Border.all(
                        color: Colors.white.withValues(alpha: 0.14)),
                    borderRadius: BorderRadius.circular(3),
                  ),
                ),
              ),
              // Barre des tâches (comme `.thumb::after`).
              Positioned(
                left: 0,
                right: 0,
                bottom: 0,
                height: 14,
                child: ColoredBox(
                    color: Colors.black.withValues(alpha: 0.28)),
              ),
              Positioned(
                left: 6,
                bottom: 3,
                child: Container(
                  width: 8,
                  height: 8,
                  decoration: BoxDecoration(
                    color: Colors.white.withValues(alpha: 0.55),
                    borderRadius: BorderRadius.circular(2),
                  ),
                ),
              ),
              // Pastille de présence, cerclée de la couleur du panneau.
              Positioned(
                top: 6,
                right: 6,
                child: Container(
                  width: 13,
                  height: 13,
                  decoration: BoxDecoration(
                    color: widget.entree.enLigne ? kNovaVert : t.texte3,
                    shape: BoxShape.circle,
                    border: Border.all(color: t.fenetre, width: 2),
                  ),
                ),
              ),
              // Menu `⋯`, révélé au survol.
              if (_survole)
                Positioned(
                  top: 4,
                  left: 4,
                  child: Material(
                    color: Colors.black.withValues(alpha: 0.35),
                    borderRadius: BorderRadius.circular(6),
                    child: InkWell(
                      borderRadius: BorderRadius.circular(6),
                      onTapDown: (d) => _ouvrirMenu(d.globalPosition),
                      onTap: () {},
                      child: const Padding(
                        padding: EdgeInsets.all(4),
                        child: NovaIcone(NovaIcones.troisPoints,
                            taille: 14, couleur: Colors.white),
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

  Widget _meta(NovaTokens t, AsyncValue<String> idFormate) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(11, 8, 11, 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            widget.entree.alias,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(
              fontSize: 12.5,
              fontWeight: FontWeight.w600,
              color: t.texte,
            ),
          ),
          const SizedBox(height: 1),
          Text(
            idFormate.maybeWhen(data: (id) => id, orElse: () => '…'),
            style: TextStyle(
              fontSize: 11.5,
              color: t.texte3,
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
        ],
      ),
    );
  }
}
