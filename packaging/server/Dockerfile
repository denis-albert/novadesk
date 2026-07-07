# syntax=docker/dockerfile:1
#
# Image des serveurs NovaDesk — construit les 4 binaires du dossier `server/`
# (nd-rendezvous, nd-relay, nd-accounts, nd-api) et les réunit dans une image
# d'exécution mince. Un seul `docker build` produit l'image commune ; le
# docker-compose lance les 4 services depuis cette image, avec un point d'entrée
# par service.
#
# Contexte de build attendu : la RACINE du dépôt (le workspace Cargo complet est
# nécessaire). Le compose s'en charge (`context: ../..`).
#
# ---------------------------------------------------------------------------
# Étape 1 — compilation (workspace Cargo, profil release)
# ---------------------------------------------------------------------------
FROM rust:1.90-bookworm AS builder

# Dépendances système identiques à la CI (voir .github/workflows/ci.yml) :
# cmake (libopus vendoré, nd-audio), nasm (openh264, nd-codec), ALSA/PipeWire
# (backends Linux à venir). Inoffensives pour les serveurs, mais garantissent
# que le workspace compile quelle que soit la fermeture transitive des deps.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        cmake nasm libasound2-dev libpipewire-0.3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# On ne compile QUE les 4 serveurs (pas l'UI/FFI, non pertinents ici).
RUN cargo build --release \
        -p nd-rendezvous -p nd-relay -p nd-accounts -p nd-api

# ---------------------------------------------------------------------------
# Étape 2 — exécution (image mince, utilisateur non-root)
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates : TLS sortant éventuel (OIDC côté nd-accounts).
# netcat-openbsd : sondes TCP des healthchecks du compose.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

# Utilisateur de service dédié (jamais root pour exécuter les serveurs).
RUN useradd --system --no-create-home --uid 10001 novadesk

COPY --from=builder /src/target/release/nd-rendezvous /usr/local/bin/nd-rendezvous
COPY --from=builder /src/target/release/nd-relay      /usr/local/bin/nd-relay
COPY --from=builder /src/target/release/nd-accounts   /usr/local/bin/nd-accounts
COPY --from=builder /src/target/release/nd-api        /usr/local/bin/nd-api
COPY packaging/server/entrypoint-*.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/entrypoint-*.sh

# Points de montage des volumes, créés avec la bonne appartenance : un volume
# NOMMÉ monté sur un répertoire existant hérite de son appartenance à la 1re
# initialisation — les serveurs (uid 10001) peuvent donc y écrire.
RUN mkdir -p /authority /data /accounts \
    && chown novadesk:novadesk /authority /data /accounts

USER novadesk
WORKDIR /home
