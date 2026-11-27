#! /usr/bin/env nix
#! nix develop .#ops --command just --justfile

install:
  @echo "🚧 Installing metor and metor-db to ~/.local/bin"
  cargo build --release --package metor --package metor-db
  cp target/release/metor target/release/metor-db ~/.local/bin

# Run the adcs-fsw3 star-tracker sim and open metor-panel on it.
star-demo:
  examples/adcs-fsw3/run-star-demo.sh

# Check that metor-gpui builds for the browser: single-threaded, on stable.
check-gpui-wasm:
  cargo check -p metor-gpui --target wasm32-unknown-unknown

# Build the metor-gpui web gallery into target/web_gallery; serve that directory to try it.
# Needs wasm-bindgen-cli at the version in Cargo.lock.
gpui-gallery:
  cargo build -p metor-gpui --example web_gallery --target wasm32-unknown-unknown --profile wasm-release
  wasm-bindgen --target web --out-dir target/web_gallery target/wasm32-unknown-unknown/wasm-release/examples/web_gallery.wasm
  cp libs/metor-gpui/examples/web_gallery/index.html target/web_gallery/
