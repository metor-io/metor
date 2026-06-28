# Work-Package 7 — Telemetry Downlink + the Output Registry

This document designs **WP7**: a telemetry system that streams other systems' outputs out of the
process in metor-proto's wire format (the format `metor-db` ingests), and — the load-bearing piece —
the **general output registry** that lets it (and any future system) reach those outputs.

It builds directly on the landed WP5 coordinator (`src/coordinator.rs`: `RingTable`/`RingEntry`,
`BufferRole`, `output_instances`, the `build()` sizing pass, the single-threaded `run_for` loop),
the typed ports (`src/port.rs`: `Output`/`Input`/`View`, `View::try_read_into`/`latest`), the
descriptors (`src/descriptor.rs`: `PortDesc { frame_id, vtable, max_size, rate_hint }`), and the
metor-proto streaming primitives (`VTableMsg`, `LenPacket::table`, `PacketSink::send`,
`SinkExt::init_world`, `ComponentPath`). Read DESIGN.md "Telemetry Downlink" first.

> **Scope.** Design only. No telemetry-specific machinery is bolted onto the coordinator beyond one
> general capability (the registry) and one general "register a system last" invocation. The
> downlink is just the registry's *first consumer*.

---

## 1. Purpose & the metor-proto wire symmetry

Flight software is useful only if its state can be observed off-board. The downlink **taps** the
output ring buffers other systems already write and re-emits each buffer's latest record as a
metor-proto **`Table` packet** referencing a once-announced **`VTable`**. Because a frame's ring
payload *is* its table bytes (the WP2/WP3 invariant — `Output::write` does a single `try_write` of
`frame.as_bytes()`, `port.rs:97`), there is no serialization step: the bytes a system committed are
the bytes that go on the wire, and `metor-db` stores them with no translation. The downlink and the
database are two ends of one protocol. This is the same VTable symmetry that runs through the rest
of the design, extended to the network edge.

The precedent already exists in tree: `examples/cube-sat/src/main.rs` streams to a db/ground endpoint
by calling `tx.init_world::<CubeSat>(id)` **once** (vtable + metadata) and then, every cycle,
`LenPacket::table(id, cap); pkt.extend_from_slice(frame.as_bytes()); tx.send(pkt); pkt.clear()`
(main.rs:548, 642–756). WP7 generalizes that hand-written loop to *every* output of a running graph,
with per-instance namespacing, without blocking the control cycle on the socket.

---

## 2. The general output registry (the load-bearing decision)

> **Constraint (from the user).** The mechanism the streamer uses to reach all outputs MUST be a
> first-class capability available to *every* system, not a telemetry special case. The registry is
> the centerpiece; telemetry is merely its first consumer. A logger, recorder, debugger, or any
> future broad/dynamic reader uses the same registry.

### 2.1 What it is

A `RingTable` already exists: `Coordinator` owns one (`coordinator.rs:307`), a `Vec<RingEntry>`
where every `RingEntry { ring, frame_id, role, instance }` is *every* buffer in the graph
(`coordinator.rs:296`). The registry is a thin, queryable index over that same table — conceptually
`Map<OutputKey, &RingEntry>` — exposing a safe way to obtain a read `View` into any output by id.

```text
OutputRegistry
  entries:  Vec<RegistryEntry>           // one per tappable buffer, build order
  by_key:   HashMap<ComponentId, usize>  // qualified-id -> entries index
```

```text
RegistryEntry
  key:       ComponentId        // instance-qualified id  (§2.2)
  instance:  Arc<str>           // "imu_left"   (the WP6 instance name)
  frame_id:  ComponentId        // F::FRAME_ID  ("imu")   (unprefixed)
  vtable:    VTable             // the prefixed announce vtable          (§6)
  metadata:  Vec<ComponentMetadata>  // prefixed component metadata      (§6)
  ring:      RingBuffer<BoxBacking>  // a clone of RingEntry.ring        (the read source)
```

`vtable`/`metadata` are **new** on the entry. Today `RingEntry` carries only `frame_id` — the
authoritative `VTable` lives on `PortDesc` (`descriptor.rs:30`) and is dropped after sizing. The
registry must capture it (and its prefixed form, §6) at `build()`, because a broad consumer reads
buffers by id and has no static `F` to call `F::as_vtable()` on.

