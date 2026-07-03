# Port unification (`design-port-unification`)

> **Status: LANDED** — implemented across commits `0ba6c284`..`8e628dfd` (A1/C1-C7, 7 staged
> commits: unified port descriptor axes, single-pathed `build()`, ONE registry, one telemetry
> Tap/HandOff/drain loop, lap policy + `CommandOut` sugar, `ReceiveAll` as a capability, the
> schema-tagged dl ABI). `docs/system.md` §5 and `docs/telemetry.md` reflect the shipped shape.

**Status:** design, not yet implemented.
**Covers review findings:** A1 (primary), A5, A6, A7, A10, A4, plus the parts of C4
subsumed by A1 and S5's panicking descriptor accessors.
**Out of scope:** the ring crate (`metor-fsw-ring` API is unchanged; both `Overwrite`
and `Lossless` stay), A2/A3 (the command plane and slot-descriptor reframe — a parallel
design, `docs/design-command-slots.md`, builds *on* this model), A9 (coordinator as
system #0), E-cluster ergonomics.

---

## 1. Problem and goal

The message plane was added as a *twin* of the frame plane: `Output`/`MsgOut`,
`Input`/`MsgIn`, `OutputRegistry`/`MessageRegistry`, `RegistryEntry`/`MessageEntry`,
`HandOff`/`MsgHandOff`, `Tap`/`MsgTap`, `capacity_for`/`msg_capacity`,
`coord_ring`/`msg_ring`, `matches`/`matches_message`, `connect`/`connect_msg`,
`PortId::Frame`/`PortId::Msg` — ~10 near-verbatim copies. The real behavioral
differences are **independent axes** that the frame/message split bundles together:

| behavior | frames today | messages today |
|---|---|---|
| record description | vtable'd table bytes | self-describing `(PacketId, postcard)` |
| consumer semantics | latest-wins snapshot | every-record log |
| input fan-in | exactly 1 | 0..N |
| lap policy | cyclic: hard stop; async: hidden copy-in | silent resync (`is_lapped()` lies `false`) |
| cycle detection | included; `delayed` legal | excluded; `delayed` silently ignored |
| downlink opt-out | none | `telemetered` flag via a wrapper type (`CommandOut`) |

**Goal:** one port concept with orthogonal axes —

```
schema     Table(VTable) | Postcard(PacketId)      what a record is
delivery   Snapshot | Log                          what a consumer reads
fan-in     One | Many                              how many producers an input takes
on-lap     Stop | Resync                           what a lap means for the reader
```

— so the DESCRIPTOR, REGISTRY, TELEMETRY, and COORDINATOR layers are single-pathed.
"Frame port" and "message port" become two *configurations* of one concept, kept as
thin user-facing facades (`Output<F>`, `Input<F>`, `MsgOut<M>`, `MsgIn<M>`) so user
code stays exactly as ergonomic as `examples/adcs-fsw2` is today. Users never spell an
axis enum unless they are overriding a default.

---

## 2. The unified descriptor (`src/descriptor.rs`)

### 2.1 Target types

```rust
/// The edge key of a port. Two disjoint value spaces (a Table port keys on the
/// 8-byte frame ComponentId, a Postcard port on the 2-byte PacketId), so a
/// mismatched pair can never accidentally satisfy an edge.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PortId {
    Component(ComponentId),   // was PortId::Frame
    Packet(PacketId),         // was PortId::Msg
}

impl PortId {
    /// Checked accessors — the panicking `frame_id()` is deleted (S5).
    pub fn component(self) -> Option<ComponentId>;
    pub fn packet(self) -> Option<PacketId>;
}

/// Axis 1 — what one record is and how it is described.
#[derive(Clone)]
pub enum PortSchema {
    /// A component-frame table: `#[repr(C)]` bytes described by a vtable.
    /// Carries the wiring-compatibility vtable and the telemetry announce factory.
    Table { vtable: VTable, announce: AnnounceFn },
    /// A self-describing `(PacketId, postcard)` record. The 2-byte id *is* the
    /// schema; no vtable, no announce.
    Postcard,
}

/// Axis 2 — what a consumer is expected to read off the channel.
/// Drives ring depth, telemetry coalescing, cycle-detection membership, and
/// whether `delayed` is meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Delivery {
    /// A state sample: readers coalesce to the newest record (latest-wins).
    Snapshot,
    /// An event/command log: every record matters, in order, never coalesced.
    Log,
}

/// Axis 3 — how many producers may wire into this *input*. (Ignored on outputs;
/// fan-out is always unbounded.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FanIn {
    /// Exactly one edge, required (today's frame-input rule).
    One,
    /// Zero, one, or many edges (today's message-input rule).
    Many,
}

/// Axis 4 — what a writer lap means for this *input*'s reader. (Ignored on outputs.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OnLap {
    /// A lap is a hard fault: the framework telemeters it and permanently stops
    /// the consumer (today's cyclic frame doctrine).
    Stop,
    /// A lap means skip to the live edge and continue (best-effort; today's
    /// message and async-copy-in behavior).
    Resync,
}

