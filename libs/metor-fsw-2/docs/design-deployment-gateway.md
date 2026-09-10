# Deployments: gateway

A deployment is a set of targets that are configured, built, and run
together. [Shared configuration](design-deployment-shared-config.md) made one
`target.py` emit a `Deployment { ir_version, targets: [Wiring, …], hosts }`
envelope keyed by `coordinator.namespace`; [running](design-deployment-running.md)
launches every member as a child process from its own bundle;
[comms](design-deployment-comms.md) lets members publish to and subscribe from
each other over the telemetry link. This document designs the last part of
[the rough design](rough-deployment-design.md): the gateway, one metor-fsw
instance with metor-db embedded that every member's ground link streams into
and that metor-panel, or any other ground system, connects to as a db.

## Design

metor-db already ingests an fsw link (`identify`/`fsw_stream`,
`libs/db/src/remote/fsw.rs`) and already mirrors another db (`RemoteDb`,
`libs/db/src/remote/db.rs`). metor-panel is a client of both
(`libs/metor-panel/src/connections/target.rs`). The gateway is the place where
the first is done N times into one db and the second is what the panel does to
it. Four decisions follow:

1. **The gateway is a member.** It is one more `Target` in the deployment with
   its own namespace, a built-in `Db` state that owns an embedded
   `metor_db::Server`, a built-in `Ingest` system that dials every named
   member's ground link into that db, and a built-in `Record` system that
   writes the gateway's own telemetry into the same db. The launcher spawns
   it, `package --target gw` bundles it, and a NixOS unit runs it, all through
   `member_argv` unchanged.
2. **Ingest is N copies of what the panel does today.** One task per member,
   dialing the way a `Subscribe` dials (loopback, `--peer`, mDNS by
   `ns`/`link`), checking `LinkInfo` the way a `Subscribe` checks it, then
   `fsw_stream` into the shared db. The one defect on that path, table
   packet ids numbered per announce set so two links collide in one
   registry, is fixed at its source: a table's id becomes a hash of its
   announced vtable, the rule message ids already follow.
3. **The panel connects to the gateway as a db.** `identify` answers
   `DbInfoResp`, the picker's bare-address backend hands off to `RemoteDb`,
   history hydrates through `PeerStore`. Two gaps in db-to-db sync are closed
   in the db crate: the real-time mirror carries message logs, and the mirror
   forwards the commands the remote advertises. The panel changes nothing
   about how it connects.
4. **Commands route by a config-static set.** Each ingest source carries the
   command tokens it forwards, copied from the member's `Uplink` at record
   time. Sets are disjoint across one gateway's sources, checked once at
   validate, so the existing per-link forwarder never sends one command to
   two members.

## Goals

- `metor-fsw run target.py` with a gateway member starts it beside the rest;
  the panel connects to one address and sees every member's components,
  logs, alarms, sequences, presets, and wiring manifest under its namespace.
- A command sent from the panel reaches the one member that accepts it.
- A member restarting, starting late, or never starting is visible in the
  db as a per-source status frame and never stalls the gateway or the member.
- Everything reused is reused as it is: `LinkState`, `Downlink`'s tap and
  framing, `identify`, `fsw_stream`, `Server`, `RemoteDb`, `PeerStore`,
  `Hydrator`, the launcher, the bundle.
- No IR change. The gateway is ordinary states and systems with ordinary
  params.

## Non-goals

- Rolling retention that deletes the only copy of data. metor-db's tiering
  offloads-then-purges and never touches an unacked span
  (`libs/db/src/tiering.rs`); the gateway inherits that. See "Storage".
- Namespace-keyed folds of message-typed snapshots in the panel (`AlarmDefs`,
  `SequenceRegistry`, `PresetDefs`, `WiringManifest` are latest-wins per
  message id, `libs/metor-panel/src/wiring/mod.rs`). Pre-existing for any two
  producers into one db; see "Decisions", 3.
- Explicit per-command addressing on the wire. Deferred until the panel can
  say which member a command is for.
- Auth, encryption, gateway federation, a gateway ingesting another gateway.
- A gateway-specific panel backend, URL scheme, or handshake.

### The prior attempt

`37b01cbf` checkpointed a gateway (`libs/db/src/gateway/unified.rs`,
`src/deployment/gateway.rs`, five design documents) and was deleted in
`2bcb7084`. It got the shape right: metor-db embedded in an ordinary member,
the panel served through the normal db protocol and db-to-db sync, every
message log synced, vtables keyed per connection, commands routed by owner
rather than by type, payloads never rewritten. It got the amount wrong: a
second deployment model (instances, placements, `provides`/`requires`, IR
v3), a second Python API (`d.gateway(…)`, `d.listen`, `d.panel`), a
`SourceIdentity` message with boot sessions and a `SourceCatalog` of
per-source db partitions with writer leases (1,100 lines) to solve a
packet-id collision, a `gateway://` scheme with a custom panel backend, and
per-target command allow-lists. Its commit message says the three-process
deployment never ran. This design keeps the shape and deletes the layers.