### 2.2 The key (resolve instance collisions)

Two instances of one system type share a frame `ComponentId` (`ImuDriver`'s `imu` frame is
`ComponentId::new("imu")` for *both* `imu_left` and `imu_right`). `frame_id` alone therefore cannot
be the key — `output_instances()` exists precisely because of this collision (`coordinator.rs:897`).

**Recommended key: the instance-qualified id** `ComponentId::new("<instance>.<frame>")` — e.g.
`ComponentId::new("imu_left.imu")`. Rationale:

- It is a `ComponentId` (a `u64`): cheap, `Copy`, `Hash`, already the project's universal name type.
- **It is exactly the value the downlink puts on the wire** — the prefixed `Op::Frame` tag id (§6).
  Keying the registry by the same id the prefix produces makes §2 and §6 *one* decision, not two.
- It is derivable from data the `RingEntry` already has: `instance` + `frame_id`'s source name.

The alternative — a `(SystemHandle, frame_id)` pair — is more explicit and collision-proof but is
not the wire identity, so the streamer would still have to compute the qualified id anyway; it also
leaks the `usize` system index. We keep `instance: Arc<str>` + `frame_id` on the entry for
human-readable subset filtering and for the metadata name, but the *map* key is the qualified id.

(Hash collisions between two distinct qualified ids are the same astronomically-unlikely fnv1a-64
top-bit-masked risk every `ComponentId` already carries — acceptable, flagged in §10.)

### 2.3 Ownership & when it is built

The coordinator owns the registry; it is built in `build()` from the fully-populated `RingTable`,
stored as `Arc<OutputRegistry>` on `Coordinator`. One ordering note: today the per-system output
rings are allocated and pushed into the table *before* the bind loop, but the **coordinator-owned**
health/log/status rings are pushed *after* it (`coordinator.rs:766–788`). For "all" mode to include
those (§3), either (a) allocate the coordinator-owned rings up front (they depend on no edges) and
build the registry just before binding, or (b) build the registry last and hand systems an
`Arc<OnceLock<OutputRegistry>>` the bind loop fills. **(a) is cleaner** and is the recommendation.

### 2.4 How a system gets a handle

A registry consumer must receive `Arc<OutputRegistry>`. Three options were considered; the
recommendation is the one most consistent with the landed `BindPorts`/`Binder` contract:

- **Recommended — a `Binder` accessor.** The binder already hands each system its rings positionally
  (`binder.rs:85` `next_output`). Add `Binder::output_registry(&self) -> Arc<OutputRegistry>`. A
  system whose bundle wants broad access pulls the handle in its generated `BindPorts::bind`, exactly
  where it pulls its typed ports. The registry is complete before the bind loop (§2.3), so this is
  safe and needs no second phase. The telemetry system stores the `Arc` and resolves its taps at
  `init()`.
- *Coordinator ctor argument* (the app constructs `TelemetrySystem::new(registry)` and the builder
  injects it) — rejected: the registry does not exist until `build()`, forcing an `OnceLock` dance
  the binder path avoids.
- *A special input-port type* (`RegistryInput`) — rejected as over-engineered; the binder accessor
  is the same idea without inventing a port kind.

This coexists with typed `connect`/`PortRef` cleanly: **typed wiring is for known compile-time edges**
(validated, compatibility-checked, sized into fan-out — `coordinator.rs:591–661`); **the registry is
for broad or dynamic access** where the consumer does not know the producer at compile time. They
share the same underlying rings; the registry never bypasses or duplicates a typed edge, it just
offers a by-id read path over the same `RingTable`.

### 2.5 Sizing: every registry reader is a fan-out consumer

Obtaining a `View` from a registry entry calls `RingBuffer::view(NoWake, NoWake)` (`ring/src/lib.rs:627`),
which **claims a reader slot** from the buffer's fixed `max_readers` table. This is the critical
interaction with WP5 sizing. Today each output ring is sized `max_readers = fan_out + READER_SLACK`
with `READER_SLACK = 4` (`coordinator.rs:45`, 670) — and that constant's own doc comment already
anticipates "late taps such as a db/telemetry sink or a debugger" and warns "v1 has no crash-slot
reclamation, so `max_readers` must be set at build time".

