# Packaging NovaDesk — chaîne complète (installeurs, signature, auto-update)

Ce dossier est passé d'**un simple README** à une **chaîne de packaging réelle** :
installeurs par OS, signature de code, orchestration serveur et squelette
d'auto-update signé. Il reste **aligné sur le [plan 15](../plan-technique/15-deploiement-mise-a-jour.md)**
(quand il sera présent) qui fait foi pour le détail.

> **Aucune crate Rust ni l'UI ne sont modifiées.** Tout est autonome sous
> `packaging/` (+ `.github/workflows/`). Les scripts hors-crate lisent la source
> unique de version `common/version.env` (alignée sur `Cargo.toml` et
> `ui/pubspec.yaml`).

## Arborescence

| Dossier | Contenu | Sous-README |
|---|---|---|
| `common/` | `version.env` : identité produit, version, GUID Windows. | — |
| `windows/` | MSI **WiX v4**, **CLI de déploiement** (parité AnyDesk), **signature Authenticode**. | [windows/README](windows/README.md) |
| `macos/` | `.app`/DMG/`.pkg`, **entitlements**, LaunchDaemon, **codesign + notarisation**. | [macos/README](macos/README.md) |
| `linux/` | **.deb**, **.rpm**, **AppImage**, **Flatpak**, `.desktop`, unité systemd. | [linux/README](linux/README.md) |
| `server/` | **Dockerfile** + **docker-compose** lançant les 4 serveurs ensemble. | [server/README](server/README.md) |
| `update/` | Métadonnées **TUF-like signées Ed25519** + **vérificateur**. | [update/README](update/README.md) |

## Invariant de sécurité — double vérification (plan 15 §15.1)

Aucun octet n'est exécuté chez l'utilisateur sans **deux** vérifications
indépendantes :

1. la signature **native de l'OS** — Authenticode (Windows), codesign +
   notarisation (macOS), GPG de dépôt (Linux) : réputation SmartScreen/Gatekeeper ;
2. notre signature **applicative** **Ed25519** sur le canal de mise à jour
   (`update/`) — indépendante des CA commerciales.

## Prérequis (⚠ machine de build avec droits)

Le **build/signature natif exige une machine à droits admin** (impossible sur le
poste de dev actuel : pas d'admin/symlinks). Par plateforme :

| Plateforme | Outils | Secrets / certificats |
|---|---|---|
| Windows | WiX v4 (`dotnet tool install -g wix`), Windows SDK (`signtool`) | Certificat **Authenticode EV** (HSM) |
| macOS | Xcode CLT, `create-dmg` | **Developer ID Application + Installer**, profil notarytool |
| Linux | `dpkg-deb`, `rpmbuild`, `appimagetool`, `flatpak-builder` | Clé **GPG** de dépôt |
| Serveurs | Docker + Compose | — |
| Auto-update | Python + `cryptography` (ou `cosign`) | Graine **Ed25519** rôle *targets* (secret CI) |

Le **client** (UI Flutter + `nd_ffi`) doit être construit au préalable
(`flutter build windows|macos|linux`) : ses dossiers runner desktop
(`ui/windows|macos|linux/`) ne sont **pas** encore générés ici.

## Procédure (résumé ; détails dans les sous-README)

```bash
# 1. Serveurs — orchestration locale des 4 services :
cd packaging/server && docker compose up --build

# 2. Client Windows — MSI signé :
wix build packaging/windows/wix/NovaDesk.wxs -arch x64 -d StageDir=… -ext WixToolset.UI.wixext -o dist/NovaDesk.msi
packaging/windows/sign.ps1 -Path dist/NovaDesk.msi

# 3. Client macOS — DMG/PKG signés + notarisés :
packaging/macos/codesign-notarize.sh && packaging/macos/build-pkg.sh

# 4. Client Linux — deb + AppImage + rpm :
packaging/linux/deb/build-deb.sh   ui/build/linux/x64/release/bundle
packaging/linux/appimage/build-appimage.sh ui/build/linux/x64/release/bundle
packaging/linux/rpm/build-rpm.sh   ui/build/linux/x64/release/bundle dist

# 5. Métadonnées de MAJ signées (exemple vérifiable sans droits) :
python packaging/update/verify_update.py verify \
    --root packaging/update/tuf/root.json \
    --manifest packaging/update/tuf/manifest.stable.json --current 0.1.0
```

## CI de release (`.github/workflows/release.yml`)

Sur tag `vX.Y.Z` :

- **`build`** — binaires serveur bruts (3 OS) : **inchangé** (aucune régression).
- **`installeurs`** — installeurs client signés par OS ;
- **`metadonnees-maj`** — manifestes de MAJ signés Ed25519 ;
- **`publier`** — un brouillon de release réunissant ce qui existe.

Les volets `installeurs`/`metadonnees-maj` exigent le runner Flutter + des
secrets de signature : ils sont **désactivés par défaut** et s'activent avec la
variable de dépôt `NOVADESK_PACKAGING_ENABLED=true`. Le volet serveur reste donc
toujours vert.

## Ce qui est vérifié ici vs à valider sur machine de build

**Vérifié ici (inspection/lint, sans droits ni réseau)** :

- YAML des workflows et du `docker-compose` (`pyyaml`, clés de fusion résolues) ;
- XML des `.wxs`/`.plist`/AppStream (bonne formation, `plistlib`) ;
- scripts `sh`/`bash` (`bash -n`) et PowerShell (analyseur AST + exécution à blanc) ;
- **chaîne de mise à jour de bout en bout** : signature + vérification Ed25519
  réelles, et **rejet d'un manifeste falsifié** (le seul volet pleinement
  exécutable ici).

**À valider sur une machine de build (droits admin, outils natifs)** :

- `wix build`, `msiexec`, `signtool` (certificat réel) ;
- `codesign`/`notarytool`/`stapler`, `pkgbuild`/`productbuild` (macOS) ;
- `dpkg-deb`, `rpmbuild`, `appimagetool`, `flatpak-builder` (Linux) ;
- `docker compose` (build multi-étapes + amorçage réel de la clé d'autorité) ;
- le câblage client de l'argument `--service` et des drapeaux `--get-id` /
  `--set-password` / `--register-license` (lot UI/FFI).