## What the gateway is

Three options were on the table.

**(a) A member with built-ins.** The db's needs are a listening socket, a
directory, an async runtime for its accept loop, and a thread-safe `DB`.
`Server::run` (`libs/db/src/lib.rs`) spawns one stellarator thread per accepted
connection (`struc_con::stellar`) and shares an `Arc<DB>` guarded by a
`RwLock`; `LinkState::start` already spawns its accept loop from
`SharedLifecycle::start` on the coordinator's runtime
(`src/telemetry/link/mod.rs`). A `Db` state does the same with a `Server`.
Nothing about the db wants a coordinator cycle, and nothing about a
coordinator minds a state that owns threads. The coordinator cycle is a cheap
tick that drains the gateway's own status into the db.

**(b) A separate `metor-db` process the deployment points at.** metor-db's
binary (`libs/db/src/main.rs`) serves but never dials: it has no member set,
no candidate rule, no identity check, no forwarder set. Every one of those
would be a config file a second process reads, a second `ExecStart` the
renderer emits, and a second log prefix the launcher does not know. It is
option (a) with the member's coordinator replaced by a config parser.

**(c) A CLI mode.** `metor-fsw gateway …` is (b) inside the same binary: a
second entry point the launcher and renderer must special-case, and no
bundle.

(a) is confirmed. The cost is that the host crate `metor-fsw-2` depends on
`metor-db`, and with it datafusion and arrow. The host crate already carries
`wasmi`, `metor-expr`, and the pack build driver; the core authoring crate
(`metor-fsw-2-core`) stays db-free, and packs depend on core. See
"Decisions", 1.

## Python API

```python
from metor_config import Db, Deployment, Downlink, Ingest, Record, Target, TcpServer, Uplink
from adcs_pack import Fsw, Plant

plant = Target(cycle_rate=120.0, namespace="plant")
plant_link = plant.state("link", TcpServer(addr="[::]:2240"))
plant.add("plant", Plant(seed=42))
plant.add("downlink", Downlink(plant_link))

fsw = Target(cycle_rate=120.0, namespace="fsw")
fsw_link = fsw.state("link", TcpServer(addr="[::]:2241"))
fsw.add("uplink", Uplink(fsw_link, msgs=["SequenceCommand", "AlarmAck", "ReloadSequences"]))
fsw.add("fsw", Fsw())
fsw.add("downlink", Downlink(fsw_link))

gw = Target(cycle_rate=120.0, namespace="gw")
db = gw.state("db", Db(addr="[::]:2250"))
gw.add("plant", Ingest(db, plant_link))
gw.add("fsw", Ingest(db, fsw_link))
gw.add("record", Record(db))

deploy = Deployment(targets=[plant, fsw, gw])
```

`Db(addr, path=None, name=None)` is a state, declared with `Target.state`
like `TcpServer`. `addr` is what the panel dials; `path` is the data
directory, a fresh temp dir per run when omitted; `name` is the mDNS instance
name, defaulting to the namespace as `TcpServer(name=)` does after the comms
section.

`Ingest(db, source, commands=None)` attaches to the `Db` state and takes one
member's ground-link **state handle**; one `Ingest` per member, named by
the gateway's author, the way one `Subscribe` mirrors one instance. A
`StateHandle` gains `target`, set by `Target.state`, the way `SystemHandle`
gained it for `Subscribe`. A handle names exactly the `(namespace, link,
port)` triple the source dials; there is no "the ground link is the one with
the unfiltered downlink" rule to get wrong. `commands` narrows the tokens
forwarded up this link; `None` is the member's whole set, `[]` is none. A
source's command tokens are the `msgs` of the `Uplink` systems attached to
that link on that member, in config order. (A single `Ingest` over a list
was the first draft; a frame's component names come from the frame's
`NAME`, not the port name, so one instance cannot publish a status frame
per source. One instance per source gives `gw.plant.source_status` for
free.)

`Record(db)` attaches to the `Db` state and stores the gateway's own outputs:
the coordinator's `system_status`/`log`, each system's `system_status`/`log`,
and each `Ingest`'s status frame, all under `gw.*`. It taps the same rings
the `Downlink` taps and writes them into the db directly; see "Reuse".

Resolution of a source happens in `Deployment.__init__`, beside
`_resolve_peer` (`python/metor-config/metor_config/_deployment.py`), because
it needs the member in scope:

- the handle's target is a member of this deployment and is not the gateway;
- the handle is a `TcpServer` state with a fixed port (port `0` is rejected
  as it is for a published server);
- the tokens copied into `commands` are the attached `Uplink`s' `msgs`;
- across the gateway's sources the command sets are disjoint; the error names
  the two members and the token and suggests `commands=[…]`.

A deployment without a gateway does nothing new. A member does not opt in;
the gateway's author names its link, and the member serves the gateway what
it serves any ground client. What crosses is what the member's `Downlink`
taps.

## The embedded db

