# Serveurs NovaDesk — images Docker + orchestration

Comble l'absence d'orchestration (tâche 4) : un `docker compose` lance les
**4 serveurs** ensemble, sur un réseau interne, avec des volumes de persistance.

| Service | Binaire | Port | Rôle |
|---|---|---|---|
| `nd-api` | `nd-api` | 9300 | API applicative + **autorité de signature** (Ed25519) |
| `nd-rendezvous` | `nd-rendezvous` | 9000 | Rendez-vous / signalisation par ID |
| `nd-relay` | `nd-relay` | 9100 | Relais opaque (tickets signés) |
| `nd-accounts` | `nd-accounts` | 9200 | Comptes / auth / 2FA / OIDC / licences |

## Le point délicat : l'amorçage de la clé d'autorité

`nd-rendezvous` et `nd-relay` sont **fermés par défaut** : ils exigent, en
argument, la **clé publique Ed25519 de l'autorité** — celle que `nd-api` génère
et **affiche à son démarrage**. Il y a donc une dépendance d'amorçage.

La solution retenue, sans modifier une ligne de Rust :

1. `nd-api` écrit sa graine d'autorité dans le volume partagé `nd-authority`
   (`/authority/autorite.cle`, déterministe et persistant) ;
2. son point d'entrée lit la sortie de `nd-api` et, à la ligne
   « … clé publique : `<hex>` », écrit la clé dans `/authority/autorite.pub`
   (écriture atomique) ;
3. les points d'entrée de `nd-rendezvous` / `nd-relay` **attendent** ce fichier
   (montage en lecture seule) puis démarrent avec la clé.

`depends_on: condition: service_healthy` garantit en plus que `nd-api` écoute
avant que les autres tentent leur lecture. Ceinture et bretelles.

## Démarrer

```bash
cd packaging/server
cp .env.example .env          # optionnel : compte racine, OIDC, secret comptes
docker compose up --build     # construit l'image commune puis lance les 4 services
```

Vérifier :

```bash
docker compose ps             # les 4 services « healthy »
docker compose logs nd-api    # « clé publique : … » puis « clé d'autorité publiée »
docker compose logs nd-relay  # « clé d'autorité=… ; écoute sur 0.0.0.0:9100 »
```

Arrêter (en conservant les données) : `docker compose down`.
Tout supprimer, **volumes compris** : `docker compose down -v`.

## Persistance

| Volume | Monté sur | Contenu |
|---|---|---|
| `nd-authority` | `/authority` | graine + clé publique d'autorité (partagé, RO pour rdv/relay) |
| `nd-api-data` | `/data` | état durable de nd-api (`etat.json`) |
| `nd-accounts-data` | `/accounts` | base redb des comptes + `comptes.redb.cle` |

> Sauvegarder `nd-accounts-data` **avec** son fichier `.cle` : sans lui, les
> secrets TOTP en base sont indéchiffrables et les jetons applicatifs changent.

## Sécurité

- Les serveurs tournent en utilisateur non-root (`uid 10001`) dans une image mince.
- Aucun secret dans l'image ni dans le compose : tout vient de l'environnement.
- Réseau `novadesk` en bridge ; n'exposer sur l'hôte que les ports nécessaires
  (retirer les `ports:` non exposés au public en production, p. ex. garder 9000
  et 9100 ouverts mais placer 9200/9300 derrière un reverse-proxy/VPN).

## Vérifiable ici vs sur machine de build

- **Vérifié ici** : syntaxe YAML du compose, syntaxe POSIX des points d'entrée
  (`sh -n`), cohérence ports/volumes/arguments avec le code des serveurs.
- **À valider sur une machine avec Docker** : `docker compose config`, le build
  multi-étapes (tag `rust:1.90-bookworm`), l'amorçage réel de la clé et les
  healthchecks `nc`. Aucun démon Docker n'est disponible sur le poste de dev.
