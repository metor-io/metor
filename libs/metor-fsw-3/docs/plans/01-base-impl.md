# Base library: implementation plan

Implements `01-base.md`. Seven tasks, in dependency order. Each task ends
with `cargo test -p <crate>` green, `cargo clippy --all-targets` clean, and
a commit. Later tasks build on the public surface the earlier ones fix, so
do not reorder.

## T1. Ring copy

Files: `ring/Cargo.toml`, `ring/src/{lib,wake,sync,tests,loom_tests,verify}.rs`,
`ring/{KANI,LOOM,MIRI}.md`.

1. Copy `libs/metor-fsw-2/ring` to `libs/metor-fsw-3/ring`.
2. Rename the package to `metor-fsw-3-ring`. Fix the `stellarator` path
   (`../../stellarator`). Leave every source file byte-identical.
3. Add `libs/metor-fsw-3/ring` to the workspace members.
4. Run, in order, and record the result in the commit message:

```sh
cargo test -p metor-fsw-3-ring
cargo +nightly miri test -p metor-fsw-3-ring --lib --target x86_64-apple-darwin
RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom cargo test -p metor-fsw-3-ring --lib
cargo kani -p metor-fsw-3-ring
```

Done when all four pass. `ring_loom` is already in the workspace
`check-cfg` list.

## T2. Crate skeleton and Frame

Files: `Cargo.toml`, `src/lib.rs`, `src/frame.rs`, `macros/Cargo.toml`,
`macros/src/{lib,frame}.rs`.

1. `metor-fsw-3` depends on `metor-fsw-3-ring`, `metor-fsw-3-macros`,
   `metor-component`, `metor-proto` (`std`), `zerocopy` (`derive`),
   `stellarator`, `thiserror`. Delete the placeholder `add` fn.
2. `lib.rs` re-exports what the derive-impl expansions name:
   `metor_proto`, `metor_proto_wkt`, `zerocopy`, and the four
   `metor_component` traits. Check the paths `derive-impl` emits against
   the `crate_name` argument before choosing the re-export names.
3. `macros` is a proc-macro crate with `darling`, `syn`, `quote`,
   `proc-macro2`, `convert_case`, `proc-macro-crate`, and
   `metor-component-derive-impl`. Add it to the workspace members.
4. Port `metor-fsw-2/macros/src/frame.rs`: attribute name `frame` only,
   crate probe `metor-fsw-3` only, drop `peer_layout`, `group`, and the
   `no_timestamp` opt-out. A missing timestamp field is an error.
5. `frame.rs`: the `Frame` trait from the design doc.

Tests (`src/frame.rs` bottom): derive a two-field frame, check `NAME`,
`ID`, `MAX_SIZE`, `timestamp()`, and that `as_bytes` round-trips through
`ref_from_bytes`. Missing timestamp: a `trybuild` compile-fail case under
`macros/tests/`.

## T3. Ports

Files: `src/port.rs`.

1. `capacity_for(max_size, depth) -> Option<usize>`: power-of-two capacity
   for `depth.max(2)` records, `None` on overflow or `max_size > u32::MAX`.
2. `Output<F>` over `Writer<NoWake>` with `write`.
3. `Input<F>` over `Vec<View<NoWake>>` with `latest` and `drain`.
   `latest` calls `try_latest` on every view and keeps the grant with the
   greatest `timestamp()`; the losing grants drop, which keeps their pins.
   `drain` runs `try_read` to exhaustion on each view in order.
4. `FrameGrant<'a, F>`: `ReadGrant` plus `Deref<Target = F>` via
   `ref_from_prefix`. A short record is `ReadError::Corrupt`, not a panic.

Tests: write then latest; latest twice with no new data returns the same
record; drain order on one producer; fan-in latest picks the greater
timestamp across two producers; zero producers drains nothing and latest
is `None`; a full ring returns `WouldBlock`; a truncated record is
`Corrupt`. Kani harness `capacity_fits_two_records` under `#[cfg(kani)]`.

## T4. Systems

Files: `src/system.rs`, `macros/src/system.rs`.