/// One port's static shape.
#[derive(Clone)]
pub struct PortDesc {
    /// Edge key, derived from schema by the constructors:
    /// `Component(F::FRAME_ID)` for Table, `Packet(M::ID)` for Postcard.
    pub id: PortId,
    /// Display / KDL-token / registry-key name: `F::NAME` or `M::NAME` (§2.4).
    pub name: &'static str,
    /// Worst-case record bytes: `F::MAX_SIZE` or `MAX_MSG_BYTES`.
    pub max_size: usize,
    pub schema: PortSchema,
    pub delivery: Delivery,
    /// Input-side axes (documented no-ops on outputs — same struct both directions,
    /// as today: the direction is which `SystemDescriptor` list a desc sits in).
    pub fan_in: FanIn,
    pub on_lap: OnLap,
    /// Output-side: whether the downlink / `AllOutputs` taps this port (A6).
    /// Now a plain field on *every* port — frames get the opt-out too.
    pub telemetered: bool,
}
```

`PortKind` is deleted. `PortKind::ReceiveAll` does **not** become a schema variant —
it was never a port (no ring, no edge, sentinel id); it becomes a descriptor-level
*capability* (§2.5).

### 2.2 Constructors and modifiers

```rust
impl PortDesc {
    /// Table × Snapshot × One × Stop × telemetered — today's frame port.
    pub fn of<F: Frame>() -> Self;

    /// Postcard × Log × Many × Resync × telemetered — today's message port.
    pub fn msg<M: NamedMsg>() -> Self;

    // Builder-style overrides, chainable after either constructor. These are what
    // the derive's `#[port(...)]` field attributes (§6.2) lower to.
    pub fn untelemetered(self) -> Self;          // replaces PortDesc::msg_untelemetered
    pub fn with_on_lap(self, p: OnLap) -> Self;
    pub fn with_fan_in(self, f: FanIn) -> Self;
    pub fn with_delivery(self, d: Delivery) -> Self;   // e.g. a future Table×Log frame log

