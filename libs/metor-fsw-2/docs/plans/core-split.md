# Splitting `metor-fsw-2` into an author crate and a host crate

## Why

Two problems, one cause: the framework crate is both the surface a system
author writes against and the runtime that loads, wires, and flies the graph.

1. **A wasm guest cannot depend on it.** `cargo check -p metor-fsw-2 --target
   wasm32-unknown-unknown` fails, because the runtime drags `stellarator` in
   (→ `polling`, `socket2`, `errno`). The sequencing arc wants one `.wasm` per
   sequence, and a wasm sequence needs `Input`, `Output`, `Outcome`,
   `sequence::*` — nothing from the runtime.
2. **Compile time.** A pack author's graph is 244 crates today. Most of them —
   `clap`, `mdns-sd`, `libloading`, `miette`'s fancy renderer, the whole
   `stellarator` stack — exist to run a target, not to write a system.

## The boundary

`metor-fsw-2-core` (`libs/metor-fsw-2/core`) is what a pack, sequence, or
frame author touches. `metor-fsw-2` is the host: it links core, adds the
runtime, and re-exports core so a target crate still sees one surface.

| | crate | modules |
|---|---|---|
| author | `metor-fsw-2-core` | `frame writer dynamic text descriptor port message health logfwd clock handler sequence pack abi params_docs shared system binder` **+ `registry` + `slot` + `params`** |
| host | `metor-fsw-2` | `coordinator wiring telemetry proc dl alarm preset ir cli` **+ `async_system`** |

The three bolded additions to core are the resolution of the back-edges below.

### Ports do not change

`Input<F, RD>` wraps a ring `View<RD>`, `Output<F, WD>` wraps a `Writer<WD>`,
both generic over the wake strategy with `NoWake` as the default, and
`AnySource` already abstracts a host-bound `Binder` from an `.so`-bound
`abi::RawBinder`. A wasm guest is the `.so` shape. No second port
implementation, no backing trait.

## Back-edges from core into host, and how each resolves

| edge | resolution |
|---|---|
| `message.rs` → `registry::RegistryEntry` (test only) | `registry` moves to core |
| `binder.rs` → `registry::Registry` | `registry` moves to core |
| `system/mod.rs` → `coordinator::{CyclicSlot, SlotState}` | `coordinator/status.rs` moves to core as `slot.rs` |
| `pack.rs` → `coordinator::{CyclicSlot, SlotState}` | same |
| `pack.rs` → `wiring::{decode_value_params, encode_value_params, NoParams, LoadError, ParamSource}` | the params codec moves to core; the five params error kinds become `core::params::ParamErrorKind`, which the host's `LoadErrorKind` absorbs as one variant |

### `registry` → core

`registry.rs` (190 lines) already depends on nothing but `std`,
`metor-fsw-ring`, `metor-proto{,-wkt}`, `crate::binder`, and
`crate::descriptor`. It was filed as host only because the coordinator builds
it. `Registry::new` and the `RegistryEntry` literal become a public
`RegistryEntry::new` constructor so `ring` stays private and readers still go
through `view()`.

### `coordinator/status.rs` → `core/src/slot.rs`

It is the cyclic-slot *vocabulary* — `CyclicSlot`, `SlotState`, `StopReason`,
`StoppedSystem`, `WorkerRunState`, `WorkerStatus`, `NAME_CAP` — and it uses
only `std` + `metor-proto{,-wkt}`. `CyclicSlot` is the interface a
`DriverSlot` (core) and a `SlotRunner`/`DlSlot`/`ProcSlot` (host) both
implement, so it belongs on the shared side. `pub(crate) trait CyclicSlot` and
`pub(super) fn code` widen to `pub`.

`coordinator/slot.rs` — the runtime slot machinery, `SlotStatus`,
`AllowedOccupant`, `OccupantBacking` — stays host.

### The params codec → core (the edge that needed tracing)

This was the one flagged as possibly sticky. It is not, but it does need the
error type split.

`wiring/params.rs` (schema-guided `encode_value_params`) plus `NoParams` and
`decode_value_params` from `wiring/registry.rs` move to `core/src/params.rs`.
They reach `serde_json`, `serde_ignored`, `postcard-dyn`, `postcard-schema`
and `miette::SourceSpan` — all of which compile for `wasm32-unknown-unknown`
(verified against the 1.94.0 toolchain before committing to this).

What does *not* move is `LoadError`: `LoadErrorKind` names `WireError`
(coordinator) and `DlError` (dl), so it is irreducibly host. The five params
variants — `MissingParam`, `InvalidParam`, `UnknownParam`, `ValueParams`,
`DlParamEncode` — are constructed *only* inside the codec and matched only in
`wiring/tests.rs`, so they split off cleanly:

```rust
// core::params
pub struct Anchor { pub src: String, pub span: SourceSpan }
pub struct ParamError { pub kind: ParamErrorKind, pub anchor: Option<Anchor> }
pub enum ParamErrorKind { MissingParam{..} InvalidParam{..} UnknownParam{..}
                          ValueParams{..} DlParamEncode{..} }
impl ParamErrorKind { fn at(self, src, span) -> ParamError; fn whole(self, src) -> ParamError;
                      pub fn code(&self) -> &'static str; pub fn label(&self) -> String;
                      pub fn help(&self) -> Option<String>; }
```

