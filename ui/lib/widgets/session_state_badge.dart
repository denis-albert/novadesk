/// Badge d'état de session : point coloré + libellé français stable, calqué
/// sur le motif `.status .g` de la maquette (pastille 7 px + texte discret).
///
/// Couleurs issues des jetons de thème — vert « accès » (active), ambre
/// (établissement / reconnexion), rouge de marque (fermée), gris (inactive) —
/// jamais de palette Material brute.
library;

import 'package:flutter/material.dart';

import '../bridge/native_api.dart';
import '../theme/nova_theme.dart';

/// Pastille + libellé (« inactive », « connexion », « active »…),
/// couleur selon l'état.
class SessionStateBadge extends StatelessWidget {
  const SessionStateBadge({super.key, required this.etat, this.dense = false});

  /// État courant de la session.
  final SessionStateDto etat;

  /// Variante compacte (barre d'état de la fenêtre de session).
  final bool dense;

  Color _couleur(NovaTokens t) => switch (etat) {
        SessionStateDto.idle => t.texte3,
        SessionStateDto.resolving ||
        SessionStateDto.connecting ||
        SessionStateDto.handshaking ||
        SessionStateDto.reconnecting =>
          kNovaAmbre,
        SessionStateDto.active => t.vert,
        SessionStateDto.closed => kNovaRouge,
      };

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleur = _couleur(t);
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Container(
          width: 7,
          height: 7,
          decoration: BoxDecoration(color: couleur, shape: BoxShape.circle),
        ),
        const SizedBox(width: 6),
        Flexible(
          child: Text(
            etat.label,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(
              color: dense ? t.texte2 : t.texte,
              fontWeight: FontWeight.w500,
              fontSize: dense ? 11 : 12.5,
            ),
          ),
        ),
      ],
    );
  }
}
