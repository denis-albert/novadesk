#!/usr/bin/env bash
# Construit une AppImage NovaDesk (binaire portable, sans installation).
#
# Usage : build-appimage.sh [bundle_flutter_linux] [sortie.AppImage]
set -euo pipefail

ICI="$(cd "$(dirname "$0")" && pwd)"
LINUX="$(cd "$ICI/.." && pwd)"
# shellcheck source=../../common/version.env
. "$LINUX/../common/version.env"

BUNDLE="${1:-ui/build/linux/x64/release/bundle}"
SORTIE="${2:-dist/NovaDesk-${NOVADESK_VERSION}-x86_64.AppImage}"

[ -d "$BUNDLE" ] || { echo "erreur : bundle Flutter introuvable : $BUNDLE" >&2; exit 1; }
mkdir -p "$(dirname "$SORTIE")"

APPDIR="$(mktemp -d)/NovaDesk.AppDir"
install -d "$APPDIR/usr/lib/novadesk" \
          "$APPDIR/usr/share/applications" \
          "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp -R "$BUNDLE/." "$APPDIR/usr/lib/novadesk/"
install -m 0755 "$ICI/AppRun" "$APPDIR/AppRun"
# La spéc AppImage veut un .desktop et une icône à la racine de l'AppDir.
install -m 0644 "$LINUX/novadesk.desktop" "$APPDIR/novadesk.desktop"
install -m 0644 "$LINUX/novadesk.desktop" "$APPDIR/usr/share/applications/novadesk.desktop"

ICONE="$LINUX/novadesk.png"
if [ -f "$ICONE" ]; then
    install -m 0644 "$ICONE" "$APPDIR/novadesk.png"
    install -m 0644 "$ICONE" "$APPDIR/usr/share/icons/hicolor/256x256/apps/novadesk.png"
else
    echo "AVERTISSEMENT : icône novadesk.png absente — placeholder vide (à remplacer)."
    : > "$APPDIR/novadesk.png"
fi

if command -v appimagetool >/dev/null 2>&1; then
    ARCH=x86_64 appimagetool "$APPDIR" "$SORTIE"
    echo "AppImage écrite : $SORTIE"
else
    echo "appimagetool absent : AppDir prêt dans $APPDIR"
    echo "Empaqueter sur la machine de build : ARCH=x86_64 appimagetool \"$APPDIR\" \"$SORTIE\""
fi
