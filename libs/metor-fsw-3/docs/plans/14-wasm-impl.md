# WASM packs: implementation plan

Implements `14-wasm.md`. Twelve tasks, in order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- from T2 on, `cargo build -p echo-pack --target wasm32-unknown-unknown`;
- one commit.

T1 also runs the ring's Kani and Miri suites (`ring/VERIFY.md`).

Order: T1 hardens the ring before anything puts a ring in guest memory.
T2 makes the crate build for wasm32. T3 to T5 reshape the dylib path
with no behaviour change. T6 to T8 add the wasm adapter. T9 to T11 wire
it through config, slots, `pack dev`, and Python. T12 is the gate.

`echo-pack` (`tests/fixtures/echo-pack`) is the test pack for both
kinds. It already has cyclic, async, panicking, param, and sequence
systems. T8 adds a `spin` system that never returns.

## T1. Ring: survive hostile control words

Files: `ring/src/lib.rs`, `ring/src/verify.rs`, `ring/src/tests.rs`,
`ring/DESIGN.md`, `ring/VERIFY.md`.

1. `WriteError::Corrupt`: "ring region violates a structural invariant".
2. `Writer` gains `committed: Cell<u64>`, read from the region once in
   `RingBuffer::writer` and the only source of `c` in `try_write` after
   that. `commit` and the wrap path store to the region and to the cell.
3. `Writer::fits` returns `Result<bool, WriteError>`:
   - `slowest > committed` is `Corrupt`. This replaces the
     `debug_assert!` and its PANIC Safety comment.
   - `committed - slowest + need` uses `checked_add`, and overflow is
     `Corrupt`.
   `try_write` propagates it.
4. `View` gains `cursor: Cell<u64>`, set at registration. `cursor()`
   reads the cell. `advance` stores to the cell and the slot.
   `ReadGrant` holds `&View`'s cell and slot, and stores both on drop.
   `locate` starts from the cell.
5. `Inner::locate_before`: `(r & !mask) + cap` is `checked_add`, and
   overflow is `Corrupt`. `View::data_end`: `hwm + cap - (hwm & mask)`
   is checked the same way. Out of range reads as "no trailing gap".
6. `DESIGN.md`: one paragraph. The writer and each view keep their own
   positions and only publish them to the region, so a peer that writes
   the control block can make a read or write fail with `Corrupt`, but
   cannot move either handle outside the region.

Tests:
- Kani `corrupt_peer_writer_never_ub`: a writer whose region has
  arbitrary `committed`, `hwm`, and reader cursors, stored after the
  claim, writes without panicking and never outside the data region.
- Kani `corrupt_peer_view_never_ub`: the same for a view's slot cursor,
  `committed`, and `hwm`, over `try_read`, `try_latest`, and `drain`.
- Unit, in `tests.rs`:
  - a reader slot set to `u64::MAX - 1` makes `try_write` return
    `Corrupt`;
  - a slot set ahead of `committed` is `Corrupt`;
  - overwriting a view's slot cursor does not move `cursor()`, and the
    view reads on from where it was;
  - overwriting `committed` with `u64::MAX` gives `Corrupt` or `None`
    from every read path, never a panic.
- The existing Loom, Kani, and Miri suites pass unchanged.

## T2. Build for wasm32

Files: `Cargo.toml`, `ring/Cargo.toml`, `ring/src/lib.rs`,
`src/lib.rs`, `src/coordinator/mod.rs`, `src/async_system.rs`,
`src/port/readiness.rs`, `src/channel/mod.rs`,
`src/thread/{mod,os,unsupported}.rs` (from `mod.rs`, `group.rs`, and
`threads.rs`), `src/coordinator/params.rs` (`ThreadsUnsupported`),
`tests/fixtures/echo-pack/Cargo.toml`,
`examples/adcs-fsw3/systems/adcs/Cargo.toml`.

