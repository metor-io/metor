# Systems as `dlopen`'d shared objects (`dl-open`)

A `metor-fsw-2` system can live in a runtime-loadable **`cdylib`** that the coordinator
`dlopen`s, instead of being statically linked into one monolithic binary. The system
exports a small, versioned `extern "C"` surface (the `fsw_*` symbols); the host opens the
shared object, asks it to describe itself, wires it into the graph exactly like a
statically-linked system, and drives it across the C ABI each cycle.

The motivation is operational, not architectural: **rebuild and re-upload one system
without recompiling the whole image.** A small control-law `cdylib` is far cheaper to
cross-compile and push to a target than a large statically-linked FSW binary. The dlopen
path makes that possible while preserving every wiring, validation, sizing, telemetry, and
health guarantee the static path provides — a dlopen'd system is validated, sized, taps,
and reports health identically to a linked one.

This is a **pure front-end** onto the existing coordinator machinery: the coordinator still
owns the rings, sizes them, validates wiring, drives cyclic slots, and taps outputs for
telemetry. The only things that change are *where a system's typed code lives* and *how its
ports are reconstructed*.

The implementation is split across:

- `metor-fsw-ring` — the erased `Backing` (`Backing::raw` is its non-owning constructor)
  and `RingBuffer::attach_raw`, the non-owning ring attachment the system side
  reconstructs over a host region.
- `src/abi/mod.rs` — the C-ABI **contract both halves compile against**: the `repr(C)`
  handles (`FswRing`, `FswStatus`), the serialized descriptor mirrors (`PortDescMsg`,
  `SystemDescriptorMsg`), the `SYM_*` symbol-name constants, the `RawBinder`, and the
  generic `run_*` helpers the macro delegates to. (Tests in `src/abi/tests.rs`.)
- `metor-fsw/macros` `export_system!` — the one-line system-author macro that emits the
  `#[unsafe(no_mangle)]` `fsw_*` exports.
- `src/dl.rs` — the host loader `DlSystem`, its `DlError`, and the `DlSlot` that drives a
  `.so` across the ABI as a `Box<dyn CyclicSlot>`.
- `src/coordinator/mod.rs` — `CoordinatorBuilder::add_dl_cyclic`, which registers a loaded
  system through the same descriptor/edge/sizing/telemetry passes as a static one.
- `src/wiring/` — the `Wiring` data model, the `WiringBuilder`, the KDL deserializer, the
  shared `resolve`, the schema-guided `encode_kdl_params`, and the `build_artifacts` driver.

The dlopen ABI and loader (`abi`, `dl`) are available **with or without the `kdl` feature**,
because system construction is decoupled from KDL (the `BuildSystem` contract, below). The
`Wiring` data model, the builder, the KDL deserializer, and the build driver ride the `kdl`
feature alongside the wiring front-end they share a resolver with.

