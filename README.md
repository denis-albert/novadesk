# NovaDesk

NovaDesk est un logiciel de **bureau à distance** (type AnyDesk/RustDesk) écrit en
**Rust** : capture d'écran, encodage vidéo H.264, transport temps réel sur QUIC,
chiffrement de bout en bout (Noise), injection d'entrées, audio Opus, transfert de
fichiers et presse-papiers partagé — plus les services serveur (rendez-vous, relais,
comptes, API).

Le **plan technique complet** qui gouverne ce code se trouve dans
[`../plan-technique/`](../plan-technique/) — commencez par
[`00-README.md`](../plan-technique/00-README.md) et
[`01-architecture-globale.md`](../plan-technique/01-architecture-globale.md).
Chaque crate cite le fichier de plan qui la spécifie.

## État

Le pipeline réel fonctionne de bout en bout en local sur **Windows** : capture DXGI →
encodage H.264 (Media Foundation ou openh264) → QUIC → décodage → affichage, session
chiffrée Noise XX par-dessus QUIC, boucle de contrôle clavier/souris, audio WASAPI →
Opus, transfert de fichiers BLAKE3 avec reprise, presse-papiers riche, et connexion
**par ID** via le serveur de rendez-vous. Les backends macOS/Linux (plans 02, 07, 08,
12), le NAT traversal complet (plan 05) et l'UI Flutter (plan 10) suivent la
[roadmap](../plan-technique/16-roadmap-planning.md).

Le workspace compte 17 crates (~20 000 lignes) et plus de 300 tests automatisés ;
Clippy strict (`-D warnings`) passe à zéro avertissement.

## Organisation du workspace

| Crate | Rôle | Plan |
|---|---|---|
| `crates/nd-proto` | Types partagés, versions de protocole, erreurs communes | 01, 04 |
| `crates/nd-capture` | Capture d'écran : trait `ScreenCapturer`, impl. DXGI (multi-écran, curseur) | 02 |
| `crates/nd-codec` | Codec vidéo : traits `VideoEncoder`/`VideoDecoder`, Media Foundation + openh264 | 03 |
| `crates/nd-transport` | Transport temps réel : canaux logiques sur QUIC (quinn), FEC Reed-Solomon | 04 |
| `crates/nd-signaling` | Client de rendez-vous, ICE/STUN/TURN, mise en relation P2P | 05 |
| `crates/nd-crypto` | Session chiffrée de bout en bout (Noise XX via snow), empreintes/SAS | 06 |
| `crates/nd-input` | Injection clavier/souris/tactile : trait `InputInjector`, impl. `SendInput` | 07 |
| `crates/nd-audio` | Capture/lecture audio (WASAPI loopback + micro), codec Opus | 08 |
| `crates/nd-files` | Transfert de fichiers (chunks BLAKE3, reprise), presse-papiers riche | 09 |
| `crates/nd-features` | Permissions de session, fonctionnalités avancées | 13 |
| `crates/nd-core` | Orchestration de session : machine à états, assemblage du pipeline | 01 |
| `crates/nd-ffi` | Pont vers l'UI Flutter (flutter_rust_bridge) | 10 |
| `crates/nd-wasm` | Cible WebAssembly (client web visualiseur) | 12 |
| `server/nd-rendezvous` | Serveur de signalisation / rendez-vous (résolution par ID) | 05, 11 |
| `server/nd-relay` | Serveur de relais (tuyau chiffré aveugle, repli quand le P2P échoue) | 05, 11 |
| `server/nd-accounts` | Comptes / authentification (Argon2id, TOTP) | 11 |
| `server/nd-api` | API applicative : carnet d'adresses, licences, mises à jour | 11, 15 |

Vue d'ensemble du pipeline et des dépendances entre crates :
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Construire et tester

