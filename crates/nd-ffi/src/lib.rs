//! `nd-ffi` — surface d'API exposée à l'interface Flutter.
//!
//! L'intégration `flutter_rust_bridge` (génération du binding Dart, `StreamSink` pour
//! les événements de session) sera ajoutée ici. Voir
//! `../../plan-technique/10-interface-client.md`. Pour l'instant, une API Rust simple
//! et testable tient lieu de contrat.

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
