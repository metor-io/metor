# Telemetry Downlink + the Output Registry

The telemetry downlink streams other systems' outputs out of the process in metor-proto's wire
format — the format `metor-db` ingests. It rests on a general capability, the **output registry**,
which lets any broad or dynamic reader reach every output buffer in the graph by id. The downlink is
the registry's first consumer; a logger, recorder, or debugger would use the same registry the same
way.

The code lives in two modules:

- `src/registry.rs` — `OutputRegistry` and `RegistryEntry`: the by-id index over the coordinator's
  ring table.
- `src/telemetry/mod.rs` — `TelemetrySystem`, the `Transport` trait and `TcpTransport`, the
  snapshot/hand-off mechanism, the async sender. Unit tests are in `src/telemetry/tests.rs`.

---

## 1. Purpose & the metor-proto wire symmetry

Flight software is useful only if its state can be observed off-board. The downlink **taps** the
output ring buffers that other systems already write and re-emits each buffer's latest record as a
metor-proto **`Table` packet** referencing a once-announced **`VTable`**. A frame's ring payload *is*
its table bytes — `Output::write` does a single `try_write` of `frame.as_bytes()` — so there is no
serialization step: the bytes a system committed are the bytes that go on the wire, and `metor-db`
stores them with no translation. The downlink and the database are two ends of one protocol.

This is the same VTable symmetry that `examples/cube-sat/src/main.rs` exercises by hand: it calls
`tx.init_world::<CubeSat>(id)` once (vtable + metadata) and then, every cycle,
`LenPacket::table(id, cap)` / `extend_from_slice(frame.as_bytes())` / `tx.send(pkt)` / `pkt.clear()`.
The downlink generalizes that hand-written loop to *every* output of a running graph, with
per-instance namespacing, without blocking the control cycle on the socket.

---

## 2. The general output registry

The mechanism the streamer uses to reach all outputs is a first-class capability available to every
system, not a telemetry special case. The registry is the centerpiece; telemetry is merely its first
consumer.

### 2.1 What it is

The coordinator owns a `RingTable`: a `Vec<RingEntry>` where every `RingEntry { ring, frame_id, role,
instance }` is one buffer in the graph. The registry is a thin, queryable index over that same table.
It is built in `build()` from the fully populated ring table and stored as `Arc<OutputRegistry>` on
the `Coordinator`; `Coordinator::registry()` returns a clone of that `Arc`.

```text
OutputRegistry
  entries:  Vec<RegistryEntry>           // one per tappable buffer, build order
  by_key:   HashMap<ComponentId, usize>  // instance-qualified id -> entries index
```

```text
RegistryEntry
  key:       ComponentId        // instance-qualified id  (§2.2)
  instance:  Arc<str>           // "imu_left", or "coordinator" for coordinator-owned buffers
  frame_id:  ComponentId        // the unprefixed frame id ("imu"), shared across instances
  vtable:    VTable             // the prefixed announce vtable          (§6)
  metadata:  Vec<ComponentMetadata>  // the prefixed component metadata  (§6)
  ring:      RingBuffer<BoxBacking>  // crate-private read source; reached only via view()
```

`vtable` and `metadata` are the **prefixed** announce schema (§6), captured at `build()`. The
authoritative unprefixed `VTable` lives on `PortDesc` and is otherwise dropped after sizing; the
registry captures the prefixed form because a broad consumer reads buffers by id and has no static
frame type `F` to call `F::as_vtable()` on.

`RegistryEntry.ring` is crate-private. External callers reach the buffer only through
`RegistryEntry::view()`, so every reader is slot-accounted (§2.5); the registry never hands out the
raw `RingBuffer`.

### 2.2 The key

Two instances of one system type share a frame `ComponentId` (`ImuDriver`'s `imu` frame is
`ComponentId::new("imu")` for *both* `imu_left` and `imu_right`), so `frame_id` alone cannot be the
key. The key is the **instance-qualified id** `ComponentId::new("<instance>.<frame>")` — e.g.
`ComponentId::new("imu_left.imu")`.