1. **`maitake-sync`.**
   - `ring/Cargo.toml`: `notify = ["dep:maitake-sync"]` with
     `maitake-sync = { path = "../../stellarator/maitake/sync" }`, the
     crate `stellarator::sync` re-exports, so the types are the same.
   - `Notifier` and `WakeSink::wait_owned` name
     `maitake_sync::wait_queue::{WaitQueue, WaitOwned}`.
   - The crate depends on the same path. `async_system.rs` and
     `port/readiness.rs` import from it.
   - `link/conn.rs` keeps `stellarator::sync`; it is host-only.
2. **Features**, as the design lists: `default = ["host", "db"]`,
   `host = ["threads", ..]`, `threads = ["dep:stellarator"]`,
   `db = ["host", "dep:metor-db"]`.
   - `stellarator`, `libloading`, `clap`, `minijinja`, `toml`, `blake3`,
     `libc`, `metor-proto-stellar`, and `wasmi` (added in T6) become
     optional.
   - `stellarator` moves under
     `[target.'cfg(not(target_family = "wasm"))'.dependencies]`.
   - `[[bin]] metor` gets `required-features = ["host"]`.
3. **Gates in `lib.rs` and `coordinator/mod.rs`:**
   - `#[cfg(feature = "host")]` on `cli`, `deploy`, `dl`, `link`,
     `channel::adapter`, `coordinator::build`, and `coordinator::run`,
     and on their re-exports;
   - `thread`'s two halves, in step 4.

   Build `echo-pack` for wasm32 and follow the compiler. Where a gated
   item is named from ungated code, move the use behind the same gate.
   No gate goes inside a function body. If one seems to be needed, the
   code belongs in a gated module.
4. **Two halves of `thread`.** `thread/mod.rs` keeps
   `DEFAULT_THREAD` and declares one of:
   - `#[cfg(feature = "threads")] mod os;`: today's `Threads`,
     `place`, `Thread`, `group`, and `threads.rs` contents, moved
     unchanged;
   - `#[cfg(not(feature = "threads"))] mod unsupported;`: `pub struct
     Threads;` with `new()`, and `place`, which returns the new
     `ParamError::ThreadsUnsupported { system }`.

   Either one is re-exported as `thread::{Threads, place}`, so
   `MakeCx`, `table.rs`, `build.rs`, `channel/adapter`, and
   `pack/mod.rs` do not change.
5. **Ring features.** The crate depends on the ring with
   `features = ["notify"]` always. `threads` is `["dep:stellarator"]`
   alone.
6. **Pack crates.** `echo-pack` and the ADCS pack take
   `default-features = false, features = ["threads"]`.

Tests:
- `thread/unsupported.rs`: `place` refuses with `ThreadsUnsupported`,
  naming the system. It runs under `cargo test -p metor-fsw-3
  --no-default-features --lib thread`.
- The existing thread, channel, and pack tests pass untouched.
- `cargo build -p echo-pack --target wasm32-unknown-unknown` succeeds.
  `cargo tree -p echo-pack --target wasm32-unknown-unknown` lists no
  `stellarator`, `libloading`, or `metor-db`.

## T3. ABI additions and system kind

Files: `src/pack/{raw,mod,def,tests}.rs`, `src/coordinator/table.rs`.

1. `RawRing::owner: Option<RawOwner>`. `RawRing::of` wraps it in
   `Some`. `pack::attach` calls `attach_owned` on `Some` and `attach_raw`
   on `None`.
2. `const` layout checks in `raw.rs`:
   ```rust
   const _: () = assert!(size_of::<Option<RawOwner>>() == size_of::<RawOwner>());
   #[cfg(target_arch = "wasm32")]
   const _: () = assert!(size_of::<RawSlice>() == 8 && size_of::<RawRing>() == 20);
   ```
   `pub const WASM32_RAW_SLICE: usize = 8` and
   `WASM32_RAW_RING: usize = 20` sit beside them for the host encoder
   (T7).
