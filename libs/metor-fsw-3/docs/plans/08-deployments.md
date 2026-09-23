# Deployments and gateway

The seventh slice of metor-fsw-3: several targets in one file, frames
and messages crossing between them, and a gateway target whose embedded
db collects every target's telemetry. Nothing new serves bytes: a
`Deployment` is a list of targets, a cross-target subscription is the
slice-4 `Subscribe` with its address and its port definitions filled in
by a build pass, and the gateway is a target like any other whose `Db`
system reads rings the way `Publish` does.

In scope:

- `Deployment` in Python, one JSON envelope holding every target, and
  the emission rules for a file with one target or several.
- `metor build`, the pass that resolves every cross-target reference
  against the packs and writes the resolved envelope, and `metor run`
  over a deployment: one target in-process, or every target as a local
  child process.
- `peer=` on a link, resolved to a `connect` address from the
  deployment's host map.
- Frame subscription: `Subscribe(peer=pub, items=[..])` takes a peer
  port's definition, routes `Table` packets by table id, and checks the
  peer's announce against what it was built with.
- A dialed `Subscribe` toward a db asks for its message streams.
- `Db`: the embedded metor-db as a built-in async system with dynamic
  inputs, behind a cargo feature.
- The fixture deployment and the multi-process tests over it.

Out of scope: the NixOS renderer (its inputs are designed below and
nothing else), mDNS or any runtime peer discovery, a `--host` override on
`run`, per-connection filtering on a `Publish`, db-to-db sync, the ADCS
example, graceful SIGTERM across children.

## Why this shape

fsw-2 ran three gates on a cross-target edge: pyright on port types, the
Python recorder on deployment shape, and a connect-time comparison of the
peer's announce against the mirror's ports. It declined a build-time
check because the connect-time one was needed anyway, and it typed a
mirror by making it an instance of the peer's own class, one socket per
mirrored system. Its gateway ingested N copies of the panel's TCP stream
straight into the db; the revision that would have fed the db from rings
was planned three times and never built.

This slice moves the check to build. A `Subscribe`'s output is a port
with a definition, and slice 6 made every definition a function of the
config; a peer port's definition is just one more thing the config can
carry. So `metor build` computes it once, from the peer's pack, and
writes it into the subscriber's config. A target then builds from its
own packs only, a missing or renamed peer port is an error before
anything runs, and the resolved file is a complete artifact for whatever
deploys it. The connect-time check stays, reduced to one comparison: a
peer whose announce disagrees with the baked definition is faulted, not
silently unrouted.

The gateway reads rings. A `Subscribe` per peer link writes the peer's
records onto the gateway's rings under the peer's own schema, because a
dynamic port clones its producer's definition and `07-qualified-schemas` rooted that
definition's names at the producer. `Db` is `Publish` with the db in
place of the sockets: dynamic inputs, one vtable insert per frame port,
one `ingest_table` per record. The db never holds a ring position; the
thread adapter's mirror bounds what a slow db costs the cycle.

Addresses are data, not discovery. A link that names a peer gets the
peer's listen port and the deployment's host for that target, baked at
build. A deployment run locally has every host at loopback, which is
also the default.

## Deployment

```python
a = Target(cycle_rate=100.0, namespace="a")
imu = a.add("imu", Imu())
est = a.add("est", Estimator(imu=imu.imu))
a_pub = a.add("pub", Publish(listen="0.0.0.0:2240", all=True))

b = Target(cycle_rate=100.0, namespace="b")
mirror = b.add("mirror", Subscribe(peer=a_pub, items=[imu, est.est]))
ctrl = b.add("ctrl", Ctrl(est=mirror.port(est.est)))
b_pub = b.add("pub", Publish(listen="0.0.0.0:2241", all=True))

gw, gw_db = gateway("gw", peers=[a_pub, b_pub], listen="0.0.0.0:2250")
cmds = a.add("cmds", Subscribe(peer=gw_db, records=[Arm]))

Deployment(targets=[a, b, gw], hosts={"a": "10.0.0.2"})
```

A target's identity in a deployment is its `namespace`: a member with
none, or two with one, is a `ConfigError` at emit. `hosts` maps a
namespace to the address peers dial it at; a target absent from it is at
`127.0.0.1`. `Target` does not change; a target neither knows nor names
the deployment it is in.

Emission: a file with one `Deployment` emits it. A file with one bare
`Target` and no `Deployment` emits a deployment of one, so every config
file has one shape. Two `Target`s without a `Deployment`, two
`Deployment`s, or a `Deployment` that omits a `Target` the file built
are errors.