The registry makes that explicit. Two policies:

- **Recommended — size for known registry consumers at build.** Registry consumers are ordinary
  systems added to the builder, so their *count* is known at `build()`. Size every output ring
  `max_readers = fan_out + n_registry_consumers + slack`. "All"-mode telemetry contributes 1 slot to
  *every* output buffer; a logger + recorder would add 1 each. This is exact and cannot over-subscribe.
- *Keep the fixed `READER_SLACK` cap* — simpler, but silently fails (the `view()` returns
  `Err(FullReaderTable)`) once taps exceed 4 on a high-fan-out buffer. Acceptable only if the cap is
  documented and the streamer surfaces the error as health.

**Late attach is out of scope for v1.** A debugger connecting at runtime would need a free slot
reserved up front (slack) or crash-slot reclamation (future work, per the `READER_SLACK` comment).
The registry returns its `View`s at consumer `init()`, all within the build-time slot budget. This
is the same constraint the coordinator already documents; the registry inherits it, it does not
worsen it.

---

## 3. Telemetry as a registry consumer

`TelemetrySystem` is an ordinary `CyclicSystem`, **registered last** so its `execute` runs after
every other system's `execute` in the cycle (`run_for` drives `self.cyclic` in registration order,
`coordinator.rs:936`) — an end-of-cycle snapshot of the freshest output of each tapped buffer.

It has no input ports in the typed sense; it pulls `Arc<OutputRegistry>` via the binder (§2.4) and:

- **`init()`** resolves its **tap set** and claims one `View` per tapped entry:
  - **All mode** — iterate every `RegistryEntry`, claim a `View` on each. This includes every
    system's user output frames *and* their implicit per-system `health`/`log` frames (those are
    `BufferRole::Output` buffers carrying the system's `instance`, because `Out<O>` pushes the
    health/log `PortDesc`s into the output set — `system.rs:108–112`), plus the coordinator-owned
    `health`/`log`/`coordinator_status` buffers (`BufferRole::Coordinator`, `instance = None`,
    prefixed `"coordinator"` — §6).
  - **Subset mode** — filter entries by a configured list of `instance` and/or `frame` names, claim
    `View`s only for matches.
- **`execute(now)`** (end-of-cycle): for each tapped `View`, read the newest record bytes
  (`View::try_read_into`/the `Input::latest` drain-to-newest pattern, `port.rs:200`), snapshot them
  into that tap's `LenPacket`, and hand the packet to the async sender (§4). Latest-wins: a buffer
  with no new record this cycle is simply skipped (or re-sent, configurable).

No telemetry-specific coordinator hook exists: the only coordinator surface is the general registry
(§2) plus the fact that *any* last-registered cyclic system observes end-of-cycle state. A recorder
or logger would be written identically against the same registry.

---

## 4. Cycle / sender split — never block control on the network

The cycle is single-threaded and synchronous; `PacketSink::send` is async TCP. `execute` must not
await the socket — a slow or stalled link must never delay control. So the work is split:

```text
 [ cycle / loop task ]                         [ async sender task (stellarator::spawn) ]
   TelemetrySystem::execute(now):                loop:
     for tap in taps:                              pkt = queue.recv().await
       if let Some(bytes) = tap.view.latest():     transport.send(pkt).await   // PacketSink::send
         pkt = tap.packet.clone_with(bytes)      (re-announce vtables on (re)connect — §5/§7)
         queue.try_send(pkt)  // never blocks
```

- **Snapshot stage (in-cycle, cheap).** Copy the latest table bytes into a per-tap `LenPacket`
  (`LenPacket::table(packet_id, cap)` + `extend_from_slice`, then `clear()` for reuse — the cube-sat
  pattern). This is a `memcpy` per tapped buffer with a fresh record; no syscalls, no `.await`.
- **Hand-off.** A bounded queue/channel between the loop task and the sender task. Two concrete
  backings, both already in tree: a `stellarator` bounded channel, or **`bbq`**
  (`metor-proto/bbq` grant-based framed queue, `commit_len_pkt`) which is also the natural SHM
  transport (§7) and gives Overwrite/drop semantics for free.