3. `pack::alloc(len, align) -> *mut u8` and `pack::free(ptr, len,
   align)`. They go through `std::alloc`, return null on a zero length
   or a bad layout, and never panic. `export_pack!` emits
   `metor_fsw_alloc` and `metor_fsw_free`.
4. `pub enum SystemKind { Cyclic, Sequence, Async }`, serde snake case,
   `Default = Cyclic`.
   - `TableEntry` gains `kind`, set by each `register*` fn.
   - `PackSystemDef` gains `#[serde(default)] kind`, projected in
     `from_table`.
   - Built-in entries set theirs.
5. `Status::try_from_raw(word) -> Option<Status>`. `from_raw` becomes
   `try_from_raw(word).unwrap_or(Status::Panicked)`.
6. A wasm32 panic hook, in `with_table`'s init under
   `#[cfg(target_family = "wasm")]`: it logs the panic info as an
   `error!` with `kind = "panic"` through `tracing`.

Tests:
- `pack/tests.rs`:
  - `create` with a `None` owner attaches and binds, and dropping the
    instance leaves the region untouched;
  - `alloc`/`free` round-trip, and a zero length is null;
  - the descriptor carries `kind` for each registration fn, and a
    descriptor without `kind` decodes as `Cyclic`;
  - `try_from_raw(7)` is `None`.
- `ABI_VERSION` stays 9; `tests/dl.rs` passes untouched.

## T4. `enum Pack` and one descriptor codec

Files: `src/dl/mod.rs` (split into `src/dl/{mod,pack,codec}.rs`),
`src/dl/slot.rs`, `src/deploy/slot.rs`, `src/cli/{run,pack_dev}.rs`,
`tests/{dl,slot}.rs`.

1. `dl/codec.rs` holds what both kinds share:
   - `DESCRIPTOR_CAPACITY`;
   - `decode_descriptor(status, bytes)`, `decode_def(status, bytes)`,
     and `decode_create_error(bytes)`, all taking bytes already read;
   - `encode_def_cx(cx)` and `encode_instance(..)`.
   `call_export` stays in the dylib path.
2. Today's `Pack` becomes `DlPack` in `dl/pack.rs`, unchanged apart from
   using the codec.
3. `dl/mod.rs`: `pub enum Pack { Dl(DlPack) }`, with `descriptor`, `def`,
   and `create -> Result<Box<dyn Step>, ParamError>` matching on the
   variant.
4. `register_pack(&mut self, id, pack: Arc<Pack>)`. Each entry's closures
   hold the `Arc<Pack>`. That replaces the separate `Arc<Library>` and
   `PackFns` copies; the `DlPack` inside holds the library.
5. `DlSlot`: `Loaded { pack: Arc<Pack>, step: Box<dyn Step> }`. `open`
   returns `Pack`.
6. Call sites wrap `Pack::Dl(DlPack::open(..)?)`: `cli/run.rs`
   `table_for`, `cli/pack_dev.rs`, and `deploy/slot.rs`.

Tests: `tests/dl.rs` and `tests/slot.rs` pass with only their `open`
calls changed. `dl/codec.rs` takes the descriptor unit tests that were
in `dl/mod.rs`.

## T5. `Mirrors`

Files: `src/mirror.rs` (new, `#[cfg(feature = "threads")]`),
`src/thread/{mod,tests}.rs`.

1. `Mirror<W>` moves to `mirror.rs`.
   `drain(&mut self) -> Result<(), ReadError>` stops at the first
   corrupt record, settles, and returns the error.
2. `Mirrors<W: WakeSource>` holds `inputs: Vec<Mirror<W>>`,
   `outputs: Vec<Mirror<NoWake>>`, `log`, `line`, `fields`, and
   `since_report`. Methods:
   - `drain_inputs` and `drain_outputs`, each `Result<(), ReadError>`
     at the first error;
   - `report_drops(now)`, `write_line(now, level, kind, message)`, and
     `release_inputs()`, which clears `inputs` and drops their views;
   - `mirror_config(capacity, factor)` and `log_port(def)` move here.
