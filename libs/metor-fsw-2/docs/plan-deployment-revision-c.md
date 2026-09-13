# Plan: deployments, revision C (the ring-backed gateway)

Implements change 3 of
[design-deployment-revision.md](design-deployment-revision.md) and the
WAL-tail rule from its "Out of scope" section, on top of
[plan-deployment-gateway-2.md](plan-deployment-gateway-2.md). Five work
packages. Each ends with a green tree. At the end an `Ingest` is one mirror
per instance of its source member, fed by one connection per member into
log-delivery rings the gateway's `Record` drains into the db; the gateway
bundle carries its sources' descriptors and no dylib; the db's connection
tails follow persisted message nodes, so a stalled mirror connection can no
longer make `push` fail for everyone. Paths are relative to
`libs/metor-fsw-2` unless they start with `libs/`, `examples/`, or
`python/`.

Test commands used throughout:

```sh
cargo test -p metor-db -p metor-db-tests && cargo check -p metor-panel   # WP1
cargo test -p metor-fsw-2                                                 # WP2–WP4
cargo test -p metor-fsw-2 --lib wiring                                    # WP2
cargo test -p metor-fsw-2 --lib subscribe                                 # WP3
cargo test -p metor-fsw-2 --lib gateway                                   # WP4
cargo clippy -p metor-fsw-2 --all-targets                                 # lints, house rule
cargo test -p metor-fsw-2 --test gateway --test ir_contract --test py_eval  # WP5
(cd python && uv run python -m unittest discover tests)                   # WP5
cargo test -p adcs-fsw2                                                   # WP2, WP5 (skip w/o python3)
```

## Sequencing

```text
WP1 ─┐
WP2 ─┼─> WP4 ─> WP5
WP3 ─┘
```

WP1 (the db's persisted-node tails, `forward` made public) touches
`libs/db` and one panel line. WP2 (the peer substrate: graph node knobs,
peer descriptors from sidecars, the bundle's peer members, the CLI passing
peers) and WP3 (the subscribe client generalised over mirror sets) touch
disjoint files in the host crate. The three run in parallel and each is
its own commit. WP4 (the gateway itself) needs all three. WP5 (Python
`depth=`, the integration test, the example, docs) needs WP4.

## Deviations from the revision doc

Found while grounding the plan against the code. The revision doc states
the shape; these are the places where the code needed a decision the doc
does not make, or where the doc's wording does not survive contact.

1. **Ingested mirrors keep the source's names.** A `Subscribe` mirror
   registers under the subscriber's namespace (`fsw.sim.gps`): every node
   goes through `InitGraph::qualify` (`src/coordinator/init.rs:339`). An
   ingest mirror must register as `plant.plant.sensors` so the db holds the
   member's components under its own namespace, which the revision's
   `Trace(plant_sim.sensors)` → `plant.plant.sensors.gyro_b` example and the
   gateway test's `a.coordinator.system_status` both require. A `Node`
   gains `qualified: bool` (its name is already the registry instance).
2. **The peer's `system_status` and `log` are mirrored, not replaced.**
   Comms drops the peer type's `system_status` and `log` and lets
   `push_node` append the mirror's own (`src/wiring/resolve.rs:514-522`,
   `init.rs:318-326`). For ingest that would lose every member's status
   frames and log lines from the db, which the gateway test asserts
   (`tests/gateway.rs:227,243-247`). The peer's `system_status` is
   mirrored as an ordinary Edge port and the host's own is skipped
   (`Node.host_status = false`; `CyclicEntry.status` is already an
   `Option`, `src/coordinator/bind.rs:97-101,174-179`). The peer's `log`
   lines are forwarded through the ingest's own log port
   (`LogPort::emit_event`, `core/src/log.rs:102`, the tracing-drain shape
   of `drain_forwarded_logs`), because every cyclic node owns a `log`
   port (`cyclic_node` requires `LogOutput`, `init.rs:177`) and two `log`
   rings under one instance would fail `freeze_registry`
   (`init.rs:551-561`).
3. **A message record is routed once.** The revision has one connection
   routing packets "into all of them". A table packet names its instance
   through its component ids; a message packet names only its id, and the
   downlink announces each id once (`src/telemetry/taps.rs`, the
   `announced_msgs` set). A message record is written to the first mirror
   port carrying its id, in registration order. Message logs are keyed by
   id in the db, so the ring is transport, not identity. `LogEvent` goes to
   the ingest's log queue (deviation 2).
4. **Log delivery is for frame ports.** "Ingested frame rings use log
   delivery" is applied to Table ports only. A message port keeps the
   source's delivery: the coordinator's `wiring` manifest is a Snapshot
   message sized from the IR (`init.rs:576-580`); at log depth 120 it would
   cost ~12 MB per source for latest-wins boot state. The `depth` override
   applies to the mirror node's Log rings, floored at `LOG_DEPTH` (64,
   `core/src/message.rs:98`) so message bursts within one gateway cycle
   are kept.
5. **The gateway reads the peer `Wiring`, not a Python instance list.**
   Both were weighed. A Python-emitted `instances: [{name, ty, artifact}]`
   would also need the source's artifacts in the gateway's IR (which
   `provision_artifacts` would try to build and `load_bundle` would
   require a `.so` for), the source's `cycle_rate`, the byte length of its
   manifest to size the `wiring` port, and a copy of every `Subscribe`
   mirror the source itself holds (the adcs `fsw` member mirrors
   `plant.plant`). The peer `Wiring` is already config-static, frozen at
   package time, and carries all of that; describing it is the one
   function each member's own resolve already runs. `ResolveOptions` gains
   `peers: Vec<Wiring>`, filled from the envelope for a source run and from
   the bundle for a cargo-free one.
6. **A gateway bundle carries peer members.** `package --target gw`
   writes `meta.json`, `wiring.json`, and no artifacts
   (`src/wiring/bundle.rs:294-371`); `load_bundle_dir` requires every
   artifact's `.so` (`MissingSo`, 511-520). Peer artifacts are described-
   only members named `<ns>.<cdylib>.manifest` and `<ns>.<id>.wasm`, with
   `<ns>.wiring.json` beside them and the namespaces listed in
   `BundleMeta.peers`; the `.so` rule never applies to them. The prefix is
   necessary: every member's program artifact is `program`
   (`_version.py:12`), so two peers' `program.wasm` would collide
   (`member_artifact_name`, `bundle.rs:378-386`).
7. **`resolve_mirror` dlopens today.** It calls `describe_raw`
   (`resolve.rs:505`), which loads the cdylib, even when the build driver's
   sidecar sits beside it. `check_manifest_hashes` (`artifacts.rs:315`) and
   `program.rs::decode_manifest` (299-302) already prefer
   `manifest_sidecar_bytes` and fall back to a describe. The mirror arm
   takes the same order, so a bundle with sidecars and no dylib resolves;
   a bundle built with `--no-manifest-sidecar` cannot carry a peer's
   descriptors and `package` refuses it for a gateway.