- **Sender task.** `stellarator::spawn`ed at start (alongside the async-system launch in
  `Coordinator::start`, `coordinator.rs:963`); drains the queue and `PacketSink::send`s.

### Outbound drop policy

Consistent with the framework's overrun philosophy (**drop, don't block** — the rings are
`Overrun::Overwrite`, `coordinator.rs:674`):

- The hand-off queue is **bounded**; the snapshot stage uses `try_send` and **drops on full**. The
  cycle never blocks on a backed-up link.
- Because every frame is latest-wins, the right shape is **per-(instance,frame) coalescing**: keep at
  most one pending packet per tap, a newer snapshot overwriting an older un-sent one (exactly the
  Overwrite ring semantics, one level up). A `bbq` Overwrite queue or a per-key slot map gives this.
- Dropped snapshots bump a `telemetry.dropped` counter via the streamer's own `output.health()`
  (`health.rs:132`) so loss is observable in-band.

---

## 5. Wire protocol — announce once, stream per cycle

Reuses the metor-proto primitives verbatim; nothing here is new protocol.

- **Announce-once (per tap, at `init()` / on connect).** Assign each tapped `(instance, frame)` a
  `PacketId` (`[u8;2]`; cube-sat uses `fastrand::u16(..).to_le_bytes()`, main.rs:547 — sequential
  assignment is fine too). Send `VTableMsg { id, vtable }` (`wkt/src/msgs.rs:17`) carrying the
  **prefixed** vtable (§6), followed by the prefixed `SetComponentMetadata(ComponentMetadata)` for
  each component. This is exactly what `SinkExt::init_world` does (`metor-fsw/src/tcp.rs:41`:
  `send_vtable` then `send_metadata`) — WP7 calls the same two steps but with a per-instance prefix
  instead of the type's static names.
- **Stream-per-cycle.** For each tap, per cycle: `LenPacket::table(id, cap)`,
  `extend_from_slice(latest_table_bytes)`, send, `clear()` and reuse. The `id` references the
  announced vtable; `metor-db` ingest (`db/src/vtable_stream.rs`) receives `VTableMsg` then
  `Table(id)` packets — precisely what we emit.
- **Dynamic frames** (`FrameList`/`FrameMap`). The table bytes already include the trailer
  (`Output::write_with` writes `fw.table()`, `port.rs:111`), and the announced `VTable` already
  carries the `Op::List`/`Op::Map` ops (`vtable.rs:116/129`). The downlink forwards both **as-is** —
  bytes + List/Map-bearing vtable — and does nothing dynamic-aware itself. db-side streaming of
  dynamics is WP2b; the downlink is a pure forwarder.

---

## 6. Instance-name prefixing (the deferred sink prefix)

The goal: announce each tapped buffer under a vtable + metadata whose component names are prefixed by
the instance name, so `imu_left` emits `imu_left.imu.omega` and `imu_right` emits
`imu_right.imu.omega` — never colliding on the wire despite sharing the `imu` frame id. The **table
bytes are unchanged** (the layout is positional; only the *names/ids the vtable maps bytes to* change).

### The precise mechanism

The unprefixed `VTable` on `PortDesc` bakes each component's *hashed* id into an `Op::Data` op that
`Op::Frame`/`Op::Component` reference (`vtable.rs:102/77`). A hash is one-way, so you **cannot**
re-prefix component ids from the unprefixed vtable alone. The clean source of a prefixed vtable is
to **re-derive it from the static frame type with the instance as a `ComponentPath` prefix**:

- `AsVTable::vtable_fields(prefix: impl ComponentPath)` already takes a path prefix, and
  `as_vtable()` is just `vtable(Self::vtable_fields(()))` (`metor-fsw/src/vtable.rs:32`). So the
  prefixed vtable is `vtable(F::vtable_fields(&instance))` — leaves become
  `component(instance.chain(frame).chain(field).id)`.
- `Metadatatize::metadata(prefix: impl ComponentPath)` likewise yields prefixed `ComponentMetadata`
  (`metor-fsw/src/metadata.rs:6`), reusing `ComponentPath::chain` / `to_component_id` / `to_metadata`
  (`metor-fsw/src/path.rs:20/15/26`) — the alloc-free `PathHasher` rolls the *same* id as
  `ComponentId::new("<instance>.<frame>.<field>")`.