3. `Thread` becomes `{ mirrors: Mirrors<Notifier>, panicked, end, group }`.
   It ignores drain errors, as today. Its bind helpers build `Mirrors`
   with factor `MIRROR_FACTOR` (4).
4. `take_drops`' two loops become one over
   `inputs.iter_mut().chain(outputs)` (via a small `counts` iterator).

Tests: the `thread/tests.rs` suite passes unchanged. New, in `mirror.rs`:
`drain` returns `Corrupt` from a ring whose record header was
overwritten, and still copies the records before it. `release_inputs`
frees the real ring's reader slot, which `RingBuffer::view` then reclaims.

## T6. `Guest`

Files: `Cargo.toml` (`wasmi = { version = "1.1", default-features = false,
features = ["std"], optional = true }` under `host`; dev-dep `wat`),
`src/wasm/{mod,guest}.rs`, `src/wasm/tests.rs`.

1. `Module(wasmi::Module)` and `Module::compile(engine, bytes)`.
   `pub fn engine() -> wasmi::Engine` builds one per `WasmPack` with:
   - `compilation_mode(CompilationMode::Eager)`;
   - `consume_fuel(true)`;
   - `set_min_stack_height(STACK)`, with `STACK` a named const sized
     from the echo-pack's deepest call;
   - `wasm_multi_value`, bulk memory, sign-extension, and
     reference-types left at their defaults.
2. `Arena { ptr: NonNull<u8>, len: usize }`: `alloc_zeroed` with 64 KiB
   alignment, freed in `Drop`. `len` is the ceiling rounded up to whole
   pages.
3. `Guest::new(module, memory)`:
   - A `Store<StoreLimits>` with limits on memories (1), tables (1),
     table elements (`TABLE_ELEMENTS`), and instances (1).
   - `Memory::new_static` over a `&'static mut [u8]` made from the arena.
     The `unsafe` block's SAFETY comment names the contract: the store
     is dropped before the arena.
   - `Linker::define("env", "memory", memory)`. Instantiate, and resolve
     each `Exports` field with `get_typed_func`. A missing export is
     `WasmError::MissingExport(name)`.
   - The module's declared minimum above the ceiling is
     `WasmError::MemoryTooSmall`, and the page count above it is refused.
4. `Guest::store` is a `ManuallyDrop<Store<StoreLimits>>`. `Drop`
   drops it, and `arena` drops afterwards as a field, so the order
   holds whatever the field order.
5. Calls: `set_fuel(fuel)`, then the typed call. The error maps through
   `as_trap_code()` into `Trap`. An error with no trap code (a host
   error, which cannot happen with no imports) maps to `Trap::Other`.
6. `read(offset, dst)`, `write(offset, src)`, and `region(offset, len)`:
   bounds-checked against `memory.data_size`, with pointers from
   `memory.data_ptr`. `region` returns `NonNull<u8>` aligned to 16, or
   `WasmError::Misaligned`.

Tests, with `wat` modules in `wasm/tests.rs`:
- `(loop br 0)` in an export is `OutOfFuel` under a grant of 10 000;
- `i32.store` past the ceiling is `MemoryOutOfBounds`, and a canary in
  the bytes just past the arena, in the same allocation, is untouched;
- a module declaring 17 pages under a 16-page ceiling is refused;
- `memory.grow` past the ceiling returns -1, and `data_ptr` is
  unchanged across a successful grow within it;
- `table.grow` past `TABLE_ELEMENTS` returns -1;
- a module missing `metor_fsw_alloc` is `MissingExport`;
- `read`, `write`, and `region` refuse out-of-range offsets.

## T7. `WasmPack::open` and `def`

