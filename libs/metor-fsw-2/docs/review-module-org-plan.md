# Module-organization review plan

## Problem
Unit tests live in crate-root sibling files named `tests_<module>.rs`, declared
from `lib.rs` as `mod tests_<module>`. The idiomatic Rust convention is for a
module's unit tests to be a `tests` *submodule of that module*. The crate already
uses directory modules (`src/wiring/`), so pattern (a) — directory module +
`<module>/tests.rs` — is the reference shape to follow.

## Mapping (tests_* file -> owning module)
- `tests_system.rs`      -> `system`      (WP4)
- `tests_abi.rs`         -> `abi`         (WP8, gated `feature = "kdl"`)
- `tests_telemetry.rs`   -> `telemetry`   (WP7)
- `tests_coordinator.rs` -> `coordinator` (WP5)
- `tests_wiring.rs`      -> `wiring`      (WP6, gated `feature = "kdl"`; already a dir module)
- `tests.rs`             -> crate-root WP3 frame tests, spans frame/dynamic/writer/reader.
  No single owning module -> keep as crate-root `mod tests` (already idiomatic, not a
  `tests_` sibling). Left as-is.

## Changes (pattern a)
For each owning module foo (single-file today): `foo.rs -> foo/mod.rs`, move
`tests_foo.rs -> foo/tests.rs`, add `#[cfg(test)] mod tests;` at the bottom of
`foo/mod.rs` (with the existing feature gate where applicable). For `wiring` (already
a dir): move `tests_wiring.rs -> wiring/tests.rs`, declare from `wiring/mod.rs`.
Remove the `mod tests_*` declarations from `lib.rs`. No `super::` usage in any test
file, and all use `crate::` paths, so moving them deeper only widens access — safe.

## Preserved exactly
- All test logic / function names / doc comments verbatim.
- Feature gates: abi+wiring tests stay `#[cfg(feature = "kdl")]` (now expressed on the
  `mod tests;` declaration inside the gated context).
- `tests/` integration tests (`dl_integration.rs`, `wiring_resolve.rs`) are public-API
  consumers — left in place.

## Verify
`cargo check --all-features` / `--no-default-features`, `cargo test --all-features`
(58 unit tests must remain), `cargo build --all-features`.
