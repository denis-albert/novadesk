# Packaging NovaDesk — notes par plateforme

Ce dossier accueillera les définitions de packaging (fichiers WiX, scripts DMG,
recettes deb/rpm/Flatpak…). Cette page résume la stratégie, **alignée sur le
[plan 15](../../plan-technique/15-deploiement-mise-a-jour.md)** qui fait foi pour
tout le détail (signature, mises à jour, déploiement d'entreprise).

**État actuel** : rien n'est encore automatisé. Le workflow
[`release.yml`](../.github/workflows/release.yml) produit des binaires serveur
bruts par OS ; le packaging client viendra avec l'UI Flutter (plan 10) et le
service système (plan 15 §15.3).

## Invariant de sécurité (plan 15 §15.1)

Aucun octet n'est exécuté chez l'utilisateur sans **double vérification** :

1. la signature **native de l'OS** (Authenticode / codesign+notarisation /
   GPG des dépôts) — réputation SmartScreen/Gatekeeper ;
2. notre signature **applicative** (Ed25519 via The Update Framework, §15.6.4)
   sur le canal de mise à jour — indépendante des CA commerciales.

## Windows — MSI (principal), MSIX, EXE portable

| Artefact | Usage | Outil |
|---|---|---|
| `.msi` | Canal principal, entreprise (GPO/Intune/SCCM, transforms `.mst`) | WiX Toolset v4, piloté par `cargo-wix` |
| `.msix` | Microsoft Store, édition « visualiseur » à privilèges réduits | MSIX Packaging Tool |
| `.exe` portable | Intervention ponctuelle sans droits admin (helpdesk) | binaire unique auto-extractible |

- Le MSI installe le **service Windows** `novadesk-svc` (compte `LocalSystem`,
  redémarrage automatique sur crash) — requis pour la capture du bureau
  sécurisé/UAC (plan 07). `UpgradeCode` **stable** entre versions ;
  `MajorUpgrade` transactionnel (rollback MSI natif).
- **Limite MSIX assumée** : le conteneur MSIX est inadapté au service
  `LocalSystem` ; l'édition complète passe par MSI (plan 15 §15.2.2).
- **Signature** : Authenticode **EV sur HSM**, `signtool sign /fd SHA256` +
  horodatage (`/tr … /td SHA256`). Tous les artefacts, portable compris.
- Mises à jour : updater maison (TUF) pour le MSI, App Installer pour le MSIX.

## macOS — .app, .dmg, .pkg, notarisation

| Artefact | Usage | Outil |
|---|---|---|
| `.app` | Bundle applicatif (UI Flutter + helper Rust) | `xcodebuild` |
| `.dmg` | Installation manuelle « glisser vers Applications » | `create-dmg` |
| `.pkg` | Installeur requis pour poser le **LaunchDaemon root** | `pkgbuild` + `productbuild` |

- Service : LaunchDaemon (root) + LaunchAgent (session), posés par le `.pkg`
  (plan 15 §15.3.2).
- **Chaîne de signature obligatoire** (sinon Gatekeeper bloque) :
  1. `codesign` avec certificat **Developer ID** + **Hardened Runtime**
     (`--options runtime`, entitlements minimaux) ;
  2. **notarisation** : `xcrun notarytool submit --wait` ;
  3. **agrafage** : `xcrun stapler staple` (le ticket suit l'artefact hors ligne).
- Mises à jour : **Sparkle 2** (appcast signé EdDSA).

## Linux — .deb, .rpm, AppImage, Flatpak, Snap

| Artefact | Usage | Outil |
|---|---|---|
| `.deb` | Debian/Ubuntu, dépôt apt signé GPG | `cargo-deb` |
| `.rpm` | Fedora/RHEL/openSUSE, dépôt dnf signé GPG | `cargo-generate-rpm` ou `fpm` |
| `.AppImage` | Binaire portable sans installation | `appimagetool` |
| Flatpak | Distribution sandboxée (Flathub) | `flatpak-builder` |
| Snap | Écosystème Ubuntu | `snapcraft` |

- Service : unité **systemd** `novadesk.service` (posée par deb/rpm).
- **Contrainte sandbox** : sous Flatpak/Snap, la capture d'écran passe
  obligatoirement par le portail `xdg-desktop-portal` + **PipeWire** (plan 02) —
  fonctionnalités host restreintes par rapport au paquet natif.
- Mises à jour : dépôts apt/dnf (signés GPG) ; Flatpak/Snap gérées par leur
  store ; AppImage via l'updater maison (TUF).

## Mobile et web (pour mémoire)

- **Android** : `.aab` (Play Store, Play App Signing) + `.apk` sideload
  (`apksigner`) ; capture via `MediaProjection`.
- **iOS** : `.ipa` App Store uniquement (sandbox strict, pas de daemon).
- **Web** : bundle statique WASM + JS (client visualiseur, `nd-wasm`), déploiement
  atomique CDN.

Détails dans le plan 15 (§15.2) et le [plan 12](../../plan-technique/12-multiplateforme.md).

## Chaîne de mise à jour (plan 15 §15.6)

Commune à toutes les plateformes hors stores : métadonnées **TUF** signées
**Ed25519**, mises à jour **delta silencieuses**, rollout progressif (canari),
**rollback** automatique vers N-1 en < 60 s via watchdog de santé post-update.
La CI de release (voir `release.yml`) est le point d'insertion prévu pour la
signature et la génération de ces métadonnées.