Files: `src/wasm/{mod,abi,pack}.rs`, `src/dl/mod.rs`, `src/wasm/tests.rs`.

1. `wasm/abi.rs`: `Args<'g>` borrows a `Guest` and writes guest-side
   values with `alloc`:
   - `bytes(&[u8]) -> u32`;
   - `slice(ptr, len) -> u32`, an 8-byte `RawSlice`;
   - `rings(&[GuestRing]) -> (ptr, len)`, 20 bytes each, with the owner
     zeroed;
   - `ports(&[(ptr, len)]) -> u32`.

   `Args` records every allocation and frees them all in `Drop`. Sizes
   come from T3's `WASM32_*` constants.
2. `WasmPack { engine, module, memory, fuel: Fuel, def: PackDef }`, where
   `Fuel { step: u64, setup: u64 }`.

   `open(bytes, memory, fuel)`:
   - compile;
   - make a scratch guest, check `abi_version == ABI_VERSION`;
   - `alloc` a `DESCRIPTOR_CAPACITY` buffer and a `usize` slot, call
     `pack_def`, read back `written` bytes, and decode with the codec;
   - drop the guest.

   Errors reuse `PackError`, which gains `Wasm(WasmError)` and
   `Trap(Trap)`.
3. `def(ty, cx)`: a scratch guest, then `Args` for `ty` and the encoded
   cx, then call `def`, read, and decode as the dylib path does.
4. `WasmPack` is not yet a `Pack` variant. T8 adds `Pack::Wasm` along
   with `create`.

Tests, over `echo-pack` built for wasm32 (a `common` helper builds it
once with `cargo rustc --target wasm32-unknown-unknown --release --
-C link-arg=--import-memory`):
- `open` reads a descriptor equal to the dylib's, `kind` included;
- `def` for `gain` under params matches the dylib's `def`;
- a module built with a wrong ABI constant is `AbiMismatch` (a `wat`
  module exporting `metor_fsw_abi_version` returning 1);
- a module missing an export is `MissingSymbol`, naming it.

## T8. `WasmPack::create` and `WasmStep`

Files: `src/wasm/{pack,step}.rs`, `src/wasm/tests.rs`,
`tests/fixtures/echo-pack/src/lib.rs`, `tests/wasm.rs` (new).

1. `Pack::Wasm(WasmPack)`, with `descriptor`, `def`, and `create`
   wired. `create(ty, params, instance, inputs, outputs)` follows the
   design's six steps:
   - A guest ring of capacity `2 * real` has region length
     `checked_region_len(&cfg)` and is allocated with `align = 16`.
   - The host side is
     `unsafe { RingBuffer::create_raw(guest.region(..)?, len, cfg) }`.
     Its SAFETY comment names the arena's lifetime and the field order
     in `WasmStep`.
   - The host takes the writer (inputs) or a view (outputs) before
     `create`.
   - A null instance reads the error `RawSlice` back through `Args`
     and decodes it with the codec.
2. `WasmStep { mirrors: Mirrors<NoWake>, guest: Guest, instance: i32,
   fuel: u64, end: Option<EndReason> }`, with `mirrors` first and a
   one-line comment saying why.
3. `execute`, as the design sketches it, with
   `fn end_of(fed, result) -> Option<EndReason>` as a free fn over the
   table:
   - `Ok(word)` goes through `Status::try_from_raw`. `None` is
     `Panicked` and writes a `bad_status` line.
   - `Err(OutOfFuel)` is `Failed("out of fuel")` and writes a `fuel`
     line.
   - Any other `Err(trap)` is `Panicked` and writes a `trap` line with
     `trap.as_str()`.
   - A `ReadError` from either drain is `Failed("corrupt ring")` and
     writes a `corrupt` line.

   Lines go through `Mirrors::write_line`. On an end,
   `release_inputs()`.
4. `fault` does nothing. `ended` returns `end`. There is no `Drop` impl.
5. `echo-pack` gains `spin`: a `#[system]` that loops until
   `now < Timestamp(0)`, so the optimizer keeps it.