`src/gateway/mod.rs` (new, host crate) registers a `gateway_pack` in
`Registry::with_builtins` beside `link_pack` (`src/wiring/registry.rs`):

```rust
gateway_pack.shared_state("Db", |ctx: StateCtx<'_>, p: DbParams| {
    DbState::open(p.path, p.addr).map(|s| s.with_identity(ctx.name, ctx.namespace, p.name))
});
```

`DbState::open` binds the listener and opens or creates the `DB` at
construction (`Server::from_listener` does both), so a taken port or an
unwritable path is the state's construction error at resolve, as for
`LinkState::bind`. `path == None` resolves to
`std::env::temp_dir().join("metor-gw-<ns>-<random>")`, the `serve_tmp_db`
precedent.

`SharedLifecycle::start` spawns `Server::run` and `metor_db::lod::spawn` on
the coordinator's runtime, as `main.rs` does for the standalone db, and
advertises over mDNS. `shutdown` drops the accept guard and the advertiser;
`DB::flush` runs on drop as today. Attached cyclic systems receive the scoped
`&mut DbState` grant; the `Arc<DB>` inside is what the ingest tasks clone.

The db server's per-connection threads and the coordinator's single-threaded
runtime coexist because `DB` is `Send + Sync` and every write is under its
lock. The coordinator's loop task is never blocked by a panel query: queries
run on the connection's own thread.

## Ingest

### Params

```rust
/// Wiring params of the built-in ingest (`type="Ingest"`, attached to a
/// `Db`): one member's ground link, as the gateway dials it.
pub struct IngestParams {
    /// The member's `coordinator.namespace`.
    pub namespace: String,
    /// Its `TcpServer` state, by declaration name.
    pub link: String,
    /// That server's port, from its `addr`.
    pub port: u16,
    /// `NamedMsg::NAME` tokens forwarded up this link; a subset of the
    /// member's advertised set.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Where the member runs, when `--peer <ns>=<host>` named it.
    #[serde(default)]
    pub host: Option<String>,
}
```

The first three fields and `host` are `PeerSpec`'s (`src/ir.rs`) without
`instance`; the candidate rule and the identity check are shared with the
subscriber by factoring `direct_candidates`, `browse`, and the
`LinkInfo` check out of `src/telemetry/subscribe.rs` into free functions
over `(namespace, link, port, host)`. `apply_overrides` (`src/cli.rs`) gains
one arm: `--peer <ns>=<host[:port]>` sets `host`/`port` on every `Ingest`
whose `namespace` matches, beside the mirror arm. The renderer emits
`--peer` for the gateway from the same `hosts` table it emits it for
subscribers.

`Ingest` is a cyclic system attached to the `Db` state
(`system_type_shared::<IngestSystem, DbState>("Ingest", …)`), not an
`AsyncSystem`: shared state is cyclic-only (`core/src/shared.rs`), and the
ingest writes into the db, not into graph rings. Its `configure` resolves the
command tokens against `ctx.msgs` as `UplinkSystem::configure` does and pushes
them into the db state (`DbState::add_commands`), before `start` serves the
first `GetDbInfo`. Its `init` spawns the client task, held as a drop guard,
the `LinkState::start` shape. Its `execute` folds the task's atomics into one
`SourceStatus` frame, `gw.<instance>.source_status`.

```rust
#[metor_fsw(name = "source_status")]
pub struct SourceStatus {
    timestamp: Timestamp,
    /// 1 while a verified connection is live.
    connected: u64,
    /// Connections established over the run; a member restart is +1.
    sessions: u64,
    /// Packets ingested over the run.
    packets: u64,
    /// Packets the db refused (schema mismatch, unknown table id).
    rejected: u64,
}
```

### One task per source

Candidates in order: `host` when set, both loopback families on `port`, then
one bounded mDNS round for `ns=<namespace>`/`link=<link>`. Each candidate:
`identify`; a `Peer::Db` or a `LinkInfo` whose `namespace`/`link` differ is
refused with a `source_identity` fault and the next candidate is tried; a
match runs `fsw_stream(command_ids, rx, tx, buf, &db)` until the socket
drops, then backs off from 500 ms doubling to 10 s, the panel's pair.

`fsw_stream`'s signature changes in one place: the forwarded set becomes an
argument instead of `info.command_ids`. The panel passes the advertised set as
today; the gateway passes the configured set intersected with the advertised
one, and logs a `source_commands` fault once for any configured token the
member no longer advertises.

### Table ids are content hashes

A `Downlink` numbers its table announces `0, 1, 2, …` per announce set
(`TelemetrySystem::init`, `src/telemetry/mod.rs` line 249), and the db's
`insert_vtable` keys its registry by that id (`State::vtable_registry`,
`libs/db/src/lib.rs`). Two links into one db both announce id `0`; the
second overwrites the first and `ingest_table` decodes one member's records
with the other's vtable. Two producers dialing into the panel's sandbox db
collide the same way today.

