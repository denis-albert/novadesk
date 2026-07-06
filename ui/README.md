# NovaDesk UI — application Flutter (plan 10)

Interface client de **NovaDesk** (bureau à distance), en Flutter/Dart,
pilotée par le cœur Rust du workspace (`novadesk/crates/*`) via la façade
`nd-ffi` et `flutter_rust_bridge`.

> **État actuel** : l'UI est complète et navigable avec une façade **fictive**
> (`lib/bridge/mock_api.dart`) qui reproduit la logique et les messages
> d'erreur français de `crates/nd-ffi/src/api.rs`. Le binding réel se génère
> en une commande (voir ci-dessous). Ce code a été écrit sur un poste **sans
> SDK Flutter** : il n'a pas encore été compilé — à faire sur un poste équipé.

## Prérequis

- **SDK Flutter** ≥ 3.27 (canaux stable), Dart ≥ 3.6 ;
- **Toolchain Rust** (pour compiler le cœur et générer le binding) ;
- `flutter_rust_bridge_codegen` : `cargo install flutter_rust_bridge_codegen`.

## Démarrage rapide (avec la façade fictive)

```bash
cd novadesk/ui

# 1. Générer les coquilles de plateforme (windows/, macos/, linux/…),
#    non versionnées dans ce dépôt :
flutter create --platforms=windows,macos,linux --project-name novadesk_ui .

# 2. Dépendances :
flutter pub get

# 3. Lancer (au choix) :
flutter run -d windows
flutter run -d macos
flutter run -d linux
```

L'application démarre sur l'écran d'accueil avec un poste local fictif
(ID `936 271 048`). Toute la navigation fonctionne sans le cœur Rust.

## Générer le vrai binding Rust

La configuration est dans `flutter_rust_bridge.yaml` (entrée `crate::api`
du crate `../crates/nd-ffi`, sortie `lib/bridge/generated/`) :

```bash
cd novadesk/ui
flutter_rust_bridge_codegen generate
```

Puis brancher l'adaptateur `FrbNativeApi` à la place du mock — procédure
détaillée dans **`lib/bridge/README.md`** (un seul provider à changer).

## Structure

```
ui/
├── pubspec.yaml               # novadesk_ui : flutter_rust_bridge, riverpod, window_manager
├── flutter_rust_bridge.yaml   # config codegen (rust_root: ../crates/nd-ffi)
├── lib/
│   ├── main.dart              # point d'entrée, thèmes Material 3 clair/sombre, routes
│   ├── bridge/
│   │   ├── native_api.dart    # interface NativeApi + DTO, miroir de nd-ffi::api
│   │   ├── mock_api.dart      # implémentation fictive (UI navigable sans Rust)
│   │   └── README.md          # génération et branchement du binding FRB
│   ├── state/
│   │   └── providers.dart     # providers Riverpod (façade, thème, carnet fictif…)
│   ├── screens/
│   │   ├── home_screen.dart       # mon ID + mot de passe, ID distant, mode, carnet
│   │   ├── session_screen.dart    # surface vidéo (Texture), barre d'outils, entrées
│   │   ├── settings_screen.dart   # interface, réseau, sécurité, à propos
│   │   └── unattended_screen.dart # accès non-surveillé (mdp permanent, confiance)
│   └── widgets/
│       ├── nova_button.dart          # bouton principal avec état « en cours »
│       ├── nova_id_field.dart        # champ ID formaté par groupes de 3
│       └── session_state_badge.dart  # badge des états SessionStateDto (libellés fr)
└── test/widget_test.dart      # test de fumée (accueil + ID formaté)
```

## Écrans

| Écran | Contenu | Façade `nd-ffi` exercée |
|---|---|---|
| **Accueil** | ID local formaté + copie, mot de passe éphémère régénérable, champ « Entrez l'ID distant » (groupes de 3), modes Contrôle / Observation / Transfert seul, sessions récentes & carnet, bouton Paramètres | `format_nova_id`, `parse_nova_id`, `new_session_config` (erreurs françaises affichées telles quelles) |
| **Session** | Surface vidéo, barre d'outils (moniteurs, qualité, plein écran F11, Ctrl+Alt+Suppr, chat, transfert de fichiers, fin de session), barre d'état (badge d'état, chiffrement, SAS, compteurs d'entrées) | `session_status`, `encode_input_event` (souris absolue normalisée, boutons, molette en crans, scancodes USB-HID, Unicode) |
| **Paramètres** | Thème clair/sombre/système, langue, réseau, sécurité (confirmation, liste blanche), empreinte, à propos | `app_info` |
| **Accès non-surveillé** | Activation du service, mot de passe permanent + force, appareils de confiance, TOTP / journalisation / Wake-on-LAN | `parse_nova_id`, `format_nova_id` |

## Rendu vidéo temps réel (plan 10 §10.3)

Le flux vidéo **ne passe pas** par le pont de données. Le décodeur matériel
du cœur Rust produit des trames **en mémoire GPU** ; le cœur les expose à
Flutter comme **texture externe** (crate `irondash_texture`, un chemin GPU
par OS : D3D11 partagé, IOSurface, dmabuf/EGL, SurfaceTexture,
CVPixelBuffer). L'UI ne reçoit qu'un **`textureId` entier** et compose la
trame avec le widget `Texture` — **zéro copie CPU**. `SessionScreen`
contient déjà l'emplacement (`_textureId`), avec un panneau d'attente tant
que le cœur ne publie pas de texture.

## Notes

- **Français partout** : libellés d'UI et messages d'erreur (ceux de la
  façade Rust sont affichés sans transformation). i18n multilingue (ARB)
  planifiée, plan 10 §10.7.2.
- **Riverpod** sans logique métier : les providers exposent la façade et de
  l'état de présentation ; le cœur reste la source de vérité (plan 10 §10.2).
- Les données « poste local », carnet, chat et file de transfert sont
  **fictives** en attendant les flux (`Stream` FRB) du cœur.
