# Index des prompts Fable — ordre de lancement & parallélisme

Voir [`../00-synthese-et-roadmap.md`](../00-synthese-et-roadmap.md) §3 pour le détail. Règle : **deux agents ne modifient jamais la même crate en même temps** ; un **verrou cargo** transitoire entre agents est **normal**.

| # | Prompt | Prio | Crate(s) | Parallélisable avec | Dépend de |
|---|---|---|---|---|---|
| 01 | Orchestrateur `SessionEngine` | **P0** | `nd-core` | 02, 05, 06, 07, 08, 09 | — |
| 02 | Reskin visuel AnyDesk | **P0** | `ui/` | 01, 05, 06, 07, 08, 09 | — |
| 03 | Façade FFI temps réel | **P0** | `nd-ffi` | 05, 06, 07, 08, 09 | **01** |
| 04 | Câblage session live UI | **P0** | `ui/` | (aucun — même crate que 02) | **03** + **02** |
| 05 | Connectivité NAT réelle | **P0** | `nd-signaling`, `nd-transport` | 01, 02, 06, 07, 08, 09 | — (intégr. 01 en fin) |
| 06 | Codec ABR + delta + bench | **P1** | `nd-codec` | 01, 02, 05, 07, 08, 09 | — |
| 07 | Auth serveurs + attribution ID | **P1** | `nd-rendezvous`, `nd-relay`, `nd-api` | 01, 02, 05, 06, 08, 09 | — |
| 08 | Enregistrement réel + intégration | **P1** | `nd-features` | 01, 02, 05, 06, 07, 09 | — |
| 09 | OIDC RS256/ES256 + SQLite | **P1** | `nd-accounts` | 01, 02, 05, 06, 08 | soft: 07 |
| 10 | Client web | **P2** | `nd-wasm` | tout | — |
| 11 | Packaging/installeurs | **P2** | `packaging/`, CI | tout | ⚠ build = machine admin |
| 12 | macOS/Wayland/multi-écran | **P2** | `nd-capture`, `nd-audio`, `nd-input` | tout | ⚠ valid. sur vraie plateforme |

## Vagues recommandées

- **Vague 1 (parallèle)** : 01, 02, 06, 07, 08 — puis 09 (peut démarrer en même temps).
- **Vague 2** : 03 (après 01), 05 (en parallèle, intégration à la fin).
- **Vague 3** : 04 (après 03 **et** 02).
- **Vague 4 (parallèle, quand capacité dispo)** : 10, 11, 12.

**Chemin critique « voir & contrôler un pair »** : 01 → 03 → 04 (avec 02 en parallèle, requis avant 04).