8. **The coordinator's ports are not in the registry.** Its descriptor is
   built inline in `InitGraph::new` (`init.rs:258-295`) and the `wiring`
   port is injected later, sized from the manifest JSON
   (`set_wiring_manifest`, 355-370). Both are factored into
   `coordinator_descriptor(ir_json: Option<&str>)` so a peer's coordinator
   mirror sizes `wiring` from the peer's own path-stripped IR exactly as
   the peer does.
9. **`MsgLog` has no positioned read over persisted nodes.** It has
   `latest()`, timestamp-keyed `get_range`, and `wal_reader()`
   (`libs/db/src/msg_log_2.rs:414,405,507`); and `persist` wakes no waiter
   (only `push` does, at 309, before the persist pass runs), so a node
   tail woken by `push` would find nothing persisted yet and lag one push
   behind. WP1 adds a `MsgCursor` and a wake after each persist pass.
10. **The ingest and its mirrors meet in `DbState`.** `IngestSystem` stays
    a cyclic system attached to `Db` (shared state is cyclic-only, plan 2
    deviation) with the client as a drop-guard task. The mirrors are
    static systems of a built-in type attached to the same `Db`; each
    hands its bound writers to the state in `init`, and the ingest takes
    its source's set in its own `init`. Cyclic inits run in registration
    order on the loop task (`src/coordinator/async_tasks.rs:133-136`) and
    the client task first runs when the loop yields, so the hand-off is
    ordered without a barrier.
11. **Slots on a source are planned from described occupants.**
    `plan_slot(name, allowed)` needs only each occupant's descriptor and a
    uniform backing (`src/coordinator/slot/plan.rs:150-172`);
    `OccupantBacking::Artifact(path)` is the described form the process
    path builds (`slots.rs:239-300`). The peer describer builds the same
    `AllowedOccupant`s from sidecar bytes.
12. **Static built-ins register their type descriptor.** Only
    `UplinkSystem` (`src/telemetry/uplink.rs:184`, extra ports
    untelemetered) and `SubscribeSystem` (`subscribe.rs:145`) override
    `instance_descriptor`; the peer describer handles the second by
    constructing the subscriber over the peer's mirror ports and reading
    its descriptor.
