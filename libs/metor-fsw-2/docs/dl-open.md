# Work-Package 8 — Systems as `dlopen`'d shared objects (`dl-open`)

Status: **reviewed — decisions locked, ready to plan/implement** (2026-06-28). The three reviewer
forks are resolved: **same-process v1** (Q-scope/Q-backing/Q-handle), the **in-document `artifact`
node** for the type→cdylib map (Q-manifest), and **one postcard `Params` encoding** for both
front-ends (Q-params). See §6.3 and §11 for how each lands. The remaining open questions take the
doc's recommended defaults.
No Rust in this WP — this document specifies how a `metor-fsw-2` system becomes a runtime-loadable
**`cdylib`** that the coordinator `dlopen`s, instead of being statically linked into one
monolithic binary. It fills in DESIGN.md's "What are systems anyway?" section (currently TBD —
DESIGN.md:99–106) and is, like WP6, a **pure front-end** onto the **landed** WP5/WP6/WP7 machinery:
the coordinator still owns the rings, sizes them, validates wiring, drives cyclic slots, and taps
outputs for telemetry. The only thing that changes is *where a system's typed code lives* and *how
its ports are reconstructed*.

The motivating goal is operational, not architectural: **rebuild and re-upload one system without
recompiling the whole image.** A 4 MB control-law `cdylib` is a far cheaper artifact to cross-compile
and push to a target than a 90 MB statically-linked FSW binary. WP8 makes that possible while
preserving every wiring/validation/telemetry guarantee WP4–WP7 already provide.

Relevant landed code (read before implementing):
- `src/port.rs` — `Output<F, B, …>` / `Input<F, B, …>` are **already generic over the ring
  `Backing` `B`** (port.rs:48, 128). `Output::new(writer)` / `Input::new(view)` wrap a raw ring
  handle (port.rs:60, 144); `Output::write` is a single `try_write(frame.as_bytes())` with no
  serialization (port.rs:97). This is the whole reason the `.so` boundary is tractable: the payload
  bytes == table bytes == ring record bytes.
- `src/descriptor.rs` — `PortDesc { frame_id, frame_name, vtable, max_size, rate_hint, announce }`
  and `SystemDescriptor { name, kind, inputs, outputs }` (descriptor.rs:29, 100). The `announce`
  field is a `fn(&str) -> (VTable, Vec<ComponentMetadata>)` **fn-pointer closing over `F`**
  (descriptor.rs:51) — the one part of `PortDesc` that **cannot** cross a stable ABI by value.
  `compatible(producer, consumer)` (descriptor.rs:123) validates a wire edge from `vtable` alone.
- `src/coordinator.rs` — the host is **already fully type-erased**: it drives cyclic systems through
  `Box<dyn CyclicSlot>` (coordinator.rs:240, 930), binds ports through the positional `Binder`
  contract (coordinator.rs:809), allocates every ring in a `RingTable`/`RingEntry` keyed by
  `BufferRole` (coordinator.rs:287–311), and sizes `max_readers = fan_out + n_registry_consumers +
  slack` (coordinator.rs:696). It never names a concrete `F`.
- `src/system.rs` — `CyclicRunner<S, O>` owns `{ system, input, output, state }` between cycles and
  implements `CyclicSlot` (system.rs:218, 294); `step(now)` does the lapped→hard-stop check, times
  `execute`, and publishes health (system.rs:255–273). This is exactly the per-system driver a `.so`
  must reconstruct on its own side of the boundary.
- `src/binder.rs` — the positional `BindPorts::bind(Binder)` contract that hands each typed port its
  pre-allocated ring in `descriptors()` order (binder.rs:73–135). The `.so` needs the same
  positional walk, but over **host-provided raw regions** rather than `BoundPort`s.
- `src/wiring.rs` — KDL is **already a pure front-end**: `load()` parses KDL, resolves each
  `type="Foo"` against a `Registry` of `fn`-pointer `SystemFactory`s (wiring.rs:320, 452), and drives
  `CoordinatorBuilder`. dl-open replaces the *compile-time* factory with a *dlopen'd* one keyed by the
  same `type=`.
- `src/registry.rs` / `src/telemetry.rs` (WP7) — the `OutputRegistry` taps every output buffer by
  `view()` (registry.rs:47) and the telemetry `All` mode enumerates them; each entry carries a
  **prefixed** announce vtable + metadata (registry.rs:33, coordinator.rs:908). WP8 must keep
  dlopen'd outputs visible to this tap.
- `ring/src/lib.rs` — the ring region is **a single contiguous, position-independent block**: all
  state lives at region-relative byte offsets, no `Box`/`Arc` pointers inside it (lib.rs:14–18). The
  header records `capacity`, `data_offset`, `reader_table_offset`, `max_readers`, `overrun` and an
  `arch_tag` (lib.rs:47–55, 87); `read_header(base)` reconstructs all of it from a bare pointer
  (lib.rs:725), and `attach_mmap` already rebuilds a `RingBuffer` over a region it did not allocate
  (lib.rs:569–595). `Backing` is an `unsafe trait` over `(base, len)` (lib.rs:181); `Config`,
  `Overrun`, `Writer`, `View` are all `Backing`-generic. `OFF_WAKE_WORD` (lib.rs:61) and
  `FLAG_WAKE_SHARED` (lib.rs:73) are **reserved for future cross-process wake** but unused in v1.

---

## 0. Design summary (orientation)

The framework is already split along the exact seam WP8 needs:

> **The host stays type-erased (raw rings + serialized descriptors). The system `.so` stays typed
> (it has `F`, so it reconstructs `Output<F>`/`Input<F>` from the raw regions the host hands it).**

Nothing about a ring requires a Rust type to move bytes (port.rs:97; lib.rs:14–18). The concrete
frame type `F` only adds *typing inside a system* — the producer's `as_bytes()` and the consumer's
`FrameRef` accessors. So the boundary is drawn precisely where the type disappears already:

- **Host side (the coordinator):** owns and sizes every ring (`RingTable`, coordinator.rs:309),
  validates wiring from serialized descriptors, and drives each dlopen'd system through a new
  `DlSlot : CyclicSlot` that forwards `init`/`step`/`shutdown` across a C ABI by passing **raw ring
  handles** (base pointer + length). It never reconstructs `Output<F>`/`Input<F>`.
- **System side (the `cdylib`):** a one-line `export_system!` macro generates the C-ABI entry points.
  Inside them the `.so` reconstructs a `RingBuffer<RawBacking>` over each host region, walks its port
  bundle positionally (the `binder.rs` contract, but over raw regions), and runs an ordinary
  `CyclicRunner<S, O, RawBacking>` — the **same host driver type**, just monomorphized in the `.so`
  over a non-owning backing.

The only Rust values that cross the ABI are **serialized bytes** (the `SystemDescriptor`, the params
blob) and **repr(C) handles** (`FswRing`, the timestamp, function pointers). No `Vec`, `Arc`, `VTable`,
or trait object is ever passed by value — which is what makes "dlopen across a stable Rust ABI"
(genuinely hard, DESIGN.md:104) actually safe here.

v1 is deliberately the **smallest thing that proves the rebuild-one-piece value**: one *cyclic* system
in a *same-process* `cdylib`, wired through the existing KDL/`Registry` path, with its outputs visible
to the WP7 telemetry tap. Async systems, separate processes, and hot reload are reserved future work
(§5, §9), with the ABI seams left open for them.

