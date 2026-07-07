# 02 — Performance AnyDesk : leviers → exigences chiffrées NovaDesk → écarts → actions

> **Cadrage honnête.** AnyDesk ne publie pas de chiffres instrumentés côté client ; ses valeurs (60 fps, < 16 ms LAN, ~100 kb/s) sont **marketing**, et le seul benchmark substantiel est **auto-commandité** (ScienceSoft 2020, client v6.0.7) — voir [`17-anydesk-realite.md`](../17-anydesk-realite.md) §10. NovaDesk transforme cela en avantage : **KPI mesurés en continu et non-régressés**. Ce fichier traduit chaque levier AnyDesk en **exigence chiffrée et vérifiable**, puis liste l'écart réel et l'action.
>
> **Correction de fond capitale ([`17`](../17-anydesk-realite.md) §2) :** AnyDesk **n'utilise pas** de codec matériel H.264. DeskRT est un codec **logiciel propriétaire spécialisé contenu d'écran** ; le réglage « accélération matérielle » d'AnyDesk = **rendu GPU local de la trame déjà décodée**, pas un pipeline NVENC. NovaDesk fait le **pari inverse** (matériel d'abord). « Être exactement aussi performant qu'AnyDesk » signifie donc **égaler la performance perçue** (fluidité, latence, faible débit), pas répliquer DeskRT.

---

## 1. Leviers de performance d'AnyDesk

| # | Levier AnyDesk | Détail | Confiance |
|---|---|---|---|
| L1 | **Codec spécialisé contenu d'écran** (DeskRT) | Exploite aplats, arêtes nettes, texte, motifs répétés ; sépare texte/mouvement | ✅ officiel |
| L2 | **60 fps** | LAN + la plupart des connexions | ✅ marketing / labo |
| L3 | **Latence < 16 ms LAN** (parfois 12 ms) | « imperceptible » ; **non vérifiable indépendamment** (labo mesure 32 ms visuel glass-to-glass) | ✅ marketing / ⚠ vérif |
| L4 | **Très faible débit** | « ~100 kb/s » minimal viable ; ~342 o/trame moyenne, ~1,23 Mo/min en labo 60 fps | ✅ |
| L5 | **Adaptation qualité/débit** | Préréglages Meilleure qualité/Équilibré/Meilleures perfs + modes Efficacité/Vidéo/Jeu | ✅ |
| L6 | **Encodage delta / régions modifiées** | Trames delta minuscules quand l'écran bouge peu (implicite au vu des ~342 o/trame) | ≈ |
| L7 | **Rendu GPU local** | Direct3D/DirectDraw/OpenGL pour composer la trame décodée sans coût CPU | ✅ |
| L8 | **Transport tolérant** | 443/80 sortant (franchit les pare-feux), repli relais ; TCP (choix historique) | ✅ |
| L9 | **Faible empreinte CPU/mémoire/binaire** | Client léger, démarre vite, tourne en VM/headless | ≈ |

---

## 2. Exigences chiffrées NovaDesk (KPI cibles, mesurables)

> Ces cibles reprennent l'intention du plan 14 et la rendent opposable. Chaque KPI doit être **mesuré par une sonde** (exemple exécutable / bench) et **non-régressé** en CI.

| KPI | Cible NovaDesk | Levier | Comment mesurer |
|---|---|---|---|
| **Débit d'images** | ≥ 60 fps @ 1080p si CPU/GPU et lien le permettent ; ≥ 30 fps garanti en dégradé | L2 | compteur fps encodeur+viewer (déjà partiellement dans exemples) |
| **Latence glass-to-glass** | **< 30 ms LAN**, < 60 ms WAN régional (honnête, mesuré ; on ne revendique pas « < 16 ms ») | L3 | timestamp incrusté / photodiode (plan 03 §2) |
| **Latence d'entrée** (input→effet) | < 30 ms LAN | L3 | horodatage aller-retour input |
| **Débit réseau** | Session bureautique 1080p : médiane < 1,5 Mbit/s ; plancher fonctionnel ~256 kbit/s | L4/L6 | compteur octets/s transport |
| **Taille trame delta** (écran ~statique) | quelques Ko, idéalement < 2 Ko | L4/L6 | taille `EncodedChunk` |
| **CPU encodeur** (hôte 1080p60 SW) | < 1 cœur ; **avec NVENC : < 15 % d'un cœur** | L1/L7 | échantillonnage CPU pendant le bench |
| **Mémoire client** | < 250 Mo en session | L9 | RSS mesuré |
| **Temps d'établissement** | < 3 s (résolution ID → image) en P2P direct | L8 | horodatage machine à états |
| **Reprise après coupure** | reconnexion transparente < 2 s (backoff) | — | sonde coupure/reprise |
| **Taux P2P direct** | > 85 % des sessions sans relais | L8 | stats NAT traversal |

---

## 3. État réel NovaDesk vs ces exigences (constaté par audit)