13. **Plan B is independent.** The gateway's connection sends no
    `Subscribe` request; it reads the shared batch, as the revision's
    change 1 says a request-less connection does. A source whose ground
    `Downlink` is filtered leaves some mirror ports unannounced; they are
    reported once per connection as today's `peer_channel_missing`.

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| `manifest_sidecar_path(so)` is `<so>.manifest`; `manifest_sidecar_bytes` reads it; `decode_pack_manifest(bytes)` yields `Vec<PackEntryDesc>` with the capability check | `src/dl.rs:251-280` | a peer's cdylib is described from its sidecar with no dlopen; the `.so` need not exist, only `<so>.manifest` |
| `resolve_mirror` reads a cdylib through `describe_raw` (dlopen) and caches by artifact id in `described: HashMap<String, Vec<PackEntryDesc>>`, one per resolve; wasm goes through `WasmCache::open`, which reads the module bytes and instantiates it to read the manifest | `src/wiring/resolve.rs:171,491-511`; `resolve/artifacts.rs:65-106` | the cdylib arm becomes sidecar-first; the cache moves into the shared describer; a peer's `program.wasm` must be a bundle member |
| `bundle_members` writes `meta.json`, `wiring.json`, one `.so`/`.wasm` per artifact plus its `.manifest` when present, then the provenance copy; member names are one path component of at most 100 bytes | `src/wiring/bundle.rs:294-371,224-241` | peer members are `<ns>.wiring.json`, `<ns>.<cdylib>.manifest`, `<ns>.<id>.wasm` |
| `member_artifact_name` is `<id>.wasm` for wasm and the per-triple cdylib name otherwise; `load_bundle_dir` fills `artifact.path` from it and errs `MissingSo` when absent, `MissingManifest`/`ManifestHashMismatch` on the sidecar | `bundle.rs:378-386,511-540` | peers get their own loop: sidecar or wasm required, never the `.so` |
| `BundleMeta.packs` is `#[serde(default)]`, the precedent for an additive meta field | `bundle.rs:64-65` | `peers: Vec<String>` is serde-defaulted; old bundles load |
| `load_bundle` has five callers | `src/cli.rs:503,1396`; `examples/adcs-fsw2/tests/bundle.rs:103,147,184` | the return type becomes `Bundle { wiring, peers }` |
| `Routes { tables: HashMap<PacketId, usize>, msgs: HashMap<PacketId, usize>, refused: HashSet<PacketId> }`; `bind(peer, ports, announced, log)` matches a table group by `<ns>.<instance>.<port>` prefix and a msg by id + exact schema; `route(pkt, routes, output, gauge, cycle, record)` writes `output.ports[index]`; `take_announce` folds the replay | `src/telemetry/subscribe.rs:303-309,442-494,521-564,402-434` | the index becomes `(mirror, port)`; the functions take a slice of mirror port sets and a writer table instead of `SubscribeOut` |
| `session` runs identity → replay → `bind` once → `route` per packet, under `context.until_cancelled`; the ingest's `session` today is identity → `fsw_stream` | `subscribe.rs:313-390`; `src/gateway/ingest.rs` | the ingest's session becomes the subscribe shape over a plain task, raced against `forward` |
| `MsgLog.list` is a newest-first `AtomicStack<MsgLogNode>`; `latest()` reads the head's last index; `get_range` is timestamp-keyed; `MsgRef { node, timestamp, index }` has private fields; no positioned read exists | `libs/db/src/msg_log_2.rs:23-30,148-152,405-423` | WP1 adds `MsgCursor` and `tail()`/`drain_after` |
| `MsgLog::push` writes the WAL (1 MiB) and returns `MapOverflow` when `try_grant` fails; `persist` drains the `pending` reader and wakes nobody; `push` wakes `data_waker` before persisting | `msg_log_2.rs:237,286-311,460-482` | a persisted-node tail needs a wake after persist |
| `try_grant` refuses (`WouldBlock`) when `committed - slowest_cursor + need > cap`; dropping a `Reader` frees its slot | `libs/db/src/disruptor.rs:67-113,289-296` | a live reader that never advances stalls every writer; a dead connection does not |
| `wal_reader()` callers: `sync_msg_log`, `forward`, and the persister's own two (`pending`, `notify`) | `libs/db/src/lib.rs:1903`; `remote/mod.rs:55`; `msg_log_2.rs:238,461` | after WP1 the persister is the WAL's only reader |
| `handle_msg_sync` spawns one `sync_msg_log` per log per connection; each shares the connection's `Mutex<PacketSink>`; one connection is one thread | `lib.rs:1844-1942,1030-1038` | a stalled socket blocks every tail on that connection behind one send while their readers pin the WAL |
| `forward` is `pub(crate)`; `RemoteDb::mirror` and `fsw_stream` race it; `fsw_stream` takes `on_packet`, passed `\|_\| {}` by the panel | `remote/mod.rs:41`; `remote/db.rs:180-183`; `remote/fsw.rs:36-53`; `libs/metor-panel/src/connections/target.rs:374` | `forward` goes `pub` for the gateway; `on_packet` goes, with one panel line |
| Public rings get `fan_out + receive_all_count + reader_slack` readers (`READER_SLACK = 4`); async private rings get exactly 1; `ring_config` sizes Log at `LOG_DEPTH` and Snapshot at `default_depth` (`DEFAULT_DEPTH = 8`) from `port.delivery`; no per-node depth | `src/coordinator/init/rings.rs:170-200,251-360,366-388`; `src/coordinator/mod.rs:40`; `core/src/port.rs:51` | a `Node.ring_depth` override sizes the mirror's Log rings; `Record`'s `ReceiveAll` is one of the five reader slots |
| `RingPump::pump` moves every record for Log and the newest on `committed` change for Snapshot | `src/io_bridge.rs:31-53` | not used: the mirrors are cyclic nodes written directly, no boundary pump |
| `push_node` appends `host_status_port()` to every non-coordinator node; `bind_systems` gives every host-stepped slot `Some(host_status_writer)`, async boundaries `None`; the step loop publishes only when `Some` | `init.rs:318-326`; `bind.rs:97-101,174-179,198-208`; `mod.rs:533-539` | `Node.host_status = false` skips the append and the writer |
| `freeze_registry` rejects a duplicate `<instance>.<port>` key | `init.rs:551-561` | one `system_status` and one `log` per instance, whoever writes them |
| Async inits run first behind a barrier; cyclic inits then run in registration order on the loop task; async runs are released after | `src/coordinator/async_tasks.rs:118-142` | a cyclic `init` hand-off is complete before any task runs |
| `InitGraph::new` builds the coordinator descriptor inline (`system_status`, `log`, `coordinator_status`, `sequences`, untelemetered `commands`); `set_wiring_manifest` injects `wiring` sized by `wiring_manifest_max_size` (private) | `init.rs:258-295,355-370,576-580` | factored into `coordinator_descriptor(ir_json)` |
| `AllowedOccupant { name, params, descriptor, backing }`; `OccupantBacking::Artifact(PathBuf)`; `plan_slot` checks uniform backing and capabilities, then derives the descriptor; `describe_occupants` gets bytes from `describe_via_worker` and decodes with `decode_pack_manifest` | `src/coordinator/slot/plan.rs:14-56,150-172`; `resolve/slots.rs:239-300` | the peer describer builds `Artifact`-backed occupants from sidecars and calls `plan_slot` |
| Only `UplinkSystem` and `SubscribeSystem` override `instance_descriptor`; the registry's `EntryDescriptor` is the type descriptor | `src/telemetry/uplink.rs:184`; `subscribe.rs:145`; `src/wiring/registry.rs` | the peer describer uses the type descriptor for statics and the subscriber's for mirrors |
| `cyclic_node` requires `S::Output: LogOutput + BindPorts`; `LogPort::emit_event(&LogEvent)` keeps the event's own timestamp and source; `LogEvent.source` is the bare instance name (`a_downlink`) | `init.rs:174-177`; `core/src/log.rs:102-107`; `tests/gateway.rs:245-246` | source log lines pass through the ingest's log port unchanged |
| `PortDesc` serializes with `conn` skipped (decodes as `Edge`) | `core/src/descriptor.rs:190-210` | a mirror's port list rides value params through the registry factory |
| `IngestSystem` is cyclic, registered `system_type_shared_many::<IngestSystem, DbState>`; `configure` unions commands into `DbState::add_commands`; `init` spawns `source_loop` as a drop guard; `session` logs `"source link connected"` with `addr`, `source`, `sessions`, which the integration test greps | `src/gateway/ingest.rs`; `src/wiring/registry.rs:262-264`; `tests/gateway.rs:397-401` | the shape stays; the session body changes |
| `RecordSystem` taps `TelemetryMode::All`, `ingest_table` keeps a frame's own timestamp, `push_msg(now, id, payload)` stamps messages with the gateway cycle | `src/gateway/record.rs:62,109-124` | unchanged; ingested frames keep source time, messages take arrival time as `handle_packet` did |
| `ResolveOptions` is `Default`, threaded through `resolve_with`; `resolve` uses the default | `resolve.rs:64-88` | `peers` joins it; every gateway test switches to `resolve_with` |
| `cmd_run`'s single-member path resolves `member` alone; `run_members` provisions and writes each member's bundle in one envelope-order loop; `Input::Bundles` loads each bundle as a member; `cmd_package` provisions one member and writes it | `src/cli.rs:483-545,610-645,349-388` | peers are provisioned before any bundle is written; a gateway bundle gets them |
| `validate_deployment` returns early for a deployment of one | `src/wiring/validate.rs:41-45` | a lone gateway bundle validates its peers at resolve instead |
| `check_ingests` already requires the source member, its link, its port, its commands, and disjoint sets | `validate.rs:111-198` | unchanged; `depth == 0` joins `check_system`'s `IngestSpec` reasons |
| `print_preflight(wiring, target)` lists IR systems; `system_detail` appends ` · ingests <ns>/<link>` | `src/cli/ui.rs:115-235` | one line per source with the instance count |
| `IngestParams { namespace, link, port, commands (default), host (default) }`; the goldens pin the four emitted fields | `ingest.rs:44-62`; `tests/golden/deployment_gateway.json` | `depth: Option<usize>` with `#[serde(default, skip_serializing_if)]` leaves the goldens byte-identical |
| Python `_Ingest(db, source, commands)`, `_resolve_ingest` emits `{namespace, link, port, commands}`; `metor-config` is `0.4.3` | `python/metor-config/metor_config/_builtins.py:261-304`; `_deployment.py:89-134`; `_version.py` | `depth` is one more constructor argument; `0.4.4` |
| The adcs `gw` member ingests `plant_link` and `fsw_link`; `gateway_member_packages_and_runs` packages `gw` alone, asserts no artifacts, and resolves without peers; `plant` holds a `Plant` pack instance and a `Subscribe(ctrl)`; `fsw` holds a `@system`, a `Subscribe(plant_sim)`, a `mode` slot, `Alarms`, `Presets`, `Uplink`, `Downlink`, `Publish` | `examples/adcs-fsw2/target.py:85-439`; `tests/bundle.rs:163-192` | the example file is unchanged; the bundle test provisions the peers and passes them |
| The gateway test asserts: both coordinators' `system_status.cycles` present; `gw.<a\|b>.source_status.connected == 1`; `LogEvent` sources `a_downlink` and `b_downlink`; one `WiringManifest` per link then `b` re-broadcasts once after `ReloadSequences`; `sessions == 2` and cycles advance after a restart; a lone gateway announces both `source_status` with no sample; the bundle run repeats case 1 | `tests/gateway.rs:225-370` | every assertion holds over the ring path; one is added for log delivery |
| `LinkInfo` carries `command_ids`; the ingest intersects them with its configured set and reports `source_commands` | `ingest.rs:334-348` | unchanged |

---

## WP1: connection tails off the WAL (`libs/db`)

The persister becomes the message WAL's only reader. Connection tails,
the mirror's message sync and the command forwarder, follow the persisted
nodes with their own cursors, the way frame streams follow a component's
head (`handle_real_time_component`, `libs/db/src/lib.rs:2162-2210`).

