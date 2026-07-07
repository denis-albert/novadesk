/// Champ de saisie d'un ID NovaDesk, formaté en direct par groupes de 3
/// chiffres (`936 271 048`) — même rendu que `format_nova_id` côté Rust.
///
/// La valeur du contrôleur contient donc des espaces ; `parse_nova_id`
/// (façade `nd-ffi`) les tolère explicitement, aucune dé-normalisation
/// n'est nécessaire avant l'appel.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'nova_icons.dart';

/// Formateur : ne garde que les chiffres (9 au plus) et insère un espace
/// tous les 3 chiffres, en préservant la position du curseur.
class NovaIdInputFormatter extends TextInputFormatter {
  const NovaIdInputFormatter();

  /// Les ID NovaDesk font 9 chiffres (plan 01 / `nd-proto::NovaId`).
  static const int maxChiffres = 9;

  @override
  TextEditingValue formatEditUpdate(
    TextEditingValue oldValue,
    TextEditingValue newValue,
  ) {
    final estChiffre = RegExp(r'\d');

    // 1. Chiffres seuls, tronqués à la longueur maximale.
    var chiffres = newValue.text.replaceAll(RegExp(r'\D'), '');
    if (chiffres.length > maxChiffres) {
      chiffres = chiffres.substring(0, maxChiffres);
    }

    // 2. Regroupement par 3 depuis la gauche (saisie progressive).
    final tampon = StringBuffer();
    for (var i = 0; i < chiffres.length; i++) {
      if (i > 0 && i % 3 == 0) {
        tampon.write(' ');
      }
      tampon.write(chiffres[i]);
    }
    final formate = tampon.toString();

    // 3. Curseur : replacé après le même nombre de chiffres qu'avant
    //    formatage (en comptant les séparateurs insérés).
    var chiffresAvantCurseur = 0;
    final curseur = newValue.selection.baseOffset;
    for (var i = 0; i < curseur && i < newValue.text.length; i++) {
      if (estChiffre.hasMatch(newValue.text[i])) {
        chiffresAvantCurseur++;
      }
    }
    if (chiffresAvantCurseur > maxChiffres) {
      chiffresAvantCurseur = maxChiffres;
    }
    var position = chiffresAvantCurseur;
    if (chiffresAvantCurseur > 0) {
      position += (chiffresAvantCurseur - 1) ~/ 3; // espaces insérés avant
    }
    if (position > formate.length) {
      position = formate.length;
    }

    return TextEditingValue(
      text: formate,
      selection: TextSelection.collapsed(offset: position),
    );
  }
}

/// Champ texte prêt à l'emploi pour saisir un ID distant.
class NovaIdField extends StatelessWidget {
  const NovaIdField({
    super.key,
    required this.controller,
    this.libelle = 'ID NovaDesk',
    this.indication = 'p. ex. 936 271 048',
    this.onSubmitted,
    this.autofocus = false,
    this.enabled = true,
  });

  final TextEditingController controller;

  /// Libellé flottant du champ.
  final String libelle;

  /// Texte d'indication (placeholder).
  final String indication;

  /// Appelé sur Entrée (permet « saisir puis Entrée = se connecter »).
  final ValueChanged<String>? onSubmitted;

  final bool autofocus;
  final bool enabled;

  @override
  Widget build(BuildContext context) {
    return TextField(
      controller: controller,
      enabled: enabled,
      autofocus: autofocus,
      keyboardType: TextInputType.number,
      inputFormatters: const [NovaIdInputFormatter()],
      style: const TextStyle(
        fontFeatures: [FontFeature.tabularFigures()],
        letterSpacing: 1.2,
      ),
      decoration: InputDecoration(
        labelText: libelle,
        hintText: indication,
        prefixIcon: const Padding(
          padding: EdgeInsets.symmetric(horizontal: 11),
          child: NovaIcone(NovaIcones.moniteur, taille: 16),
        ),
        prefixIconConstraints:
            const BoxConstraints(minWidth: 38, minHeight: 38),
      ),
      onSubmitted: onSubmitted,
    );
  }
}
