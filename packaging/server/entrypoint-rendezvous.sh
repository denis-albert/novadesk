#!/bin/sh
# Point d'entrée nd-rendezvous : attend la clé publique d'autorité publiée par
# nd-api (volume partagé), puis démarre le serveur de rendez-vous. Sans cette
# clé, le rendez-vous refuse d'ouvrir (enregistrement par preuve de possession).
set -eu

ADRESSE="${ND_RDV_ADDR:-0.0.0.0:9000}"
PUBFILE="${ND_AUTHORITY_PUB:-/authority/autorite.pub}"
ATTENTE_MAX="${ND_ATTENTE_MAX:-120}"

echo "nd-rendezvous : attente de la clé d'autorité (${PUBFILE})…"
i=0
while [ ! -s "${PUBFILE}" ]; do
    i=$((i + 1))
    if [ "${i}" -gt "${ATTENTE_MAX}" ]; then
        echo "nd-rendezvous : clé d'autorité introuvable après ${ATTENTE_MAX}s" >&2
        exit 1
    fi
    sleep 1
done

CLE="$(cat "${PUBFILE}")"
echo "nd-rendezvous : clé d'autorité=${CLE} ; écoute sur ${ADRESSE}"
exec nd-rendezvous "${CLE}" "${ADRESSE}"