- It is a `ComponentId` (a `u64`): cheap, `Copy`, `Hash`, the project's universal name type.
- **It is exactly the value the downlink puts on the wire** — the prefixed `Op::Frame` tag id (§6).
  The key, the wire id, and the prefix are one identity.
- It is derivable from data the `RingEntry` already carries: `instance` + `frame_id`.

`instance` (`Arc<str>`) and `frame_id` are kept on the entry for human-readable subset filtering
(§3) and for the metadata names, but the map key is the qualified id.

### 2.3 Build order

The registry is built in `build()` before the bind loop, so a system can pull it in
`BindPorts::bind`. To make this work, the per-system output rings *and* the coordinator-owned
`health`/`log`/`coordinator_status` rings are allocated up front (the coordinator-owned rings depend
on no edges), the registry is assembled from all of them, and only then does binding run. Each
`RegistryEntry` is built once at allocation, capturing the prefixed announce vtable + metadata (§6).

### 2.4 How a system gets a handle

A registry consumer receives `Arc<OutputRegistry>` through the binder. The host `Binder` carries the
registry and exposes `RingSource::output_registry(&self) -> Arc<OutputRegistry>`. A system whose
output bundle wants broad access pulls the handle in its generated `BindPorts::bind`, exactly where
it pulls its typed ports. Because the registry is complete before the bind loop (§2.3), this is safe
and needs no second phase.

The registry coexists with typed `connect`/`PortRef` wiring cleanly: **typed wiring is for known
compile-time edges** (validated, compatibility-checked, sized into fan-out); **the registry is for
broad or dynamic access** where the consumer does not know the producer at compile time. They share
the same underlying rings; the registry never bypasses or duplicates a typed edge, it offers a by-id
read path over the same ring table.

Registry access is host-only: `TelemetryPorts::bind` is implemented for `B = BoxBacking`. The
telemetry downlink is never dlopen'd.

### 2.5 Sizing: every registry reader is a fan-out consumer

`RegistryEntry::view()` calls `RingBuffer::view(NoWake, NoWake)`, which **claims a reader slot** from
the buffer's fixed `max_readers` table. This is the critical interaction with build-time sizing: the
rings have no crash-slot reclamation, so `max_readers` is set once at `build()`.

The coordinator sizes for the known registry consumers. `n_registry_consumers` counts how many
systems pull the broad registry; `add_telemetry` bumps it by one. Every output ring is sized

```text
max_readers = fan_out + n_registry_consumers + READER_SLACK   // READER_SLACK = 4
```

and the coordinator-owned buffers are sized `1 + n_registry_consumers + READER_SLACK`. "All"-mode
telemetry contributes one slot to every buffer; a second registry consumer would add one more each.
This is exact and cannot over-subscribe for the known consumers.

If a slot budget is nonetheless exhausted — a hand-built over-subscription — `view()` returns
`Err(FullReaderTable)`; the downlink surfaces it as health and skips that tap rather than panicking
(§3). There is no runtime late attach: a consumer connecting after `build()` has no reserved slot
beyond the static `READER_SLACK`.

---

## 3. Telemetry as a registry consumer

`TelemetrySystem<T: Transport>` is an ordinary `CyclicSystem`, **registered last** so its `execute`
runs after every other system's `execute` in the cycle (`run_for` drives the cyclic systems in
registration order) — an end-of-cycle snapshot of the freshest output of each tapped buffer.

It has no typed input ports. `TelemetryIn` is empty; `TelemetryPorts` (its output bundle) declares no
descriptors and exists only to carry the `Arc<OutputRegistry>` pulled via the binder (§2.4).

**`TelemetryMode`** selects the tap set:

- `All` — tap every registry entry. This includes every system's user output frames *and* their
  implicit per-system `health`/`log` frames (those are `Output`-role buffers carrying the system's
  instance), plus the coordinator-owned `health`/`log`/`coordinator_status` buffers (prefixed
  `"coordinator"`, §6).
- `Subset { instances, frames }` — tap an entry when its `instance` name matches one of `instances`
  **or** its `frame_id` matches one of `frames` (compared as `ComponentId::new(frame) ==
  entry.frame_id`). Matching either list is enough.

