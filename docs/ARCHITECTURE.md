# Architecture de NovaDesk — vue d'ensemble

Cette page donne la carte du workspace en une lecture. Le **détail normatif** est
dans [`../../plan-technique/`](../../plan-technique/) : chaque section renvoie au
fichier de plan correspondant.

## 1. Le pipeline d'une session

Une session relie un **hôte** (la machine contrôlée) et un **viewer** (celui qui
regarde et pilote). La vidéo descend dans un sens, les entrées remontent dans
l'autre, le tout multiplexé sur **une seule connexion QUIC** en canaux logiques
(plan 04) :

```
        HÔTE (contrôlé)                              VIEWER (contrôleur)
  ─────────────────────────────              ──────────────────────────────
   écran, audio, presse-papiers                affichage, haut-parleurs

   nd-capture   capture d'écran                fenêtre viewer (démo minifb ;
   (DXGI aujourd'hui ; PipeWire,               UI Flutter à terme via nd-ffi,
    ScreenCaptureKit à venir)                  plan 10)
        │                                              ▲
        ▼                                              │
   nd-codec     encodage H.264                 nd-codec     décodage H.264
   (Media Foundation / openh264)               (openh264)
        │                                              ▲
        ▼                                              │
  ┌──────────────┐                             ┌──────────────┐
  │ nd-transport │  canaux logiques sur QUIC   │ nd-transport │
  │ (quinn, FEC) │═════════════════════════════│ (quinn, FEC) │
  └──────────────┘   vidéo · audio · input     └──────────────┘
        ▲            contrôle · fichiers               │
        │                                              ▼
   nd-input     injection des entrées          entrées clavier/souris/tactile
   (SendInput, SAS, tactile)                   saisies par l'utilisateur

   nd-audio : WASAPI (loopback/micro) → Opus → canal audio → décodage/lecture
   nd-files : chunks BLAKE3 + reprise, presse-papiers partagé → canal fichiers
```

Par-dessus le chiffrement de saut de QUIC (TLS 1.3), **nd-crypto** établit une
session **Noise XX** de bout en bout sur le canal de contrôle (plan 06) : même un
relais intermédiaire ne peut rien déchiffrer. Les empreintes croisées des clés
publiques permettent la vérification mutuelle (SAS).

**nd-core** orchestre le tout : machine à états de session, assemblage
capture→encode→transport côté hôte (`HostPipeline`) et réception→décodage côté
viewer (`ViewerPipeline`), permissions appliquées via **nd-features** (plan 13).

## 2. Mise en relation (connectivité)

Avant le pipeline, il faut se trouver (plan 05) :

```
   client A                                          client B
      │  1. publie/résout un ID                          │
      ▼                                                  ▼
  nd-signaling ────────▶ server/nd-rendezvous ◀──────── nd-signaling
                          (ID → adresse + certificat)
      │                                                  │
      │◀════ 2. connexion directe QUIC (P2P) ═══════════▶│
      │                                                  │
      └──────▶ server/nd-relay ◀─────────────────────────┘
               3. repli : relais « tuyau chiffré aveugle »
                  quand le P2P échoue (NAT symétriques…)
```

Côté infrastructure, **server/nd-accounts** (comptes, Argon2id, TOTP) et
**server/nd-api** (carnet d'adresses, licences, mises à jour) complètent le
backend (plan 11).

## 3. Empilement des crates

Du bas (fondation) vers le haut (intégrations) :

```
                     ┌────────┐   ┌─────────┐
   intégrations      │ nd-ffi │   │ nd-wasm │      UI Flutter (plan 10),
                     └───┬────┘   └────┬────┘      client web (plan 12)
                         └─────┬───────┘
                         ┌─────▼─────┐
   orchestration         │  nd-core  │             machine à états de session
                         └─────┬─────┘
             ┌──────────┬──────┼───────┬───────────┬────────────┐
   domaines  │          │      │       │           │            │
        nd-capture  nd-codec  nd-input  nd-audio  nd-files  nd-features
             └──────────┴──────┼───────┴───────────┴────────────┘
                               │
        nd-transport      nd-crypto      nd-signaling          (réseau & sécurité)
             └────────────────┼──────────────────┘
                         ┌────▼─────┐
   fondation             │ nd-proto │              types partagés, erreurs,
                         └──────────┘              versions de protocole

   serveurs (binaires) : nd-rendezvous · nd-relay · nd-accounts · nd-api
                         (ne dépendent que de nd-proto / nd-signaling)
```

Règles structurantes (plan 01) :

- **nd-proto** est la seule dépendance universelle : types, erreurs (`NdError`),
  identifiants (`MonitorId`, `ChannelKind`…). Aucune crate domaine ne dépend d'une
  autre crate domaine (exception : `nd-codec` consomme les frames de `nd-capture`).
- Chaque domaine expose un **trait** portable (`ScreenCapturer`, `VideoEncoder`,
  `InputInjector`, `Transport`…) et des fabriques (`create_*`) ; les
  implémentations par OS vivent derrière `#[cfg(...)]`. Un OS sans backend renvoie
  `NdError::NotImplemented` — le workspace compile partout, en permanence.
- L'`unsafe` (FFI Win32, codecs) est confiné dans des modules dédiés, chaque bloc
  documenté `// SAFETY:` (voir [`CONTRIBUTING.md`](../CONTRIBUTING.md)).

## 4. Où lire le détail

| Sujet | Plan |
|---|---|
| Architecture globale, découpage en crates | [01](../../plan-technique/01-architecture-globale.md) |
| Capture d'écran par OS | [02](../../plan-technique/02-capture-ecran.md) |
| Codec vidéo (matériel d'abord) | [03](../../plan-technique/03-codec-video.md) |
| Transport QUIC, canaux, FEC | [04](../../plan-technique/04-transport-reseau.md) |
| Rendez-vous, NAT traversal, relais | [05](../../plan-technique/05-connectivite-nat.md) |
| Sécurité, Noise, vérification | [06](../../plan-technique/06-securite-chiffrement.md) |
| Injection d'entrées | [07](../../plan-technique/07-injection-entrees.md) |
| Audio | [08](../../plan-technique/08-audio.md) |
| Fichiers et presse-papiers | [09](../../plan-technique/09-fichiers-clipboard.md) |
| UI client (Flutter) | [10](../../plan-technique/10-interface-client.md) |
| Backend / infrastructure | [11](../../plan-technique/11-backend-infrastructure.md) |
| Portage multiplateforme | [12](../../plan-technique/12-multiplateforme.md) |
| Fonctionnalités avancées, permissions | [13](../../plan-technique/13-fonctionnalites-avancees.md) |
| Tests, qualité, performance | [14](../../plan-technique/14-tests-qualite-performance.md) |
| Déploiement, packaging, mises à jour | [15](../../plan-technique/15-deploiement-mise-a-jour.md) |
