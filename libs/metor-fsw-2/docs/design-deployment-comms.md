# Deployments: cross-target comms

A deployment is a set of targets that are configured, built, and run
together. [Shared configuration](design-deployment-shared-config.md) made one
`target.py` emit a `Deployment { ir_version, targets: [Wiring, …] }` envelope
keyed by `coordinator.namespace`; [running](design-deployment-running.md)
launches every member as a child process from its own bundle. This document
designs the comms part of [the rough design](rough-deployment-design.md):
`Publish` and `Subscribe` between members, typing across the boundary, how a
subscriber finds its publisher, and what a member sees when its peer is not
there.

The gateway is not designed here. See "Seams for the gateway".

## Design

The telemetry link already moves components between processes: a `TcpServer`
state owns a listener, a `Downlink` frames every tapped ring into one batch per
cycle, a client receives an identity packet, a schema announce, and then the
stream ([telemetry.md](telemetry.md); `src/telemetry/mod.rs`,
`src/telemetry/link/mod.rs`). metor-db and metor-panel are clients of that
link (`libs/db/src/remote/fsw.rs`, `libs/metor-panel/src/connections/target.rs`).
A peer target is one more client. Four decisions follow:

1. **A `Publish` is a `Downlink` with an instance filter.** In the IR it *is*
   `type="Downlink"` with `instances=[…]`, attached to a `TcpServer` state.
   Nothing new serves the bytes; a `Publish` server is a normal fsw link the
   panel can connect to as well.
2. **A `Subscribe` is a mirror of one peer instance.** It is a system of the
   peer instance's own *type* (`Plant` from the `adcs` pack), registered in
   the subscriber's graph with that type's telemetered outputs and no inputs,
   whose implementation is a client of the peer's link server writing
   received records into those outputs. Local edges from the mirror are
   ordinary edges; a local system reads `plant.gps` without knowing it came
   over a socket.
3. **The mirror is typed by declaration, checked at three points.** pyright
   types `sub.gps` from the generated `Plant` class; the Python recorder
   checks that the instance exists in a peer target of the same deployment
   and is published; resolve mints the mirror's ports from the pack manifest
   the subscriber already loads; and at connect time the peer's announce
   replay is compared against those ports with the same subset rule local
   edges use (`compatible`, `core/src/descriptor.rs`).
4. **Addresses are config-static; hosts are deploy data.** A subscriber
   carries the peer's namespace, link name, and port in its own `Wiring`. A
   lone bundle dials loopback and browses mDNS by namespace; a rendered
   deployment passes `--peer <ns>=<host>` from the envelope's host table.
   The launcher reads none of it.

## Goals

- Two members exchange frames and messages with `Publish`/`Subscribe`,
  spelled like local wiring, typed in pyright, and checked before and at
  connect.
- A member's bundle is self-contained: it runs against its peers with no
  launcher, no envelope, and no shared file.
- The link server, the downlink, the wire format, the announce pipeline, and
  the client's reconnect shape are reused as they are.
- A subscriber starting first, a publisher restarting, and a peer that never
  comes up are ordinary states a consuming system can observe.
- The IR change is additive with one version bump.

## Non-goals

- Lockstep or a shared clock across members. Each member keeps its own
  coordinator and clock; a cross-target edge is asynchronous by construction.
- Server-side per-client filtering of the stream. A subscriber receives the
  whole batch its server sends and keeps what it mirrors.
- Cross-architecture frames. A frame's ring bytes are its wire bytes
  ([frames.md](frames.md)); both ends must share layout and byte order, as
  today between a target and its process workers.
- Auth and encryption. The link has neither ([telemetry.md](telemetry.md),
  "Local discovery"); peers inherit the trusted-network assumption.
- Gossip membership (SWIM). The panel's phase-2 plan
  (`libs/metor-panel/docs/plans/service-discovery-phase2-gossip.md`) stays a
  later layer over the same link.
- Subscribing to a `@system` Python instance across targets. Its descriptor
  lives in the peer's compiled program, not in a pack manifest a subscriber
  can read.

A prior attempt at this section landed a contracted peer protocol
(`6ba78bc8`, `6ce6c60e`: `PeerServer`/`PeerClient` states, `Publish`/`Consume`
systems, JSON endpoint ICDs, a second wire format) and was deleted in
`2bcb7084`. Its offline contract module survives as
`metor_fsw_2_core::peer` (`core/src/peer/`). This design keeps the two ideas
worth keeping from it, the exact schema check at connect and a typed
freshness output, and drops the rest: there is one wire protocol, one server,
and no contract file, because the announce replay already is the contract.