The model is **same-process and cyclic-only**. Multi-process systems, async systems, and
cross-process wake are deferred; see [Limitations and future work](#limitations-and-future-work).

---

## The erasure boundary

The framework is already split along the exact seam the dlopen path needs:

> **The host stays type-erased (raw rings + serialized descriptors). The system `.so` stays
> typed (it has the frame type `F`, so it reconstructs `Output<F>`/`Input<F>` from the raw
> regions the host hands it).**

Nothing about a ring requires a Rust type to move bytes. `Output::write` is a single
`try_write(frame.as_bytes())` with no serialization — the payload bytes *are* the table
bytes *are* the ring record bytes. The concrete frame type `F` only adds *typing inside a
system*: the producer's `as_bytes()` and the consumer's `FrameRef` accessors. So the
boundary is drawn precisely where the type disappears already.

- **Host side (the coordinator):** owns and sizes every ring, validates wiring from
  serialized descriptors, and drives each dlopen'd system through a `DlSlot : CyclicSlot`
  that forwards `init`/`step`/`shutdown` across the C ABI by passing **raw ring handles**
  (base pointer + length + role). It never reconstructs `Output<F>`/`Input<F>`.
- **System side (the `cdylib`):** a one-line `export_system!` macro generates the C-ABI
  entry points. Inside them the `.so` reconstructs a `RingBuffer` over each host region
  (a non-owning attach), walks its port bundle positionally (the standard bind contract,
  over raw regions), and runs an ordinary `CyclicRunner<S, O>` — the **same host driver
  type**, the identical monomorphic code.

The only Rust values that cross the ABI are **serialized bytes** (the descriptor, the
postcard params blob) and **`repr(C)` handles** (`FswRing`, the timestamp word, function
pointers, `FswStatus`). No `Vec`, `Arc`, `VTable`, or trait object is ever passed by value —
which is what makes "dlopen across a stable Rust ABI" sound here.

Three properties of the existing machinery carry the whole design:

1. **A ring is position-independent shared state.** Every cursor, the committed handshake,
   the reader table, and the data region live at fixed region-relative offsets inside one
   contiguous block; nothing inside it is a process-local pointer. `read_header(base)`
   recovers `capacity`, `data_offset`, `reader_table_offset`, and `max_readers`
   from a bare base pointer, and the ring already rebuilds a working `RingBuffer` over a
   region it never allocated. **In the same address space, a `.so` reconstructing a ring over
   the host's `base` pointer sees the identical atomics the host's other systems do** — no
   copy, no IPC.

2. **Rings are backing-erased.** A `RingBuffer` carries one concrete
   `Backing { base, len, ctx, drop_fn }` value; how a region is owned (heap, mmap, or a
   non-owning attach) is a runtime constructor choice, not a type. `Output<F, …>` /
   `Input<F, …>` therefore never know or care where their ring's bytes live — the same
   port types, bundles, and bind path serve a host-allocated ring and a `.so`'s
   non-owning view into the host's regions (below).

3. **The host already deals in erased descriptors.** `PortDesc`/`SystemDescriptor` are how
   the coordinator sizes, allocates, and validates a system *without constructing it*. The
   dlopen path makes serializable mirrors of these so they survive the trip out of a `.so`,
   leaning on the fact that the payload a `PortDesc` carries — a `metor_proto::vtable::VTable`
   — already derives `Serialize`/`Deserialize`/`postcard_schema::Schema`.

### `Backing::raw` and `attach_raw`

`Backing` (in `metor-fsw-ring`) is one concrete struct — `{ base, len, ctx, drop_fn }` —
whose constructors pick the ownership at runtime: `Backing::heap` (a leaked `Box<[Word]>`
the `drop_fn` reconstructs and frees), the feature-gated `Backing::mmap` (a boxed `MmapMut`
behind `ctx`), and `unsafe Backing::raw(base, len)` — a host-provided region the ring does
**not** own, `drop_fn: None`, so `Drop` frees nothing. `Backing::raw` is the dlopen attach:
`RingBuffer::attach_raw(base, len)` wraps it, validating the region header (via
`read_header`, including the arch tag) and building a working ring over it — the same path
the mmap attachment uses (`unsafe RingBuffer::attach(backing)`, also the public door for
custom `Backing::from_raw_parts` regions), with the mapping step removed. Same-process,
this is called directly over the host ring's `RingBuffer::region()` `(base, len)`. Its
safety contract: the named region is a live, header-valid ring that outlives every
`Writer`/`View` produced from it and is not concurrently torn down.

### The backing-erased system stack

Because the backing is an erased runtime value, nothing above the ring is generic in it:
**one monomorphic bind path serves the host (heap rings) and the `.so` (non-owning attaches
via `RingBuffer::attach_raw`)**.

- **Traits are plain.** `trait System` and `trait CyclicSystem: System`, with
  `type Input: BindPorts` / `type Output: BindPorts`. A system author writes
  `impl System for Foo` and `impl CyclicSystem for Foo` — the only spelling; the same
  impl serves the static build and the `cdylib`.
- **Bundles are plain structs.** The author writes
  `#[derive(SystemInput)] struct FooIn { tick: Input<TickIn> }` and the matching
  `SystemOutput` struct. The host and the `.so` bind the identical types.
- **Bind is abstracted over a `RingSource`.** `trait BindPorts: Sized` has
  `fn bind<S: RingSource>(src: &mut S) -> Self`, and a `RingSource` yields the next plain
  `RingBuffer` (plus its wake endpoints) for an output or an input in `descriptors()`
  order. The host's `Binder` is a `RingSource` over its pre-allocated `BoundPort`s; the
  `.so`'s `RawBinder` is a `RingSource` that pops the next `FswRing` and `attach_raw`s it.
  `RingSource` also carries a provided `output_registry` that panics by default and is
  overridden only by the host `Binder` (the telemetry downlink needs it; a dlopen'd system
  is never the downlink, so its `RawBinder` uses the default).
- **`CyclicRunner<S, O>`** is the one driver type on both sides — the health / timing logic
  is reused verbatim, and each dl bundle compiles **once**, not once per backing.
  `dyn CyclicSlot` is unchanged: a host slot is a `CyclicRunner`, a dlopen'd slot is a
  `DlSlot` that forwards across the ABI.

---

## The C-ABI surface

A system `cdylib` exports a small, versioned, `extern "C"` surface. Every export is
`#[unsafe(no_mangle)]`, takes/returns `repr(C)` types or serialized byte slices, and **never
unwinds across the boundary**. An opaque state pointer threads a single boxed runner through
the lifecycle.

The exported symbols, their `SYM_*` name constants (the one source of truth the host resolves
by), and their resolved signatures:

| Symbol | `SYM_*` constant | Signature | Purpose |
|---|---|---|---|
| `fsw_abi_version` | `SYM_ABI_VERSION` | `fn() -> u32` | The ABI word, checked for equality first. |
| `fsw_describe` | `SYM_DESCRIBE` | `fn(ByteSink, *mut c_void) -> i32` | Serialize the `SystemDescriptorMsg` to a host sink. |
| `fsw_create` | `SYM_CREATE` | `fn(*const u8, usize) -> *mut c_void` | Decode params, construct the system, box the state. |
| `fsw_bind_init` | `SYM_BIND_INIT` | `fn(*mut c_void, *const FswRing, usize, *const FswRing, usize)` | Reconstruct the typed bundles, run `System::init`. |
| `fsw_execute` | `SYM_EXECUTE` | `fn(*mut c_void, u64) -> FswStatus` | Run one cyclic `step`, return status. |
| `fsw_shutdown` | `SYM_SHUTDOWN` | `fn(*mut c_void)` | Run `System::shutdown` once. |
| `fsw_destroy` | `SYM_DESTROY` | `fn(*mut c_void)` | Drop the boxed state inside the `.so`. |

`fsw_describe` carries the system's full self-description, including its telemetry schema, so
there is **no separate per-port announce export**: the host derives every output's prefixed
telemetry schema from the metadata the descriptor carries (see
[Telemetry and health](#telemetry-and-health)).

### Version word

`FSW_ABI_VERSION` is a single monotonic `u32` (currently `4` — the version-history comment on
the constant in `src/abi/mod.rs` is the changelog), exported as `fsw_abi_version`. The host
checks it for equality before any other call. It is bumped on any change to the C surface or
the `*Msg` wire structs — exactly once per released ABI shape. A mismatch fails the load
cleanly as `DlError::VersionMismatch` — never a crash.

This layers on the ring's own guards: the region header stamps an `arch_tag` (pointer width +
endianness), so `attach_raw` rejects a foreign-arch region at the header handshake, and the
ring layout `VERSION` covers ring-layout changes orthogonally.

### `repr(C)` handles

`FswRing` is the ring-region handle the host fills from its ring table and the `.so` turns
back into a ring:

```rust
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FswRing {
    pub base: *mut u8, // region base — the host ring's RingBuffer::region().0
    pub len:  usize,   // region length — the host ring's RingBuffer::region().1
    pub role: u8,      // ROLE_INPUT (0) or ROLE_OUTPUT (1)
}
pub const ROLE_INPUT:  u8 = 0; // the system registers a read-only View
pub const ROLE_OUTPUT: u8 = 1; // the system is the buffer's sole Writer
```

Everything else the system needs — capacity, data offset, reader-table offset,
`max_readers` — is **self-describing in the region header**, recovered by
`attach_raw`. So the handle is just `(base, len, role)`. Single-writer discipline is preserved
by `role`: for a `ROLE_OUTPUT` region the host hands the region but creates **no** writer
itself, so the `.so` is the sole writer; for a `ROLE_INPUT` region the `.so` calls `view()`,
claiming a reader slot by CAS in the shared reader table — which is why the host already sizes
`max_readers` to include every consumer. A dlopen'd consumer is just another reader slot.

`FswStatus` is the lifecycle status `fsw_execute` returns, `repr(u32)` for FFI stability:

```rust
#[repr(u32)]
pub enum FswStatus {
    Running  = 0, // ran (or runnable); keep cycling it
    Panicked = 1, // a panic was caught at the boundary, or the state was never bound
    Done     = 2, // a sequence occupant ran to completion (sequences-slots.md)
}
```

The host maps `Panicked` to a `SlotState::Stopped { reason: Panicked }` and folds it into the
coordinator status frame; it is a permanent hard-stop, and a stopped slot is never ticked
again (and is destroyed immediately — see `DlSlot` below). There is no input-driven stop:
the rings are lossless, so a slow reader backpressures its producer instead of being lapped.

`ByteSink` is the host-owned callback a describe-style export hands its serialized bytes to:

```rust
pub type ByteSink = extern "C" fn(ctx: *mut c_void, buf: *const u8, len: usize);
```

The `.so` allocates and frees its own buffer and the host copies, so neither side frees the
other's allocations.

### The timestamp word

`fsw_execute`'s `now: u64` carries the coordinator's **raw `Timestamp` tick**, not
nanoseconds. `Timestamp` is an `i64` of microseconds; the host `DlSlot` passes `now.0 as u64`
and the `.so` reconstructs `Timestamp(now as i64)`. It is a reversible, unit-agnostic raw-tick
contract.

---

## The serialized descriptor

`PortDesc` cannot cross by value: a Table port's `announce` field is a closure over the frame
type `F` (it produces the prefixed `VTable` + metadata for telemetry), and `F` does not exist
on the host side of the boundary. So the `.so` serializes mirrors of the unified descriptor
(`docs/design-port-unification.md`) that drop the closure and carry the data the host needs
to reconstruct everything — schema-tagged, so a `.so` declares message ports exactly like a
static system:

```rust
#[derive(Serialize, Deserialize)]
pub enum PortSchemaMsg {
    Table {
        frame_id: ComponentId,
        vtable:   VTable,                  // unprefixed; what compatible() validates against
        metadata: Vec<ComponentMetadata>,  // unprefixed; lets the host re-derive `announce`
    },
    Postcard {
        id: PacketId,                      // self-describing — the 2-byte id IS the schema
    },
}

#[derive(Serialize, Deserialize)]
pub struct PortDescMsg {
    pub name:        String,              // F::NAME / M::NAME (was &'static str)
    pub max_size:    usize,
    pub schema:      PortSchemaMsg,       // axis 1
    pub delivery:    Delivery,            // axis 2 — Snapshot | Log
    pub fan_in:      FanIn,               // axis 3 — One | Many
    pub telemetered: bool,                // the downlink/AllOutputs opt-out (A6)
}

#[derive(Serialize, Deserialize)]
pub struct SystemDescriptorMsg {
    pub name:    String,
    pub kind:    SystemKind,
    pub inputs:  Vec<PortDescMsg>,
    pub outputs: Vec<PortDescMsg>,
    pub params_schema: OwnedNamedType,     // <Params as postcard_schema::Schema>::SCHEMA
    pub capabilities: Vec<Capability>,     // host-only; non-empty is REJECTED at load (v1)
}
```

Serialization is **postcard** (the format `VTable`'s derives already target). `fsw_describe`
lowers the system's static `SystemDescriptor` (deriving each Table port's unprefixed vtable +
metadata by calling its `announce` with the empty prefix; a Postcard port lowers to its bare
id), postcard-encodes the `SystemDescriptorMsg`, and hands the bytes to the host `ByteSink`.
The host deserializes it, rejects a non-empty `capabilities` list (`ReceiveAll` needs the host
registry, which cannot cross the ABI — `DlError::UnsupportedCapabilities`), and calls
`SystemDescriptorMsg::into_descriptor()` to rebuild a `SystemDescriptor` — reconstructing each
`PortDesc` with its carried axes and synthesizing the missing `announce` closure from a Table
port's metadata — and feeds that descriptor to the existing builder path. The port name and
system name are `Box::leak`ed once at load to recover the `&'static str` the wiring path
expects.

The unprefixed `vtable` on each reconstructed Table `PortDesc` is exactly what `compatible()`
needs (a Postcard edge is pure id equality), so **wiring validation runs unchanged** on a
dlopen'd system.

`params_schema` is the load-bearing piece behind the single-encoding params model: it lets the
host **encode a system's params without linking that system's `Params` type** (see
[Params](#params-the-single-encoding)). The host stays fully schema-agnostic — it validates
frames from the serialized `VTable`s and encodes params from the serialized `Params` schema,
never linking a frame or param Rust type.

---

## The system side: `RawBinder`, `run_*` helpers, and the macro

### `RawBinder`

`RawBinder` is the `.so`-side `RingSource`, the twin of the host's `Binder` over
host-provided raw regions. It holds slice cursors over the input and output `FswRing` arrays;
`next_input`/`next_output` pop the next handle and `attach_raw` it, walking the ports in the
identical positional `descriptors()` order. Because the system is cyclic, every wake endpoint
is `NoWake`, default-constructed.

### The opaque state and the `run_*` helpers

The macro-generated exports are one-liners delegating to generic `run_*` helpers in
`src/abi/mod.rs`, so the real logic is testable without dlopen and the macro stays thin. The
opaque state they thread is:

```rust
struct AbiState<S> {
    pending:  Option<S>,                  // the constructed system, until bind_init
    runner:   Option<Box<dyn CyclicSlot>>, // the verbatim CyclicRunner, type-erased
    poisoned: bool,                        // latches a caught execute panic
}
```

- **`run_create<S>(params, params_len) -> *mut c_void`** postcard-decodes `S::Params`,
  constructs the system via `BuildSystem::new`, and boxes an unbound `AbiState`. Null params
  with length 0 is the documented empty-params case. Returns a null pointer if decoding or
  construction panics.
- **`run_bind_init<S, O>(state, inputs, n_in, outputs, n_out)`** builds a `RawBinder` over the
  handle arrays, binds `S::Input` and `S::Output` through it (the positional `descriptors()`
  walk over `attach_raw`), assembles a `CyclicRunner<S, O>`, and runs its `init`.
  Bind and init are fused into one call because both need the bound bundles and both run
  exactly once. A caught panic leaves the runner unbound, so the next `execute` reports
  `Panicked`.
- **`run_execute<S>(state, now) -> FswStatus`** reconstructs `Timestamp(now as i64)`, runs the
  verbatim `CyclicRunner::step` logic (time `execute`, publish
  health), and maps the runner's `SlotState` to an `FswStatus`. A caught `execute` panic
  latches `poisoned` and returns `Panicked`; an unbound or poisoned state returns `Panicked`
  too.
- **`run_shutdown<S>(state)`** runs `System::shutdown` once; a poisoned/unbound state is a
  no-op.
- **`run_destroy<S>(state)`** drops the boxed state inside the `.so`, running the user system's
  `Drop` and every port's `Drop` (the non-owning backings free nothing — the host still owns
  the regions). Idempotent on null.
- **`run_describe<S>(sink, ctx) -> i32`** lowers and postcard-encodes the descriptor as above,
  returning `0` on success and `-1` if anything panics.

Every helper wraps its body in `catch_unwind` and converts a caught panic to a null-safe
outcome (`FswStatus::Panicked`, a null pointer, or a non-zero describe code) — **no unwind ever
crosses the `extern "C"` boundary**.

### `export_system!`

A system author writes an ordinary `impl CyclicSystem` and adds one line to their `cdylib`
crate:

```rust
// crate-type = ["cdylib"] in Cargo.toml
metor_fsw_2::export_system!(MySystem);
```

The macro emits each `#[unsafe(no_mangle)] pub extern "C" fn fsw_*` as a one-liner delegating
to the matching `run_*` helper for `MySystem`. The only bound it adds beyond an ordinary
cyclic system is that `MySystem::Params` is `Serialize + Deserialize + Schema` (the postcard
params contract). A paramless system uses `type Params = ()`, which postcard-encodes to zero
bytes.

Because `#[no_mangle]` symbols are crate-unique, **one `export_system!` per `cdylib`** — one
system type per shared object, which is also the finest rebuild granularity.

### `BuildSystem` — construction decoupled from KDL

A system is constructed through `BuildSystem`:

```rust
pub trait BuildSystem: Sized {
    type Params;
    fn new(params: Self::Params) -> Self;
}
```

This is the kdl-independent construction contract `export_system!`/`fsw_create` need — they
bound only on it (plus `Deserialize`/`Schema` on `Params` where the wire/schema demand it). The
KDL static-registry path layers `RegisteredSystem` on top, a blanket impl that adds a
`Params: FromKdlNode` bound for the static path only. A dlopen'd system therefore needs **no**
`FromKdlNode` impl at all — it is constructed solely by decoding canonical postcard bytes in
`fsw_create`.

---

## The host side: `DlSystem`, `DlError`, `DlSlot`

### Loading and validation

`DlSystem::open(path)` loads a system `cdylib` and reconstructs its descriptor:

1. `dlopen`s the `.so` (via `libloading::Library`).
2. Resolves `fsw_abi_version` by its `SYM_ABI_VERSION` name and checks it equals
   `FSW_ABI_VERSION`.
3. Calls `fsw_describe` with a host sink that collects the postcard bytes, deserializes the
   `SystemDescriptorMsg`, keeps its `params_schema`, and `into_descriptor()`s the rest into a
   `SystemDescriptor`.
4. Resolves the lifecycle symbols (`fsw_create`/`fsw_bind_init`/`fsw_execute`/`fsw_shutdown`/
   `fsw_destroy`) by their `SYM_*` names, dereferencing each into a bare fn pointer kept alive
   by an `Arc<Library>`.

Every failure is a clean `DlError`, never a crash:

```rust
pub enum DlError {
    Open(libloading::Error),                                  // dlopen failed
    MissingSymbol { symbol: &'static str, source: … },        // a required fsw_* is absent
    VersionMismatch { found: u32, expected: u32 },            // ABI word disagreement
    Describe(i32),                                            // fsw_describe returned non-zero
    Decode(postcard::Error),                                  // descriptor failed to decode
}
```

`DlSystem` exposes `descriptor()` (the reconstructed self-description, with its ports'
`announce` closures ready to prefix the carried vtable ids) and `params_schema()` (the `.so`'s
exported `Params` schema, fed to `encode_kdl_params` so the host can schema-encode KDL config
without linking `Params`). The `.so` is opened **once** and reused for both the params encode
and the bound slot.

### `DlSlot`

`DlSlot` is the dlopen twin of a `CyclicRunner`'s `CyclicSlot` impl — indistinguishable from a
static runner in the coordinator's per-cycle loop, since both are just `Box<dyn CyclicSlot>` in
the same `Vec`. It holds an `Arc<Library>`, the resolved lifecycle fn pointers, the opaque
`*mut state`, the per-port `FswRing` arrays (built by the host at `build()` from its
already-computed edges and output rings), the descriptor name, and a tracked `SlotState`.

`DlSystem::into_slot` (called at `build()`) `fsw_create`s the opaque state from the postcard
params and captures the handle arrays. Then:

- **`init`** calls `fsw_bind_init`, handing the `.so` the per-port ring regions so it
  reconstructs its typed bundles and runs `System::init`.
- **`step(now)`** calls `fsw_execute(state, now.0 as u64)` and maps the returned `FswStatus`
  into the tracked `SlotState`. The panic check lives **inside the `.so`** (the boundary
  catches it) and is reported back via the status word; the host's status-frame machinery
  consumes it the same way it consumes a static runner's `SlotState`. A `Panicked` slot is
  telemetered through the coordinator status frame. A permanent stop runs `fsw_destroy`
  **immediately** (not at teardown): dropping the `.so`'s ports releases its reader slots
  (their attaches are non-owning, so nothing else is freed), so on lossless rings a dead
  consumer cannot backpressure upstream producers.
  `state` is nulled, so the eventual `Drop` is a no-op.
- **`shutdown`** calls `fsw_shutdown`.
- **`Drop`** calls `fsw_destroy`.

The host's per-cycle loop is **unchanged**.

### Teardown ordering

`DlSlot::Drop` calls `fsw_destroy` (dropping the `.so`'s non-owning ports and the user
system) **before** the `Arc<Library>` field drops (so no `.so` code runs after the library
unloads) and **before** the host ring table frees the regions (the coordinator drops its
`cyclic` slot vec before its `rings` field). So no non-owning attach ever outlives its region, and
all destructors run in the code that defined them. `state` is nulled after destroy, so a
double-drop is a no-op.

---

## Coordinator integration

`CoordinatorBuilder::add_dl_cyclic(name, loaded, params)` is the dl twin of `add_cyclic_named`.
It pushes the loaded system's reconstructed `SystemDescriptor`, records the system kind and
instance name, and records a `Reg::Dl` registration carrying the `DlSystem` and the postcard
params blob. At `build()` that registration's bind gathers the per-port ring regions (from the
host's already-computed connection map and output rings), `fsw_create`s the state, and produces
a `DlSlot` instead of a typed `CyclicRunner`.

Everything else is reuse: the same `compatible()` / `WireError` validation, the same ring
sizing and allocation, the same edge passes, the same `OutputRegistry` tap. A dlopen'd system's
output buffers land in the registry with their (prefixed) announce, so telemetry `All` taps
them like a static system's.

`add_dl_cyclic` is the low-level builder method; the `resolve` entry point drives it from a
`Wiring`.

---

## The wiring data model and front-ends

A mission's description is a plain Rust data model, `Wiring`, that is the **single source of
truth**, independent of any text format. Both front-ends produce it and one resolver consumes
it:

```text
KDL text ──parse──▶ Wiring ──resolve──▶ CoordinatorBuilder ──build──▶ Coordinator
              ▲                     ▲
        one deserializer      one shared resolver
   (the Rust WiringBuilder    (static Registry + dlopen)
        is the other)
```

### The data model

```rust
pub struct Wiring {
    pub coordinator: CoordinatorSpec,   // cycle_rate, default_depth, clock
    pub artifacts:   Vec<Artifact>,     // the cdylibs this mission loads
    pub systems:     Vec<SystemSpec>,
    pub slots:       Vec<SlotSpec>,     // runtime-loadable slots (sequences-slots.md)
    pub edges:       Vec<EdgeSpec>,     // from/out/to/in/delayed/kind (Frame|Msg)
}
// The telemetry downlink and the command uplink are ordinary systems — built-in
// registry types ("TcpDownlink"/"TcpUplink"), not dedicated fields.

pub struct Artifact {
    pub id:          String,            // referenced by SystemSpec::artifact
    pub crate_name:  String,            // cargo package, for the build driver
    pub cdylib:      String,            // libfoo.so / libfoo.dylib / foo.dll
    pub system_type: String,            // the ONE type= this .so's export_system! provides
    pub path:        Option<PathBuf>,   // resolved by the build driver; None until built
}

pub struct SystemSpec {
    pub name:     String,               // instance name (the telemetry prefix)
    pub ty:       String,               // the type= key
    pub artifact: Option<String>,       // Some(id) => dlopen; None => static Registry
    pub params:   ParamSource,
}

pub enum ParamSource {
    None,            // empty postcard bytes (dl) / a minimal synthesized node (static)
    Postcard(Vec<u8>), // canonical Params bytes (the typed Rust builder path)
    Kdl(String),     // the KDL system-node source text, re-decoded at resolve
}
```

`CoordinatorSpec`, `ClockSpec`, and `EdgeSpec` are serializable mirrors of the runtime
types (`CoordinatorConfig`, `ClockMode`, the connect edge), deliberately decoupled from
runtime values so the model is a pure serde data format. The conversion to runtime types
lives in `resolve`.

One `Artifact` exports a single system type (the fixed `fsw_*` symbols), but multiple
`SystemSpec`s may reference one `Artifact` to instance that type more than once — the loader
`dlopen`s the `.so` once and `fsw_create`s per instance. A system with `artifact = None`
resolves through the static `Registry`; the two kinds mix freely in one mission, so a mission
can statically link its stable systems and dlopen only the ones it iterates on.

### The Rust builder

`WiringBuilder` constructs a `Wiring` in code, so anything KDL can express, Rust can express:

```rust
let wiring = WiringBuilder::new()
    .coordinator(120.0, ClockSpec::Simulated { dt_secs: DT })
    .artifact("adcs", "adcs-systems", "adcs_systems", "Plant")  // lib= is a stem (cli-runner.md §4.6)
    .system("plant").ty("Plant").from_artifact("adcs").params(PlantParams { .. }).end()
    .system("nav").ty("Nav").from_static().params(NavParams { .. }).end()
    .connect("plant", "sensors", "nav", "sensors")
    .connect_delayed("ctrl", "torque_cmd", "plant", "torque_cmd")
    .telemetry("127.0.0.1:2240".parse().unwrap())   // sugar for a TcpDownlink system spec
    .build();
```

`SystemSpecBuilder::params<P: Serialize>(p)` postcard-encodes the typed value into a
`ParamSource::Postcard`. Because the app links the system's contract crate, this produces
exactly the bytes a dl system's `fsw_create` decodes.

### KDL as one deserializer

The KDL front-end (`parse`) deserializes the same `Wiring`. Its surface adds an `artifact` node
and a per-system `artifact=` reference:

```kdl
artifact "adcs" crate="adcs-systems" lib="adcs_systems" type="Plant"   // lib= is a stem (cli-runner.md §4.6)

system "plant" type="Plant" artifact="adcs" init_angle=0.5 init_rate=0.15
system "nav"   type="Nav"   meas_sigma=0.02            // no artifact= => static

connect "plant" -> "nav"  frame="sensors"
connect "ctrl"  -> "plant" frame="torque_cmd" delayed=#true
system "telemetry" type="TcpDownlink" addr="127.0.0.1:2240"
```

Note the two properties are spelled differently on purpose: the `artifact` node's own `lib=` is
the library **stem** (unchanged, cli-runner.md §4.6); a `system` node's `artifact=` references
that node's `id`. (`system … lib=…` was the original spelling but was hard-renamed to `artifact=`
— a guidance error, no alias — precisely so the two could not be confused.)

A `system` with no `artifact=` resolves statically. A `system` node's params are carried verbatim
as `ParamSource::Kdl(source_text)` and re-decoded at `resolve` through one shared serde
`Deserializer` over the KDL node (`src/wiring/de.rs`, `docs/design-kdl-serde.md`): a static system
deserializes them straight into its `Params: serde::de::DeserializeOwned` type; a dl system
deserializes the same node into a `serde_json::Value` and schema-encodes it (below). There is no
`FromKdlNode`/`FromKdlScalar` trait anymore — both paths share the one KDL-to-serde walk.

### Params: the single encoding

Per-system params must reach the `.so`, which owns the `Params` type — but the host must not
link it. The model is **one canonical encoding**: postcard `Params` bytes cross `fsw_create`
from *both* front-ends, so KDL and the Rust builder are provably identical on the wire. Postcard
reflection (`postcard-schema` + `postcard-dyn`) makes this work without the host ever linking
the concrete `Params` type:

- The `.so`'s `fsw_describe` exports its `Params` **schema** as an `OwnedNamedType`
  (`<Params as postcard_schema::Schema>::SCHEMA`), carried on `SystemDescriptorMsg`.
- **KDL front-end:** `encode_kdl_params(node_text, schema, system, reserved, skip_args)`
  (`src/wiring/mod.rs`) first deserializes the node into a `serde_json::Value` through the shared
  `de::params_value` walk (the same one the static path uses), then `conform_to_schema` checks
  that value against the `.so`'s schema field-by-field (`conform_value` coerces/recurses) and
  hands the conformed value to `postcard_dyn::to_stdvec_dyn` — producing the **same bytes** the
  typed Rust builder postcard-encodes. No `FromKdlNode`, no host-linked `Params`. Errors are
  span-aware `LoadError`s: `UnknownParam` (a property with no schema field), `MissingParam` (a
  required field with no property), `InvalidParam` (a property whose type does not match the
  field), `DlParamEncode` (an un-encodable schema shape).
- **Rust-builder front-end:** the app has the `Params` type (from the shared contract crate),
  so a typed value postcard-encodes directly to the same bytes.
- **`.so` side:** `fsw_create` postcard-decodes the bytes into the real `Params`.

The byte-equality of the schema-guided KDL encode and the typed builder encode is the headline
equivalence gate (asserted across mixed field types in `src/abi/tests.rs`). The shared
frame/param **contract** crate is shared among the system cdylibs (a producer and consumer must
agree on a frame's layout), **not** linked into the host.

### The build driver

`build_artifacts(&mut wiring, &opts)` reads the `Wiring`'s `artifacts` and, for each, runs
`cargo build -p <crate_name>` (debug or `--release`, plus any extra args via `BuildOptions`),
locates the produced shared object by parsing cargo's JSON artifact messages, and records it in
`Artifact.path`. Because each system `cdylib` is its own cargo package, **cargo already does the
incremental work** — only changed crates recompile. Failures are clean `BuildError`s (cargo not
spawnable, a non-zero build, or a missing `cdylib` in the output). The resolver then maps each
dl system to its `Artifact.path` and `dlopen`s it.

---

## Telemetry and health

Two system-wide invariants survive the boundary with no special handling.

**Implicit health and log ports.** Every system has implicit health + log output ports
appended after its user outputs. Because they are part of the system's output bundle, they are
already inside the `.so`'s descriptor and its bind walk — `RawBinder` binds them after the user
outputs exactly as the host does, and the host allocates their rings exactly as it does for a
static system (the descriptor enumerates them). They are simply two more `ROLE_OUTPUT`
`FswRing`s in the array, and the `.so` reuses `CyclicRunner`'s end-of-cycle logic to populate
the standard health counters.

**The telemetry `All` tap.** The tap works off the host's `OutputRegistry`, populated from each
system's descriptor at `build()` — and a dlopen'd system *has* a descriptor — so its output
buffers land in the registry like any other and the tap enumerates them unchanged. The one
wrinkle is the **prefixed announce** each registry entry needs: a dl `PortDesc` has no closure
over `F`, so the host synthesizes `announce` from the carried metadata. `into_port_desc` builds
a closure that, given an instance prefix, (a) re-prefixes the metadata names by a rehash and (b)
rewrites the vtable's baked component ids via `prefix_announce_vtable`. That rewrite builds an
unprefixed→prefixed id map from the metadata and rewrites every baked 8-byte `Op::Data` leaf id
whose value is a known component, leaving the frame-tag id and dynamic path templates untouched.
The result matches what a static system bakes via its own `announce` **bit-for-bit**, so
telemetry keys a dlopen'd output's components identically. The unprefixed vtable on the
`PortDesc` itself is left alone, so wiring `compatible()` still sees the frame-relative ids.

---

## Safety and containment

dlopen across a stable Rust ABI is the genuinely hard part; the containment rules:

- **No unwinding across `extern "C"`.** Every `run_*` helper wraps its body in `catch_unwind`.
  A caught panic becomes `FswStatus::Panicked` (which the host telemeters and hard-stops), a
  null pointer (`fsw_create`), or a non-zero describe code — never an unwind across the
  boundary. A caught `execute` panic also latches `poisoned`, so subsequent cycles
  short-circuit to `Panicked`.
- **Allocator ownership.** Each side frees only what it allocated. The state `Box` is created
  by `fsw_create` and dropped by `fsw_destroy` in the same `.so`; `fsw_describe` hands bytes to
  the host `ByteSink` and frees its own buffer. No `Vec`/`Box` is ever freed across the
  boundary.
- **`Drop` correctness.** Because the runner (and therefore the user system and every
  non-owning port) is dropped by `fsw_destroy` inside the `.so`, all destructors run in the
  code that defined them. The teardown ordering (above) guarantees `fsw_destroy` runs before
  the library unloads and before the host frees the ring regions.
- **No Rust types by value.** Only serialized bytes and `repr(C)` structs cross — which removes
  the `std`/`Arc`/trait-object-layout fragility that makes naive Rust-ABI dlopen unsound.

The `RawBinder` and `attach_raw` safety contract — each named region is a live, header-valid
ring that outlives every `Writer`/`View` the binder produces — is upheld by the coordinator,
which keeps the owning ring table alive past every `DlSlot`.

---

## The `adcs-fsw2` example

The ADCS example (`examples/adcs-fsw2`) runs its three-system mission as dlopen'd cdylibs. The
`Plant`/`Nav`/`Ctrl` systems each live in their own `cdylib` crate
(`examples/adcs-fsw2/systems/{plant,nav,ctrl}`) with a one-line `export_system!`; the frame
definitions live in a shared contract crate (`examples/adcs-fsw2/contracts`) that the cdylibs
and the host both depend on, so the `VTable`s match on both sides (the `frame_id`/`compatible`
check enforces it). The mission builds a `Wiring`, runs the build driver to produce the shared
objects, and resolves them through the dl loader. The closed loop runs the same `connect_delayed`
feedback edge, the same clock, and the same telemetry `All` downlink, and the convergence test
is the acceptance gate: the dlopen'd mission converges bit-for-bit with the static build, with
the host linking neither the system nor the contract crates at runtime. The integration test
`tests/dl_integration.rs` covers the same path against a minimal fixture
(`tests/fixtures/dl-fixture`) over a real `.so`.

---

## Limitations and future work

The dlopen subsystem is **same-process and cyclic-only**. The following are deferred; the ABI
and the ring leave the seams open for them.

- **Async systems.** Only `CyclicSystem` is loadable. An async system owns a `run` loop driven
  by `stellarator` and woken by ring notifiers; a notifier is `Arc`-backed and process-local,
  so it cannot cross the C ABI, and a `.so` running its own executor (or sharing the host's) is
  a second, orthogonal problem. Cyclic dlopen sidesteps both because cyclic edges are polled,
  not woken — every dl port uses `NoWake`. An async dl system would need a `.so`-side spawned
  task woken by a shared-memory wake word (the ring reserves `OFF_WAKE_WORD` / `FLAG_WAKE_SHARED`
  for this) in place of `fsw_execute`.
- ~~**Separate-process systems.**~~ Shipped: `docs/process-systems.md`. It came out exactly as
  sketched here — the worker process `attach_mmap`s the ring files the host allocated, turns the
  regions into the same positional `FswRing` handles, and drives an ordinary `DlSlot` through this
  ABI unchanged; the region identifiers cross in a launch manifest, and supervision is a spawn +
  step-doorbell + dead-owner reclamation (no restart yet).
- **Cross-process wake.** The shared-memory wake handshake (`OFF_WAKE_WORD` / `FLAG_WAKE_SHARED`)
  the ring reserves is unimplemented; it is the prerequisite for async cross-process systems.
- **Hot reload.** Swapping a `.so` at runtime (drain → `fsw_destroy` → reload →
  `fsw_create`/rebind) is not supported.
- **ABI hardening.** Compatibility relies on the `fsw_abi_version` word plus the ring's
  `arch_tag`/`VERSION` region guards. An embedded `postcard_schema` digest of the
  descriptor/frame schemas (for forward-compat detection) and a conformance test that a `.so`
  and host agree on every frame `VTable` are possible additions.
</content>
</invoke>
