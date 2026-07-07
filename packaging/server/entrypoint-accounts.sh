#!/bin/sh
# Point d'entrée nd-accounts : service comptes/auth, indépendant de l'autorité
# de rendez-vous. Persistance redb sur volume dédié. Le secret serveur vient de
# ND_ACCOUNTS_SECRET (hex) si fourni ; sinon un fichier « <base>.cle » est
# auto-généré à côté de la base (à sauvegarder avec elle).
# Fédération OIDC : passer les variables ND_OIDC_* via l'environnement du compose.
set -eu

ADRESSE="${ND_ACCOUNTS_ADDR:-0.0.0.0:9200}"
BASE="${ND_ACCOUNTS_DB:-/accounts/comptes.redb}"

echo "nd-accounts : écoute sur ${ADRESSE} (base=${BASE})"
exec nd-accounts "${ADRESSE}" "${BASE}"
