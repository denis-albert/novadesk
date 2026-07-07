# 00 — Synthèse exécutive & roadmap de parité AnyDesk

> Audit du dépôt `novadesk` (17 crates Rust + UI Flutter) réalisé en lecture seule le **2026-07-07**. Cible : « exactement aussi performant qu'AnyDesk » + « exactement la même interface ». Documents liés : [`01-analyse-ecarts`](01-analyse-ecarts.md), [`02-performance-anydesk`](02-performance-anydesk.md), [`03-interface-anydesk-exacte`](03-interface-anydesk-exacte.md), prompts dans [`04-prompts-fable/`](04-prompts-fable/).

---

## 1. État global — la vérité sans fard

NovaDesk est un **socle d'ingénierie réel, de bonne qualité et bien testé** (~36 000 lignes Rust, **466 tests unitaires**), mais **ce n'est pas encore une application utilisable comme AnyDesk**. Le fossé n'est **pas** la qualité des briques — c'est l'**intégration verticale**.

**Ce qui est vrai et solide** (pas du vent) :
- Crypto E2E **Noise XX/IK** complète (SAS, pinning TOFU, rekey) — la crate la plus mûre.
- Transport **QUIC (quinn)** + datagrammes non fiables + **FEC Reed-Solomon adaptatif**.
- Rendez-vous par ID + **STUN** + **UDP hole punching** + détection NAT (testés loopback).
- Capture **DXGI** + dirty-rects + curseur ; **H.264** encode/decode fonctionnel ; **audio Opus + WASAPI** ; **transfert BLAKE3** avec reprise ; **injection SendInput** complète ; presse-papiers riche.
- La **tranche verticale marche de bout en bout EN LOOPBACK** : les exemples `nd-core/examples/viewer_window.rs` et `secure_desktop.rs` prouvent capture→encode→QUIC→Noise→décode→**affichage réel** (fenêtre minifb).
- Côté serveurs : modèles **comptes/RBAC/carnet** réels, relais par ticket, Argon2id/TOTP/PKCE.

**Ce qui bloque un usage réel « comme AnyDesk »** (voir [`01`](01-analyse-ecarts.md) §8) :
1. **Le cœur et l'UI ne sont pas connectés.** `nd-ffi` n'expose que des **helpers purs** (formatage d'ID, encodage d'entrées, validation) — **pas de démarrage de session, pas de sortie de frames, pas de flux**. Conséquence : dans l'UI Flutter, la session est **entièrement simulée** (timers codés en dur), la surface vidéo est un **placeholder noir** (`_textureId` toujours `null`), et les entrées sont encodées mais **jamais transmises**.
2. **Pas d'orchestrateur de session.** `nd_core::Session` est une **coquille** (s'arrête à `Resolving`) ; le vrai pipeline n'existe que comme **glu d'exemples**, et `ViewerPipeline` **jette même les pixels décodés**.
3. **Connectivité loopback uniquement.** STUN/punch/relais **non câblés** dans le path de session ; **aucune infra déployée** ; rendez-vous et relais **sans authentification** ; **pas de service d'attribution d'ID**.
4. **Parité visuelle éloignée.** Marque **indigo** (pas le rouge AnyDesk), pas de rendu vidéo, pas de sidebar/vignettes, chrome Material générique, barre d'outils de session incomplète, **pas de dialogue d'acceptation entrante**.
5. **Performance non bouclée et non mesurée.** ABR **réel mais non câblé**, **dirty-rects ignorés** (ré-encodage plein cadre), codec **logiciel** seulement (MF = MFT logiciel MS, **pas NVENC** ; RTX 4080 inutilisée), **aucun bench non-régressé**.
6. **Pas d'application packagée.** `packaging/` **absent** (aucun installeur/signature/MAJ) ; `nd-wasm` **stub**.

**Faux-amis à connaître** (présentés comme finis, en réalité MVP/stub) : l'**enregistrement de session** (conteneur d'octets opaques, aucun encodeur/mux → aucune vidéo lisible), le **mode confidentialité** (calcule des actions mais **ne touche pas le système**), le **tunnel TCP** (branché à un socket local, pas à la session chiffrée), et `nd-features` en général qui est une **île non consommée** par le reste du code.

