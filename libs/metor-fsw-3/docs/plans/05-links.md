# Links

The fourth slice of metor-fsw-3: telemetry out, commands in. `Publish`
serves the records a target lists over metor-proto to whoever connects
(panel, db, gateway, other targets), or pushes them to a peer it dials.
`Subscribe` receives records over a socket it dials or accepts and
writes them onto rings. The two replace the four fsw-2 concepts
(`Downlink`, `Uplink`, `Publish`, `Subscribe`). Each owns its own
sockets. Their IO runs on a background thread, behind an adapter any
system may use.

The wire format is metor-panel's and metor-db's; it is reproduced from
fsw-2, not designed here.

In scope:

- `AsyncSystem`: a system authored as one `async fn run` that paces
  itself, and `Thread`, the adapter that runs it on a background thread
  and copies rings across.
- Thread placement: async systems share one background thread unless
  placed on a named one.
- Dynamic ports: a system whose port list is fixed by the config, not the
  type, and the `PortDef` schema every port needs to be announced.
- `Publish` and `Subscribe` as built-ins, each over a listen or connect
  transport, and their Python builders.

Out of scope: `Deployment` resolution of peer addresses and frame
mirroring between targets (slice 7; `Subscribe` gains nothing there but
a resolved address and `Table` routing), shared state (designed below,
held), mDNS, per-connection filtering, auth.

## Why this shape

fsw-2 ran the link on the cycle thread as cooperative tasks. Under a
simulated clock the cycle yields once per iteration, so socket tasks got
one poll per cycle (the documented starvation), and the cycle-to-server
handoff was a bounded control queue whose overflow was a panic. It also
needed `Rc`/`RefCell` everywhere and a `ReceiveAll` capability so the
downlink could tap every ring after every cyclic system.

Rings are already thread-safe (they cross processes), and stellarator's
executor is per thread with an external waker. So the link moves to a
thread of its own. The cycle never waits on IO and IO never waits on the
cycle. What crosses is bytes, through rings, the same primitive as
everywhere else.

The background thread must not read production rings directly: a lagging
reader stalls the producer's writer, and a stalled `nav` output would
drop records for `control` because a socket was slow. Instead the
adapter mirrors each ring: the cycle thread drains the real ring and
copies into a mirror ring the background thread owns. A full mirror
drops for the link only. Loss is bounded and local; the loop never
stalls.

Every system owns mutable state and nothing else sees it; that is true
of cyclic and async systems alike and this slice does not change it.
`Publish` and `Subscribe` share nothing: each owns its listener or its
socket. What they have in common, connection slots, pending buffers,
packet framing, is a module both call. A shared-state parameter kind is
designed below for the day two systems must hold one socket, and held
until then.

Commands stay a subscription, not an uplink. A `Subscribe` is "these
records arrive on this socket". The likely shape is a `Subscribe` that
dials a gateway's db or another target's `Publish`; a `Subscribe` that
listens is direct commanding without a gateway, on its own port. The
role and the transport are orthogonal.

Background threads are pooled by name, not by system. Async systems
land on one default thread and a system that would block, or must not
be starved, is placed on a named thread of its own.

## Async systems

```rust
pub trait AsyncSystem {
    type State;
    type Inputs: SystemInputs;
    type Outputs: SystemOutputs;
    fn def() -> SystemDef;
    async fn run(
        &self,
        state: &mut Self::State,
        inputs: &mut Self::Inputs,
        outputs: &mut Self::Outputs,
        stop: Stop,
    );
}
```

`run` is called once and returns when `stop` resolves. It runs on a
stellarator executor on a background thread, so it may `await` sockets,
timers, and ring reads. Its ports are the same `Input<T>` and `Output<T>`
a cyclic system has, over mirror rings with a `Notifier` wake, so
`input.next().await` parks the thread until the cycle copies a record.
`Input` and `Output` grow a wake parameter defaulted to `NoWake`; the
cyclic path does not change.

The `#[system]` attribute accepts an `async fn run(&mut self, .., stop:
Stop)` in place of `execute`, with the same parameter kinds plus `Stop`.
Registration is `table.register_async(ty, ctor)`.

### Thread adapter

`Thread` implements `Step` and is what the table binds. It is not
generic: a system's types are erased into the `Launch` its group's thread
constructs. It owns:

- one mirror ring per input edge and one per output port, allocated at
  build from the same `PortDef` as the real ring, four times its
  capacity (`MIRROR_FACTOR`), so the adapter needs nothing from the
  coordinator config and works inside a pack unchanged;
- an `Arc<GroupHandle>` for the thread its system runs on, shared with
  every other adapter on that group;
