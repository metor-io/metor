#!/usr/bin/env bash
# Build the guest for wasm, then run the host harness against it.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release -p wasm-poll-guest --target wasm32-unknown-unknown
cargo run --release -p wasm-poll -- "$@"
