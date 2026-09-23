# Deployments and gateway: implementation plan

Implements `08-deployments.md`. Nine tasks, in dependency order. Each
task ends with `cargo test -p metor-fsw-3` green, `cargo clippy
--all-targets` clean, the Python tests green, and a commit. T1 to T3
change the config and the Python surface with nothing resolved yet. T4
is the build pass. T5 and T6 are the two systems that consume it. T7 is
the multi-process run, T8 the fixture deployment and its gate, T9 docs.

Layout after this slice:

```
libs/metor-fsw-3/
  src/
    cli/
      config.rs        DeploymentConfig, TargetConfig, CONFIG_VERSION 2
      run.rs           eval, load one target, spawn children
      build.rs         `metor build` (the cargo helpers move to cargo.rs)
      children.rs      spawn, prefix, drain, kill
    deploy/
      mod.rs           resolve: the pass over an envelope
      index.rs         DefIndex: on-demand memoized defs across targets
      error.rs         DeployError
    coordinator/
      config.rs        OutputSource, PeerPort
      build.rs         instance_def moves to def.rs's neighbourhood
    def.rs             DefError::Unresolved, instance_def
    link/
      transport.rs     Transport::Peer
      inbox.rs         Table bodies, announce watch
      subscribe.rs     frame routes, mismatch fault, MsgStream seed
      db.rs            Db (feature `db`)
  python/metor-config/metor_config/
    _deployment.py     Deployment, hosts, emission rules, gateway()
    _model.py          Record._kind, System._ports, OutPort.target
    _builtins.py       peer=, items, records, port(), Db, LinkStatus
    _target.py         SystemHandle carries its target and port kinds
  tests/
    deploy.rs          the gate: multi-process run, db query
    fixtures/echo-pack/deploy.py
    golden/deploy.json resolved envelope, pack paths normalized
```

New dependency: `metor-db` by path, optional, behind feature `db`
(default on). Everything else is in tree.

## T1. The envelope

Files: `src/cli/config.rs`, `src/cli/run.rs`, `python/metor-config/
metor_config/{_deployment,_target,_config,__init__}.py`, Python tests,
`tests/run.rs`.

1. `config.rs`: `CONFIG_VERSION = 2`. `TargetConfig { packs,
   coordinator }`. `DeploymentConfig { config_version, hosts:
   BTreeMap<String, String>, targets: Vec<TargetConfig> }` with
   `from_slice` doing the version check `TargetConfig::from_slice` did.
   `DeploymentConfig::target(&self, namespace: Option<&str>) ->
   Result<usize, ConfigError>`: no name selects the only target, or
   errors `SelectTarget` listing the namespaces; a name selects by
   `coordinator.namespace` or errors `NoSuchTarget`.
2. `run.rs`: `Run` gains `--target`. `eval_target` returns a
   `DeploymentConfig`. `load` takes the envelope and the selected index
   and builds that target only. Children come in T7; for now more than
   one target without `--target` is the `SelectTarget` error.
3. `_deployment.py`: `Deployment(targets, hosts=None)`. Its constructor
   records itself in a module list beside `_targets`. `to_config()`
   builds `{config_version, hosts, targets: [t.to_config() ..]}` and
   checks: every member has a namespace, namespaces are unique, every
   `Target` the file built is a member. `Target.to_config()` drops
   `config_version`.
4. `_target.py`: `emit` and the atexit hook pick what to emit: one
   `Deployment`, else one bare `Target` wrapped as a deployment of one,
   else the errors the design lists. `_the_target` goes.
5. `_config.py`: `CONFIG_VERSION = 2`.

Tests: Rust decodes a two-target envelope and selects by namespace,
refuses version 1 and 3, `SelectTarget` names both members. Python: a
bare target's file equals `Deployment(targets=[t])`'s; two bare targets,
two deployments, an omitted member, a member without a namespace, and
two with one are each `ConfigError`; the fixture's `target.py` emits a
one-member envelope and `tests/run.rs` still passes.

## T2. Reference forms in the config

Files: `src/coordinator/config.rs`, `src/def.rs`, `src/port/mod.rs`,
`src/link/transport.rs`, `src/coordinator/build.rs`.

1. `config.rs`: `OutputConfig { port, #[serde(flatten)] source:
   OutputSource }`, `OutputSource { Record(String), Peer(PeerPort),
   Def(PortDef) }` with `#[serde(rename_all = "snake_case")]`, so the
   three JSON forms are `record`, `peer`, `def`. `PeerPort { target,
   link, port }`. `PeerLink { target, link }` for transports.