- a `Panicked` slot the task writes into, read through an atomic flag.

Each cycle the adapter's `execute` drains every input view into its
mirror writer, then drains every output mirror view into the real output
writer. Both are byte copies; no decode. A mirror write that returns
`WouldBlock` is a drop, counted per port and reported on the system's
`log` once per `DROP_REPORT_CYCLES` cycles, carrying every port's count
since the last line (`kind = "mirror_dropped"`). The line's fields are
allocated at bind and written in place, so a dropping mirror costs the
cycle thread nothing.
The `log` output is owned by the adapter on the cycle thread; the async
side's lines arrive through the log mirror and are merged in.

A record written by the async side at time `t` is visible to the graph on
the next `execute` of the adapter, at its position in step order. Placing
a `Subscribe` first and a `Publish` last gives commands and telemetry the
same cycle they are copied.

A panic ends only its own task, which leaves the message in that
system's `Panicked` slot; the adapter reads it on its next `execute`,
writes a `panic` fault line, and latches like a cyclic panic. A
constructor that panics is caught where it runs and reaches the build as
a param error naming the system, leaving the group's other systems
running. Dropping the last adapter on a group drops the `GroupHandle`,
which sets the stop, wakes the thread, waits for it to finish, and
detaches with an error line at `JOIN_TIMEOUT`.

The adapter composes with packs without touching the ABI: a pack that
registers an async system exports `Thread<S>` as an ordinary system, and
the thread lives in the pack. The host sees `Step`.

### Thread placement

`SystemConfig` gains `thread: Option<String>`. Build groups every async
system by that name, absent meaning `"default"`, and spawns one thread
per group running one executor; each system's `run` is a task on it.
Placement is a property of the instance, set where it is added, so a
pack's system runs shared or dedicated without the pack knowing.

A task on a shared thread must not block, since the executor is
cooperative; a driver that does takes a thread of its own. A panicking
task is caught per task and latches only its own system.

A pack's async system honors `thread` like any other: the descriptor
carries the name across the ABI, and the group thread is spawned inside
the pack, one per name the host placed a system on.

## State

A system's state is its own. In the `#[system]` form it is `self`; in
the trait form it is `Self::State`. The coordinator constructs one per
instance from the instance's params and hands it to `execute` or `run`
as `&mut`. No other system can reach it, on any thread. This is
unchanged from slices 2 and 3 and applies to async systems as written
above.

### Shared state, held

Not built in this slice; kept here as the design to build when a case
needs it, such as two systems over one bi-directional socket. `TODO.md`
tracks it.

Sharing is opt-in and additive. A shared instance is declared once in
the config under its own id, constructed from its own params, has no
ports and no lifecycle, and is registered like a system:
`table.register_state::<S>("name", ctor)`. A system that wants it
adds a `&Shared<S>` parameter, where `Shared<S>` is an `Rc<RefCell<S>>`,
and its config maps that parameter to the instance id. A system may
take several; most take none. The system's own state stays private
beside them.

One rule: a state and every system naming it live on one thread. For
cyclic systems that is the cycle thread, where steps run in order and a
borrow spans one `execute`. For async systems it is one background
thread per state, where sibling tasks borrow across await-free spans.
Either way "one mutable accessor at a time" is a `RefCell` borrow that
never contends. A state named by both a cyclic and an async system is a
build error, since honoring it would take a lock. With thread placement
that means a state pins its async sharers to one named thread.

Config, when built:

```rust
struct CoordinatorConfig { clock, ring_depth, states: Vec<StateConfig>, systems }
struct StateConfig { id: String, ty: String, params: serde_json::Value }
struct SystemConfig { id, ty, params, thread, states: Vec<StateRef>, inputs, outputs }
struct StateRef { param: String, state: String }
```

A state type lives in one pack, so its sharers do; the ABI hands a
pack-constructed state to the pack's own systems as an opaque handle and
never reads it. Cross-pack sharing is not a goal.

## Dynamic ports

`Publish` takes whatever rings the config lists; `Subscribe` emits
whatever records the config lists. Neither is known to the type.

Every type computes its definition from its instance config, which a
[later slice](06-def-from-config.md) settled:

```rust
trait System { fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError>; }
```

`DefCx` carries the config's input edges with the def of the output
feeding each, the config's output ports with the record each names, and
every record the table knows. A static type ignores it. `DynInputs` and
`DynOutputs` complete themselves in their `Param` impls:

- A dynamic input port takes its `PortDef` from its edge. Exactly one
  producer per port; a second is `DefError::FanIn`, since a published
  port has one producer identity.
