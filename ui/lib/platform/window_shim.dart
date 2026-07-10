/// Fenêtrage natif réel — ré-export du plugin `window_manager`.
///
/// Historiquement, ce fichier était une **façade no-op** sans dépendance : le
/// plugin natif `window_manager` (et son transitif `screen_retriever`) exige le
/// support des liens symboliques Windows au moment de la compilation, ce qui
/// était impossible sans droits administrateur. Depuis l'obtention des droits
/// admin (les liens symboliques fonctionnent, les plugins natifs se compilent),
/// la contrainte est levée : ce fichier **ré-exporte directement** le vrai
/// plugin.
///
/// Tous les appelants (`import '../platform/window_shim.dart';`) obtiennent donc
/// le `windowManager` réel — minimiser / agrandir / restaurer / fermer, plein
/// écran, déplacement de la fenêtre par la barre de titre custom — ainsi que
/// `WindowOptions`, `TitleBarStyle`, `WindowListener`, etc. Le point d'entrée
/// unique reste ce fichier pour garder un seul endroit à modifier si le plugin
/// change (ou pour réintroduire un repli).
library;

export 'package:window_manager/window_manager.dart';
