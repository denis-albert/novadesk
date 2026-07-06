//! `nd-ffi` — surface d'API exposée à l'interface Flutter.
//!
//! L'intégration `flutter_rust_bridge` (génération du binding Dart, `StreamSink` pour
//! les événements de session) sera ajoutée ici. Voir
//! `../../plan-technique/10-interface-client.md`. Pour l'instant, une API Rust simple
//! et testable tient lieu de contrat.
//!
//! La façade orientée UI (types plats « DTO » + fonctions synchrones renvoyant
//! `Result<_, String>`) vit dans le module [`api`], ré-exporté à la racine.

// Binding généré par `flutter_rust_bridge_codegen generate` (config dans
// `ui/flutter_rust_bridge.yaml`). `unsafe` toléré : code FFI généré, non écrit
// à la main ; ne pas éditer (régénéré à chaque `generate`).
#[allow(unsafe_code)]
mod frb_generated;

pub mod api;

pub use api::*;

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
