# Linux — .deb, .rpm, AppImage, Flatpak

Tous les formats empaquettent le **bundle Flutter Linux** déjà construit
(`ui/build/linux/x64/release/bundle/` : binaire `novadesk` + `lib/` + `data/`),
**sans** ajouter de métadonnées dans les `Cargo.toml` (aucune crate modifiée).

| Format | Recette | Outil | Cible |
|---|---|---|---|
| `.deb` | `deb/` + `deb/build-deb.sh` | `dpkg-deb` | Debian/Ubuntu (dépôt apt signé GPG) |
| `.rpm` | `rpm/novadesk.spec` + `rpm/build-rpm.sh` | `rpmbuild` | Fedora/RHEL/openSUSE |
| `.AppImage` | `appimage/` | `appimagetool` | portable, sans installation |
| Flatpak | `flatpak/com.novadesk.NovaDesk.yaml` | `flatpak-builder` | Flathub (sandbox) |

Fichiers partagés : `novadesk.desktop` (entrée de menu) et
`systemd/novadesk.service` (accès non surveillé, **désactivé par défaut**).

## Disposition installée (FHS)

```
/usr/lib/novadesk/…          bundle Flutter (binaire + lib/ + data/)
/usr/bin/novadesk            -> lien vers /usr/lib/novadesk/novadesk
/usr/share/applications/novadesk.desktop
/lib/systemd/system/novadesk.service   (deb)   |  /usr/lib/systemd/system (rpm)
```

## Construire

```bash
# .deb
packaging/linux/deb/build-deb.sh ui/build/linux/x64/release/bundle
# .rpm
packaging/linux/rpm/build-rpm.sh ui/build/linux/x64/release/bundle dist
# AppImage
packaging/linux/appimage/build-appimage.sh ui/build/linux/x64/release/bundle
# Flatpak (placer le bundle dans packaging/linux/flatpak/staging/bundle/)
flatpak-builder --repo=repo build-dir packaging/linux/flatpak/com.novadesk.NovaDesk.yaml
```

## Dépendances & sandbox

- **.deb/.rpm** déclarent les dépendances runtime probables (GTK3, PipeWire,
  libX11). ⚠ À **confirmer par `ldd`** sur le binaire réel de la machine de
  build (la liste peut varier selon la version de Flutter).
- **Flatpak** : la capture d'écran est **impossible en direct** en bac à sable ;
  elle passe par `org.freedesktop.portal.ScreenCast` + PipeWire (déclaré dans
  `finish-args`). Fonctionnalités host restreintes par rapport au paquet natif.

## Signature & mises à jour

- **.deb/.rpm** : signature **GPG au niveau du dépôt** (apt/dnf) — non gérée ici
  (relève de l'infra de dépôt). Signer les paquets et les métadonnées de dépôt.
- **AppImage** : mise à jour via l'updater maison (voir `../update/`), signature
  applicative Ed25519.

## Vérifiable ici vs sur machine de build

- **Vérifié ici** : syntaxe des scripts (`bash -n`), lisibilité INI du `.desktop`
  et de l'unité systemd, bonne formation du manifeste Flatpak (YAML) et du
  metainfo (XML), présence des sections RPM obligatoires.
- **À valider sur machine Linux** : `dpkg-deb --build`, `rpmbuild -bb`,
  `appimagetool`, `flatpak-builder`, et le contrôle qualité `lintian`/`rpmlint`/
  `desktop-file-validate`/`appstreamcli validate`. Aucun de ces outils n'est
  présent sur le poste de dev.