`gateway(namespace, peers, listen, cycle_rate=10.0, path=None)` is a
function in `metor_config` that builds a `Target`, adds one
`Subscribe(peer=p)` per peer named after the peer's namespace, and a
`Db(listen=listen, all=True)` on thread `"db"`, and returns the target
and the db's handle. A gateway with anything else in it is written by
hand from the same three calls.

### Envelope

```json
{
  "config_version": 2,
  "hosts": { "a": "10.0.0.2" },
  "targets": [
    { "packs": [..], "coordinator": { "namespace": "a", .. } },
    ..
  ]
}
```

`DeploymentConfig { config_version, hosts: BTreeMap<String, String>,
targets: Vec<TargetConfig> }`; `TargetConfig` loses `config_version`
and is otherwise the slice-3 file. Each target keeps its own pack list,
so a target is extracted by index and run without reading its siblings.
`config_version` goes to 2; a version-1 file is refused with the
existing message.

## Cross-target references

Python emits references. `metor build` resolves them. The coordinator
refuses an unresolved one.

### What Python emits

A link's transport gains a third form beside `listen` and `connect`:

```json
"transport": { "peer": { "target": "a", "link": "pub" } }
```

`Subscribe(peer=pub)` and `Publish(peer=sub)` take a handle of a link on
any member, including the same target. `peer` is one of the three
transport keywords; exactly one is given. Since a link handle must exist
before it is named, a target that dials another must be built after it
in the file, and a mutual pair dials in one direction and listens in the
other, which is the shape the wire has anyway.

A `Subscribe` with a `peer` takes `items` (frame ports and system
handles, as `Publish` does) and `records` (message classes, as today).
No `items` and no `records` means everything the peer publishes, which
is what a gateway wants. Every copied port keeps its `{system}.{port}`
name. A message record arrives on one port however many ports the peer
publishes it on, since the wire carries only its id: the first listed
wins, and `items=[imu, nav]` on a peer with `log` on both yields one
`imu.log`, on which `nav`'s lines also arrive. (A copy named after the
record alone would collide with the subscriber's own `log`.) The
generated module carries what that needs: each system class maps its
output ports to record classes, and a record class says whether it is a
frame or a message.

```json
"outputs": [
  { "port": "est.est", "peer": { "target": "a", "link": "pub", "port": "est.est" } },
  { "port": "imu.log", "peer": { "target": "a", "link": "pub", "port": "imu.log" } },
  { "port": "arm", "record": "arm" }
]
```

Every item, frame or message, is a `peer` reference the build pass
bakes, so a subscriber needs none of the peer's packs. `records=` keeps
today's form, resolved against the subscriber's own table, and is what
a subscription to a `Db` takes, since a db publishes no ports.

`SystemHandle.port(source: OutPort[T]) -> OutPort[T]` on a `Subscribe`
handle returns this subscriber's copy of a port it was given, typed by
the source, so pyright checks `Ctrl(est=mirror.port(est.est))` with no
generated code for the subscriber. For a message source it returns the
one port that record arrives on. A source that is not among the
subscriber's items is a `ConfigError`. Attribute access stays for the
subscriber's own ports (`link_status`, `log`).

Python checks what it can see: the peer is a member of this deployment,
the peer link listens, its port is fixed (port 0 is refused), every
item is published by the peer link, and two listening links of one host
do not share a port.

### The build pass

`metor build` and `metor run` share `deploy::resolve(&mut envelope,
&tables)`, where `tables` is one `SystemTable` per target with its packs
registered. Resolution is one pass over every system of every target:

- A `peer` transport becomes `connect { addr }`: the peer link's
  listen port under `hosts[target]`, or loopback.
- A `peer` output becomes `def { .. }`: the `PortDef` of the peer link's
  input port with that name, renamed to this port. The peer link's def
  comes from an index that computes a system's definition on demand,
  memoized per `(target, system)`, chasing edges to producers and peer
  references to other targets, with a visiting set that turns a cycle
  (a mirror re-published to a mirror of it) into an error.
- A `Db` gains `command_ids`: the message ids of every output of every
  `Subscribe` whose peer it is, so the panel learns what it may send
  through the gateway.

Every reference is checked as it is resolved: the target, link, and
port exist, the link listens on a fixed port, and no two listeners on
one host bind one port. Errors name the reference.

`OutputConfig` becomes `{ port, source }` with `source` flattened:

```rust
pub enum OutputSource { Record(String), Peer(PeerPort), Def(PortDef) }
```