---

## 1. The erasure boundary we build on (no new transport)

The data path is **already** ABI-stable; WP8 invents no new transport. Three landed facts carry the
whole design:

1. **A ring is position-independent shared state.** Every cursor, the committed handshake, the reader
   table, and the data region live at fixed region-relative offsets inside one block; nothing inside
   it is a process-local pointer (lib.rs:14–18, 47–67). `read_header(base)` recovers `capacity`,
   `data_offset`, `reader_table_offset`, `max_readers`, and `overrun` from a bare base pointer
   (lib.rs:725), and `attach_mmap` already rebuilds a working `RingBuffer` over a region it never
   allocated (lib.rs:569). **In the same address space, a `.so` reconstructing a ring over the host's
   `base` pointer sees the identical atomics the host's other systems do.** No copy, no IPC.

2. **The ports are `Backing`-generic — but the *bundles* and the *bind path* are not (yet).**
   `Output<F, B, …>` / `Input<F, B, …>` already take the backing as a type parameter (port.rs:48, 128)
   and their read/write methods work for any `B`. **However**, a user's bundle (`struct PlantOut {
   sensors: Output<Sensors> }`) pins `B = BoxBacking` in its field types, `type Output = Out<PlantOut>`
   pins it in the associated type, `Binder::next_output` returns `RingBuffer<BoxBacking>`
   (binder.rs:103), and `Output::bind` is impl'd only for `BoxBacking` (port.rs:81). So a dlopen'd
   system that must hold `Output<F, RawBacking>` views into the host's regions requires the **whole
   `System`/bundle/bind stack to become `Backing`-generic** — see §1.4. (Reviewed decision: we take
   this refactor to preserve zero-copy views, rather than the host-mediated copy-in alternative.)

3. **The host already deals in erased descriptors.** `PortDesc`/`SystemDescriptor` (descriptor.rs)
   are how the coordinator sizes, allocates, and validates a system *without constructing it*. WP8
   only needs to make these **serializable** so they survive the trip out of a `.so` — and the
   payload they carry (a `VTable`) **already has a wire form** (`metor_proto::vtable::VTable` derives
   `Serialize`/`Deserialize`/`postcard_schema::Schema`, vtable.rs:230). We lean into that symmetry
   rather than inventing a descriptor encoding.

### 1.1 `RawBacking` — the one new ring primitive

A new `Backing` impl in the ring crate, holding a host-provided region it does **not** own:

```rust
/// A ring region owned by someone else (the host, or another process's mmap). The
/// pointer/len are asserted valid + stable by the caller; `Drop` frees nothing.
pub struct RawBacking { base: *mut u8, len: usize }
unsafe impl Backing for RawBacking { /* base()/len() return the stored pair */ }

impl RingBuffer<RawBacking> {
    /// # Safety: `base..base+len` is a live ring region (header validated) that
    /// outlives every Writer/View produced here, and is not concurrently torn down.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError>;
}
```

`attach_raw` is `attach_mmap` (lib.rs:569) with the mapping step removed: validate the header via
`read_header`, then build `Inner { backing: RawBacking, …geometry from header… }`. This is the
"`from_raw`-style path" the WP brief names. Same-process v1 uses it directly over the host's
`BoxBacking.base()`; the cross-process future (§5) uses `MmapBacking`/`attach_mmap` — **the same
ports and the same `RawBacking` reconstruction logic, only the region's provenance differs.**
*(Landed in WP8 1A: `RawBacking` + `attach_raw`, sharing a private `from_validated` with `attach_mmap`.)*

### 1.2 Making the `System` stack `Backing`-generic (the real shape)

Because a dlopen'd cyclic system holds `Output<F, RawBacking>`/`Input<F, RawBacking>` views into the
host's regions, the bundle, the `System` traits, and the bind path must all parameterize over `B`.
The refactor (WP8 1B) is mechanical but cross-cutting; the target shape:

- **Traits gain a defaulted backing param.** `trait System<B: Backing = BoxBacking>` and
  `trait CyclicSystem<B: Backing = BoxBacking>: System<B>`, with `type Input: SystemInput + BindPorts<B>`
  / `type Output: SystemOutput + BindPorts<B>`. Every existing `impl System for Foo` becomes
  `impl<B: Backing> System<B> for Foo`. Static call sites (`add_cyclic`, `CyclicSystem::descriptor`)
  resolve `B = BoxBacking` through the default, so the host path stays **source-compatible**.
- **Bundles become generic.** The author writes `struct PlantOut<B: Backing = BoxBacking> { sensors:
  Output<Sensors, B>, … }`. `SystemInput`/`SystemOutput` stay non-generic traits (their `descriptors()`
  is static metadata and `any_lapped(&self)` has no `B` in its signature), impl'd for the generic
  bundle for all `B`. The `#[derive(SystemInput/SystemOutput)]` already propagates `#generics`
  (macros/src/system.rs:78,87) — it is extended to emit a `BindPorts<B>` impl over a ring source.
- **Bind is abstracted over a `RingSource`** so one bundle `bind` serves both providers:
  ```rust
  trait RingSource { type B: Backing;
      fn next_output<WD, WS>(&mut self) -> (RingBuffer<Self::B>, WD, WS);
      fn next_input <RD, RS>(&mut self) -> (RingBuffer<Self::B>, RD, RS); }
  trait BindPorts<B: Backing> { fn bind<S: RingSource<B = B>>(src: &mut S) -> Self; }
  ```
  The host's `Binder` impls `RingSource<B = BoxBacking>` (from its `BoundPort`s, binder.rs:103); the
  `.so`'s `RawBinder` impls `RingSource<B = RawBacking>` (popping the next `FswRing` and `attach_raw`-ing
  it, §3). `Output<F, B>::bind`/`Input<F, B>::bind` become generic over the source. `RingSource` also
  carries a provided `output_registry(&self) -> Arc<OutputRegistry>` that **panics by default** and is
  overridden only by the host `Binder` (the telemetry downlink's bundle needs it; a dlopen'd system is
  never the downlink, so its `RawBinder` uses the default) — landed in 1B. **No host ring
  allocation or sizing changes** — only the binder is now one impl of a shared trait.
- **`CyclicRunner<S, O, B>`** (and the `Out<O, B>`/`HealthPort<B>` it wraps, already `B`-carrying)
  thread `B`; the host instantiates `CyclicRunner<_, _, BoxBacking>`, the `.so`
  `CyclicRunner<_, _, RawBacking>` — the lapped→stop/health/timing logic (system.rs:255–273) is reused
  verbatim. `dyn CyclicSlot` stays backing-free: a host slot is a `CyclicRunner<…, BoxBacking>`, a
  dlopen'd slot is a `DlSlot` that forwards across the ABI (§4.2), so no `B` leaks into the trait object.

---

## 2. The C-ABI surface of a system `.so`

A system `cdylib` exports a small, versioned, `extern "C"` surface. All exports are `#[no_mangle]`,
take/return `repr(C)` types or serialized byte slices, and **never** unwind across the boundary
(§2.5). The opaque state pointer threads a single boxed runner through the lifecycle.

