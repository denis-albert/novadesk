# Prompt Fable 04 — Câblage de la session live dans l'UI (rendu vidéo + entrées)

**Priorité : P0** · **Crate ciblé : `ui/` (Flutter)** · **Dépend de : 03 (FFI streaming) ET 02 (reskin — même crate `ui/`)**. Lancer **après** 02 et 03. **NON parallélisable avec 02** (même crate).

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : remplacer la **session simulée** de l'UI par une **vraie session** pilotée par le cœur Rust — afficher le **flux vidéo décodé** (aujourd'hui un placeholder noir), pousser les **entrées** réellement, et refléter l'état/les stats en direct.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\ui\`.
- Flutter via `C:\Users\udohkak\flutter\bin\flutter.bat` ; analyse `... analyze`.
- **NE réintroduis AUCUN plugin natif.** En particulier **`irondash_texture` est INTERDIT** (plugin natif, pas d'admin/symlinks). ⇒ **Ne pas** utiliser le widget `Texture` pour le rendu ici. **Rendu par frames RGBA en pur Dart** (voir TÂCHE).
- **AUCUN git.** Le pont FRB (`lib/bridge/generated/`) est **régénéré par l'orchestrateur** avec les nouvelles fonctions du lot 03 : **suppose que les fonctions générées existent** ; si elles ne sont pas encore régénérées sur ce poste, code contre l'interface `NativeApi` (miroir manuel) et le `MockNativeApi`, en ajoutant les nouvelles méthodes aux deux, pour que l'app reste navigable sous mock.

## BARRE QUALITÉ
- `C:\Users\udohkak\flutter\bin\flutter.bat analyze` → **aucune erreur**.
- L'app reste **navigable sous mock** (contrainte poste : la DLL native ne se charge pas ici).

## ÉTAT ACTUEL (à respecter)
- `lib/screens/session_screen.dart` : `_textureId` **toujours `null`** (`:63`) → `_panneauAttente` permanent ; cycle **simulé** `_deroulerConnexion()` (timers `:131-143`) ; entrées capturées + encodées via `encodeInputEvent` mais **jamais envoyées** (comptées `:191-196`).
- `lib/bridge/native_api.dart` : interface `NativeApi` (miroir manuel de `nd-ffi`) + DTO (`InputEventDto`, `SessionConfigDto`, `SessionStateDto`, `PermissionsDto`, …). `lib/bridge/mock_api.dart` : `MockNativeApi` (défaut réel). `lib/bridge/frb_api.dart` : `FrbNativeApi` (adaptateur vers le pont généré).
- `lib/state/providers.dart` : `nativeApiProvider` (défaut `MockNativeApi`).
- **Nouveau, dépend de 03** : fonctions FFI `start_session`, `session_state_stream` (Stream d'états), `session_video_stream` (Stream de `VideoFrameDto {width,height,rgba}`), `session_stats`, `send_input`, `stop_session`.

## TÂCHE
1. **Étendre le contrat `NativeApi`** (`native_api.dart`) avec les nouvelles méthodes : `Future<int> startSession(...)`, `Stream<SessionStateDto> sessionStateStream(int id)`, `Stream<VideoFrameDto> sessionVideoStream(int id)`, `Future<SessionStatsDto> sessionStats(int id)`, `Future<void> sendInput(int id, InputEventDto e)`, `Future<void> stopSession(int id)`. Ajoute le DTO Dart `VideoFrameDto {int width, int height, Uint8List rgba}` et `SessionStatsDto`.
2. **Implémenter dans `FrbNativeApi`** (délégation aux fonctions générées) **et dans `MockNativeApi`** (le mock génère un flux d'images de synthèse — par ex. un dégradé animé ou une mire — pour que le rendu soit **démontrable sans le cœur natif** sur ce poste). Le mock doit émettre ~30 `VideoFrameDto`/s d'une petite image (p. ex. 320×180) et faire progresser l'état Idle→…→Active.
3. **Rendu vidéo pur Dart** (remplace le placeholder dans `session_screen.dart`) : consomme `sessionVideoStream`, convertis chaque `VideoFrameDto` en `ui.Image` via `ui.decodeImageFromPixels(rgba, width, height, PixelFormat.rgba8888, callback)` (ou `ImmutableBuffer` + `ImageDescriptor`), et affiche via `RawImage`/un `CustomPaint`. Gère : dispose des `ui.Image` précédentes (éviter les fuites), `FilterQuality.low/medium`, mise à l'échelle en conservant le ratio, `RepaintBoundary`. **Aucun `Texture`/plugin natif.**
4. **Cycle de vie réel** : à l'ouverture de la session, appeler `startSession` (avec le `SessionConfigDto` déjà construit) ; s'abonner à `sessionStateStream` pour piloter l'UI (remplace `_deroulerConnexion`) ; à la fermeture/`dispose`, `stopSession`. Gère les erreurs (message français, retour accueil).
5. **Entrées réelles** : dans `_envoyer(...)`, remplacer le simple comptage par `await _api.sendInput(_sessionId, event)` (garde le throttle 8 ms souris et les gardes de permission). Conserve les compteurs pour le HUD.
6. **Stats live** : alimente la barre d'état (fps/rtt/octets) depuis `sessionStats` (polling ~1 s ou stream si dispo). Remplace les valeurs en dur (SAS/TLS) par les vraies si disponibles, sinon garde des libellés honnêtes.
7. Garde la barre d'outils et le chrome du lot 02 ; ne régresse pas le reskin.

## VÉRIF (obligatoire)
- `C:\Users\udohkak\flutter\bin\flutter.bat analyze` → **aucune erreur** (reporte le compte).
- **Démonstration sous mock** : lancer l'app (ou un `flutter test` de widget) et vérifier que l'écran de session **affiche l'image animée du mock** (dégradé/mire) au lieu du placeholder — c'est la preuve que le chemin `VideoFrameDto → ui.Image → RawImage` fonctionne bout en bout. Décris ce que tu observes.
- Vérifie l'absence de fuite évidente (dispose des images) et que les entrées appellent bien `sendInput` (log/compteur).

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés.
- Description du **chemin de rendu** retenu (`decodeImageFromPixels` → `RawImage`) et de la façon dont le mock prouve le rendu sans natif.
- Confirmation que le cycle de session et l'envoi d'entrées sont câblés (plus de simulation).
- Sortie EXACTE de `flutter analyze`.
- **Pas de git.**