`dyn_output_defs` maps `Record` through the table as today, `Def`
straight through under the config's name, and `Peer` to
`DefError::Unresolved { port }`, whose message says to run `metor
build`. `Transport::bind` refuses `Peer` the same way. Since the
resolved form is only ever produced by the pass, a target running from a
resolved envelope opens no pack but its own.

`build.rs`'s `instance_def` is what the index calls per system; it moves
to where both can reach it and takes the producer lookup as a closure.
The coordinator's own config-order resolve is unchanged.

### Routing and the connect-time check

`Subscribe::routes` stops faulting a frame output: a frame port routes
`Table` packets whose id is `table_id(schema.vtable)`, which is the id
the peer's `Publish` sends under because both hold the same vtable. The
inbox accepts `Table` bodies for those ids beside `Msg` bodies and the
`max_len` bound covers both.

On a dialed connection the read half watches the announce: each
`VTableMsg` id is recorded, and on the first `Table` or `Msg` packet
every frame output whose id was not announced is faulted once,
`peer_schema_mismatch`, naming the port. A peer rebuilt with a changed
frame is therefore reported, not silently unrouted. Message ids are name
hashes and need no check.

A dialed `Subscribe` sends one `MsgStream { id }` per message output
after the peer's identity arrives. A `Publish` peer reads and discards
it; a db peer starts forwarding that message log from its live edge.
The `MsgStream` request rides the existing seed mechanism: a dialer's
seed is the requests, a listener's is its `LinkInfo`.

## Gateway

`Db` is a built-in async system in `link/db.rs`, registered as
`fsw.db` when the `db` cargo feature is on, which the `metor` binary
turns on and the crate defaults to. A pack build without the feature
does not link metor-db.

```rust
pub struct DbParams {
    pub transport: Transport,        // listen only; connect is a ParamError
    pub path: Option<PathBuf>,       // default: a fresh temp dir
    pub namespace: Option<String>,
    pub link: String,
    pub command_ids: Vec<PacketId>,  // filled by resolve
}
```

`Db::new` binds the listener and opens or creates the store with
`Server::from_listener`, so a taken port or unwritable path fails the
build. `run`:

1. For each frame input def, `insert_vtable` and one
   `set_component_metadata` per component; for each message def,
   `set_msg_metadata`. Then `set_identity(namespace, command_ids)`.
2. Spawns `server.run()` and `lod::spawn` under drop guards.
3. Loops on `inputs.any_ready()`: a frame record is `ingest_table(id,
   bytes)`, a message record is `push_msg(now, id, bytes)`. A refused
   record counts.
4. Writes `db_status { records, refused }` on change.

The system takes `items` and `all=True` exactly as `Publish` does; a
gateway that also runs its own systems records them beside the mirrors.
Component names in the db are the peers' own (`a.imu.imu.accel`), since
the mirror's definitions carry the peers' roots.

`ingest_table` writes files, so `Db` belongs on its own thread; the
`gateway` helper places it there and a hand-built gateway should. Table
ids are 16-bit hashes; the collision odds fsw-2 computed apply per
gateway, and a colliding pair is refused by `insert_vtable` and faulted
by the `Db`.

## Running

`metor run <target.py | deploy.json> [--target NS] [--cycles N]
[--wall RATE | --sim-dt SECS] [--print-ports]`:

- A `.py` is evaluated as today and resolved; a `.json` is read and
  resolved, which is a no-op on a resolved file and otherwise needs
  every target's packs.
- One target in the envelope, or `--target`, runs in-process as today.
- Several targets without `--target` run as children: the parent writes
  the resolved envelope to a temp file and spawns `metor run <file>
  --target NS` per target with the clock and cycle flags forwarded,
  stderr piped and drained by one thread per child, each line prefixed
  `NS │ `. The first child to exit nonzero kills the rest; the parent's
  status is nonzero if any child's was. SIGINT reaches every child
  through the process group. `--print-ports` needs one target.
- `metor build <target.py> [-o deploy.json]` evaluates, resolves, and
  writes; stdout by default.

`load` refreshes dev packs and opens each target's packs once, sharing
a `Pack` across the tables of targets that name it.

## Rendering to an OS

Not built; the design says what the envelope has to carry so it can be.

A renderer is a function of the resolved envelope alone. Per host it
lists the targets `hosts` places there, and per target it needs: the
`metor` binary, the envelope's path, `--target NS`, the ports its
listening links bind (read from the resolved transports, for a
firewall), and the pack libraries its `packs` name. Only the last is
not deployable as written: `libs` is an absolute `.metor` path from a
dev build, which the packaging arc's built wheels replace with a
per-triple directory the renderer maps to a store path.