Messages never had this problem because a message id is a function of its
schema: `msg_id` (`libs/metor-proto/src/types.rs`) is the 16-bit FNV-1a
XOR-fold of the schema name, with the reserved protocol row `[224, _]`
remapped to `[223, _]`. Tables get the same rule:

```rust
/// A table's packet id: the fold of its announced vtable's postcard bytes,
/// off the reserved row by the same remap `msg_id` applies.
pub fn table_id(vtable: &VTable) -> PacketId
```

lives beside `msg_id` in `metor_proto::types`, callable from the fsw crate
and the db. The remap moves into one `fn off_reserved([u8; 2]) -> PacketId`
that both call; it is not duplicated. The hashed bytes are the serialized
**prefixed** vtable, the one `RegistryEntry::announce` returns
(`PortDesc::announce`, `core/src/descriptor.rs`), not the leading metadata
name. Three reasons. The prefixed vtable already carries the qualified
component ids in its fields, so `plant.plant.gps` and `fsw.plant.gps` hash
differently with nothing added. The id then identifies the wire schema, not
the name: a member rebuilt with a changed frame layout announces under a
new id and registers beside the old one, and a name-keyed id would make
that rebuild a refused re-announce until the gateway restarted. And equal
bytes give equal ids, which is what makes a reconnect replay a no-op.

`PacketTy` in the packet header keeps table and message ids in separate
spaces, so a table id on the `[224, _]` row would decode correctly; the
remap is applied anyway so no tool must check the packet type before
trusting that the reserved row is never data. No other range is reserved
(`PacketTy`, `types.rs`; the retired ids in `wkt/src/msgs.rs` are all on
`[224, _]`). The blanket `Msg::ID` impl skips the remap today while `msg_id`
applies it; that is pre-existing and untouched here.

Collision policy in a 16-bit space:

- `TelemetrySystem::init` detects two of its own taps hashing equal and
  refuses the later one with a `telemetry_table_id_collision` fault naming
  both entries. The vtables are static, so this fires on the first run of a
  configuration or never; the fix is a rename.
- `DB::insert_vtable` refuses an id already registered with a **different**
  vtable, `Error::VTableConflict(id)`, instead of overwriting. An identical
  re-announce, a reconnect replay, stays a no-op. `fsw_stream` warns and
  continues as it does on any bad packet, so the source's `rejected` counts
  every table under that id and the fault is visible in the gateway's log
  and in `gw.<instance>.source_status`.

Exposure: n tables across one db draw from 65,280 ids (65,536 less the
reserved row, whose fold lands on `[223, _]`). Fifty tables collide with
probability about 2%, a hundred about 7%, three hundred about half. The
refuse-on-differ rule turns that into a loud, deterministic failure on the
first run rather than one member's records silently decoded through
another's schema, which is what the sequence numbers produce today.

For existing consumers: the panel and the db key their registries by id per
connection today and now simply see ids that are stable across
connections and links. The subscribe client's per-connection `Routes.tables`
map (`src/telemetry/subscribe.rs`) becomes unnecessary and stays harmless.
`Announce::Table { packet_id }` keeps its field and computes it. Tests
pinning `[0, 0]`/`[1, 0]` table ids (`TICK_ID` in `subscribe.rs`, the link
tests) change to `table_id(&vtable)`. This extends the project's natural-id
rule from messages to tables; there is no per-connection map anywhere.

Message logs are keyed by message id, not per connection, and that is right:
`LogEvent.source` is the qualified instance name, so two members' log lines
merge into one `LogEvent` log the panel already filters by source. Snapshot
messages (`AlarmDefs`, `SequenceRegistry`, `PresetDefs`, `WiringManifest`)
also merge, one record per member per publish, and the panel's latest-wins
folds keep whichever member published last. That is the pre-existing gap
named under non-goals; the adcs example publishes all four from `fsw` only.

### Down, late, restarted

Before a member answers, its components do not exist in the db and
`gw.<instance>.source_status.connected` is `0`. After it drops, its components keep their
last records with their original timestamps; nothing is zeroed. On reconnect
the announce replay re-announces the same hashed ids, `insert_vtable`
no-ops on the identical vtable, `insert_component` accepts an unchanged
schema, and the retained snapshots replay as new records, which the
latest-wins folds absorb. A member rebuilt with a changed frame layout
announces under a new id and fails `insert_component`'s schema check on the
unchanged component id; `fsw_stream` warns and continues per packet as
today, and the source counts `rejected`. Whatever a member sent while the gateway was away is gone: the
link has no retransmission ([telemetry.md](telemetry.md), "Bounded loss"),
and the db shows the gap.

Backpressure follows the link's rules. A gateway that falls behind loses whole
batches on the member's side, counted in the member's `link_status.dropped`;
the member's cycle never waits. The gateway's ingest is one `handle_packet`
per packet under the db lock, and its own cycle does no ingest work.

Logs and alarms cost nothing extra: they are message taps on the member's
downlink and arrive through the same stream.

## Commanding

