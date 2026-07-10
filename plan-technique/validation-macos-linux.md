# Validation des backends macOS / Linux — capture, entrées, audio

> **But de ce document.** Le développement se fait sur un poste **Windows**. Le
> code des cibles a été écrit et durci par **revue de code** (contre l'API réelle
> des bibliothèques) et couvert, pour sa **logique portable**, par des tests
> agnostiques exécutés sur Windows. Ce document liste, backend par backend, **ce
> qui reste impérativement à valider sur une vraie machine** (prérequis, commandes
> de build cible, scénarios, pièges).
>
> Périmètre : `nd-capture`, `nd-input`, `nd-audio`. Voir les plans 02 (capture),
> 07 (entrées) et 08 (audio).

> ### Ce qui se **vérifie déjà depuis Windows** (type-check croisé, sans exécution)
>
> Les backends **100 % Rust** se **type-checkent en cross** depuis ce poste (les
> cibles `aarch64-apple-darwin` et `x86_64-unknown-linux-gnu` sont installées) —
> `cargo clippy` ne **lie** pas, il n'exige donc pas de linker/framework cible :
>
> ```bash
> cargo clippy -p nd-capture -p nd-input --target aarch64-apple-darwin      --all-targets -- -D warnings
> cargo clippy -p nd-capture -p nd-input --target x86_64-unknown-linux-gnu  --all-targets -- -D warnings
> ```
>
> **Vérifié** : ces quatre combinaisons passent à **0 warning** (macOS
> CoreGraphics/Quartz, Linux X11/XTEST/uinput). Cela garantit que le code des
> cibles **compile et passe clippy**, mais **pas** qu'il se comporte correctement
> à l'exécution (permissions, pixels réels, injection effective).
>
> Ce qui **ne se cross-check PAS** ici (dépendances **C** à lier / `pkg-config`
> cible) — revue de code + validation sur machine obligatoires :
> - **`nd-audio`** en entier (`libopus_sys` via CMake/MSVC, `libpulse-sys`) ;
> - **`nd-capture --features wayland-pipewire`** (`libpipewire`/`libspa` via
>   `pkg-config`). Les parties portail (`ashpd`/`zbus`) sont du Rust pur mais la
>   feature entière échoue tant que `pipewire-sys` ne trouve pas la lib C.

---

## 1. Matrice d'état des backends

Légende : **OK-revue** = implémenté, vérifié par revue + tests agnostiques, jamais
exécuté sur cible • **Partiel** = fonctionnel mais limites connues • **Stub** =
renvoie `NotImplemented` volontairement • **Absent** = non écrit.

### nd-capture (écran)

| Cible | Voie | État | Notes |
|---|---|---|---|
| macOS | CoreGraphics `CGDisplayCreateImage` (`macos.rs`) | OK-revue | Modèle « tirer », BGRA, région pleine image. ScreenCaptureKit (zéro-copie, dirty-rects) prévu ensuite. |
| macOS | Curseur (position via `CGEvent`) | Partiel | Position seule ; forme du curseur (`capture_cursor_shape`) = `NotImplemented` hors Windows. |
| Linux X11 | `x11rb` `GetImage` ZPixmap (`linux.rs`) | OK-revue | RandR multi-écran, repli racine. Pas de dirty-rects (XDamage plus tard). |
| Linux Wayland | Portail xdg + PipeWire (`linux_portal.rs`, `linux_pipewire.rs`) | Partiel (feature) | Derrière `--features wayland-pipewire` (lie `libpipewire`, non compilable ici). Chemin CPU (MemFd/MemPtr) ; DMA-BUF sauté. Sous-région non gérée. |
| Linux Wayland | Sans la feature | Stub | `WAYLAND_DISPLAY` sans `DISPLAY` ⇒ `NotImplemented` honnête. |

### nd-input (entrées)

