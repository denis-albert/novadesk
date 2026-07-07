/// Badge d'état de session : pastille colorée + libellé français stable,
/// directement issu du miroir de `SessionStateDto::label()` (façade `nd-ffi`).
library;

import 'package:flutter/material.dart';

import '../bridge/native_api.dart';

/// Pastille + libellé (« inactive », « connexion », « active »…),
/// couleur selon l'état.
class SessionStateBadge extends StatelessWidget {
  const SessionStateBadge({super.key, required this.etat, this.dense = false});

  /// État courant de la session.
  final SessionStateDto etat;

  /// Variante compacte (barre d'état de la fenêtre de session).
  final bool dense;

  Color _couleur(ColorScheme schema) => switch (etat) {
        SessionStateDto.idle => schema.outline,
        SessionStateDto.resolving ||
        SessionStateDto.connecting ||
        SessionStateDto.handshaking =>
          Colors.amber.shade800,
        SessionStateDto.active => Colors.green.shade600,
        SessionStateDto.reconnecting => Colors.orange.shade800,
        SessionStateDto.closed => schema.error,
      };

  @override
  Widget build(BuildContext context) {
    final couleur = _couleur(Theme.of(context).colorScheme);
    return Container(
      padding: EdgeInsets.symmetric(
        horizontal: dense ? 8 : 10,
        vertical: dense ? 3 : 5,
      ),
      decoration: BoxDecoration(
        color: couleur.withValues(alpha: 0.14),
        borderRadius: BorderRadius.circular(999),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: 8,
            height: 8,
            decoration: BoxDecoration(color: couleur, shape: BoxShape.circle),
          ),
          const SizedBox(width: 6),
          Flexible(
            child: Text(
              etat.label,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: couleur,
                fontWeight: FontWeight.w600,
                fontSize: dense ? 12 : 13,
              ),
            ),
          ),
        ],
      ),
    );
  }
}