**`init()`** resolves the tap set and spawns the sender. It runs on the coordinator's loop task
within `start()`, so `stellarator::spawn` has a runtime and the sender announces before any data is
queued. For each registry entry that `mode.matches`, `init`:

- claims one read `View` via `entry.view()`; on `Err(FullReaderTable)` it records
  `output.health().error("telemetry.reader_slot")` and skips the tap;
- assigns the tap a sequential `PacketId` = `(slot_index as u16).to_le_bytes()`;
- records a `Tap { slot, packet_id, view, scratch }` and an `Announce { packet_id, vtable, metadata }`
  carrying the entry's prefixed schema.

It then builds the hand-off (§4) sized to the tap count, an `AtomicBool` stop flag, and spawns
`run_sender`, retaining the join handle as a `JoinHandleDropGuard` (its `Drop` cancels the task at
teardown).

**`execute(now)`** (end-of-cycle) takes a latest-wins snapshot of each tap (§4) and surfaces any
drops as health.

**`shutdown()`** sets the stop flag and wakes the sender so it exits cooperatively; the drop guard
cancels it regardless when the system is dropped.

No telemetry-specific coordinator hook exists: the only coordinator surface is the general registry
(§2) plus the fact that any last-registered cyclic system observes end-of-cycle state. A recorder or
logger would be written identically.

---

## 4. Cycle / sender split — never block control on the network

The cycle is single-threaded and synchronous; `PacketSink::send` is async TCP. `execute` must not
await the socket — a slow or stalled link must never delay control. The work is split between the
in-cycle snapshot stage and an async sender task, bridged by a bounded, per-tap-coalescing hand-off.

### Snapshot stage (in-cycle, `execute`)

For each tap, `execute` drains the read `View` to its newest committed record (latest-wins):

```text
loop:
  match tap.view.try_read_into(&mut tap.scratch):
    Ok(true)  => got = true        // copied one record into the scratch Vec; keep draining
    Ok(false) => break             // caught up to the live edge
    Err(_)    => { tap.view.resync(); break }   // lapped: skip to the live edge
if got:
  pkt = LenPacket::table(tap.packet_id, tap.scratch.len())
  pkt.extend_from_slice(&tap.scratch)
  handoff.push(tap.slot, pkt)      // never blocks
```

A buffer with no new record this cycle is simply skipped. The snapshot is two copies: the ring bytes
land in the tap's reused `scratch: Vec<u8>` (via `try_read_into`), and then in the freshly built
`LenPacket` (via `extend_from_slice`). Both are `memcpy`s; there are no syscalls and no `.await` in
the cycle.

### Hand-off (`HandOff`)

The hand-off is a bounded, per-tap-coalescing slot map — **one pending packet slot per tap**:

```text
HandOff
  slots:    Mutex<Vec<Option<LenPacket>>>   // one slot per tap
  pending:  AtomicBool                      // a snapshot is waiting; avoids busy-spin
  dropped:  AtomicU64                        // snapshots lost to overwrite
  wq:       WaitQueue                        // wakes the parked sender
```

- `push(slot, pkt)` (cycle side, never blocks): if the slot is already occupied, the previous un-sent
  packet is overwritten and `dropped` is incremented; the new packet takes the slot, `pending` is set,
  and the sender is woken. A newer snapshot overwriting an older un-sent one is exactly the Overwrite
  ring semantics, one level up — at most one pending packet per `(instance, frame)`.
- `drain()` (sender side): takes every occupied slot, releasing the lock before any `.await`.

### Sender task (`run_sender`)

`stellarator::spawn`ed at `init`. It first announces every tap once (§5); if any announce fails it
exits. Then it loops: if the stop flag is set it returns; otherwise it drains the hand-off and sends
each packet via `Transport::send`. When the hand-off is empty it parks on the wait queue until
`pending` is set or stop is signalled. Any transport error stops downlinking — the task returns and
the cycle is unaffected.

### Drop policy

Consistent with the framework's overrun philosophy (drop, don't block — the rings are
`Overrun::Overwrite`):

