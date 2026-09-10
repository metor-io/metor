# Plan: deployments, gateway (2 of 2: the member)

Implements the member half of
[design-deployment-gateway.md](design-deployment-gateway.md), on top of
[plan 1](plan-deployment-gateway-1.md). Six work packages. Each ends with a
green tree. At the end a three-member fixture runs under the launcher with
a gateway a test db mirrors, and the adcs example has a `gw` member the
panel connects to on one address. Paths are relative to `libs/metor-fsw-2`
unless they start with `libs/`, `examples/`, or `python/`.

Test commands used throughout:

```sh
cargo test -p metor-fsw-2                               # WP1–WP3, WP5
cargo clippy -p metor-fsw-2 --all-targets               # lints, house rule
cargo test -p metor-fsw-2 --lib gateway                 # WP1–WP3
cargo test -p metor-fsw-2 --lib cli                     # WP2
cargo test -p metor-fsw-2 --test ir_contract --test py_eval  # WP4
(cd python && uv run python -m unittest discover tests) # WP4; pyright optional
cargo test -p metor-fsw-2 --test gateway                # WP5
cargo test -p adcs-fsw2                                 # WP6 (skip w/o python3)
cargo check -p metor-panel                              # WP1 (feature unification)
```

## Sequencing

```text
WP1 ─┬─> WP2 ──┐
     ├─> WP3 ──┼─> WP5 ─> WP6
     └─> WP4 ──┘
```

WP1 (the dependency and the `Db` state) is the root. WP2 (`Ingest`, CLI,
validation), WP3 (`Record`), and WP4 (Python) touch disjoint files and run
in parallel after it; WP4 emits names WP2 and WP3 register, so all three
must land before WP5 (integration and docs). WP6 (the example) needs WP5's
proof.

## Deviations from the design doc

Found while grounding the plan. The design doc was corrected where noted.

1. A frame port's component names come from the frame's `NAME`
   (`Metadatatize::metadata`, `libs/metor-component/src/metadata.rs:6`;
   `prefix_vtable`, `core/src/descriptor.rs:382`), not from the port name,
   so one `Ingest` instance cannot mint a distinct `source_status` per
   source. The design's `Ingest(db, [links])` becomes one `Ingest(db, link)`
   per member, named by the author: `gw.plant.source_status`. `IngestParams`
   is the flat source, no `SourceSpec` list. Design doc corrected.
2. The design had `validate` check that a `Db` state's `addr` parses.
   `TcpServer`'s address is not validated there either; the state factory
   binds at construction and reports through `LoadError::StateInit`
   (`src/wiring/validate.rs:331`, `check_state`). One gate; the rule is
   dropped. Design doc corrected.
3. `print_preflight` lists systems and slots, not states
   (`src/cli/ui.rs:115–190`), so `db` never appears; `Ingest` and `Record`
   read `builtin`. `system_detail` (`ui.rs:213–225`) appends
   ` · mirror of <ns>/<link>` for a mirror; an `Ingest` appends
   ` · ingests <ns>/<link>` the same way.
4. `Server::run` spawns the UDP loop and each connection on its own
   stellarator thread through `struc_con::stellar` and drops the `Thread`
   handle (`libs/db/src/lib.rs:995–1007`; `Thread` has no `Drop` that
   cancels, `libs/stellarator/src/struc_con.rs:15`). `DbState::shutdown`
   drops the accept task's guard, so no new connection is accepted; live
   panel connections end when the process exits. Accepted; a leaf exits.
5. `ReloadSequences` reaches the coordinator only through an explicit
   `route(uplink, coordinator, msg="ReloadSequences")` edge
   (`src/coordinator/mod.rs:334–351`; `examples/adcs-fsw2/target.py:418`).
   The integration fixture declares it on `b`.
6. The launch fixtures bind `127.0.0.1:2250–2252` and the comms fixture
   `2253–2255`; the gateway fixture takes `2256–2258`. The adcs example's
   `gw` keeps `[::]:2250` from the design; the two never run in one
   process.