- A dynamic output port is listed in the system's `outputs` config with
  a record name, resolved against every `PortDef` the table knows;
  unknown or conflicting names are `DefError`s.

Bind then hands the completed definitions along with the handles:

```rust
pub struct InputBinding { pub def: PortDef, pub views: Vec<View<W>> }
pub struct OutputBinding { pub def: PortDef, pub writer: Writer<W> }
trait SystemInputs { fn bind(inputs: Vec<InputBinding>) -> Self; }
trait SystemOutputs { fn bind(outputs: Vec<OutputBinding>) -> Self; }
```

Static bundles ignore `def`. Two new bundles, `DynInputs` and
`DynOutputs`, keep the list and expose `iter()` over `(&PortDef, &mut
Input<Bytes>)`, where `Bytes` is a record whose codec is the identity.

### Port schema

Announcing a port needs more than its name. `PortDef` gains:

```rust
pub enum RecordSchema {
    Frame { vtable: VTable, metadata: Vec<ComponentMetadata> },
    Msg { id: PacketId, name: String, codec: MsgCodec },
}

pub enum MsgCodec {
    Postcard(OwnedNamedType),
    Json,
    Bytes,
    Other(String),
}
```

`Record` gains `fn schema() -> RecordSchema`. The `Frame` derive fills it
from `AsVTable` and `Metadatatize`, with leaves relative to the port.
This crosses the pack ABI inside the descriptor it already serializes,
so a pack's records announce without the host knowing their types.

Messages are not tied to postcard. The codec is the record's, as slice 2
set it, and the schema names it so the ground can decode:

- The `Record` derive is the postcard path: `MsgCodec::Postcard` with the
  type's `postcard_schema::Schema`, and `id = Msg::ID`, the hash of the
  schema name (which differs from `NAME`: `LogEvent` versus `log`).
  Existing wkt messages keep the ids the panel already matches on.
- A hand-written `Record` picks any other codec through helpers beside
  the existing `record::postcard`: `record::json` for serde JSON,
  `record::bytes` for an opaque payload, or its own encode and decode
  with `MsgCodec::Other(name)`. Their `id` is `msg_id(NAME)`, the record
  name's hash off the reserved row.

On the wire, `SetMsgMetadata` carries `MsgMetadata { name, schema,
metadata }`. For a postcard message `schema` is the type's schema. For
any other codec `schema` is the schema of `Vec<u8>` and `metadata`
carries `codec = "json"` (or `bytes`, or the `Other` name). A panel that
ignores the key shows the payload raw, as it does today for a failed
decode; a panel that reads it renders JSON as JSON. No packet format
changes.

`Subscribe` never decodes: an inbound `Msg` is routed by id and written
verbatim, and the consumer's `Record` decodes it with whichever codec it
declares.

## The link

### Transport

Both systems take a `transport` param:

```rust
enum Transport {
    Listen { addr: String, max_connections: usize },
    Connect { addr: String },
}
```

`Listen` binds in the constructor, so a taken port is a build error, and
accepts up to `max_connections` (default 8) into slots pre-allocated
there; a connection past the limit is closed on accept. `Connect` dials
in `run`, backing off from 500 ms to 10 s as fsw-2's subscriber did, and
holds one connection.

### Connections

`link/conn.rs` is the module both systems call. A `Connections` holds
one slot per allowed connection. An open slot is the send side of that
connection's `Outbox` (a `pending_cap`-byte buffer, default 1 MiB,
behind a `RefCell` with a wake) and the guard of the task that owns the
socket halves and every other buffer. `enqueue(batch)` copies a batch
into every open outbox, dropping it whole for one it does not fit and
counting the drop. The task's write half swaps the outbox out and
writes it; its read half frames inbound packets with `PacketStream` and
hands each to the closure given at `open`. A closed socket frees its
slot on `prune`, folding its byte count into the link. Counters roll up
into a `LinkStatus` frame (connections, bytes out, batches dropped,
inbound dropped) each system writes on change on its own `link_status`
output.

Buffers are allocated per accepted connection, not per slot at
construction: a cancelled task may still hold the previous occupant's
outbox, so reusing it would need a reference count check. The per-batch
path allocates nothing.

Connections arrive through `Incoming`, a task of the link's own that
accepts or dials one at a time, so the link's loop can race a batch
against a connection without cancelling an in-flight accept.

### `Publish`

An async system with dynamic inputs, a `transport`, and a `link_status`
output. Its `run`:

1. Builds the announce blob from the input defs: `LinkInfo` (id
   `[224, 61]`, protocol 2, `command_ids` empty, `namespace`, `link` =
   the system id); per frame port, `VTableMsg` then one
   `SetComponentMetadata` per component; per message record,
   `SetMsgMetadata`. Every new connection receives it first.
