# Prompt Fable 01 — Orchestrateur de session `SessionEngine` (nd-core)

**Priorité : P0** · **Crate ciblé : `crates/nd-core`** · **Parallélisable avec : 02 (ui), 06 (nd-codec), 07 (serveurs), 08 (nd-features)** — crates disjoints. **Bloque : 03 (nd-ffi)**.

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : transformer la tranche verticale qui n'existe aujourd'hui que dans des exemples loopback en un **orchestrateur de session réutilisable et instrumenté**, qui pilote la machine à états, **conserve les frames décodées** (aujourd'hui jetées) et les expose à un consommateur (future UI), et accepte un flux d'entrées à transmettre.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-core\`.
- Cargo **toujours** avec `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git** (l'orchestrateur committe). Plusieurs agents tournent en parallèle : un **verrou cargo** temporaire est **normal**, réessaie.
- Ne modifie **aucune autre crate**. Tu **consommes** les API publiques déjà existantes de `nd-transport`, `nd-signaling`, `nd-crypto`, `nd-capture`, `nd-codec`, `nd-input`, `nd-features`, `nd-proto` **sans les changer**.

## BARRE QUALITÉ
- `cargo clippy -p nd-core --all-targets -- -D warnings` = **ZÉRO warning** (attention `type_complexity` : introduis des alias de types ou des structs plutôt que des tuples imbriqués ; encapsule les canaux dans des structs nommées).
- `cargo fmt -p nd-core`.
- Tout nouveau code public documenté (`///`) en français.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `crates/nd-core/src/lib.rs` expose déjà : `SessionRole`, `SessionState {Idle,Resolving,Connecting,Handshaking,Active,Reconnecting,Closed}`, `SessionConfig {role, local_id:NovaId, peer_id:Option<NovaId>, permissions}`, `SessionComponents`, `Session` (machine à états coquille : `begin()` ne va qu'à `Resolving`), `HostPipeline::{new,run}`, `ViewerPipeline::{new,run}` (**jette les pixels : n'utilise que `frame.width/height`**), `EncryptedTransport`, `establish(inner, HandshakeRole, &static_priv) -> Result<EncryptedTransport>`, `apply_input(&dyn InputInjector, &InputEvent)`, `engine_version()`. **Garde toutes ces signatures.**
- Dépendances déjà disponibles (API publiques) :
  - `nd_transport`: `trait Transport {open_channel, send, poll_recv, path_estimate}`, `bind(addr)->Listener`, `connect(addr,&cert)->impl Transport`, `Listener::{accept, local_addr, server_cert_der}`, `ChannelHandle`, `PathEstimate {rtt_us, loss_ratio, estimated_bandwidth_kbps}`.
  - `nd_signaling`: `RendezvousClient::{new, register, lookup, heartbeat, publish_candidates, peer_candidates, request_punch, poll_punch}`, `PeerRecord {addr, cert_der}`, modules `stun`, `punch`, `nat`.
  - `nd_crypto`: `generate_static_keypair()`, `HandshakeRole::{Initiator,Responder}`, `PeerFingerprint`.
  - `nd_codec`: `create_encoder(CodecKind)`, `create_decoder(CodecKind)`, `CodecKind::H264`, `EncoderConfig`, `EncodedChunk`, `DecodedFrame {width,height,rgba:Vec<u8>}`, `VideoEncoder`, `VideoDecoder`.
  - `nd_capture`: `create_capturer()`, `CaptureConfig`, `CapturedFrame {width,height,image,dirty}`, `ScreenCapturer`.
  - `nd_input`: `InputInjector`, `create_injector()` (vérifie le nom exact dans `nd-input/src/lib.rs`), `MouseButton`.
  - `nd_proto`: `InputEvent`, `ChannelKind::{Video,Control,Input}`, `Reliability`, `NovaId`, `MonitorId`.
- Les exemples `nd-core/examples/{viewer_window,secure_desktop,connect_by_id,control_loop,e2e_session,vertical_slice}.rs` montrent le câblage cible (loopback). **Inspire-toi-en**, notamment `viewer_window.rs` (décode→garde la frame la plus récente) et `connect_by_id.rs` (résolution par ID).
- Pas de runtime async : le transport ponte `block_on` en interne ; **utilise des threads `std` + `std::sync::mpsc`** (n'ajoute pas tokio).

## TÂCHE
1. **Rendre les frames consommables.** Ajoute à `ViewerPipeline` (sans casser `run`) un mode « callback » : par ex. `run_streaming(&mut self, mut on_frame: impl FnMut(DecodedFrame), stop: Arc<AtomicBool>) -> Result<usize>` qui décode en continu et **passe chaque `DecodedFrame` la plus récente** au callback (skip des frames en retard, comme `viewer_window.rs`). L'ancien `run` reste inchangé.
2. **Créer `SessionEngine`** (nouveau module `session.rs`, réexporté par `lib.rs`) :
   - `SessionEngine::start(config: SessionConfig, endpoint: SessionEndpoint) -> Result<SessionHandle>`.
   - `SessionEndpoint` : enum décrivant comment joindre le pair — au minimum `Loopback { listener: Listener }` / `Direct { addr, cert }` (contrôleur) — de façon à rester **testable en loopback maintenant** ; prévois une variante `ByRendezvous { server, ... }` documentée mais tu peux la laisser en repli simple (la connectivité NAT complète est le lot 05).
   - Le moteur lance un/des **threads** qui pilotent la machine à états et poussent les transitions `SessionState` dans un canal.
   - **Rôle contrôleur (viewer)** : connecte QUIC → `establish(Initiator)` → ouvre le canal vidéo → `ViewerPipeline::run_streaming` qui pousse les `DecodedFrame` dans un `mpsc::Receiver<DecodedFrame>` exposé ; lit un `mpsc::Receiver<InputEvent>` et envoie les entrées sérialisées sur le canal `Input`.
   - **Rôle contrôlé (hôte)** : accepte → `establish(Responder)` → `HostPipeline` (capture→encode→send) ; reçoit les entrées et les applique via `apply_input`.
   - **`SessionHandle`** (struct nommée, pas de tuple) : `state_rx: Receiver<SessionState>`, `frame_rx: Receiver<DecodedFrame>` (contrôleur), `input_tx: Sender<InputEvent>` (contrôleur), `stats() -> SessionStats`, `stop(self)`.
   - **`SessionStats`** : `fps: f32`, `rtt_us: u64` (depuis `path_estimate`), `bytes_in/bytes_out: u64`, `frames_decoded: u64`. Mets à jour en continu.
3. **Instrumentation** : compte fps (fenêtre glissante), octets, rtt ; expose via `stats()`.
4. **Exemple-sonde exécutable** `examples/session_engine_demo.rs` : monte **hôte + viewer en loopback** via `SessionEngine`, laisse tourner ~2 s, **assert** qu'au moins N (≥ 10) `DecodedFrame` sont arrivées dans `frame_rx` et que `stats().fps > 0`, envoie quelques `InputEvent` du viewer et vérifie qu'ils sont reçus côté hôte (compteur). Affiche un résumé et sort `Ok(())`/`Err`.
5. Ne touche pas au `Session` existant (garde-le), mais tu peux le réutiliser en interne si utile.

## VÉRIF (obligatoire, reporte les chiffres EXACTS)
- `cargo build -p nd-core --examples --manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml` → OK.
- `cargo test -p nd-core --manifest-path ...` → tous verts (les 4 tests existants + tes nouveaux tests unitaires du moteur/pipeline streaming).
- `cargo run --example session_engine_demo -p nd-core --manifest-path ...` → doit imprimer le nombre de frames reçues (≥ 10) et `OK`.
- `cargo clippy -p nd-core --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt -p nd-core --manifest-path ...`.
- **Régression** : vérifie que les autres exemples compilent toujours (`--examples`).

## RÉPONSE FINALE ATTENDUE
- Liste des fichiers modifiés/créés.
- Résumé (5-10 lignes) de l'architecture du `SessionEngine` (threads, canaux, machine à états).
- Les **signatures publiques exactes** ajoutées (`SessionEngine`, `SessionHandle`, `SessionStats`, `SessionEndpoint`, `run_streaming`).
- État EXACT des vérifs **avec chiffres** (nb tests, nb frames dans la sonde, fps mesuré, clippy 0).
- **Pas de git.**
