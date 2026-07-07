#!/usr/bin/env bash
# Construit un .deb NovaDesk SANS cargo-deb : dpkg-deb sur un arbre stagé
# (aucune métadonnée à ajouter dans les Cargo.toml des crates).
#
# Usage : build-deb.sh [bundle_flutter_linux] [sortie.deb]
set -euo pipefail

ICI="$(cd "$(dirname "$0")" && pwd)"
LINUX="$(cd "$ICI/.." && pwd)"                 # packaging/linux
# shellcheck source=../../common/version.env
. "$LINUX/../common/version.env"

BUNDLE="${1:-ui/build/linux/x64/release/bundle}"
SORTIE="${2:-dist/novadesk_${NOVADESK_VERSION}_amd64.deb}"

[ -d "$BUNDLE" ] || { echo "erreur : bundle Flutter introuvable : $BUNDLE" >&2; exit 1; }
mkdir -p "$(dirname "$SORTIE")"

STAGE="$(mktemp -d)"
install -d "$STAGE/DEBIAN" \
          "$STAGE/usr/lib/novadesk" \
          "$STAGE/usr/bin" \
          "$STAGE/usr/share/applications" \
          "$STAGE/lib/systemd/system"

# Payload : bundle Flutter -> /usr/lib/novadesk ; lien -> /usr/bin/novadesk.
cp -R "$BUNDLE/." "$STAGE/usr/lib/novadesk/"
ln -sf ../lib/novadesk/novadesk "$STAGE/usr/bin/novadesk"

# Intégration bureau + unité systemd (accès non surveillé, désactivé par défaut).
install -m 0644 "$LINUX/novadesk.desktop" "$STAGE/usr/share/applications/novadesk.desktop"
install -m 0644 "$LINUX/systemd/novadesk.service" "$STAGE/lib/systemd/system/novadesk.service"

# Contrôle : Version resynchronisée depuis version.env (évite toute dérive).
sed "s/^Version: .*/Version: ${NOVADESK_VERSION}/" "$ICI/control" > "$STAGE/DEBIAN/control"
install -m 0755 "$ICI/postinst" "$STAGE/DEBIAN/postinst"
install -m 0755 "$ICI/prerm" "$STAGE/DEBIAN/prerm"

# Taille installée (Ko).
TAILLE_KO="$(du -sk "$STAGE/usr" "$STAGE/lib" | awk '{s += $1} END {print s}')"
echo "Installed-Size: ${TAILLE_KO}" >> "$STAGE/DEBIAN/control"

dpkg-deb --root-owner-group --build "$STAGE" "$SORTIE"
rm -rf "$STAGE"
echo "DEB écrit : $SORTIE"
echo "Contrôle qualité : lintian \"$SORTIE\" ; contenu : dpkg -c \"$SORTIE\""
