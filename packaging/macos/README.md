# macOS — .app, DMG/pkg, codesign + notarisation

## Contenu

| Fichier | Rôle |
|---|---|
| `Info.plist` | Clés de bundle de **référence** (identité + `*UsageDescription`). |
| `NovaDesk.entitlements` | Entitlements **Hardened Runtime** (audio, apple-events, chargement de dylib). |
| `launchd/com.novadesk.NovaDesk.plist` | **LaunchDaemon** root (accès non surveillé). |
| `build-dmg.sh` | Emballe `NovaDesk.app` en **DMG** (create-dmg, repli hdiutil). |
| `codesign-notarize.sh` | **codesign → notarytool → stapler** (chaîne Gatekeeper complète). |
| `build-pkg.sh` | **.pkg** installant l'app + le LaunchDaemon (pkgbuild/productbuild). |
| `scripts/postinstall` | Charge le LaunchDaemon à l'installation. |

## Prérequis (machine **macOS**, Xcode + compte Apple Developer)

- Xcode / Command Line Tools (`codesign`, `pkgbuild`, `productbuild`, `xcrun`).
- Certificats **Developer ID Application** et **Developer ID Installer**.
- Un profil de notarisation : `xcrun notarytool store-credentials`.
- La sortie du build Flutter macOS (`ui/build/macos/.../Release/NovaDesk.app`),
  **non produite ici** (macOS requis).

## Capture d'écran & accessibilité — le point sensible

- **Screen Recording** (ScreenCaptureKit) et **Accessibility** (injection
  clavier/souris via CGEvent) ne sont **pas** des entitlements : ce sont des
  autorisations **TCC** accordées par l'utilisateur dans *Réglages Système ›
  Confidentialité et sécurité*. L'app doit détecter l'absence de droit et guider
  l'utilisateur. Aucune signature ne les remplace.
- Les entitlements ici couvrent ce qui doit l'être à la signature : capture
  **audio**, **apple-events**, et `disable-library-validation` (charger
  `nd_ffi.dylib` + plugins Flutter).

## Chaîne complète (sur runner macOS signé)

```bash
export NOVADESK_MAC_DEV_ID_APP="Developer ID Application: Ma Société (TEAMID)"
export NOVADESK_NOTARY_PROFILE="novadesk-notary"        # via store-credentials

# 1) Signer + notariser + agrafer, et produire le DMG :
./packaging/macos/codesign-notarize.sh

# 2) (Option service) Construire le .pkg qui pose le LaunchDaemon :
export NOVADESK_MAC_DEV_ID_INSTALLER="Developer ID Installer: Ma Société (TEAMID)"
./packaging/macos/build-pkg.sh
xcrun notarytool submit dist/NovaDesk-0.1.0.pkg --keychain-profile novadesk-notary --wait
xcrun stapler staple dist/NovaDesk-0.1.0.pkg
```

`DRY_RUN=1 ./codesign-notarize.sh` imprime chaque commande sans l'exécuter
(utile pour relire la chaîne sans certificat).

## Vérifiable ici vs sur machine de build

- **Vérifié ici** : validité des `.plist` (`plistlib`), syntaxe des scripts
  (`bash -n`), cohérence des identifiants/chemins.
- **À valider sur macOS** : `codesign`, `xcrun notarytool`, `stapler`,
  `pkgbuild`/`productbuild`, et le chargement du LaunchDaemon — aucun de ces
  outils n'existe hors macOS. Le `--service` du binaire relève du lot client.