| Cible | Voie | État | Notes |
|---|---|---|---|
| macOS | Quartz `CGEventPost` (`macos.rs`) | OK-revue | Souris (abs/rel/drag), boutons X1/X2, molette 2 axes, clavier, Unicode. **Permission Accessibilité (TCC) requise**. |
| Linux X11 | XTEST (`linux.rs`) | OK-revue | Souris, molette (boutons 4-7), clavier evdev+8, Unicode par remappage keysym. |
| Linux Wayland | `/dev/uinput` (`uinput.rs`) | Partiel | Niveau noyau. **Droits `/dev/uinput` requis**. Ciblage mono-écran seulement ; Unicode `NotImplemented` (niveau keycode). |
| Linux Wayland | Portail RemoteDesktop + libei | Absent | Voie « intégrée bureau » future (voir `uinput.rs`). |

### nd-audio

| Cible | Voie | État | Notes |
|---|---|---|---|
| macOS | Lecture CoreAudio AUHAL (`macos.rs`) | OK-revue | AudioUnit default output, file bornée 500 ms. |
| macOS | Capture système ScreenCaptureKit (`macos.rs`) | OK-revue | macOS **13+** (`capturesAudio`) ; avant ⇒ `NotImplemented`. **Consentement Enregistrement de l'écran**. |
| macOS | Capture micro | Stub | AUHAL entrée + TCC micro à écrire (`create_microphone_capturer` ⇒ `NotImplemented`). |
| Linux | PulseAudio simple API (`linux.rs`) | OK-revue | Loopback `@DEFAULT_MONITOR@`, micro source défaut, lecture. Marche sous PipeWire via `pipewire-pulse`. Lie `libpulse`. |

---

## 2. macOS

### 2.1 Prérequis machine

