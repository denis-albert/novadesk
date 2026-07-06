//! Script de build de `nd-ffi`.
//!
//! `flutter_rust_bridge` (via l'attribut `#[frb(...)]`) émet du code gardé par
//! `#[cfg(frb_expand)]` — un cfg que le codegen active uniquement lors de son
//! `cargo expand`. On le déclare ici pour que le lint `unexpected_cfgs` ne le
//! signale pas lors des `cargo build`/`clippy` ordinaires.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(frb_expand)");
}