7. `Target.add` captures `spec._param_source()` at add time
   (`python/metor-config/metor_config/_target.py:235`), before a
   `Deployment` exists to resolve the peer's port. `_Ingest` is recorded in
   a `_ingests` map beside `_subscribes` and `_system_ir` folds its
   resolved params in at emission, exactly as it folds `peer`
   (`_target.py:417–420`).

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| The host crate depends on `stellarator` with no features; `metor-db` asks for `["miette", "tokio"]` | `Cargo.toml:39`, `libs/db/Cargo.toml` | cargo unifies; `cargo check -p metor-panel` proves nothing else moved |
| `metor-db` costs the binary 5.6x debug and 7.9x release: `target/debug/metor-fsw` goes 51,126,280 -> 286,594,472 bytes and `target/release/metor-fsw` 11,463,168 -> 90,129,776; `cargo clean -p metor-fsw-2` + rebuild goes 4.4 s -> 5.1 s debug and 9.7 s -> 7.5 s release, and the one build that compiles metor-db, datafusion, and arrow costs 44 s debug / 86 s release | WP1, measured either side of the dependency | the `sql` feature of "Decisions", 1 is the lever if the size ever matters |
| `Registry::with_builtins` registers `link_pack` through `Pack::shared_state` + `system_type_shared` and `register_pack` | `src/wiring/registry.rs:224–263,302` | `gateway_pack` sits beside it |
| `system_type_shared` runs `ctor(params, token)` then `system.configure(&BuildCtx { msgs, namespace })`, and requires `T: CyclicSystem` | `core/src/pack.rs:369–430` | `IngestSystem::configure` sees its `Shared<DbState>` and can push command ids before `start` |
| `Shared::get()` is a scoped `RefCell` borrow; `SharedLifecycle::start` runs before the first attached init on the loop task | `core/src/shared.rs:21,40` | `DbState::start` spawns the server the way `LinkState::start` spawns the accept loop |
| `Server::from_listener(listener, path)` opens or creates the `DB`; `Server::run(self)` consumes it; `lod::spawn(db)`, `tiering::spawn(db, store, config)`, `LocalDirStore::new(root)` | `libs/db/src/lib.rs:984,995`, `lod.rs:106`, `tiering.rs:48`, `store/local_dir.rs:25` | `DbState` holds `Option<Server>` until `start` |
| `serve_tmp_db` names its temp dir `metor_db_<random>` under `std::env::temp_dir()` | `libs/db/src/lib.rs:1019` | the `path=None` default copies it |
| `advertise(name, addr, namespace, link, role)` after plan 1 WP6; skipped on loopback | `src/telemetry/discovery.rs:27` | `DbState::start` passes `"gateway"` and the state name as `link` |
| `SubscribeOut` binds dynamic frame writers with `try_next_output` and writes raw bytes with `Writer::try_write` | `src/telemetry/subscribe.rs:98–115,570` | not needed: one `Ingest` has one static `Output<SourceStatus>` |
| `LinkState::start` holds its accept task as a `JoinHandleDropGuard` in an `Option` | `src/telemetry/link/mod.rs:389` | the ingest task and the db accept task use the same guard |
| `direct_candidates`, `browse`, `check_identity` are `pub(crate)` after plan 1 WP6; `fsw_stream(command_ids, ..)` after WP5 | plan 1 | the ingest task is those plus `fsw_stream` |
| `apply_overrides` handles `--peer` by matching `spec.peer.namespace` and errs with the mirror list when nothing matched | `src/cli.rs:775–805` | the `Ingest` arm sits in the same loop |
| `check_hosts_and_peers` walks every member's systems for `peer` | `src/wiring/validate.rs:78–101` | the `Ingest` checks go in the same walk |
| `LoadError` variants render with `#[error(..)]`; `PeerSpec { system, reason }` is the one-variant-many-reasons shape | `src/wiring/error.rs:279–307` | `IngestSpec { system, reason }` copies it |
| `TCP_SERVER_TYPE`, `DOWNLINK_TYPE`, `UPLINK_TYPE` are `pub const` in `src/ir.rs:170–182` | | `DB_TYPE`, `INGEST_TYPE`, `RECORD_TYPE` join them |
| `WiringBuilder` has `state_value`, `system(..).attach(..).end()`, `serve`, `uplink`, `publish`, `subscribe` | `src/wiring/builder.rs:146,241,268,289,298,324,519` | tests build a gateway target without Python |
| `AllOutputs` is a `Capability::ReceiveAll` grant; `TelemetryPorts { status, all }` derives `SystemOutput` | `core/src/registry.rs:145–172`, `src/telemetry/mod.rs:132` | `RecordPorts { all: AllOutputs }` is a valid bundle and gets the tail position |
| `StateHandle(name, spec)`; `Target.state` calls `spec._bind(self)` | `_model.py:148`, `_target.py:174–190` | `target` is one more constructor argument |
| `Deployment.__init__` resolves `_subscribes` via `_resolve_peer`; `_state`, `_port` helpers exist | `_deployment.py` | `_resolve_ingest` sits beside them and reuses both |
| `metor-config` is `0.4.2`; `IR_VERSION = 11` | `_version.py`, `pyproject.toml` | `0.4.3`; the IR version stays |
| `test_golden.py` pins `tests/golden/deployment.json`; `ir_contract.rs::golden_deployment_round_trips` pins it from Rust | `python/tests/test_golden.py:75,249`, `tests/ir_contract.rs:432` | `deployment_gateway.json` gets one test in each |
| `tests/comms.rs` has `have_python`, `fixtures`, `fsw`, `run`, `connected`, `kill`, `dial` | `tests/comms.rs:26–103` | `tests/gateway.rs` copies the harness |
| `bundle.rs` selects `fsw` and packages it; `build_sim_coordinator` selects `fsw` | `examples/adcs-fsw2/tests/bundle.rs:51`, `src/lib.rs:50` | `gw` is packaged beside `fsw`; the sim coordinator is untouched |