- macOS **13 (Ventura) ou plus** recommandé (capture audio ScreenCaptureKit ;
  la capture écran CoreGraphics et l'injection Quartz marchent avant, mais viser 13+).
- **Xcode Command Line Tools** : `xcode-select --install`.
- **Rust** cible native : `rustup target add aarch64-apple-darwin` (Apple Silicon)
  ou `x86_64-apple-darwin` (Intel).
- Aucune bibliothèque tierce à installer : `core-graphics`, `coreaudio-rs`, les
  crates `objc2-*` et `block2`/`dispatch2` encapsulent le FFI système.

### 2.2 Permissions TCC (le piège n°1 de macOS)

Les API réussissent **silencieusement à vide** sans consentement. À accorder dans
**Réglages Système → Confidentialité et sécurité** pour le binaire (ou le
terminal parent lors des tests) :

| Fonction | Permission | Symptôme si absente |
|---|---|---|
| Capture écran (`nd-capture`) | **Enregistrement de l'écran** | `CGDisplayCreateImage` renvoie `None` ou seulement le fond d'écran. |
| Capture audio système (`nd-audio` SCK) | **Enregistrement de l'écran** | Démarrage SCK refusé / aucun `CMSampleBuffer`. |
| Injection d'entrées (`nd-input`) | **Accessibilité** | Événements postés **ignorés sans erreur**. |
| Capture micro (à venir) | **Microphone** | — |

> La **détection** (`AXIsProcessTrusted`, etc.) et le guidage utilisateur relèvent
> de l'app hôte (plan 07 §macOS) — hors périmètre de ces crates.

### 2.3 Build cible

```bash
# Compilation + lints (sur le Mac)
cargo clippy -p nd-capture -p nd-input -p nd-audio --all-targets -- -D warnings
cargo build  -p nd-capture -p nd-input -p nd-audio
cargo test   -p nd-capture -p nd-input -p nd-audio
```

> `nd-audio` embarque **libopus** vendoré (`libopus_sys`, build **CMake ≥ 3.16**).
> Prérequis : `brew install cmake`. C'est aussi la raison pour laquelle nd-audio
> **ne compile pas en cible croisée depuis Windows** (CMake/MSVC ne cible pas
> arm64-apple) — contrairement à `nd-capture`/`nd-input` qui se type-checkent en
> cross (`--target aarch64-apple-darwin`, voir l'encart en tête de document).

### 2.4 Scénarios de test

1. **Énumération écrans** : `cargo run -p nd-capture --example monitors_probe` →
   vérifier nombre d'écrans, dimensions **en pixels** (Retina : 2× les points),
   position, drapeau principal.
2. **Capture** : `capture_probe` → une frame BGRA non nulle ; sur écran Retina,
   `width`/`height` = pixels physiques ; vérifier le recadrage `set_region`
   (jamais de fuite hors cadre).
3. **Curseur** : `cursor_probe` → position cohérente après conversion points→pixels
   (facteur d'échelle Retina).
4. **Injection** (après Accessibilité) : `input_probe` → déplacement absolu
   multi-écran, glisser (bouton tenu ⇒ événements *Dragged*), molette 2 axes,
   frappe Unicode (accents, emoji).
5. **Audio lecture** : jouer une trame de silence puis un sinus décodé (le test
   `creation_du_lecteur_et_trame_de_silence` le fait à vide sans paniquer).
6. **Audio capture système** (macOS 13+, Enregistrement de l'écran) : lancer une
   source sonore, vérifier des paquets Opus non silencieux à cadence 20 ms.

### 2.5 Pièges connus (recensés dans le code)

- **`SckSystemCapturer` < macOS 13** : renvoie `NotImplemented` (repli
  BlackHole/Loopback hors périmètre). Confirmer le message.
- **Fuite ponctuelle sur timeout SCK** (`macos.rs`, `decouvrir_contenu`,
  `NOTE (revue)`) : si la découverte expire (5 s) puis le rappel arrive, un
  `SCShareableContent` retenu n'est jamais réclamé. Chemin d'erreur rare, assumé ;
  à revoir sur machine.
- **`extraire_stereo`** : la structure `AblDeux` (2 `AudioBuffer`) suppose au plus
  2 tampons (mono/stéréo, planaire ou entrelacé). Vérifier sur un vrai
  `CMSampleBuffer` audio SCK (agencement `mNumberBuffers`, `mNumberChannels`).
- **Format micro** (à écrire) : AUHAL entrée + `kAudioOutputUnitProperty_EnableIO`
  + TCC micro.

---

## 3. Linux — X11 (backend par défaut, 100 % Rust)

### 3.1 Prérequis

- Serveur **X11** (ou **XWayland** : `DISPLAY` défini) ; extension **XTEST** pour
  l'injection ; **RandR ≥ 1.5** pour le multi-écran (repli racine sinon).
- `nd-audio` : **`libpulse`** côté système — `libpulse-dev`
  (Debian/Ubuntu) / `pulseaudio-libs-devel` (Fedora). Fonctionne aussi sous
  PipeWire via `pipewire-pulse`.
- `nd-audio` : **CMake ≥ 3.16** (libopus vendoré).
- Rust : cible native `x86_64-unknown-linux-gnu` (ou `aarch64-…`).

> `nd-capture` (X11) et `nd-input` (XTEST + uinput) sont **100 % Rust** (`x11rb`,
> `libc`) : ils se **compilent en cross depuis Windows** pour vérification —
> `cargo clippy -p nd-capture -p nd-input --target x86_64-unknown-linux-gnu` (sans
> exécution). `nd-audio` (libpulse + libopus) ne se cross-compile pas aussi
> simplement : viser une vraie machine.

### 3.2 Build cible

```bash
cargo clippy -p nd-capture -p nd-input -p nd-audio --all-targets -- -D warnings
cargo test   -p nd-capture -p nd-input -p nd-audio
```

### 3.3 Scénarios de test (session X11)

1. **Multi-écran** : `monitors_probe` → un `MonitorInfo` par sortie RandR, noms
   d'atomes (`DP-1`, `eDP-1`), positions cohérentes. Débrancher un écran :
   `enumerate_monitors` doit refléter le changement au prochain appel.
2. **Capture** : `capture_probe` sur chaque écran → BGRA correct. **Vérifier
   spécifiquement les profondeurs 24 et 32 bpp** et un `scanline_pad` non trivial
   (le calcul de stride est couvert par les tests `stride_zpixmap_*`, mais la
   décodage complet ZPixmap + masques du *visual* est à confirmer visuellement).
3. **Sous-région** (`set_region`) : recadrage correct, jamais de fuite hors cadre.
4. **Injection** : `input_probe` → souris absolue par moniteur, molette
   (crans → boutons 4-7), Unicode (remappage keysym d'un keycode libre, restauré).
5. **Audio** : loopback `@DEFAULT_MONITOR@` (jouer un son, vérifier des paquets),
   micro, lecture.

### 3.4 Pièges connus

- **XWayland** : la capture X11 peut ne montrer **que les fenêtres X11** (pas les
  fenêtres Wayland natives) selon le compositeur → passer au backend Wayland
  (§4) pour une capture complète.
- **XTEST sous compositeur Wayland** : souvent refusé (pas d'accès pointeur/clavier
  globaux) → uinput (§5).
- **Table de keysyms pleine** : `unicode` échoue proprement si aucun keycode libre
  (`trouve_keycode_libre` renvoie `None`) — rare, à confirmer.

---

## 4. Linux — Wayland capture (PipeWire + portail, **feature `wayland-pipewire`**)

> **Désactivé par défaut** : lie `libpipewire` (bibliothèque **C**), non vérifiable
> en cross depuis Windows. À activer et valider sur un vrai Linux Wayland.

### 4.1 Prérequis

- **`libpipewire-0.3-dev`** (Debian/Ubuntu) / `pipewire-devel` (Fedora), pkg-config.
- **`xdg-desktop-portal`** + un backend ScreenCast : `xdg-desktop-portal-wlr`
  (wlroots/Sway), `-gnome` (GNOME) ou `-kde` (KDE).
- Session **Wayland** active (`WAYLAND_DISPLAY` défini).
- Versions ciblées par le code (à confirmer) : **pipewire-rs 0.10**, **ashpd 0.13**.

### 4.2 Build cible

```bash
cargo build -p nd-capture --features wayland-pipewire
cargo clippy -p nd-capture --features wayland-pipewire --all-targets -- -D warnings
```

### 4.3 Scénarios de test

1. **Négociation portail** : au `create_capturer` en session Wayland pure, une
   **boîte de consentement** de sélection de source apparaît. Refuser ⇒
   `NdError::Capture` propre.
2. **Flux** : accepter un moniteur → frames BGRA à la cadence négociée ; timeout ⇒
   frame vide (`image: None`) sans blocage.
3. **Arrêt** : `stop()` termine le thread PipeWire et **libère la session**
   (le nœud disparaît de `pw-top`).
4. **Curseur** : mode `Embedded` (incrusté) vs `Hidden` selon `capture_cursor`.

### 4.4 Points d'API à confirmer (marqués `// NOTE (à valider sur Linux)`)

- `linux_pipewire.rs` : API **possédée** pipewire-rs 0.10 (`MainLoopRc`,
  `ContextRc`, `StreamBox`, `connect_fd_rc`) ; `format_utils::parse_format` ;
  signatures des callbacks `state_changed`/`param_changed`/`process`.
- `linux_portal.rs` : builder `SelectSourcesOptions` (setters `set_*`) d'ashpd
  0.13 ; `Stream::size()` (`Option`) ; `SessionKeepAlive` **`Send`** (déplacement
  vers le thread PipeWire) — si une version rend les types `!Send`, garder la
  session sur le thread appelant.

### 4.5 Limites assumées

- **DMA-BUF sauté** : buffers GPU (`Data::data()` = `None`) ignorés dans ce chemin
  CPU ; zéro-copie GPU = jet ultérieur.
- **Sous-région** : `set_region(Some(..))` ⇒ `NotImplemented` (recadrage logiciel
  côté consommateur à faire).
- **Curseur hors bande** : `CursorMode::Metadata` (position via `spa_meta_cursor`)
  non exploité — `CursorState` reste `None`.
- **`enumerate_monitors`** en Wayland pur reste `NotImplemented` **même avec la
  feature** : la sélection de source passe par le dialogue du portail.

---

## 5. Linux — Wayland injection (`/dev/uinput`)

### 5.1 Prérequis / droits (piège n°1)

- Module noyau **`uinput`** chargé : `sudo modprobe uinput`.
- Accès à **`/dev/uinput`** : `root`, ou membre du groupe autorisé par une **règle
  udev** de déploiement, p. ex. :
  ```
  KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
  ```
  puis ajouter l'utilisateur au groupe `input`. Sans droits, `UinputInjector::new`
  renvoie une **erreur claire** (jamais de panique) et `create_injector` retombe
  sur XTEST si un serveur X est joignable.

### 5.2 Scénarios de test

1. **Création sans droits** : le test `creation_sans_panique` doit échouer
   proprement (déjà couvert, sans droits).
2. **Avec droits** : le périphérique **`NovaDesk Virtual Input`** apparaît dans
   `libinput list-devices` / `sudo libinput debug-events`.
3. **Souris/clavier/molette** : déplacement absolu, boutons, molette V/H, touches
   evdev. `release_all` relâche tout (anti « stuck key »).
4. **`Drop`** : le périphérique disparaît (`UI_DEV_DESTROY`).

### 5.3 Pièges connus / à valider

- **Périphérique ABS + REL simultané** (`configurer` déclare `EV_ABS` **et**
  `EV_REL`) : certains `libinput`/compositeurs classent mal un tel hybride
  (tablette vs souris) ou ignorent un des types d'axes. **À observer sur cible** ;
  si problème, envisager deux périphériques distincts (un absolu, un relatif).
- **Ciblage par moniteur** : la plage absolue `0..=65535` est étalée sur **tout le
  bureau virtuel** ; correct en mono-écran, à affiner en multi-écran (géométrie
  `wl_output`/portail inconnue d'un uinput isolé). `mouse_move_abs` ignore donc
  `monitor` (documenté).
- **Unicode** : `NotImplemented` au niveau evdev (voie propre = portail
  RemoteDesktop + libei).

---

## 6. Rappel — ce qui est déjà verrouillé par les tests agnostiques (Windows)

La **logique portable** des backends macOS/Linux est extraite dans des modules
compilés et testés **sur toutes les plateformes**, y compris Windows où les
backends cibles ne compilent pas. Ces tests protègent contre les régressions
même sans machine cible :

| Module | Logique couverte | Backends protégés |
|---|---|---|
| `nd-capture::pixel` | Décodage pixel ZPixmap (`valeur_pixel`, `canal`), **stride ZPixmap** (`stride_zpixmap`), conversion RGB32→BGRA (`convertit_bgra`) | Linux X11 + PipeWire |
| `nd-capture::clamp_region` | Bornage sous-région (« cadre d'écran »), garantie anti-fuite | macOS, Linux, Windows |
| `nd-input::screen` | Projection multi-écran normalisé→absolu, normalisation 65535 | macOS, Linux, Windows |
| `nd-input::keysym` | Caractère Unicode → keysym X11 | Linux XTEST |
| `nd-input::uinput` (tests) | Encodage ioctl `_IOC`, quantification absolue, codes boutons | Linux uinput |
| `nd-audio::convert` | Décodage PCM i16/i32/f32↔octets, mixage stéréo/planaire, rééchantillonnage | tous |
| `nd-audio::codec` | Taille de trame, **horloge média A/V** (`horodatage_media_us`) | tous |

**Ce qui NE peut PAS être testé sans machine** : tout appel réel aux API système
(CoreGraphics/Quartz/ScreenCaptureKit, X11/XTEST/uinput, PipeWire/portail,
PulseAudio/CoreAudio), donc la **capture réelle de pixels**, l'**injection
effective**, la **capture/lecture audio réelle**, et les **permissions**
(TCC macOS, droits uinput, consentement portail). C'est l'objet des scénarios
ci-dessus.

### Commande de vérification (poste Windows)

```bash
cargo clippy -p nd-capture -p nd-input -p nd-audio --all-targets -- -D warnings
cargo fmt   -p nd-capture -p nd-input -p nd-audio -- --check
cargo test  -p nd-capture -p nd-input -p nd-audio
```