2. Loops: waits for any input mirror to have a record, drains every
   input, appends one packet per record to the batch buffer (`Table` with
   the table id for a frame, `Msg` with the wire id for a message, record
   bytes verbatim), and enqueues the batch. Inbound packets on a
   `Publish` connection are read and discarded, so a probing client
   never wedges the socket.

Wire names: a frame published from port `plant.imu` announces its leaves
as `{namespace}.plant.imu.{field}`, re-rooting the port-relative vtable
under that path before hashing the table id (the record-relative leaf
`imu.sample` has its record segment stripped first, so the path is not
`plant.imu.imu.sample`). Two `Publish` instances on two links announce
the same names for the same producer. `SetMsgMetadata.name` is the
postcard schema's type name for postcard records, matching the id's
hash, and the record name for other codecs.

A leaf therefore roots at `{namespace}.{producer}.{port}`, the port's
name, where fsw-2 rooted it at `{namespace}.{instance}.{frame}`, the
frame's. The two agree only where a port is named after the record it
carries: `nav.est` carrying frame `nav_est` announces
`cube_sat.nav.est.x`, not `cube_sat.nav.nav_est.x`, so a panel layout
saved against fsw-2 names has to be re-keyed. Open decision 7.

Input port names are `{producer}.{port}`; the Python builder enforces it,
the Rust side accepts any name.

### `Subscribe`

An async system with dynamic outputs, a `transport`, and a `link_status`
output. Its connection tasks copy each `Msg` packet whose id is one of
its outputs' wire ids into a bounded inbox (`inbound_cap` slots, default
256, each sized to the largest output), and its `run` drains the inbox
onto the ports, verbatim. A full inbox counts a drop. A `Frame` output
cannot be refused at construction, since port defs arrive at bind; it
faults `frame_output` on the log at start and is skipped.
Unmatched ids are ignored, not logged; the panel probes with node
protocol messages on connect. When listening it sends a `LinkInfo` with
`command_ids` = its outputs' ids and no announces, so a panel knows what
it may send. When connecting to a `Publish` or a db it takes the peer's
announce as data and ignores it; a db peer additionally needs a
`MsgStream` request per id, which slice 7 adds with address resolution.

`Table` packets are dropped and counted in this slice; slice 7 routes
them for frame mirroring.

## Python

```python
fsw = Target(cycle_rate=100.0, namespace="cube_sat")

cmds = fsw.add("cmds", Subscribe(listen="0.0.0.0:2241", records=[Arm, SetGain]))
plant = fsw.add("plant", Plant(...))
nav = fsw.add("nav", Nav(imu=plant.imu))
control = fsw.add("control", Control(est=nav.est, arm=cmds.arm))
pub = fsw.add("pub", Publish(listen="0.0.0.0:2240", items=[plant, nav.est, control]))
gps = fsw.add("gps", SerialGps(port="/dev/ttyUSB0"), thread="gps")
```

`Target.add` gains `thread: str | None`, emitted as the system's
`thread`. `Publish` and `Subscribe` take exactly one of `listen` or
`connect`. `Publish` takes system handles (every output, including
`log` and `status`) and single ports, and expands to one input per port
named `{system}.{port}`. `Subscribe` takes record classes and expands
to `outputs: [{port: <record name>, record: <record name>}]`; its handle
resolves `cmds.arm` to that port. `Publish(all=True)` expands at emit
time to every system added before it. A `Publish` handle resolves only its
own outputs (`link_status` and `log`); the ports it reads stay
addressable through their producers' handles.

`Publish` and `Subscribe` live in `metor_config` as built-ins with the
pack id `fsw`, whose empty `lib` is why `to_config().packs` omits it;
`Arm` and `SetGain` are record classes from pack modules or
`metor_config.wkt`. Generated record classes carry `_name`, the record
name the builders emit. `to_config()` fills each link's `link` (the
system id) and `namespace` params and expands `all=True` in a
finalize pass, so repeated emission is stable. `metor_config` keeps its
own copy of the host ABI number for the built-in pack, pinned by a Rust
test; an ABI bump touches it too.

## Build changes

- `thread` on a cyclic system is an error.
- Each system's definition is computed from its config, in config order,
  before `check_ids`. An undeclared port reading a system configured
  later has no producer definition to take, which is
  `BuildError::DynamicFromLater`.
- Rings for an async system's ports are allocated twice: the real ring
  and the mirror. Reader counting is unchanged; the mirror has one
  reader.