Prérequis : Rust stable (≥ 1.90, voir `rust-toolchain.toml`), CMake ≥ 3.16 (build de
libopus vendoré). Sous Linux, installer aussi : `nasm`, `libasound2-dev`,
`libpipewire-0.3-dev` (voir [`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

```sh
cargo build --workspace     # compile les 17 crates
cargo test --workspace      # exécute la suite de tests
cargo run -p nd-rendezvous  # lance un binaire serveur
```

### Barre de qualité

Les quatre commandes suivantes doivent passer **sans erreur ni avertissement** avant
tout commit (elles sont rejouées par la CI sur Windows, Linux et macOS) :

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

Voir [`CONTRIBUTING.md`](CONTRIBUTING.md) pour les conventions du projet.

## Démos exécutables (« sondes »)

Chaque étape validée est prouvée par un exemple exécutable :
`cargo run --example <nom> -p <crate>`. Les sondes marquées **W** exigent Windows
(backends DXGI/WASAPI/SendInput — les autres OS viendront avec les plans 02/07/08) ;
celles marquées `--release` sont sensibles à la vitesse de l'encodeur logiciel.

| Exemple | Crate | OS | Description |
|---|---|---|---|
| `capture_probe` | `nd-capture` | W | Capture quelques frames de l'écran principal (DXGI) et affiche leurs caractéristiques. |
| `monitors_probe` | `nd-capture` | W | Énumère les moniteurs attachés au bureau et capture une frame de chacun. |
| `cursor_probe` | `nd-capture` | W | Capture le bitmap RGBA du curseur courant (dimensions, hotspot, pixels opaques). |
| `encode_probe` | `nd-codec` | W, `--release` | Capture l'écran, encode chaque frame en H.264, affiche taille/type/compression puis re-décode le flux. |
| `mf_encode_probe` | `nd-codec` | W | Encode des frames synthétiques via le MFT H.264 Media Foundation, puis re-décode via openh264. |
| `loopback` | `nd-transport` | tous | Bouclage QUIC local : serveur et client échangent des messages multiplexés (canaux vidéo + input). |
| `input_probe` | `nd-input` | W | Déplace le curseur vers des cibles vérifiées (`GetCursorPos`), teste molette et saisie Unicode, puis restaure la position. |
| `audio_probe` | `nd-audio` | W | Capture ~1 s de l'audio système (loopback WASAPI), encode en Opus puis redécode. |
| `mic_probe` | `nd-audio` | W | Capture ~1 s du microphone, encode en Opus profil voix (DTX) puis redécode. |
| `files_probe` | `nd-files` | tous | Plan de chunks BLAKE3 + reprise sur un fichier temporaire, listing `LocalFs`, aller-retour texte du presse-papiers (Windows). |
| `clipboard_probe` | `nd-files` | W | Aller-retour d'une image `CF_DIB` et lecture de la liste de fichiers copiés (`CF_HDROP`), contenu précédent restauré. |
| `fsops_probe` | `nd-files` | tous | Opérations d'écriture `RemoteFs` confinées (`LocalFs::jailed`) : mkdir, copie, renommage, tentatives d'évasion refusées. |
| `vertical_slice` | `nd-core` | W, `--release` | Tranche verticale complète : un hôte capture/encode/envoie sur QUIC, un viewer reçoit/décode. |
| `viewer_window` | `nd-core` | W, `--release` | Fenêtre de démo (minifb) affichant en direct l'écran capturé, reçu et décodé depuis QUIC. |
| `control_loop` | `nd-core` | W, `--release` | Boucle bidirectionnelle sur une connexion QUIC : vidéo dans un sens, entrées clavier/souris dans l'autre. |
| `e2e_session` | `nd-core` | tous | Handshake Noise XX par-dessus QUIC, échange de messages chiffrés de bout en bout, empreintes croisées vérifiées. |
| `connect_by_id` | `nd-core` | tous | Connexion **par ID** : un rendez-vous local résout l'ID en adresse + certificat, le viewer se connecte en QUIC. |

## CI, packaging, contribution

- **CI** : [`.github/workflows/ci.yml`](.github/workflows/ci.yml) — matrice
  Windows/Linux/macOS, barre de qualité complète, audit RustSec, couverture
  indicative.
- **Release** : [`.github/workflows/release.yml`](.github/workflows/release.yml) —
  squelette déclenché sur tag `v*` (build `--release`, artefacts par OS).
- **Packaging** : [`packaging/README.md`](packaging/README.md) — notes MSI/MSIX,
  .dmg/notarisation, .deb/.rpm/AppImage/Flatpak, alignées sur le
  [plan 15](../plan-technique/15-deploiement-mise-a-jour.md).
- **Contribuer** : [`CONTRIBUTING.md`](CONTRIBUTING.md).
