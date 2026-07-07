#!/usr/bin/env bash
# Construit un DMG « glisser vers Applications » autour de NovaDesk.app.
# Le bundle doit être DÉJÀ signé (voir codesign-notarize.sh) avant l'emballage.
#
# Usage : build-dmg.sh [chemin/NovaDesk.app] [chemin/sortie.dmg]
set -euo pipefail

ICI="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../common/version.env
. "$ICI/../common/version.env"

APP="${1:-ui/build/macos/Build/Products/Release/${NOVADESK_APP_NAME}.app}"
SORTIE="${2:-dist/${NOVADESK_APP_NAME}-${NOVADESK_VERSION}-universal.dmg}"
VOLNAME="${NOVADESK_APP_NAME} ${NOVADESK_VERSION}"

[ -d "$APP" ] || { echo "erreur : bundle introuvable : $APP" >&2; exit 1; }
mkdir -p "$(dirname "$SORTIE")"
rm -f "$SORTIE"

if command -v create-dmg >/dev/null 2>&1; then
    # Rendu soigné (icônes positionnées, lien Applications).
    create-dmg \
        --volname "$VOLNAME" \
        --icon "${NOVADESK_APP_NAME}.app" 150 190 \
        --app-drop-link 450 190 \
        --window-size 600 400 \
        "$SORTIE" "$APP"
else
    echo "create-dmg absent : repli hdiutil (DMG fonctionnel, sans mise en page)."
    STAGE="$(mktemp -d)"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    hdiutil create -volname "$VOLNAME" -srcfolder "$STAGE" -ov -format UDZO "$SORTIE"
    rm -rf "$STAGE"
fi

echo "DMG écrit : $SORTIE"