```c
/* Version + identity ----------------------------------------------------- */
uint32_t fsw_abi_version(void);          /* must equal the host's FSW_ABI_VERSION */

/* Self-description (wiring validation, telemetry schema) ------------------ */
/* Serializes a SystemDescriptorMsg (postcard) and hands it to the host via a
 * host-owned sink callback, so neither side frees the other's allocations. */
int32_t  fsw_describe(void (*sink)(void *ctx, const uint8_t *buf, uintptr_t len),
                      void *ctx);

/* Lifecycle (opaque state pointer threads through) ----------------------- */
void    *fsw_create(const uint8_t *params, uintptr_t params_len);   /* -> *mut Runner */
void     fsw_bind_init(void *state,
                       const FswRing *inputs,  uintptr_t n_in,
                       const FswRing *outputs, uintptr_t n_out);
uint32_t fsw_execute(void *state, uint64_t now_nanos);   /* -> FswStatus */
void     fsw_shutdown(void *state);
void     fsw_destroy(void *state);

/* Prefixed schema for one output port (telemetry; §7) — optional, see Q-announce */
int32_t  fsw_announce(void *state, uintptr_t out_idx,
                      const uint8_t *instance, uintptr_t instance_len,
                      void (*sink)(void *ctx, const uint8_t *buf, uintptr_t len),
                      void *ctx);
```

### 2.1 `fsw_describe` — the serialized descriptor

`PortDesc` cannot cross by value: its `announce` field is an `fn(&str) -> (VTable, …)` closing over
`F` (descriptor.rs:51), and `F` does not exist on the host side of the boundary. So the `.so`
serializes a **mirror without the fn-pointer**:

```rust
struct PortDescMsg {
    frame_id: ComponentId,            // serializable
    frame_name: String,               // was &'static str
    vtable: VTable,                   // already Serialize/Schema (vtable.rs:230) — unprefixed
    max_size: usize,
    rate_hint: Option<Hz>,
    metadata: Vec<ComponentMetadata>, // unprefixed; lets the host re-derive `announce` (§7)
}
struct SystemDescriptorMsg {
    name: String, kind: SystemKind,
    inputs: Vec<PortDescMsg>, outputs: Vec<PortDescMsg>,
    params_schema: OwnedNamedType,    // <Params as postcard_schema::Schema>::SCHEMA, owned form (§6.3)
}
```

The `params_schema` is the load-bearing piece behind the one-postcard-encoding params decision (§6.3):
it lets the host **encode a system's params from KDL without linking that system's `Params` type**, via
`postcard-dyn`'s schema-guided dynamic serialization. The host stays fully schema-agnostic — it
validates frames from the serialized `VTable`s and encodes params from the serialized `Params` schema,
never linking a frame or param Rust type.

The host deserializes this, reconstructs a `PortDesc` for each port, and synthesizes the missing
`announce` closure from `metadata` (the metadata-driven prefix rewrite — §7, telemetry.md §6
fallback). The unprefixed `vtable` is exactly what `compatible()` (descriptor.rs:123) needs, so the
**existing wiring validation runs unchanged** on a dlopen'd system.

