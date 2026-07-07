# Prompt Fable 08 — Enregistrement réel + API d'intégration des fonctionnalités (nd-features)

**Priorité : P1** · **Crate ciblée : `crates/nd-features`** · **Parallélisable avec : 01, 02, 05, 06, 07** (crates disjointes).

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : sortir `nd-features` de son statut d'« île non branchée » — rendre l'**enregistrement de session** réellement exploitable (produire un fichier **lisible**), et fournir des **API d'intégration** propres pour que l'orchestrateur (lot 01) puisse **appliquer les permissions**, **alimenter l'enregistreur** et **réagir aux signaux de reconnexion**.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-features\`.
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo parallèle = normal.
- Tu peux **lire** `nd-codec` (`EncodedChunk`, `DecodedFrame`) pour typer les entrées de l'enregistreur, mais **n'édite qu'`nd-features`**.

## BARRE QUALITÉ
- `cargo clippy -p nd-features --all-targets -- -D warnings` = **ZÉRO** (attention `type_complexity`).
- `cargo fmt -p nd-features`.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `nd-features` (103 tests) modules : `permissions` (RÉEL : 12 capacités, deny-par-défaut, `PermissionBroker` request/grant/deny/revoke/authorize + audit), `recording` (**MVP** : conteneur `.ndr` BLAKE3/index, mais `record(data:&[u8])` sérialise des **octets opaques**, aucun encodeur/mux, aucun capteur ne l'alimente), `annotation` (rasteriseur alpha, texte = soulignement), `tunnel` (pipe TCP vers **socket local**, pas la session), `wol` (RÉEL), `privacy` (**STUB d'effet** : calcule des actions, ne touche pas le système), `reconnect` (RÉEL : backoff+jitter, pur calcul), `hotkeys` (MVP : table, pas de dispatch), `invite` (RNG non crypto), `settings` (RÉEL).
- **Constat** : aucune autre crate ne consomme ces API. Ne casse pas les signatures publiques existantes ; **ajoute** des points d'intégration.

## TÂCHE
1. **Enregistrement lisible** (`recording`) : faire produire à `SessionRecorder` un **fichier réellement lisible**. Deux options (choisis, documente) :
   - (a) **Mux des trames H.264** dans un conteneur standard **MKV/MP4** (annexe B → boîtes, ou via une petite lib de mux si déjà dans l'arbre — vérifie `Cargo.lock`), en acceptant des `EncodedChunk` (données H.264 + keyframe flag + timestamp) plutôt que des octets opaques ; OU
   - (b) si le mux complet est trop lourd, écrire un **`.ndr` + convertisseur** qui produit un MKV/MP4 rejouable à partir du flux enregistré, avec un **test** qui vérifie l'entête/piste vidéo valide.
   - Dans les deux cas : API `record_video_chunk(&EncodedChunk)`, en-tête avec dimensions/codec/fps réels, index de keyframes, intégrité BLAKE3 conservée. Fournir un petit **lecteur/validateur** (exemple ou test) qui rouvre le fichier et confirme N trames.
2. **API d'application des permissions** : exposer une surface claire que l'orchestrateur appellera **avant** d'injecter une entrée / d'ouvrir un canal fichiers / audio, p. ex. `PermissionBroker::authorize(capability) -> bool` déjà présent — ajoute des helpers ergonomiques (`is_allowed(Capability)`, mapping `InputEvent` → `Capability` requise) et **documente le contrat d'intégration** (où l'orchestrateur doit brancher les gardes). Ne fais pas l'injection ici (c'est nd-core), mais rends l'application **triviale à câbler**.
3. **Signal de reconnexion** : exposer une API `reconnect` orientée événement (`ReconnectController::on_disconnect()/next_delay()/reset()`) que nd-core peut piloter, documentée. (Le module calcule déjà les délais — enveloppe-le pour l'usage.)
4. **Durcir `invite`** : remplacer le RNG non cryptographique par un **CSPRNG** (`getrandom`/`rand` CSPRNG déjà dans l'arbre) pour les codes QuickSupport. Test de non-régression.
5. (Optionnel si temps) **`privacy`/`hotkeys`** : documenter précisément le point où l'effet système devra être branché (nd-input/nd-capture) sans l'implémenter ici ; laisser des TODO ciblés. **Ne prétends pas que c'est fait.**

## VÉRIF (obligatoire, chiffres exacts)
- `cargo build -p nd-features --examples --manifest-path ...` → OK.
- `cargo test -p nd-features --manifest-path ...` → verts (103 + nouveaux, dont test « fichier d'enregistrement rejouable/valide » et test invite CSPRNG). Reporte le compte.
- Si tu ajoutes un exemple `record_and_replay`, le lancer et confirmer N trames relues.
- `cargo clippy -p nd-features --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt -p nd-features`.
- **Régression** : `nd-features/tests/tunnel.rs` passe toujours.

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés.
- Format d'enregistrement retenu (MKV/MP4/.ndr+convert) + preuve de relecture (N trames).
- Surface d'intégration exposée (permissions/reconnect) et **où nd-core doit la brancher**.
- Ce qui reste stub honnêtement (privacy/hotkeys système).
- État EXACT des vérifs (tests, clippy 0).
- **Pas de git.**