A command is a record in a db message log. The panel's stores push commands
into the panel's local db (`inspector/edits.rs`, `alarms/mod.rs`,
`sequences/mod.rs`) and `fsw_stream`'s forwarder tails those logs from the
live edge and sends each record up the link (`forward`,
`libs/db/src/remote/fsw.rs`). The gateway keeps that shape at both hops:

```text
panel db ──(mirror connection)──▶ gateway db ──(link, per source)──▶ member
   forward(gateway.command_ids)       forward(source.commands)
```

**Panel to gateway.** `RemoteDb::mirror` races a `forward` over the mirror
connection for the ids the gateway advertises in `DbInfoResp.command_ids`
(below). The gateway's `handle_packet` records an unmatched `Msg` into its log
as it records any (`libs/db/src/lib.rs`, the last `Packet::Msg` arm). `forward`
moves from `remote/fsw.rs` to `remote/mod.rs` and is shared.

**Gateway to member.** One `forward` per source over its link, for that
source's configured `commands`, the existing `fsw_stream` arm.

**No fan-out.** The comms document noted that reusing the forwarder naively
sends one command to every link that accepts the id. Disjoint command sets
across a gateway's sources, checked at record time and at
`validate_deployment`, make "the links that accept it" exactly one link. Two
strings that both accept `AlarmAck` cannot both be commanded through one
gateway until commands carry a destination; `commands=[…]` picks one now,
and decision 2 states the path.

**No echo.** A command recorded in the gateway's log must not ride the
gateway's message stream back to the panel, where the panel's forwarder
would send it again. The rule at both ends is the same: an id in a link's
advertised command set is a request going up that link and never a record
coming down it. The db's message stream skips `State.command_ids`; a member's
uplink ports are untelemetered already (`UplinkSystem::instance_descriptor`).

## Serving the panel

### db-to-db sync gains messages and commands

`RemoteDb::mirror` today sends `Stream { RealTime }` and the server streams
components only (`handle_real_time_stream`): message logs never cross, so a
panel mirroring the gateway would see frames and no logs, alarms, sequences,
presets, or wiring manifest. The prior attempt saw this too. Two additions
in `libs/db`, gated on a feature bit so old peers are unaffected:

- `DbInfoResp` gains `command_ids: Vec<PacketId>` and
  `namespace: Option<String>`, serde-defaulted; `NODE_PROTOCOL_VERSION` goes
  to 3; feature bit 1 is `MSG_SYNC`. postcard ignores trailing bytes, so a
  v2 reader decodes a v3 reply, the `LinkInfo` v2 precedent. A plain metor-db
  advertises no commands and no namespace.
- `SyncMsgs` (`[224, 63]`, in `NODE_PROTOCOL_MESSAGES`). A mirror that sees
  the bit sends it after `Stream`; the server spawns one tail per message log
  not in its command set. Each tail sends the log's latest record first, the
  retained-snapshot equivalent of what a link replays to a late joiner, then
  every record its WAL reader sees, with `msg_with_timestamp` so the source
  time survives. The reader is taken before `latest` is read, so nothing
  falls between. New logs join as `metadata_gen` moves. The receiver's
  unmatched-`Msg` arm records them, timestamp preserved.

The mirror then reads `command_ids` and races `forward` as above. An old
panel against a gateway mirrors components and forwards nothing; a new panel
against an old db mirrors components and never sends `SyncMsgs`. Old servers
record unknown requests as telemetry, which is why the request is gated and
not sent blind.

### Discovery and the picker

`DbState::start` calls `discovery::advertise` (`src/telemetry/discovery.rs`)
with the instance name, the db address, the namespace, the state name, and
`role=gateway`. `advertise` gains the role argument; `TXT_ROLE` was reserved
for exactly this (`libs/metor-proto/wkt/src/msgs.rs`). The service type stays
`_metor-fsw._tcp`, so the panel's one browse finds it.

The panel's browse upserts `ConnectionTarget::tcp(name, addr)`
(`libs/metor-panel/src/connections/discovery.rs`); its `DiscoverBackend`
identifies a `Peer::Db` and hands off to `RemoteDb`. The one panel change is
cosmetic: `detail` reads the `role` TXT record, so the row reads
`10.0.0.7:2250 · gateway` where a db reads `· metor-db`. Everything else the
gateway needs from the panel lands in `metor_db::remote`.

A panel connected to the gateway sees a per-member view because the view is
per namespace already: `plant.*`, `fsw.*`, and `gw.*` are three prefixes in
one component tree, and `gw.<instance>.source_status` says whether each is live. There is
no merged target to invent.

History hydrates through `PeerStore` against the gateway's sealed nodes, and
LoD series seed from its manifests, because the gateway runs `lod::spawn`
like the standalone db and the panel's db mirror reports
`local_authority: false`.

## Storage

`Db(path=)` is config-static, in the state params, like `TcpServer(addr=)`.
Omitted, it is a temp dir the OS reclaims, which is what a local run wants
and what the launcher's temp bundles already rely on. A file that declares
`Db(path="/var/lib/metor/gw")` writes there whoever runs it; that is the
deployable form, and a NixOS unit's `StateDirectory` matches it. No flag
overrides it; see "Decisions", 6.

