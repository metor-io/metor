# Resolved: `wiring-model` dead-code warnings → feature removal

## Problem

`cargo build -p metor-panel` compiled metor-fsw-2 with `default-features =
false, features = ["wiring-model"]` and emitted 74 dead-code warnings: the only
caller of the coordinator-construction machinery (`InitGraph`, `bind`,
`SlotRunner`, `ProcSlot`, `SeqWorker`, …) was `wiring::resolve`, behind the
`wiring` feature. Default builds were warning-free — nothing was dead, just
unreachable in the IR-only configuration.

## Decision

An honest IR-only boundary needed ~40 new `#[cfg(feature = "wiring")]` sites on
top of the ~20 existing ones, with awkward seams (the half-live `CyclicSlot`
trait, keepalive fields in `pack.rs`). The panel was the feature split's only
consumer, and workspace builds unified to `wiring` anyway. So instead of
enforcing the boundary, we deleted it: the `wiring` and `wiring-model` features
are gone and everything they gated is always compiled.

## What changed (2026-07-22)

- `Cargo.toml`: `[features]` removed; the former optional deps (`serde_json`,
  `miette`, `clap`, `toml`, `sha2`, `base64`, `crc32fast`, `postcard-dyn`,
  `serde_ignored`, `tempfile`, `tracing-indicatif`, `owo-colors`) and the
  `tracing-subscriber` fmt/env-filter/ansi features are unconditional; the
  `metor-fsw` bin lost `required-features`.
- All `cfg(feature = "wiring")` / `cfg(feature = "wiring-model")` /
  `cfg_attr(not(feature = "wiring"), allow(…))` sites removed from `src/` and
  `tests/`; composed cfgs keep only their `target_os` half; the
  `not(wiring)` fallback arm of the shared-entry create closure in `pack.rs` is
  deleted.
- metor-panel depends on the crate plainly (`metor-fsw-2 = { path = … }`).

Verified: `cargo build -p metor-panel` warning-free; `cargo test -p
metor-fsw-2` all green; clippy clean for both crates.