2. `def.rs`: `DefError::Unresolved { port }`, message "run `metor
   build`". `dyn_output_defs`: `Record` as today, `Def(def)` cloned
   under the config's name, `Peer` is `Unresolved`.
3. `transport.rs`: `Transport::Peer(PeerLink)`; `bind` returns a
   `ParamError` naming the link and saying to run `metor build`.
4. `build.rs`: `check_outputs` and every `OutputConfig` construction
   use the enum. `instance_def` and `producers` move to `def.rs` as
   `pub(crate) fn instance_def(system, entry, namespace, records,
   producer: impl Fn(&PortRef) -> Result<Option<PortDef>, BuildError>)`,
   with the coordinator passing a closure over its plan. T4 passes the
   index's lookup.

Tests: each `OutputSource` form round-trips through JSON in the shape
the design shows; a `Def` output completes to that def under the new
name and its ring spec has the def's length; a `Peer` output is
`BuildError::Def { source: Unresolved }`; a `Peer` transport is a
`ParamError` at construction; the coordinator's tests are unchanged.

## T3. Python surface and the generated module

Files: `src/cli/module.rs`, `tests/golden/echo_pack.py`,
`tests/fixtures/echo-pack/.metor/echo_pack/__init__.py`,
`python/metor-config/metor_config/{_model,_target,_builtins,
_deployment}.py`, Python tests.

1. `module.rs`: every record class gains `_kind = "frame"` or `"msg"`
   from its `RecordSchema`; every system class gains `_ports = {"est":
   Est, "log": LogEvent, "status": SystemStatus}`. Goldens regenerated.
2. `_model.py`: `Record._kind`. `System._ports: dict[str, type[Record]]`,
   empty by default; the base `__init__` extends it with `records=`
   classes. `OutPort` gains `target` (the `Target` it belongs to, or
   `None` for a loop's producer) and `kind`.
3. `_target.py`: `SystemHandle(name, target, ports)`; attribute access
   returns an `OutPort` carrying the target and the port's kind; `log`
   and `status` come from a framework map. `Target.add` passes itself.
4. `_builtins.py`: `LinkStatus(Record)` frame; `Publish` and
   `Subscribe` set `_ports`. `_transport` takes `peer` as a third
   exclusive keyword: a handle whose system is a link; emitted as
   `{"peer": {"target": ns, "link": name}}`. `Subscribe` takes
   `items` beside `records` when `peer` is given. `Db(listen, path=None,
   items=(), all=False)` mirrors `Publish`'s input handling with
   `_ty = "db"` and `_ports = {"db_status": DbStatus}`.
5. `_deployment.py`: the deployment resolves what Python can. For each
   `Subscribe` with a peer: the peer is a member's link; it listens on a
   fixed port; its published ports come from `peer._inputs_for(target,
   index)` after `all` expansion. Items expand to those ports: a frame
   item is an output `{port: "{system}.{port}", peer: {target, link,
   port}}`; a message item, or every message record a handle carries,
   is `{port: record, peer: ..}` named once per record, the first port
   winning. No items and no records is every published port. Missing
   ports, a dialing peer, and port 0 are `ConfigError`s naming the
   reference. `records=` stays `{port: record, record: record}`, which
   is what a `Db` peer supports. Host port collisions: two listeners
   under one host on one port is an error.
6. `SystemHandle.port(source)`: on a subscriber with a peer, returns
   `OutPort(self._name, output name, kind, target)` for a source among
   its resolved items, else `ConfigError`. Typed `(OutPort[T]) ->
   OutPort[T]` in the stub, so pyright carries the record through.
7. `gateway(namespace, peers, listen, cycle_rate=10.0, path=None) ->
   tuple[Target, SystemHandle]`: the target and the `Db`'s handle, so a
   member subscribes commands with `Subscribe(peer=gw_db, ..)`.

The Python `Deployment.to_config` runs `Target.to_config` for members
in list order but resolves peer items lazily per reference, so a
gateway listed after its peers and a target dialing the gateway's db
both work in any member order.

Tests: golden module contains `_kind` and `_ports`; a peer subscribe
with `items=[handle, port]` emits the frame and message outputs the
design shows, deduping `log`; no items emits everything the publish
does after `all=True`; `port()` returns the right name and kind and
raises on a non-item; `peer=` to a dialer, a non-member, and port 0
raise; `Db` emits `fsw.db` with `all` expanded; the golden envelope
for the design doc's three-target file; pyright on a target file using
`mirror.port(est.est)` in a typed input passes and a wrong record fails.

## T4. The build pass

Files: `src/deploy/{mod,index,error}.rs`, `src/cli/{build,run,mod}.rs`,
`src/link/db.rs` params (only the field, ahead of T6), `src/lib.rs`.

1. `deploy/error.rs`: `DeployError { NoSuchTarget, NoSuchLink { target,
   link }, NotAListener, NoSuchPort { target, link, port }, PortTaken {
   host, port, links }, Cycle { path }, Build(BuildError), Def { target,
   id, source } }`.
2. `deploy/index.rs`: `DefIndex<'a> { envelope, tables: &[SystemTable],
   memo: HashMap<(usize, usize), SystemDef>, visiting: Vec<(usize,
   usize)> }`. `def(target, system) -> Result<&SystemDef, DeployError>`
   computes through `instance_def` with a producer closure that recurses
   into this index, and a peer-output closure for the system's own
   `Peer` outputs (so a mirror re-published to a peer resolves through
   the mirror's baked def). `visiting` turns re-entry into `Cycle`.
3. `deploy/mod.rs`: `resolve(envelope: &mut DeploymentConfig, tables:
   &[SystemTable]) -> Result<(), DeployError>`. Three passes over every
   system of every target: transports (`Peer` → `Connect { addr }` from
   the peer's `Listen` addr port and `hosts`, loopback default; the
   peer must be a listener with a nonzero port); outputs (`Peer` →
   `Def` from `index.def(peer target, peer link)`'s input port of that
   name, renamed); `Db` command ids (for each `Subscribe` whose
   transport peer is a `fsw.db`, append the ids of its message outputs
   to that db's `command_ids` param, deduped). Then the port collision
   check over every resolved listener. Resolving an envelope that has
   no `Peer` anywhere touches nothing, which is the fixpoint property.
4. `cli/build.rs` becomes `cli/cargo.rs`; a new `cli/build.rs` is `metor
   build <target.py> [-o out]`: `eval_target`, open every target's
   packs into one table per target, `resolve`, write pretty JSON.
5. `run.rs`: `load` becomes `resolve_and_load`: opens the packs of every
   target, resolves, then builds the selected one. A resolved envelope
   opens only the selected target's packs, because `resolve` is run
   only when any `Peer` remains; `DeploymentConfig::unresolved()` says.

Tests, all with in-process tables and no packs: a peer output becomes
the producer's def under the new name and its schema keeps the peer's
root; a peer transport becomes `127.0.0.1:2240`, or the host's address;
a `Db` collects the ids of two subscribers' message outputs, deduped; a
missing target, link, and port each name themselves; a dialing peer
link is `NotAListener`; port 0 is refused; two listeners on one host
with one port is `PortTaken`; a mirror of a mirror of itself is
`Cycle` naming both links; running `resolve` twice is a no-op;
`unresolved()` is false after it.

## T5. Frame subscription

Files: `src/link/{inbox,subscribe,wire,transport}.rs`.

1. `inbox.rs`: `Inbox::new(msg_ids, table_ids, cap, max_len)`. `accept`
   takes a `Table` body whose id is a table id and a `Msg` body whose id
   is a message id; `message` returns `(PacketTy, PacketId, &[u8])` and
   `deliver` matches on both. A `Msg` carrying `VTableMsg` records its
   table id in `announced: RefCell<Vec<PacketId>>` when the watch is on,
   and the first data packet after that sets `seen_data`. `Inbox::
   unannounced() -> Vec<PacketId>` is what the loop faults from, once.
2. `subscribe.rs`: `routes` returns frame ports too, keyed by
   `table_id(vtable)`; `duplicate_id` covers table ids. `max_len` is the
   longest of both. The watch is on for `Endpoint::Connect`. After
   `deliver`, if `seen_data` and the unannounced list is non-empty, one
   `peer_schema_mismatch` fault per port, then the list is cleared and
   the watch is off.
3. The dialer's seed: `wire::msg_streams(ids) -> Vec<u8>`, one
   `MsgStream { id }` packet per message output. `Endpoint::Connect`
   seeds it; `Listen` keeps `LinkInfo`.

Tests: a `Table` body routes to the frame port and the consumer decodes
the frame; a `Table` for an unknown id is ignored; a frame and a message
port coexist with the right `max_len`; two frame ports of one vtable
fault `duplicate_id`; the announce of one of two baked tables followed
by a `Table` faults the other once and routes the first; the dialer's
seed decodes to one `MsgStream` per message output and none per frame.

## T6. The Db system

Files: `Cargo.toml`, `src/link/{db,mod}.rs`, `src/cli/run.rs`.

1. `Cargo.toml`: `metor-db = { path = "../db", optional = true }`,
   `[features] default = ["db"]`, `db = ["dep:metor-db"]`.
2. `db.rs` under `#[cfg(feature = "db")]`: `DbParams` as designed;
   `Db::new` binds through `Transport::bind` (a `Connect` is a
   `ParamError`), takes the listener out of the `Endpoint`, and calls
   `Server::from_listener(listener, path)`, `path` defaulting to
   `temp_dir()/metor-db-{link}-{pid}`. `run`: announce every input def
   into the db (`insert_vtable`, `set_component_metadata` per component,
   `set_msg_metadata`), `set_identity`, spawn `server.run()` and
   `lod::spawn` under drop guards, then loop on `inputs.any_ready()`:
   frames through `ingest_table`, messages through `push_msg(now, ..)`;
   count refusals; write `DbStatus { timestamp, records, refused }` on
   change. `record_bound` so `--print-ports` lists it.