| Domaine | Constat (fichier:preuve) | Verdict perf |
|---|---|---|
| **Pipeline de bout en bout** | Fonctionne **en loopback** (exemples `nd-core/examples/viewer_window.rs`, `secure_desktop.rs`) : capture→encode→QUIC→décode→affichage. Mais **rien n'est mesuré/non-régressé**, et **rien n'est relié à l'UI**. | Non instrumenté |
| **Codec — matériel** | Backend « Media Foundation » = **MFT logiciel Microsoft** (`nd-codec/src/mediafoundation.rs:339` `hardware:false`), **pas NVENC**. RTX 4080 **inutilisée**. Decode = openh264 **logiciel** uniquement. | ❌ pari matériel non tenu |
| **Codec — ABR** | `BitrateLadder`/`negotiate` **réels et testés** (`negotiation.rs`, 14 tests) mais **NON câblés** : débit **codé en dur** (8000 kbps `nd-core/lib.rs:216`, 12000 dans un exemple). `set_target_bitrate` = **no-op TODO** en logiciel (`software.rs:114`). | ❌ adaptation absente en pratique |
| **Codec — dirty-rects/delta** | DXGI **remplit** `CapturedFrame.dirty` (`nd-capture/win.rs:180-211`) mais les encodeurs **l'ignorent** : **ré-encodage plein cadre** à chaque trame (`software.rs`, `mediafoundation.rs`). | ❌ L6 absent → débit/CPU trop hauts |
| **Codec — spécialisation écran** | H.264 générique ; pas de séparation texte/mouvement type DeskRT (chantier R&D, plan 03 §12). | ⚠ écart de conception assumé |
| **Capture — zéro-copie GPU** | `FrameImage` n'a qu'une variante **CPU** (`nd-capture/lib.rs:170`). Copie CPU systématique. | ❌ L7 partiel |
| **Viewer — frames** | `ViewerPipeline::run` **jette les pixels décodés** (`nd-core/lib.rs:278-281`). Aucun chemin vers une texture. | ❌ bloque le rendu UI |
| **Transport** | QUIC (quinn) réel, datagrammes non fiables + **FEC Reed-Solomon adaptatif** réels et testés (`nd-transport`, 22 tests). `PathEstimate` (rtt/loss/bw) **existe** mais **n'alimente pas l'ABR** (non câblé). | ⚠ bon socle, boucle ouverte |
| **Réseau réel** | **Loopback uniquement** ; NAT traversal/relais **non câblés** dans le path de session (0 réf. dans nd-transport). Pas de mesure WAN. | ❌ KPI WAN non mesurables |
| **Instrumentation** | Outils `metrics.rs` (PSNR/SSIM, 13 tests) présents ; **aucun bench de latence/fps/CPU non-régressé** en CI. | ❌ pas de garde-fou perf |

**Synthèse perf.** Le socle est bon (QUIC+FEC solides, codec fonctionnel, capture DXGI + dirty-rects disponibles) **mais la boucle de performance est ouverte** : pas d'ABR câblé, pas d'encodage delta, pas de matériel, frames jetées, et **aucune mesure**. En l'état, NovaDesk ne peut ni **atteindre** ni **prouver** les KPI.

---

## 4. Actions priorisées (→ prompts Fable)

| Priorité | Action | Impact KPI | Crate | Prompt |
|---|---|---|---|---|
| **P0** | Faire produire au viewer un **flux de frames** consommable (ne plus jeter les pixels) + orchestrateur | rend le rendu possible | nd-core | `04-prompts-fable/01` |
| **P0** | **Instrumenter** fps/latence/débit dans l'orchestrateur + exemple-sonde exécutable | mesurabilité | nd-core | `01` |
| **P1** | **Câbler l'ABR** : `PathEstimate` → `BitrateLadder`/`negotiate` → `set_target_bitrate` réel | débit, fluidité | nd-codec | `06` |
| **P1** | **Encodage delta / dirty-rects** : passer les régions modifiées à l'encodeur (au moins skip-frame si identique + ROI si supporté) | débit, CPU, taille trame | nd-codec | `06` |
| **P1** | **`set_target_bitrate` réel** (openh264 + MF) | débit adaptatif | nd-codec | `06` |
| **P1** | **Bench de perf non-régressé** (latence loopback, fps, octets/trame, CPU) en exemple + CI | garde-fou | nd-core/nd-codec | `06` |
| **P2** | **Encodage matériel NVENC** (RTX 4080) + décodage matériel, repli SW | CPU hôte, 60 fps 4K | nd-codec | `06` (extension) / futur |
| **P2** | **Capture zéro-copie GPU** (variante GPU de `FrameImage`, texture partagée D3D11) | CPU, latence | nd-capture | `12` (extension) |
| **P2** | Validation **WAN réelle** (au-delà du loopback) une fois la connectivité câblée | KPI WAN | nd-signaling/transport | `05` |

---

## 5. Note sur « exactement aussi performant »

- **Atteignable à court terme** (P0+P1, software) : 60 fps 1080p en LAN, < 30 ms glass-to-glass, débit bureautique < 1,5 Mbit/s **si** on câble ABR + delta + frames. C'est le niveau « fluide comme AnyDesk » pour la bureautique.
- **Pour dépasser AnyDesk** (P2, matériel) : NVENC/decode matériel donnent le 4K60 à faible CPU qu'un codec logiciel comme DeskRT ne peut pas offrir — c'est le **différenciateur** assumé du plan.
- **Ce qu'on ne clonera pas** : la spécialisation « contenu d'écran » extrême de DeskRT (séparation texte/mouvement) est un chantier R&D long (plan 03 §12) ; sur du contenu très textuel à très bas débit, AnyDesk gardera un avantage jusqu'à ce chantier. **Rester honnête là-dessus.**