Files:

- `libs/db/src/msg_log_2.rs`:

  ```rust
  /// A position in the persisted nodes: the last record a tail has taken, or nothing yet.
  pub struct MsgCursor { node: Option<Arc<AtomicNode<MsgLogNode>>>, index: usize }
  impl MsgLog {
      /// The newest persisted record and a cursor just past it, off one head snapshot.
      pub fn tail(&self) -> (Option<MsgRef>, MsgCursor)
      /// Every record persisted after `cursor`, oldest first, advancing it.
      pub fn drain_after(&self, cursor: &mut MsgCursor, f: impl FnMut(Timestamp, &[u8]))
  }
  ```

  `drain_after` walks `list` from the head collecting nodes until it
  reaches `cursor.node` (`Arc::ptr_eq`), yields the rest of that node from
  `index + 1` and every newer node whole, oldest first, and leaves the
  cursor on the last record yielded; a cursor with no node yields
  everything. `flush_pending` returns whether it persisted anything and
  `persist` calls `data_waker.wake_all()` when it did, so a tail woken by
  `push` and finding nothing persisted yet is woken again once it is.
- `libs/db/src/lib.rs`, `sync_msg_log`: `log.flush_pending()?` then
  `let (latest, mut cursor) = log.tail()`; send `latest` with its
  timestamp; loop `drain_after` sending `msg_with_timestamp`, then
  `log.wait().await`. The `wal_reader`, the `seed` pair, and the skip go.
  The doc comment says the tail holds no WAL position.
- `libs/db/src/remote/mod.rs`, `forward`: `pub async fn`; per id
  `log.tail().1` at connect (no replay, as today), then
  `drain_after`/`wait_any`. Rewrite the doc comment: the live edge is the
  cursor taken at connect; the persister's wake after each pass is what
  makes a persisted-node tail live.
- `libs/db/src/remote/fsw.rs`: `fsw_stream(command_ids, rx, tx, buf, db)`,
  `on_packet` removed; `ingest` loses the callback. The gateway was its
  only user with a gauge.
- `libs/metor-panel/src/connections/target.rs:374`: drop the `|_| {}`
  argument.
- `libs/db/src/lib.rs`, the `NODE_PROTOCOL`/db docs comment near
  `handle_msg_sync` and `libs/db/README.md` if it describes the sync:
  state the rule from the revision's decision 1 in one sentence: dynamic
  readers never hold WAL positions; connections are served from persisted
  nodes.

Tests to add:

- `msg_log_2.rs` test module (new; the file has none):
  `tail_cursor_follows_records_across_nodes`: push enough records to roll
  a node (a node rolls on `MapOverflow` of its `AppendLog`; use payloads
  near the node size or many small ones), `flush_pending`, take `tail`,
  push more, flush, `drain_after` yields exactly the later records in
  order; a second `drain_after` yields nothing.
  `a_parked_tail_never_stalls_push`: take a cursor and never drain it;
  push 64 KiB messages until 4 MiB have gone through (four WAL
  capacities), yielding to the persister between batches
  (`stellarator::test`); every `push` is `Ok`.
  `persist_wakes_a_waiter`: a task parked on `log.wait()` after a `push`
  has already woken it once wakes again after the persist pass.
- `lib.rs` test module, beside `sync_msgs_does_not_duplicate_the_seed`:
  `a_stalled_sync_connection_does_not_block_push`: serve a db; a raw
  `Client` sends `SyncMsgs` and never reads its socket; push 64 KiB
  records into one log until 4 MiB have crossed; every push is `Ok`. Before
  this WP the pushes fail with `MapOverflow` once the WAL and the socket
  buffers are full.
- `metor-db-tests`: `remote_db_mirrors_message_logs` and
  `remote_db_forwards_advertised_commands_and_never_echoes` pass unchanged;
  `fsw_stream_ingests_and_forwards_live_edge` updated for the signature.

What could go wrong: `AtomicStack::iter` is newest-first and the cursor's
node may be the head, the tail, or gone from the stack (a purged node);
treat a cursor whose node is no longer reachable as "everything newer than
the head at the time", which is the newest node's records, and document
it. A record pushed between `flush_pending` and `tail` is persisted later
and lands after the cursor, so it is delivered once and `latest` never
includes it; no skip logic is needed. The 4 MiB pushes on a stalled
socket rely on the kernel send buffer filling; loopback buffers are a few
hundred KiB on macOS and Linux, well under 4 MiB.

Green when:

```sh
cargo test -p metor-db -p metor-db-tests && cargo check -p metor-panel
```

## WP2: the peer substrate (graph knobs, descriptors from sidecars, the bundle, the CLI)

Everything a gateway needs to describe a source member without loading
its code, at `resolve` and inside a bundle. No gateway behaviour changes
here; `Subscribe` mirrors stop dlopening when a sidecar exists.

Files:

- `src/coordinator/init.rs`: `Node` gains three defaulted fields:

  ```rust
  pub(crate) struct Node {
      pub(crate) name: String,
      pub(crate) desc: SystemDescriptor,
      pub(crate) bind: SystemBind,
      /// `name` is already the registry instance (a mirror minted under a source member's namespace); `qualify` leaves it alone.
      pub(crate) qualified: bool,
      /// Whether the host appends and writes `system_status`; `false` for a node whose descriptor carries its own.
      pub(crate) host_status: bool,
      /// Records per Log ring of this node, overriding `LOG_DEPTH`.
      pub(crate) ring_depth: Option<usize>,
  }
  ```

  Every constructor (`cyclic_node`, `async_node`, `pending_node`,
  `push_system`, `resolve_slot`'s literal) sets `qualified: false,
  host_status: true, ring_depth: None`. `push_node` appends
  `host_status_port()` only when `host_status`. `qualify` takes the node:
  `fn instance_of(&self, node: &Node) -> String`, returning `node.name`
  when `qualified`, else the namespaced name; `rings.rs:187,231,318,353`
  call it. The coordinator descriptor is factored:

  ```rust
  /// The coordinator's own bundle, `wiring` included when the manifest JSON is known.
  pub(crate) fn coordinator_descriptor(ir_json: Option<&str>) -> SystemDescriptor
  ```

  `InitGraph::new` calls it with `None`; `set_wiring_manifest` keeps its
  replace-in-place but builds the port through the same helper so the two
  cannot drift. `wiring_manifest_max_size` stays private to the module.
- `src/coordinator/init/rings.rs`, `alloc_rings`: the depth passed to
  `ring_config` for a Log port is `sys.ring_depth.unwrap_or(LOG_DEPTH)`;
  a Snapshot port keeps `default_depth`. `ring_config`'s signature gains
  the Log depth as an argument in place of reading the constant.
- `src/coordinator/bind.rs`, `bind_systems`: the `entry` closure takes
  `host_status: bool` and sets `status: host_status.then(|| host_status_writer(..))`.
- `src/wiring/resolve/peers.rs` (new), `mod peers;` in `resolve.rs`:

  ```rust
  /// One instance a member serves, as its own resolve would register it: the qualified name and its telemetered outputs.
  pub(super) struct PeerInstance { pub name: String, pub outputs: Vec<PortDesc> }
  /// Every instance `peer` serves, coordinator first, described from sidecars, modules, and the registry alone.
  pub(super) fn peer_instances(peer: &Wiring, registry: &Registry, cache: &mut DescribeCache) -> Result<Vec<PeerInstance>, LoadError>
  /// The described entries of an artifact: its sidecar, else a describe (dlopen for a cdylib, the interpreter for a module).
  pub(super) fn describe_artifact(wiring: &Wiring, artifact_id: &str, owner: &str, cache: &mut DescribeCache) -> Result<&[PackEntryDesc], LoadError>
  /// A peer type's telemetered outputs, the mirror arm's descriptor step, shared with `resolve_mirror`.
  pub(super) fn mirror_outputs(spec: &SystemSpec, wiring: &Wiring, registry: &Registry, cache: &mut DescribeCache) -> Result<Vec<PortDesc>, LoadError>
  ```

  `DescribeCache` is `resolve_with`'s `described` map plus the
  `WasmCache`, moved out of `resolve.rs` into this module so the mirror
  arm and the peer describer share one cache per resolve. `peer_instances`
  runs `validate::validate(peer)` first, then, in `peer.systems` order
  followed by `peer.slots`: the coordinator from
  `coordinator_descriptor(Some(&serde_json::to_string(&peer.path_stripped())))`;
  a static system from the registry's type descriptor; a `peer`-carrying
  system from `SubscribeSystem::new(peer, mirror_outputs(..)).instance_descriptor()`;
  a dl, proc, or wasm system from `describe_artifact` and the entry
  `spec.ty` names (or the sole entry, `wasm_entry`'s rule); a slot from
  `plan_slot` over `AllowedOccupant`s built with
  `OccupantBacking::Artifact(path)` from the described entries and
  `encode_occupant_params`. Every instance's output list is filtered to
  `telemetered` and gains `PortDesc::of::<SystemStatus>()` (the host
  appends it on the peer, so the peer announces it) except the
  coordinator, whose descriptor carries it. The `log` port is kept in the
  list here; the gateway drops it when it mints (WP4).
- `src/wiring/resolve.rs`: `resolve_mirror` calls `mirror_outputs` and
  keeps only the node push; the cdylib arm inside `mirror_outputs` reads
  `manifest_sidecar_bytes(path).or_else(|| describe_raw(path).ok())`
  (deviation 7). `ResolveOptions` gains
  `pub peers: Vec<Wiring>` with a doc line: the other members a gateway
  member's `Ingest`s name, built or bundle-loaded. Nothing reads it yet.
- `src/wiring/resolve/slots.rs`: `describe_occupants`'s inner `describe`
  takes its bytes through a closure so the peer describer can pass the
  sidecar reader while the process path keeps `describe_via_worker`.
- `src/wiring/bundle.rs`:
  - `PackageOptions` gains `pub peers: Vec<Wiring>` (built; their artifact
    paths are read for sidecars and modules).
  - `BundleMeta` gains `#[serde(default)] pub peers: Vec<String>`, the
    namespaces carried.
  - `bundle_members`: after the artifact members, for each peer in
    `opts.peers` (sorted by namespace): `<ns>.wiring.json` inline
    (`wiring_json(peer)`), then per artifact sorted by id: a cdylib's
    `<ns>.<cdylib>.manifest` copied from `manifest_sidecar_path(path)`,
    erroring `BundleError::PeerManifestMissing { namespace, artifact }`
    when absent; a wasm's `<ns>.<id>.wasm` copied. A peer with no
    namespace is `BundleError::PeerNamespace`.
  - `pub struct Bundle { pub wiring: Wiring, pub peers: Vec<Wiring> }`;
    `load_bundle` and `load_bundle_dir` return it. The peer loop reads
    `<ns>.wiring.json`, fills each cdylib artifact's `path` with
    `dir.join("<ns>.<cdylib>")` (the sidecar sits at `<that>.manifest`;
    the `.so` is absent by design) after checking the sidecar exists and
    its hash matches a recorded `manifest_hash`, and each wasm artifact's
    `path` with the copied module, `MissingSo`-style errors naming the
    peer.
  - Module doc: a gateway bundle carries its sources' descriptors and
    never their dylibs.
- `src/wiring/mod.rs`: re-export `Bundle`.
- `src/wiring/error.rs`: `LoadError::IngestPeer { system, namespace }`
  ("resolve has no `Wiring` for the source `<ns>`; run it from the
  deployment or a bundle packaged with its peers") and
  `LoadError::IngestPeerArtifact { namespace, artifact }` (a peer artifact
  with no sidecar and no loadable object). Used by WP4; declared here so
  the bundle and CLI tests can name them.
- `src/cli.rs`:
  - `fn ingest_sources(member: &Wiring) -> Vec<String>`: the namespaces
    its `Ingest` params name (the shape `peer_sources` already walks).
  - `fn peers_of(member: &Wiring, deployment: &Deployment, source: &Path, args) -> miette::Result<Vec<Wiring>>`:
    the named members, cloned and provisioned through
    `provision_run_artifacts`; a gateway whose sources were packaged
    without sidecars is refused here with the `--no-manifest-sidecar`
    reason.
  - `cmd_run`, single-member path: `resolve_with(member, &registry, ResolveOptions { peers, ..Default::default() })`,
    peers from `peers_of` for a source, from the loaded `Bundle` for a
    bundle.
  - `run_members`: provision every member first, then write each bundle
    with `PackageOptions { peers: peers_of(..) }` for members with ingests.
  - `Input::Bundles`: keep each `Bundle`; the launcher needs only the
    wiring, the preflight the peers.
  - `cmd_package`: provision the selected member and its sources; pass
    `peers`. Print `… and N peer descriptor(s)` when non-empty.
- `src/cli/ui.rs`: `print_preflight(wiring, target, peers: &[Wiring])`;
  `system_detail` for an `Ingest` reads
  ` · ingests a/link (4 instances)` when the peer is present, the
  instance count being `peer.systems.len() + peer.slots.len() + 1`; the
  bare form without a count when it is not (a preflight before
  provisioning).
- `examples/adcs-fsw2/tests/bundle.rs`, `gateway_member_packages_and_runs`:
  `eval_and_build("plant")` and `("fsw")` as peers, `PackageOptions { peers }`,
  assert the bundle dir holds `plant.wiring.json`, `fsw.wiring.json`, one
  `.manifest` per peer cdylib artifact, `fsw.program.wasm`, and no
  `.dylib`/`.so`; `load_bundle` yields two peers whose cdylib paths do not
  exist and whose sidecars do; `resolve_with(.., peers)` still resolves
  (the gateway does not mint mirrors until WP4, so this proves only the
  substrate). The other two bundle tests destructure `Bundle`.

Tests to add:

- `src/coordinator/init.rs` or `src/tests.rs`: a `qualified` node under a
  namespaced graph registers keys without the prefix; a `host_status:
  false` node with its own `system_status` output builds, its ring holds
  what the node writes, and the step loop leaves it alone; a
  `ring_depth: Some(200)` Log ring holds 200 records (write 200, read 200).
- `src/wiring/resolve/peers.rs`: over `WiringBuilder` peers: a peer with
  the coordinator only yields one instance whose outputs are
  `system_status`, `log`, `coordinator_status`, `sequences`, `wiring`
  sized from the peer's IR JSON; a peer with a static `Downlink` and an
  `Uplink` yields their telemetered outputs plus `system_status`; a peer
  with a `Subscribe` of a static type yields `peer_status`, `log`, the
  mirrored ports, `system_status`; the dl fixture pack
  (`tests/fixtures/dl-fixture`, under `crate::dl::FIXTURE_LOCK`)
  described from its sidecar alone, with the `.so` renamed away, yields
  the entry's outputs; a slot over two fixture entries yields the planned
  outputs. A `SubscribeSystem` mirror of the fixture pack resolves with
  the `.so` renamed away and the sidecar present.
- `src/wiring/bundle.rs`: a bundle written with one peer holds the five
  member kinds above; `load_bundle` returns the peer with the sidecar
  path resolvable; a peer whose cdylib has no sidecar fails `write_bundle`
  with `PeerManifestMissing`; a pre-peers `meta.json` loads with no peers.
- `src/cli.rs`: `ingest_sources` over a gateway wiring; `print_preflight`
  with and without peers renders the count.

What could go wrong: `Node` literal constructions outside `init.rs`
(`resolve_slot`, the tests' `Node {..}`) need the new fields; the compiler
finds them. `qualified` interacts with the ring file name for
process-crossing outputs (`{instance}.{port}.ring`); mirrors are never
process-crossing, but the name path is shared and must not double-prefix.
The sidecar describe for a wasm artifact in `program.rs::decode_manifest`
tries `manifest_sidecar_bytes(path)` first too; the peer describer for a
wasm module goes through `WasmCache::open`, which needs the bytes, so the
module itself is the bundle member. `NAME_CAP` is 100 bytes; a long
namespace plus a per-triple cdylib name plus `.manifest` can exceed it;
the writer's `validate_member_names` reports it and the fix is a shorter
namespace, documented. `run_members` today provisions and writes in one
loop; splitting it means a build failure in member 3 is reported before
bundle 1 is written, which is fine.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets && cargo test -p adcs-fsw2
```

## WP3: the subscribe client over mirror sets

`bind`, `route`, and the replay fold become functions over a set of
mirrors, each with its own prefix and ports, and over a writer table
rather than `SubscribeOut`. `SubscribeSystem` becomes the one-mirror caller.

Files:

- `src/telemetry/subscribe.rs`:

  ```rust
  /// One mirror the announce binds: the `<ns>.<instance>` its channels sit under and the ports it fills.
  pub(crate) struct MirrorPorts { pub prefix: String, pub ports: Vec<PortDesc> }
  /// Where an arriving packet goes: `(mirror, port)` per announced id.
  pub(crate) struct Routes { tables: HashMap<PacketId, (usize, usize)>, msgs: HashMap<PacketId, (usize, usize)>, refused: HashSet<PacketId> }
  pub(crate) struct Announced { .. }   // unchanged shape, now pub(crate)
  pub(crate) fn take_announce(pkt: &OwnedPacket<Slice<Vec<u8>>>, announced: &mut Announced) -> bool
  /// A port the announce did not cover (`Missing`) or covered with another shape (`Mismatch { component }`), with its mirror's prefix.
  pub(crate) enum BindFault { Missing { prefix: String, port: String }, Mismatch { prefix: String, port: String, component: String } }
  /// Bind every mirror's ports; a message id carried by several mirrors binds to the first (deviation 3); `skip` ids bind to nothing and are not refused. Faults come back as data, the `collect_taps` shape, so a caller with no log port can count them.
  pub(crate) fn bind(mirrors: &[MirrorPorts], announced: &Announced, skip: &[PacketId]) -> (Routes, Vec<BindFault>)
  /// Copy one packet into the writer its id binds; `Ok(true)` written, `Ok(false)` unbound, `Err` full.
  pub(crate) fn route(pkt: &OwnedPacket<Slice<Vec<u8>>>, routes: &Routes, writers: &mut [Vec<Writer<NoWake>>], record: &mut Vec<u8>) -> Result<bool, ()>
  ```

  `session` builds `[MirrorPorts { prefix: "<ns>.<instance>", ports }]`,
  passes `writers = [output.ports]` through a one-element slice, logs each
  `BindFault` through `missing`/`mismatch` as today (now naming the
  prefix beside the port), and keeps the gauge and cancellation exactly
  where they are. The module doc gains a sentence: the same functions
  bind a gateway's whole member.
- `src/telemetry/mod.rs`: nothing; `subscribe` stays `pub(crate) mod`.

Tests to add (`subscribe.rs` test module, beside `bind`'s existing cases):

- `bind_routes_two_mirrors_by_prefix`: an announce carrying `a.x.tick` and
  `a.y.tick` groups binds `x`'s port to the first id and `y`'s to the
  second; a group under `a.z` binds nothing and refuses nothing.
- `a_shared_message_id_binds_once`: two mirrors both carrying a `Beat`
  port; the announced id routes to `(0, port)`; no fault.
- `skipped_ids_bind_nowhere`: `skip = [LogEvent::ID]` leaves the id out of
  `routes.msgs` and out of `refused`.
- `route_reports_full_and_unbound`: a one-record ring returns `Err` on
  the second write; an unannounced id returns `Ok(false)`.
- The existing `a_connected_peer_fills_the_mirror`,
  `a_renamed_field_refuses_the_port`, `an_unannounced_port_is_refused_once`,
  `a_message_schema_mismatch_refuses_the_port`, and
  `a_wrong_identity_moves_to_the_next_candidate` pass unchanged.

What could go wrong: `route` today takes `&mut SubscribeOut` for the
writers and `&mut Gauge` for the counters; splitting the counters out is
what makes it callable from a task with no gauge of that shape. The
`refused` set is per id, and with several mirrors one id refused for
mirror `x` and bound for `y` cannot happen (ids are per group and groups
are per instance), but the message arm must not insert into `refused`
after binding the id for an earlier mirror: bind in mirror order and skip
an id already routed.

Green when:

```sh
cargo test -p metor-fsw-2 --lib subscribe && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP4: the gateway ingests into rings

`Ingest` mints one mirror per instance of its source, the client feeds
them over one connection, `Record` drains them. `fsw_stream` leaves the
gateway.

Files:

- `src/gateway/mirror.rs` (new), re-exported from `src/gateway/mod.rs`
  and `src/lib.rs`:

  ```rust
  /// Wiring params of one ingested mirror (`type="IngestMirror"`, attached to a `Db`), minted at resolve, never written by a front-end.
  pub struct MirrorParams { pub namespace: String, pub instance: String, pub ports: Vec<PortDesc> }
  /// The mirror's outputs: its (unused) log, then one raw writer per mirrored port.
  pub struct MirrorOut { log: LogPort, ports: Vec<Writer<NoWake>> }
  /// A passive mirror of one source instance: its rings are written by the source's ingest client.
  pub struct MirrorSystem { db: Option<Shared<DbState>>, params: MirrorParams }
  impl BuildSystem for MirrorSystem { type Params = MirrorParams; }
  impl System for MirrorSystem { type Input = (); type Output = MirrorOut; const NAME = "ingest_mirror"; fn instance_descriptor(&self) -> SystemDescriptor; fn init(&mut self, output) }
  impl CyclicSystem for MirrorSystem { fn execute(..) {} }
  ```

  `instance_descriptor` is `[log] ++ params.ports` with every Table port's
  `delivery` set to `Log` (deviation 4) and every port `conn: Edge`.
  `init` moves `output.ports` into
  `db.get().hand_off(namespace, MirrorSlot { instance, ports, writers })`.
  `MirrorOut::bind` binds `log` first then drains `try_next_output`, the
  `SubscribeOut` shape.
- `src/gateway/mod.rs`, `DbState`:

  ```rust
  /// One minted mirror's ports and the writers its node bound, handed over in its `init` for the source's ingest to take in its own.
  pub(crate) struct MirrorSlot { pub instance: String, pub ports: Vec<PortDesc>, pub writers: Vec<Writer<NoWake>> }
  impl DbState {
      pub(crate) fn hand_off(&mut self, namespace: &str, slot: MirrorSlot)
      pub(crate) fn take_mirrors(&mut self, namespace: &str) -> Vec<MirrorSlot>
  }
  ```

  A `sources: HashMap<String, Vec<MirrorSlot>>` field with a doc line
  naming it the resolve-time hand-off between an ingest and its mirrors,
  empty once every ingest has initialised. Identity and commands are
  unchanged.
- `src/gateway/ingest.rs`:
  - `IngestParams` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub depth: Option<usize>`,
    doc: records each ingested frame ring holds; `None` is one second of
    the source's cycle rate, never below `LOG_DEPTH`.
  - `IngestSystem` gains `mirrors: Vec<MirrorSlot>` (taken in `init`) and
    `logs: Rc<RefCell<Vec<LogEvent>>>`.
  - `init`: `self.mirrors = db.take_mirrors(&namespace)`; spawn
    `source_loop(params, commands, db, counters, mirrors, logs)`; the
    writers move into the task.
  - `execute`: drain `logs` into `output.log().emit_event(&ev)` before the
    gauge; the `SourceStatus` fields keep their names with `packets` now
    "records written into mirror rings" and `rejected` "records refused: a
    full ring, an unannounced or mismatched channel, an unparseable log
    line". Doc the struct accordingly.
  - `session`: identity as today; then the subscribe shape over the
    connection: fold the replay with `take_announce`, then
    `bind(&mirror_ports, &announced, &[LogEvent::ID])`. The task owns no
    log port, so the returned `BindFault`s are counted into the counters
    (`missing`, `mismatched`) with each one's prefix and port named
    through `tracing::warn!`; `execute` reports one `source_channels`
    fault per connection carrying both counts. A `LogEvent` packet is
    parsed and pushed onto `logs`. Every other packet goes through
    `route`; `Ok(true)` bumps `packets`, anything else `rejected`. The
    read loop is raced with `metor_db::remote::forward(forwarded, tx, &db)`
    the way `fsw_stream` raced it, so commands still go up the link. The
    `"source link connected"` tracing line keeps its fields.
  - `configure` unchanged.
- `src/wiring/resolve.rs`, the systems pass: an `INGEST_TYPE` spec is
  resolved through a new arm before `resolve_static`:

  ```rust
  /// Mint one mirror node per instance of the source member, then the ingest itself.
  fn resolve_ingest(spec: &SystemSpec, wiring: &Wiring, registry: &Registry, state_tokens: &HashMap<&str, AttachTarget>, opts: &ResolveOptions, cache: &mut DescribeCache, graph: &mut InitGraph) -> Result<(SystemHandle, SystemDescriptor), LoadError>
  ```

  It decodes `IngestParams`, finds the peer in `opts.peers` by namespace
  (`IngestPeer` when absent), calls `peer_instances`, and for each
  instance builds a synthesized `SystemSpec { name: "<ns>.<instance>", ty: INGEST_MIRROR_TYPE, params: Value(MirrorParams{..}), attach: spec.attach }`
  with the instance's outputs minus `log`, constructs it through the
  registry exactly as `resolve_static` does but sets `qualified: true`,
  `host_status: false`, `ring_depth: Some(depth)` on the returned `Node`
  before `push_node`. `depth` is `params.depth` or
  `peer.coordinator.cycle_rate.ceil() as usize`, `.max(LOG_DEPTH)`. Then
  `resolve_static(spec, ..)` for the ingest. Mirror instances are not
  entered into `instances` (no edge can name them; plan D reaches them
  through the registry), so the `instances` map and `validate`'s
  name-uniqueness rule are untouched.
- `src/ir.rs`: `pub const INGEST_MIRROR_TYPE: &str = "IngestMirror";`.
- `src/wiring/registry.rs`, `with_builtins`:
  `gateway_pack.system_type_shared_many::<MirrorSystem, DbState>("IngestMirror", |p, db| MirrorSystem::new(p).attach(db))`.
- `src/wiring/validate.rs`, `check_system`: an `IngestMirror` in the IR
  is `IngestSpec { reason: "minted at resolve; not a front-end type" }`;
  `depth == Some(0)` is an `IngestSpec` reason.
- `src/wiring/resolve.rs`, `resolve_with`: `ResolveOptions.peers` is
  read here and nowhere else.
- `src/gateway/tests.rs`: `gateway()` builder tests gain a `peer(ns,
  port)` helper building a `WiringBuilder` member with a `TcpServer` on
  the port, a `serve`d downlink, and one frame producer from
  `src/tests.rs`; every `resolve(..)` becomes `resolve_with(.., peers)`.
- `Cargo.toml`: nothing; `metor-db` is already a dependency and `forward`
  is `pub` after WP1.

Tests to add (`src/gateway/tests.rs`, `src/gateway/mirror.rs`):

- `an_ingest_mints_one_mirror_per_instance`: a gateway with `Ingest` of
  peer `a` (coordinator, a `Ticker` producer, a downlink) resolves; the
  coordinator's registry holds `a.coordinator.system_status`,
  `a.coordinator.wiring`, `a.ticker.tick`, `a.ticker.system_status`,
  `a.telemetry.link_status`, and `gw.a.source_status`, and no
  `gw.a.ticker.*`; the `a.ticker.tick` ring is Log delivery with
  `max(ceil(rate), 64)` records (write until full, count).
- `depth_overrides_the_default`: `depth: Some(300)` gives 300.
- `a_missing_peer_is_a_resolve_error`: no peers → `IngestPeer` naming
  the namespace and the system.
- `a_session_fills_the_mirrors`: a fake link (the `fake_link` helper,
  extended to replay a `VTableMsg` + `SetComponentMetadata` group under
  `a.ticker.tick` and a `SetMsgMetadata` for `LogEvent`, then one table
  record and one `LogEvent` msg) against `session` with a hand-built
  `MirrorSlot` over in-memory rings; the record lands in the tick ring
  byte for byte; the `LogEvent` is on `logs` with its own timestamp and
  source; `packets == 1`; an unannounced port is counted once.
- `a_log_burst_reaches_the_ingest_log`: after a cycle, the ingest's
  `gw.a.log` ring holds the forwarded event and `execute` counted it.
- `the_gateway_records_a_mirrored_member`: two `WiringBuilder` targets
  in one test: a peer `a` served on a free port with a producer
  publishing every cycle at 1000 Hz, and a gateway ingesting it at 100
  Hz with `Record`; run both coordinators on one runtime for 50 gateway
  cycles; the gateway's db holds `a.ticker.tick` with a sample count
  within a factor of two of the peer's cycles (log delivery kept them),
  `a.coordinator.system_status`, the peer's `LogEvent` lines, and
  `gw.a.source_status.connected == 1`.
- `a_command_still_goes_up_the_link`: the fake link advertises a command
  id; a record pushed into the gateway's db arrives at the fake link (the
  `forward` race).
- `ingests_advertise_the_union_of_their_commands` and
  `an_unknown_command_token_is_a_resolve_error` updated to pass peers.

What could go wrong: the writers are `Writer<NoWake>` over public rings,
written from a task between cycles; the `Record` view reads them during
the cycle; the runtime is single-threaded so the two never overlap, but a
`try_write` on a full ring fails rather than waits, which is the lossless
contract and is counted. `MirrorOut`'s `log` is the node's own, never
written, and costs one `LOG_DEPTH` message ring per instance (~256 KB);
noted as the price of `cyclic_node`'s `LogOutput` bound, not fixed here.
The ingest's `log` ring is `LOG_DEPTH` deep and `emit_event` fails on a
burst of more than 64 source lines in one gateway cycle; `LogPort` counts
the drop and reports it at the next flush. A `SetMsgMetadata` for
`LogEvent` is announced once per member; skipping the id in `bind` keeps
it out of `refused` so the parse path is the only one counting it.
`peer_instances` on the gateway resolves the source's `Subscribe` mirrors
through `mirror_outputs`, which reads the *source's* artifacts; those are
in the source's `Wiring`, which the bundle carries.

Green when:

```sh
cargo test -p metor-fsw-2 --lib gateway && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP5: Python `depth=`, integration, the example, docs

Files:

- `python/metor-config/metor_config/_builtins.py`: `_Ingest.__init__(self, db, source, commands, depth)`;
  `Ingest(db, source, commands=None, depth=None)` with a doc sentence on
  `depth`; `_Ingest._bind` rejects `depth == 0`.
- `_deployment.py`, `_resolve_ingest`: adds `"depth": ingest.depth` only
  when it is not `None`, so the goldens stay byte-identical.
- `_version.py`, `pyproject.toml`: `0.4.4`.
- `python/tests/test_recorder.py`, `GatewayTest`: `depth=200` is emitted;
  `depth=0` is rejected; the default emits no `depth` key.
- `tests/fixtures/gateway_target.py`: unchanged members; the module doc
  says the gateway holds one mirror per instance of each and the db is
  fed by `Record`. `a` gains `Ingest(db, link_a, depth=50)` on the gateway
  to exercise the param end to end.
- `tests/gateway.rs`:
  - `both_sources_live` also waits until `a.coordinator.system_status`'s
    sample count over one second is at least half the member's cycle rate
    (100 Hz wall clock): log delivery keeps every sample where the
    db-direct path kept every batch, so the check is the same count but
    now taken off a ring drained once per gateway cycle. Use `samples`
    from `src/gateway/tests.rs`'s shape over `time_series.get_range`.
  - Case 2 (`WiringManifest` counts and the `ReloadSequences` two-hop) is
    unchanged: the command goes gateway db → `forward` up `b`'s link.
  - Case 3 (`sessions == 2`) unchanged.
  - Case 4 (`--target gw --cycles 200` from source): unchanged
    assertions; the run now provisions `a` and `b` as peers (no
    artifacts, so no build) and resolves their instances.
  - Case 5 (three bundles): unchanged assertions; `gw.bundle` now carries
    `a.wiring.json` and `b.wiring.json`; add an assertion that the bundle
    dir has them and no `.manifest` (the fixture members have no packs).
  - The stderr assertion on `"source link connected"` stays.
- `examples/adcs-fsw2/target.py`: unchanged. Re-verify by hand:
  `cargo run -p metor-fsw-2 --bin metor-fsw -- run examples/adcs-fsw2/target.py`,
  connect the panel to `:2250`, confirm `plant.plant.*`, `fsw.*` and
  `gw.*` components, the Logs pane showing both members, the dashboards
  unchanged, and `--target gw` alone preflighting
  `ingests plant/link (N instances)`. `cargo test -p adcs-fsw2` covers the
  bundle round trip from WP2.
- `docs/telemetry.md`, "Gateway": rewritten for the ring path: a `Db`
  state; `Ingest(db, link)` mints one mirror per instance of the member
  (the coordinator included) under the member's own names, from the
  member's pack manifests, its `program.wasm`, and the registry; frame
  rings use log delivery `depth` deep (one second of the source's rate by
  default) so a slower gateway keeps every sample; one connection per
  member binds the announce to every mirror and routes records; message
  records route once by id; the member's log lines pass through the
  ingest's own log; `Record` drains every ring into the db, frames with
  their source timestamps; the fault kinds (`source_identity`,
  `source_disconnect`, `source_commands`, `source_channels`,
  `record_table_conflict`, `record_rejected`); `source_status` with its
  new field meanings; commands still two hops, `forward` up each link;
  the db's connection tails follow persisted nodes (decision 1, in one
  sentence, pointing at the db docs).
- `docs/cli.md`, "Deployments": a gateway bundle carries its sources'
  `wiring.json`, manifest sidecars, and programs, never their dylibs;
  `package --target gw` needs the sources buildable with sidecars;
  `run gw.bundle` needs nothing else; the preflight count.
- `docs/packaging.md`, the bundle member list (lines ~130-141): the peer
  members and `meta.json`'s `peers`.
- `docs/wiring.md`, the gateway paragraph (~104-107): one sentence on
  mirrors per instance.
- `docs/design-deployment-gateway.md`, "Revised": mark change 3 landed
  with this plan's name; no other edit.

What could go wrong: the sample-rate check in case 1 needs a wall-clocked
member; the fixture is wall-clocked at 100 Hz already. A `Db(path=None)`
temp store per run is unchanged. The panel's Logs pane filters by
`source`, which the forwarded events keep. The example's `fsw` member has
a `process=#true`-free slot; the gateway plans it from sidecars, so the
adcs bundle test must build `adcs_pack` with sidecars (the default).

Green when:

```sh
(cd python && uv run python -m unittest discover tests) && cargo test -p metor-fsw-2 --test gateway --test ir_contract --test py_eval && cargo test -p metor-fsw-2 && cargo test -p adcs-fsw2
```