- The hand-off coalesces per tap; the cycle never blocks on a backed-up link.
- When a stalled link leaves a slot occupied, the next snapshot overwrites it and counts a drop.
- `execute` surfaces drops in-band: it compares `HandOff.dropped` against a `last_dropped` watermark
  and emits `output.health().error("telemetry.dropped")` once per newly dropped snapshot, so loss is
  observable through the telemetry system's own health frame.

---

## 5. Wire protocol — announce once, stream per cycle

The downlink reuses the metor-proto primitives verbatim; nothing here is new protocol.

- **Announce-once (per tap, on connect).** Each tap has a sequential `PacketId` (`[u8;2]`). The sender
  sends `VTableMsg { id, vtable }` carrying the **prefixed** vtable (§6), followed by one
  `SetComponentMetadata(ComponentMetadata)` per component. This is exactly what `SinkExt::init_world`
  does (`send_vtable` then `send_metadata`), but with a per-instance prefix instead of the type's
  static names.
- **Stream-per-cycle.** For each tap with a fresh record, `execute` builds `LenPacket::table(id, cap)`
  and `extend_from_slice`s the latest table bytes; the sender forwards it. The `id` references the
  announced vtable; `metor-db` ingest receives `VTableMsg` then `Table(id)` packets — precisely what
  the downlink emits.
- **Dynamic frames** (`FrameList`/`FrameMap`). The table bytes already include the trailer
  (`Output::write_with` writes `fw.table()`), and the announced `VTable` already carries the
  `Op::List`/`Op::Map` ops. The downlink forwards both as-is — bytes plus List/Map-bearing vtable —
  and does nothing dynamic-aware itself; it is a pure forwarder.

---

## 6. Instance-name prefixing

Each tapped buffer is announced under a vtable + metadata whose component names are prefixed by the
instance name, so `imu_left` emits `imu_left.imu.omega` and `imu_right` emits `imu_right.imu.omega` —
never colliding on the wire despite sharing the `imu` frame id. The **table bytes are unchanged**: the
layout is positional, and only the names/ids the vtable maps those bytes to change.

The unprefixed `VTable` on `PortDesc` bakes each component's *hashed* id into the ops, and a hash is
one-way, so the prefixed vtable cannot be re-derived from the unprefixed one. Instead the prefixed
schema is re-derived from the static frame type with the instance as a `ComponentPath` prefix:

- `AsVTable::vtable_fields(prefix)` takes a path prefix, so the prefixed vtable is
  `vtable(F::vtable_fields(prefix))` — leaves become `component(instance.chain(frame).chain(field).id)`.
- `Metadatatize::metadata(prefix)` likewise yields prefixed `ComponentMetadata`. The alloc-free
  `PathHasher` rolls the same id as `ComponentId::new("<instance>.<frame>.<field>")`.

Because this needs the static `F` (erased to `PortDesc` by build time), `PortDesc` carries a prefix
factory captured when the descriptor is derived:

```text
PortDesc {
    frame_id, vtable, max_size, rate_hint,
    announce: AnnounceFn,   // Arc<dyn Fn(&str) -> (VTable, Vec<ComponentMetadata>)>
}
```

`PortDesc::of::<F>()` stores `announce_of::<F>`, a closure that closes over `F` and produces the
prefixed vtable + metadata for any instance name. At `build()` the coordinator knows each port's
instance, calls `announce(instance)`, and stores the result on the `RegistryEntry`. Coordinator-owned
buffers (`instance = None`) use the synthetic prefix `"coordinator"`.

The prefixed `Op::Frame` tag id equals `ComponentId::new("<instance>.<frame>")` — the same value used
as the registry key (§2.2), so the key, the wire id, and the prefix are one consistent identity.

---

## 7. Transport abstraction

A small trait isolates the wire from the streamer:

```rust
pub trait Transport {
    async fn announce(&mut self, msg: &VTableMsg, meta: &[ComponentMetadata]) -> Result<(), TransportError>;
    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError>;
}
```

