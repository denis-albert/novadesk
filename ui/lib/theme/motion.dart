/// Mouvement de NovaDesk — durées, courbes et amplitudes des transitions de
/// navigation, **centralisées** ici pour un rendu homogène (rail, session,
/// dialogues). Le parti pris est sobre et rapide : fondus courts, glissements
/// discrets, aucun rebond ni effet gadget.
///
/// Le réglage système « animations réduites »
/// ([MediaQueryData.disableAnimations]) est respecté partout — voir
/// [NovaMotion.animationsReduites] et [montrerDialogueNova].
library;

import 'package:flutter/material.dart';

/// Constantes de mouvement partagées par toute l'application.
abstract final class NovaMotion {
  // --- Sections du rail (coquille persistante) -----------------------------

  /// Fondu + léger glissement entre les cinq sections principales.
  static const Duration sections = Duration(milliseconds: 160);

  /// Courbe d'entrée d'une section (sobre, sans dépassement).
  static const Curve sectionsCourbe = Curves.easeOut;

  /// Amplitude du glissement vertical d'entrée d'une section, en pixels.
  static const double sectionsDecalage = 8;

  // --- Fenêtre de session --------------------------------------------------

  /// Ouverture / fermeture de la session : fondu + léger zoom.
  static const Duration session = Duration(milliseconds: 200);

  /// Courbe d'ouverture / fermeture de la session.
  static const Curve sessionCourbe = Curves.easeOutCubic;

  /// Échelle de départ du zoom d'ouverture de la session (0.98 → 1).
  static const double sessionZoomInitial = 0.98;

  // --- Dialogues / modales -------------------------------------------------

  /// Apparition d'un dialogue : fondu + léger scale.
  static const Duration dialogue = Duration(milliseconds: 160);

  /// Courbe d'apparition d'un dialogue.
  static const Curve dialogueCourbe = Curves.easeOutCubic;

  /// Échelle de départ d'un dialogue (0.98 → 1).
  static const double dialogueEchelleInitiale = 0.98;

  /// Vrai si l'utilisateur a demandé des animations réduites (accessibilité) :
  /// les transitions sont alors supprimées (apparition nette et instantanée).
  static bool animationsReduites(BuildContext context) =>
      MediaQuery.maybeOf(context)?.disableAnimations ?? false;
}

/// Ouvre une modale avec la transition NovaDesk (fondu + léger scale 0.98 → 1),
/// cohérente avec l'ouverture de la session. Remplace [showDialog] pour
/// homogénéiser toutes les modales ; conserve la même signature utile
/// (`context` + `builder`) et le même comportement de barrière.
///
/// Le réglage « animations réduites » supprime la transition.
Future<T?> montrerDialogueNova<T>({
  required BuildContext context,
  required WidgetBuilder builder,
  bool barrierDismissible = true,
}) {
  final reduites = NovaMotion.animationsReduites(context);
  return showGeneralDialog<T>(
    context: context,
    barrierDismissible: barrierDismissible,
    barrierLabel: MaterialLocalizations.of(context).modalBarrierDismissLabel,
    barrierColor: Colors.black54,
    transitionDuration: reduites ? Duration.zero : NovaMotion.dialogue,
    pageBuilder: (context, animation, secondaryAnimation) =>
        SafeArea(child: Builder(builder: builder)),
    transitionBuilder: (context, animation, secondaryAnimation, child) {
      if (reduites) return child;
      final courbe =
          CurvedAnimation(parent: animation, curve: NovaMotion.dialogueCourbe);
      return FadeTransition(
        opacity: courbe,
        child: ScaleTransition(
          scale: Tween<double>(begin: NovaMotion.dialogueEchelleInitiale, end: 1)
              .animate(courbe),
          child: child,
        ),
      );
    },
  );
}
