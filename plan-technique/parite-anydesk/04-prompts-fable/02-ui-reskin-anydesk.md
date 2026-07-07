# Prompt Fable 02 — Reskin visuel « clone AnyDesk » (UI Flutter)

**Priorité : P0** · **Crate ciblé : `ui/` (Flutter)** · **Parallélisable avec : 01, 05, 06, 07, 08** (crates Rust disjointes). **NON parallélisable avec 04** (même crate `ui/`) → lancer 02 **avant** 04.

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : rapprocher l'apparence de l'UI Flutter de celle d'AnyDesk (couleur de marque rouge, densité compacte, sidebar de navigation, vignettes « speed-dial », barre d'outils de session complète, dialogue d'acceptation entrante) **sans toucher au câblage du cœur** (fait au lot 04) et **sans réintroduire de plugin natif**.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\ui\`.
- Utilise Flutter via `C:\Users\udohkak\flutter\bin\flutter.bat` (hors PATH). Analyse : `C:\Users\udohkak\flutter\bin\flutter.bat analyze`.
- **NE réintroduis AUCUN plugin natif** (contrainte : pas de droits admin / pas de symlinks — `window_manager`, `bitsdojo_window`, `irondash_texture` sont **interdits**). Le fenêtrage passe par le shim pur-Dart `lib/platform/window_shim.dart` (no-op) — laisse-le tel quel.
- **AUCUN git.** Le pont FRB se régénère avec `flutter` dans le PATH (hors périmètre ici) — **ne régénère pas** `lib/bridge/generated/`.
- Ne modifie pas la logique métier des écrans (validation ID, providers) au-delà du strict habillage ; le câblage session live est le lot 04.

## BARRE QUALITÉ
- `C:\Users\udohkak\flutter\bin\flutter.bat analyze` → **aucune erreur** (warnings d'info tolérés mais à minimiser).
- Respecte `analysis_options.yaml` existant. Pas de `TODO` laissé sans commentaire.

## ÉTAT ACTUEL (à respecter)
- Thème unique dans `lib/main.dart:92-105` : `ColorScheme.fromSeed(seedColor: Color(0xFF4C5FD5))` (**indigo**), Material 3, `VisualDensity.adaptivePlatformDensity`. C'est la SEULE couleur de marque ; tout en découle.
- 4 écrans : `lib/screens/{home,session,settings,unattended}_screen.dart`. Widgets : `lib/widgets/{nova_button,nova_id_field,session_state_badge}.dart`. Navigation : `Navigator` + routes nommées (`onGenerateRoute` dans `main.dart`), **pas de go_router**.
- `home_screen.dart` : `AppBar` + 3 `Card` empilées (« Ce poste », « Se connecter », « Sessions récentes & carnet »), responsive (breakpoint 920px).
- `session_screen.dart` : `AppBar` barre d'outils (moniteur, qualité, plein écran, Ctrl+Alt+Suppr, chat, fichiers, fin), surface `#101014` avec placeholder, barre d'état basse.
- Providers Riverpod dans `lib/state/providers.dart` (ID local fictif, carnet fictif, themeMode).
- **Spécification visuelle détaillée** : voir `../03-interface-anydesk-exacte.md` (couleurs hex, dimensions, composants) — **suis-la**.

## TÂCHE
1. **Thème AnyDesk** (`main.dart` + éventuel nouveau `lib/theme/nova_theme.dart`) : remplacer le seed indigo par un `ColorScheme` **construit à la main** avec `primary = Color(0xFFEF443B)` (rouge AnyDesk) en **accent parcimonieux**, surfaces neutres (clair `#FFFFFF`/`#F5F5F7`, sombre `#1C1C1E`/`#262629`), `outline` séparateurs 1px, `visualDensity: VisualDensity(-1,-1)`, `cardTheme` elevation 0 rayon 8, `filledButtonTheme` rayon 4, `inputDecorationTheme` dense rayon 4. Clair **et** sombre. **Ne pas** tout teinter en rouge (voir §1.1 du doc 03). Utilise les valeurs de `../03-interface-anydesk-exacte.md` §7 comme point de départ.
2. **Sidebar de navigation** (nouveau `lib/widgets/nav_sidebar.dart`) : colonne gauche ~220px, items Accueil / Favoris / Récents / Carnet / Découverte + bas Réglages / Compte, item actif accentué. Intègre-la dans un layout desktop `Row[sidebar, contenu]` sur l'accueil (garde le repli responsive en colonne unique sous ~800px).
3. **Vignettes « speed-dial »** (nouveau `lib/widgets/session_thumbnail.dart`) : carte ~140×90, zone d'aperçu (placeholder image + icône bureau pour l'instant), alias, pastille en ligne, menu `⋯` (Connecter / Favori / Renommer / Supprimer). Sur l'accueil, remplace la liste `ListTile` par un `Wrap`/`GridView` de vignettes, avec des **onglets** Récents / Favoris / Découverte (`TabBar`/`SegmentedButton`). Données depuis le carnet fictif existant.
4. **Barre d'outils de session complète** (`session_screen.dart`, habillage seulement) : ajoute les groupes manquants en **menus** (sans logique cœur, juste l'UI + SnackBar « à venir ») : indicateur sécurité/empreinte (icône cadenas + menu montrant SAS/empreinte), **Permissions** (menu à cases : audio/clavier/souris/presse-papiers/bloquer entrée/confidentialité), **Actions** (élévation, Ctrl+Alt+Suppr, verrouiller, redémarrer, capture d'écran, tunnel TCP), **Enregistrement** (toggle), **Clavier/saisie**, **favori**. Réorganise le fond de surface en `#000000`.
5. **Dialogue d'acceptation entrante** (nouveau `lib/screens/incoming_request_dialog.dart` ou widget dialog) : identité + empreinte du connecteur, cases de permissions demandées, boutons Accepter/Refuser, sélection de profil (Default/Screen-Sharing/Full/Unattended). Purement visuel + un bouton de démo sur l'accueil ou les réglages pour l'ouvrir (`showDialog`).
6. **Réglages en onglets** (`settings_screen.dart`) : passe la page en `TabBar` (Interface/Sécurité/Connexion/Affichage/Enregistrement/À propos) ; ajoute une section **Profils de permissions** (liste des 4 profils) et **ACL liste blanche** (champ + liste, avec support du joker `*@espace`), en état local pour l'instant.
7. Garde la navigation `Navigator`/routes nommées. N'introduis pas go_router.

## VÉRIF (obligatoire)
- `C:\Users\udohkak\flutter\bin\flutter.bat analyze` → **aucune erreur** (reporte le compte exact d'issues).
- L'app doit rester **navigable sous mock** (défaut `MockNativeApi`) : vérifie que chaque écran se construit (au moins par un `flutter test` de widget si tu en ajoutes, sinon décris la navigation manuelle).
- Bascule clair/sombre fonctionnelle (le `themeModeProvider` existe déjà).
- Reporte : le rendu clair et sombre sont cohérents (accent rouge parcimonieux, surfaces neutres).

## RÉPONSE FINALE ATTENDUE
- Fichiers créés/modifiés.
- Résumé des changements visuels (thème, sidebar, vignettes, toolbar, dialogue, réglages onglets).
- Confirmation des couleurs/densité appliquées (hex primary, surfaces, densité).
- Sortie EXACTE de `flutter analyze` (nb d'issues).
- **Pas de git.**