---

## WP1: the dependency and the `Db` state

Files:

- `Cargo.toml`: `metor-db = { path = "../db" }` under a `# gateway` comment.
  Before adding it: `cargo clean && time cargo build --release -p metor-fsw-2`
  and `ls -la target/release/metor-fsw`; after: the same two commands.
  Record both times and both sizes in this file's "Verified facts" table
  in the same commit, replacing the "before" row above.
- `src/gateway/mod.rs` (new), `mod gateway;` in `src/lib.rs` beside
  `mod telemetry;`, re-exporting `DbParams`, `DbState`:

  ```rust
  /// Wiring params of the built-in embedded db (`state type="Db"`).
  pub struct DbParams { pub addr: SocketAddr, pub path: Option<PathBuf>, pub name: Option<String>, pub store: Option<PathBuf>, pub max_bytes: Option<u64>, pub max_age_secs: Option<f64> }
  /// The pack-shared embedded db: bound and opened at construction, served from `start`.
  pub struct DbState { server: Option<Server>, db: Arc<DB>, local_addr: SocketAddr, name: Option<String>, link: String, namespace: Option<String>, tiering: Option<(PathBuf, TieringConfig)>, accept_guard: Option<JoinHandleDropGuard<()>>, advertiser: Option<ServiceDaemon> }
  impl DbState {
      pub fn open(params: DbParams) -> Result<Self, std::io::Error>
      pub fn with_identity(self, link: &str, namespace: Option<&str>, name: Option<String>) -> Self
      pub fn db(&self) -> &Arc<DB>
      /// Add forwarded command ids to what `GetDbInfo` advertises; runs from `Ingest::configure`, before `start`.
      pub(crate) fn add_commands(&mut self, ids: &[PacketId])
  }
  impl SharedLifecycle for DbState { fn start(&mut self); fn shutdown(&mut self); }
  ```

  `open` binds `TcpListener::bind(addr)`, resolves `path` (`None` →
  `temp_dir().join(format!("metor-gw-{random}"))`), and calls
  `Server::from_listener`. `start` calls `db.set_identity(namespace, ..)`
  with the accumulated commands, spawns `server.run()` as a drop guard,
  `lod::spawn`, `tiering::spawn` when `store` is set, and
  `advertise(name.or(namespace).unwrap_or(hostname), local_addr, namespace, link, "gateway")`.
  `shutdown` drops the guard and shuts the advertiser down, the
  `LinkState` shape.
