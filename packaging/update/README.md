# Auto-update NovaDesk — métadonnées signées (squelette TUF-like)

Cette couche livre le **format de métadonnées de mise à jour + le vérificateur**
que le cœur Rust ne porte pas encore. Elle complète, côté client, le service
`UpdateService` de [`server/nd-api/src/update.rs`](../../server/nd-api/src/update.rs)
(canaux `stable`/`beta`/`canary`/`lts`, `latest`, `min_supported`, `sha256`,
delta) en ajoutant la **signature applicative Ed25519** exigée par le plan 15
§15.6.4 : indépendante des autorités de certification de l'OS, elle constitue la
seconde moitié de l'« invariant de double vérification ».

> Ce n'est **pas** un serveur de mise à jour complet : c'est le *format*, le
> *signeur*, le *vérificateur* et des *manifestes d'exemple*. Le service HTTP de
> distribution (CDN + rollout progressif) vit ailleurs (plan 15).

## Fichiers

| Fichier | Rôle |
|---|---|
| `sign_manifest.py` | Génère les clés, `root.json`, et **signe** les manifestes. |
| `verify_update.py` | **Vérifie** `root.json` + un manifeste, puis rend la décision (UpToDate / UpdateAvailable / ForcedUpdate). |
| `tuf/root.json` | Racine de confiance : clés + rôles (root/timestamp/snapshot/targets) + seuils. |
| `tuf/manifest.stable.json` | Manifeste d'exemple du canal **stable** (signé). |
| `tuf/manifest.beta.json` | Manifeste d'exemple du canal **beta** (signé, avec delta). |
| `keys/` | Emplacement des clés — **aucune clé privée versionnée** (voir `keys/README.md`). |

## Modèle de confiance

- Chaque métadonnée est une enveloppe `{ "signed": {...}, "signatures": [...] }`.
- La **signature Ed25519 porte sur le JSON canonique** de `signed` (clés triées,
  séparateurs compacts, UTF-8). Signeur et vérificateur appliquent la même règle,
  sinon la vérification échoue — c'est voulu.
- `root.json` **s'auto-authentifie** (ses signatures satisfont son rôle `root`),
  puis fait autorité sur les clés qui signent les manifestes (rôle `targets`).

> **Démonstration vs production.** Ici une **unique** clé Ed25519 sert tous les
> rôles, pour rester vérifiable sans infrastructure. En production : une clé
> **distincte et hors ligne (HSM)** par rôle, la clé `root` conservée déconnectée,
> rotation et seuils multi-signatures (`threshold > 1`) sur `root`/`targets`.

## Reproduire la chaîne d'exemple (vérifiable ici, sans droits)

```bash
cd packaging/update

# 1. Clé de DÉMONSTRATION déterministe (graine = sha256("novadesk-demo-tuf-root-key")).
#    NE PAS committer la graine ; elle se régénère à volonté.
python sign_manifest.py keygen --demo --out /tmp/demo.seed.hex

# 2. Racine de confiance (déjà fournie dans tuf/root.json).
python sign_manifest.py emit-root --key /tmp/demo.seed.hex --out tuf/root.json

# 3. Vérifier la racine puis un manifeste, et obtenir la décision de MAJ.
python verify_update.py verify-root --root tuf/root.json
python verify_update.py verify --root tuf/root.json \
    --manifest tuf/manifest.stable.json --current 0.1.0 --platform windows-x86_64
```

Sortie attendue : `root.json` de confiance, `stable` → **UpToDate** pour 0.1.0,
`beta` → **UpdateAvailable** (0.1.0 < 0.2.0). Un manifeste **falsifié** (p. ex.
`latest` remonté sans re-signature) est **rejeté** (« seuil de signatures non
atteint »).

## Signer un vrai manifeste en CI (chaîne complète, sur runner)

```bash
# La clé privée vient d'un secret CI (jamais du dépôt) :
echo "$NOVADESK_TUF_TARGETS_SEED_HEX" > targets.seed.hex

# Le corps `signed` est produit à partir des artefacts réels : la CI y injecte
# les URL et les sha256 CALCULÉS sur les installeurs signés (remplace les 0*64).
python sign_manifest.py sign --key targets.seed.hex \
    --in manifest.stable.body.json --out tuf/manifest.stable.json
shred -u targets.seed.hex
```

Alternative **cosign** (alignée plan 15, si l'écosystème sigstore est retenu) :
`cosign sign-blob --key cosign.key tuf/manifest.stable.json > manifest.sig`, la
clé `cosign.key` étant un secret CI. Les deux approches sont Ed25519 ; le format
d'enveloppe ci-dessus reste la référence interne.

## Placeholders à remplacer (honnêteté)

- `sha256` des artefacts = `0`×64 : **placeholders**. La CI les remplace par les
  empreintes réelles des installeurs **après signature native**.
- `url` = `updates.novadesk.example` : domaine d'exemple.
- Les clés de `tuf/root.json` sont celles de la **clé de démonstration** : à
  remplacer par les clés de production (rôles séparés) avant tout déploiement.