**Verdict** : **pré-alpha applicatif sur socle réel**. Priorité absolue = **intégration verticale** (cœur↔UI↔réseau) + **parité visuelle**, avant toute nouvelle fonctionnalité.

---

## 2. Priorisation

- **P0 — bloquant pour un produit utilisable comme AnyDesk** : orchestrateur de session ; pont FFI temps réel (état/frames/entrées) ; rendu vidéo dans l'UI ; reskin visuel AnyDesk ; connectivité par ID hors loopback + auth d'infra + attribution d'ID.
- **P1 — crédibilité « classe AnyDesk »** : ABR + dirty-rects + bench perf ; application effective des permissions + dialogue d'acceptation ; enregistrement réel ; auth serveurs complète ; OIDC réel + DB.
- **P2 — parité étendue / dépassement** : NVENC + capture zéro-copie ; client web ; packaging/signature/MAJ ; macOS SCK, Wayland, multi-écran ; whiteboard/tunnel-sur-session/privacy système.

---

## 3. Roadmap priorisée & ordre de lancement des lots Fable

Les prompts complets sont dans [`04-prompts-fable/`](04-prompts-fable/). **Règle de parallélisme** : deux agents ne doivent pas éditer la **même crate** en même temps (le **verrou cargo est normal** et attendu ; c'est l'édition concurrente des mêmes fichiers qu'on évite).

### Vague 1 — fondations P0 (lancer en parallèle, crates disjoints)

| Prompt | Priorité | Crate(s) éditée(s) | Parallélisable avec | Dépend de |
|---|---|---|---|---|
| `01-nd-core-orchestrateur-session` | P0 | `crates/nd-core` | 02, 06, 07, 08 | — |
| `02-ui-reskin-anydesk` | P0 | `ui/` | 01, 05, 06, 07, 08 | — (Flutter pur) |
| `06-nd-codec-abr-dirtyrects-bench` | P1 | `crates/nd-codec` | 01, 02, 07, 08 | — |
| `07-serveurs-auth-id-allocation` | P1 | `server/nd-rendezvous`, `nd-relay`, `nd-api` | 01, 02, 06, 08 | — |
| `08-nd-features-integration-recording` | P1 | `crates/nd-features` | 01, 02, 06, 07 | — |

### Vague 2 — câblage P0 (séquentiel sur dépendances)

| Prompt | Priorité | Crate(s) | Dépend de |
|---|---|---|---|
| `03-nd-ffi-streaming-session` | P0 | `crates/nd-ffi` | **01** (SessionEngine) |
| `05-connectivite-nat-reelle` | P0 | `crates/nd-signaling`, `nd-transport` | soft : 01 (peut démarrer en parallèle, intégration à la fin) |
| `09-nd-accounts-oidc-persistence` | P1 | `server/nd-accounts` | soft : 07 (crates disjoints, câblage jetons ensuite) |

### Vague 3 — intégration UI live P0 (séquentiel, même crate `ui/`)

| Prompt | Priorité | Crate | Dépend de |
|---|---|---|---|
| `04-ui-cablage-session-live` | P0 | `ui/` | **03** (FFI streaming) **et 02** (reskin) — car même crate `ui/` que 02 |

### Vague 4 — parité étendue P2 (parallèle)

| Prompt | Priorité | Crate(s) | Parallélisable |
|---|---|---|---|
| `10-nd-wasm-client-web` | P2 | `crates/nd-wasm` | oui |
| `11-packaging-installeurs` | P2 | `packaging/` (+ CI) | oui (⚠ build natif = machine avec droits admin) |
| `12-multiplateforme-macos-wayland` | P2 | `crates/nd-capture`, `nd-audio`, `nd-input` | oui |

> **Chemin critique P0 « voir & contrôler un pair »** : `01 → 03 → 04` (orchestrateur → FFI streaming → UI). Le `02` (reskin) et `05/06/07/08` tournent **en parallèle** de ce chemin. `04` attend `02` **et** `03` car il touche la même crate `ui/` que `02`.

---

## 4. Estimation d'effort (ordre de grandeur, agent Fable senior)

| Lot | Effort | Note |
|---|---|---|
| 01 orchestrateur nd-core | **L** (élevé) | threading, cycle de vie, frames, instrumentation |
| 02 reskin UI | **M** | thème + sidebar + vignettes + toolbar + dialog |
| 03 FFI streaming | **M** | StreamSink FRB + régénération pont |
| 04 UI câblage live | **M/L** | rendu RGBA sans plugin natif (repli CPU) |
| 05 connectivité NAT | **L** | orchestration STUN/punch/relais + validation |
| 06 codec ABR/delta/bench | **L** | câblage ABR + dirty-rects + set_bitrate + bench |
| 07 auth serveurs + ID | **M/L** | signature tickets + attribution ID + jetons |
| 08 features intégration | **M** | recording réel + hooks permissions/reconnect |
| 09 accounts OIDC/DB | **M** | RS256/ES256 + JWKS + sqlite |
| 10 wasm | **L** | client web quasi from scratch |
| 11 packaging | **M** | installeurs + signature (hors machine actuelle) |
| 12 multiplateforme | **L** | SCK + Wayland + multi-écran |

**Jalon « démo utilisable en LAN »** (voir un écran distant fluide et le contrôler, sur le réseau local, look AnyDesk) : lots **01+02+03+04+06** = le minimum pour une **vraie démo**. **Jalon « pair distant Internet »** : + **05+07**. **Jalon « produit installable »** : + **09+11**.

---

## 5. Décisions/risques majeurs à arbitrer AVANT lancement

1. **Identité de marque : cloner AnyDesk à l'identique (rouge #EF443B, chrome custom) vs. garder la marque NovaDesk (indigo, plan 10) ?** La demande « exactement la même interface » **contredit** le plan technique existant. Les livrables `03`/`02` supposent le **clone visuel**. À trancher : clone pixel, ou « inspiré d'AnyDesk » sous marque NovaDesk. *(Risque juridique/identité si clone total du logo/rouge.)*

2. **Rendu vidéo sans plugin natif.** La cible « texture GPU zéro-copie » (`irondash_texture`) est un **plugin natif → impossible sur ce poste** (pas d'admin/symlinks). Repli proposé : **streamer les frames RGBA via FRB + `RawImage`** (CPU, perf moindre). À valider : accepte-t-on le repli CPU pour la démo ici, en réservant la texture GPU à une machine de build avec droits ? *(Impacte le KPI latence/CPU.)*

3. **Codec matériel (NVENC) : maintenant ou plus tard ?** Le pari « matériel d'abord » du plan n'est **pas tenu** (MF = logiciel). NVENC donnerait le 60 fps 4K à faible CPU (différenciateur réel), mais c'est **L** d'effort. Le P0/P1 vise d'abord la parité **logicielle** (ABR+delta). À confirmer : NVENC en P1 ou P2 ?

4. **Périmètre « pair distant réel ».** Atteindre un pair sur Internet exige non seulement du code (05/07) mais **l'hébergement d'une infra** (rendez-vous/relais/STUN publics) et de l'**auth**. Décide-t-on d'un environnement de déploiement (VPS/conteneurs) maintenant, ou reste-t-on en **LAN/loopback** pour la démo ? *(Sans infra hébergée, la parité « connexion par ID sur Internet » reste théorique.)*

5. **Honnêteté des revendications de perf.** On ne pourra pas prouver « < 16 ms » (ni AnyDesk d'ailleurs). Valide-t-on la position « **KPI mesurés et honnêtes** » (< 30 ms LAN prouvé) plutôt que des chiffres marketing ? Cela conditionne le bench du lot `06`.

---

## 6. Où lire quoi

- Écarts détaillés par crate → [`01-analyse-ecarts.md`](01-analyse-ecarts.md)
- Perf (leviers → KPI → actions) → [`02-performance-anydesk.md`](02-performance-anydesk.md)
- Interface exacte + tableau d'écart écran par écran → [`03-interface-anydesk-exacte.md`](03-interface-anydesk-exacte.md)
- Prompts Fable prêts à lancer → [`04-prompts-fable/`](04-prompts-fable/)
- Référence factuelle AnyDesk → [`../17-anydesk-realite.md`](../17-anydesk-realite.md)
