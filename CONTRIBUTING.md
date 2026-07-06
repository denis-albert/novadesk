# Contribuer à NovaDesk

Merci de lire cette page avant tout commit. Le projet suit une discipline stricte :
petite surface, zéro avertissement, chaque incrément prouvé.

## Le plan technique fait foi

La conception vit dans [`../plan-technique/`](../plan-technique/) (hors dépôt git).
Toute contribution s'inscrit dans un fichier de plan (01 à 17) : citez-le dans les
commentaires de module (`//! … (voir plan 0X)`) et suivez la
[roadmap](../plan-technique/16-roadmap-planning.md) — pas de fonctionnalité hors
plan sans discussion préalable.

## Barre de qualité (bloquante)

Avant chaque commit, les quatre commandes doivent passer **sans erreur ni
avertissement** (la CI les rejoue sur Windows, Linux et macOS) :

```sh
cargo fmt --all --check                                  # formatage canonique
cargo clippy --workspace --all-targets -- -D warnings    # zéro lint
cargo build --workspace                                  # tout compile
cargo test --workspace                                   # tous les tests passent
```

Astuce : `cargo fmt --all` corrige le formatage en place. Ne désactivez jamais un
lint globalement ; un `#[allow]` local doit être justifié par un commentaire.

## Conventions

### Langue

**Français** partout : documentation, commentaires, messages de commit, sorties des
exemples. Les identifiants de code suivent l'usage Rust (anglais accepté quand c'est
l'idiome, ex. `create_capturer`), mais les explications sont en français.

### `unsafe` : isolé et justifié

- Le workspace déclare `unsafe_code = "warn"` ; seules les crates plateforme
  (FFI Win32, codecs…) le passent en `allow` **localement**, dans des modules dédiés
  (ex. `win_*.rs`, `mediafoundation.rs`).
- **Chaque bloc `unsafe` est précédé d'un commentaire `// SAFETY:`** expliquant
  pourquoi les invariants sont respectés.
- L'API publique d'une crate reste 100 % sûre : l'`unsafe` ne fuit jamais dans les
  signatures.

### Tests requis

Chaque incrément apporte ses tests (`cargo test --workspace` doit croître avec le
code). Les chemins qui exigent du matériel réel (écran, micro, presse-papiers) sont
prouvés par une **sonde** dans `examples/` — nommée `*_probe.rs`, avec un en-tête
`//!` décrivant ce qu'elle prouve et la commande pour la lancer. Les sondes doivent
compiler sur tous les OS (stub `#[cfg(not(windows))]` si besoin) car la CI compile
`--all-targets`.

### Commits

- **Un commit par incrément** : une étape cohérente, compilable et testée — pas de
  commits « wip », pas de méga-commits fourre-tout.
- Message en français, à l'impératif, première ligne ≤ 72 caractères, avec le
  numéro de plan quand il s'applique (ex. `nd-audio : capture micro WASAPI (plan 08)`).

### Dépendances

Les dépendances externes s'ajoutent crate par crate (pas de dépendance « au cas où »
dans le workspace), avec un commentaire dans le `Cargo.toml` expliquant le choix.
Le job `audit` de la CI (cargo-audit) doit rester vert.

### Portabilité

Windows (MSVC), macOS et Linux sont des cibles de premier rang. Le code spécifique
à un OS vit derrière `#[cfg(...)]` et derrière les traits communs (`ScreenCapturer`,
`InputInjector`…) ; les autres OS reçoivent une fabrique qui renvoie
`NdError::NotImplemented` tant que leur backend n'existe pas — le workspace doit
**compiler partout**, en permanence.

## Environnement

- Rust stable ≥ 1.90 (`rust-toolchain.toml` installe rustfmt + clippy).
- CMake ≥ 3.16 (libopus vendoré). Linux : `nasm`, `libasound2-dev`,
  `libpipewire-0.3-dev` (voir `.github/workflows/ci.yml`).