The host's `LoadErrorKind` gains `#[error(transparent)] Params(ParamErrorKind)`
and its `code`/`label`/`help` delegate to it; `LoadError` gains
`From<ParamError>` and keeps holding core's `Anchor`, so the rendered
diagnostic (code string, label, span, snippet) is byte-identical to today's.
`MakeError::Params` carries `Box<ParamError>` instead of `Box<LoadError>`.

`ParamSource` (in `ir`) is only named in a doc link from `pack.rs`; the link
becomes prose.

### `AsyncSystem` / `AsyncContext` → host

Not in the brief's list, but it falls out of the wasm requirement:
`AsyncContext` holds a `stellarator::util::CancelToken`, and `stellarator` is
exactly the dependency that breaks wasm.

Moving it down is right rather than expedient. An `AsyncSystem` is a
free-running task the coordinator's executor owns; it is registered only
through the static registry (`wiring/registry.rs`), driven only by
`coordinator::AsyncSlot`, and **no `Pack` entry can construct one** — pack
entries are function systems or `Pack::task` futures. It cannot exist in a
`.so` or a wasm guest. So it moves
to `src/async_system.rs` with its own test, and `system/mod.rs` keeps
`System`, `CyclicSystem`, `BuildSystem`, `Out`, `HealthOutput`, `CyclicRunner`
and the bundle traits.

## `metor-fsw-ring`: two default-on features

The ring fails for wasm only through `stellarator` and `memmap2`, each used in
one place:

- `Notifier(Arc<stellarator::sync::WaitQueue>)` — referenced only from
  `src/coordinator/`, i.e. the host.
- `BackingOwner::Mmap` + `Backing::mmap` + `create_mmap`/`attach_mmap`.

`libc`/`wake.rs` is already `#[cfg(any(target_os = "linux", target_os =
"macos"))]`-gated, and `create_in_memory` — what a guest wants — is already
there.

```toml
[features]
default = ["mmap", "notify"]
mmap   = ["dep:memmap2"]
notify = ["dep:stellarator"]
```

Core takes `metor-fsw-ring = { default-features = false }`. The host takes the
default. Nothing else changes.

## The macro crate

Derive macros resolve `metor-fsw-2-core` first and fall back to the host crate,
which re-exports core. Free-running `AsyncSystem` implementations stay
hand-written in host-dependent crates; this keeps their runtime boundary
explicit and the pack macro surface small.

## Re-exports

The host does `pub use metor_fsw_2_core::*` (plus the `#[macro_export]`
`export_pack!`). This is not a compat shim: `Coordinator`, `Wiring`, and the
CLI take core's types throughout, so the host's own public API is stated in
them, and a target crate depending on the host reasonably expects the whole
framework. Pack, sequence, contract, and fixture crates switch their manifests
to `metor-fsw-2-core`.

## Order of work

1. Ring features; verify `cargo check -p metor-fsw-ring --target
   wasm32-unknown-unknown --no-default-features`.
2. Create `core/`, `git mv` the core modules, split `system/mod.rs`,
   `coordinator/status.rs`, `wiring/params.rs` + `wiring/registry.rs`.
3. Widen the `pub(crate)` items the host reaches (compiler-driven), fix doc
   links that now cross the boundary.
4. Host `lib.rs` re-export; move `handler/tests.rs` → host `src/tests.rs`
   (it drives a live coordinator) and the async-system test → `async_system.rs`.
5. Macro crate name probe.
6. Workspace members, example and fixture manifests.
7. Verify; update `docs/`.

## Verification gates

- `cargo build --workspace`, `cargo test -p metor-fsw-2`, `cargo test -p metor-fsw-2-core`
- `cd examples/adcs-fsw2 && cargo test --test sequences` — 4 tests, file unedited
- `cargo check -p metor-fsw-2-core --target wasm32-unknown-unknown`
- `cargo check -p metor-fsw-ring --target wasm32-unknown-unknown --no-default-features`
- `cargo clippy -p metor-fsw-2 -p metor-fsw-2-core --all-targets` — no new warnings
- `libs/metor-fsw-2/spikes/wasm-poll/build.sh` still builds and runs

The two `tests/ui/*.stderr` trybuild snapshots shift from `src/…` /
`metor_fsw_2::handler::…` to `core/src/…` / `metor_fsw_2_core::handler::…`;
regenerate with `TRYBUILD=overwrite`. The `pass/` case stays in the host,
because it exercises the async form.

`cargo check --workspace` fails on `metor-proto-kdl` before and after this
change: the lockfile pins `kdl` 6.7.1, which requires rustc 1.95, and
`rust-toolchain.toml` pins 1.94.0. Verify with `--exclude metor-proto-kdl`.

## Note on the spike

`spikes/wasm-poll/guest` hand-rolls a ~30-line sequence runtime because it
could not depend on the framework. Once core builds for wasm that
reimplementation is unnecessary — the guest could use the real `sequence`
module. Out of scope for this pass; recorded here so the next sequencing phase
picks it up.