Because this needs the static `F` (gone by build time, where everything is `PortDesc`-erased), the
recommendation is to **capture a prefix factory on `PortDesc` when it is derived**:

```text
PortDesc {
    frame_id, vtable, max_size, rate_hint,
    // NEW, set in PortDesc::of::<F>():
    announce: fn(prefix: &str) -> (VTable, Vec<ComponentMetadata>),
}
```

`PortDesc::of::<F>()` (`descriptor.rs:39`) closes over `F` and can therefore produce the prefixed
vtable+metadata for *any* instance name. At `build()` the coordinator knows each port's instance
(`self.names[s]`, `coordinator.rs:461`), calls `announce(instance)`, and stores the result in the
`RegistryEntry` (§2.1). Coordinator-owned buffers (`instance = None`) use the synthetic prefix
`"coordinator"`.

The prefixed `Op::Frame` tag id equals `ComponentId::new("<instance>.<frame>")` — **the same value
chosen as the registry key in §2.2**, so the key, the wire id, and the prefix are one consistent
identity.

*Fallback if a function pointer on `PortDesc` is undesirable:* prefix the **metadata** names
directly (prepend `"<instance>."`, recompute `component_id = ComponentId::new(name)`), build an
unprefixed→prefixed id map from those names, rewrite the vtable's component-id `Op::Data` bytes
through the map, and string-prefix the `Op::List`/`Op::Map` `name` `Op::Data` strings. This avoids
the static `F` but is more fragile (it assumes every announced component appears in metadata);
flagged in §10.

---

## 7. Transport abstraction

A small trait isolates the wire from the streamer:

```text
trait Transport {
    async fn announce(&mut self, msg: &VTableMsg, meta: &[ComponentMetadata]) -> Result<()>;
    async fn send(&mut self, pkt: LenPacket) -> Result<()>;
}
```

- **v1 ships TCP.** `TcpTransport` wraps `PacketSink<TcpStream>` (`metor-proto/stellar`), connecting
  to a ground/db endpoint (`TcpStream::connect(addr).split()` then `PacketSink::new`, exactly
  cube-sat main.rs:541–544). `announce` is `send_vtable` + `send_metadata`; `send` is
  `PacketSink::send`. Lives in the async sender task (§4).
- **`bbq`/SHM is a documented future impl.** `metor-proto/bbq` (`PacketGrantW::commit_len_pkt`) is a
  local shared-memory framed queue speaking the identical `LenPacket` format — a drop-in `Transport`
  for a co-located consumer, and the same primitive usable for the in-process snapshot hand-off (§4).

**Lifecycle (v1).** Connect once at start. On disconnect, drop the sender and stop downlinking; the
in-cycle snapshot stage keeps running and simply drops (its queue fills and `try_send` fails) so
control is unaffected. Reconnect re-runs the announce phase for every tap (db needs the vtables
before any `Table`). Automatic reconnect/backoff is a documented future note, not v1.

---

## 8. KDL / builder surface

The downlink is declared like any other node in a WP6 wiring document, plus a builder convenience.

```kdl
telemetry {
    transport "tcp" addr="127.0.0.1:2240"   // v1: tcp; future: "bbq"
    mode "all"                              // or: mode "subset"
    // subset only — taps these instances/frames:
    // tap instance="imu_left"
    // tap frame="control"
}
```

- The loader (WP6 style: hand-walk `KdlDocument`, `miette` diagnostics) maps `telemetry` to a
  builder call. Because the streamer is registered **last** (§3), the loader adds it after every
  `system` node.
- **Builder surface:** `CoordinatorBuilder::add_telemetry(TelemetryConfig { transport, mode })` (an
  ordinary `add_cyclic_named("telemetry", TelemetrySystem::new(config))`). No bespoke builder phase.
- **Requesting the registry:** the telemetry system's `BindPorts::bind` calls
  `binder.output_registry()` (§2.4). The builder, knowing one registry consumer was added, sizes
  every output ring's `max_readers` to include it (§2.5).

---

## 9. Reused vs new

**Reused (do not reinvent):**

- `RingTable`/`RingEntry`/`BufferRole`/`output_instances` (`coordinator.rs`) — the registry indexes
  this, it does not replace it.
