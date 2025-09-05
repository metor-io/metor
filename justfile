#! /usr/bin/env nix
#! nix develop .#ops --command just --justfile

install:
  @echo "🚧 Installing metor and metor-db to ~/.local/bin"
  cargo build --release --package metor --package metor-db
  cp target/release/metor target/release/metor-db ~/.local/bin
