#!/usr/bin/env bash
# Construit un .pkg NovaDesk qui installe l'app ET le LaunchDaemon root (accès
# non surveillé, plan 15 §15.3.2). Le .pkg doit être signé (Developer ID
# Installer) puis notarisé pour passer Gatekeeper.
#
# Env : NOVADESK_MAC_DEV_ID_INSTALLER « Developer ID Installer: … (TEAMID) »
#       DRY_RUN=1  n'affiche que les commandes.
#
# Usage : build-pkg.sh [chemin/NovaDesk.app] [chemin/sortie.pkg]
set -euo pipefail

ICI="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../common/version.env
. "$ICI/../common/version.env"

APP="${1:-ui/build/macos/Build/Products/Release/${NOVADESK_APP_NAME}.app}"
SORTIE="${2:-dist/${NOVADESK_APP_NAME}-${NOVADESK_VERSION}.pkg}"
DEV_ID_INSTALLER="${NOVADESK_MAC_DEV_ID_INSTALLER:-}"
DRY_RUN="${DRY_RUN:-0}"

run() { echo "+ $*"; [ "$DRY_RUN" = "1" ] || "$@"; }

[ -d "$APP" ] || { echo "erreur : bundle introuvable : $APP" >&2; exit 1; }
mkdir -p "$(dirname "$SORTIE")"

# Racine de payload : /Applications/NovaDesk.app + /Library/LaunchDaemons/…plist
RACINE="$(mktemp -d)"
mkdir -p "$RACINE/Applications" "$RACINE/Library/LaunchDaemons"
cp -R "$APP" "$RACINE/Applications/"
cp "$ICI/launchd/com.novadesk.NovaDesk.plist" "$RACINE/Library/LaunchDaemons/"

COMPOSANT="$(mktemp -d)/novadesk-component.pkg"
run pkgbuild --root "$RACINE" \
    --identifier "$NOVADESK_APP_ID" \
    --version "$NOVADESK_VERSION" \
    --scripts "$ICI/scripts" \
    --install-location "/" \
    "$COMPOSANT"

if [ -n "$DEV_ID_INSTALLER" ]; then
    run productbuild --package "$COMPOSANT" --sign "$DEV_ID_INSTALLER" "$SORTIE"
else
    echo "AVERTISSEMENT : pas d'identité Installer — .pkg NON signé (dev uniquement)."
    run productbuild --package "$COMPOSANT" "$SORTIE"
fi

rm -rf "$RACINE"
echo "PKG écrit : $SORTIE (à notariser ensuite : xcrun notarytool submit \"$SORTIE\" …)"
