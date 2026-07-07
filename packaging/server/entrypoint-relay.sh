#!/bin/sh
# Point d'entrée nd-relay : attend la clé publique d'autorité publiée par nd-api
# (volume partagé), puis démarre le relais. Sans cette clé, le relais refuse les
# tickets (fermé par défaut) et n'ouvre pas.
set -eu

ADRESSE="${ND_RELAY_ADDR:-0.0.0.0:9100}"
PUBFILE="${ND_AUTHORITY_PUB:-/authority/autorite.pub}"
ATTENTE_MAX="${ND_ATTENTE_MAX:-120}"

echo "nd-relay : attente de la clé d'autorité (${PUBFILE})…"
i=0
while [ ! -s "${PUBFILE}" ]; do
    i=$((i + 1))
    if [ "${i}" -gt "${ATTENTE_MAX}" ]; then
        echo "nd-relay : clé d'autorité introuvable après ${ATTENTE_MAX}s" >&2
        exit 1
    fi
    sleep 1
done

CLE="$(cat "${PUBFILE}")"
echo "nd-relay : clé d'autorité=${CLE} ; écoute sur ${ADRESSE}"
exec nd-relay "${CLE}" "${ADRESSE}"