Because the envelope is JSON, a NixOS module can consume it with
`builtins.fromJSON` and emit one systemd unit per target on that host,
with no Python and no Rust in the loop; `Deployment(hosts=)` is the
whole placement surface, and a target file never imports anything Nix
knows. A `Host` type richer than a string (users, paths, a store
override per pack) widens `hosts` when a renderer needs it.

## Tests

Python (`tests/test_deployment.py`):

- Golden envelope for the three-target file above.
- Emission: one bare target is a one-member envelope identical to a
  `Deployment` of it; two bare targets, two deployments, and an omitted
  member are errors; a member without a namespace and two with one are
  errors.
- References: `peer=` to a link of a non-member, to a link that dials,
  and to a port-0 listener are errors; `items` naming a port the peer
  link does not publish is an error; an `items` handle splits into frame
  ports and one message port per record; no items means everything;
  `port()` on a non-item is an error and on an item returns the
  subscriber's port; two listeners on one host with one port is an
  error.

Rust unit (io-less):

- `resolve`: a peer output becomes the producer's def under the new
  name; a peer transport becomes the host's address; a `Db` collects
  command ids from its subscribers; a missing target, link, or port
  names itself in the error; a mirror-of-a-mirror cycle is an error; a
  resolved envelope is a fixpoint.
- `dyn_output_defs`: `Def` passes through, `Peer` is `Unresolved`.
- `Subscribe::routes` binds a frame port to its table id; `deliver`
  routes a `Table` body to it; a `Msg` body still routes by record id;
  the mismatch check faults once and only for unannounced frame ports.
- The dialer's seed carries one `MsgStream` per message output.
- `DeploymentConfig` version 1 and 3 are refused.

Integration:

- `tests/deploy.rs`: the fixture deployment run through `metor run`
  with `--cycles`; both children exit zero; the gateway's db answers a
  component query for a frame produced on `a` and mirrored through `b`,
  and a message log for `b`'s pings.
- `metor run deploy.json --target b` on the resolved envelope with the
  other targets' `libs` pointed at a directory that does not exist,
  which proves a resolved target opens only its own packs.
- `metor build` on the fixture equals the golden resolved envelope, up
  to the pack paths.
- A `Subscribe` dialed at a `Publish` announcing a different vtable for
  a baked port faults `peer_schema_mismatch` and keeps routing the
  others.

The fixture pack gains a frame record (`Sample`, a timestamp and one
`f64`) and a system producing one per cycle, so a frame can cross
targets; the deployment file is `tests/fixtures/echo-pack/deploy.py`.

## Shipped deviations

- Copied message ports keep their `{system}.{port}` name rather than the
  record's, as above; the first port of a record wins and the rest of
  that record's ports are not copied.
- The coordinator's `status` port was rooted at plain `status`, so every
  system's status table shared one id and a gateway could hold one. It
  now roots at `{namespace}.{id}.status` like every other port.
- A mirrored frame keeps its origin's names when re-published, so a
  gateway subscribing to both a producer and a mirror of it holds one
  table, fed twice; the second feed's out-of-order stamps are refused
  and counted.
- Test fixtures build against `metor-fsw-3` with `default-features =
  false`, so a pack never links the db.
- The announce watch counts only records as data: a peer's `LinkInfo`,
  vtable, and metadata messages end no announce. The first version
  counted the identity, so the check ran the instant a mirror connected
  and reported nothing when a peer announced first and sent later.

## Open decisions

1. Hosts bake into the envelope at build. A `--host NS=ADDR` override on
   `run` and `build`, fsw-2's `--peer`, is a later addition; loopback
   default covers local runs.
2. Messages from a peer are routed by record id, so a subscription is
   per record, not per producing port, and two of a peer's systems
   producing one message type arrive on one port. This is the wire's
   `Msg` packet, not a choice here; the one-id-rule TODO in `05-links`
   is where a per-port message identity would come from.
3. `Db` timestamps messages with the wall clock, as every async system
   does; under a simulated clock its message log and its tables carry
   different timelines (`05-links`, decision 8).
4. The build pass computes definitions by chasing edges on demand,
   while the coordinator computes them in config order. Two walks of
   the same rule, in one function; if the coordinator's order rule
   (`DynamicFromLater`) ever relaxes, the on-demand index can replace
   it.
5. A gateway that also commands its peers is two links per peer: its
   `Subscribe` from the peer's `Publish`, and the peer's `Subscribe`
   from its `Db`. A single bi-directional socket is the shared-state
   case `05-links` holds.