- `RingBuffer::view`, `View::try_read_into`/`latest`/`resync`, `Input::latest` drain (`ring`, `port.rs`).
- `PortDesc.vtable` (`descriptor.rs`) — the authoritative layout, captured into the registry entry.
- `VTableMsg`, `SetComponentMetadata`, `LenPacket::table`/`extend_from_slice`/`clear`,
  `PacketSink::send`, `SinkExt::{send_vtable, send_metadata, init_world}` (`wkt`, `proto`, `stellar`,
  `metor-fsw/src/tcp.rs`) — the whole wire path; cube-sat is the working precedent.
- `ComponentPath`/`chain`/`to_component_id`/`to_metadata`/`PathHasher`,
  `AsVTable::vtable_fields(prefix)`, `Metadatatize::metadata(prefix)` (`metor-fsw`) — the prefix.
- `bbq` (`metor-proto/bbq`) — hand-off queue and future SHM transport.

**New:**

- `OutputRegistry` + `RegistryEntry` + the qualified-id key + `Arc<OutputRegistry>` on `Coordinator`.
- `Binder::output_registry()` handle.
- `vtable`/`metadata` (prefixed) on the registry entry, sourced via a new `announce` prefix-factory
  on `PortDesc` (or the §6 fallback rewrite).
- `max_readers = fan_out + n_registry_consumers + slack` sizing change (replacing the fixed
  `READER_SLACK` for outputs).
- `TelemetrySystem` (a `CyclicSystem`), the snapshot→async-sender split, the outbound drop/coalesce
  policy, the `Transport` trait + `TcpTransport`.
- Reordering coordinator-owned ring allocation before the bind loop (§2.3).

---

## 10. Open questions / risks for the reviewer

1. **Registry key.** Recommended: instance-qualified `ComponentId::new("<instance>.<frame>")` (= the
   wire prefix id). Alternative: `(SystemHandle, frame_id)` (collision-proof, but not the wire
   identity and leaks the system index). Accept the fnv1a-64 collision risk between two qualified ids?
2. **System-access handle.** Recommended: a `Binder::output_registry()` accessor (consistent with
   `next_output`/`next_input`). Alternatives: an `Arc<OnceLock<OutputRegistry>>` injected at
   construction, or a dedicated `RegistryInput` port. Which fits the intended ergonomics?
3. **max_readers / late attach.** Recommended: size `max_readers` for the known build-time registry
   consumers. v1 has **no runtime late attach** (no crash-slot reclamation — the existing
   `READER_SLACK` constraint). Is build-time-only broad access acceptable for v1, or must we reserve
   runtime slack / design reclamation now?
4. **Prefix derivation.** Recommended: a `fn(&str) -> (VTable, Vec<ComponentMetadata>)` factory on
   `PortDesc` (clean, reuses `vtable_fields(prefix)`). Fallback: post-hoc metadata-driven vtable id
   rewrite (no static `F`, but assumes full metadata coverage). Is adding a fn-pointer to `PortDesc`
   acceptable, or is the rewrite preferred?
5. **Coordinator-owned buffers' prefix.** `instance = None` buffers (coordinator `health`/`log`/
   `coordinator_status`) are proposed to downlink under the synthetic prefix `"coordinator"`. Right
   name? Should they be in "all" mode at all, or opt-in?
6. **PacketId assignment.** Per-tap `[u8;2]` ids: random (cube-sat) risks collision across many taps;
   sequential is denser but must be coordinated. With "all" mode tapping every buffer (dozens of
   ids), which scheme, and what is the cap?
7. **Drop policy / coalescing.** Recommended: bounded queue, `try_send`, drop-on-full, per-
   (instance,frame) latest-wins coalescing, `telemetry.dropped` health counter. Is per-key
   coalescing (vs a flat ring) worth the bookkeeping for v1?
8. **Registry encapsulation.** Should the registry expose only `view()` factories (safe, slot-
   accounted) or also the raw `RingBuffer` clone? Raw access invites unaccounted reader slots;
   recommend `view()`-only.
9. **Ordering change.** Moving coordinator-owned ring allocation before the bind loop (§2.3) is a
   small but real change to `build()`; confirm it has no interaction with the status/health port
   wiring that currently happens last.