    /// Checked schema accessors (S5) — the panicking `vtable()`/`announce()`/
    /// `frame_id()` are deleted. Internal frame-only call sites use
    /// `.expect("table port")` with the invariant stated; user-reachable paths
    /// (wiring resolve) surface a WireError/LoadError instead.
    pub fn vtable(&self) -> Option<&VTable>;
    pub fn announce(&self) -> Option<&AnnounceFn>;
}
```

The full axis product is legal in the model. v1 facades construct only the two rows
below; other combinations (e.g. `Table × Log` — an every-record frame log) are
expressible via modifiers and are handled generically by the coordinator and telemetry
(§4, §5), but no facade mints them yet.

| facade | schema | delivery | fan_in | on_lap | telemetered |
|---|---|---|---|---|---|
| `Output<F>` / `Input<F>` | Table | Snapshot | One | Stop | true |
| `MsgOut<M>` / `MsgIn<M>` | Postcard | Log | Many | Resync | true |

### 2.3 Why `OnLap` is a fourth axis, not implied by `Delivery`

The obvious collapse — Snapshot ⇒ Stop, Log ⇒ Resync — is exactly the current hidden
coupling, and the codebase already exhibits **three of the four** cells:

- Snapshot × Stop — cyclic frame inputs (the hard-stop doctrine).
- Snapshot × Resync — async frame inputs (the copy-in ring is drop-on-full; the
  consumer never faults) and every telemetry tap (`resync()` on lap).
- Log × Resync — message inputs (`MsgIn::drain` resyncs) and the message downlink.

The fourth cell (Log × Stop — "a command channel that must never lose a record") is
precisely what a guaranteed-delivery command input would declare, and the
command-slots design may want it. A policy that three call sites already override is
not derivable; it is an axis. **Defaults preserve today's semantics** (Snapshot
defaults Stop, Log defaults Resync), so nothing changes for existing bundles.

Consequences of making it explicit:

- **`MsgIn::is_lapped()`'s hard-coded `false` dies.** The unified input core reports
  `lap_fault() = view.is_lapped() && self.on_lap == OnLap::Stop`. A Resync port
  returns `false` *because its policy says laps are not faults* — derived, not lied.
  `SystemInput::any_lapped()` keeps its name but is generated as
  `self.f.lap_fault() || …`.
- **The async copy-in buffer becomes a policy consequence, not a separate
  mechanism.** The rule is: *an async system cannot be step-gated, so an async
  Snapshot input is effectively Resync, implemented by the private copy-in ring*
  (which also supplies the matched `Notifier` pair `recv()` parks on). The copy-in
  pass keys on `(consumer kind == Async, delivery == Snapshot)` instead of
  `PortId::Frame`. Declaring `OnLap::Stop` on an async input is rejected at `build()`
  (`WireError::StopOnAsyncInput`) — the framework has no way to honor it.
- **Runtime read behavior follows the port's policy**, not its type:
  `Input::latest()` on a Resync port resyncs internally and keeps returning data
  (no unreachable `Err` arm — a first step toward E3); on a Stop port it surfaces
  the lap and the runner hard-stops as today. `MsgIn::drain` on a Stop port stops
  draining and lets `lap_fault()` report.

### 2.4 Message wire identity — `NamedMsg` (A10)

`msg_name::<M>()` (last segment of `std::any::type_name`) is deleted: renaming a Rust
type silently changed the KDL token and registry key, generics produced garbage
(`"Baz>"`), and the format is unspecified. Replacement — a metor-fsw-2-owned trait,
the message twin of `Frame::NAME`:

```rust
// src/message.rs
/// A Msg usable as a wired port: carries an explicit, stable wire/KDL/registry
/// name beside its `Msg::ID` edge key.
pub trait NamedMsg: Msg {
    /// The token a mission file writes (`msg="SequenceCommand"`) and the channel
    /// part of the registry key `<instance>.<NAME>`. Renaming the Rust type must
    /// not change this.
    const NAME: &'static str;
}
```

`MsgOut<M>`, `MsgIn<M>`, and `PortDesc::msg::<M>` bound `M: NamedMsg` (plus the
existing `Serialize`/`DeserializeOwned` where needed). Because metor-fsw-2 owns the
trait, impls for foreign wkt types are legal here:

```rust
// src/message.rs — migration for the wkt set, preserving today's tokens so every
// existing mission.kdl and registry key is unchanged:
impl NamedMsg for SequenceCommand      { const NAME: &'static str = "SequenceCommand"; }
impl NamedMsg for SequenceRegistry     { const NAME: &'static str = "SequenceRegistry"; }
impl NamedMsg for SequenceChannelEvent { const NAME: &'static str = "SequenceChannelEvent"; }
```

(The coordinator-minted channels that pass an explicit channel string today —
`"sequences"`, `"commands"` — keep doing so; `NAME` is the default, not a cage.)

*Alternative considered:* adding `const NAME` to `metor_proto::types::Msg` itself with
the blanket impl defaulting to `T::SCHEMA.name`. That fixes the same problem one layer
down but touches ~40 hand-written `impl Msg` blocks in `metor-proto/wkt/src/msgs.rs`
and every downstream crate that implements `Msg` manually (db, panel). The fsw-local
trait has the blast radius of exactly the types used as ports. **Open question 1.**

### 2.5 Capabilities on `SystemDescriptor` (A4 — separable work item)

`ReceiveAll` today is a pseudo-port: sentinel id `PortId::Frame(ComponentId::new(""))`,
a throwaway placeholder ring allocated just to keep the positional binder aligned, and
a *read* capability declared in the *output* list because only outputs reach `init`.
It lifts out of the port lists entirely:

```rust
/// A non-port resource a system needs from the host at bind time.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Capability {
    /// A read view over every telemetered output in the graph (`AllOutputs`).
    /// Counts one reader slot on every buffer at sizing time. Host-only.
    ReceiveAll,
}

pub struct SystemDescriptor {
    pub name: &'static str,
    pub kind: SystemKind,
    pub inputs: Vec<PortDesc>,
    pub outputs: Vec<PortDesc>,
    pub capabilities: Vec<Capability>,   // NEW
}
```

Bundle derives feed this through a small decl enum so one generated walk still covers
every field (§6.2):

```rust
/// What one bundle field contributes to the descriptor.
pub enum PortDecl {
    Port(PortDesc),
    Capability(Capability),
}
```

Each bindable port type gains `fn decl() -> PortDecl` (ports return
`PortDecl::Port(Self::descriptor())`; `AllOutputs::decl()` returns
`PortDecl::Capability(Capability::ReceiveAll)`). `SystemInput::descriptors()` /
`SystemOutput::descriptors()` are replaced by `fn decls() -> Vec<PortDecl>`, and
`SystemDescriptor` construction splits ports from capabilities. The bind walk skips
capability decls on the ring cursor (`AllOutputs::bind` pulls the registry, consuming
no `BoundPort` — already true), so `build()`'s placeholder ring is deleted and
`n_reg` becomes a count of `Capability::ReceiveAll` across all descriptors.

This is deliberately a **separable commit** (C6 in §9): everything before it works
with `ReceiveAll` still parked as a schema-less sentinel port.

### 2.6 `compatible()` — one path

```rust
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool {
    if producer.id != consumer.id || producer.delivery != consumer.delivery {
        return false;
    }
    match (&producer.schema, &consumer.schema) {
        (Table { vtable: pv, .. }, Table { vtable: cv, .. }) =>
            subset(realize_set(cv), realize_set(pv)),   // unchanged subset rule
        (Postcard, Postcard) => true,                    // id equality already checked
        _ => false,                                      // cross-schema never matches
    }
}
```

New rule: **delivery must match across an edge.** A Log consumer of a Snapshot ring
would silently see coalesced gaps; a Snapshot consumer of a Log ring would silently
discard records. The facades make agreement automatic; a hand-modified desc that
disagrees is a build error (`WireError::Incompatible` carries both deliveries in its
rendering).

---

## 3. The unified registry (`src/registry.rs`)

`OutputRegistry`/`MessageRegistry` and `RegistryEntry`/`MessageEntry` collapse into
one of each (this *is* C4's first bullet; the generic `Registry<E>` intermediate step
is skipped because there is only one `E` left):

```rust
/// What a tap needs to know about an entry's records.
pub enum EntrySchema {
    /// Announce-then-Table wire form.
    Table {
        frame_id: ComponentId,          // unprefixed, shared across instances
        vtable: VTable,                 // PREFIXED announce vtable (built once at build())
        metadata: Vec<ComponentMetadata>,
    },
    /// Self-describing records; the 2-byte id is read from each record.
    Postcard,
}

pub struct RegistryEntry {
    /// `ComponentId::new("<instance>.<name>")` — key, wire prefix, and identity.
    pub key: ComponentId,
    pub instance: Arc<str>,
    /// The port name (`F::NAME`, `M::NAME`, or an explicit channel string).
    pub name: Arc<str>,
    pub schema: EntrySchema,
    /// Tells a broad reader how to drain: coalesce (Snapshot) or FIFO (Log).
    pub delivery: Delivery,
    /// A6: carried on every entry; enforced at the AllOutputs source (§3.1).
    pub telemetered: bool,
    pub(crate) ring: RingBuffer<BoxBacking>,
}

impl RegistryEntry {
    pub fn view(&self) -> Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable>;
}

/// THE registry — one by-key index over every tappable buffer, frames and
/// messages alike, in build order.
pub struct Registry { entries: Vec<RegistryEntry>, by_key: HashMap<ComponentId, usize> }
```

One keyspace instead of two parallel ones means a same-instance name collision between
a frame and a channel (both `"<instance>.foo"`) is now *detectable*: `build()` rejects
a duplicate key as a new `WireError::DuplicateRegistryKey` rather than shadowing.

### 3.1 `AllOutputs` filters at the source (A6)

```rust
/// The broadcast tap capability (§2.5). Filters telemetered-ness HERE — consumers
/// (downlink, logger, recorder) can no longer forget the convention.
pub struct AllOutputs { registry: Arc<Registry> }

impl AllOutputs {
    /// Every TELEMETERED entry, in build order — the only iteration surface.
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry>;
    pub fn get(&self, key: ComponentId) -> Option<&RegistryEntry>;  // telemetered only
}
```

The full unfiltered `Registry` remains reachable only via the host-side
`Coordinator::registry()` (debugger/test surface). `CommandOut<M>` is deleted: the
uplink declares `#[port(telemetered = false)] commands: MsgOut<SequenceCommand>`
(§6.2). Frame outputs get the identical opt-out. The coordinator's own command ring —
today untelemetered by *not being registered at all* — becomes an ordinary registered
entry with `telemetered: false` (visible to a debugger by key, never downlinked),
removing the last "opt out by omission" path.

`Binder`/`RingSource` shrink accordingly: one `fn registry(&self) -> Arc<Registry>`
(host-only, panicking default as today) replaces the
`output_registry`/`message_registry` pair; `AllOutputs::bind` wraps it.

---

## 4. Coordinator `build()` — single-pathed passes (`src/coordinator/mod.rs`)

Pseudocode of the passes that currently fork on `PortId::Frame` vs `PortId::Msg`.
(The pass *extraction* itself is C2 of the findings and can ride along; what matters
here is that each pass branches on an axis, never on frame-vs-message.)

```text
PASS 1 — edge validation → one connection map
  cons_edges: HashMap<(cons, in_idx), Vec<(prod, out_idx)>>       // ONE map, Vec-valued
  for (p, c, delayed) in edges:
      out_idx, in_idx = position lookups (as today)
      compatible(prod_desc, cons_desc)?                            // §2.6, incl. delivery match
      in = cons_desc(in_idx)
      if delayed && in.delivery == Log        -> Err(DelayedLogEdge)        // A7
      if in.on_lap == Stop && kinds[c] == Async -> Err(StopOnAsyncInput)    // §2.3
      match in.fan_in:
          One  -> entry must be empty, else Err(DoubleConnect)
          Many -> reject exact duplicate (prod,out_idx), else append          // fixes B7
      if in.delivery == Snapshot && !delayed && p != c:
          forward_adj[p].push(c)               // cycle detection: Snapshot edges only

PASS 2 — cycle check: unchanged, over forward_adj.

PASS 3 — input coverage
  for each input desc: match fan_in { One if no edge -> Err(UnconnectedInput),
                                      Many -> 0..N legal }
  Additional: FanIn::Many requires Delivery::Log (latest-wins over many producers is
  ill-defined without cross-ring ordering) -> Err(SnapshotFanIn) at descriptor
  registration or here. (Open question 3.)

PASS 4 — fan-out: uniform count over cons_edges (no second message loop).

PASS 5 — ring allocation: ONE loop, ONE helper
  fn alloc_ring(desc, readers) -> RingBuffer:
      depth = match desc.delivery { Snapshot => config.default_depth,
                                    Log      => LOG_DEPTH /* = 64, was MSG_DEPTH */ }
      RingBuffer::create_in_memory(Config {
          capacity: capacity_for(desc.max_size, depth),   // msg_capacity DELETED (same body)
          max_readers: readers, overrun: Overwrite })
  readers = fan_out + n_receive_all + READER_SLACK
          (+ the SequenceCommand n_slots residue — an explicitly-marked remnant of A2
             that docs/design-command-slots.md deletes by making command edges explicit)
  registry_entry(instance, desc, ring) pushed for EVERY port:
      EntrySchema::Table{..} from desc.schema.announce(instance) | EntrySchema::Postcard
      delivery/telemetered copied from desc                       // one entry fn, not two
  coord_ring/msg_ring/msg_writer collapse into alloc_ring + one owned_writer helper;
  the coordinator's health/log/status/seq-registry/command channels all go through it.

PASS 6 — copy-in (async decoupling)
  for async system s, input in_idx:
      if desc.delivery == Snapshot:            // was: if PortId::Frame
          private overwrite ring + matched Notifier pair, CopyIn job   (unchanged mechanics)
      else (Log):
          nothing — direct fan-in views, poll-drained (unchanged)

PASS 7 — bind
  BoundInput chosen by desc.fan_in (was: by PortId match):
      One  -> BoundInput::One(producer ring | private copy-in ring)
      Many -> BoundInput::Many(all producer rings)
  Capability decls consume no cursor position (§2.5); the ReceiveAll placeholder
  ring and its filter in the outs list are deleted.
```

`connect_msg` is **deleted** (A7): `connect` already infers everything from the port
descs, exactly as the KDL front-end does (`EdgeKind` from `frame=`/`msg=` collapses to
one `builder.connect(..)` / `connect_delayed(..)` call; a `delayed=#true` on a
`msg=` edge now surfaces `DelayedLogEdge` at build instead of being silently
meaningless). `PortRef::new::<F>` / `PortRef::msg::<M>` stay as the typed addressing
sugar.

New/changed `WireError` variants: `DelayedLogEdge`, `StopOnAsyncInput`,
`SnapshotFanIn`, `DuplicateRegistryKey`, `DuplicateEdge`; `FrameIdMismatch` renames to
`PortIdMismatch` (message ports were always covered; the name was a fossil).

---

## 5. Telemetry — one tap, one hand-off (`src/telemetry/mod.rs`)

### 5.1 `HandOff` + `MsgHandOff` → one `HandOff`, two lanes

The wake scaffolding was identical (C4); the difference is coalescing policy — which
is the `Delivery` axis again. One struct, one `WaitQueue`, one sender:

```rust
struct HandOff {
    /// Snapshot lane: one coalescing slot per Snapshot tap (latest-wins;
    /// overwriting an occupied slot counts a drop).
    slots: Mutex<Vec<Option<LenPacket>>>,
    /// Log lane: one bounded FIFO shared by every Log tap (drop-oldest past
    /// LOG_HANDOFF_CAP, counted).
    fifo: Mutex<VecDeque<LenPacket>>,
    pending: AtomicBool,
    dropped_snapshots: AtomicU64,   // -> "telemetry.dropped"
    dropped_logs: AtomicU64,        // -> "telemetry.msg_dropped" (health key kept)
    wq: Arc<WaitQueue>,
}
impl HandOff {
    fn push_snapshot(&self, slot: usize, pkt: LenPacket);
    fn push_log(&self, pkt: LenPacket);
    fn drain(&self) -> (Vec<LenPacket>, Vec<LenPacket>);
}
```

### 5.2 `Tap` + `MsgTap` → one `Tap`

```rust
struct Tap {
    view: View<BoxBacking, NoWake, NoWake>,
    scratch: Vec<u8>,
    lane: Lane,     // from entry.delivery
    wire: Wire,     // from entry.schema
}
enum Lane { Coalesce { slot: usize }, Fifo }
enum Wire { Table { packet_id: PacketId }, Msg }   // Msg: id read from each record
```

`init` iterates `output.all.entries()` — already telemetered-only (§3.1) — with **one**
`TelemetryMode::matches(&RegistryEntry)` (the `matches`/`matches_message` pair merges:
`Subset.frames` matches `entry.name`, which now covers both frame names and channel
names). Table entries get an announce + an assigned `packet_id`; Postcard entries get
none. `execute` is one loop: drain the view (Coalesce ⇒ keep only newest; Fifo ⇒ every
record), frame the packet per `wire`, push to the matching lane. The generic
combination `Table × Log` falls out: FIFO lane + announced Table packets — an
every-record frame log downlinks correctly with zero extra code.

### 5.3 The shared drain helper (C4's second bullet)

The `try_read_into`/resync loop hand-rolled at six sites becomes one helper in
`src/port.rs` used by `Input::latest/drain`, `MsgIn::drain`, the copy-in jobs, and
both tap lanes:

```rust
/// Drain `view` into `scratch`, calling `f` per record; on lap, apply `on_lap`
/// (Resync: skip to live edge, report false; Stop: stop draining, report true).
pub(crate) fn drain_view(view: &mut View<..>, scratch: &mut Vec<u8>,
                         on_lap: OnLap, f: impl FnMut(&[u8])) -> bool /* lap_fault */;
```

---

## 6. User-facing facades and derives

### 6.1 The port types (`src/port.rs`, `src/message.rs`)

The four public names survive as thin typed facades over shared internals; **user
code in `examples/adcs-fsw2` compiles unchanged** except where it used `CommandOut`:

```rust
/// Table × Snapshot writer — as today. write()/write_with()/write_async().
pub struct Output<F, B = BoxBacking, WD = NoWake, WS = NoWake> { .. }

/// Table × Snapshot reader. Gains `on_lap: OnLap` (default Stop), set at bind
/// from the field's descriptor; `latest()` resyncs internally under Resync.
pub struct Input<F, B = BoxBacking, RD = NoWake, RS = NoWake> { .., on_lap: OnLap }

/// Postcard × Log writer — as today; `M: NamedMsg`.
pub struct MsgOut<M, B = BoxBacking, WD = NoWake, WS = NoWake> { .. }

/// Postcard × Log × Many reader — as today (K views); gains `on_lap: OnLap`
/// (default Resync). `is_lapped()` is DELETED; `lap_fault()` replaces it on the
/// SystemInput surface for all input types.
pub struct MsgIn<M, B = BoxBacking, RD = NoWake, RS = NoWake> { .., on_lap: OnLap }

// DELETED: CommandOut<M> (§3.1), PortDesc::msg_untelemetered, msg_capacity,
//          MSG_DEPTH (renamed LOG_DEPTH, an internal sizing constant).
```

The write-path split (raw table bytes vs id-prefix + postcard) genuinely differs per
schema and stays in the facades; everything the review counted as twinned below the
facades (descriptor, registry, hand-off, tap, sizing, matching, edge rules) is now
single-pathed.

### 6.2 Derive implications (`metor-fsw/macros/src/system.rs`)

The derives stay type-driven and positional; two additions:

1. **`#[port(...)]` field attributes** — lowered onto both generated fns so
   descriptor and runtime construction can never disagree:

   ```rust
   #[derive(SystemOutput)]
   pub struct UplinkPorts<B: Backing = BoxBacking> {
       #[port(telemetered = false)]              // was: CommandOut<SequenceCommand>
       pub commands: MsgOut<SequenceCommand, B>,
   }

   #[derive(SystemInput)]
   pub struct GuardIn<B: Backing = BoxBacking> {
       #[port(on_lap = "stop")]                  // Log input that must not lose records
       pub cmds: MsgIn<GuardCmd, B>,
   }
   ```

   Expansion: `descs.push(<#ty>::descriptor().untelemetered())` /
   `…with_on_lap(OnLap::Stop)`, and `#id: <#ty>::bind(src).with_on_lap(OnLap::Stop)`
   (ports carry a chainable `with_on_lap` at both levels; `telemetered` is
   descriptor-only). Unattributed fields expand exactly as today.

2. **Decls instead of descs** (with C6/§2.5): `descriptors()` →
   `decls() -> Vec<PortDecl>`; `any_lapped()` body ORs `lap_fault()` per input field.
   `AllOutputs` contributes `PortDecl::Capability(..)` and is skipped by the bind
   cursor. No other macro change — the message-wiring doc's "derives need no change"
   property is preserved for ordinary ports.

`Out<O>` (the health/log wrapper) is untouched: it appends two ordinary
Table × Snapshot descs/binds.

### 6.3 Wiring front-end (`src/wiring/`)

- `EdgeKind` stays (it disambiguates `frame=` vs `msg=` *name lookup*), but both arms
  resolve to `connect`/`connect_delayed` — the `connect_msg` arm is deleted.
- `msg="…"` matches `p.name == token` where the name is now `M::NAME` (§2.4) —
  tokens preserved for the wkt set, so existing mission files parse identically.
- `EdgeSpec.delayed` on a message edge: no parse change; `build()` rejects with
  `DelayedLogEdge` (previously accepted-and-ignored).

---

## 7. dl ABI wire deltas (`src/abi/mod.rs`)

`PortDescMsg` is frame-only today (`fsw_describe` cannot express a message port — a
real limitation now that the command-slots design wants occupants declaring
`MsgIn<SequenceCommand>`). It becomes the serialized mirror of the unified desc:

```rust
#[derive(Serialize, Deserialize)]
pub enum PortSchemaMsg {
    Table { frame_id: ComponentId, vtable: VTable, metadata: Vec<ComponentMetadata> },
    Postcard { id: PacketId, name: String },   // name: the NamedMsg token, leaked at load
}

#[derive(Serialize, Deserialize)]
pub struct PortDescMsg {
    pub name: String,
    pub max_size: usize,
    pub schema: PortSchemaMsg,
    pub delivery: Delivery,
    pub fan_in: FanIn,
    pub on_lap: OnLap,
    pub telemetered: bool,
}

#[derive(Serialize, Deserialize)]
pub struct SystemDescriptorMsg {
    pub name: String,
    pub kind: SystemKind,
    pub inputs: Vec<PortDescMsg>,
    pub outputs: Vec<PortDescMsg>,
    pub params_schema: OwnedNamedType,
    pub capabilities: Vec<Capability>,   // rejected non-empty at load in v1
                                         // (ReceiveAll is host-only)
}
```

`into_port_desc()` reconstructs a `Table` schema exactly as today (announce closure
synthesized from carried metadata) and a `Postcard` schema by leaking the carried
name. `lower()` drops its "dl systems carry frame ports only" panic path.

**Version bump:** `FSW_ABI_VERSION` is `3` in the working tree (the wave-1 `rate_hint`
deletion). This change is another incompatible re-shape of `PortDescMsg`. If wave-1's
`3` has not shipped in any release/artifact by the time this lands, **fold both into
the single bump to 3** (update the version-history comment to describe the combined
change); otherwise bump to `4`. Either way there is exactly one bump per released ABI
shape — never two bumps inside one release train.

---

## 8. Hooks for `docs/design-command-slots.md`

The unified model is the substrate the command-slots design needs; verify list:

- **`SequenceCommand` fan-in is expressible declaratively:** a slot's registered
  descriptor declares an input `PortDesc::msg::<SequenceCommand>()` = Postcard × Log ×
  Many × Resync — bound as `BoundInput::Many` → `MsgIn::from_views`. Nothing bespoke.
- **Explicit command edges:** ordinary `connect` (post-A7) from the uplink's
  `MsgOut<SequenceCommand>` / the coordinator's command port to each slot's input.
  A broadcast convenience (`connect_from_all::<M>(consumer)` or a wiring-level
  fan-out node) is that design's call; the edge machinery here supports N edges into
  one Many input with duplicate rejection.
- **The coordinator command ring is already a first-class registered entry**
  (untelemetered, §3.1); command-slots can promote it to a real descriptor-declared
  producer port (A9-lite) without registry surgery.
- **Residues intentionally left for it to delete:** the `n_slots` reader-budget
  special case and the type-keyed `command_producers` collection in build()
  (both marked in §4 PASS 5), and `SlotAux`'s undeclared ports (A3).
- **Possible need:** `OnLap::Stop` on a Log input (guaranteed-command doctrine) — the
  axis exists (§2.3); the ring-level `Lossless` mode remains available if it wants
  writer-side backpressure too (out of scope here).

---

## 9. Migration sequence (compile-green commits)

Each step builds green, passes the full test suite, and is a commit boundary.
Regression net throughout: the existing coordinator/telemetry/message/wiring/dl tests
plus `examples/adcs-fsw2` (closed_loop, sequences, bundle) — none of which should
need semantic changes until the step that touches their surface.

1. **C1 — descriptor axes (additive, behavior-identical).**
   `PortSchema`/`Delivery`/`FanIn`/`OnLap` + `telemetered` on `PortDesc`; `PortKind`
   deleted (`Message{telemetered}` folds into `Postcard` + field; `ReceiveAll` kept
   temporarily as a sentinel `PortDesc::receive_all()` with `Postcard` schema and a
   reserved name); constructors/modifiers; checked accessors replace the panicking
   `frame_id`/`vtable`/`announce` (S5) with `expect()` at internal frame-only sites;
   `PortId` variants renamed with `#[deprecated]` aliases if churn warrants.
   `NamedMsg` + wkt impls + `msg_name` deletion ride here (small, self-contained).
2. **C2 — coordinator single path.** One `cons_edges` map; axis-driven validation
   (new `WireError`s incl. `DelayedLogEdge`, duplicate-edge rejection); one
   `alloc_ring`/`owned_writer` helper (deletes `msg_capacity`, `coord_ring`+`msg_ring`
   twins); copy-in keyed on delivery; `BoundInput` keyed on `fan_in`; `connect_msg`
   deleted (builder + wiring resolve + adcs example updated).
3. **C3 — registry unification.** One `RegistryEntry`/`Registry`; `AllOutputs` over
   one `Arc<Registry>` with source-side telemetered filtering; single
   `RingSource::registry()`; coordinator command ring registered untelemetered;
   `DuplicateRegistryKey` check. `MessageRegistry`/`MessageEntry` deleted.
4. **C4 — telemetry unification.** One `Tap`, one two-lane `HandOff`, one
   `TelemetryMode::matches`; `drain_view` helper adopted at all six drain sites.
   `MsgTap`/`MsgHandOff`/`matches_message` deleted.
5. **C5 — policy plumbing (A5/A6 finish).** `on_lap` carried by `Input`/`MsgIn`;
   `lap_fault()` replaces `is_lapped()` on the bundle surface (`MsgIn::is_lapped`'s
   hard-coded `false` dies); `#[port(...)]` derive attributes; `CommandOut` deleted
   (uplink moves to the attribute); `StopOnAsyncInput` validation.
6. **C6 — capabilities (A4, separable).** `PortDecl`/`decls()`;
   `SystemDescriptor.capabilities`; `ReceiveAll` sentinel port + placeholder ring
   deleted; `n_reg` counts capabilities.
7. **C7 — dl ABI.** Schema-tagged `PortDescMsg` + `capabilities` on
   `SystemDescriptorMsg`; `FSW_ABI_VERSION` per §7; `docs/dl-open.md` +
   `DESIGN.md`/`docs/messages.md`/`docs/message-wiring.md` terminology sweep
   (frame/message → the axis vocabulary where they describe internals).

Ordering constraints: C3 needs C1's `telemetered` field; C5's `CommandOut` deletion
needs the derive attribute; C7 last so the wire shape changes once. C2 and C3 could
swap; nothing else can.

---

## 10. Test plan

**Descriptor unit tests (`descriptor.rs`):**
- Constructor axis defaults match the table in §2.2; modifiers override exactly one
  axis; `id`↔`schema` consistency (`of::<F>()` ⇒ `Component(F::FRAME_ID)`,
  `msg::<M>()` ⇒ `Packet(M::ID)`).
- `compatible()` matrix: Table/Table subset (positive + missing-field + shape
  mismatch), Postcard/Postcard id equality, cross-schema false, **delivery-mismatch
  false**.
- Checked accessors return `None` off-schema (no panics reachable from public API).
- `NamedMsg`: wkt names are the frozen strings above; `PortDesc::msg` name/token
  round-trips through the KDL edge resolver (guards KDL compat).

**Build-time wiring tests (coordinator + wiring):**
- `DelayedLogEdge` (builder `connect_delayed` on Log ports and KDL `delayed=#true`
  on `msg=`), `DoubleConnect` on One, `DuplicateEdge` on Many (B7), unconnected One
  errs / unconnected Many builds, `SnapshotFanIn`, `StopOnAsyncInput`,
  `DuplicateRegistryKey`, cycle detection ignores Log edges (existing test keeps
  passing) and still catches Snapshot cycles.
- Ring sizing: Log ports sized at `LOG_DEPTH`, Snapshot at `default_depth`; reader
  budgets unchanged vs today for the adcs graph (snapshot the numbers in a test).

**Runtime parity (the existing suites are the net — must pass unmodified in spirit):**
- All current `message.rs`, `coordinator/tests`, `telemetry/tests` behavior tests.
- New: untelemetered **frame** output absent from downlink but present in
  `Coordinator::registry()`; command ring findable by key, never downlinked;
  Log × `OnLap::Stop` input hard-stops its cyclic consumer on lap (the new fourth
  cell); Resync frame input on an async consumer keeps flowing across a lap
  (existing copy-in test, re-asserted through the policy path); `Table × Log` entry
  downlinks every record via the FIFO lane with a valid announce.
- Hand-off: coalescing lane drops-newest-overwrite counted, FIFO lane drop-oldest
  counted, one waitqueue wakes for either lane.

**Derive tests (`metor-fsw/macros` + a UI crate):**
- `#[port(telemetered = false)]` / `#[port(on_lap = "stop")]` reflected in *both*
  `decls()` and the bound port; unknown attribute key is a compile error (trybuild).
- Capability field consumes no bind cursor position (bundle with
  `AllOutputs` + ports in mixed order binds correctly).

**dl/ABI:**
- `PortDescMsg` postcard round-trip for both schema arms; version-mismatch load
  rejection; a `.so` declaring a Postcard port wires end-to-end (new fixture);
  non-empty `capabilities` rejected at load.
- `examples/adcs-fsw2` bundle/e2e tests green with an unchanged `mission.kdl`.

---

## 11. Open questions (need a human decision)

1. **`NamedMsg` home** — fsw-2-local trait (recommended: blast radius = port types
   only) vs `const NAME` on `metor_proto::types::Msg` with a blanket
   `SCHEMA.name` default (touches ~40 wkt impls + external `Msg` implementors, but
   puts the identity where `ID` lives).
2. **`PortId` variant rename** (`Frame`/`Msg` → `Component`/`Packet`): aligned
   vocabulary vs pure mechanical churn across ~60 use sites. Deprecated aliases can
   soften it; or keep the old names and only fix the accessors.
3. **`FanIn::Many × Snapshot`** — reject (recommended; latest-wins across producers
   is ill-defined) or define as "newest by per-ring arrival, per drain pass"?
4. **Untelemetered entries in the registry** — include-with-flag (recommended:
   debugger/test visibility, one keyspace, kills opt-out-by-omission) vs omit
   entirely (today's command-ring behavior; smaller registry surface).
5. **ABI bump folding** — will wave-1's v3 ship separately before this lands? Decides
   §7's "stay at 3" vs "bump to 4".
6. **`OnLap` on `Input::latest`'s signature** — this design keeps
   `Result<Option<_>>` and merely makes the `Err` arm honest (Stop ports only);
   E3's `latest() -> Option<FrameRef>` simplification is a natural follow-up once
   policy routes laps to health — fold it in here or keep E3 separate?
