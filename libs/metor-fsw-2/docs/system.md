# Work-Package 4 — The `System` trait

Status: **design only, pre-implementation**. Reviewer sign-off required before any code
lands. No Rust in this WP — this document specifies the `System` trait surface and how it
sits on top of the **already-landed** ring (WP1), vtable (WP2) and frame (WP3) primitives.
The coordinator that drives systems is WP5; the wiring/config language is WP6. This document
defines only the **system-side contract**: what a system *is*, and what a coordinator needs to
read from it and hand to it.

Relevant existing code (read before implementing):
- `libs/metor-fsw-2/ring/src/lib.rs` — `RingBuffer<B>`, `Writer<B,WD,WS>` (`try_write`/async
  `write`), `View<B,RD,RS>` (`is_lapped`, `try_read_into`/async `read_into`, `try_read` →
  `ReadGrant`, `resync`, `cursor`/`committed`), `Overrun::{Overwrite,Lossless}`,
  `Config { capacity, max_readers, overrun }`, `WriteError`, `ReadError::Lapped`,
  `WakeSource`/`WakeSink`/`NoWake`/`Notifier`, `BoxBacking`/`MmapBacking`/`Backing`,
  `round_up8` (and the **private** `frame_len`).
- `libs/metor-fsw-2/src/frame.rs` — `Frame: AsVTable + Metadatatize + Componentize +
  Decomponentize`, with `NAME`, `FRAME_ID`, `timestamp()`.
- `libs/metor-fsw-2/src/{writer.rs,reader.rs,dynamic.rs}` — `FrameWriter<F>`
  (`new`/`list`/`map`/`table`/`finish`), `ListWriter`/`MapWriter`, `ListReader`/`MapReader`,
  `FrameList`/`FrameMap`/`Slot`.
- `libs/metor-proto/src/vtable.rs` — `VTable` (`as_vtable`, `apply`, `realize_fields`,
  `for_each_field`), `RealizedField`, `Op`.
- `libs/metor-proto/src/com_de.rs` — `Componentize` (`sink_columns`, `MAX_SIZE`),
  `Decomponentize` (`apply_value`).
- `libs/metor-fsw/src/{vtable.rs,metadata.rs}` — `AsVTable` (`vtable_fields`/`as_vtable`),
  `Metadatatize`.

---

## 0. Design summary (orientation)

