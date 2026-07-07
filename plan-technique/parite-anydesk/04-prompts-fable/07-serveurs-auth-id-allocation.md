# Prompt Fable 07 — Auth d'infra + attribution d'ID + jetons applicatifs (serveurs)

**Priorité : P1** (bloquant pour « pair distant réel » sûr) · **Crates ciblées : `server/nd-rendezvous`, `server/nd-relay`, `server/nd-api`** · **Parallélisable avec : 01, 02, 05, 06, 08** (client). **Coordination faible avec 09** (nd-accounts) : crates disjointes, câblage jetons ensuite.

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : fermer les trous de sécurité serveur qui rendent la connectivité « ouverte » : **preuve de possession d'ID** au rendez-vous (anti-squatting), **tickets de relais signés**, **service d'attribution d'ID**, et **autorisation réelle** dans nd-api (aujourd'hui : tout jeton non vide accepté).

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\server\nd-rendezvous\`, `server\nd-relay\`, `server\nd-api\`. **Ne touche PAS `nd-signaling`** (lot 05) ni `nd-accounts` (lot 09) — coordonne via API/protocole documenté.
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo parallèle = normal.

## BARRE QUALITÉ
- `cargo clippy -p nd-rendezvous -p nd-relay -p nd-api --all-targets -- -D warnings` = **ZÉRO**.
- `cargo fmt` sur les trois.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `nd-rendezvous` : `main.rs` fait `serve(listener, Registry::new())` (toute la logique est dans `nd-signaling`). **Aucune authentification** : n'importe qui `Register` n'importe quel ID.
- `nd-relay` (8 tests) : relais aveugle par **ticket** `[u32 len][ticket]`, quotas mémoire, sélection par RTT. **Tickets non signés** (`main.rs:13` : « signés au plan 11 »). Quota par défaut illimité.
- `nd-api` (48 tests) : 14 endpoints (carnet/RBAC/groupes/partage/MAJ/config) sur **TCP binaire maison** (`protocol.rs`), persistance JSON. **Autorisation factice** : tout jeton non vide accepté (`lib.rs:138-146`), compte agi **fourni dans la requête**, `AssignRole` sans contrôle admin.
- `NovaId` = `u64` (`nd-proto`) sans logique d'attribution.
- **Le plan 11** (`../11-backend-infrastructure.md`) décrit l'intention (tickets signés, attribution FPE/FF1, RBAC). Aligne-toi dessus.

## TÂCHE
1. **Preuve de possession d'ID au rendez-vous** (nd-rendezvous, sans modifier nd-signaling si possible — sinon expose un hook) : lier `Register` à une **clé statique** (l'ID doit être accompagné d'une **signature** prouvant la possession de la clé associée à l'ID, ou d'un **jeton d'enregistrement** émis par le service d'attribution). Empêcher qu'un tiers enregistre l'ID d'autrui. Si nd-signaling doit exposer un point d'extension, **documente** la coordination (mais n'édite pas nd-signaling ici).
2. **Service d'attribution d'ID** (dans nd-api ou un petit module dédié) : allouer des `NovaId` **uniques** et **non énumérables** (FPE/FF1 ou au minimum un compteur + brouillage), **liés à un compte** (référence au compte nd-accounts, validé via jeton — voir #4), avec anti-réattribution. Endpoint `AllocateId` + persistance.
3. **Tickets de relais signés** (nd-relay) : le relais n'accepte que des tickets **signés** (Ed25519) par une autorité (clé publique configurée), avec **portée** (paire d'IDs, expiration). Rejeter les tickets non signés/expirés. Ajouter un **quota par défaut** raisonnable (octets/paire, connexions/IP). Garder le pipe aveugle.
4. **Autorisation réelle nd-api** : dériver le **compte agissant du jeton** (pas de la requête) ; **valider le jeton** (signature/format ; la validation croisée complète avec nd-accounts est coordonnée au lot 09 — pour l'instant, valide un **jeton signé** vérifiable localement, et documente le point de jonction). Appliquer le **RBAC comme contrôle d'accès** : `AssignRole`/`SetPolicy`/etc. exigent le rôle adéquat ; refuser sinon. Ne pas casser les 48 tests (adapte-les si la sémantique d'auth change, en gardant la couverture).
5. **Tests** : ajoute des tests pour chaque garde (enregistrement d'ID d'autrui refusé, ticket non signé refusé, ticket expiré refusé, opération RBAC sans rôle refusée, attribution d'ID unique/non réattribué).

## VÉRIF (obligatoire, chiffres exacts)
- `cargo build -p nd-rendezvous -p nd-relay -p nd-api --manifest-path ...` → OK.
- `cargo test -p nd-rendezvous -p nd-relay -p nd-api --manifest-path ...` → verts (8+48 existants adaptés + nouveaux). Reporte le compte.
- `cargo clippy -p nd-rendezvous -p nd-relay -p nd-api --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt` sur les trois.
- **Régression** : le scénario `nd-api/tests/scenario_tcp.rs` passe (adapté à l'auth réelle si nécessaire).

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés.
- Modèle d'auth retenu (schéma de signature, portée des tickets, format des jetons) + **point de jonction avec nd-accounts** (lot 09).
- Ce qui est appliqué vs encore permissif (honnêteté).
- État EXACT des vérifs (tests, clippy 0).
- **Pas de git.**