- A table entry is one closure, `MakeFn = dyn Fn(MakeCx<'_>) ->
  Result<Box<dyn Step>, ParamError>`, over `MakeCx { id, thread, def,
  params, inputs, outputs, threads }`. Build calls it and wraps the
  error; it does not know async systems exist. An async registration's
  closure allocates its mirrors, hands its member to
  `threads.get_or_spawn(thread)`, and returns the `Thread` adapter as an
  ordinary step. Groups accept members after they start, with a
  synchronous ack per add, so config order does not affect placement.
  Each adapter owns its `Arc<GroupHandle>`; the coordinator keeps only
  weak handles, so the last system on a thread dropping is what stops
  and joins it. A pack keeps its own `Threads` and honors placement in
  its own pool; the instance id and thread ride in the JSON that
  already crosses `metor_fsw_create`.
- The system is constructed on the thread that runs it, from an owned
  `serde_json::Value`, so nothing `Rc` crosses.
- Every `PortDef` in the table gains a schema, and the create payload
  gains the instance id and thread. ABI 4.
- `metor run --print-ports` prints each listening link's bound address
  as `<id> <addr>` before cycling, for tests that bind port zero. The
  address comes from a process-wide registry the link constructors fill
  by their `link` param, since the `log` line that also carries it is on
  a background thread and reaches the graph a cycle later.

## Tests

Unit (io-less):

- Dynamic input completion from one edge; `FanIn` on two edges; dynamic
  output resolution by record name; unknown and conflicting names.
- Thread placement: two async systems with no `thread` share one thread
  and a third with `thread = "own"` gets another; `thread` on a cyclic
  system is a build error; a panic on the shared thread latches one
  system and the other keeps running.
- `Thread` copy: records cross in order, a full mirror drops and counts,
  outputs surface on the next cycle, a panicked task latches, drop joins.
- Announce blob: golden bytes for one frame and one message port,
  including the re-rooted component ids and the table id.
- Batch assembly: one packet per record, framing lengths, and a batch
  that drains a stalled mirror still inside its reservation.
- Pending buffer: a batch past the cap drops whole and the next smaller
  batch still lands.

Integration (`tests/link.rs`):

- Run a target with `Publish`; connect with `metor_proto_stellar::
  identify`; assert `LinkInfo`, then vtable and metadata order, then
  records with the cycle's timestamps.
- Send an `Arm` message; a consumer after `Subscribe` sees it the same
  cycle it is copied.
- A client that never reads keeps the cycle time flat and other clients
  receiving.
- Simulated clock at full speed: no starvation, drops counted, loop
  cycle count unaffected.

Prover: none of this slice's code carries a proof; the `bounded` and
`ring_capacity` proofs slice 1 wrote cover what a link writes onto a
ring.

## Open decisions

1. Direct panel commanding uses a second port (a listening
   `Subscribe`). A single bi-directional socket is what shared state
   would buy; held until a deployment wants it over the two-port shape.
2. The default thread name is `"default"` and every async system lands
   on it. The alternative, one thread per system by default, isolates
   better and costs a thread per link; revisit if a target grows several
   IO-heavy async systems.
3. `Connect` backoff constants are fsw-2's; a `transport` param may
   carry them later.
4. `MIRROR_FACTOR` is a constant. A per-system override belongs in
   `SystemConfig` beside `thread` if a link ever needs deeper mirrors.
5. Message ids split by codec: postcard hashes the schema name, every
   other codec hashes the record name. This slice keeps the split so wkt
   messages announce under the ids the panel and db already match on.
   TODO: one id rule for all messages, fixed in metor-proto-wkt where
   the built-in matchers live, then the derive drops `Msg::ID`.
6. Retained snapshot messages (fsw-2 sent the wiring manifest on
   connect) are absent. The panel gets topology from the config file for
   now; a config-announce message can follow without changing the link.
7. Decided 2026-09-18: component ids root at the port's name,
   `{namespace}.{producer}.{port}.{field}`, not the frame's. Two ports of
   one record on one system announce as two tables, and a name says
   where its data came from. fsw-2 layouts keyed by a frame name that
   differs from its port name are re-keyed. A frame subscription in
   slice 7 references a `Publish` port, so its output takes the source
   port's name and binds the announced table id; messages stay routed
   by record id.
8. Wall stamps under a simulated clock: `link_status`, async log lines,
   and anything else an async system stamps take `Timestamp::now()`,
   while every record on the graph carries the cycle's time, so a
   simulated run's telemetry has two timelines. Either carry the cycle
   stamp across in the mirror, which the adapter knows, or document the
   split and leave link telemetry on the wall clock.
