# Prompt Fable 05 — Connectivité par ID hors loopback (STUN + hole punch + relais)

**Priorité : P0** · **Crates ciblées : `crates/nd-signaling`, `crates/nd-transport`** · **Parallélisable avec : 01, 02, 06, 07, 08** (crates disjointes). Intégration finale avec `nd-core` (lot 01) via une API publique stable.

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : câbler les briques **déjà existantes et testées** (rendez-vous, STUN, hole punching, détection NAT, relais) en un **connecteur de bout en bout** qui établit une connexion QUIC **par ID** entre deux pairs **derrière NAT** (au-delà du loopback), avec **repli relais**.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-signaling\` et `crates\nd-transport\`.
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo parallèle = normal.
- Expose une **API publique stable** (documentée) que `nd-core` (lot 01) consommera ; ne modifie pas `nd-core`.

## BARRE QUALITÉ
- `cargo clippy -p nd-signaling -p nd-transport --all-targets -- -D warnings` = **ZÉRO** (attention `type_complexity`).
- `cargo fmt -p nd-signaling -p nd-transport`.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `nd-signaling` (43 tests) : `RendezvousClient::{new, register, lookup, heartbeat, publish_candidates, peer_candidates, request_punch, poll_punch}`, `Registry`, `serve`, `PeerRecord {addr, cert_der}`, `PunchDemand {from, candidates}`, `DEFAULT_TTL`, `MAX_CANDIDATES`, `PUNCH_TTL`. Modules publics `stun` (client RFC 5389 : `discover_public_addr`), `punch` (`udp_hole_punch`, `PunchRole::{Caller,Callee}`), `nat` (`detect_nat_type`, `classifier` — **simplifié**, n'émet jamais `FullCone`/`Restricted`). **Tout est testé en loopback uniquement.**
- `nd-transport` (22 tests) : `Transport {open_channel, send, poll_recv, path_estimate}`, `bind(addr)->Listener`, `connect(addr, &cert)`, `connect_quic`, `Listener::{accept, local_addr, server_cert_der}`. **QUIC quinn réel.** `connect()` prend une **adresse directe** ; **STUN/punch ne sont PAS référencés** dans nd-transport (0 occurrence).
- Le serveur de production auth (tickets signés, attribution d'ID) est le **lot 07** — ici tu peux rester sur le rendez-vous **non authentifié** existant, mais **documente** la dépendance.

## TÂCHE
1. **Connecteur NAT de bout en bout** (nouveau module, p. ex. `nd-signaling/src/connect.rs`, réexporté) : une fonction publique
   `establish_p2p(rv: &RendezvousClient, local_id, peer_id, stun_servers: &[SocketAddr]) -> Result<ConnAttempt>` côté **appelant**, et le pendant côté **appelé** (relève via `poll_punch` puis `udp_hole_punch(Callee)`), qui :
   - découvre l'adresse réflexive via `stun::discover_public_addr` ;
   - publie les candidats (`publish_candidates`) ;
   - (appelant) `request_punch` pour obtenir les candidats de la cible + déposer la demande ;
   - lance `punch::udp_hole_punch` **simultanément** des deux côtés (rôles opposés) ;
   - renvoie le **socket UDP percé** (ou son adresse) prêt à porter QUIC, **plus** le certificat du pair (via `lookup`).
2. **Porter QUIC sur le socket percé** (nd-transport) : ajoute un point d'entrée `connect_over_socket(socket, peer_addr, &cert) -> impl Transport` / `accept_over_socket(...)` permettant à quinn d'**utiliser le socket UDP déjà ouvert** par le hole punching (quinn accepte un `UdpSocket` existant via son API d'endpoint). Si l'API quinn impose des contraintes, documente l'approche retenue (endpoint partagé). Garde `bind`/`connect` existants intacts.
3. **Repli relais** : si le punch échoue (timeout), fournir un chemin `connect_via_relay(relay_addr, ticket, &cert)` (le relais du lot 07 attend `[u32 len][ticket]` puis relaie en aveugle) — pour l'instant, un ticket **opaque** convient ; documente que la **signature** viendra du lot 07.
4. **Améliorer `nat::classifier`** (optionnel mais utile) pour émettre `FullCone`/`Restricted`/`PortRestricted`/`Symmetric` correctement (CHANGE-REQUEST RFC 3489/5780) afin d'anticiper le repli — si trop coûteux, laisse un TODO documenté et **ne régresse pas** les tests existants.
5. **Sonde exécutable** `nd-signaling/examples/p2p_two_process.rs` (ou test d'intégration) démontrant, **sur la machine locale mais via de vraies adresses d'interface (pas 127.0.0.1)**, l'échange de candidats + punch + établissement QUIC + transfert de N messages. À défaut d'un vrai NAT, simule deux « pairs » sur des sockets distincts et prouve le chemin candidats→punch→QUIC.

## VÉRIF (obligatoire, chiffres exacts)
- `cargo build -p nd-signaling -p nd-transport --examples --manifest-path ...` → OK.
- `cargo test -p nd-signaling -p nd-transport --manifest-path ...` → verts (les 43+22 existants + nouveaux). **Ne régresse aucun test.**
- Lancer la sonde `p2p_two_process` → doit établir la connexion et transférer les messages (reporte le compte).
- `cargo clippy -p nd-signaling -p nd-transport --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt` sur les deux crates.

## RÉPONSE FINALE ATTENDUE
- Fichiers créés/modifiés.
- **Signature publique exacte** du connecteur (`establish_p2p`, `connect_over_socket`, repli relais).
- Ce qui marche vraiment vs ce qui reste simulé (honnêteté : vrai NAT non testable ici).
- État EXACT des vérifs (tests, sonde).
- Note d'intégration pour `nd-core` (lot 01) : comment appeler le connecteur.
- **Pas de git.**