## The model

### What crosses

Both port kinds cross, with their existing semantics:

- A frame crosses as a `Table` packet whose payload is the ring record
  (`Wire::Table`, `src/telemetry/mod.rs`). The subscriber writes the payload
  into the mirror's frame ring unchanged. Delivery stays `Snapshot`: the
  downlink contributes the newest record only when `committed` moved, so a
  cycle with no new sample sends nothing and the mirror's `latest()` keeps
  the pinned record.
- A message crosses as a `Msg` packet, id first (`Wire::Msg`). The subscriber
  prepends the id and writes one record per packet into the mirror's message
  ring, in arrival order. Delivery stays `Log`.

Loss follows the link's rules ([telemetry.md](telemetry.md), "Bounded
loss"): a subscriber whose pending buffer would cross `PENDING_CAP` misses a
whole batch and the publisher counts it; the cycle never waits for a socket.
Frames are latest-state, so a missed batch is a stale sample until the next.
Messages are at-most-once and ordered per producer, the same guarantee the
ground has. A message ring the mirror cannot write (full) drops the record
and counts it, the `publish` rule ([messages.md](messages.md)).

### Cycle position

A `Publish` is a `Downlink`, so it carries `ReceiveAll` and resolve places it
at the tail of the cycle (`src/wiring/resolve.rs`, the deferred pass). It
publishes the cycle's final state.

A `Subscribe` is a free-running `AsyncSystem` ([system.md](system.md)). Its
task reads the socket between cycles and writes private rings; at the
mirror's registered position the coordinator exports those rings to the
graph, newest record for a snapshot port and every record for a log port
(`plan_async_io`, `src/coordinator/init/rings.rs`). Systems after the mirror
in the same cycle read what arrived before that boundary. A cross-target edge
is therefore always at least one cycle late on the subscriber's clock plus
network time, and it is never a same-cycle dependency.

The frame cycle check needs no change. Each member's graph is checked on its
own; the mirror is a source with no inputs, and a `plant → fsw → plant` loop
is two forward edges in two graphs, broken by the boundary on both sides.
An edge out of a mirror follows the ordinary rule: forward when the mirror
was added before its consumer, `delayed=True` when after.

### Peer down, peer not yet up

Before the first record arrives, the mirror's rings are empty and a consumer's
`latest()` returns `None`, the startup case [messages.md](messages.md)
already documents. After the peer drops, the rings keep their last records
with their original timestamps; `latest()` keeps returning the pinned record.
Nothing is zeroed and no timestamp is invented.

The mirror publishes one extra frame, `peer_status`, on change:

```rust
#[metor_fsw(name = "peer_status")]
pub struct PeerStatus {
    timestamp: Timestamp,
    /// 1 while a verified connection is live.
    connected: u8,
    /// Connections established over the run; a restart is visible as +1.
    sessions: u64,
    /// Records written into mirror rings over the run.
    records: u64,
    /// The subscriber's own cycle count when the last record was written.
    last_rx_cycle: u64,
    /// Records the mirror could not write (a full ring) or map (unannounced).
    dropped: u64,
}
```

A consumer that must gate on freshness connects it
(`fsw.connect(sub.peer_status, ctrl.peer_status)`) and compares
`last_rx_cycle` with its own cycle count. It
never subtracts a peer frame's timestamp from the local clock; the two
members' clocks are unrelated. The frame is telemetered under
`<ns>.<mirror>.peer_status` like `link_status` is today.

## Python API

### Shapes

