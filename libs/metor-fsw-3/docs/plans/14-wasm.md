# WASM packs

A pack can be a WebAssembly module instead of a shared library. The
module is the same pack crate built for `wasm32-unknown-unknown`, it
exports the same entry points, and the host registers its systems into
the same `SystemTable` under the same keys. Only the boundary changes: a
library runs in the host's address space and is handed host pointers; a
module runs under an interpreter, sees only its own linear memory, and
is metered in fuel.

In scope:

- Feature flags that let a pack crate build for
  `wasm32-unknown-unknown` with no source change.
- Two ABI additions: guest `alloc`/`free` exports and unowned rings.
- `enum Pack { Dl, Wasm }` behind `register_pack` and `DlSlot`.
- The wasm adapter: `WasmPack`, `WasmStep`, and the copy across the
  boundary.
- `Guest`, the one file that touches wasmi, kept small so spacewasm can
  replace it.
- Ring hardening against a peer that writes the region's control words.
- `metor pack dev` building a module, the config's pack kind, and
  `Target.wasm`.

Out of scope: the spacewasm backend itself, async systems inside a
module, WASI, host imports, zero-copy rings, reusing a guest's memory
across a `Reset`, and the process adapter.

## Why this shape

Three constraints set the design:

1. **The layering is already right.** A sequence in a dylib pack is a
   `Step` the pack's table built. `DlStep` forwards `execute` and reads
   back a status word that already carries `Completed`, `Aborted`, and
   `Failed`. The channel adapter wraps the `DlStep` on the host
   (`coordinator/build.rs`). Resumption lives in the step inside the
   pack, and the adapter only moves calls across a boundary. A wasm
   adapter that forwards the same calls gets
   `Channel -> WASM -> Sequence` for nothing, and a cyclic system in a
   module never sees sequence machinery.
2. **A guest cannot follow a host pointer.** Every ring a guest touches
   lives in its linear memory. The host keeps the real rings and copies
   records across each cycle, as the thread adapter already does with
   its mirrors.
3. **The interpreter is replaceable.** One file touches the
   interpreter, and what it asks for is something both wasmi and
   spacewasm do today: host-placed linear memory, fuel per call, i32 and
   i64 arguments, one return value, no imports. The instructions inside
   a module are the one gap.

```
host                                              guest (one instance)
Coordinator
  Channel (optional)
    WasmStep::execute(now)
      inputs:  real View ──copy──▶ guest ring ◀──── Input<T> (View)
      call metor_fsw_execute(instance, now) ──────▶ catch_step(slot, now)
                                                      Sequence / FnSystem
      outputs: real Writer ◀──copy── guest ring ◀── Output<T> (Writer)
      status word ◀──────────────────────────────── Status
```

## Building for wasm32

Packs already depend on `metor-fsw-3` with `default-features = false`,
which drops the `db` built-in. Two more features finish the job:

```toml
[features]
default = ["host", "db"]
# Stepping systems in this process: the coordinator's build and run loop,
# the channel adapter, dl, wasm, links, deployments, and the CLI.
host = ["threads", "dep:libloading", "dep:wasmi", "dep:clap", ..]
# Running async systems on OS threads. A dylib pack turns this on.
threads = ["dep:stellarator"]
db = ["host", "dep:metor-db"]

[target.'cfg(not(target_family = "wasm"))'.dependencies]
stellarator = { path = "../stellarator", optional = true }
```

A pack crate names one dependency for both builds:

```toml
metor-fsw-3 = { path = "..", default-features = false, features = ["threads"] }
```

`stellarator` is a target-specific optional dependency, so `threads`
does nothing on wasm32: the dylib build gets threads and the module
build does not, from the same line. `[[bin]] metor` requires `host`.

The gates sit on modules in `lib.rs` (`coordinator::build`,
`coordinator::run`, `channel::adapter`, `dl`, `wasm`, `link`, `deploy`,
`cli`) and on the two halves of `thread` below, not scattered through
function bodies. The ring's `notify` feature no longer needs
`stellarator`, so it is always on.