- `src/ir.rs`: `pub const DB_TYPE: &str = "Db";` beside `TCP_SERVER_TYPE`.
- `src/wiring/registry.rs`, `with_builtins`: `gateway_pack.shared_state("Db", |ctx, p: DbParams| DbState::open(p).map(|s| s.with_identity(ctx.name, ctx.namespace, p.name)))`
  and `register_pack(gateway_pack)`.
- `src/wiring/builder.rs`: `pub fn db(mut self, name: &str, addr: SocketAddr) -> Self` over `state_value`.

Tests to add (`src/gateway/mod.rs` test module):

- `db_state_serves_identify`: `DbState::open` on `127.0.0.1:0`, `start`,
  `identify(local_addr)` yields `Peer::Db` with `namespace` and empty
  `command_ids`; `shutdown`; the temp dir exists.
- `db_state_reports_a_taken_port`: bind twice; the second `open` errs.
- A `WiringBuilder` target with `.db("db", ..)` and no systems resolves
  and `run_for(5)` ends with nothing stopped.

What could go wrong: `metor-db` pulls datafusion; the first build is
minutes, and the size delta is the point of the measurement. `Server::run`
spawns the UDP loop on the same address; two gateways on one host need two
addresses, as two links do. `DbState` is `!Send` (it holds a
`ServiceDaemon` and a guard), which shared state permits. `DB::set_identity`
must run before the accept task can answer a probe: call it in `start`
before spawning.

Green when:

```sh
cargo test -p metor-fsw-2 --lib gateway && cargo clippy -p metor-fsw-2 --all-targets && cargo check -p metor-panel
```

## WP2: `Ingest`, `--peer`, validation

Files:

- `src/gateway/ingest.rs` (new), re-exported from `src/gateway/mod.rs` and
  `src/lib.rs`:

  ```rust
  /// Wiring params of the built-in ingest (`type="Ingest"`, attached to a `Db`): one member's ground link.
  pub struct IngestParams { pub namespace: String, pub link: String, pub port: u16, pub commands: Vec<String>, pub host: Option<String> }
  #[metor_fsw(name = "source_status")] pub struct SourceStatus { timestamp, connected, sessions, packets, rejected }
  #[derive(SystemOutput)] pub struct IngestOut { status: Output<SourceStatus> }
  pub struct IngestSystem { db: Option<Shared<DbState>>, params: IngestParams, commands: Vec<PacketId>, counters: Rc<SourceCounters>, task: Option<JoinHandleDropGuard<()>>, last: SourceStatus }
  impl BuildSystem for IngestSystem { type Params = IngestParams; fn new(p) -> Self; fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError> }
  impl System for IngestSystem { type Input = (); type Output = Out<IngestOut>; const NAME = "ingest"; fn init(&mut self, output) }
  impl CyclicSystem for IngestSystem { fn execute(..) }
  /// The client: candidates, identity, `fsw_stream`, backoff; runs until dropped.
  async fn source_loop(params: IngestParams, commands: Vec<PacketId>, db: Arc<DB>, counters: Rc<SourceCounters>)
  ```

  `configure` resolves `commands` tokens through `ctx.msgs` as
  `UplinkSystem::configure` does (`UnknownMsg` on a miss) and calls
  `self.db.get().add_commands(&ids)`. `init` clones the `Arc<DB>` out of
  the state and spawns `source_loop` as a drop guard. `source_loop`:
  `direct_candidates(host, port)` then `browse(namespace, link, ..)`,
  `identify`, `check_identity` (a `Peer::Db` is a `source_identity` reject),
  intersect the advertised `command_ids` with `commands` (a missing token
  bumps a `commands_missing` counter once), `fsw_stream(set, rx, tx, buf, &db)`,
  backoff 500 ms doubling to 10 s. `execute` folds `counters` into a
  `SourceStatus`, publishes on change, and logs `source_identity`,
  `source_disconnect`, `source_commands` fault lines from counters the task
  bumped since the last cycle. The task has no `AsyncContext`; it is
  cancelled by dropping the guard at `shutdown`, the `LinkState` shape.
