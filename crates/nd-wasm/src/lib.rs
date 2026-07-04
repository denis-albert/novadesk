//! `nd-wasm` — point d'entrée du client web (cœur Rust compilé en WebAssembly).
//!
//! L'intégration `wasm-bindgen`, WebTransport/WebRTC, WebCodecs et le rendu WebGL
//! seront ajoutés ici. Voir `../../plan-technique/12-multiplateforme.md`. Le code
//! compile aussi sur l'hôte (cible de test) tant que les dépendances wasm ne sont pas
//! introduites.

/// Version du moteur, exposée au client web.
#[must_use]
pub fn engine_version_string() -> String {
    nd_core::engine_version().to_string()
}
