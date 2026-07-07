# Clés de signature — emplacement (aucune clé versionnée)

**Aucune clé privée ne doit jamais être committée ici.** Ce dossier ne contient
que cette note.

## Production

- **Rôle `root`** : clé Ed25519 générée et conservée **hors ligne** (HSM, poste
  déconnecté). Elle ne signe que `root.json` (délégations de confiance), jamais
  les manifestes du quotidien. Rotation planifiée, seuil multi-signatures.
- **Rôle `targets`** (et `snapshot`/`timestamp`) : clés en ligne, fournies aux
  runners de release **exclusivement via des secrets CI** :
  - `NOVADESK_TUF_TARGETS_SEED_HEX` — graine Ed25519 (hex, 32 octets) du rôle targets ;
  - ou `NOVADESK_COSIGN_KEY` + `NOVADESK_COSIGN_PASSWORD` si l'on retient cosign.

## Démonstration (reproductible, non secrète)

Les manifestes d'exemple de `../tuf/` sont signés par une graine **déterministe** :

```
graine = sha256("novadesk-demo-tuf-root-key")
```

Régénérable à volonté par `python ../sign_manifest.py keygen --demo --out <chemin>`.
Cette clé n'a **aucune valeur de sécurité** : elle sert uniquement à faire
tourner `verify_update.py` de bout en bout sans infrastructure.

## `.gitignore` recommandé (à ajouter au dépôt, hors de ce lot)

```
packaging/update/keys/*.hex
packaging/update/keys/*.key
packaging/update/keys/*.pem
```
