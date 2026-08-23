#!/usr/bin/env bash
# Rebuild the checked-in guest template.
#
# The compiler never runs a toolchain: it embeds `src/prelude.wasm` and appends
# generated functions to it. That file is regenerated only when the prelude
# crate changes, which is the same bargain `tests/fixtures/seq-fixture` makes.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$here/src/prelude.wasm"

if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
    echo "installing the wasm32-unknown-unknown target" >&2
    rustup target add wasm32-unknown-unknown
fi

# wasm-ld defaults to a 1 MiB shadow stack placed first, so an untouched build
# claims 17 linear-memory pages before a single expression exists. Guest code
# has no deep recursion to fund — fuel bounds it long before the stack does —
# and one page of stack leaves the whole module inside three.
export RUSTFLAGS="-C link-arg=-zstack-size=65536 -C link-arg=--global-base=65536 ${RUSTFLAGS:-}"

cargo build \
    --manifest-path "$here/prelude/Cargo.toml" \
    --target wasm32-unknown-unknown \
    --profile wasm-release

built="$here/prelude/target/wasm32-unknown-unknown/wasm-release/metor_expr_prelude.wasm"
cp "$built" "$out"
echo "$out: $(wc -c < "$out" | tr -d ' ') bytes"