3. `mod.rs`: `register_builtins` adds `fsw.db` under the feature;
   `DbStatus` beside `LinkStatus`.

Tests (io, in `src/link/db.rs`'s test module against a temp dir): a
`Db` bound to port 0 over one frame port and one message port ingests
three records of each and the db reports the component's samples and
the message log's entries through `DB::with_state`; a second vtable
under a colliding id is counted refused; a `Connect` transport is
refused at construction; the crate builds with `--no-default-features`.

## T7. Multi-process run

Files: `src/cli/{run,children}.rs`, `tests/run.rs`.

1. `children.rs`: `Child { namespace, process, drain: JoinHandle }`.
   `spawn(envelope_path, namespace, overrides) -> Child` runs
   `current_exe() run <path> --target NS` with `--cycles`, `--wall`,
   `--sim-dt` forwarded, stdout and stderr piped into one reader that
   prefixes each line `NS │ ` onto the parent's stderr. `wait_all
   (children) -> ExitStatus`: the first nonzero exit kills the others;
   the result is nonzero if any was.
2. `run.rs`: several targets and no `--target` write the resolved
   envelope to `temp_dir()/metor-deploy-{pid}.json`, spawn every
   target, wait, remove the file. `--print-ports` with several targets
   and no `--target` is a usage error. Clock overrides reach children
   through their own flags; `RunError::Children(namespaces)` lists the
   failures.
3. SIGINT: the parent's handler already flips `INTERRUPTED`; children
   share the process group and stop on their own. The parent's wait
   returns when they exit.

Tests: `member_argv` is a pure function pinned by a unit test; a
two-target envelope of two empty targets runs to `--cycles 3` with both
prefixes on stderr and exit zero; a child that latches makes the
parent exit nonzero and names it; `--print-ports` without `--target`
on two targets exits 2.

## T8. Fixture deployment and the gate

Files: `tests/fixtures/echo-pack/{src/lib.rs,deploy.py,.metor/..}`,
`tests/golden/{echo_pack.py,deploy.json}`, `tests/deploy.rs`.

1. The pack gains `Sample`, a frame with a timestamp and one `f64`, and
   `Sampler`, a cyclic system writing one per cycle counting up.
   `pack dev` regenerates the module and the golden.
2. `deploy.py`: target `a` (`sampler`, `Publish(listen=127.0.0.1:P1,
   all=True)`); target `b` (`Subscribe(peer=a_pub, items=[sampler])`,
   `echo` reading `mirror.port(sampler.sample)` through a system that
   accepts the frame, a `Pinger`-style producer, `Publish(listen=
   127.0.0.1:P2, all=True)`); `gateway("gw", peers=[a_pub, b_pub],
   listen=127.0.0.1:P3)`. Fixed ports from a small range the tests
   own; the test picks free ones and passes them through an env var
   the file reads, so parallel test binaries do not collide.
3. `tests/deploy.rs`, one target process per test run:
   - `metor run deploy.py --cycles 200`: exit zero; connect to the
     gateway's db with `metor_db`'s client and read `a.sampler.sample`
     samples with increasing values and `b`'s ping message log.
   - `metor build deploy.py` equals `tests/golden/deploy.json` with pack
     paths replaced.
   - `metor run <resolved> --target b` with `a`'s and `gw`'s `libs`
     rewritten to a directory that does not exist exits zero after
     `--cycles 3`, which pins that a resolved target opens only its own
     packs.
   - A `Publish` announcing `Sample` with a changed field, built in the
     test binary and served on P1 in place of `a`, makes `b`'s
     `mirror.log` carry `peer_schema_mismatch` for `sampler.sample`.

## T9. Docs

Files: `DESIGN.md`, `docs/plans/05-links.md`, `08-deployments.md`,
`TODO.md`.

- `DESIGN.md`: the Deployments section describes the envelope, `metor
  build`, `peer=`, `port()`, and the gateway as built; the config
  example shows version 2.
- `05-links.md`: "Table packets are dropped and counted in this slice"
  and the `MsgStream` note point at slice 7 as landed; the `Subscribe`
  section's frame-output fault is replaced by the routing rule.
- `08-deployments.md`: shipped deviations folded in.
- `TODO.md`: slice 7 marked done with the date; the `--host` override,
  a `loop()`-style forward link reference, and the per-port message
  identity listed as follow-ups.