`TransportError` is either `Disconnected` or `Io(String)` (a rendered, transport-agnostic I/O error).
Any error stops downlinking; the in-cycle snapshot stage keeps running and simply drops.

**`TcpTransport`** is the shipping transport. It holds a `SocketAddr` and an optional `TcpConn`
(`PacketSink<OwnedWriter<TcpStream>>` plus the read half, held only to keep the socket open — replies
are not read). It connects lazily on first use inside the async sender task — `TcpStream::connect`,
`split()`, `PacketSink::new` — the same path cube-sat uses. `announce` is `send_vtable` +
`send_metadata` (a `VTableMsg` then a `SetComponentMetadata` per component); `send` is
`PacketSink::send`.

The system is generic over `T: Transport`. The builder/loader supplies a `TcpTransport`; the unit
tests drive a deterministic in-memory mock against the same trait.

**Lifecycle.** Connect once. On disconnect the sender task returns and stops downlinking; the in-cycle
snapshot stage keeps running and drops (its hand-off fills) so control is unaffected.

---

## 8. KDL / builder surface

The downlink is declared like any other node in a wiring document, plus a builder convenience.

```kdl
telemetry {
    transport "tcp" addr="127.0.0.1:2240"
    mode "all"                              // or: mode "subset"
    // subset only — taps these instances/frames:
    // tap instance="imu_left"
    // tap frame="control"
}
```

- The loader (`src/wiring/mod.rs`) parses the `telemetry` node into a `(addr, mode)` pair, validates
  that `transport` is `"tcp"` and `mode` is `"all"` or `"subset"`, and adds the downlink after every
  `system` node so it is registered last (§3).
- **Builder surface:** `CoordinatorBuilder::add_telemetry(TelemetryConfig { transport, mode })`. It is
  an ordinary `add_cyclic_named("telemetry", TelemetrySystem::new(config))` that also bumps
  `n_registry_consumers` so every output ring's `max_readers` includes the new tap (§2.5).
- `TelemetryConfig<T: Transport>` carries the concrete `transport` value and the `mode`.

---

## 9. Reused vs new

**Reused:**

- The coordinator's `RingTable`/`RingEntry`/`BufferRole` and `output_instances` — the registry indexes
  this, it does not replace it.
- `RingBuffer::view`, `View::try_read_into`/`resync` (`metor-fsw-ring`).
- `PortDesc.vtable` — the authoritative layout, captured into the registry entry.
- `VTableMsg`, `SetComponentMetadata`, `LenPacket::table`/`extend_from_slice`, `PacketSink::send`,
  `SinkExt::{send_vtable, send_metadata, init_world}` — the whole wire path; cube-sat is the precedent.
- `ComponentPath`/`chain`/`PathHasher`, `AsVTable::vtable_fields(prefix)`,
  `Metadatatize::metadata(prefix)` — the prefix.

**New:**

- `OutputRegistry` + `RegistryEntry`, keyed by the instance-qualified id, stored as
  `Arc<OutputRegistry>` on `Coordinator` and reachable via `Binder::output_registry()`.
- The prefixed `vtable`/`metadata` on the registry entry, sourced via the `announce` prefix factory on
  `PortDesc`.
- `max_readers = fan_out + n_registry_consumers + READER_SLACK` sizing.
- `TelemetrySystem`, the snapshot → hand-off → async-sender split, the per-tap coalescing drop policy
  with its `telemetry.dropped`/`telemetry.reader_slot` health counters, and the `Transport` trait with
  `TcpTransport`.

---

## 10. Not yet implemented

- **`bbq`/SHM transport.** `metor-proto/bbq` is a local shared-memory framed queue speaking the
  identical `LenPacket` format and would be a drop-in `Transport` for a co-located consumer. Only the
  TCP transport ships; the KDL loader accepts `transport "tcp"` only.
- **Automatic reconnect/backoff.** The TCP transport connects once and stops downlinking on
  disconnect.
- **Runtime late attach.** Broad access is sized for the registry consumers known at `build()`; the
  rings have no crash-slot reclamation, so a consumer attaching at runtime has no reserved slot beyond
  the static `READER_SLACK` (§2.5).
