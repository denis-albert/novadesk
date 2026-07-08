/// Noms de routes nommées de NovaDesk (navigation via [Navigator], sans
/// go_router). Centralisés ici pour que le rail de navigation, la table des
/// routes ([main]) et les écrans partagent les mêmes constantes.
library;

abstract final class NovaRoutes {
  /// Accueil (première route, base de la pile).
  static const String accueil = '/';

  /// Carnet d'adresses.
  static const String carnet = '/carnet';

  /// Lecteur d'enregistrements.
  static const String enregistrements = '/enregistrements';

  /// Accès non surveillé (hôte permanent).
  static const String nonSurveille = '/acces-non-surveille';

  /// Réglages.
  static const String reglages = '/parametres';

  /// Fenêtre de session (nécessite des `SessionScreenArgs`).
  static const String session = '/session';
}