Retention is metor-db's: `Db(store=<dir>, max_bytes=, max_age=)` maps onto
`TieringConfig` with a `LocalDirStore` (`libs/db/src/store/local_dir.rs`),
spawned from `start` beside the server. Without a store nothing is evicted,
because tiering only purges a span another copy holds. A rolling window that
deletes the only copy is one policy flag on tiering when someone needs it,
not a gateway feature; see "Decisions", 5.

## Local run and deploy

`metor-fsw run target.py` with the example above spawns three children in
envelope order; the gateway's position is irrelevant. Its sources start
dialing at `init`, find nothing, and back off; the members' links accept
whenever they are up. `run --target gw` runs the gateway alone, retrying
against empty loopback ports, as a lone subscriber does. `package --target gw`
writes a bundle with no artifacts. The preflight lists `db`, `ingest`, and
`record` as builtin.

Clock overrides apply to the gateway as to every member (running document,
decision 5). `--cycles N` ends it after N of its own cycles, so the example
gives the gateway the members' rate and the run ends together; a cycle costs
the gateway nothing but a status drain. `--sim-dt` makes its loop yield
instead of sleep, a busy loop for a test run, accepted for uniformity.

Under the renderer the gateway is one more unit:
`metor-fsw run /nix/store/…-gw.bundle --target gw --peer plant=10.0.0.5 --peer fsw=10.0.0.6`,
`StateDirectory` equal to `Db(path=)`. The panel dials the gateway host on
the `Db` port. Its identity on the wire is `DbInfoResp { namespace: "gw",
command_ids }` and, on the link, the `role=gateway`/`ns=gw` TXT records.

`--serve` on the gateway member declares a `TcpServer` and an all-taps
`Downlink` beside the db, as on any target that has none. Harmless and
unneeded.

## IR changes

None. `Db` is a `StateSpec` with value params; `Ingest` and `Record` are
`SystemSpec`s with `attach` naming it. `IR_VERSION` stays 11. `metor-config`
goes to 0.4.3 for the three builders and `StateHandle.target`.

`validate` (`src/wiring/validate.rs`) adds: at most one `Record` per `Db`
state (the `check_downlinks` rule, same reason). A `Db` address that does not
bind is the state's construction error, as for `TcpServer`.
`validate_deployment` adds, beside `check_hosts_and_peers`: every `Ingest`
names a member other than its own; that member declares the named
`TcpServer` with that port; its `commands` are tokens the member's attached
`Uplink`s list; command sets across one gateway's `Ingest`s are disjoint. The
params decode through `IngestParams`, the struct the factory decodes.

## Reuse

Verbatim: `Server`, `DB`, `handle_packet`, `lod`, `tiering`, `PeerStore`,
`Hydrator`, `identify`, `fsw_stream`'s ingest arm, the panel's backends and
picker, `member_argv` and the launcher, `write_bundle`, `LinkState` and the
downlink's framing. Extended, each named in its section above:
`metor_proto::types` (`table_id`, the shared remap), `TelemetrySystem::init`
(hashed ids, the collision fault), `DB::insert_vtable` (refuse on differ),
`DbInfoResp`, `RemoteDb::mirror`, `fsw_stream`, `discovery::advertise`,
`apply_overrides`, the two validators, and `subscribe.rs`'s candidate and
identity functions.

`Record` and `Downlink` are two systems over shared free functions, not one
struct with two sinks. Two pieces of `TelemetrySystem` move out of the impl
into `src/telemetry/taps.rs`:

- `collect_taps(all: &AllOutputs, mode: &TelemetryMode) -> Taps`: the
  `init` loop that filters entries, claims one `View` per tapped ring, takes
  `entry.announce()` for the vtable and metadata (now under `table_id`),
  collects each message port's `SetMsgMetadata` once per id, and records
  each tap's delivery mode and wire kind. About sixty lines.
- `drain_taps(taps, on_record: impl FnMut(&Tap, &[u8]))`: the per-cycle
  walk. A snapshot tap contributes its newest record only when `committed`
  moved; a log tap contributes every record through `drain_view`; a corrupt
  read is returned for the caller to fault. About forty lines.

What stays in `TelemetrySystem`: `set_announces` and `set_retained_slots`
on the link, `append_record`/`append_packet` framing, the batch buffer and
`PENDING_CAP` policy, `link_status`, and the retained-slot copy for late
joiners. A db keeps every record and has no late joiner, so none of it
applies to `Record`.

`RecordSystem` (`src/gateway/record.rs`, about a hundred lines) is a cyclic
system attached to `DbState` whose only port bundle is `AllOutputs`, so it
carries `ReceiveAll` and resolve defers it to the tail beside a `Downlink`.
`init` calls `collect_taps` with the all-taps mode and registers each table
once, `insert_vtable(VTableMsg { id: table_id(&vt), vtable: vt })` plus
`set_component_metadata` per field, and each message id once,
`set_msg_metadata`. `execute` calls `drain_taps` and, per record,
`ingest_table(id, bytes)` for a table tap or `split_record` then
`push_msg(now, id, payload)` for a message tap. No packet is framed and none
is parsed; the ring bytes are the record `ingest_table` reads its timestamp
from. It publishes nothing but its own `system_status` and `log`, which its
own taps carry from the next cycle.