Tests:
- `wasm/tests.rs`, with `wat` guests whose `create` returns a handle and
  whose `execute` returns a word from a global the test sets:
  - each ends row: `Ok`, `Completed`, `Aborted`, `Failed`, word 9,
    `unreachable`, a fuel loop, and a guest that overwrites its output
    ring's record header with `u32::MAX`, which gives `corrupt`;
  - outputs are drained after a trap: the guest writes one record, then
    traps;
  - after an end, the real input ring's reader slot is free;
  - Miri (`cargo +nightly miri test -p metor-fsw-3 wasm::tests::drop`)
    over a `WasmStep` dropped with live mirrors. The test skips the
    interpreter by building `Mirrors` over rings in a bare `Arena`, and
    runs only the drop order.
- `tests/wasm.rs`, over `echo-pack` as a module:
  - `static -> wasm echo -> static` delivers same-cycle data for 32
    cycles; with fan-in, it merges by timestamp;
  - `echo` as a dylib and as a module produce identical record streams;
  - `relay` (async) is refused at build with `ThreadsUnsupported`;
  - `boom` ends `Panicked`, and its `log` has the host `trap` line and
    the guest's panic line;
  - `spin` ends `Failed`, its producer keeps writing for 32 more cycles
    without `WouldBlock`, and the next system runs every cycle;
  - `count_up` in a module completes and reports `Completed`. Aborted
    through a channel, it reports `Aborted`. `Reset` makes it again.

## T9. Config and run

Files: `src/cli/config.rs`, `src/cli/run.rs`, `src/wasm/mod.rs`,
`tests/run.rs`.

1. `PackRef` gains:
   - `#[serde(default)] kind: PackKind` (`Dylib | Wasm`, snake case);
   - `memory: Option<usize>`;
   - `fuel: Option<u64>`.

   `artifact()` picks `<libs>/wasm32-unknown-unknown/<lib>.wasm` for
   `Wasm`. `DEFAULT_MEMORY` (16 MiB) and `DEFAULT_FUEL` sit in
   `wasm/mod.rs`, and `DEFAULT_SETUP_FUEL` is 100x `DEFAULT_FUEL`.
2. `PackRef::open(&self) -> Result<Pack, PackError>` holds the one
   `match` on kind. `Dylib` keeps its `unsafe` block and SAFETY comment.
   `Wasm` reads the file and calls `WasmPack::open`.
   `table_for` and `deploy/slot.rs` call it.

Tests: `config.rs` round-trips a `wasm` pack ref and defaults `kind` to
`dylib`. `tests/run.rs` runs `echo-pack` as a wasm pack for 3 cycles
through `load`.

## T10. Slots

Files: `src/dl/slot.rs`, `src/deploy/slot.rs`, `tests/slot.rs`.

1. `Candidate` and `Resolved` gain `memory: Option<usize>` and
   `fuel: Option<u64>`.
2. `DlSlot::open`:
   - a `.wasm` path reads the bytes, compares their blake3 with
     `candidate.hash`, and calls `WasmPack::open` on the same buffer;
   - other paths keep `is_loaded` and `still_at`.
3. `deploy/slot.rs::candidate`:
   - opens through `PackRef::open`;
   - stores the artifact with `store::put`, whose file name keeps the
     `.wasm`;
   - writes `memory` and `fuel` from the pack ref beside `path`, `hash`,
     and `def`.

Tests, in `tests/slot.rs`:
- load a wasm `count_up`, swap to a dylib `count_down`, and back;
- a wasm candidate whose stored bytes were changed is refused with
  `Hash`;
- a resolved wasm candidate's `memory` and `fuel` reach its `Guest`: a
  `spin` under a tiny fuel grant ends in one cycle.

## T11. `pack dev`, the module template, and Python

