#!/usr/bin/env bash
# Construit le module WebAssembly de `nd-wasm` et le place dans `web/pkg/`, prêt à être
# chargé par `web/index.html`.
#
# IMPORTANT — API « unstable » : WebTransport et WebCodecs sont des liaisons web-sys
# encore instables ; la compilation wasm EXIGE le drapeau `--cfg=web_sys_unstable_apis`
# (transmis ici via RUSTFLAGS). Sans lui, `web_sys::WebTransport` / `VideoDecoder`
# n'existent pas et la compilation échoue.
#
# Cible wasm requise (une seule fois) :
#   rustup target add wasm32-unknown-unknown
#
# Depuis le dossier de cette crate : `crates/nd-wasm/`.
set -euo pipefail

export RUSTFLAGS="--cfg=web_sys_unstable_apis"

if command -v wasm-pack >/dev/null 2>&1; then
  # Chemin recommandé : wasm-pack génère pkg/nd_wasm.js + pkg/nd_wasm_bg.wasm (cible web,
  # modules ES natifs importables par index.html sans bundler).
  wasm-pack build --target web --out-dir web/pkg --release
else
  # Repli sans wasm-pack : cargo build + wasm-bindgen CLI (mêmes artefacts dans web/pkg).
  #   cargo install wasm-bindgen-cli   # doit correspondre à la version de la crate
  echo "wasm-pack absent — repli cargo + wasm-bindgen-cli"
  cargo build -p nd-wasm --target wasm32-unknown-unknown --release \
    --manifest-path ../../Cargo.toml
  wasm-bindgen ../../target/wasm32-unknown-unknown/release/nd_wasm.wasm \
    --target web --out-dir web/pkg
fi

echo "OK : servez ce dossier en HTTPS (WebCodecs/WebTransport l'exigent), ex. :"
echo "     python -m http.server 8000   # puis ouvrez https://…/web/index.html"
