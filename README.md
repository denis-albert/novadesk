# NovaDesk — code source (squelette)

Implémentation du bureau à distance NovaDesk. Le plan technique complet qui gouverne
ce code se trouve dans [`../plan-technique/`](../plan-technique/) — commencez par
[`00-README.md`](../plan-technique/00-README.md) et
[`01-architecture-globale.md`](../plan-technique/01-architecture-globale.md).

## État

**Phase 0/1 — squelette du workspace.** Cette étape pose la structure de crates, les
**traits d'interface** entre couches et des types partagés. Les implémentations
(capture GPU, codec matériel, transport QUIC, crypto Noise…) sont ajoutées phase par
phase selon [`16-roadmap-planning.md`](../plan-technique/16-roadmap-planning.md).

Le workspace **compile** en l'état (`cargo build`) : les traits sont définis et les
fabriques renvoient `NdError::NotImplemented` là où le code réel manque encore.

## Organisation

| Crate | Rôle | Fichier de plan |
|---|---|---|
| `nd-proto` | Types partagés, versions de protocole, erreurs | 01, 04 |
| `nd-capture` | Trait `ScreenCapturer` + impls OS | 02 |
| `nd-codec` | Traits `VideoEncoder`/`VideoDecoder` | 03 |
| `nd-transport` | Trait `Transport` (QUIC, canaux) | 04 |
| `nd-signaling` | Client rendez-vous, ICE, relais | 05 |
| `nd-crypto` | Session chiffrée (Noise), empreintes/SAS | 06 |
| `nd-input` | Trait `InputInjector` + impls OS | 07 |
| `nd-audio` | Capture/lecture audio (Opus) | 08 |
| `nd-files` | Transfert de fichiers, presse-papiers | 09 |
| `nd-features` | Permissions, fonctionnalités avancées | 13 |
| `nd-core` | Orchestration de session, machine à états | 01 |
| `nd-ffi` | Pont vers l'UI Flutter (flutter_rust_bridge) | 10 |
| `nd-wasm` | Cible WebAssembly (client web) | 12 |
| `server/nd-rendezvous` | Serveur de signalisation | 05, 11 |
| `server/nd-relay` | Serveur de relais | 05, 11 |
| `server/nd-accounts` | Comptes / authentification | 11 |
| `server/nd-api` | Carnet d'adresses, licences, MAJ | 11 |

## Construire

```sh
cargo build          # compile le squelette
cargo test           # exécute les tests unitaires (nd-proto)
cargo run -p nd-rendezvous   # lance un binaire serveur (stub)
```