A feature flag rather than a `metor-fsw-3-core` crate:

- A pack crate's source is the same for both targets. With a crate split,
  a pack with async systems would name the host crate for its dylib build
  and core for its module build, so its `use` paths would differ by
  target.
- Packs already build this crate without its default features, and
  `ring` already gates `mmap` and `notify` for wasm32 the same way.
- Nothing `pub(crate)` has to become public. A split would expose the
  items that 29 files share across what would become the crate boundary.
- A dylib pack needs `Threads` either way, so a split would not keep
  host code out of native packs.

A crate split would enforce the boundary at compile time. Here, the test
pack's wasm32 build does that job in CI.

### Async systems

`AsyncSystem`, `Stop`, and `register_async` compile without `threads`.
The only reason they reach `stellarator` today is `WaitQueue`, which
`stellarator::sync` re-exports from `maitake-sync`, a `no_std` crate.
The crate and the ring's `Notifier` depend on `maitake-sync` directly.

Placement is what needs threads. `thread/` keeps its public surface,
`Threads` and `place`, in both builds, and the feature picks which of
two modules supplies it:

- `thread/os.rs`, under `threads`: today's `Threads`, `place`,
  `Thread` step, and thread groups, unchanged.
- `thread/unsupported.rs`, without it: `Threads` is a unit struct, and
  `place` returns `ParamError::ThreadsUnsupported { system }`.

`MakeCx::threads` stays `&mut Threads`, and nothing that calls `place`
changes. There is no trait: one build never holds both.

`PackSystemDef` gains `kind: Cyclic | Sequence | Async`, defaulted on
decode. `metor pack dev` refuses a `wasm = true` pack with an `Async`
system, naming it, before anything runs.

## ABI

The export signatures do not change, and `ABI_VERSION` stays 9. A dylib
host sends exactly the bytes it sends today. A wasm host needs the two
new exports and refuses a module without them as `MissingSymbol`, which
is the check an ABI 9 module fails.

### Exports as they are

`metor_fsw_create` and `metor_fsw_def` take `RawSlice` by value. On
wasm32, rustc follows the tool-conventions C ABI: a by-value aggregate
of more than one field is passed as an `i32` pointer to a copy the
caller owns. The host writes each `RawSlice` into guest memory, in wasm32
layout, and passes its address. Every other argument is an `i32` or
`i64`, and every export returns one `i32`. A golden test on a built
module pins each export's wasm signature, so an ABI change in rustc
fails loudly.

Guests may use multi-value and the other post-1.0 features rustc emits
by default. wasmi runs them. We move to spacewasm once it runs them too.

### Guest allocator

```rust
unsafe extern "C" fn metor_fsw_alloc(len: usize, align: usize) -> *mut u8;
unsafe extern "C" fn metor_fsw_free(ptr: *mut u8, len: usize, align: usize);
```

The host places arguments and ring regions in guest memory through
these. `export_pack!` emits them on every target; the dylib host never
calls them.

### Unowned rings

`RawRing::owner` becomes `Option<RawOwner>`. `RawOwner` holds
non-nullable function pointers, so the option uses their null niche and
the layout is unchanged. On `None`, the pack attaches with the existing
`RingBuffer::attach_raw`, which takes no lease. A guest's rings live in
its own memory and outlive every handle in the guest. The ring crate's
`RawOwner` is untouched.

## `Pack`

`register_pack` and `DlSlot` are written against the dylib today. They
take an enum over the two kinds:

```rust
pub enum Pack {
    Dl(DlPack),
    Wasm(WasmPack),
}

impl Pack {
    pub fn descriptor(&self) -> &PackDef;
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError>;
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        inputs: &[Vec<&RingBuffer>],
        outputs: &[&RingBuffer],
    ) -> Result<Box<dyn Step>, ParamError>;
}

impl SystemTable {
    pub fn register_pack(&mut self, id: &str, pack: Arc<Pack>) -> Result<(), BuildError>;
}
```

