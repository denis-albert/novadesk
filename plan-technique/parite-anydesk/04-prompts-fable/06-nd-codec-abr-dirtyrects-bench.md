# Prompt Fable 06 — Boucle de performance : ABR câblé + encodage delta + bench (nd-codec)

**Priorité : P1** · **Crate ciblée : `crates/nd-codec`** · **Parallélisable avec : 01, 02, 05, 07, 08** (crates disjointes).

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : fermer la **boucle de performance** — brancher l'ABR existant (aujourd'hui non utilisé), rendre `set_target_bitrate` **réel**, exploiter les **dirty-rects** (aujourd'hui ignorés) pour réduire débit et CPU, et fournir un **bench non-régressé**. Objectifs chiffrés : voir `../02-performance-anydesk.md`.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-codec\`.
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo parallèle = normal.
- Tu peux **lire** `nd-capture` (pour `CapturedFrame.dirty`) et `nd-transport` (pour `PathEstimate`) mais **n'édite qu'`nd-codec`**. Si l'API d'entrée doit évoluer, fais-le **de façon rétrocompatible** (nouvelle méthode, ancien chemin conservé).

## BARRE QUALITÉ
- `cargo clippy -p nd-codec --all-targets -- -D warnings` = **ZÉRO** (attention `type_complexity` sur les configs).
- `cargo fmt -p nd-codec`.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `nd-codec` (29 tests) : `create_encoder(CodecKind)`, `create_decoder(CodecKind)`, `CodecKind::H264`, `VideoEncoder {configure(EncoderConfig), encode(&CapturedFrame, force_keyframe) -> EncodedChunk, set_target_bitrate(...)}`, `VideoDecoder {decode(&EncodedChunk) -> Option<DecodedFrame>}`, `EncoderConfig {kind,width,height,target_bitrate_kbps,max_fps}`, `EncodedChunk {data,is_keyframe,monitor,timestamp_us}`, `DecodedFrame {width,height,rgba}`.
- **Backends** : `software.rs` (openh264, encode+decode) — `set_target_bitrate` est un **no-op TODO** (`:114-117`) ; `mediafoundation.rs` — **MFT logiciel Microsoft** (`:339` `hardware:false`, **pas NVENC**), `set_target_bitrate` best-effort.
- **ABR réel mais NON câblé** : `negotiation.rs` (14 tests) `BitrateLadder`, `negotiate(...)`, profils Texte/Vidéo, hystérésis. **Personne ne l'appelle** dans le chemin d'encodage.
- **Dirty-rects** : `nd_capture::CapturedFrame.dirty` est **rempli** par DXGI mais **ignoré** par les encodeurs (ré-encodage plein cadre).
- `metrics.rs` (13 tests) : PSNR/SSIM (outil de mesure de qualité).

## TÂCHE
1. **`set_target_bitrate` réel** :
   - openh264 (`software.rs`) : appeler l'API de réglage de débit d'openh264 (bitrate cible + éventuellement `RC_BITRATE_MODE`) au lieu du no-op. Vérifier l'effet par un test (taille de flux qui varie avec le débit cible sur une séquence synthétique).
   - Media Foundation : consolider le best-effort existant.
2. **Câbler l'ABR** : ajouter un contrôleur `RateController` (ou similaire) qui, à partir d'un `PathEstimate`-like (`rtt_us`, `loss_ratio`, `estimated_bandwidth_kbps` — définis une petite struct d'entrée locale à nd-codec pour ne pas dépendre de nd-transport), utilise `BitrateLadder`/`negotiate` (hystérésis) pour décider du débit cible et appelle `set_target_bitrate`. Expose une méthode publique du type `VideoEncoder::apply_network_estimate(&mut self, est: NetEstimate)` **ou** un helper libre. Rétrocompatible.
3. **Encodage delta / dirty-rects** : exploiter `CapturedFrame.dirty` :
   - au minimum, **skip d'encodage** si aucune région n'a changé (émettre une trame « répétition »/nulle très légère au lieu d'un plein cadre) ;
   - idéalement, restreindre l'encodage aux régions modifiées (ROI) si le backend le permet, sinon documenter la limite et livrer le skip-frame + une heuristique keyframe adaptative.
   - Mesurer le gain (octets/trame sur écran ~statique).
4. **Bench non-régressé** `examples/perf_bench.rs` (ou `benches/` si tu configures criterion — sinon un exemple qui imprime des chiffres) : sur une séquence synthétique déterministe (mire animée + zones statiques), mesurer et imprimer : **fps d'encodage**, **octets/trame moyens** (écran statique vs mouvement), **temps d'encodage moyen (ms)**, et vérifier que l'ABR fait **varier** le débit selon l'estimation réseau. Ajouter des **assertions** (ex. octets/trame statique < X après delta) pour en faire un garde-fou.
5. **NVENC (optionnel, si temps)** : esquisser un backend matériel derrière un `cfg`/feature `nvenc` **sans casser la compilation par défaut** ; sinon, laisser un TODO documenté et le point d'extension. **Ne bloque pas** le reste sur NVENC.

## VÉRIF (obligatoire, chiffres exacts)
- `cargo build -p nd-codec --examples --manifest-path ...` → OK.
- `cargo test -p nd-codec --manifest-path ...` → verts (29 + nouveaux, dont test set_bitrate et test delta/skip-frame).
- `cargo run --example perf_bench -p nd-codec --release --manifest-path ...` → imprime fps, octets/trame (statique vs mouvement), ms/trame ; assertions OK.
- `cargo clippy -p nd-codec --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt -p nd-codec`.
- **Régression** : les exemples `encode_probe`/`mf_encode_probe` compilent et tournent toujours.

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés.
- Chiffres du bench **avant/après** (octets/trame statique surtout), preuve que l'ABR varie le débit.
- Ce qui reste (NVENC ? ROI matériel ?) documenté honnêtement.
- État EXACT des vérifs (tests, clippy 0).
- **Pas de git.**
