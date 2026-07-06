/// Bouton d'action principal NovaDesk (Material 3).
library;

import 'package:flutter/material.dart';

/// `FilledButton` avec icône optionnelle et état « en cours »
/// (indicateur circulaire + désactivation le temps de l'opération).
class NovaButton extends StatelessWidget {
  const NovaButton({
    super.key,
    required this.libelle,
    this.onPressed,
    this.icone,
    this.enCours = false,
  });

  /// Texte du bouton (français).
  final String libelle;

  /// Action ; `null` désactive le bouton.
  final VoidCallback? onPressed;

  /// Icône optionnelle, affichée à gauche du libellé.
  final IconData? icone;

  /// Si vrai : bouton désactivé + indicateur de progression à la place
  /// de l'icône.
  final bool enCours;

  @override
  Widget build(BuildContext context) {
    final action = enCours ? null : onPressed;
    final Widget? icon = enCours
        ? const SizedBox(
            width: 16,
            height: 16,
            child: CircularProgressIndicator(strokeWidth: 2),
          )
        : (icone != null ? Icon(icone) : null);

    if (icon == null) {
      return FilledButton(onPressed: action, child: Text(libelle));
    }
    return FilledButton.icon(
      onPressed: action,
      icon: icon,
      label: Text(libelle),
    );
  }
}