1. `PortDef`, `SystemDef`, `System`, `SystemInputs`, `SystemOutputs` as
   in the design doc. `SystemInputs::defs() -> Vec<PortDef>` and
   `SystemOutputs::defs()` sit beside `bind` so `System::def()` can be
   built from the two bundles.
2. `impl SystemInputs for ()` and `impl SystemOutputs for ()`.
3. Derives: a named struct whose fields are all `Input<F>` (or all
   `Output<F>`). `defs` pushes `PortDef { name: <field>, frame: F::ID,
   max_size: F::MAX_SIZE }` per field; `bind` pops one list (or writer) per
   field in the same order. No field attributes.
4. `SystemDef::new::<I: SystemInputs, O: SystemOutputs>(name)`, which
   collects the two bundles' `defs`. A system's `def()` is one call to it.

Tests: a hand-written bundle and a derived bundle produce the same `defs`;
`bind` with the wrong list length panics with a message naming the system
(PANIC Safety: only reachable from a coordinator bug, never from config).

## T5. Coordinator build

Files: `src/coordinator/{mod,config,table,build,error}.rs`.

1. `config.rs`: `CoordinatorConfig`, `SystemConfig`, `InputConfig`,
   `PortRef`, `Clock`, all `serde::{Serialize, Deserialize}`.
2. `table.rs`: `SystemTable::register`. The factory is stored erased as
   `Box<dyn Fn(Vec<Vec<View>>, Vec<Writer>) -> Box<dyn Step>>` plus the
   `SystemDef`, so build never sees `S`.
3. `error.rs`: `BuildError` with `DuplicateId`, `UnknownType`,
   `UnknownSystem`, `UnknownPort`, `FrameMismatch`, `RingTooLarge`, each
   naming the system and port involved.
4. `build.rs`: the five passes from the design doc as five fns of no more
   than a screen each, called in sequence by `Coordinator::build`.
   Reader count per output ring is `edges + reader_slack`. Every ring is
   `RingBuffer::create_in_memory`. The status ring for each system is
   allocated and its writer kept by the coordinator.
5. `Coordinator` holds `Vec<Entry { name, step: Box<dyn Step>, status:
   Output<SystemStatus> }>`, the ring handles (declared last), the clock,
   and the cycle count.

Tests: one test per `BuildError` variant; a valid two-system config builds
and reports the expected ring count.

## T6. Coordinator run

Files: `src/coordinator/{run,status}.rs`.

1. `status.rs`: the `SystemStatus` frame.
2. `step(now)`: `Instant` at cycle start; per entry, `Instant` before
   `execute`, then write `SystemStatus { timestamp: now, exec_time_ns,
   exec_offset_ns }`; increment `cycle`.
3. `run(stop)`: loop `{ now from clock; step; pace; if stop is ready,
   return }`. Poll `stop` with `futures_lite::future::poll_once` or a
   pinned `select`; do not spawn. Wall pacing is `stellarator::sleep(budget
   - elapsed)` or `yield_now` on overrun; simulated is `yield_now`.
4. Simulated `now` is `epoch + cycle * dt` in checked integer nanoseconds.

Tests: `step` on a three-system pipeline shows same-cycle data flow; a
consumer listed before its producer sees the previous cycle; fan-in from
two producers; status records have `exec_time_ns > 0` and increasing
`exec_offset_ns` down the list; `run` under `#[stellarator::test]` with a
stop future that resolves at cycle 5 runs exactly five cycles; simulated
clock advances by `dt` per cycle.

## T7. Example and docs

Files: `tests/pipeline.rs`, `src/lib.rs` crate docs.

1. `tests/pipeline.rs`: `imu -> nav -> control` with real frames, built
   from a `CoordinatorConfig` literal and a `SystemTable`, run for a fixed
   number of cycles, asserting the control output.
2. Crate docs: one page, the example above in a doctest, no prose beyond
   what the design doc says.
3. Move `01-base.md`'s content that describes shipped behaviour into module
   docs; the plan file stays as history.

## Review

After T7, an antagonistic review by a second agent against `01-base.md`
and the style guide, focused on: unsafe outside `ring`, any allocation on
the `step` path, panics reachable from config, and lifetime soundness of
`FrameGrant` across `latest`.
