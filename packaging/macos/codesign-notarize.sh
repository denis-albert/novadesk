#!/usr/bin/env bash
# Signe (Developer ID + Hardened Runtime), notarise et agrafe un artefact macOS.
# Sans cette chaîne complète, Gatekeeper bloque l'app sur un poste tiers.
#
# Identités et secrets par l'ENVIRONNEMENT (jamais en dur) :
#   NOVADESK_MAC_DEV_ID_APP   « Developer ID Application: Nom (TEAMID) »  (requis)
#   NOVADESK_NOTARY_PROFILE   profil trousseau notarytool (recommandé)
#   — ou — NOVADESK_APPLE_ID + NOVADESK_TEAM_ID + NOVADESK_APP_PASSWORD
#   DRY_RUN=1                 n'affiche que les commandes (validation sans clés)
#
# Usage : codesign-notarize.sh [chemin/NovaDesk.app]
set -euo pipefail

ICI="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../common/version.env
. "$ICI/../common/version.env"

APP="${1:-ui/build/macos/Build/Products/Release/${NOVADESK_APP_NAME}.app}"
ENTITLEMENTS="$ICI/NovaDesk.entitlements"
DRY_RUN="${DRY_RUN:-0}"

: "${NOVADESK_MAC_DEV_ID_APP:?identité requise (Developer ID Application: … (TEAMID))}"

run() { echo "+ $*"; [ "$DRY_RUN" = "1" ] || "$@"; }

[ -d "$APP" ] || { echo "erreur : bundle introuvable : $APP" >&2; exit 1; }

# 1) Signer le code IMBRIQUÉ d'abord (dylibs/frameworks : nd_ffi, plugins),
#    puis l'app de premier niveau avec les entitlements. « --deep » est
#    déconseillé : on signe de l'intérieur vers l'extérieur.
echo "== codesign =="
while IFS= read -r -d '' cible; do
    run codesign --force --options runtime --timestamp \
        --sign "$NOVADESK_MAC_DEV_ID_APP" "$cible"
done < <(find "$APP/Contents" \( -name '*.dylib' -o -name '*.framework' \) -print0 2>/dev/null || true)

run codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" \
    --sign "$NOVADESK_MAC_DEV_ID_APP" "$APP"
run codesign --verify --deep --strict --verbose=2 "$APP"

# 2) Emballer en DMG puis notariser le DMG (l'app y est déjà signée).
DMG="dist/${NOVADESK_APP_NAME}-${NOVADESK_VERSION}-universal.dmg"
run "$ICI/build-dmg.sh" "$APP" "$DMG"

echo "== notarisation =="
if [ -n "${NOVADESK_NOTARY_PROFILE:-}" ]; then
    run xcrun notarytool submit "$DMG" --keychain-profile "$NOVADESK_NOTARY_PROFILE" --wait
else
    : "${NOVADESK_APPLE_ID:?}" "${NOVADESK_TEAM_ID:?}" "${NOVADESK_APP_PASSWORD:?}"
    run xcrun notarytool submit "$DMG" \
        --apple-id "$NOVADESK_APPLE_ID" \
        --team-id "$NOVADESK_TEAM_ID" \
        --password "$NOVADESK_APP_PASSWORD" --wait
fi

# 3) Agrafer le ticket : l'artefact se valide ensuite hors ligne.
run xcrun stapler staple "$DMG"
run xcrun stapler validate "$DMG"
echo "OK : $DMG signé, notarisé, agrafé."
