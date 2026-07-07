# Prompt Fable 10 — Client web (nd-wasm)

**Priorité : P2** · **Crate ciblée : `crates/nd-wasm`** · **Parallélisable avec : tout** (crate isolée, aujourd'hui stub 12 lignes).

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : transformer la coquille `nd-wasm` en un **client web de visualisation** (contrôle sortant, comme le client web d'AnyDesk : contrôle seul, jamais contrôlé) : se connecter à un pair, **décoder et afficher** le flux, transmettre les entrées.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-wasm\` (+ éventuel dossier `web/` d'exemple sous ce chemin).
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`. Cible `wasm32-unknown-unknown` (vérifie que la target est installée ; sinon documente `rustup target add wasm32-unknown-unknown`).
- **AUCUN git.** Verrou cargo parallèle = normal.

## BARRE QUALITÉ
- `cargo clippy -p nd-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings` = **ZÉRO** (ou, si la cible pose souci en CI, clippy natif + build wasm séparé — documente).
- `cargo fmt -p nd-wasm`.

## ÉTAT ACTUEL
- `nd-wasm/src/lib.rs` = **12 lignes** : une fonction `engine_version_string()` appelant `nd_core::engine_version()`. `Cargo.toml` déclare `cdylib` mais **aucune dépendance wasm** (pas de `wasm-bindgen`/`web-sys`/`js-sys`).
- Contraintes navigateur : pas de QUIC/UDP brut → utiliser **WebTransport** (HTTP/3) ou **WebRTC DataChannel** ; décodage via **WebCodecs** (`VideoDecoder` H.264) ; rendu **Canvas/WebGL**.

## TÂCHE
1. **Bindings wasm** : ajouter `wasm-bindgen`, `web-sys` (features WebTransport/WebCodecs/Canvas/WebGL selon besoin), `js-sys`, `wasm-bindgen-futures`. Exposer une API JS (`#[wasm_bindgen]`) : `connect(signaling_url, peer_id, token)`, `on_frame` (callback vers Canvas), `send_input(...)`, `disconnect()`.
2. **Transport navigateur** : implémenter la connexion via **WebTransport** (préféré) vers un endpoint compatible (à documenter : le relais/rendez-vous devra offrir un pont WebTransport, hors périmètre de cette crate — **documente la dépendance infra**). À défaut d'infra, livrer un **mode démo** qui décode un flux H.264 de test et l'affiche (prouve le chemin decode→canvas).
3. **Décodage** : brancher **WebCodecs `VideoDecoder`** (H.264) → frames → **Canvas/WebGL**. Gérer keyframes/erreurs.
4. **Entrées** : capter souris/clavier du canvas, sérialiser au format `nd-proto` (réutilise `encode_input_event` logique via `nd-proto`/`nd-ffi` si compilable en wasm, sinon réimplémente le mapping documenté), envoyer sur le canal.
5. **Page de démo** `nd-wasm/web/index.html` + glue JS minimale chargeant le module et affichant le canvas. Documenter la commande `wasm-pack build`/`wasm-bindgen`.

## VÉRIF (obligatoire)
- `cargo build -p nd-wasm --target wasm32-unknown-unknown --manifest-path ...` → OK (ou `wasm-pack build` documenté et lancé).
- Décrire la **démo** : ce qui s'affiche dans le canvas (flux de test décodé) — preuve du chemin decode→rendu.
- `cargo clippy -p nd-wasm ... -- -D warnings` → **0** ; `cargo fmt`.
- **Régression** : ne casse pas le workspace (`cargo build --workspace` natif doit ignorer proprement la crate wasm-only — vérifie qu'elle compile aussi côté natif ou qu'elle est correctement `cfg`).

## RÉPONSE FINALE ATTENDUE
- Fichiers créés/modifiés ; API JS exposée.
- Ce qui marche (démo decode→canvas) vs dépendances infra (pont WebTransport) documentées honnêtement.
- État EXACT des vérifs.
- **Pas de git.**