Files: `src/cli/{cargo,pack_dev,module}.rs`, `src/cli/module.py.jinja`,
`python/metor-config/metor_config/{_model,_target,_config}.py`,
`python/metor-config/tests/test_target.py`, `tests/pack_dev.rs`.

1. `PackConfig` gains `wasm: bool`, read from `[tool.metor.pack] wasm`.
2. `cargo::wasm_build(package, dir) -> Result<PathBuf, CargoError>` runs
   `cargo rustc -p <pkg> --target wasm32-unknown-unknown --release
   --message-format=json -- -C link-arg=--import-memory` and finds the
   `.wasm` artifact the way `cdylib` finds the dylib. `cargo rustc`
   passes the flag to the pack crate alone, so dependencies stay cached.
3. `pack_dev`, when `wasm`:
   - build, and stage with `copy_atomic` at
     `_libs/wasm32-unknown-unknown/<lib>.wasm`;
   - open it with `WasmPack::open`, and check its descriptor serializes
     equal to the dylib's (`PackDevError::DescriptorMismatch`);
   - refuse any `SystemKind::Async` system
     (`PackDevError::AsyncInWasm { system }`).
4. The template renders `PACK = Pack(.., wasm=True)` when built with
   wasm. `Pack` gains `wasm: bool = False`.
5. `Target.wasm(pack, *, fuel=None, memory=None)`:
   - raises `ValueError` if `pack.wasm` is false;
   - records `{"kind": "wasm", "fuel": .., "memory": ..}` in
     `self._wasm[pack.id]`, dropping `None`s.

   `to_config` merges it into that pack's entry.

Tests:
- `tests/pack_dev.rs`: `echo-pack` with `wasm = true` refuses on
  `relay`. A copy of the fixture without `relay`, in a temp dir, stages
  the `.wasm` and a module with `wasm=True`.
- `test_target.py`:
  - `Target.wasm` writes `kind`, `fuel`, and `memory`;
  - a pack without `wasm` raises;
  - a second `wasm` call replaces the first;
  - `to_config` for a target without `wasm` is unchanged.
- `tests/golden/echo_pack.py` is re-blessed for the new `wasm` field.

## T12. The ADCS gate

Superseded by `16-adcs-sequences-pack.md`.

Files: `examples/adcs-fsw3/systems/adcs/pyproject.toml`,
`examples/adcs-fsw3/tests/convergence.rs`.

1. The ADCS pack sets `wasm = true`. It has no async systems.
2. `convergence.rs` gains a third run: the staged `.wasm` opened through
   `PackRef::open` with `kind = Wasm`.
3. Assertions:
   - the pointing error falls below the existing bound within the
     existing cycle count;
   - `nav.log` carries the first-fix line;
   - each `ctrl.motor_cmd` component is within `1e-9` of the native run
     at every cycle.
4. Record in the design doc's shipped deviations anything that moved.

## Risks to check early

- T2: `tracing-subscriber`'s registry on wasm32-unknown-unknown. If it
  needs a clock or threads, the guest installs `LogLayer` over
  `tracing_core::Dispatch` directly.
- T2: `log_scope` stamps with `Timestamp::now()`, which reads a clock.
  Only async systems use it, and a guest refuses those at placement, so
  it is compiled but never called in a module. A test in T8 keeps it
  that way: the guest steps 100 cycles with logging.
- T6: `set_min_stack_height` sizing. If the echo-pack's deepest call
  needs more, a guest step can allocate the interpreter stack
  mid-cycle. The T8 `spin` and `count_up` tests run under a counting
  allocator that fails the test on any allocation after `create`.
- T7: `Args` places the descriptor buffer in guest memory, so a
  guest's ceiling must hold 1 MiB beyond its own needs during `open`.
  `DEFAULT_MEMORY` of 16 MiB covers the echo-pack and ADCS; a smaller
  ceiling fails `open` with `MemoryTooSmall`, not a trap.