```python
from metor_config import Deployment, Downlink, Publish, Subscribe, Target, TcpServer
from adcs_pack import Ctrl, Nav, Plant

plant = Target(cycle_rate=120.0, namespace="plant")
fsw = Target(cycle_rate=120.0, namespace="fsw")

# plant: simulate, serve the ground, and offer `sim` to peers.
ground = plant.state("link", TcpServer(addr="[::]:2240"))
peers = plant.state("peer", TcpServer(addr="[::]:2242"))
sim = plant.add("sim", Plant(seed=42), process=True)
plant.add("downlink", Downlink(ground))
plant.add("publish", Publish(peers, [sim]))

# fsw: mirror `sim`, then control against it.
sim_in = fsw.add("sim", Subscribe(sim))          # typed as Plant
nav = fsw.add("nav", Nav())
ctrl = fsw.add("ctrl", Ctrl())
fsw.connect(sim_in.sensors, nav.sensors)
fsw.connect(sim_in.gps, ctrl.gps)
fsw.connect(nav.attitude_estimate, ctrl.attitude_estimate)
fsw_peers = fsw.state("peer", TcpServer(addr="[::]:2243"))
fsw.add("publish", Publish(fsw_peers, [ctrl]))

# plant: mirror `ctrl`. Added after `sim`, so the edge into it is a back
# edge and says so, as the actuator edges in the single-target example do.
ctrl_in = plant.add("ctrl", Subscribe(ctrl))
plant.connect(ctrl_in.torque_cmd, sim.torque_cmd, delayed=True)

deploy = Deployment(targets=[plant, fsw])
```

`Publish(state, instances)` takes a `TcpServer` handle and a list of system
handles from the same target. It records `type="Downlink"`,
`attach=<state>`, `instances=[<bare names>]`, exactly what
`Downlink(state, instances=[...])` records, and returns the `SystemHandle`.
`Downlink(state)` with no filter publishes everything on its server; a target
with no ground downlink may `Publish` on its only server, and a target with
no peers may skip `Publish` entirely.

`Subscribe(handle, via=None)` takes a system handle from *another* target of
the deployment. It records a `SystemSpec` whose `ty` and `artifact` are the
peer instance's own (`Plant`, `adcs`), with `params="None"`, no `attach`, and
one new field, `peer` (below). The declaring artifact registers on the
subscriber like any generated `System`'s does (`_register_spec_artifact`,
`python/metor-config/metor_config/_target.py`). `via=` names the `Publish`
handle to dial when the peer target offers the instance on more than one
server.

Step order is `add` order, as today. The fsw side adds its mirror before
`nav` and `ctrl`, so a plant sample that arrived between cycles is read the
same fsw cycle. The plant side cannot add its mirror before `sim` in one
sequential file (`Subscribe(ctrl)` needs `ctrl`, which needs `sim_in`, which
needs `sim`), so it adds it after and marks the edge delayed: the command
lands the next plant cycle, which is what the single-target example's
actuator edges already do.

### Typing

`add` is overloaded to return its spec's own class (`Target.add`'s `H`
overload). `Subscribe` is annotated `def Subscribe(of: H, via: SystemHandle |
None = None) -> H`, so `fsw.add("sim", Subscribe(sim))` is typed `Plant` and
`sim_in.gps` is `OutPort[Gps]`; `fsw.connect(sim_in.gps, ctrl.gps)` checks
under the existing `connect(OutPort[F], InPort[F])` signature. A wrong frame
is the pyright error it is today; the stub generator
(`src/wiring/pack_module.rs`) emits nothing new. `sim_in.system_status` types
as the peer's status frame and resolves at runtime to the mirror's own
host-written record, the same frame type.

At runtime the handle is a `SystemHandle` whose `__getattr__` yields
`PortRef`s (`_model.py`). `SystemHandle` gains two fields set by `Target.add`:
`target` (the owning `Target`) and `spec` (the `Spec` it registered), so
`Subscribe` can read the peer's type, artifact, and target. `_the_deployment`
keeps running the emission rule; `Deployment.__init__` walks every member's
subscriptions and checks, with the source anchor of the `Subscribe` call:

- the peer target is a member of this deployment and is not the subscriber;
- the peer instance is a pack `System` or a `static_system` spec, not an
  `ExprHandle` (a `@system`);
- exactly one `Downlink`-typed system in the peer target lists the instance
  or has no filter; several is an error naming them and asking for `via=`;
  none is an error naming the peer's servers;
- the server that `Publish` attaches to declares a fixed port. Port `0` is
  valid for a ground link and rejected for a published one.

Every rule above is about the deployment's shape and is only checkable where
every `Target` is in scope, so it lives in Python. Everything about ports
lives in Rust.

### Fan-out, several peers, collisions

One `Publish` serves any number of subscribers; the link fans one batch out
to every connection. Two members subscribing to `sim` each add their own
mirror. A member subscribing to two peers adds two mirrors; two redundant
strings each with a `nav` become `fsw.add("nav_a", Subscribe(a_nav))` and
`fsw.add("nav_b", Subscribe(b_nav))`, both typed `Nav`.

