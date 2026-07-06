# `lib/bridge/` — pont Dart ↔ Rust (`nd-ffi`)

Ce dossier contient le **contrat Dart** de la façade Rust `crates/nd-ffi`
(module `api`) et son implémentation fictive :

| Fichier | Rôle |
|---|---|
| `native_api.dart` | Interface `NativeApi` + DTO Dart, **miroir manuel** de `crates/nd-ffi/src/api.rs` (`AppInfo`, `SessionRoleDto`, `SessionStateDto`, `SessionStatusDto`, `PermissionsDto`, `SessionConfigDto`, `InputEventDto`, `encode/decode_input_event`…). |
| `mock_api.dart` | `MockNativeApi implements NativeApi` : Dart pur, mêmes validations et messages d'erreur français que la façade Rust. Permet de lancer et naviguer dans l'UI **sans** le cœur Rust. |
| `generated/` | (ignoré par git) Binding réel produit par `flutter_rust_bridge_codegen`. |

## Générer le vrai binding

Prérequis : SDK Flutter, toolchain Rust, puis :

```bash
cargo install flutter_rust_bridge_codegen
```

Depuis `novadesk/ui/` (la configuration est lue dans `flutter_rust_bridge.yaml`,
qui pointe `rust_root: ../crates/nd-ffi` et `rust_input: crate::api`) :

```bash
flutter_rust_bridge_codegen generate
```

Le code Dart généré est écrit dans `lib/bridge/generated/`. Côté Rust, le
câblage FRB (annotations `#[flutter_rust_bridge::frb]`, `StreamSink` pour les
futurs flux d'événements) est prévu dans `nd-ffi` — voir l'en-tête de
`crates/nd-ffi/src/api.rs` et le plan `10-interface-client.md` §10.2.

## Brancher le binding dans l'UI

1. Créer un adaptateur qui délègue aux fonctions générées :

   ```dart
   // lib/bridge/frb_api.dart (à créer après la génération)
   import 'generated/api.dart' as frb;
   import 'native_api.dart';

   class FrbNativeApi implements NativeApi {
     @override
     Future<String> formatNovaId({required int id}) =>
         frb.formatNovaId(id: id);
     // … idem pour chaque méthode ; convertir BigInt <-> int si la
     // configuration FRB expose u64 en BigInt, et adapter
     // AnyhowException -> NovaApiException.
   }
   ```

2. Remplacer le mock dans `lib/state/providers.dart` :

   ```dart
   final nativeApiProvider = Provider<NativeApi>((ref) => FrbNativeApi());
   ```

3. Initialiser le runtime FRB au démarrage (`RustLib.init()` dans `main()`),
   selon le squelette généré.

Aucun écran n'importe le binding directement : tout passe par `NativeApi`,
le basculement mock ↔ réel se fait donc en un seul endroit.

## Correspondance des types

| Rust (`nd-ffi`) | Dart (`native_api.dart`) |
|---|---|
| `u64` | `int` (64 bits ; les ID NovaDesk font 9 chiffres) |
| `Result<T, String>` | `Future<T>` qui lève `NovaApiException(message français)` |
| `enum` sans données (`SessionRoleDto`, `SessionStateDto`) | `enum` Dart |
| `enum` à variantes porteuses (`InputEventDto`) | classe `sealed` + sous-classes `Input*` |
| `Vec<u8>` | `Uint8List` |

## Rendu vidéo (rappel, plan 10 §10.3)

La façade actuelle ne couvre pas encore la session temps réel : le flux vidéo
n'empruntera **pas** le pont de données. Le cœur Rust enregistrera une
**texture GPU externe** auprès de l'embedder Flutter (crate `irondash_texture`)
et ne transmettra à l'UI qu'un `textureId` entier, affiché par le widget
`Texture` — zéro copie CPU. `SessionScreen` contient déjà l'emplacement prévu.
