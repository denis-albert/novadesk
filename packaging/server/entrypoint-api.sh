#!/bin/sh
# Point d'entrée nd-api : démarre l'API et **publie la clé publique d'autorité**
# (Ed25519) dans le volume partagé, pour que nd-rendezvous et nd-relay puissent
# la consommer (ils refusent de démarrer sans elle — « fermés par défaut »).
#
# nd-api n'a pas de mode « imprime la clé et sors » : on lit donc sa sortie au
# vol et, à la ligne « … clé publique : <hex> », on écrit la clé (écriture
# atomique via un fichier temporaire renommé).
set -eu

ADRESSE="${ND_API_ADDR:-0.0.0.0:9300}"
ETAT="${ND_API_STATE:-/data/etat.json}"
SEED="${ND_AUTHORITY_SEED:-/authority/autorite.cle}"
PUBFILE="${ND_AUTHORITY_PUB:-/authority/autorite.pub}"
RACINE="${ND_COMPTE_RACINE:-}"

echo "nd-api : démarrage sur ${ADRESSE} (état=${ETAT}, autorité=${SEED})"

# Arguments positionnels : [adresse] [état] [clé-autorité] [compte-racine].
# On n'ajoute le compte racine que s'il est fourni (sinon administration
# verrouillée, ce qui est le comportement sûr par défaut).
set -- "${ADRESSE}" "${ETAT}" "${SEED}"
[ -n "${RACINE}" ] && set -- "$@" "${RACINE}"

nd-api "$@" 2>&1 | while IFS= read -r ligne; do
    printf '%s\n' "${ligne}"
    case "${ligne}" in
        *"publique : "*)
            cle="${ligne##*publique : }"
            printf '%s' "${cle}" > "${PUBFILE}.tmp" && mv "${PUBFILE}.tmp" "${PUBFILE}"
            echo "nd-api : clé d'autorité publiée dans ${PUBFILE}"
            ;;
    esac
done