- `src/ir.rs`: `INGEST_TYPE = "Ingest"`.
- `src/wiring/registry.rs`: `gateway_pack.system_type_shared::<IngestSystem, DbState>("Ingest", |p, db| IngestSystem::new(p).attach(db))`.
- `src/cli.rs`, `apply_overrides`: inside the `--peer` loop, for every
  system with `ty == Some(INGEST_TYPE)` and `ParamSource::Value(v)` whose
  `v["namespace"] == namespace`, set `v["host"]` and, when given,
  `v["port"]`; `named = true`. The "names no peer" error lists ingest
  namespaces with the mirrors: "this target subscribes to a, ingests b".
- `src/cli/ui.rs`, `system_detail`: `Ingest` appends ` · ingests <ns>/<link>` from its params.
- `src/wiring/validate.rs`:
  - `check_system`: an `Ingest` spec decodes as `IngestParams` (else
    `IngestSpec { system, reason }`), `port != 0`, `namespace` is not the
    member's own.
  - `check_hosts_and_peers`: for every `Ingest` in every member, the
    `namespace` names another member; that member has a `TcpServer` state
    named `link` whose `addr` port equals `port`; every `commands` token
    appears in the `msgs` of that member's `Uplink`s with `attach == link`;
    across the `Ingest`s of one member attached to one `Db`, the `commands`
    sets are disjoint. Errors: `UnknownSource { system, namespace }`,
    `SourceLink { system, namespace, link, reason }`,
    `SourceCommands { system, token, reason }`,
    `CommandOverlap { first, second, token }`.
- `src/wiring/error.rs`: the five variants, miette-rendered like
  `UnknownPeer`.
- `src/wiring/builder.rs`: `pub fn ingest(self, name: &str, db: &str, params: IngestParams) -> Self`.

Tests to add:

- `src/gateway/ingest.rs`: `configure` resolves tokens and the db state
  advertises them; two `Ingest`s union; an unknown token is `UnknownMsg`.
  `source_loop` against `fake_fsw`-style servers (port the helper from
  `libs/db/src/remote/fsw.rs` tests into `src/gateway/tests.rs`): a
  `Peer::Db` at the address is rejected and the loop moves on; a matching
  `LinkInfo` ingests one table into the db; a dropped socket bumps
  `sessions` on reconnect.
- `src/cli.rs` tests: `--peer a=10.0.0.5:2256` on a target with an `Ingest`
  of `a` sets its host and port and leaves a mirror of `b` alone; `--peer c=…`
  on a target with an ingest of `a` errs naming `a`.
- `src/wiring/validate.rs`: each rule above, positive and negative, over
  `WiringBuilder` deployments.
- `src/cli/ui.rs`: `system_detail` for an `Ingest` reads `builtin · ingests a/link`.