Today's `Pack` becomes `DlPack`. JSON encoding and decoding of the
descriptor, `def`, params, and errors move into one module that both
kinds call.

## The guest

`wasm/guest.rs` is the only file that names wasmi. It wraps one
instantiated module in a concrete struct whose methods speak our types:

```rust
/// One instantiated module: its store, its memory, and the ABI's exports.
pub struct Guest {
    store: wasmi::Store<wasmi::StoreLimits>,
    memory: wasmi::Memory,
    exports: Exports,
    /// Backs the linear memory. Freed in `Drop`, after `store`.
    arena: Arena,
}

/// The exports the ABI fixes, resolved to typed functions once.
struct Exports {
    create: wasmi::TypedFunc<(i32, i32, i32, i32, i32, i32), i32>,
    execute: wasmi::TypedFunc<(i32, i64), i32>,
    alloc: wasmi::TypedFunc<(i32, i32), i32>,
    ..
}

/// Why a call stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    OutOfFuel,
    /// `unreachable`, which is what a guest panic compiles to.
    Unreachable,
    MemoryOutOfBounds,
    StackOverflow,
    /// Any other trap code: a bad indirect call, integer overflow, and so on.
    Other,
}

impl Trap {
    /// The kind a fault line names.
    pub fn as_str(self) -> &'static str;
}

impl Guest {
    pub fn new(module: &Module, memory: usize) -> Result<Guest, WasmError>;
    pub fn execute(&mut self, instance: i32, now: i64, fuel: u64) -> Result<u32, Trap>;
    pub fn create(&mut self, args: CreateArgs, fuel: u64) -> Result<i32, Trap>;
    pub fn alloc(&mut self, len: u32, align: u32, fuel: u64) -> Result<u32, Trap>;
    pub fn read(&self, offset: u32, dst: &mut [u8]) -> Result<(), WasmError>;
    pub fn write(&mut self, offset: u32, src: &[u8]) -> Result<(), WasmError>;
    /// The host address of `offset`, for formatting a ring there.
    pub fn region(&self, offset: u32, len: u32) -> Result<*mut u8, WasmError>;
}
```

`Module` is a newtype over `wasmi::Module` in the same file. The rest of
the adapter calls `Guest`'s methods and never sees a wasmi type, so the
move to spacewasm rewrites this one file.

`WasmPack` holds one compiled `Module`. Each system instance gets its
own `Guest`: its own memory, its own `SystemTable` inside the guest, and
its own fuel. A trap in one instance never touches another, and a memory
ceiling is per system rather than per pack.

### Engine config

One `wasmi::Engine` per process:

- `compilation_mode(Eager)`. The default translates each function on
  its first call, which allocates inside a cycle.
- `consume_fuel(true)`.
- `set_min_stack_height` sized so the interpreter's value stack never
  grows during a step.

Each store gets `StoreLimits` capping tables, table elements, instances,
and memories at what a pack module needs (fsw-2 did this in `wasm.rs`).
`new_static` bounds only memory.

### Memory

The module imports its memory as `env.memory`. `Arena` allocates the
ceiling once, at instantiation, as a raw pointer. The store gets a
`&'static mut [u8]` made from it; `Guest`'s `Drop` drops the store
first, then frees the arena.

- wasmi: `Memory::new_static(store, ty, buf)`. Growth within `buf` needs
  no allocation, and growth past it fails.
- spacewasm: a `WasmMemoryAllocator` that returns the same buffer.

Once `new_static` owns the buffer, every host pointer into it is derived
from `Memory::data_ptr`, never from the arena. `Guest` hands out bytes
through `read` and `write` by offset, never as a `&mut [u8]` over the
whole memory, so no unique borrow overlaps the ring handles. `region`
gives `RingBuffer::create_raw` its pointer and length; the region is
interior-mutable as `create_raw` requires, since the guest and the host
both write it.

The base never moves, so host ring handles into guest memory stay valid
for the guest's life. A module whose declared minimum exceeds the
ceiling is refused at instantiation. Packs link with
`-C link-arg=--import-memory`, which `metor pack dev` passes.