A **system** owns some private state and a set of **output ports** (single-writer ring
buffers it produces frames into) and reads a set of **input ports** (read-only views into
other systems' output buffers). The coordinator owns the ring regions, builds the per-system
port handles at wiring time, validates that producer outputs satisfy consumer inputs (using
the frames' VTables), then drives the system: cyclic systems are `execute`d once per cycle,
async systems run their own loop. Systems never return errors; they emit health as ordinary
frames over a framework-provided health output port.

The genuinely new surface in WP4 is small: the `System`/`SystemInput`/`SystemOutput` traits,
the typed `Output<F>`/`Input<F>` port wrappers around the landed `Writer`/`View`, a
`SystemDescriptor` self-description struct built from `Frame`/`AsVTable`, and a standard
health frame. Everything below the port wrappers is reuse.

---

## 1. The `System` trait

### 1.1 The trait

```rust
/// What kind of driving a system needs from the coordinator. The *only* structural
/// distinction between systems (DESIGN.md "Cross System Communication").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SystemKind {
    /// Invoked once per coordinator cycle. Inputs are views straight into upstream
    /// output buffers; a lapped input is a hard error (§3.1).
    Cyclic,
    /// Runs at its own rate / on its own events. Inputs are views into private buffers
    /// the coordinator copies upstream outputs into (§3.2).
    Async,
}

pub trait System {
    /// The read-only inputs this system consumes (a bundle of typed input ports).
    type Input: SystemInput;
    /// The owned outputs this system produces (a bundle of typed output ports).
    type Output: SystemOutput;

    /// How the coordinator drives this system.
    const KIND: SystemKind;
    /// Human/wiring name; the prefix the system's health frame hangs off (§4).
    const NAME: &'static str;

    /// Runs once before the first `execute`. May emit initial frames / health.
    fn init(&mut self, output: &mut Self::Output);

    /// One unit of work: read the latest inputs, write outputs. Reports any trouble
    /// through `output` (health), never through a return value (§4).
    fn execute(&mut self, input: &Self::Input, output: &mut Self::Output);

    /// Runs once when the system is torn down. May flush final frames / health.
    fn shutdown(&mut self, output: &mut Self::Output);
}
```

This is the DESIGN.md sketch made concrete, with three resolved deviations, each justified:

1. **`Input`/`Output` are *port-handle bundles*, not raw frame data.** `Output` is the set of
   `Writer`-backed ports the system owns; `Input` is the set of `View`-backed ports it
   borrows. Passing frame *values* in/out would force a representation that crosses the
   process boundary as Rust types — which DESIGN.md explicitly rejects ("dl-open across a
   stable ABI is genuinely hard … the data path is built on shared memory"). Handles over
   rings keep the data path as table bytes.
2. **`execute` takes `&Self::Input` and `&mut Self::Output` (by reference, not by value).**
   The output ports wrap ring `Writer`s, which need `&mut self` to `try_write` and must
   **persist across cycles** (a `Writer` holds the single-writer role for a buffer; recreating
   it each cycle is wrong). The inputs are borrowed read-only, matching DESIGN.md "systems own
   their outputs and borrow their inputs". The coordinator owns the bundles between cycles.
3. **`init`/`shutdown` also receive `&mut Self::Output`.** A driver needs to publish its first
   frame in `init` (e.g. a default mode) and flush a final health record in `shutdown`. They
   take no input — there is nothing meaningful to read before/after the run.

### 1.2 Configuration / parameters vs streamed inputs

These are deliberately different mechanisms:

- **Parameters** — gains, limits, modes, buffer depths, calibration. Fixed (or
  reconfigured rarely) and *not* a frame stream. They live in the system struct's own fields,
  set at construction, before `init`:
  ```rust
  struct NavFilter { q: f64, r: f64, mode: NavMode, /* state… */ }
  impl NavFilter { fn new(params: NavParams) -> Self { … } }
  ```
  The coordinator/wiring (WP6) constructs the system from a config blob; WP4 only requires
  that a system is *constructible before `init`* and carries its own params. (Live parameter
  updates, if ever needed, would arrive as an ordinary input frame — but that is a future
  extension, not part of the trait.)
- **Streamed inputs** — IMU samples, nav estimates, commands. These flow as frames over rings
  and are read through `Self::Input` ports each `execute`.

The split keeps the hot path (`execute`) about frame streams only, and keeps params out of the
ring transport.

---

## 2. Outputs (owned buffers) & Inputs (views)

### 2.1 Output ports — `SystemOutput`

A system's `Output` associated type is a struct of one or more **output ports**, one per
output frame type. The port wraps the single ring `Writer` the system owns for that frame:

```rust
/// One owned output: the single writer into a ring buffer carrying frame `F`.
pub struct Output<F: Frame, B: Backing, WD: WakeSource, WS: WakeSink> {
    writer: Writer<B, WD, WS>,
    scratch: FrameWriter<F>-or-LenPacket, // reused build buffer (no per-write alloc)
}

impl<F: Frame + IntoBytes + Immutable, …> Output<F, …> {
    /// Publish a *fixed* frame (no dynamic members). Serializes `F`'s table bytes and
    /// writes one ring record. Overwrite mode never blocks; lossless may error.
    pub fn write(&mut self, frame: &F) -> Result<(), WriteError> { … }

    /// Publish a frame with dynamic `FrameList`/`FrameMap` members: the closure drives a
    /// `FrameWriter<F>` (its `list`/`map` builders) to patch the trailer before sending.
    pub fn write_with(
        &mut self,
        fixed: &F,
        build: impl FnOnce(&mut FrameWriter<F>),
    ) -> Result<(), WriteError> { … }
}
```

How a write lands as bytes (grounded in WP3): `FrameWriter::new(fixed)` seeds a `LenPacket`
with `F`'s fixed `#[repr(C)]` region, the closure appends any dynamic trailer via
`FrameWriter::{list,map}`, and `FrameWriter::table()` yields the table bytes (fixed region +
trailer, offset 0 at the fixed region). Those table bytes are exactly what `VTable::apply`
consumes and what the ring record payload carries — there is **no separate serialization
step**. The port hands `table()` to `Writer::try_write`.

`SystemOutput` is the bundle trait. It exposes (a) the static descriptors for wiring/sizing
and (b) the binding the coordinator uses to construct the ports from rings it created:

```rust
pub trait SystemOutput: Sized {
    /// Static: the frames this output bundle produces — for buffer sizing and wiring
    /// validation, read *before* any port exists (§5).
    fn descriptors() -> Vec<OutputDesc>;

    /// Coordinator-side: build the concrete port bundle from the rings it allocated,
    /// in `descriptors()` order. (Exact handle type elided; see §6 / WP5.)
    fn bind(rings: &mut dyn PortBinder) -> Self;
}
```

Multiple outputs at different rates are just multiple ports in the bundle; the coordinator
sizes each buffer independently and the system writes to each whenever it has new data. There
is no requirement that all outputs advance every cycle.

### 2.2 Output buffer sizing (from `MAX_SIZE` via the ring's record framing)

Each output buffer is sized from the frame's worst-case table size. WP3 already computes that
as `F::MAX_SIZE` (`Componentize::MAX_SIZE` — fixed region + the dynamic trailer budget, e.g.
`FrameList<T, MAX>::MAX_SIZE = round_up8(MAX * size_of::<T>())`). The ring wraps each record in
an 8-byte header + 8-byte-padded payload (`frame_len(payload) = 8 + round_up8(payload)`). So:

```
record_bytes  = frame_len(F::MAX_SIZE)            // worst-case one record
capacity      = (record_bytes * depth).next_power_of_two()   // Config.capacity must be pow2
```

`depth` is the number of in-flight records the buffer must hold — at least 2 (one being
written while the slowest active reader still holds one), more for a bursty producer or a lagging
async consumer. `Config.max_readers` is set to the **fan-out count**: the number of distinct
consumers (views) the coordinator will register on this output (cyclic consumers + one per
async private-buffer copy-in). The coordinator derives all three from the wiring graph (WP6) +
the descriptors (§5).

> **Gap to close (open question Q1).** The ring exposes `round_up8` publicly but **`frame_len`
> is private**. WP4 needs a public `frame_len`/`record_len` (or a `Config::for_record(max,
> depth)` helper) on the ring crate to size buffers without re-deriving the 8-byte header rule.
> Small, additive change to WP1.

### 2.3 Input ports — `SystemInput`

A system's `Input` associated type is a struct of one or more **input ports**, one per input
frame type, each wrapping a read-only ring `View`:

```rust
/// One borrowed input: a view into an upstream output buffer (cyclic) or a private
/// copy-in buffer (async), reading frame `F`.
pub struct Input<F: Frame, B: Backing, RD: WakeSink, RS: WakeSource> {
    view: View<B, RD, RS>,
    scratch: Vec<u8>,   // reused copy-out target (overwrite mode copies bytes)
    decoded: F,         // reusable decode target for the fixed region
}

impl<F: Frame + FromBytes + KnownLayout + …, …> Input<F, …> {
    /// True iff the writer lapped this view (overwrite buffers only). The coordinator
    /// checks this on cyclic systems *before* `execute` (§3.1).
    pub fn is_lapped(&self) -> bool { self.view.is_lapped() }

    /// Drain to the newest committed record and return a typed view of it, or `None`
    /// if no record has arrived yet. Returns `Err(Lapped)` on overwrite lap.
    pub fn latest(&mut self) -> Result<Option<FrameRef<'_, F>>, ReadError> { … }
}
```

**Typed access.** Two complementary paths, both over the bytes `View::try_read_into` copies
into `scratch`:

1. **Fixed region — zerocopy.** For fixed frames (and the fixed part of dynamic frames) the
   table bytes *are* the `#[repr(C)]` `F` layout (FrameWriter wrote `fixed.as_bytes()` at table
   offset 0). The port reads `F` directly via `F::ref_from_prefix(&scratch)` — no per-field
   decode. This requires `F: FromBytes + KnownLayout` (a bound the typed reader already needs).
2. **Dynamic members — typed reader.** `FrameList`/`FrameMap` members are read with the WP3
   `ListReader::new(table, slot)` / `MapReader::new(table, slot)` over the same `scratch`
   bytes, indexed by position or key. `FrameRef` exposes accessors that hand these out.

A third, uniform path is available where a system wants components rather than a typed struct:
`F::as_vtable().apply(&scratch, &mut sink)` drives any `Decomponentize` sink (the same path
metor-db uses). The typed paths above are the fast paths; the vtable path is the escape hatch.

**Latest-wins drain.** `latest()` loops `View::try_read_into` until `Ok(false)` (caught up),
keeping the last record — cyclic systems want the freshest sample, not a backlog. A system that
must process *every* record (e.g. a command channel) instead loops and handles each; that is a
system-author choice, not a trait constraint. Async event-driven systems use the async
`View::read_into` to await the next record (§3.2).

### 2.4 Fan-in / fan-out rule

- **Single writer per buffer.** Every ring buffer has exactly one `Writer` — the producing
  system's output port. The ring enforces this ("at most one live writer per buffer"); the
  coordinator upholds it by constructing exactly one `Output<F>` per output buffer.
- **N producers → N views.** A consumer of N upstream producers holds **N input ports**, each a
  `View` into a *distinct* single-producer buffer. There is never a shared writer and never a
  fan-in buffer. Combining N streams is the consumer's job (it reads N ports and fuses them),
  not the transport's.

---

## 3. Cyclic vs async — representation & lifecycle

Per DESIGN.md the only two differences are (a) where inputs come from and (b) how execution is
triggered. WP4 keeps **one `System` trait** and expresses the difference with the
`SystemKind` const plus, for async, one extra driver entry point. Two separate traits were
considered and rejected (open question Q3): they would duplicate the whole lifecycle surface
for a one-bit difference and make a system hard to reclassify.

### 3.1 Cyclic systems

- `KIND = SystemKind::Cyclic`.
- **Input source:** each input port's `View` is registered directly on the upstream system's
  output buffer (overwrite mode). No copy.
- **Triggering:** the coordinator calls `execute` once per cycle.
- **Lap = hard error.** Before invoking, the coordinator calls `is_lapped()` on every input
  port. A lapped view means the reader is hopelessly behind; per DESIGN.md the coordinator
  **telemeters it and stops invoking that system** (it does not silently `resync`). WP4's
  contribution here is only exposing `is_lapped()` per port and the descriptor list so the
  coordinator can iterate the ports; the stop policy itself is WP5.
- **Wake:** none. Cyclic ports use `NoWake` for both writer and view (the synchronous `try_*`
  paths never touch the wake hooks).

### 3.2 Async systems

- `KIND = SystemKind::Async`.
- **Input source:** each input port's `View` reads a **private buffer the coordinator owns**.
  The coordinator runs a copy-in `Writer` into that private buffer, copying the relevant
  upstream output records in. The system never sees the upstream buffer directly.
- **Drop on full, not stop.** An async system cannot be gated by skipping invocation, so on lap
  its input port **`resync()`s to the live edge and continues** (the dropped records are simply
  lost) — exactly DESIGN.md's "if there is no room, the data is dropped." This is the read-side
  behavioral difference from cyclic ports and is encapsulated in the async `Input` port.
- **Triggering:** the system runs its own loop. WP4 models this with an extra entry the
  coordinator launches **once**:
  ```rust
  pub trait AsyncSystem: System {
      /// The system's own loop. Returns when the system is shutting down. Implemented in
      /// terms of `self.execute(...)`, paced by either a timer or by awaiting inputs.
      async fn run(&mut self, input: &Self::Input, output: &mut Self::Output);
  }
  ```
  Inside `run`, an **event-driven** system awaits its input ports with the ring's async
  `View::read_into` (backed by a `Notifier` `WakeSink`) and calls `execute` on each wake; a
  **rate-driven** system sleeps on its own timer and calls `execute` on each tick. Either way it
  uses the async output path (`Writer::write` with a `Notifier`) so a lossless output can
  suspend for space. The coordinator spawns `run` as a task; it does **not** tick the system.
- **What the coordinator integrates:** the async system's outputs are ordinary ring buffers; a
  downstream consumer (cyclic or async) reads them like any other output. The coordinator reads
  them back out exactly as it would a cyclic system's outputs — there is nothing async-specific
  on the *output* side. The async-ness is entirely on the *input* side (private copy-in) and in
  who calls the loop.

### 3.3 Lifecycle ordering (the contract the coordinator honors)

`init(&mut output)` → for cyclic, `execute` per cycle while no input is lapped; for async,
`run` once → `shutdown(&mut output)` once at teardown. `init`/`shutdown` are exactly-once;
`execute` is many-times (cyclic) or driven from within `run` (async). Deterministic ordering
and replay across systems are explicitly future work (DESIGN.md "Each cycle").

---

## 4. Health / error telemetry

Systems do not return errors (DESIGN.md). A system reports its health as **ordinary frames**
flowing out over a dedicated, framework-provided **health output port** that every system gets
implicitly (the coordinator wires a health buffer per system, named `"<System::NAME>.health"`).
Because health is just frames, it lands in metor-db and any UI through the same path as all
other data — no special channel.

### 4.1 Shape — a standard health frame plus open-ended counters

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "health")]   // full id = "<system>.health" once prefixed
struct SystemHealth {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    /// Coordinator-maintained standard counters (filled by the port wrapper, not the
    /// system): cycles run, total errors, lapped-input events, last execute duration.
    cycles: u64,
    errors: u64,
    lapped_inputs: u64,
    last_execute_micros: u64,
    /// Domain-specific error counters the system bumps by name (e.g. "imu_timeout",
    /// "crc_fail"). Open-ended → a dynamic map keyed by error name.
    error_counts: FrameMap<Name, u64, MAX_ERR_KINDS>,
}
```

- **Error counters** are plain `u64` components. The four standard ones are maintained by the
  health-port wrapper around `execute` (so they exist even for a system that never touches
  health); domain-specific kinds use a `FrameMap<Name, u64>` so they need not be enumerated at
  compile time and still land as fully-qualified components (`<system>.health.error_counts.
  imu_timeout`) via the WP3 dynamic-frame path.
- **String logs** ride a separate, parallel **log frame**, because metor-proto `PrimType` has
  no string type — components are fixed-size scalar arrays. A log line is therefore a
  fixed-size byte component:
  ```rust
  #[derive(Frame, …)]
  #[metor_fsw(name = "log")]
  struct SystemLog {
      #[metor_fsw(timestamp)] timestamp: Timestamp,
      lines: FrameList<LogLine, MAX_LINES>,
  }
  #[derive(AsVTable, IntoBytes, Immutable, KnownLayout)]
  #[repr(C)]
  struct LogLine { level: u8, len: u8, msg: [u8; LOG_MSG_CAP] }  // u8 array = a component
  ```
  Each line lands as `…log.lines.0.msg` (a `U8` array component) + `level`/`len`, queryable in
  db like anything else. Capacity is bounded (`MAX_LINES`, `LOG_MSG_CAP`) so the buffer sizes
  via `MAX_SIZE` like every output.

### 4.2 The system-facing handle

The health/log ports are surfaced to the system as a small handle inside `Self::Output` (or a
distinguished `health()` accessor on it), so error reporting is one call and stays the *only*
mechanism:

```rust
output.health().error("imu_timeout");          // bump a named counter
output.health().log(Level::Warn, "skipped frame");  // append a log line
```

The wrapper batches counters into one `SystemHealth` record per cycle (or on change) and flushes
log lines as they arrive. Fault management beyond this telemetry is **out of scope** (DESIGN.md).

---

## 5. Self-description for wiring (what the coordinator reads)

Before any port exists, the coordinator must size buffers, allocate reader slots, and validate
that producers satisfy consumers. A system exposes a **static descriptor** built entirely from
the frame metadata WP3 already provides:

```rust
pub struct PortDesc {
    pub frame_id: ComponentId,   // F::FRAME_ID
    pub vtable: VTable,          // F::as_vtable()
    pub max_size: usize,         // F::MAX_SIZE  (table bytes; size buffers via frame_len)
    pub rate_hint: Option<Hz>,   // advisory, for buffer depth / async pacing
}
pub type OutputDesc = PortDesc;   // a produced frame
pub type InputDesc  = PortDesc;   // a required (expected) frame shape

pub struct SystemDescriptor {
    pub name: &'static str,       // System::NAME
    pub kind: SystemKind,         // System::KIND
    pub inputs: Vec<InputDesc>,   // <Self::Input as SystemInput>::descriptors()
    pub outputs: Vec<OutputDesc>, // <Self::Output as SystemOutput>::descriptors()
}
```

Each `PortDesc` is derived from a `Frame`: `frame_id = F::FRAME_ID`, `vtable = F::as_vtable()`,
`max_size = F::MAX_SIZE`. The coordinator reads this to:

1. **Size + allocate** each output buffer (`frame_len(max_size) * depth`, pow2; `max_readers` =
   fan-out) — §2.2.
2. **Validate compatibility.** A producer output satisfies a consumer input iff they share a
   `frame_id` **and** the consumer's component set is a subset of the producer's, with matching
   `ty`/`shape`. Both sides are enumerated with `VTable::realize_fields(None)` (registration
   mode — `table = None` surfaces every `(component_id, ty, shape)` including dynamic
   member templates, per the WP2 test `test_dynamic_registration_mode`). The check is a subset
   comparison over those triples. This catches "consumer expects a field the producer doesn't
   emit" and "type/shape mismatch" before a single byte flows.

WP4 defines only this **read surface**. The wiring language that says "system A's `imu` output
feeds system B's `imu` input", and the machinery that runs the validation and builds the
buffers, is WP6/WP5 — out of scope here.

---

## 6. dl-open / process boundary (noted, deferred)

DESIGN.md: a system is either a dl-opened dynamic library or a separate process, interacting via
shared-memory ring buffers. WP4 keeps **v1 in-process** and only maps the trait onto the two
deployment shapes so the boundary is not designed into a corner:

- **In-process (v1, the only one built now).** A `System` is a Rust value behind the trait (a
  generic or a `Box<dyn>` driver). `execute` is a direct method call. Ports wrap `Writer`/`View`
  over `BoxBacking` (or `MmapBacking` if cross-process readers attach). Cyclic ports use
  `NoWake`; async ports use `Notifier`. No Rust type crosses any boundary.
- **dl-open (later WP).** The ABI surface is deliberately narrow because **the bytes are
  described by the VTable, not by a shared Rust type**: the only things crossing the `.so`
  boundary are (a) the ring regions, attached by offset via `MmapBacking`/`attach_mmap`, and (b)
  the system's `SystemDescriptor` — and `VTable` is already `Serialize`/`Deserialize`
  (serde/postcard), so descriptors cross as bytes. The entry point is a stable C
  `system_execute(ctx)` where `ctx` carries the attached ring regions; `init`/`shutdown` get
  parallel C entries. The Rust `System` trait is the *in-process realization* of that same
  contract.
- **Separate process (later WP).** Identical data path; the difference is triggering. The ring
  header already **reserves** `FLAG_WAKE_SHARED` and `OFF_WAKE_WORD` for a cross-process wake
  word — that is the hook a future WP uses to notify a process-resident system that its private
  input buffer has data, replacing the in-process direct call / `Notifier`.

**Explicitly deferred:** the stable C ABI, descriptor marshaling format, cross-process wake
protocol, and lifecycle of dl-opened handles. v1 ships in-process; this section only records the
seams (`MmapBacking`, serializable `VTable`, the reserved wake word) so they stay open.

---

## 7. Reused vs. new

| Concern | Reused (landed) | New in WP4 |
|---------|-----------------|------------|
| Transport | `RingBuffer`, `Writer`/`try_write`/`write`, `View`/`try_read_into`/`read_into`/`is_lapped`/`resync`, `Overrun`, `Config` | `Output<F>`/`Input<F>` port wrappers binding a frame type to one ring handle |
| Wake | `WakeSource`/`WakeSink`/`NoWake`/`Notifier` | cyclic = `NoWake`, async = `Notifier` selection per port kind |
| Serialization | `FrameWriter` (`new`/`list`/`map`/`table`/`finish`), `ListReader`/`MapReader`, table bytes == ring payload | `Output::write`/`write_with`, `Input::latest`/`FrameRef` typed accessors |
| Frame identity / shape | `Frame` (`FRAME_ID`, `NAME`, `timestamp`), `AsVTable::as_vtable`, `Componentize::MAX_SIZE` | `PortDesc`/`SystemDescriptor` self-description built from them |
| Sizing | `round_up8`, `MAX_SIZE`, `Config.capacity` pow2 | buffer-sizing rule `frame_len(MAX_SIZE)*depth`; **needs `frame_len` made public** |
| Wiring validation | `VTable::realize_fields(None)` registration mode, `RealizedField` | subset/ty/shape compatibility check (run by WP5/WP6) |
| Health | `Frame`/`FrameMap`/`FrameList`, dynamic-name path, db ingest | `SystemHealth`/`SystemLog` standard frames + `output.health()` handle |
| The system itself | `Componentize`/`Decomponentize` traits | the `System`/`SystemInput`/`SystemOutput`/`AsyncSystem` traits, `SystemKind` |

Genuinely new code is concentrated in: the `System` family of traits, the `Output<F>`/`Input<F>`
port wrappers, `SystemDescriptor`, and the standard health/log frames. The data path is reuse.

---

## 8. Open questions / risks for the reviewer

1. **Q1 — public `frame_len`.** Buffer sizing needs the ring's record framing, but `frame_len`
   is private (only `round_up8` is `pub`). Make `frame_len` public, or add a
   `Config::for_record(max_size, depth)` helper to the ring crate? (Small additive WP1 change.)
2. **Q2 — `execute` signature (by-ref vs by-value).** This doc takes `&Self::Input` /
   `&mut Self::Output`, deviating from DESIGN.md's by-value sketch, because writers need `&mut`
   and must persist across cycles. Confirm the by-ref signature and that `init`/`shutdown` also
   receive `&mut Self::Output`.
3. **Q3 — one trait + `KIND` vs two traits.** Cyclic/async modeled as one `System` plus a
   `SystemKind` const and an extra `AsyncSystem::run`. Is the single-trait approach preferred,
   or should `CyclicSystem`/`AsyncSystem` be fully separate (clearer but duplicated lifecycle)?
4. **Q4 — async triggering surface.** Is `async fn run(&mut self, input, output)` the right
   shape, with the system itself choosing timer-paced vs input-awaited? Or should the
   coordinator own the loop and call a plain `execute`, with the system only declaring a
   trigger policy (rate / on-input)? The former gives systems full control; the latter keeps
   all scheduling in the coordinator.
5. **Q5 — port-bundle ergonomics.** `Self::Input`/`Self::Output` as hand-written structs of
   `Input<F>`/`Output<F>` is explicit but verbose. Should a `#[derive(SystemInput)]` /
   `#[derive(SystemOutput)]` generate `descriptors()`/`bind()` from the field frame types
   (mirroring the WP3 derives)? Likely yes, but it adds macro surface.
6. **Q6 — latest-wins vs every-record.** `Input::latest()` drops backlog by design (cyclic wants
   freshest). Command/event channels need every record. Is a second accessor (`drain(|rec|…)` /
   an iterator) part of WP4, or deferred until a command channel actually exists?
7. **Q7 — health buffer provisioning.** Is the per-system health/log port auto-wired by the
   coordinator (every system always has one, the standard counters always populated), or opt-in?
   Auto is simplest and matches "the only error mechanism", but costs a buffer per system.
8. **Q8 — log representation.** Logs as `FrameList<LogLine>` with a fixed `[u8; CAP]` message
   (truncating long lines) — acceptable, or do we want a dedicated variable-length text path
   (an `Op::Ext` blob, or a byte-stream lossless ring) so log text isn't capped/padded?
9. **Q9 — compatibility check strength.** Is exact `frame_id` + subset-with-matching-`ty`/`shape`
   the right contract, or should it be strict equality of the component sets? Subset allows a
   producer to emit extra fields a consumer ignores (forward-compatible); confirm that is wanted.
10. **Q10 — depth/`max_readers` source of truth.** Buffer `depth` and `max_readers` come from the
    wiring graph + `rate_hint`. Should `rate_hint` be mandatory on outputs (so depth is
    derivable) or is a global default depth acceptable for v1?
