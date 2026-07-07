/// Bouton d'action principal NovaDesk (rendu plat, densité AnyDesk).
library;

import 'package:flutter/material.dart';

import 'nova_icons.dart';

/// `FilledButton` avec icône optionnelle (jeu [NovaIcones]) et état
/// « en cours » (indicateur circulaire + désactivation le temps de
/// l'opération). Neutre par défaut — le rouge de marque reste réservé au
/// bouton « Se connecter » de l'accueil.
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
  final NovaIconeData? icone;

  /// Si vrai : bouton désactivé + indicateur de progression à la place
  /// de l'icône.
  final bool enCours;

  @override
  Widget build(BuildContext context) {
    final action = enCours ? null : onPressed;
    // Couleur du contenu : celle du thème du bouton (fond neutre inversé).
    final couleurContenu = Theme.of(context).colorScheme.surface;

    if (icone == null && !enCours) {
      return FilledButton(onPressed: action, child: Text(libelle));
    }
    return FilledButton(
      onPressed: action,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (enCours)
            SizedBox(
              width: 14,
              height: 14,
              child: CircularProgressIndicator(
                strokeWidth: 2,
                color: couleurContenu,
              ),
            )
          else
            NovaIcone(icone!, taille: 15, couleur: couleurContenu),
          const SizedBox(width: 8),
          Text(libelle),
        ],
      ),
    );
  }
}