Mirror names are instance names in the subscriber's target and collide like
any other (`validate`, `src/wiring/validate.rs`). Component ids qualify with
the subscriber's namespace: `fsw.sim.gps.lla` is the plant's `gps` as fsw saw
it, distinct from `plant.sim.gps.lla`.

A `Publish` nobody subscribes to is a downlink with no client: it drains its
taps and serves nothing, as a ground downlink does with no panel connected. A
one-member deployment may keep one; the file does not change when the peer
is added later.

## Reuse of telemetry

Verbatim:

- `TcpServer` / `LinkState` and its accept loop, per-connection queues,
  `PENDING_CAP`, and inbound FIFO (`src/telemetry/link/mod.rs`).
- `TelemetrySystem` (`Downlink`), its tap set, batching, retained snapshot
  messages, and `link_status` (`src/telemetry/mod.rs`). A `Publish` is one.
- The wire: `LinkInfo` first, then `VTableMsg` + `SetComponentMetadata` per
  table tap, `SetMsgMetadata` per message id, retained snapshots, live
  batches (`LinkState::set_announces`).
- The client's reconnect shape: identify, stream until the socket drops,
  back off 500 ms doubling to 10 s, repeat (`fsw_loop`,
  `libs/metor-panel/src/connections/target.rs`; `identify`/`fsw_stream`,
  `libs/db/src/remote/fsw.rs`).
- `PacketStream`/`PacketSink` (`libs/metor-proto/stellar`), already a
  dependency of the fsw crate.
- The async boundary: private rings, snapshot/log export, drop accounting
  (`src/coordinator/init/rings.rs`, `src/coordinator/bind.rs`).

Extended:

- `TelemetrySystem::configure` qualifies its `instances` list with
  `ctx.namespace`, as `AlarmSystem::configure` does (`src/alarm/mod.rs`).
  Today `RegistryEntry::instance` is the qualified name
  (`InitGraph::qualify`, `src/coordinator/init/rings.rs`) while the filter
  compares against the bare name the config wrote, so
  `Downlink(instances=["nav"])` on a namespaced target matches nothing. This
  is a defect on the standard path that `Publish` depends on; fix it there.
