//! `nd-wasm` — client web NovaDesk (cœur Rust compilé en WebAssembly).
//!
//! Rôle : **client de visualisation à contrôle sortant** — comme le client web
//! d'AnyDesk, il pilote un pair mais n'est **jamais** piloté. Il se connecte à un
//! pair via **WebTransport**, **décode** le flux H.264 avec **WebCodecs**
//! (`VideoDecoder`), l'affiche sur un **Canvas**, et renvoie les **entrées**
//! souris/clavier au format binaire de `nd-proto` (canal `Input`). Voir
//! `../../plan-technique/12-multiplateforme.md` et le prompt
//! `parite-anydesk/04-prompts-fable/10-nd-wasm-client-web.md`.
//!
//! # Organisation & portabilité
//!
//! * [`entree`], [`demo`] et [`h264`] sont **purs** (aucune dépendance navigateur) :
//!   ils compilent et se testent sur l'hôte (`cargo test -p nd-wasm`) et alimentent
//!   aussi le build wasm.
//! * Le module `client` (API `#[wasm_bindgen]`, WebTransport, WebCodecs, Canvas) n'est
//!   compilé que pour `target_arch = "wasm32"`. Côté natif, `nd-wasm` reste un rlib
//!   quasi vide : `cargo build --workspace` l'ignore proprement (aucune dépendance
//!   navigateur n'est tirée — voir la table `[target.'cfg(target_arch = "wasm32")']`
//!   de `Cargo.toml`).
//!
//! # Dépendance d'infrastructure (hors périmètre de cette crate)
//!
//! WebTransport exige un **pont HTTP/3** côté serveur : le relais/rendez-vous NovaDesk
//! devra exposer un endpoint WebTransport traduisant vers le transport QUIC natif
//! (`nd-transport`). Tant que ce pont n'existe pas, le **mode démo**
//! ([`WebClient::demarrer_demo_codec`]) prouve le chemin decode→canvas **sans aucune
//! infrastructure** : il génère un flux H.264 de test avec le `VideoEncoder` du
//! navigateur, le décode avec le `VideoDecoder`, et peint le résultat sur le canvas.

pub mod demo;
pub mod entree;
pub mod h264;

// Le client navigateur (`#[wasm_bindgen]`) n'existe que pour la cible wasm.
// `unsafe_code` toléré ici : l'`unsafe` est **entièrement généré** par la macro
// `#[wasm_bindgen]` (glue ABI JS↔wasm) ; aucun bloc `unsafe` n'est écrit à la main
// dans ce module. Même politique que `nd-ffi` pour le code de pont généré.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod client;

#[cfg(target_arch = "wasm32")]
pub use client::WebClient;

/// Version du protocole/moteur, exposée au client web.
///
/// Identique à `nd_core::engine_version()` — toutes deux renvoient
/// [`nd_proto::ProtocolVersion::CURRENT`] — mais **sans** dépendre de `nd-core`, dont
/// la chaîne de dépendances (capture, codec natif, QUIC…) ne compile pas en wasm.
#[must_use]
pub fn engine_version_string() -> String {
    nd_proto::ProtocolVersion::CURRENT.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_non_vide_et_stable() {
        // La version exposée au web doit rester celle du protocole courant.
        assert_eq!(engine_version_string(), "0.1");
    }
}
