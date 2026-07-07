# Prompt Fable 09 — OIDC réel (RS256/ES256) + persistance SQLite + exposition réseau (nd-accounts)

**Priorité : P1** · **Crate ciblée : `server/nd-accounts`** · **Parallélisable avec : 01, 02, 05, 06, 08**. **Coordination faible avec 07** (nd-api valide les jetons émis ici) : crates disjointes.

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : rendre la **fédération OIDC réellement utilisable** (les fournisseurs signent en RS256/ES256, pas HS256), **persister** dans une vraie base (SQLite), **chiffrer les secrets**, et **exposer par le réseau** les flux aujourd'hui inatteignables (2FA, OIDC, licensing).

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\server\nd-accounts\`.
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo parallèle = normal.

## BARRE QUALITÉ
- `cargo clippy -p nd-accounts --all-targets -- -D warnings` = **ZÉRO**.
- `cargo fmt -p nd-accounts`.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `nd-accounts` (66 tests) : **Argon2id** (PHC, sel), **TOTP** RFC 6238, **PKCE S256** (vecteur RFC 7636 vérifié), URL d'autorisation OIDC (state/nonce), validation ID token. Modules `oidc.rs`, `totp.rs`, `storage.rs`, `main.rs`.
- **Failles** : `oidc.rs:266-268` ne vérifie que **HS256** ; RS256/ES256 → `AlgorithmeNonSupporte` (test `:730`), `alg:none` refusé. Pas d'échange **code→jetons** (pas de client HTTP). Persistance = **fichier JSON** (`storage.rs`), **secrets TOTP en clair**. Le serveur TCP n'expose que **Register + Login** (`main.rs:531-534`) : 2FA/OIDC/licensing inatteignables par le réseau.

## TÂCHE
1. **Signatures OIDC RS256/ES256** : implémenter la vérification de signature **asymétrique** des ID tokens (RS256 et ES256) via **JWKS** — récupération de la clé publique par `kid` depuis l'endpoint JWKS du fournisseur (client HTTP ; vérifie une lib HTTP+TLS déjà dans `Cargo.lock`, sinon `ureq`/`reqwest` selon ce qui est acceptable). Valider `iss`, `aud`, `exp`, `nonce`. Garder HS256 pour les tests/dev. Refuser `alg:none`. Cache des JWKS avec expiration.
2. **Échange code→jetons** : implémenter l'appel POST au **token endpoint** (avec PKCE `code_verifier`) pour obtenir l'`id_token`/`access_token`, puis valider (via #1). Testable avec un fournisseur simulé (serveur de test local émettant des JWKS + tokens signés RS256 avec une clé de test).
3. **Persistance SQLite** : remplacer le stockage JSON par **SQLite** (`rusqlite` ou `sqlx` selon ce qui est déjà dans l'arbre — vérifie `Cargo.lock`), avec **migrations** (schéma comptes/2FA/sessions/licences). Écriture transactionnelle. Fournir une **migration d'import** depuis l'ancien JSON si présent.
4. **Chiffrement des secrets** : les **secrets TOTP** (et jetons sensibles) doivent être **chiffrés au repos** (clé dérivée d'un secret serveur ; AEAD). Ne plus les stocker en clair.
5. **Exposition réseau** : étendre le protocole serveur pour couvrir **le flux 2FA complet** (login → challenge TOTP → validation), **le flux OIDC** (démarrage/rappel), et l'**émission de jetons applicatifs** signés (Ed25519) que **nd-api (lot 07)** pourra vérifier. Documenter le **format du jeton** (claims : sujet=compte, rôles, exp) pour le point de jonction avec 07.
6. **Tests** : RS256 valide/invalide, ES256 valide, `alg:none` refusé, JWKS `kid` inconnu refusé, échange code→token (fournisseur simulé), round-trip SQLite + migration, secret TOTP chiffré illisible en base, flux 2FA réseau de bout en bout.

## VÉRIF (obligatoire, chiffres exacts)
- `cargo build -p nd-accounts --manifest-path ...` → OK.
- `cargo test -p nd-accounts --manifest-path ...` → verts (66 adaptés + nouveaux). Reporte le compte et **retire/rends caduque** l'ancienne assertion « RS256 non supporté » (`oidc.rs:730`).
- `cargo clippy -p nd-accounts --all-targets --manifest-path ... -- -D warnings` → **0**.
- `cargo fmt -p nd-accounts`.

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés.
- Confirmation RS256/ES256 + JWKS fonctionnels (test avec fournisseur simulé) ; base SQLite + migrations ; secrets chiffrés.
- **Format du jeton applicatif** émis (pour jonction avec nd-api lot 07).
- État EXACT des vérifs (tests, clippy 0).
- **Pas de git.**
