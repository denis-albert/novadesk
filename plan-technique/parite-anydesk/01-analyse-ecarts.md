# 01 — Analyse des écarts par sous-système (état réel constaté)

> **Méthode.** Lecture du code (lib.rs + modules + exemples), comptage des tests, grep des `NotImplemented`/`todo!`/`unimplemented!`. Verdicts : **RÉEL+TESTÉ** (code fonctionnel + tests) · **MVP** (fonctionne mais limité/loopback/non câblé) · **STUB** (coquille/NotImplemented) · **ABSENT**. Chiffres : **~36 000 lignes Rust**, **466 tests unitaires**, **0 `#[tokio::test]`** (le transport ponte le runtime en `block_on`), **~5 900 lignes Dart** (UI).
>
> **Thèse centrale.** Le fossé n'est **pas** la qualité des briques (élevée, bien testée) — c'est **l'intégration** : les composants marchent isolément et en **exemples loopback**, mais **rien n'assemble une session live reliée à l'UI et au réseau réel**. `nd-features` est même une **île** (aucune autre crate ne la consomme).

---

## 0. Tableau de bord (verdict par crate)

| Crate | Lignes | Tests | Verdict global | Rôle dans la parité |
|---|---:|---:|---|---|
| nd-proto | 349 | 5 | RÉEL+TESTÉ | Types/sérialisation — socle sain |
| nd-crypto | 1360 | 25 | **RÉEL+TESTÉ** (le plus solide) | Noise XX/IK, SAS, pinning TOFU, rekey |
| nd-transport | 1789 | 22 | RÉEL+TESTÉ (loopback) | QUIC quinn + datagrammes + FEC RS adaptatif |
| nd-signaling | 2441 | 43 | RÉEL+TESTÉ (loopback) | Rendez-vous + STUN + hole punch + NAT (MVP typage) |
| nd-capture | 1651 | 5 | RÉEL+TESTÉ (Win) | DXGI DDA + dirty-rects + curseur ; GPU zéro-copie absent |
| nd-codec | 2315 | 29 | MVP | H.264 SW (openh264) + MF **logiciel** ; ABR/delta/HW **non câblés** |
| nd-input | 1373 | 8 | RÉEL (Win/mac/X11), peu testé | SendInput complet ; injection **jamais testée**, multi-écran non fait |
| nd-audio | 3366 | 64 | RÉEL+TESTÉ | Opus + WASAPI capture/playback + DSP ; macOS SCK absent |
| nd-files | 2793 | 20 | RÉEL+TESTÉ | Transfert BLAKE3 + reprise + LocalFs + presse-papiers riche |
| nd-features | 5074 | 103 | MVP (**île non branchée**) | Permissions/recording/annotation/tunnel/wol/privacy/hotkeys/invite/settings |
| nd-core | 1446 | 4 | **MVP/STUB** | Pipelines réels (exemples) ; **`Session` = coquille**, pas d'orchestrateur |
| nd-ffi | 1718 | 16 | RÉEL+TESTÉ (helpers) | **Aucune API de session live** (formatage/validation/sérialisation seules) |
| nd-wasm | 12 | 0 | **STUB** | Client web entièrement à faire |
| server/nd-rendezvous | 25 | 0 (+43) | RÉEL+TESTÉ | Délègue à nd-signaling ; **aucune auth (squatting d'ID)** |
| server/nd-relay | 680 | 8 | RÉEL+TESTÉ | Relais par ticket ; **tickets non signés** |
| server/nd-accounts | 3379 | 66 | RÉEL (biblio) / MVP (réseau) | Argon2id/TOTP/PKCE ; **OIDC RS256/ES256 absent** ; JSON ; 2FA/OIDC inatteignables par le réseau |
| server/nd-api | 3592 | 48 | RÉEL (données) / STUB (autorisation) | RBAC/groupes/partage ; **jetons factices, RBAC non appliqué** |
| ui (Flutter) | ~5900 | — | MVP | 4 écrans ; **session simulée, vidéo absente, marque indigo** |
| packaging | — | — | **ABSENT** | README seul ; aucun installeur/signature/MAJ |

---

## 1. Chemin de session live (le cœur du problème — P0)

### 1.1 nd-core — orchestration
- **Constaté** : `Session` (`lib.rs:69-127`) est une **machine à états coquille** : `begin()` ne fait que `Idle→Resolving` (`:106-114`), `Connecting/Handshaking/Active/Reconnecting` **jamais pilotés**, `SessionComponents.transport/secure` **jamais peuplés**. Le **vrai** pipeline (`HostPipeline`, `ViewerPipeline`, `establish`, `EncryptedTransport`) est **réel** mais assemblé **uniquement dans des exemples** (`examples/secure_desktop.rs`, `viewer_window.rs`, `e2e_session.rs`, `connect_by_id.rs`, `control_loop.rs`, `vertical_slice.rs`) — tous **loopback**. `ViewerPipeline::run` **jette les pixels décodés** (`:278-281`).
- **Cible AnyDesk** : un moteur de session qui enchaîne résolution→connexion→handshake→média en continu, gère le multithread, les erreurs, la reprise, et **expose** l'état + les frames + un canal d'entrées.
- **Écart** : pas d'orchestrateur réutilisable ; frames non exposées.
- **Action (P0)** : `SessionEngine` (nouveau) qui câble les briques existantes dans la machine à états, un `ViewerPipeline` **qui conserve les frames** (callback/canal), instrumentation fps/latence. → prompt `01`.

### 1.2 nd-ffi — pont vers l'UI
- **Constaté** : n'expose que des **helpers purs** (`app_info`, `format_nova_id`, `parse_nova_id`, `session_status`, `new_session_config`, `encode/decode_input_event`, `engine_version_string`). **Aucune** fonction start/stop/connect, **aucun** `StreamSink`, **aucune** sortie de frame. `new_session_config` **valide** un DTO, ne démarre rien.
- **Cible** : API de cycle de vie (démarrer/arrêter), `Stream` d'états, **`Stream` de frames vidéo**, envoi d'entrées, `Stream` de stats/chat/transferts.
- **Écart** : tout le contrôle de session live.
- **Action (P0)** : étendre `nd-ffi` avec des fonctions FRB `StreamSink` (état, frames RGBA, stats) + `start_session`/`stop_session`/`send_input`. → prompt `03` (après `01`).

### 1.3 ui (Flutter) — rendu et cycle de vie
- **Constaté** : session **simulée** (`session_screen.dart:128` « SIMULATION », timers codés en dur `:131-143`) ; `Texture` conditionnée à `_textureId` **toujours `null`** (`:63`, jamais assigné) → **placeholder permanent** ; entrées capturées et **encodées** mais **jamais envoyées** (comptées pour un HUD, `:191-196`) ; chat **local** ; transferts **fictifs** ; défaut = **`MockNativeApi`** (`providers.dart:22`).
- **Cible** : surface vidéo live, cycle de session réel piloté par le cœur, entrées transmises, chat/transferts réels.
- **Écart** : rendu vidéo + câblage session.
- **Contrainte** : le rendu par **texture GPU externe** (`irondash_texture`) est un **plugin natif → interdit ici** (pas d'admin/symlinks). Solution sans plugin : **streamer les frames RGBA** via FRB et peindre avec `ui.decodeImageFromPixels`/`RawImage` (repli CPU, perf moindre mais fonctionnel). → décision P0 dans [`00`](00-synthese-et-roadmap.md).
- **Action (P0)** : brancher le `Stream` de frames sur un `RawImage`, câbler start/stop/état/entrées. → prompt `04` (après `03`).

### 1.4 Connectivité réseau réelle
- **Constaté** : `connect()` prend une **adresse directe** ; STUN/hole-punch/NAT (nd-signaling, **réels et testés**) et relais **non câblés** dans le path de session (0 réf. dans nd-transport) ; **loopback uniquement** ; **aucune infra publique** ; rendez-vous et relais **sans authentification**.
- **Cible** : connexion par ID à un pair **derrière NAT** sur Internet, repli relais authentifié.
- **Écart** : orchestration STUN→publication→punch→QUIC + auth d'infra + hébergement + validation WAN.
- **Action (P0/P1)** : connecteur `connect_by_id` NAT dans nd-core/nd-signaling + auth serveurs + attribution d'ID. → prompts `05`, `07`.

---

## 2. Média (capture / codec / audio)

### 2.1 nd-capture — RÉEL+TESTÉ (Windows)
DXGI Desktop Duplication (`win.rs:214-346`), **dirty-rects** (`:180-211`), curseur GDI (`win_cursor.rs`). **Manques** : zéro-copie GPU (`FrameImage` = CPU seul, `lib.rs:170`), Wayland/PipeWire (`NotImplemented`, chantier), multi-écran côté injection.

### 2.2 nd-codec — MVP (voir aussi [`02`](02-performance-anydesk.md))
H.264 encode+decode **réels** (openh264 SW ; MF = **MFT logiciel MS**, pas NVENC, `mediafoundation.rs:339`). **ABR réel mais non câblé** ; **dirty-rects ignorés** (plein cadre) ; `set_target_bitrate` **no-op** en SW ; **pas de matériel**. → prompt `06`.

### 2.3 nd-audio — RÉEL+TESTÉ
Opus FFI (`codec.rs`), WASAPI capture système (loopback) + micro + playback (Win), CoreAudio (macOS playback), Pulse/PipeWire (Linux), DSP (jitter/mixing/level/convert, 43 tests). **Manque** : capture système **macOS ScreenCaptureKit** (`NotImplemented`, honnête, `lib.rs:104`). Non câblé à une session live (comme tout le reste).

---

## 3. Entrées, fichiers, presse-papiers

### 3.1 nd-input — RÉEL (peu testé)
`SendInput` complet (souris absolue ×65535/relative/5 boutons/molette HiRes ; clavier scancode ; Unicode UTF-16 surrogates ; injection tactile ; SendSAS) `win.rs`. macOS `CGEventPost`, Linux **XTEST**. **Faiblesses** : injection **jamais testée en comportement** (3 tests = suivi d'état seul) ; **multi-écran non fait** (écran primaire, param moniteur ignoré `:319`) ; **pas de touches mortes** ; SendSAS **no-op** hors service SYSTEM ; Wayland absent.

### 3.2 nd-files — RÉEL+TESTÉ
Transfert BLAKE3 chunké avec **reprise** (`transfer.rs`), `LocalFs` **complet** (les `NotImplemented` de `lib.rs:47-89` sont des **défauts de trait**, réimplémentés par `LocalFs:237-306` + jail anti-traversée), presse-papiers **riche** Windows (texte `CF_UNICODETEXT`, image `CF_DIB`, fichiers `CF_HDROP`). **Manque** : le transfert n'est **pas branché** sur le canal de session ; presse-papiers non testé (session bureau requise).

---

## 4. Fonctionnalités (nd-features) — MVP, **île non branchée**

**Constat transverse critique** : aucune autre crate ne consomme `nd-features` (grep des symboles publics → références internes seulement). Les permissions ne sont **jamais appliquées** (aucun `authorize()` hors tests), la privacy jamais exécutée, les hotkeys jamais dispatchées, l'enregistreur jamais alimenté.

| Sous-fonction | Verdict | Détail |
|---|---|---|
| permissions | RÉEL+TESTÉ (modèle) | 12 capacités granulaires, deny-par-défaut, broker + audit (`permissions.rs`). **Non invoqué** par la session. |
| recording | **MVP « vendu fini »** | Conteneur `.ndr` (BLAKE3, index) mais **sérialise des octets opaques** ; **aucun encodeur/mux mp4-webm, aucun capteur ne l'alimente** (`recording.rs:25,144`). Ne produit **aucune vidéo lisible**. |
| annotation | RÉEL+TESTÉ (partiel) | Rasteriseur alpha (ligne/rect/ellipse/flèche) ; **texte = soulignement**, pas de glyphes. Buffer RGBA non branché à un overlay. |
| tunnel | **MVP « vendu fini »** | Pipe TCP réel mais flux « distant » = **socket local**, pas la session chiffrée (`tunnel.rs:2-6`). |
| wol | RÉEL+TESTÉ | Paquet magique + UDP broadcast, fonctionnel. |
| privacy | **STUB d'effet** | Calcule des actions mais **ne touche pas le système** (`privacy.rs:7`). |
| reconnect | RÉEL+TESTÉ | Backoff+jitter, **pur calcul, ne reconnecte rien**, non câblé. |
| hotkeys | MVP | Table + sérialisation ; **pas de capture globale ni dispatch**. |
| invite | RÉEL (logique) / MVP (sécu) | Codes QuickSupport ; **RNG non cryptographique** auto-signalé. |
| settings | RÉEL+TESTÉ | Presets + validation. |

→ prompt `08` (intégration + recording réel).

---

## 5. Sécurité / crypto — RÉEL+TESTÉ (point fort)

`nd-crypto` : `snow`, motif **Noise_XX_25519_ChaChaPoly_BLAKE2s** + **IK** (non-surveillé), handshake 3 messages, AEAD, **rekey** deux directions, **empreinte** BLAKE2s + **SAS 6 chiffres**, **pinning TOFU** disque. 25 tests. **Réserve** : SAS ~**20 bits** d'entropie (4 octets) — un peu faible face à un attaquant motivé ; à porter à 5-6 octets. Handshake testé en mémoire (normal).

---

## 6. Infrastructure serveur

### 6.1 nd-rendezvous — RÉEL+TESTÉ mais **ouvert**
Délègue à nd-signaling (register/lookup/heartbeat/candidates/punch, TTL+sweeper). **Faille** : **aucune authentification** — n'importe qui enregistre **n'importe quel ID** (squatting, usurpation). → prompt `07`.

### 6.2 nd-relay — RÉEL+TESTÉ mais **non authentifié**
Relais aveugle par ticket, quotas mémoire, sélection par RTT. **Faille** : **tickets non signés** (`main.rs:13`), quota par défaut illimité. → prompt `07`.

### 6.3 nd-accounts — RÉEL (biblio) / MVP (réseau)
Argon2id, TOTP RFC 6238, **PKCE S256** réels et testés. **Failles** : **OIDC RS256/ES256 absent** (seul HS256 ; fédération Google/Entra/Keycloak **inutilisable**), pas d'échange code→jetons (pas de client HTTP), **persistance JSON** (pas de DB), **secrets TOTP en clair**, et le serveur n'expose que **Register+Login** (2FA/OIDC/licensing **inatteignables par le réseau**). → prompt `09`.

### 6.4 nd-api — RÉEL (données) / **autorisation STUB**
14 endpoints (carnet/RBAC/groupes/partage/MAJ/config) sur **TCP binaire maison** (pas axum), persistance JSON. **Faille majeure** : **tout jeton non vide accepté**, aucune validation croisée avec nd-accounts, le compte agi est **fourni dans la requête** (pas dérivé du jeton), `AssignRole` sans contrôle admin → **RBAC non appliqué comme contrôle d'accès**. → prompt `07`.

### 6.5 Persistance & déploiement
**Fichiers JSON** uniquement (pas de DB, pas de migrations), **4 binaires indépendants non câblés entre eux**, **aucun docker-compose/Dockerfile**. CI présente ; release = binaires serveur **bruts non signés**.

---

## 7. Client web & packaging

- **nd-wasm** : **STUB** 12 lignes (une fonction version). Aucun wasm-bindgen/web-sys/WebTransport/WebCodecs. Client web **entièrement à faire**. → prompt `10`.
- **packaging/** : **ABSENT** (README seul). Aucun MSI/dmg/deb/AppImage, aucune signature (Authenticode/codesign/GPG), aucun auto-update/TUF. → prompt `11`.

---

## 8. Ce qui empêche AUJOURD'HUI un usage « comme AnyDesk »

1. **Installer** : pas d'installeur, pas d'app cliente packagée (UI non construite ici ; nd-wasm stub). → **impossible**.
2. **Obtenir un ID** : `NovaId` = `u64` sans service d'attribution/unicité/liaison compte. → **impossible proprement**.
3. **Se connecter à un pair distant réel** : loopback only, NAT/relais non câblés, infra non déployée, rendez-vous ouvert. → **impossible hors LAN de test**.
4. **Voir l'écran fluide** : rendu vidéo absent dans l'UI (`_textureId` null), session simulée. → **impossible depuis l'UI**.
5. **Contrôler** : entrées encodées mais non transmises. → **impossible depuis l'UI**.
6. **Transférer** : moteur réel mais non branché à la session ni à l'UI. → **impossible depuis l'UI**.

**Conclusion** : produit **pré-alpha** en tant qu'application, bâti sur un **socle d'ingénierie réel et testé**. La priorité absolue est l'**intégration verticale** (cœur↔UI↔réseau), pas l'ajout de fonctionnalités.