New, in `src/gateway/`: `DbState`, `DbParams`, `IngestSystem`,
`IngestParams`, `SourceStatus`, the per-source task, and
`RecordSystem`; about the size of `subscribe.rs` minus its ring routing. In
`libs/db`: the message stream and the shared forwarder, about the size of
`remote/fsw.rs`.

## Alternatives considered

A separate `metor-db` process or a CLI mode. Rejected under "What the gateway
is".

Ingest as a `Subscribe` of every member instance into gateway rings, then a
`Downlink` into the db. Rejected: a mirror is typed by one instance's pack
descriptor and cannot mirror a `@system` instance or the coordinator's own
ports; the gateway wants everything a member announces, which is what
`fsw_stream` ingests with no descriptor. The db is the right client.

Per-source db partitions with a catalog and writer leases (the prior
attempt), or a per-connection `PacketId → PacketId` map in the db's
`ConnState` (this document's first draft). Rejected: both work around ids
that collide instead of giving tables ids that do not, and the project
already settled that rule for messages.

`Record` as `TelemetrySystem` over a `DownlinkSink` trait implemented by
`LinkState` and `DbState`, the db decoding the batch it is handed. Rejected:
encode-then-decode inside one process, retained slots and a connection
gauge that mean nothing to a db, and a trait for exactly two implementors
when the shared part is a hundred lines of free functions.

Route commands by decoding them (`AlarmAck.def_id` to the member whose
`AlarmDefs` listed it). Rejected for now: a per-type table that grows with
every command message, for an ambiguity the panel cannot express either.
Forwarding to every accepting link is the defect the comms document named.

Self-ingest: the gateway declares a `TcpServer` and dials itself on
loopback. Rejected: a second port for a link nobody else needs and an
announce replay through a socket for records already in the process; the
ring taps are the same records one call away.

A new `_metor-db._tcp` service type. Rejected: the panel browses one type and
`TXT_ROLE` exists so one type can carry siblings.

Full message-log history on mirror connect. Rejected for now:
latest-then-live is what a link gives a late joiner; ranges are one
`GetMsgs` away when a Logs pane wants backfill.

## Decisions

The review accepted the proposals above as written, with these calls:

1. `metor-db` is a plain dependency of the host crate. A `sql` feature on
   `metor-db` gating datafusion and arrow is a later size lever, not a
   config split.
2. Command overlap across a gateway's sources is rejected at validate;
   `commands=[…]` narrows. A destination on the wire and namespace-keyed
   panel folds are a later section.
3. Snapshot messages from several members colliding in the panel's
   latest-wins folds is out of scope; the panel's connection module doc
   records the assumption. `WiringManifest` carries its namespace;
   `SequenceRegistry` would need one.
4. The db's own real-time stream uses `table_id` on the vtable it builds
   instead of `fastrand::u16`, so its ids are deterministic and collide
   only where a hash collides.
5. No rolling retention here. A `purge_unacked` policy on `TieringConfig`
   when a long-running gateway needs a byte ceiling without an archive.
6. No storage-path override for the renderer. `Db(path=)` in the file is
   the deployable form; `--db-path` is the `--serve` precedent if a host
   layout demands it.
7. The gateway's cycle rate is the user's; the example matches the
   members' 120 Hz so `--cycles` ends together.
8. Message history on mirror connect is latest-then-live; `GetMsgs`
   backfill when the Logs pane asks for it.
9. Of two taps hashing equal, the `Downlink` refuses the later in registry
   order; the fault names both.
10. Deferred: tracing writes synchronously on the coordinator thread, so a
    slow stderr consumer stalls the member behind it. The launcher's own
    pump is fast, but any parent that pipes a run's stderr without draining
    it fills the 64 KB pipe and blocks the next log line mid-cycle, which is
    how `tests/gateway.rs` flaked. A non-blocking or lossy stderr writer
    would decouple the member's cycle from whoever is reading it.
11. Deferred: the timer under a ground client's runtime misbehaves both
    ways. It has been seen spinning at 100% for a long sleep on maitake's
    1 ns clock (`Sleep::poll` → `Timer::advance_locked` →
    `wheel::Core::turn_to`), and seen wedged the other way: `Executor::run`
    passes `None` to `wait_for_io` whenever `try_turn` reports no next
    deadline, so a client whose only pending work is a 50 ms sleep sits in
    `kevent` at 0.1% CPU until an unrelated socket wakes it — about a minute
    in `tests/gateway.rs`, which is the residue of the flake that draining
    stderr does not remove. The coordinator's own cycle is unaffected; both
    belong to an investigation in `libs/stellarator` and `maitake`.

The implementation plans are
[plan-deployment-gateway-1.md](plan-deployment-gateway-1.md) (the
substrate: table ids, db-to-db message sync and forwarding, the tap
functions, link identity) and
[plan-deployment-gateway-2.md](plan-deployment-gateway-2.md) (the member:
`Db`, `Ingest`, `Record`, Python, validation, tests, the example).

## Testing strategy

Unit (`libs/metor-proto`):

- `table_id`: the same vtable hashes the same; a prefixed vtable hashes
  differently from its bare form and from another instance's prefix; a
  vtable whose fold lands on `[224, _]` comes back on `[223, _]`, through
  the function `msg_id` also calls.

Unit (`libs/db`):

- Two `fake_fsw` servers announcing different vtables into one db: both
  components decode under distinct ids. The same vtable announced twice, a
  reconnect replay, is a no-op. A different vtable under an already
  registered id is refused with `VTableConflict` and the registered one is
  intact.
- `SyncMsgs` against a db with two logs, one in the command set: the other
  log's latest record first, then live records with source timestamps; the
  command log never streams; a log created after the request joins.
- `forward` over a mirror: a record pushed after connect lands in the remote
  log, one pushed before does not replay, a round trip never echoes.
- `DbInfoResp` v3 decodes under the v2 struct; `SyncMsgs` is sent only when
  bit 1 is set; `fsw_stream` forwards exactly its explicit set.

Unit (`src/gateway`, `src/wiring`):

- `IngestParams` round-trip; `--peer` sets one source's host and port and
  leaves the rest; the "names no peer" error mentions ingest sources.
- `validate_deployment`: a source naming the gateway itself, a missing
  member, a wrong link or port, a token the member's uplink does not list,
  overlapping command sets. `validate`: two `Record`s on one `Db`.
- `configure` pushes the resolved command set into the db state; two
  `Ingest`s on one `Db` union their sets.
- `TelemetrySystem::init` with two taps built to hash equal: the later is
  refused with `telemetry_table_id_collision` naming both; the announce set
  carries hashed ids and no `[0, 0]`.
- `collect_taps` over a `WiringBuilder` target yields the entries the
  `Downlink` tapped before the split, byte-identical announces aside from
  the ids; `drain_taps` hands a snapshot tap's newest record once per
  change and a log tap's records in order.
- `Record` over a `WiringBuilder` target: after one cycle the db holds
  `gw.coordinator.system_status` with the cycle timestamp and the gateway's
  `LogEvent` lines; a second cycle with no new snapshot writes nothing to
  that component; the vtables registered once, under `table_id`.
- The per-source task against `fake_fsw`: an identity mismatch moves on, a
  match ingests, a dropped socket bumps `sessions` and reconnects.

Python: `Ingest` record-time errors (a state handle of the gateway itself,
of a target outside the deployment, a non-`TcpServer` state, port `0`,
overlapping command sets without `commands=`); the emitted `sources` for the
example, pinned in `tests/golden/deployment_gateway.json` by `test_golden.py`
and `tests/ir_contract.rs`; `StateHandle.target` set by `Target.state`.

Integration (`tests/gateway.rs`, the `tests/launch.rs` and `tests/comms.rs`
harness, skipping without `python3 >= 3.10`):

- `tests/fixtures/gateway_target.py`: member `a` runs the `dl-fixture`
  producer with a ground link and `Uplink(msgs=["AlarmAck"])`; member `b`
  runs the same with `Uplink(msgs=["ReloadSequences"])`; member `gw` ingests
  both. `run gateway_target.py` under the launcher; the test opens a
  `RemoteDb` mirror from a temp db to the gateway's port and waits until
  `a.<producer>.*` and `b.<producer>.*` components and both members'
  `LogEvent` lines are present locally, then asserts `gw.a.source_status` and
  `gw.b.source_status` read `connected == 1`.
- The test pushes one `ReloadSequences` into its temp db. Direct clients on
  `a`'s and `b`'s links, dialed as `tests/comms.rs::dial` does, count
  `WiringManifest` re-broadcasts: `b` re-emits one, `a` none. This proves
  the two-hop forward and the no-fan-out rule with the coordinator's existing
  reload behaviour as the observable.
- Kill `a` and restart it from its bundle: `gw.a.source_status.sessions` reads 2 and
  `a`'s components resume in the mirror.
- `run … --target gw --cycles 200` alone exits 0 with `connected == 0` on
  both sources and no panic; `package --target gw` and a cargo-free run of
  the three bundles pass the first case again.

Example: `examples/adcs-fsw2/target.py` gains the `gw` member above on
`[::]:2250`; `plant_link` and `fsw_link` are its sources; `fsw`'s uplink set is
the forwarded set. The README's "Watch it live" section becomes one address:
the panel connects to `gw` and the picker shows it as a gateway; the plant
and fsw links stay reachable directly for diagnostics. `bundle.rs` adds
`package --target gw`; the dashboards need no edit because every path they
name exists under `fsw.*` as before.