### Fuel

`execute` runs under the instance's `fuel` grant. Instantiation,
descriptor, `def`, and `create` run under a separate setup grant, so
tightening `fuel` never breaks loading.

Running out is a trap, not a pause. Both runtimes could resume an
exhausted call next cycle, but that splits one cycle's step across two:
half its outputs go out a cycle early and its inputs are sampled twice.
Fuel bounds a step. Pausing is the sequence's job, one layer down.

### Why wasmi first

It is in the tree already (fsw-2, metor-expr, panel), it runs everything
`wasm32-unknown-unknown` emits, and it has two audits behind it.
`Guest` uses only features spacewasm also has, apart from the
instruction set (see [spacewasm](#spacewasm)).

## The wasm adapter

### Opening

```rust
impl WasmPack {
    pub fn open(bytes: &[u8], memory: usize, fuel: Fuel) -> Result<WasmPack, PackError>;
}
```

`open` compiles, instantiates a scratch guest, checks
`metor_fsw_abi_version` and the two allocator exports, reads the
descriptor into a 1 MiB buffer in guest memory, and drops the guest.
The buffer and the decode are the ones the dylib path uses. `def`
instantiates its own scratch guest per call. It runs only at build and
at a slot's `Load`, when allocating is allowed, so `WasmPack` keeps no
loader and no trap state between calls. `open` is safe to call, unlike
`DlPack::open`: a module cannot run code in the host.

### Creating

`WasmPack::create`, for one system instance:

1. Instantiate a guest.
2. For each input edge, allocate a guest ring with twice the real
   ring's capacity and `max_readers: 1`, format it from the host with
   `RingBuffer::create_raw` over `Guest::region`, and keep its writer.
   Keep a `View` on the real ring.
3. For each output, allocate and format a guest ring the same way and
   keep a `View` on it. Keep the real ring's writer.
4. Write the type, params, instance JSON, and the `RawPort` and
   `RawRing` arrays into guest memory, in wasm32 layout, with no owner.
5. Call `metor_fsw_create`. A null return reads the error slice back as
   the `ParamError`, as the dylib path does.
6. Free the argument buffers. The rings stay.

The host claims the writer of each input ring and the reader slot of
each output ring before the guest attaches. The guest then claims the
other end, and binds as it would in a dylib. Fan-in keeps one guest ring
per edge, so the guest merges by timestamp exactly as a native system
does.

Twice the capacity covers the wrap point: a guest ring's wrap falls at a
different place from the real ring's, so a full real ring's records can
need more than one capacity of room.

A channel's `Reset` and a slot's `Load` call `create` again, so they
instantiate a fresh guest and allocate its memory mid-run. A dylib's
`create` allocates at the same moments. Both are commanded events.

### Stepping

`Mirror` moves out of `thread/` along with everything `Thread` keeps
around it: the log port index, the encode buffer, the drop fields, the
drop report, and `emit`. The bundle is `Mirrors`, and both adapters hold
one.

```rust
pub struct WasmStep {
    /// Declared before `guest`: dropping them writes into guest memory.
    mirrors: Mirrors,
    guest: Guest,
    instance: i32,
    fuel: u64,
    end: Option<EndReason>,
}

impl Step for WasmStep {
    fn execute(&mut self, now: Timestamp) {
        let fed = self.mirrors.drain_inputs();
        let result = self.guest.execute(self.instance, now.0, self.fuel);
        let taken = self.mirrors.drain_outputs();
        self.end = end_of(fed.and(taken), result);
        if let Some(end) = &self.end {
            self.mirrors.report_end(now, end);
            self.mirrors.release_inputs();
        }
        self.mirrors.report_drops(now);
    }
}
```

`Mirror::drain` returns `Result<(), ReadError>` instead of stopping
silently at a bad record, so a corrupt guest ring ends the step.

When the step ends, `release_inputs` drops the input mirrors. The
coordinator never calls an ended step again, so a mirror still holding a
reader slot on a real ring would back its producer up; dropping the
view frees the slot. Outputs are drained once after a trap, because
every record a guest committed before it is whole.

`drain` allocates nothing and neither does the call, so a step is
allocation-free once built.

Outputs never block a guest. A native system's `write` sees
`WouldBlock` when its readers are behind. A guest writes into its own
ring, and the record is dropped at the host copy, counted, and reported
in the drop line. That is the thread adapter's behaviour too.

### Ends

| Result | `EndReason` | Line on the system's `log` |
| --- | --- | --- |
| status `Ok` | none | none |
| status `Completed`, `Aborted`, `Failed` | as the word says | written by the guest |
| status out of range | `Panicked` | host: `bad_status` |
| `Trap::OutOfFuel` | `Failed("out of fuel")` | host: `fuel` |
| any other `Trap` | `Panicked` | host: `trap`, with `Trap::as_str` |
| `ReadError::Corrupt` from a guest ring | `Failed("corrupt ring")` | host: `corrupt` |

The host's line is the guaranteed report of how a guest ended.

Dropping `WasmStep` never calls `metor_fsw_destroy`. Everything the
guest's step owns lives in guest memory, which the drop frees. Nothing
runs guest code in `Drop`, so there is no fuel or trap path there.

### Panics in a guest

`wasm32-unknown-unknown` panics abort, and an abort is `unreachable`.
There is no unwinding, so `catch_step` inside a guest catches nothing and
a panic kills the instance. The guest installs a panic hook when it
builds its table. The hook writes the message through the log layer,
which reaches a ring only while a system's log port is installed: during
a step, and not inside the log layer's own formatting. Elsewhere the
message is lost, and the host's `trap` line is the only report.

One sequence semantic differs. On native, a panic in the body runs the
abort handlers, each under `catch_unwind`. In a module the instance is
dead at the panic, and no handler can run. A sequence that must safe
hardware on a panic should run native until the exception-handling
proposal is available in both runtimes.

## Ring hardening

Copying keeps a guest's damage inside its own memory only if the host's
ring handles survive whatever the guest writes. The host holds two
handles into guest memory:

- the writer of each input ring. `try_write` reloads `committed` from
  the region and reads the guest's reader cursor for backpressure.
- the view of each output ring. Its own cursor lives in a reader slot
  the guest can write, and `committed` is the guest's.

The Kani proofs in `ring/src/verify.rs` assume sane control words
(`committed <= MAX - 2 * cap`, `slowest <= committed`). A guest can
break both, and `committed + gap` then overflows.

The ring changes:

- A `Writer` keeps its committed position locally and only stores it to
  the region. It is the sole writer, so the region copy is only for
  readers.
- A `View` keeps its cursor locally and only stores it to its slot.
- Every sum and difference of control words is checked. Out of range is
  `WriteError::Corrupt` or `ReadError::Corrupt`, never a panic or an
  out-of-bounds access.
- New proofs: writer and view stay in bounds and never panic for
  arbitrary values in every control word.

This touches `ring/`, which is edited alongside this work, so it lands
first as its own change.

## Slots

`DlSlot` keeps its name and config type. A candidate whose `path` ends
in `.wasm` loads as a `WasmPack`; anything else loads as a `DlPack`.
`check_ports` and `create` go through `Pack`, and a failed load keeps
the old occupant, all unchanged. `Occupant::artifact` and `hash` report
a module the same way they report a library.

A wasm load reads the file once, hashes those bytes, and compiles the
same buffer. There is no resident library to reuse and no file to
re-check, so `is_loaded` and `still_at` stay on the dylib path.

`Candidate` gains `memory` and `fuel`. `deploy/slot.rs` carries them
through when it swaps a candidate's `pack` for `path`, `hash`, and
`def`, taking the pack's defaults when the candidate sets none.

Wasm candidates are what make uplinked sequences practical. A module is
one file for every host triple, it cannot corrupt the host, and a
runaway one costs one fuel grant.

## Build and config

### `metor pack dev`

`[tool.metor.pack]` gains `wasm = true`. `pack dev` then also runs
`cargo build --target wasm32-unknown-unknown --release` with
`--import-memory`, and stages `<stem>.wasm` under
`_libs/wasm32-unknown-unknown/`. The descriptor is read from the dylib
as now, then from the module, and `pack dev` checks the two match.

### Config

```json
"packs": [
  { "id": "adcs", "lib": "adcs_systems", "libs": "..", "kind": "wasm",
    "memory": 16777216, "fuel": 10000000 }
]
```

`kind` defaults to `dylib`. `memory` and `fuel` are the pack's defaults
for every instance. `PackRef::artifact` picks
`<libs>/wasm32-unknown-unknown/<stem>.wasm` for a wasm pack.

### Python

```python
from adcs_pack import PACK, Nav, Plant

fsw = Target(cycle_rate=100.0)
fsw.wasm(PACK, fuel=10_000_000, memory=16 << 20)
```

`Pack` is a frozen dataclass, so `Target.wasm` records the choice on the
`Target`, keyed by pack id, and `to_config` writes `kind`, `memory`, and
`fuel` into that pack's entry. The generated modules are unchanged, and
since the ABI version stays 9, none need regenerating.

## spacewasm

The move rewrites `wasm/guest.rs` and nothing else. `Guest` already
avoids imports, growth past a host-placed memory, and resumable calls.

What stays:

- **Instruction set.** spacewasm runs wasm 1.0 plus mutable globals.
  Rust's `wasm32-unknown-unknown` also emits sign-extension,
  non-trapping float-to-int, bulk memory, multi-value, and
  reference-types by default, and its prebuilt `std` already contains
  them. The spacewasm backend waits until spacewasm runs what rustc
  emits.
- **Allocator hook.** spacewasm allocates through an `extern "C"`
  `__spacewasm_alloc` the embedder defines. The host defines it over the
  global allocator. All of spacewasm's allocation happens at
  instantiation, when we allow it.
- **Threads.** spacewasm's `Memory` and engine are `!Send`. `Step` is
  not `Send` and the coordinator builds and runs on one thread, so
  nothing requires `Guest` to be `Send`.
- **Fuel units.** Neither runtime's unit is time. A pack's `fuel` is
  tuned per runtime, and a migration re-tunes it.

## Tests

Ring, in `ring/`:

- Kani: writer and view never panic and stay in bounds for arbitrary
  control words.
- Unit: a writer whose reader slot and `committed` are overwritten with
  extreme values returns `WriteError::Corrupt`; a view whose cursor is
  overwritten returns `ReadError::Corrupt`.

Unit, in `wasm/`:

- Layout: the wasm32 `RawSlice`, `RawRing`, and `RawPort` encoders
  against `size_of`/`offset_of` from a wasm32 build, pinned as golden
  bytes. `Option<RawOwner>` is the size of `RawOwner` on both targets.
- `Mirrors`: the thread tests pass untouched after the move; `drain`
  returns `Corrupt` on a bad record.
- `Guest` with hand-written `wat` modules (the `wat` dev-dep fsw-2
  used):
  - a loop that never ends is `OutOfFuel` within the grant;
  - an out-of-bounds store is `MemoryOutOfBounds`, and memory outside
    the guest is untouched;
  - a module whose minimum exceeds the memory ceiling is refused;
  - `memory.grow` past the ceiling returns -1 and does not move the base;
  - `table.grow` past the store limit fails;
  - a module missing `metor_fsw_alloc` is `MissingSymbol`.
- `WasmStep` over `wat` modules that return each status or trap: every
  row of the ends table, outputs drained after a trap, and the real
  input ring's reader slot free after an end.
- Drop order, under Miri: dropping a `WasmStep` with live mirrors frees
  guest memory only after the ring handles in it.

Integration, in `tests/`, over a test pack built for both kinds. Its
wasm32 build is also the CI check that nothing host-only leaks into a
`default-features = false` build.

- A ring the 64-bit host formats attaches in the wasm32 guest, and each
  side reads what the other wrote.
- The guest's table builds, logs, and steps without touching a clock:
  a system that logs every cycle runs 100 cycles.
- Nominal: a pipeline `static -> wasm -> static` delivers same-cycle
  data, in order, with fan-in merged by timestamp.
- The same pack as a dylib and as a module produces the same records for
  integer and frame-copy systems, with readers that keep up so no
  backpressure is involved.
- A `#[sequence]` in a module sleeps, waits, completes, and reports
  `Completed`. Aborted through its channel, it runs its handlers and
  reports `Aborted`. A `Reset` makes it again from its config.
- Off-nominal: a guest that stops reading its input drops only its own
  copies and reports them. A guest whose output reader is behind drops
  at the host copy and reports it.
- Failure: a panicking guest ends `Panicked`, with the host's `trap`
  line and the guest's panic message on its `log`. An exhausted guest
  ends `Failed`, its producer keeps writing, and the next system still
  runs. A guest that scribbles over its rings' control words ends
  `Failed("corrupt ring")`, and no downstream reader sees the damage.
- Slot: load a wasm candidate with its own `memory` and `fuel`, swap to
  a dylib one and back, and refuse a module whose hash does not match.
- `pack dev` refuses a `wasm = true` pack with an async system.

Gate: the ADCS convergence test gains a third run with the `adcs` pack as
a module. It converges under the same bound. Its `ctrl.motor_cmd` stream
matches the native run within a tolerance, not bit for bit, because the
module's `libm` is not the host's.

## Phases

1. Ring hardening and its proofs.
2. The `host` and `threads` features, `maitake-sync`, and the two
   halves of `thread`. The
   ADCS pack builds for wasm32; the guest-table test runs under wasmi.
   This phase also confirms that `tracing-subscriber`'s registry works on
   wasm32 and that nothing in a guest reads a clock.
3. `Option<RawOwner>`, the allocator exports, `PackSystemDef::kind`, and
   `enum Pack` with `DlPack` alone. Every existing test passes.
4. `Mirrors` extracted, `Guest` over wasmi, `WasmPack`, `WasmStep`.
5. Wasm candidates in `DlSlot`, and `deploy/slot.rs`.
6. `pack dev`, config `kind`, `Target.wasm`, and the gate.

## Decisions

1. **Copy both ways.** Inputs and outputs cross by copy through
   `Mirrors`. Rings stay coordinator-owned, and a guest cannot damage a
   ring another system reads.
2. **One instance per system instance.** Faults, memory ceilings, and
   fuel are per system. The module compiles once per pack.
3. **Out of fuel ends the step.** No resumable calls. Pausing is a
   sequence's job inside the guest.
4. **Host-placed, fixed memory.** Allocated once at instantiation, never
   moved, freed after the store.
5. **One ABI, signatures unchanged, version unchanged.** wasm32 passes a
   `RawSlice` by pointer under the standard C ABI, and a golden test
   pins that. Guests may use the post-1.0 features rustc emits.
6. **No imports.** A module exports the ABI and imports only its memory.
   It has no clock (`now` arrives as an argument) and no host calls.
7. **Pack kind is per pack, per target.**
8. **Feature flags, not a crate split.** `host` and `threads`; a pack's
   source and dependency line are the same for both targets.
9. **Async systems compile everywhere.** Without `threads`, `place`
   refuses. `pack dev` checks for async systems first. There is no trait
   over placement, since one build never has both.
10. **An enum over pack kinds, not a trait.** The set is closed.
11. **No `destroy` for wasm, no kept loader guest.** Freeing the memory
    is the teardown, and a scratch guest answers each `def`.

## Open decisions

- Per-system `fuel` and `memory` overrides in `SystemConfig`, beyond the
  pack defaults. Nav and a mode switch need very different grants.
- Reporting fuel used per cycle, beside `exec_time_ns` in
  `system_status`, to help tune grants.
- Reusing a guest's arena across `Reset` and `Load`, so a commanded
  remake allocates nothing.

## Shipped deviations

What landed differs from the plan above as follows, by task in
`14-wasm-impl.md`.

- **T1.** The writer and the view also check their cursors' alignment
  and that no reader is lapped. `ReadGrant` is no longer `Send`.
- **T2.** The threads gate is
  `all(feature = "threads", not(target_family = "wasm"))`. `thread/os/`
  has child modules. `catch_step` and `Runner` moved to
  `coordinator/step.rs`. Items only the host uses sit in host-gated
  modules or impl blocks, and test modules that need the host are gated
  on it. The ring takes
  stellarator as a dev-dependency.
- **T3.** The panic hook is `panic::log_panics`.
- **T4.** The codec takes `Option<&[u8]>`.
- **T5.** Each `drain_*` drains everything and returns the first error.
  `Mirror::drain` returns `mirror::Corrupt` for corruption on either
  the read or the write side.
- **T6.** The engine lives in `Module`. `Guest::new` takes the setup
  grant. `StoreLimits` allows two memories, since instantiation counts
  the host's memory again as the import. A mistyped export is
  `WasmError::Signature`, and a missing one stays
  `PackError::Wasm(WasmError::MissingExport)`. The interpreter's own
  errors are one `WasmError::Load`. `Guest::create` takes the export's
  argument tuple. The store drops before the arena by field order.
- **T8.** Guest errors in params are `ParamError::Wasm(String)`, and a
  trap in `create` is `ParamError::Panicked`. The fan-in test uses
  `meter`, since `Input::drain` reads producer by producer. `STACK` is
  64 Ki slots and `TABLE_ELEMENTS` 16 Ki. echo-pack gained `tick` and
  `spin`.
- **T9.** A dylib reference omits `kind`. `PackError::Read` covers an
  unreadable module, `PackRef::dylib` builds a dylib reference, and
  `cli::cargo` has `WASM_TRIPLE` and `wasm_name`. echo-pack's measured
  fuel: setup, the descriptor about 11.0M, a `def` at most 5.5M, and a
  `create` at most 4.5M; a step 0.7k to 9k.
- **T10.** `Limits { memory, fuel }` flattens into a reference and a
  slot candidate, and writes each only when set. `WasmPack::open` fills
  the defaults, and setup always runs under `DEFAULT_SETUP_FUEL`.
  `metor build` bakes a candidate's `kind` and limits into the slot, and
  `DlSlot` opens a candidate by its `kind`.
- **T11.** The generated module renders `wasm=True` only for a wasm
  pack. `pack dev` checks for async systems before it builds the module.
  The `sync-pack` fixture is a one-system pack with no async systems.
- **T12.** Superseded by `16-adcs-sequences-pack.md`. The `adcs` pack
  cannot build for wasm32: its contracts link C (WMM, and SOFA through
  `nox-frames`) and `hifitime` pulls `wasm-bindgen`. Instead, a new
  `seqs` pack runs the `commission` sequence as a module in the ADCS
  loop, beside the native `adcs` pack, and the gate compares its run
  with the linked pack's. The torque streams match bit for bit.
  - A guest cannot call `nox`'s `dot` or `norm`. They go through `faer`,
    whose `pulp` dependency compiles `simd128` functions into every
    wasm32 build, and wasmi without its `simd` feature refuses the
    module. `commission` sums its norm in scalar order.
  - `commission`'s measured fuel: the descriptor 4.5M, `def` 8.5M,
    `create` 3.7M, and a step 0.9k to 3.2k. The defaults stay.
- **Phase 2.** The ADCS pack does not build for wasm32; see T12.

### Follow-ups

- A slot candidate's Python `"pack"` reference does not carry `kind`,
  `fuel`, or `memory`.
- `LoadError::Hash` says "library" for a module.
- A panicked async `Thread` keeps its input reader slots.
- Two packs that carry one record each generate a class for it, so a
  static checker refuses an edge between them (`AttitudeEstimate` and
  `ModeCmd` in the ADCS `target.py`). The host matches by record id, so
  the config is right.