What could go wrong: `IngestOut` has one static output; `Out<IngestOut>`
and the derive give it `log`. The counters are `Rc<AtomicU64>`s shared
between the task and the system on one thread, the `ServerMetrics` shape.
`fsw_stream` warns per rejected packet through `tracing`, not the system
log; the `rejected` counter is bumped by wrapping the ingest error path or
by counting `VTableConflict`/`VTableNotFound` warns in the loop; the
simplest is a `PacketTx`-independent count in `source_loop` around
`handle_packet`, which means `fsw_stream`'s `ingest` takes an optional
counter. Prefer extending `fsw_stream` with an `on_reject: impl FnMut()`
argument over duplicating its loop; both callers pass a no-op or a bump.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP3: `Record`

Files:

- `src/gateway/record.rs` (new), re-exported:

  ```rust
  #[derive(SystemOutput)] pub struct RecordPorts { all: AllOutputs }
  /// Stores this target's telemetered outputs into the embedded db, straight from the rings.
  pub struct RecordSystem { db: Option<Shared<DbState>>, taps: Vec<Tap> }
  impl BuildSystem for RecordSystem { type Params = NoParams; }
  impl System for RecordSystem { type Input = (); type Output = Out<RecordPorts>; const NAME = "record"; fn init(..) }
  impl CyclicSystem for RecordSystem { fn execute(..) }
  ```

  `init`: `collect_taps(&output.all, &TelemetryMode::All)`; for each
  `Announce::Table { packet_id, vtable, metadata }`,
  `db.insert_vtable(VTableMsg { id: packet_id, vtable })` then
  `set_component_metadata` per entry; for each `Announce::Msg(m)`,
  `set_msg_metadata(m.id, m.metadata)`; the `exhausted` and `collisions`
  reports go to `output.log()` under the downlink's fault kinds; a
  `VTableConflict` at init is `record_table_conflict`. `execute`:
  `drain_taps(&mut self.taps, |tap, rec| match tap.wire { Table { packet_id } => db.ingest_table(packet_id, rec), Msg => split_record(rec).map(|(id, payload)| db.push_msg(now, id, payload)) })`,
  counting errors into one `record_rejected` fault per cycle.
- `src/ir.rs`: `RECORD_TYPE = "Record"`.
- `src/wiring/registry.rs`: `gateway_pack.system_type_shared::<RecordSystem, DbState>("Record", |_, db| RecordSystem::new().attach(db))`.
- `src/wiring/validate.rs`, `check_downlinks`: at most one `Record` per
  `attach`, `DuplicateRecord { state }` (or widen `DuplicateDownlink`'s
  message and reuse it; prefer a second variant for the exact wording).
- `src/wiring/builder.rs`: `pub fn record(self, name: &str, db: &str) -> Self`.

Tests to add (`src/gateway/record.rs`):

- `record_stores_the_targets_outputs`: a `WiringBuilder` target with
  `.db(..)`, one frame producer from `src/tests.rs`'s fixtures, and
  `.record("record", "db")`; `run_for(3)`; the state's db holds the
  producer's component with three samples at cycle timestamps,
  `gw.coordinator.system_status`, and `LogEvent` records; the vtable is
  registered under `table_id`.
- `record_writes_a_snapshot_once_per_change`: a producer publishing every
  other cycle; the component's sample count equals the publish count.
- `validate`: two `Record`s on one `Db` → `DuplicateRecord`.

What could go wrong: `ingest_table` reads the timestamp through the
vtable's timestamp op, so a frame record's own stamp is what lands; a
message record has none and takes `now`, the cycle timestamp, which is what
the downlink path's receiver would have stamped at arrival. `RecordSystem`
carries `ReceiveAll` through `AllOutputs`, so resolve defers it to the
tail beside a `Downlink`; two tail systems are ordered by registration.
`Record`'s own `log` and `system_status` are in the tap set from the next
cycle, as the downlink's are.

Green when:

```sh
cargo test -p metor-fsw-2 --lib gateway && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP4: Python `Db`, `Ingest`, `Record`

Files:

- `python/metor-config/metor_config/_model.py`: `StateHandle.__init__(self, name, spec=None, target=None)`.
- `_target.py`: `state` returns `StateHandle(name, spec, self)`; `add`
  records an `_Ingest` spec in `self._ingests`; `_system_ir` folds
  `params` from `_ingests` the way it folds `peer`.
- `_builtins.py`:

  ```python
  def Db(addr: str, path: str | None = None, name: str | None = None, store: str | None = None, max_bytes: int | None = None, max_age_secs: float | None = None) -> Spec
  def Ingest(db: StateHandle, source: StateHandle, commands: list[str] | None = None) -> Spec
  def Record(db: StateHandle) -> Spec
  ```

  `Db` is `static_system("Db", **_drop_none({...}))`. `Record` is
  `_attached(static_system("Record"), db)`. `Ingest` returns an
  `_Ingest(Spec)` attached to `db`, holding `source` and `commands`;
  `_bind(target)` rejects `source.target is target`; the rest needs the
  deployment. `_params_json()` raises if unresolved, like `_peer_json`.
- `_deployment.py`: `_resolve_ingest(name, spec)`: the source's target is
  a member and not the ingesting one; its spec is a `TcpServer`; port from
  `_port(_state(..))`, not `0`; `commands` defaults to the tokens of that
  member's `Uplink` entries with `attach == source.name`, in order; an
  explicit list must be a subset (error names the token); across one
  member's `_Ingest`s attached to one `Db`, sets disjoint (error names both
  instances and the token, suggests `commands=[…]`). Fills
  `spec.params = {namespace, link, port, commands}`.
- `__init__.py`: export `Db`, `Ingest`, `Record`.
- `python/metor-config/metor_config/_version.py`, `pyproject.toml`: `0.4.3`.
- `python/tests/data/deployment.py`: a `gw` member with `Db`, two
  `Ingest`s, `Record`, so the pyright gate types the new builders.
- `python/tests/test_golden.py`: `build_gateway_deployment()` (the golden
  `deployment.json`'s two members plus `gw`), pinned as
  `tests/golden/deployment_gateway.json`.
- `tests/ir_contract.rs`: `golden_gateway_round_trips`: round trip; the
  `gw` member's `Ingest` params decode as `IngestParams` with the copied
  ports and command tokens; `validate_deployment` accepts it.
- `python/tests/test_recorder.py`: record-time errors (source on the
  gateway itself; outside the deployment; a non-`TcpServer` handle; port
  `0`; a token the member does not accept; two `Ingest`s overlapping) and
  the emitted params for the example.

What could go wrong: `commands=[]` must mean none, distinct from `None`;
`_drop_none` would drop `[]` only if it treats empty as none, so pass the
list through explicitly. The `gw` member has no artifacts and no program;
`_program_ir` handles a target with no `@system` already
(`test_no_added_system_emits_no_program`). `test_pyright.py`'s positive
case runs over `deployment.py`; a `StateHandle` annotation on `Ingest`
keeps a `SystemHandle` from being passed.

Green when:

```sh
(cd python && uv run python -m unittest discover tests) && cargo test -p metor-fsw-2 --test ir_contract --test py_eval
```

## WP5: integration and docs

Files:

- `tests/fixtures/gateway_target.py`: members `a` (`TcpServer 127.0.0.1:2256`,
  `Downlink`, `Uplink(msgs=["AlarmAck"])`, `Alarms([])` to consume it) and
  `b` (`127.0.0.1:2257`, `Downlink`, `Uplink(msgs=["ReloadSequences"])`,
  `b.route(uplink, b.coordinator, msg="ReloadSequences")`), and `gw`
  (`Db(addr="127.0.0.1:2258")`, `Ingest(db, link_a)`, `Ingest(db, link_b)`,
  `Record(db)`), all wall-clocked at 100 Hz like `comms_target.py`.
- `tests/gateway.rs` (new), the `tests/comms.rs` harness, one `#[test]`
  running the cases in sequence:
  1. `run gateway_target.py`; a `RemoteDb` from a temp `DB` to `2258`
     (`metor-db` is a dev-dependency of the fsw crate already through the
     main dependency); wait until `a.coordinator.system_status`,
     `b.coordinator.system_status`, `gw.a.source_status`, and
     `gw.b.source_status` exist locally and the two `source_status`
     records read `connected == 1`; the local `LogEvent` log holds lines
     whose `source` starts with `a.` and `b.`.
  2. Dial `a`'s and `b`'s links directly with `dial`; push one
     `ReloadSequences` into the temp db; within a deadline `b`'s client
     sees a second `WiringManifest` packet and `a`'s sees none.
  3. Kill `a` (find its pid through the launcher's prefix is not
     possible; instead run `a` as its own child with `run … --target a`
     beside a launcher of `b` and `gw` started from bundles, so the test
     owns `a`'s `Child`); restart it; `gw.a.source_status.sessions` reads 2
     and `a.coordinator.system_status` advances.
  4. `run gateway_target.py --target gw --cycles 200`: exit 0, stderr has
     no panic, and a mirror sees both `source_status` frames at
     `connected == 0`.
  5. `package --target a/b/gw`, `run a.bundle b.bundle gw.bundle`: case 1
     again, cargo-free.
- `docs/telemetry.md`: a "Gateway" section after "Peers": the `Db` state,
  `Ingest`, `Record`, the candidate rule, the identity check, the fault
  kinds (`source_identity`, `source_disconnect`, `source_commands`,
  `record_table_conflict`, `record_rejected`), the `source_status` frame,
  the command path and the no-echo rule, the `role=gateway` record.
- `docs/cli.md`, "Deployments": `--peer` applies to a gateway's ingests
  too; a gateway member packages and runs like any other; the error line
  for an ingest with no matching `--peer`.
- `docs/wiring.md`, "Deployments": the example gains the `gw` member.
- `docs/README.md`, "Built-in services": one line for the gateway.

What could go wrong: case 3's process ownership is the awkward one; the
launcher owns its children, so the test drives `a` itself. The `Alarms([])`
on `a` exists only so `AlarmAck` has a consumer edge; if `Alarms` rejects an
empty list, route the uplink into a fixture consumer instead. The
`WiringManifest` count needs the announce replay's retained copy excluded:
count packets after the first one each client reads.

Green when:

```sh
cargo test -p metor-fsw-2 --test gateway && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP6: the adcs example

Files:

- `examples/adcs-fsw2/target.py`: after the `fsw` member,
  `gw = Target(cycle_rate=120.0, namespace="gw")`, `db = gw.state("db", Db(addr="[::]:2250"))`,
  `gw.add("plant", Ingest(db, plant_link))`, `gw.add("fsw", Ingest(db, fsw_link))`,
  `gw.add("record", Record(db))`; `Deployment(targets=[plant, fsw, gw])`.
- `examples/adcs-fsw2/README.md`, "Watch it live in metor-panel": the
  panel connects to `gw` on `2250` (it appears in the picker as a gateway
  when the bind is not loopback, or by address) and sees both members; the
  plant and fsw links on `2240`/`2241` stay reachable for diagnostics; the
  `uplink`'s commands reach `fsw` through the gateway.
- `examples/adcs-fsw2/tests/bundle.rs`: `package --target gw` beside
  `fsw`; a cargo-free run of the three bundles for 20 cycles exits 0.

What could go wrong: the panel's mDNS browse skips loopback binds; `[::]`
is a wildcard, so the gateway advertises on every interface as the links
do. `build_sim_coordinator` selects `fsw` and is untouched; the sequences
and python-system suites select `fsw` and are untouched.

Green when:

```sh
cargo test -p adcs-fsw2 && cargo test -p metor-fsw-2 --test gateway
```
