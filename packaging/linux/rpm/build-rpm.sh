#!/usr/bin/env bash
# Construit un .rpm NovaDesk via rpmbuild, en injectant le bundle Flutter et les
# fichiers d'intégration par --define (aucune métadonnée dans les Cargo.toml).
#
# Usage : build-rpm.sh [bundle_flutter_linux] [dossier_sortie]
set -euo pipefail

ICI="$(cd "$(dirname "$0")" && pwd)"
LINUX="$(cd "$ICI/.." && pwd)"
# shellcheck source=../../common/version.env
. "$LINUX/../common/version.env"

BUNDLE="${1:-ui/build/linux/x64/release/bundle}"
SORTIE="${2:-dist}"

[ -d "$BUNDLE" ] || { echo "erreur : bundle Flutter introuvable : $BUNDLE" >&2; exit 1; }
mkdir -p "$SORTIE"

TOPDIR="$(mktemp -d)"
mkdir -p "$TOPDIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

rpmbuild -bb \
    --define "_topdir $TOPDIR" \
    --define "stagedir $(cd "$BUNDLE" && pwd)" \
    --define "desktopfile $LINUX/novadesk.desktop" \
    --define "unitfile $LINUX/systemd/novadesk.service" \
    --define "version ${NOVADESK_VERSION}" \
    "$ICI/novadesk.spec"

find "$TOPDIR/RPMS" -name '*.rpm' -exec cp {} "$SORTIE/" \;
rm -rf "$TOPDIR"
echo "RPM(s) copiés dans : $SORTIE"
echo "Contrôle qualité : rpmlint \"$SORTIE\"/*.rpm"
