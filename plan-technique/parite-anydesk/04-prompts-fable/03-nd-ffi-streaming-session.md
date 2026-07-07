# Prompt Fable 03 — Façade FFI temps réel : session, frames, entrées (nd-ffi)

**Priorité : P0** · **Crate ciblé : `crates/nd-ffi`** · **Dépend de : 01 (SessionEngine dans nd-core)**. **Parallélisable avec : 05, 06, 07, 08, 09** (crates disjointes). **Bloque : 04 (ui câblage)**.

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : étendre la façade `nd-ffi` (aujourd'hui limitée à des helpers purs) avec une **API de session live** exposable à Flutter via `flutter_rust_bridge` : démarrer/arrêter une session, **streamer l'état et les frames vidéo**, envoyer des entrées, remonter des stats.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-ffi\`.
- Cargo **toujours** avec `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo en parallèle = normal.
- **Ne régénère PAS** le pont Dart ici (`frb_generated.rs` est généré ; il se régénère avec `flutter`/`flutter_rust_bridge_codegen` dans le PATH, hors de ce poste). Écris l'API Rust **prête à être bridgée** (types plats, `StreamSink`), et documente la commande de régénération pour l'orchestrateur.
- Tu **consommes** `nd-core` (lot 01) sans le modifier.

## BARRE QUALITÉ
- `cargo clippy -p nd-ffi --all-targets -- -D warnings` = **ZÉRO** (attention `type_complexity`).
- `cargo fmt -p nd-ffi`.
- Doc `///` française sur tout public.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `crates/nd-ffi/src/api.rs` expose des **helpers purs** : `app_info`, `format_nova_id`, `parse_nova_id`, `session_status`, `new_session_config`, `encode_input_event`, `decode_input_event`, + DTO plats `SessionRoleDto`, `SessionStateDto`, `SessionStatusDto`, `PermissionsDto`, `SessionConfigDto`, `InputEventDto` (avec `From`/`Into` vers les types `nd_core`/`nd_features`/`nd_proto`). **Garde toutes ces signatures et conversions.**
- `crates/nd-ffi/src/lib.rs` réexporte l'API ; `frb_generated.rs` est le pont généré (**ne pas éditer à la main**).
- La convention FRB du projet : DTO **plats** (`String`, `u64`, `bool`, `f64`, `Option`, `Vec<u8>`), fonctions faillibles en `Result<_, String>` (message français), flux via `StreamSink`.
- **Nouveau, dépend de 01** : `nd_core::{SessionEngine, SessionHandle, SessionStats, SessionEndpoint}` + `ViewerPipeline::run_streaming` + `DecodedFrame {width,height,rgba:Vec<u8>}`. Vérifie leurs signatures exactes dans `nd-core/src/session.rs` avant de coder.

## TÂCHE
1. **DTO frame** : `VideoFrameDto { width: u32, height: u32, rgba: Vec<u8> }` (plat, bridgeable). Conversion depuis `nd_core::DecodedFrame`.
2. **DTO stats** : `SessionStatsDto { fps: f64, rtt_us: u64, bytes_in: u64, bytes_out: u64, frames: u64 }`.
3. **Gestion d'instance de session** : comme FRB ne porte pas bien un objet mutable partagé, expose une **API par identifiant de session opaque** (`u64` ou `String`) gérée dans une table statique (`OnceLock<Mutex<HashMap<...>>>`), OU un objet opaque `#[frb(opaque)]` `SessionController` encapsulant le `SessionHandle`. Choisis l'approche **la plus simple compatible FRB 2.x** et documente-la. Fonctions :
   - `start_session(config: SessionConfigDto, endpoint: SessionEndpointDto) -> Result<SessionId, String>` — démarre le `SessionEngine` (pour l'instant `SessionEndpointDto` peut ne proposer que `Loopback`/`Direct{addr,cert}` en attendant le lot 05 ; documente).
   - `session_state_stream(id, sink: StreamSink<SessionStateDto>)` — pousse chaque transition d'état.
   - `session_video_stream(id, sink: StreamSink<VideoFrameDto>)` — pousse chaque frame décodée (rôle contrôleur). **C'est la fonction clé du rendu UI.**
   - `session_stats(id) -> Result<SessionStatsDto, String>` — snapshot des stats.
   - `send_input(id, event: InputEventDto) -> Result<(), String>` — pousse une entrée dans le canal du moteur.
   - `stop_session(id) -> Result<(), String>`.
4. **Pont des `StreamSink`** : sur un thread dédié, draine les `Receiver` du `SessionHandle` (state_rx, frame_rx) et appelle `sink.add(...)`. Gère l'arrêt propre (fermeture du sink → arrêt du drain).
5. **Type_complexity** : encapsule la table de sessions et les canaux dans des structs/alias nommés.
6. **Tests** : au moins un test d'intégration (dans `tests/`) qui, **en loopback**, appelle `start_session` (hôte + viewer via l'endpoint loopback), collecte quelques `VideoFrameDto` via un sink de test (ou via l'API si un mode collecte synchrone est plus simple à tester), et vérifie `width/height > 0` et `rgba.len() == width*height*4`. Réutilise le pattern de la sonde du lot 01.
7. **Documente** en tête de `lib.rs` la **commande de régénération FRB** à lancer par l'orchestrateur (`flutter_rust_bridge_codegen generate` avec le `flutter_rust_bridge.yaml` de `ui/`), et la liste des **nouvelles fonctions** que le Dart devra exposer.

## VÉRIF (obligatoire, chiffres exacts)
- `cargo build -p nd-ffi --manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml` → OK.
- `cargo test -p nd-ffi --manifest-path ...` → verts (les 16 existants + tes nouveaux, dont le test loopback de frames).
- `cargo clippy -p nd-ffi --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt -p nd-ffi --manifest-path ...`.
- **Régression** : `cargo build -p nd-core -p nd-ffi` OK ; les helpers existants (`format_nova_id`, etc.) inchangés (le test `tests/facade.rs` passe toujours).

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés.
- **Signatures publiques exactes** des nouvelles fonctions + DTO, et l'approche de gestion d'instance retenue (table statique vs `#[frb(opaque)]`).
- La **commande de régénération FRB** documentée.
- État EXACT des vérifs (nb tests, nb frames collectées dans le test, clippy 0).
- **Pas de git.**