Serialization is **postcard** (the format `VTable`'s derives already target). `fsw_describe` hands the
bytes to a **host-supplied sink callback** rather than returning a pointer, so the `.so` allocates +
frees its own buffer and the host copies — no cross-allocator free (§2.5).

### 2.2 The opaque state pointer + lifecycle

`fsw_create(params)` boxes a generated runner and returns it as `*mut c_void`:

```rust
// Generated by export_system!; the .so-side twin of CyclicRunner, parameterized
// on RawBacking. Bundles are Option until fsw_bind_init fills them.
struct Runner {
    system: MySystem,
    input:  Option<<MySystem as System>::Input>,   // Input<F, RawBacking> ports
    output: Option<Out<MyOut, RawBacking>>,
    state:  SlotState,
}
```

- **`fsw_create`** decodes the params blob (§6.3 — opaque to the host), constructs `MySystem::new(params)`,
  and boxes a `Runner` with empty bundles. The `Box` is allocated **by the `.so`'s allocator** and is
  freed by `fsw_destroy` in the same `.so` (§2.5).
- **`fsw_bind_init`** reconstructs each `RingBuffer<RawBacking>` from the `FswRing` handles, walks the
  port bundle **positionally** (`descriptors()` order — the binder.rs contract), constructs every
  `Input<F, RawBacking>` / `Output<F, RawBacking>`, stores them in the `Runner`, then runs the user's
  `System::init`. (Bind + init are fused into one call because both need the bound bundles and both
  run exactly once.)
- **`fsw_execute(state, now_nanos)`** reconstructs `Timestamp` from `now_nanos`, then runs
  `CyclicRunner::step(now)` logic verbatim: lapped-input check → hard-stop, time `execute`, publish
  health (system.rs:255–273). Returns a `FswStatus` (`Running` / `Stopped { LappedInput }`) so the
  host can update its status frame (§4.2) without owning the `View`.
- **`fsw_shutdown`** runs `System::shutdown`; **`fsw_destroy`** drops the `Runner` box (running
  `MySystem::drop`, all ports' `Drop`) inside the `.so`.

The macro can literally instantiate the host's `CyclicRunner` if that type is generalized over
`Backing` (§3, Q-runner) — making the lapped/health/timing logic **pure reuse**, not a reimplementation.

### 2.3 The ring handle representation

A `repr(C)` handle the host fills from its `RingEntry`/`RingTable` (coordinator.rs:298) and the `.so`
turns into a ring:

```rust
#[repr(C)]
struct FswRing {
    base: *mut u8,       // region base — same-process: host's Backing::base()
    len:  usize,         // region length — host's Backing::len()
    role: u8,            // 0 = input (the .so registers a View), 1 = output (the .so makes the Writer)
}
```

Everything else the `.so` needs — capacity, data offset, reader-table offset, `max_readers`,
`overrun` — it reads back out of the region header via `attach_raw`/`read_header` (lib.rs:725). So the
handle is just `(base, len, role)`; geometry is **self-describing in the region**. Single-writer
discipline is preserved by `role`: for an `output` the host hands the region but creates **no** writer
itself; the `.so` is the sole writer. For an `input` the `.so` calls `view()`, claiming a reader slot
by CAS in the shared reader table (lib.rs:627) — which is why the host already sizes `max_readers` to
include every consumer (coordinator.rs:696): a dlopen'd consumer is just another reader slot.

This handle form is the **same-process** representation. The cross-process representation (§5) replaces
`base`/`len` with a region **identifier** (a path or fd the peer `attach_mmap`s); the `role` and the
"geometry is in the header" property are unchanged. (Open: Q-handle.)

### 2.4 ABI versioning & arch compatibility

Two independent guards, both already half-present:

- **`fsw_abi_version() -> u32`** — a single monotonic ABI word the host checks for equality before any
  other call. Bumped on any change to the C surface or the `*Msg` wire structs. A mismatch fails the
  load cleanly (a `LoadError`), never a crash.
- **Region arch tag** — the ring header already stamps `arch_tag()` (pointer width + endianness,
  lib.rs:87) and `attach`/`attach_raw` reject a foreign-arch region. So a `.so` built for the wrong
  target cannot silently misread a region; it fails the header handshake. The ring `VERSION`
  (lib.rs:44) covers ring-layout changes orthogonally.

The serialized descriptor adds a third, softer guard: `VTable` carries a `postcard_schema::Schema`, so
a schema digest *could* be embedded for forward-compat detection (Q-version). v1 relies on the ABI
word + the arch tag.

### 2.5 The genuinely hard parts (and how they're contained)

dlopen across a stable Rust ABI is hard (DESIGN.md:104). The containment rules:

- **No unwinding across `extern "C"`.** Every export wraps its body in `catch_unwind`; a caught panic
  is converted to a `FswStatus::Panicked` (or a non-zero return) and the host telemeters it and
  hard-stops that slot — it never lets the unwind cross the boundary (UB). Building the `cdylib` with
  `panic = "abort"` is the belt-and-suspenders option (Q-panic).
- **Allocator ownership.** Each side frees only what it allocated: the state `Box` is created and
  dropped in the `.so` (`fsw_create`/`fsw_destroy`); `fsw_describe`/`fsw_announce` hand bytes to a
  host sink and free their own buffer. No `Vec`/`Box` is ever freed across the boundary. (Recommend
  both sides use the system allocator to remove even the theoretical mismatch — Q-panic.)
- **`Drop` correctness.** Because the `Runner` (and therefore `MySystem` and every `RawBacking` port)
  is dropped by `fsw_destroy` inside the `.so`, all destructors run in the code that defined them. The
  host drops only its own `RingTable` (which owns the actual regions) — *after* it has called
  `fsw_destroy` on every dlopen'd system, so no `RawBacking` outlives its region. Teardown order is
  the existing one (coordinator.rs:1170) with `fsw_destroy` slotted before the `RingTable` drop.
- **No Rust types by value.** Only serialized bytes + `repr(C)` structs cross. This is what removes
  the `std`/`Arc`/trait-object-layout fragility that makes naive Rust-ABI dlopen unsound.

---

## 3. The `export_system!` macro (system author surface)

A system author writes an ordinary `impl CyclicSystem` (unchanged from WP4) and adds **one line** in
their `cdylib` crate:

```rust
// crate-type = ["cdylib"] in Cargo.toml
metor_fsw_2::export_system!(PlantSystem);
```

The macro generates every `extern "C"` export of §2 for `PlantSystem`, specifically:

1. **`fsw_describe`** — calls `<PlantSystem as CyclicSystem>::descriptor()` (system.rs:176), lowers it
   to a `SystemDescriptorMsg` (replacing each `PortDesc`'s `announce` fn-pointer with the port's
   unprefixed `vtable` + `metadata`), postcard-encodes it, and feeds the host sink.
2. **`fsw_create`** — decodes the params blob into `PlantSystem::Params`, calls `PlantSystem::new`,
   boxes the `Runner`.
3. **`fsw_bind_init`** — reconstructs the typed bundles from the `FswRing` array. The author's bundle
   is a `#[derive(SystemInput/SystemOutput)]` struct, so the macro reuses the **same positional walk**
   the `BindPorts` derive already emits (binder.rs:140) — except the cursor yields
   `RingBuffer<RawBacking>` (built via `attach_raw`) instead of `BoundPort`s. Concretely, this is a
   `RawBinder` analogous to `Binder` (binder.rs:73) whose `next_output`/`next_input` pop the next
   `FswRing` and `attach_raw` it. The health/log ports are bound after the user ports exactly as
   `Out::bind` does (system.rs:126) — see §7.
4. **`fsw_execute` / `fsw_shutdown` / `fsw_destroy`** — drive the boxed `Runner`.

Because `Output`/`Input` are already `Backing`-generic, the **only** new generic instantiation is over
`RawBacking`; the frame write/read code is identical. The cleanest implementation makes the host's
`CyclicRunner` (system.rs:218) generic over `Backing` so the macro instantiates
`CyclicRunner<S, O, RawBacking>` and the lapped→hard-stop / health / timing logic is reused **verbatim**
rather than re-emitted by the macro (Q-runner).

### 3.0 Landed notes & deviations (WP8 2A — `abi.rs` + `export_system!`)

The system-side ABI is implemented; real-world deviations from the sketch above, for the host half
(W2b) and the doc to track:

- **The `now` word is the raw `Timestamp` tick, not nanoseconds.** `Timestamp` is an `i64` of
  microseconds with no nanos conversion, so `fsw_execute(state, now: u64)` carries `Timestamp.0 as u64`
  and the `.so` reconstructs `Timestamp(now as i64)`. The host `DlSlot` (§4.2) must pass `now.0 as u64`
  — a reversible raw-tick contract, unit-agnostic.
- **One `export_system!` per cdylib (fixed `fsw_*` symbols).** v1 is one system per `.so` — the finest
  rebuild granularity, and the host loader resolves the fixed symbol names. Multiple systems per `.so`
  (namespaced `fsw_<name>_*` symbols) is a later option; revisit when the example (§8) is split (it
  becomes one cdylib per system, or we add namespacing).
- **`abi`/`dl` no longer ride the `kdl` feature (FIXED in Wave 3a).** `run_create` used to lean on
  WP6's `RegisteredSystem` for `Params`+`new`, whose `Params: FromKdlNode` bound lived in the
  kdl-gated `wiring` module, so `abi`/`dl` were `#[cfg(feature = "kdl")]`. Wave 3a introduced the
  kdl-independent construction trait `BuildSystem { type Params; fn new }` (in `system.rs`); the
  `abi` `run_*` helpers + `export_system!` now bound on `BuildSystem` (plus `Deserialize`/`Schema`
  where needed), and `RegisteredSystem: BuildSystem` is a **blanket marker** that only adds the
  `Params: FromKdlNode` bound for the static-registry path. `pub mod abi` / `pub mod dl` /
  `Reg::Dl` / `add_dl_cyclic` are ungated; `libloading`/`thiserror` are now non-optional deps. Both
  `--no-default-features` and `--features kdl` build, and a dl system (the fixture) needs **no**
  `FromKdlNode` impl at all (§6.3). The `Wiring` data model / builder / resolver / build driver
  stay in the kdl-gated `wiring` module (they share the `Registry`/`LoadError` surface).
- **`into_port_desc`'s `announce` re-prefixes metadata but NOT the vtable's baked component ids**
  (a `// TODO telemetry.md §6` in `abi.rs`). The host-side metadata-driven vtable-id rewrite (§7,
  Q-announce) lands in W2b so telemetry prefixing over a dlopen'd system is correct end-to-end.
- `#[unsafe(no_mangle)]` (edition 2024 spelling); `RingBuffer::region()` promoted to `pub` so the host
  can fill `FswRing { base, len }`; `metor-fsw-2` gained direct `postcard` / `postcard-schema`
  (`use-std`, for the owned-schema module) / `serde` deps.

### 3.1 Async systems

`AsyncSystem` owns a `run` loop driven by `stellarator` and woken by ring `Notifier`s
(system.rs:190, coordinator.rs:358–376). Two things make it **out of scope for v1 dlopen**:

- **Cross-boundary wake.** A `Notifier` is `Arc`-backed and process-local (binder.rs:14–20); waking an
  awaiting reader requires the writer and view to share the *same* `Arc` clone. That clone cannot cross
  the C ABI, and the shared-memory wake word that would replace it (`OFF_WAKE_WORD`, lib.rs:61;
  `FLAG_WAKE_SHARED`, lib.rs:73) is explicitly reserved-but-unused. Cyclic dlopen sidesteps this
  entirely because **cyclic edges are polled, never woken** (binder.rs:14–18) — so v1 dl systems use
  `NoWake` on every port and need no notifier across the boundary.
- **Runtime ownership.** An async system spawns onto `stellarator`; a `.so` running its own executor,
  or sharing the host's, is a second hard problem (TLS, the global reactor) orthogonal to the wake
  word.

v1 ships cyclic-only. The async path (a spawned task *inside* the `.so`, woken by the shm wake word) is
ordered future work (§9), and the ABI already reserves the seam (`fsw_execute` would become a no-op for
async slots; an `fsw_run`/wake-word handshake replaces it).

---

## 4. The host side — `DlSystem` loader + `DlSlot`

### 4.1 Loading & validation

A `DlSystem` loader (over `libloading::Library`, or a thin raw `dlopen` wrapper — Q-loader):

1. `dlopen`s the `.so`, resolves the `fsw_*` symbols, checks `fsw_abi_version()`.
2. Calls `fsw_describe` and deserializes a `SystemDescriptor` (reconstructing each `PortDesc`,
   synthesizing `announce` from metadata — §7).
3. Hands that descriptor to the **existing** builder path so `compatible()` (descriptor.rs:123) and
   every `WireError` check (coordinator.rs:607–675) run identically to a static system. **A dlopen'd
   system is wiring-validated exactly like a linked one** — this is the payoff of serializing the
   descriptor rather than trusting the `.so`.

### 4.2 The `DlSlot : CyclicSlot`

`DlSlot` is the dlopen twin of `CyclicRunner`'s `CyclicSlot` impl (system.rs:294). It holds the loaded
library + function pointers, the opaque `*mut state`, and the **`FswRing` arrays** (built by the host
at `build()` from its already-computed `cons_edge` / `output_rings`, coordinator.rs:786–817). It
implements the trait the coordinator already drives:

```rust
impl CyclicSlot for DlSlot {
    fn init(&mut self)              { (self.lib.fsw_bind_init)(self.state, &self.inputs, …, &self.outputs, …); }
    fn step(&mut self, now: Timestamp) {
        let status = (self.lib.fsw_execute)(self.state, now.as_nanos());
        self.state_for_status(status);   // map Stopped/Panicked → SlotState (status frame, coordinator.rs:1098)
    }
    fn shutdown(&mut self)          { (self.lib.fsw_shutdown)(self.state); }
    fn name(&self) -> &str          { &self.name }        // from the descriptor
    fn state(&self) -> &SlotState   { &self.state_field }
}
```

The host's per-cycle loop (`for slot in &mut self.cyclic { slot.step(now) }`, coordinator.rs:1007) is
**unchanged** — a `DlSlot` is just another `Box<dyn CyclicSlot>` in the same `Vec`. The lapped check now
happens *inside* the `.so` (it owns the `View`), and is reported back via `FswStatus`; the host's
`update_status` / status-frame machinery (coordinator.rs:1098) consumes it the same way it consumes a
`CyclicRunner`'s `SlotState`.

### 4.3 Composing with the existing `Registry` / `CoordinatorBuilder`

WP8 **adds a factory kind, it does not replace the registry.** WP6's `Registry` maps `type="Foo"` to a
`fn`-pointer `SystemFactory` resolved at compile time (wiring.rs:320, 391). WP8 adds a second factory
flavor — a `DlFactory` keyed by the same `type=` string — that, instead of constructing `S` and calling
`add_cyclic_named`, `dlopen`s an artifact and calls a new `CoordinatorBuilder::add_dl_cyclic(name,
descriptor, dl_handle)`. That builder method registers the system the same way `add_cyclic_named` does
(coordinator.rs:501) — it pushes the (deserialized) `SystemDescriptor`, the `SystemKind`, the instance
name, and a `Reg::Dl(..)` registration whose `bind` produces a `DlSlot` instead of a `CyclicRunner`. The
edge/connect/build/sizing/telemetry passes are **all reuse**. KDL (wiring.rs:452) routes a `type=` to a
static or a dl factory based on whether the document binds it to an artifact (§6).

---

## 5. In-process vs separate-process — scope split

**Recommendation: v1 is same-process dlopen only.** Justification:

- It delivers the entire motivating value (rebuild/reupload one `cdylib`, not the monolith) with the
  *smallest* new surface: no IPC, no process supervision, no cross-process wake, no fd passing.
- Same-process can reconstruct rings **directly over the host's existing `BoxBacking` regions** via
  `RawBacking::attach_raw` — zero changes to how the host allocates or sizes rings (coordinator.rs:693).
  The handle is a bare `(base, len)`.
- It exercises and hardens every boundary (`describe`, the lifecycle, panic containment, `Drop`
  ownership, telemetry visibility) that a multi-process design *also* needs — so it is strict
  groundwork, not a throwaway.

The **separate-process** future is left with its seams open:

- The data path is identical — the same ports, the same `RawBacking` reconstruction logic — but the
  region is an **`MmapBacking`** the peer `attach_mmap`s (lib.rs:569), and the `FswRing` handle carries
  a region identifier (path/fd) instead of a raw pointer (Q-handle). The brief's "shared-memory backed
  so the same mechanism works in-process or cross-process" (DESIGN.md:70) is realized by **swapping the
  backing, nothing else.**
- Cross-process **wake/notification** is the genuinely missing piece, and the ring already reserves it:
  `OFF_WAKE_WORD` + `FLAG_WAKE_SHARED` (lib.rs:61, 73). It is needed for async cross-process and for a
  process-version "execute trigger" (DESIGN.md:101–104 names this TBD). Deferred.
- **Process supervision** (spawn, health-driven restart, crash-slot reclamation — which v1 explicitly
  lacks, coordinator.rs:46) is its own work package.

Whether v1 should reconstruct over the host's `BoxBacking` rings or **always go through `MmapBacking`**
(uniform with the future, but adds a temp-file/shm-fd per ring even in-process) is a real fork —
Q-backing. Recommendation: `RawBacking` over `BoxBacking` for v1 (no per-ring file), since `RawBacking`
and `MmapBacking` share the identical attach/reconstruct path, so moving to mmap later is a backing
swap, not a redesign.

---

## 6. Build system & the data/serialization split

The user's hard constraint: **separate the data format from the serialization format.** Today
`wiring.rs` parses KDL *directly* into `CoordinatorBuilder` calls (wiring.rs:452) — there is no
standalone data model; KDL *is* the model. WP8 inserts the missing middle layer.

### 6.1 The `Wiring` data model (the single source of truth)

A plain Rust data model that is the authoritative description of a mission, independent of any text
format:

```rust
pub struct Wiring {
    pub coordinator: CoordinatorSpec,            // cycle_rate, default_depth, clock (== CoordinatorConfig)
    pub artifacts:   Vec<Artifact>,              // the cdylibs this mission loads
    pub systems:     Vec<SystemSpec>,
    pub edges:       Vec<EdgeSpec>,              // from/out/to/in/delayed (== the wiring.rs Edge)
    pub telemetry:   Option<TelemetrySpec>,
}
pub struct Artifact {                            // "which shared object + what crate it comes from"
    pub id:        ArtifactId,                    // referenced by SystemSpec
    pub crate_name: String,                       // cargo package, for the build driver
    pub cdylib:    String,                         // libfoo.so / libfoo.dylib / libfoo.dll
    pub system_type: String,                       // the ONE `type=` this .so's export_system! provides
    pub path:      Option<PathBuf>,               // resolved artifact location (build output)
}
// Reviewed: ONE system per cdylib (fixed fsw_* symbols, §3.0). An Artifact therefore exports a single
// system type; multiple `system` nodes may still reference one Artifact to instance that type more than
// once (the loader dlopens the .so once and `fsw_create`s per instance). Finest rebuild granularity:
// editing one control law rebuilds only its cdylib.
pub struct SystemSpec {
    pub name:     String,                          // instance name (the telemetry prefix, wiring.md §6)
    pub ty:       String,                          // the `type=` key
    pub artifact: Option<ArtifactId>,             // Some => dlopen this; None => resolve in the static Registry
    pub params:   ParamSource,                     // an opaque-to-host params payload (§6.3)
}
```

`CoordinatorSpec`/`EdgeSpec`/`TelemetrySpec` are the already-existing concepts (`CoordinatorConfig`,
the `connect` edge, `TelemetryConfig`) lifted into serializable data. `Wiring` is what both front-ends
produce and what one resolver consumes.

### 6.2 The Rust builder API (equivalent to KDL)

A `WiringBuilder` constructs a `Wiring` in code, so **anything KDL can express, Rust can express**:

```rust
let wiring = WiringBuilder::new()
    .coordinator(CoordinatorSpec { cycle_rate: 120.0, clock: Simulated { dt: DT }, .. })
    .artifact("adcs", crate_name = "adcs-systems", cdylib = "libadcs_systems.so",
              exports = ["Plant", "Nav", "Ctrl"])
    .system("plant").ty("Plant").from_artifact("adcs").param("init_angle", 0.5)./*…*/.end()
    .system("nav").ty("Nav").from_artifact("adcs").param("meas_sigma", 0.02).end()
    .connect("plant", "sensors", "nav", "sensors")
    .connect_delayed("ctrl", "torque_cmd", "plant", "torque_cmd")
    .telemetry(Tcp("127.0.0.1:2240"), All)
    .build();                                       // -> Wiring
```

A system with `artifact = None` resolves through the **static** `Registry` (a statically-linked
system); a system `from_artifact(..)` is dlopen'd. The two are interchangeable in one document — a
mission can statically link its stable systems and dlopen only the ones it iterates on.

### 6.3 KDL as *one* deserializer onto `Wiring`

The KDL front-end becomes `impl TryFrom<&KdlDocument> for Wiring` (or `Deserialize`), strictly
equivalent to the builder. `wiring.rs::load()` is refactored to two stages:

```text
KDL text ──parse──▶ Wiring (data model) ──resolve──▶ CoordinatorBuilder ──build──▶ Coordinator
              ▲                                  ▲
        one deserializer                  one shared resolver
        (Rust builder is the other)       (static Registry + dlopen)
```

New KDL surface — an `artifact` node + a per-system `lib=` reference:

```kdl
artifact "adcs" crate="adcs-systems" lib="libadcs_systems.so" exports="Plant Nav Ctrl"

system "plant" type="Plant" lib="adcs" init_angle=0.5 init_rate=0.15 meas_sigma=0.002 seed=42
system "nav"   type="Nav"   lib="adcs" meas_sigma=0.02
system "ctrl"  type="Ctrl"  lib="adcs" q_weight=5.0 r_weight=8.0

connect "plant" -> "nav"  frame="sensors"
connect "ctrl"  -> "plant" frame="torque_cmd" delayed=#true
telemetry { transport "tcp" addr="127.0.0.1:2240"; mode "all" }
```

A `system` with no `lib=` resolves statically (the WP6 path, unchanged). Per-system params
(`init_angle=…`) must reach the `.so`, which owns the `Params` type. **Locked decision (Q-params): one
canonical encoding** — postcard `Params` bytes cross `fsw_create` from *both* front-ends, so KDL and
the Rust builder are provably identical on the wire. Postcard's reflection makes this work **without
the host ever linking the concrete `Params` type** (`postcard-schema` is already a proto dependency,
vtable.rs:230; `postcard-dyn` is in-tree):

- The `.so`'s `fsw_describe` exports its `Params` **schema** as an `OwnedNamedType`
  (`<Params as postcard_schema::Schema>::SCHEMA`), carried on `SystemDescriptorMsg` (§2.1).
- **KDL front-end:** walk the `system` node's properties into a dynamic value and encode it *guided by
  that schema* via `postcard-dyn` → canonical postcard `Params` bytes. No `FromKdlNode` call host-side,
  no `Params` type linked into the host.
- **Rust-builder front-end:** the app *has* the `Params` type (from the shared contract crate), so a
  typed value postcard-encodes directly to the **same** bytes — the two front-ends are byte-equivalent.
- **`.so` side:** `fsw_create` postcard-decodes the bytes into the real `Params` (its `Deserialize`).

So the host is **fully schema-agnostic**: it validates frames from serialized `VTable`s and encodes
params from the serialized `Params` schema, never linking a system's frame or param Rust types. The
shared frame/param *contract* crate is shared **among the system cdylibs** (a producer and consumer must
agree on a frame's layout), **not** linked into the host — the cleanest realization of the
data/serialization split, reusing the same postcard reflection that already underpins dynamic frames
(WP2/WP2b). (The `FromKdlNode` derive stays the static-Registry path's tool; the dl path uses the
schema-guided encoder instead, so a dl system needs no `FromKdlNode` impl at all.)

Whether the `type → artifact` mapping lives **in the wiring document** (the `artifact` node above) or in
a **sibling manifest** (a separate `artifacts.kdl`/`Cargo`-metadata file the build driver and runtime
both read) is Q-manifest. Recommendation: in-document `artifact` node for v1 (one file, one source of
truth, matches the builder's `.artifact(..)`), with the sibling-manifest form as a later option for
sharing one artifact set across many mission documents.

### 6.4 The build driver

A small driver reads a `Wiring` (or just its `artifacts`), and for each `Artifact` runs
`cargo build -p <crate_name>` (release/target as configured), collecting the produced `cdylib` into
`Artifact.path`. Because each system `cdylib` is its own cargo package, **`cargo` already does the
incremental work** — only changed crates recompile. The driver's added value is (a) knowing *which*
crates a mission needs from the `artifacts` list (the brief's "understand which shared objects are
required and what crates they come from"), and (b) a content hash per `.so` so a deploy step
re-uploads only the artifacts whose bytes changed. The runtime resolver (§4.3) then maps each `type=`
to its `Artifact.path` and `dlopen`s it.

### 6.5 Reused vs new in the build layer

The data model + builder + the KDL-as-deserializer refactor is the **bulk of WP8's non-ABI work**, but
it is mostly *relocation*: the `Edge`, the coordinator config, the telemetry spec, and the param walk
all already exist in `wiring.rs` — they move behind `Wiring`. Genuinely new: the `Artifact` concept,
the `lib=`/`artifact` KDL surface, the `WiringBuilder`, the dl resolver branch, and the build driver.

---

## 7. Implicit health/log ports & the telemetry `All` tap across the boundary

Two WP4/WP7 invariants must survive the boundary:

- **Every system has implicit health + log output ports** (system.rs:108–114; `Out::bind` appends them
  after the user ports, system.rs:126). Because those ports are *part of the system's output bundle*,
  they are **already inside the `.so`'s descriptor and its bind walk** — the macro's `RawBinder` binds
  them after the user outputs exactly as `Out::bind` does, and the host allocates their rings exactly as
  it does today (the descriptor enumerates them, coordinator.rs:695). The health frames a dlopen'd
  system publishes flow over host-owned rings like any other output. **No special handling** — the
  health/log ports are just two more `FswRing` outputs in the array. The `.so` reuses
  `HealthPort`/`CyclicRunner::step`'s `end_cycle` (system.rs:272) to populate the standard counters.

- **The telemetry `All` tap must enumerate dlopen'd outputs.** WP7's tap works off the host's
  `OutputRegistry`/`RingTable` (registry.rs; coordinator.rs:704), which is populated from each system's
  **descriptor** at `build()` — and a dlopen'd system *has* a descriptor (§4.1). So its output buffers
  land in the registry like any other (`registry_entry`, coordinator.rs:908), and the `All` tap
  enumerates them with no change. The one wrinkle is the **prefixed announce** each registry entry
  needs (`(port.announce)(instance)`, coordinator.rs:909): the dl `PortDesc` has no fn-pointer
  `announce` closing over `F`. The host instead synthesizes it from the `metadata` the descriptor
  carried (§2.1) via the **metadata-driven prefix rewrite** telemetry.md §6 already specifies as the
  no-static-`F` fallback — which is *exactly* the dlopen situation. Alternatively the `.so` exports
  `fsw_announce` (§2) to re-derive the prefixed vtable on demand (keeping the derivation where `F`
  lives). Q-announce picks between them; recommendation is the metadata rewrite (no extra ABI call, and
  the host already needs that path).

---

## 8. Refactoring `adcs-fsw2` to dl-open (the integration milestone)

The final milestone (not implemented now) re-expresses the existing three-system ADCS mission
(`examples/adcs-fsw2/src/lib.rs`) with dlopen'd systems, proving the whole stack end-to-end:

1. Split `Plant`/`Nav`/`Ctrl` into a `cdylib` crate (`adcs-systems`) — the `impl CyclicSystem`s are
   **unchanged**; each gets a one-line `export_system!`. The frame definitions (`Sensors`,
   `AttitudeEstimate`, `TorqueCmd`, `Truth`) live in a shared *library* crate both the `cdylib` and the
   host depend on (frames are compile-time contracts; their `VTable`s must match on both sides — the
   `frame_id`/`compatible` check enforces it, descriptor.rs:123).
2. The mission binary builds a `Wiring` (via the builder *or* the KDL of §6.3, which is the existing
   `KDL` const, lib.rs:509, plus an `artifact` node and `lib=` refs), runs the build driver to produce
   `libadcs_systems.so`, and resolves it through the dl loader.
3. The closed loop runs identically — same `connect_delayed` feedback edge, same `Simulated`/`Wall`
   clock, same telemetry `All` downlink to metor-panel (lib.rs:470–499). The convergence test
   (`tests/closed_loop.rs`) is the acceptance gate: **the dlopen'd mission must converge bit-for-bit
   like the static one.**

The payoff is demonstrable: edit `YangLQR` gains, `cargo build -p adcs-systems` (a small crate), and
re-run — without recompiling the mission host or the telemetry stack.

---

## 9. v1 scope & ordered future work

**v1 (the smallest thing that proves the value):**
- One **cyclic** system in a same-process `cdylib`, `dlopen`'d by the host.
- `RawBacking::attach_raw` over the host's existing `BoxBacking` rings; `NoWake` everywhere.
- `export_system!` generating the §2 C-ABI; `CyclicRunner` generalized over `Backing`.
- `DlSlot : CyclicSlot` + `add_dl_cyclic` on the builder; wiring-validated via the serialized
  descriptor through the **existing** `compatible()`/`WireError` passes.
- The `Wiring` data model + `WiringBuilder` + KDL-as-deserializer refactor + a minimal build driver.
- Implicit health/log + telemetry `All` working over the boundary (metadata-rewrite announce).

**Future work, ordered:**
1. **Async dlopen** — a spawned task inside the `.so`, woken by the shm wake word (`OFF_WAKE_WORD`,
   `FLAG_WAKE_SHARED`); cross-boundary `Notifier` replacement.
2. **Separate-process systems** — `MmapBacking`/`attach_mmap` rings, region identifiers in `FswRing`,
   process spawn + supervision + health-driven restart + crash-slot reclamation.
3. **Cross-process wake/notification** — the shm wake handshake the ring reserves.
4. **Hot reload** — swap a `.so` at runtime (drain → `fsw_destroy` → reload → `fsw_create`/rebind).
5. **Build/deploy polish** — content-hash incremental upload, signed/verified artifacts, a sibling
   artifact manifest, params hot-config.
6. **ABI hardening** — embedded schema digest (Q-version), a conformance test that a `.so` and host
   agree on every frame `VTable`.

---

## 10. Reused primitives

| Concern | Reused (landed) | New in WP8 |
|---|---|---|
| Transport | `RingBuffer`, position-independent region (lib.rs:14–18), `read_header`/`attach_mmap` (lib.rs:725, 569), `Backing` trait (lib.rs:181) | `RawBacking` + `attach_raw` (one non-owning backing) |
| Typed ports | `Output<F, B>`/`Input<F, B>` — **already `Backing`-generic** (port.rs:48, 128) | `RawBacking` instantiation only; write/read code unchanged |
| Per-system driver | `CyclicRunner` lapped→stop/health/timing (system.rs:255–273), `CyclicSlot` (coordinator.rs:240) | generalize `CyclicRunner` over `Backing`; `DlSlot : CyclicSlot` |
| Descriptors | `PortDesc`/`SystemDescriptor` (descriptor.rs), `compatible()` (descriptor.rs:123) | `*Msg` serializable mirrors (drop the `announce` fn-pointer); postcard via `VTable`'s own derives (vtable.rs:230) |
| Port binding | positional `Binder`/`BindPorts` contract (binder.rs) | `RawBinder` over the `FswRing` array |
| Host wiring | `CoordinatorBuilder`, `connect`/`build`/sizing/validation (coordinator.rs:607) | `add_dl_cyclic`; a `Reg::Dl` registration |
| KDL front-end | `wiring.rs::load`, `Registry`, `FromKdlNode` (wiring.rs) | the `Wiring` data model + `WiringBuilder`; KDL becomes a deserializer onto it; `artifact`/`lib=` surface |
| Telemetry | `OutputRegistry` tap, `All` enumeration, prefixed `announce` + metadata-rewrite fallback (registry.rs; telemetry.md §6) | metadata-rewrite (or `fsw_announce`) for dl outputs |
| Health/log | implicit health/log ports (system.rs:108–114), `HealthPort` | none — they ride the bundle across the boundary |
| Versioning | ring `arch_tag`/`VERSION` (lib.rs:44, 87) | `fsw_abi_version()` word; `catch_unwind` boundaries |
| Cross-proc seam | reserved `OFF_WAKE_WORD`/`FLAG_WAKE_SHARED` (lib.rs:61, 73) | nothing in v1 — left reserved |

The genuinely new code is concentrated in: `RawBacking`/`attach_raw`, the `export_system!` macro + the
C-ABI exports, `DlSlot` + the loader, the `Wiring` data model + builder + KDL-deserializer refactor, and
the build driver. The data path, validation, sizing, telemetry tap, and health are all reuse.

---

## 11. Open questions / risks for the reviewer

1. **Q-scope — same-process-only v1? [RESOLVED: yes, same-process only.]** v1 is in-process dlopen over
   the host's `BoxBacking` rings via `RawBacking` (so Q-backing = `RawBacking`-over-`BoxBacking` and
   Q-handle = bare `{base, len, role}` are resolved with it); multi-process supervision and cross-process
   wake are future work (§5, §9) with the ring seams reserved.

2. **Q-handle — ring handle representation.** Recommended: `FswRing { base, len, role }` for v1
   (same-process raw pointer; geometry self-described in the region header). The cross-process form
   swaps `(base, len)` for a region identifier (path/fd) that the peer `attach_mmap`s. (Options:
   bare pointer+len now / identifier-based now / a tagged union covering both. Recommend pointer+len,
   evolve to a tagged handle when §5.2 lands.)

3. **Q-backing — reuse host rings vs always-mmap.** Recommended: `RawBacking` over the host's existing
   `BoxBacking` rings (no per-ring file in-process). Alternative: route *every* ring through
   `MmapBacking` even in-process, for uniformity with the future cross-process path, at the cost of a
   shm file/fd per ring. (Recommend `RawBacking`; the attach/reconstruct path is identical to mmap, so
   the later switch is a backing swap, not a redesign.)

4. **Q-async — async dlopen scope.** Recommended: out of scope for v1 (§3.1) — cyclic dl systems poll
   (`NoWake`), so they need no cross-boundary `Notifier`; async needs the reserved shm wake word and a
   `.so`-side executor. Confirm v1 is cyclic-only and async is future work. (Options: cyclic-only now /
   async via a `.so`-spawned task now / coordinator-owns-the-async-loop. Recommend cyclic-only.)

5. **Q-describe — descriptor serialization.** Recommended: postcard-encode a `SystemDescriptorMsg`
   (the `PortDesc` mirror without the `announce` fn-pointer, carrying the unprefixed `VTable` —
   already `Serialize`/`Schema`, vtable.rs:230 — plus `metadata`), handed to a host sink callback.
   (Options: postcard via the existing `VTable` derives / a bespoke C-struct descriptor encoding /
   the `VTableMsg` wkt wire form. Recommend reusing `VTable`'s own serialization — the WP brief's
   symmetry.)

6. **Q-announce — prefixed telemetry schema across the boundary.** Recommended: the host synthesizes
   each dl output's prefixed announce vtable from the `metadata` in the descriptor (telemetry.md §6's
   no-static-`F` metadata-rewrite — which is exactly the dlopen case), so no extra ABI call. Alternative:
   the `.so` exports `fsw_announce(out_idx, instance)` to re-derive it where `F` lives. (Recommend
   metadata-rewrite; the host already needs that path. This also forces a decision on broadening
   `PortDesc.announce` from a bare `fn` to a boxed closure so a dl entry can capture its metadata —
   confirm that change is acceptable.)

7. **Q-runner — make the `System` stack `Backing`-generic? [RESOLVED: yes — full backing-generic
   refactor.]** The reviewed decision (over host-mediated copy-in) is to give a dlopen'd system real
   zero-copy `RawBacking` views, which requires `System<B>`/`CyclicSystem<B>` (defaulted to
   `BoxBacking`), generic bundles (author-visible `<B = BoxBacking>`), a `RingSource`-abstracted
   `BindPorts<B>` with `Binder`(BoxBacking)/`RawBinder`(RawBacking) impls, generic port `bind`, and
   `CyclicRunner<S, O, B>` — see §1.2. Static call sites stay source-compatible via the `B = BoxBacking`
   default; the lapped/health/timing logic is reused verbatim. Cost: every bundle struct (incl. the
   example's) gains `<B>`, and the derives emit the `BindPorts<B>` impl.

8. **Q-manifest — `type → artifact` in-document vs sibling file. [RESOLVED: in-document.]** An
   in-document `artifact` node + per-system `lib=` ref (one source of truth, mirrors the builder's
   `.artifact(..)`; static and dlopen'd systems mix in one document). A sibling manifest stays a later
   option for sharing one artifact set across many mission documents.

9. **Q-params — how params cross the boundary. [RESOLVED: one postcard encoding via schema
   reflection.]** Both front-ends produce **canonical postcard `Params` bytes** crossing `fsw_create`.
   The `.so` exports its `Params` schema (`OwnedNamedType`) on `SystemDescriptorMsg`; the KDL
   front-end encodes from KDL *guided by that schema* via `postcard-dyn` (no host-linked `Params`
   type, no `FromKdlNode` host-side), and the Rust builder encodes a typed `Params` to the same bytes.
   The host stays fully schema-agnostic (§6.3). `Params` gains a `Serialize`/`Deserialize`/`Schema`
   derive triple; the shared frame/param contract crate is shared among cdylibs, not linked into the
   host.

10. **Q-version — ABI compatibility policy.** Recommended: a single `fsw_abi_version() -> u32` word the
    host checks for equality, layered on the ring's existing `arch_tag`/`VERSION` region guards
    (lib.rs:44, 87). Should we *also* embed a `postcard_schema` digest of the descriptor/frame schemas
    for forward-compat detection now, or defer to the future "ABI hardening" item (§9)? (Recommend ABI
    word + arch tag for v1; schema digest deferred.)

11. **Q-panic — unwind & allocator policy.** Recommended: wrap every `extern "C"` export in
    `catch_unwind` (a caught panic → `FswStatus::Panicked` → host telemeters + hard-stops the slot,
    never an unwind across the boundary), and free across the boundary nowhere (state `Box` created and
    dropped in the `.so`; `describe`/`announce` via host sink callbacks). Additionally build the
    `cdylib` with `panic = "abort"` and/or pin both sides to the system allocator? (Recommend
    `catch_unwind` + no-cross-free as mandatory; `panic = "abort"` + system allocator as recommended
    belt-and-suspenders — confirm whether they are required.)

12. **Q-loader — `libloading` vs raw `dlopen`.** Recommended: `libloading` for the symbol resolution +
    lifetime management (the loaded `Library` must outlive every `DlSlot` and the `*mut state`).
    Alternative: a thin raw `dlopen`/`dlsym` wrapper to avoid the dependency on targets where
    `libloading` is heavy. (Recommend `libloading`; revisit per target.)

13. **Q-status — how a dl slot reports a hard stop.** Recommended: `fsw_execute` returns a `FswStatus`
    (`Running`/`Stopped{LappedInput}`/`Panicked`) that the host maps to `SlotState` and folds into the
    existing status frame (coordinator.rs:1098), since the lap check lives inside the `.so` (it owns the
    `View`). Alternative: the host *also* holds a `View` on each dl input and does the lap check itself
    (more host-side machinery, breaks the "the `.so` owns its typed inputs" symmetry). (Recommend the
    status return code.)
