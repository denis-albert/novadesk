//! `nd-ffi` — surface d'API exposée à l'interface Flutter.
//!
//! La façade orientée UI (types plats « DTO » + fonctions synchrones renvoyant
//! `Result<_, String>`, flux via `StreamSink`) vit dans le module [`api`],
//! ré-exporté à la racine. Voir `../../plan-technique/10-interface-client.md`.
//! Depuis le lot « session live », elle pilote le moteur réel
//! ([`nd_core::SessionEngine`]) par identifiant de session opaque : démarrage
//! ([`api::start_session`]), flux d'états et de frames vidéo
//! ([`api::session_state_stream`], [`api::session_video_stream`]), entrées
//! ([`api::send_input`]), statistiques ([`api::session_stats`]) et arrêt
//! ([`api::stop_session`]).
//!
//! # Régénération du pont Dart (orchestrateur, hors de ce poste)
//!
//! Le binding Rust (`src/frb_generated.rs`) et le Dart (`ui/lib/bridge/generated`)
//! se régénèrent avec `flutter` et `flutter_rust_bridge_codegen` **2.12.0** (même
//! version que la crate `flutter_rust_bridge` de `Cargo.toml` et que le paquet Dart
//! de `ui/pubspec.lock`) dans le PATH :
//!
//! ```text
//! # 1. Retirer l'échafaudage de compilation pré-régénération :
//! #    supprimer crates/nd-ffi/src/pont_provisoire.rs
//! #    et la ligne `mod pont_provisoire;` ci-dessous.
//! #    (Oubli = conflit d'impl E0119 à la compilation, impossible à rater.)
//! # 2. Depuis novadesk/ui (lit flutter_rust_bridge.yaml : rust_input crate::api) :
//! flutter_rust_bridge_codegen generate
//! ```
//!
//! Nouvelles fonctions que le binding Dart exposera après régénération :
//! `start_session`, `session_listen_info`, `session_state_stream`
//! (→ `Stream<SessionStateDto>`), `session_video_stream` (→ `Stream<VideoFrameDto>`),
//! `wait_session_state`, `collect_video_frames`, `session_stats`,
//! `session_last_error`, `send_input`, `stop_session` — et leurs DTO
//! `VideoFrameDto`, `SessionStatsDto`, `SessionEndpointDto`, `ListenInfoDto`.

// Binding généré par `flutter_rust_bridge_codegen generate` (config dans
// `ui/flutter_rust_bridge.yaml`). `unsafe` toléré : code FFI généré, non écrit
// à la main ; ne pas éditer (régénéré à chaque `generate`).
#[allow(unsafe_code)]
mod frb_generated;

pub mod api;

/// Gestion interne des sessions live (table statique + threads de drainage).
/// Hors du périmètre scanné par le codegen (`rust_input: crate::api`).
mod flux;

pub use api::*;

// `StreamSink` (défini par le pont généré) apparaît dans les signatures publiques de
// `api` : ce ré-export le rend publiquement visible (sinon lint `private_interfaces`).
pub use frb_generated::StreamSink;

/// Version du moteur, exposée à l'UI (ex. écran « À propos »).
#[must_use]
pub fn engine_version_string() -> String {
    nd_core::engine_version().to_string()
}

/// Formate un ID NovaDesk pour affichage (groupé par 3).
#[must_use]
pub fn format_id(id: u64) -> String {
    nd_proto::NovaId(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_non_vide() {
        assert!(!engine_version_string().is_empty());
    }

    #[test]
    fn format_id_groupe() {
        assert_eq!(format_id(123_456_789), "123 456 789");
    }
}