- `LinkInfo` gains `namespace: Option<String>` and `link: String` (the
  serving state's declaration name); `LINK_PROTOCOL_VERSION` goes to 2
  (`libs/metor-proto/wkt/src/msgs.rs`). postcard's `from_bytes` ignores
  trailing bytes, so a v1 reader (an older panel) still decodes the packet.
- `discovery::advertise` (`src/telemetry/discovery.rs`) adds the TXT records
  `ns=<namespace>` (the existing, unused `TXT_NAMESPACE`) and
  `link=<state>` (new `TXT_LINK`), and defaults the instance name to the
  namespace. A shared-state constructor sees only its params today
  (`Pack::shared_state`, `core/src/pack.rs`); it gains a `StateCtx` with
  the state's declaration name and the target namespace, the two facts the
  link's identity is made of. See "Discovery".
- `resolve` gains a mirror arm beside the dl/proc/wasm arms
  (`src/wiring/resolve.rs`): a `SystemSpec` with `peer` set never goes
  through a registry factory, because its descriptor comes from the peer
  type, not from a constructor.

New, all in one module `src/telemetry/subscribe.rs`:

- `SubscribeSystem`, an `AsyncSystem` with a dynamic output bundle (raw ring
  writers taken with `RingSource::try_next_output`, the shape the deleted
  `FrameInputs` used and `MsgFanOut` uses today), a `peer_status` output,
  and a log.
- The client loop: candidates, connect, verify `LinkInfo`, map the announce
  to mirror ports, route packets to rings. About the size of `fsw_stream`.
- An mDNS browse that filters on `ns`/`link`, from the panel's `browse`
  (`libs/metor-panel/src/connections/discovery.rs`), including `pick_addr`.

Where the code does not support the preferred shape: a `Subscribe` cannot be
a `Downlink` twin over the shared `LinkState`, because `LinkState` is a
server. It has no dial path, and its inbound queue is command ingest, not a
stream. The client is the one genuinely new piece.

## Typing across the boundary

Three gates, each checking what only it can see.

**pyright** checks port types from the generated class, as for a local edge.

**Resolve** builds the mirror's descriptor from the peer type's manifest
entry, read the way the loader already describes a pack without creating
anything in it (`describe_raw` + `decode_pack_manifest`, `src/dl.rs`; a
wasm pack's `WasmCache` entries), or from the `Registry`'s static
descriptor for a builtin type. The descriptor keeps the entry's
*outputs*, drops the framework-appended `system_status` and `log` (the
mirror gets its own from `push_node`), drops untelemetered outputs (the peer
never announces them; a `CommandOut` cannot cross), and appends
`peer_status`. Inputs are empty. `manifest_hash` on the artifact keeps a
stale generated module from resolving (`StaleStubs`). The mirror registers
as an ordinary node at its `add` position; edges out of it resolve through
`resolve_endpoint` and `resolve_msg_edge` unchanged
(`src/wiring/resolve/endpoints.rs`).

**Connect** compares the peer's announce against those ports. For each
`VTableMsg` + `SetComponentMetadata` group the client derives the announced
instance and frame from the leading metadata name
(`<peer-ns>.<instance>.<frame>.…`). A group whose instance is the subscribed
one and whose frame names a mirror port is a candidate; the mirror port's
own announce form under the peer's prefix
(`PortDesc::announce("<peer-ns>.<instance>")`, `core/src/descriptor.rs`)
must be a subset of the announced vtable with equal
types and shapes, the rule `compatible` applies to a local edge. A match binds
the group's packet id to the port for this connection; packet ids are
per-announce-set and are remapped on every connect. A `SetMsgMetadata` whose
id equals a mirror message port's id binds when the announced schema equals
the port's `PortSchema::Postcard` schema; local edges compare ids only, but
across a build boundary the payload type may have drifted and postcard is not
self-describing, so the check is exact.

A port the announce does not cover is refused for this connection, once,
with a `peer_channel_missing` fault naming the port. A port whose schema does
not match is refused with `peer_schema_mismatch`, naming the first differing
component. The rest of the ports flow; the refused ones look like a peer that
is down, and `peer_status.dropped` counts the packets they would have taken.
This is per port because a consumer of an unaffected frame has no reason to
lose it, and the status frame is where the whole is reported.

`LinkInfo` is checked first: `protocol_version >= 2`, `namespace ==
peer.namespace`, `link == peer.link`. A mismatch closes the connection with
`peer_identity` and moves to the next candidate; a loopback port that a
different deployment happens to own is caught here rather than by data.

Nothing in the fsw bundle records what the peer *will* send. The peer's pack
manifest hash is not copied across, because two members built from one
`target.py` share their packs and `manifest_hash` already pins each member to
the module it was recorded against; the connect-time check is the one that
is true when the peer is a different build.

## Discovery and address allocation

### What a subscriber knows

Its own `Wiring` carries, on the mirror's spec:

```rust
/// Where a mirror's peer instance lives.
pub struct PeerSpec {
    /// The peer member's `coordinator.namespace`.
    pub namespace: String,
    /// The `TcpServer` state, by declaration name, the peer publishes on.
    pub link: String,
    /// That server's bound port, from its `addr`.
    pub port: u16,
    /// The peer instance's bare name.
    pub instance: String,
    /// Whether the mirror's outputs keep the type's telemetry flags
    /// (decision 1). Default `true`.
    pub telemetered: bool,
}
```

Python fills the first four from the peer target: it has the `Publish`'s state
handle and that state's `addr`. The bind address itself is not copied;
`[::]` and `0.0.0.0` are listener choices, not destinations.

### Candidates

A subscriber dials, in order, until a connection verifies:

1. the `--peer <ns>=<host[:port]>` override, when given;
2. `localhost:<port>`, both loopback families;
3. every mDNS instance of `_metor-fsw._tcp` whose TXT has `ns=<namespace>`
   and `link=<link>`, addresses picked as `pick_addr` does.

Loopback is first because a local multi-process run is the common case and a
refused connect costs nothing; a foreign server on that port fails the
identity check and the loop moves on. mDNS is last because the server does
not advertise a loopback bind (`advertise`), so a local run never depends on
it, and a LAN run needs no configuration to use it. Failed rounds back off as
the panel's loop does.

### The envelope table

The envelope gains the per-member table the shared-config document reserved:

```json
"hosts": { "plant": "10.0.0.5", "fsw": "10.0.0.6" }
```

`Deployment(targets=[…], hosts={…})` records it; it defaults to empty. The
renderer, when it lands, emits `--peer <ns>=<host>` into `member_argv` for
every subscription a member holds (it has each member's `wiring.json`). The
launcher does not read it: local means every member here, and loopback is
already first. The port stays the one in `PeerSpec`; a host entry may carry
`:<port>` to override a forwarded or NATed port. This keeps the running
document's contract: the leaf's argument list is the only thing a host needs.

Three situations, one rule:

- local multi-process: no table, loopback wins;
- packaged bundles run by hand on a LAN: no table, mDNS finds the peer by
  namespace, or `--peer` if the network does not carry multicast;
- NixOS: the table, rendered to `--peer`.

### The mDNS name

`TcpServer(name=)` stays the human instance name. Its default becomes the
namespace when the target has one, else the hostname as today. A target with
several `TcpServer` states must name all but one, checked once in `validate`
(two servers advertising one instance name on one host would fight). The
subscriber never matches on the instance name; `ns`/`link` in TXT are the
machine identity, and the same records let a panel picker group a fleet by
namespace later.

## Readiness and reconnection

A subscriber that starts before its publisher loops on candidates with the
panel's backoff (500 ms doubling to 10 s). Its consumers see `None` until the
first record and `peer_status.connected == 0`. There is no readiness gate in
the launcher, as the running document decided; readiness is the mirror's
status frame.

A publisher restart drops the socket. The client logs `peer_disconnect` once,
sets `connected = 0`, and reconnects. The new connection replays the announce
and the retained snapshot messages; the client rebuilds its packet-id map,
bumps `sessions`, and resumes. Records already in the mirror's rings stay.

A subscriber whose consumers fall behind loses batches on the *publisher*
side, counted there as `link_conn_dropped`, and never stalls either cycle. A
mirror ring that is full when the boundary exports drops on the subscriber
side, counted as `boundary dropped records` on the coordinator log as for
any async system.

Shutdown cancels the task through `AsyncContext::until_cancelled`, closing
the socket; the publisher counts one `link_disconnect`.

## IR changes

`IR_VERSION` goes from 10 to 11 in `src/ir.rs` and `metor_config/_version.py`;
`metor-config` goes to `0.4.2`. Both changes are additive and serde-defaulted,
so a v10 document differs from a v11 one only in the version field:

```rust
pub struct Deployment {
    pub ir_version: u32,
    pub targets: Vec<Wiring>,
    /// Per-member host, by namespace, for deploy rendering. Empty locally.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, String>,
}

pub struct SystemSpec {
    …
    /// `Some` marks this instance a mirror of a peer's, of the same `ty` and
    /// `artifact`; its implementation is the built-in subscriber.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerSpec>,
}
```

`validate` (`src/wiring/validate.rs`) adds: a `peer` spec has
`params == None`, `process == false`, and `attach == None`; a `peer.namespace`
is not the member's own; `peer.port != 0`; at most one `Downlink`-typed
system attaches to any one state (a second corrupts the announce replay,
today a runtime health fault, `link_announce_conflict`); advertised names
unique per member. `validate_deployment` adds: every `hosts` key names a
member, and every `peer.namespace` in any member names another member.

`WiringBuilder` gains `subscribe(name, ty, artifact, PeerSpec)` and
`publish(state, instances)` as thin spellings over the existing `system` and
`downlink` builders, so `tests/` can build a two-member deployment without
Python.

Bundles are one `Wiring` and carry `peer` specs like any other; `--check-ir`
sees them in the re-evaluated member. `path_stripped` does not touch them.
The pack ABI is unchanged; `PortDesc` did not move.

## CLI

`run` gains a repeatable `--peer <NS>=<HOST[:PORT]>`, applied in
`apply_overrides` (`src/cli.rs`) to every mirror whose `peer.namespace`
matches, in the way `--serve` edits the `TcpServer` state. Like `--serve` it
names one member's view of the world, so with several members and no
`--target` it is rejected with the same wording. `Overrides`
(`src/cli/launch.rs`) does not carry it; the launcher never needs it and the
renderer emits it per member itself.

The preflight lists a mirror as its type with a `peer` line:

```text
  • sim       Plant
              adcs-pack 0.1.0 · mirror of plant/peer
```

`docs/cli.md` gains the flag; `docs/telemetry.md` gains a "Peers" section;
`docs/wiring.md`'s deployment example gains a `Subscribe`.

## Seams for the gateway

This document assumes, and does not design, that:

- The gateway is one more member with its own namespace, embedding metor-db,
  and it is a *client* of every other member's ground link through
  `identify`/`fsw_stream` (`libs/db/src/remote/fsw.rs`). Nothing here makes
  a member dial the gateway; members keep serving, the gateway keeps dialing,
  the same direction as the panel today.
- It finds members the way a subscriber finds a publisher: `hosts` rendered
  to its argument list, else loopback, else mDNS by `ns`/`link`. The TXT
  records and `LinkInfo` v2 exist so it can label a source by namespace
  without a registry.
- `LinkInfo.command_ids` per link is how it learns what each member accepts;
  routing a command to one member rather than every member that accepts the
  id is its concern. `fsw_stream`'s forwarder tails one db's logs for one
  link and would forward the same command to every link if reused naively
  (noted by the deleted gateway design, `37b01cbf`).
- Table packet ids are per announce set and per connection; a db ingesting
  several links keys its vtable registry per connection or it collides.
  Component ids do not collide because every member is namespaced.
- The panel reaches the gateway through db-to-db mirroring (`RemoteDb`,
  `libs/db/src/remote/db.rs`), which this section does not touch.

## Alternatives considered

Contracted peer endpoints: `PeerServer`/`PeerClient` states, `Publish` with
input ports bound to ICD channels, `Consume` with contract-generated outputs,
JSON manifests, and a second wire protocol (the deleted `6ce6c60e`).
Rejected: two servers and two protocols for one kind of byte; a contract
file that duplicates what the announce replay already sends; typing that
needed a generated contract crate per interface. Its exact-check and
freshness ideas are kept.

`deploy.connect(sim.gps, ctrl.gps)` at deployment scope, no `Subscribe`
system. Rejected: an edge between two `Wiring`s has no home in either bundle,
and a member is no longer self-contained. The mirror also makes the
asynchronous boundary a visible system at a cycle position, which a
cross-member edge would hide.

A mirror typed by its consumers (mint each output from the input wired to
it). Rejected: ports would exist only when consumed, resolve would need a
descriptor pre-pass over consumers registered later, and the widest consumer
would silently define the type. The pack manifest is already the type.

Peer descriptors frozen into the subscriber's `Wiring` at build. Rejected:
a build-time copy of the announce, valid only until the peer is rebuilt, so
the connect-time check is still required, and the `WiringManifest` grows by
a vtable per port.

A subscription request so the server sends a subset per connection.
Rejected for now: `broadcast` shares one batch buffer across connections;
per-connection filtering means per-connection framing, a change to the
server's hot path for a bandwidth saving a dedicated `Publish` server already
gives.

A cyclic `Subscribe` draining a bounded queue owned by a shared client state,
the `Uplink` shape. Rejected: the async boundary already implements
snapshot/log copy and drop accounting into graph rings; a queue would
re-implement it.

`Target(host=)`. Rejected by the running document; hosts are deploy data on
the envelope.

Depending on `metor-db` for `identify`. Rejected: the fsw crate must not
pull the db in for a 40-line handshake. See open questions.

## Decisions

The review accepted the proposals above as written, with these calls:

1. Mirror outputs keep the pack's `telemetered` flags; `Subscribe(…,
   telemetered=False)` is the opt-out. Alarms and presets on the subscriber
   read through `AllOutputs`, and the re-downlinked copy is "what fsw saw".
2. `hosts` is read by the deploy renderer only. `run --target <ns>` from a
   source file does not apply it; `--peer` is the hand-run spelling.
3. `identify` and `Peer` move from `libs/db/src/remote/fsw.rs` to
   `metor-proto-stellar`, as their own commit ahead of this section.
4. No lockstep across members. The split example runs wall-clocked at
   120 Hz; a barrier protocol is later work.
5. No deployment-scope build-time link check. pyright, the recorder, and
   the connect gate are the three gates.
6. A message schema mismatch refuses that port, like a frame mismatch.
7. The `Downlink` instance-filter namespace defect is fixed in this section.
8. The adcs example's in-process closed-loop tests (`closed_loop.rs`,
   `momentum.rs`, `eclipse.rs`, `alarms.rs`, and the plant-driven cases of
   `sequences.rs` and `python_system.rs`) are deleted when the example
   splits, not reworked and not kept behind a second single-target file.
   What testing looks like across processes is a deliberate deferral; the
   tests that hold on one member stay.
9. The split example's `mode` slot no longer auto-runs `commissioning`: it
   starts empty and the sequence is started from the panel. Nothing then
   races the plant's startup or the peer's first connect.

The implementation plans are [plan-deployment-comms-1.md](plan-deployment-comms-1.md)
(the foundation: `identify` move, link identity, IR, Python API, the mirror
at resolve) and [plan-deployment-comms-2.md](plan-deployment-comms-2.md)
(the client, CLI, docs, integration, the example split).

## Testing strategy

Unit (`src/telemetry/subscribe.rs`, `src/wiring/validate.rs`, `src/ir.rs`):

- `PeerSpec` and `hosts` round-trip; a v10 document with neither field
  deserializes as before after the version bump; a spec with `peer` and
  `attach`, `process`, or params fails `validate` with its own error.
- Mirror descriptor from a manifest entry: outputs only, `system_status`,
  `log`, and untelemetered ports dropped, `peer_status` appended, port order
  stable.
- Announce matching: a peer vtable with an extra field binds (subset); a
  renamed field refuses with the differing component named; an unannounced
  port refuses once; a message id with a different schema refuses.
- Packet routing over a hand-written server in the style of `fake_fsw`
  (`libs/db/src/remote/fsw.rs` tests): identity, one table announce, one
  message announce, then packets; the mirror's rings hold the record bytes
  verbatim; an id outside the map counts in `dropped`.
- Candidate order: override alone; loopback then mDNS results; identity
  mismatch moves to the next candidate.
- `LinkInfo` v2 decodes under the v1 struct (trailing bytes ignored).
- `TelemetrySystem::configure` qualifies `instances` under a namespace.

Python (`python/tests`):

- Record-time errors: subscribing an instance of the same target; of a
  target outside the deployment; unpublished; published on two servers
  without `via=`; a published server on port `0`; a `@system` handle.
- The emitted `peer` spec for the example above, pinned in a new
  `tests/golden/deployment_comms.json` by `test_golden.py` and
  `tests/ir_contract.rs`.
- `tests/data/deployment.py` gains a `Subscribe` and a `connect` off it;
  the pyright gate proves `sub.gps` types from the generated class, and a
  deliberate cross-frame `connect` is a reported error in a sibling
  negative fixture.
- Stub output is byte-identical to today (`pack_module` tests).

Integration (`tests/comms.rs`, the `tests/launch.rs` harness, skipping
without `python3 >= 3.10`):

- `tests/fixtures/comms_target.py`: member `a` adds the `dl-fixture` producer
  and `Publish`es it on a second server; member `b` mirrors it and connects
  the fixture consumer, which logs one line per received record. `run
  comms_target.py --cycles 200` exits 0 and stderr carries `b │ …received…`.
- Start `b` alone (`--target b`) for 100 cycles: exits 0, `peer_status`
  never connected, no panic. Then `package --target a` and `--target b` and
  run the two bundles cargo-free with `b` first: data arrives.
- A test client on `b`'s ground link sees `b.<mirror>.<frame>` announced and
  streamed, proving re-downlink under the subscriber's namespace.
- Kill `a` mid-run and restart it under the launcher's bundle: `b`'s log
  shows `peer_disconnect`, then records resume; `sessions` reads 2.

Example: `examples/adcs-fsw2/target.py` becomes the two-member deployment the
running document deferred. `plant` keeps `Plant(process=True)`, its ground
link and downlink, gains a `peer` server with `Publish(peer, [plant])`, and
mirrors `ctrl` after `plant` with delayed edges for `torque_cmd` and
`mtq_cmd`, as those edges are delayed today. `fsw` mirrors `plant`, keeps
`nav`, `ctrl`, `gyro_norm` (its `plant.sensors.gyro_b` binding resolves to
the mirror through `locate_producer`), `alarms`, `presets`, `uplink`, `mode`,
and its own ground link, and publishes `ctrl` for the plant. Both members run
wall-clocked at 120 Hz. The dashboards need no edit: every component they
name (`plant.sensors.gyro_b`, `nav.attitude_estimate.q_hat_b_eci`, …) exists
in `fsw` under the mirror.

The example's tests split with it (decision 8). `bundle.rs` packages
`--target fsw`; `python_system.rs` keeps its resolve-and-run leg against
`fsw` alone; `sequences.rs` keeps the interactive load/start/abort case. The
in-process loop tests are deleted; see plan 2 for the per-file call.
