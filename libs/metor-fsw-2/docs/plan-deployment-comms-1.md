# Plan: deployments, cross-target comms (1 of 2: foundation)

Implements the first half of
[design-deployment-comms.md](design-deployment-comms.md). Six work packages.
Each ends with a green tree. At the end a `Subscribe` records, validates,
resolves, and runs as a node that publishes `peer_status` disconnected and
never connects; the client is [plan 2](plan-deployment-comms-2.md). Paths
are relative to `libs/metor-fsw-2` unless they start with `libs/`,
`examples/`, or `python/`.

Test commands used throughout:

```sh
cargo test -p metor-proto-stellar -p metor-db          # WP1
cargo check -p metor-panel                             # WP1, WP3 (compile only)
cargo test -p metor-fsw-2                              # Rust unit + integration
cargo test -p metor-fsw-2-core                         # core (WP3)
cargo clippy -p metor-fsw-2 --all-targets              # lints, house rule
cargo test -p metor-fsw-2 --test ir_contract           # WP4
(cd python && uv run python -m unittest discover tests) # WP5; pyright optional
cargo test -p adcs-fsw2                                # example (skip w/o python3)
```

## Sequencing

```text
WP1 ──────────────────────┐
WP2 ──────────────────────┤
WP3 ──────────────────────┼─> WP6
WP4 ─> WP5 ───────────────┘
```

WP1 (the `identify` move), WP2 (the instance-filter fix), and WP3 (link
identity) touch disjoint code and can run in parallel; each is its own
commit. WP4 (IR) precedes WP5 (Python), which emits the new fields. WP6
(the mirror at resolve) needs WP4's `PeerSpec` and WP3's `StateCtx` only
through `with_builtins`, so it waits for all of them.

## Deviations from the design doc

Found while grounding the plan. The design doc was corrected where noted.

1. The mirror's descriptor cannot come from `describe_occupants`
   (`src/wiring/resolve/slots.rs`): that is the process-slot path and runs a
   describe *worker*. The in-process shape is `dl::describe_raw` +
   `dl::decode_pack_manifest` (`src/dl.rs:243`, `:273`), which dlopens,
   describes, and closes without creating any entry; `DlPack::system`
   would run the peer type's create phase in the subscriber. WP6 uses the
   former, cached per artifact id. Design doc corrected.
2. `Subscribe` is not a `Registry` type. A registry factory constructs from
   params through `BuildSystem::new`; the mirror's descriptor is the peer
   type's, so it is a fourth `resolve` arm keyed on `SystemSpec::peer`,
   beside dl/proc/wasm. Design doc corrected.
3. A shared-state constructor sees only its params (`Pack::shared_state`,
   `core/src/pack.rs:293`, `F: FnMut(P) -> Result<S, E>`), so `LinkState`
   knows neither the target namespace (for `ns=` and the name default) nor
   its own declaration name (for `link=`). WP3 gives the closure a
   `StateCtx { name, namespace }` and threads `namespace` into
   `EntryParams::Value`. Five call sites in `src/tests.rs`, one in
   `with_builtins`, two doc examples. Design doc corrected.
4. The `LinkInfo` identity test helper lives in `libs/db/src/remote/fsw.rs`
   (`identity()`, line 215) and the link's own test builds one at
   `src/telemetry/link/tests.rs:68`. Both gain the v2 fields; WP1 moves the
   first with `identify`.
5. `StateHandle` (`_model.py`) carries only a name. `Subscribe`'s port-0
   check reads the publisher's server `addr`, so WP5 gives `StateHandle` its
   `spec`, the same way `SystemHandle` gains `target`/`spec`.

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| `identify`/`Peer` are `pub` in `libs/db/src/remote/fsw.rs` and re-exported by `remote/mod.rs`; the panel imports them from `metor_db::remote` | `libs/metor-panel/src/connections/target.rs:18,366,428` | a re-export in db keeps the panel untouched |
| `metor-proto-stellar` already depends on `metor-proto-wkt` and has its own `Error` with `From<stellarator::Error>` | `libs/metor-proto/stellar/Cargo.toml`, `src/lib.rs:169` | `identify` moves with no new dependency; `Peer::Db(DbInfoResp)` decodes there |
| `handshake_request`/`HANDSHAKE_TIMEOUT` (5 s) are `pub(super)` in `libs/db/src/remote/db.rs:125` | | stellar gets its own 5 s bound; db keeps its helper for `mirror` |
| `postcard::from_bytes` does not check for unread bytes | `~/.cargo/registry/.../postcard-1.*/src/de/mod.rs:12` | a v1 reader decodes a v2 `LinkInfo` |
| `LinkInfo` is built once, in `LinkState::set_announces` | `src/telemetry/link/mod.rs:300` | one construction site to extend |
| `TXT_NAMESPACE = "ns"` exists and has no user | `libs/metor-proto/wkt/src/msgs.rs:1138` | WP3 uses it; `TXT_LINK` is new |
| `RegistryEntry::instance` is `InitGraph::qualify(&sys.name)` | `src/coordinator/init/rings.rs:187,231` | `TelemetryMode::Subset` compares a qualified name to a bare one (WP2) |
| `AlarmSystem::configure` qualifies with `ctx.namespace` | `src/alarm/mod.rs:463` | the precedent WP2 copies |
| `Binder` implements `try_next_output`; `MsgFanOut::bind` drains it | `core/src/binder.rs:248`, `core/src/message.rs:238` | a dynamic output bundle binds through the host path |
| `AsyncSystem::instance_descriptor` is overridable; `init::async_node` is `pub(crate)` | `src/async_system.rs:72`, `src/coordinator/init.rs:189` | the mirror registers as an async node with a per-instance descriptor |
| `System::Output: SystemOutput + BindPorts` | `core/src/system/mod.rs:196` | the mirror's bundle declares its static ports in `decls()` and takes the rest dynamically, as `UplinkOut` does (`src/telemetry/uplink.rs:60`) |
| `EntryDescriptor` is a private enum with one method | `src/wiring/registry.rs` | WP6 adds a `descriptor()` accessor for a static peer type |
| `apply_overrides` edits the `TcpServer` state's `params["addr"]` in place | `src/cli.rs:714` | plan 2's `--peer` follows the same shape |
| `IR_VERSION = 10`; every `tests/golden/*.json` with an `ir_version` says 10; `docs/wiring.md`, `docs/README.md` say v10 | `src/ir.rs:17` | WP4 bumps all of them |
| `metor-config` is `0.4.1` in `_version.py` and `pyproject.toml` | `python/metor-config` | WP4 goes to `0.4.2`; the wheel's `metor-config>=0.4,<0.5` pin (shared-config doc) still covers it — confirm the generated requirement in `src/wiring/wheel.rs`/`pack_dist.rs` |
| `WiringBuilder` has `serve`, `serve_named`, `uplink`, `system`, `connect`, `state_value` | `src/wiring/builder.rs:145–325` | `publish`/`subscribe` sit beside them |
| `Deployment.__init__` walks members already | `python/metor-config/metor_config/_deployment.py` | the shape checks attach there |
| `Target.state` does not call `Spec._bind`; `add`/`slot` do | `_target.py` | WP5 adds the call so a state spec can see its target |

---

## WP1: `identify` and `Peer` move to `metor-proto-stellar`

Files:

- `libs/metor-proto/stellar/src/lib.rs`: add

  ```rust
  /// What answered at one shared-protocol address: a metor-db, or an fsw link with its identity already read.
  pub enum Peer { Db(DbInfoResp), Fsw { info: LinkInfo, rx: PacketStream<OwnedReader<TcpStream>>, tx: PacketSink<OwnedWriter<TcpStream>>, buf: Vec<u8> } }
  /// Dial `addr` and let the first packet say what lives there; bounded by a 5 s handshake timeout.
  pub async fn identify(addr: SocketAddr) -> Result<Peer, Error>
  ```

  moved verbatim from `libs/db/src/remote/fsw.rs:35–82` (`Peer`,
  `identify`, `identity_err`), with `handshake_request` replaced by a local
  `futures_lite::future::or` against `stellarator::sleep(5 s)` and the
  error mapped onto `stellar::Error` (`std::io::Error` →
  `stellarator::Error`, which `Error::Stellar` wraps). `identity_err`
  returns that shape too.
- `libs/db/src/remote/fsw.rs`: delete the moved items; `use
  metor_proto_stellar::{Peer, identify};` for `fsw_stream`'s signature,
  which does not change. Move `identify_tells_an_fsw_link` and
  `identify_times_out_on_a_silent_peer` (lines ~272–308) with a trimmed
  `fake_fsw` (identity + one data packet, no inbound recording) into
  `libs/metor-proto/stellar/src/tests.rs`; `identify_tells_a_db_server`
  stays (it needs `crate::Server`).
- `libs/db/src/remote/mod.rs:13`: `pub use fsw::{Peer, fsw_stream, identify}`
  becomes `pub use fsw::fsw_stream; pub use metor_proto_stellar::{Peer, identify};`.
- `libs/db/Cargo.toml`: no change; db already depends on stellar.
- `libs/metor-panel/src/connections/target.rs`: no change.

Tests to add: none beyond the moved ones.

What could go wrong: `Peer` was defined in db, so `fsw_stream` matched on
its fields; the moved enum's field types are stellar's own
(`PacketStream`/`PacketSink`), unchanged. The stellar `tests.rs` uses
`#[stellarator::test]` already, so the moved tests keep their attribute. An
`Error` conversion gap surfaces as a compile error in `fsw_stream`'s call
sites, which return `crate::Error` from `metor_proto_stellar::Error` via the
existing `From`.

Green when:

```sh
cargo test -p metor-proto-stellar -p metor-db && cargo check -p metor-panel
```

## WP2: `Downlink` instance filter under a namespace

Files:

- `src/telemetry/mod.rs`: `impl BuildSystem for TelemetrySystem` gains

  ```rust
  fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError>
  ```

  that, under `TelemetryMode::Subset`, rewrites each `instances` entry to
  `format!("{ns}.{name}")` when `ctx.namespace` is `Some`, the
  `AlarmSystem::configure` shape. `frames` is untouched: it matches
  `RegistryEntry::name`, which is not qualified. The `DownlinkParams` doc
  comment says the list is bare instance names.
- `docs/telemetry.md`, "Target setup": one sentence after the subset
  example: instance names are bare; the namespace is applied for you.

Tests to add (`src/telemetry/mod.rs` test module, or `src/tests.rs` beside
the other link tests):

- `subset_matches_qualified_instances`: a `Subset { instances: ["nav"] }`
  after `configure` with `namespace: Some("sat")` matches an entry whose
  `instance` is `"sat.nav"` and not one whose instance is `"nav"`; with
  `None` the reverse.

What could go wrong: the registry factory already calls `configure` for
every static system (`factory`, `src/wiring/registry.rs`), so nothing else
needs to change. A target that worked around the defect by writing
qualified names in `instances=` breaks; the design accepted that
(decision 7).

Green when:

```sh
cargo test -p metor-fsw-2 --lib telemetry && cargo clippy -p metor-fsw-2 --all-targets
```

## WP3: link identity: `StateCtx`, `LinkInfo` v2, TXT records, name default

Files:

- `core/src/pack.rs`:
  - `EntryParams::Value` gains `namespace: Option<&'a str>`; the four
    construction sites (`resolve_state`, `src/wiring/resolve.rs:~300`; the
    pack-entry factory in `src/wiring/registry.rs`; slots and tests, found
    by the compiler) pass the target namespace, `None` where none exists.
  - `pub struct StateCtx<'a> { pub name: &'a str, pub namespace: Option<&'a str> }`
    and `shared_state`'s bound becomes `F: FnMut(StateCtx<'_>, P) -> Result<S, E>`,
    built from the `EntryParams::Value` fields inside the `create` closure
    (a `Postcard` params surface has no name; states never take one, and
    `resolve_state` proves it).
- `src/tests.rs:339,380,396,418,717`, `docs/packs.md:113`,
  `docs/system.md:213`: closures take `|_, p|` (or use the ctx).
- `src/wiring/registry.rs:226`: `LinkState::bind(p.addr).map(|s| s.with_identity(ctx.name, ctx.namespace, p.name))`.
- `src/telemetry/link/mod.rs`:
  - `LinkState` gains `link: String` and `namespace: Option<String>`;
    `with_name` becomes `with_identity(link: &str, namespace: Option<&str>, name: Option<String>)`.
  - `set_announces` fills `LinkInfo { protocol_version, features: 0, command_ids, namespace, link }`.
  - `SharedLifecycle::start` computes the advertised name as
    `name.or(namespace).unwrap_or(hostname)` and passes `namespace` and
    `link` to `advertise`.
- `src/telemetry/discovery.rs`: `advertise(name, addr, namespace: Option<&str>, link: &str)`
  adds `(TXT_NAMESPACE, ns)` when present and `(TXT_LINK, link)` to `props`.
- `libs/metor-proto/wkt/src/msgs.rs`: `LinkInfo` gains
  `pub namespace: Option<String>, pub link: String` after `command_ids`;
  `LINK_PROTOCOL_VERSION = 2`; `pub const TXT_LINK: &str = "link";`. The
  `link_info_round_trips` test (line ~1609) gains the fields and a second
  case decoding a v2 encoding through a local v1-shaped struct.
- `src/telemetry/link/tests.rs:68` and the moved stellar test helper: add
  the two fields.
- `src/wiring/validate.rs`: `check_link_names(wiring)`: among states with
  `ty == TCP_SERVER_TYPE`, at most one may lack `params.name`, and present
  names are distinct; new `LoadError::LinkNameRequired { state }` and
  `LoadError::DuplicateLinkName { name }` in `src/wiring/error.rs`.
- `docs/telemetry.md`, "Local discovery": the name default, the two TXT
  keys, the several-servers rule.

Tests to add:

- wkt: the round-trip and v1-decodes-v2 cases above.
- `src/telemetry/link/tests.rs`: the replayed identity carries `namespace`
  and `link` (extend `link_replays_fans_out_and_ingests…`, which already
  reads the first packet).
- `src/wiring/validate.rs`: two unnamed servers → `LinkNameRequired`; two
  servers named alike → `DuplicateLinkName`; one unnamed plus one named →
  `Ok`.
- `core`: a `shared_state` closure sees `ctx.name == "link"` and the
  namespace it was created under (`core/src/tests.rs` or `src/tests.rs`,
  wherever the existing shared-state tests live).

What could go wrong: the panel decodes `LinkInfo` through
`identify` only, so it needs no change; a panel older than this change
reads v2 fine (postcard ignores the tail). `ServiceInfo::new` takes the
props as `&[(&str, &str)]`; a `Vec` built conditionally is the simplest
shape. mDNS is skipped on loopback, so no unit test exercises `advertise`;
the TXT keys are checked by inspection and by plan 2's browse test.

Green when:

```sh
cargo test -p metor-fsw-2-core -p metor-proto-wkt && cargo test -p metor-fsw-2 && cargo check -p metor-panel && cargo clippy -p metor-fsw-2 --all-targets
```

## WP4: IR 11

Files:

- `src/ir.rs`:
  - `IR_VERSION = 11`.
  - `pub struct PeerSpec { pub namespace: String, pub link: String, pub port: u16, pub instance: String, pub telemetered: bool }`
    with the design's field docs, `Clone, Debug, PartialEq, Serialize, Deserialize`;
    `telemetered` is `#[serde(default = "…true")]` and skipped when true
    (decision 1's opt-out).
  - `SystemSpec` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub peer: Option<PeerSpec>`.
    Every struct literal gains `peer: None`: `link_builtin` (`src/ir.rs`),
    `SystemSpecBuilder::end` (`src/wiring/builder.rs`), `maximal()`
    (`tests/ir_contract.rs:23`), and whatever else the compiler names.
  - `Deployment` gains `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub hosts: BTreeMap<String, String>`;
    `path_stripped` and the one-member wrappers in `src/cli.rs` and
    `src/wiring/py.rs` fill it (clone or empty).
- `src/wiring/validate.rs`:
  - `check_system`: with `peer` set, `params == None`, `!process`,
    `attach.is_none()`, `peer.port != 0`, and `peer.namespace !=
    wiring.coordinator.namespace` → `LoadError::PeerSpec { system, reason }`
    (one variant, a `reason` string; four reasons is not four types).
  - `check_downlinks`: at most one system with `ty == DOWNLINK_TYPE` per
    `attach` → `LoadError::DuplicateDownlink { state }`.
  - `validate_deployment`: every `hosts` key names a member
    (`LoadError::UnknownHost { namespace }`); every member's `peer.namespace`
    names another member (`LoadError::UnknownPeer { system, namespace }`).
- `src/wiring/error.rs`: the four variants, miette-rendered like their
  neighbours.
- `src/wiring/builder.rs`:

  ```rust
  /// A `Downlink` on `state` tapping `instances`, the `Publish` shape.
  pub fn publish<'a>(self, state: &str, instances: impl IntoIterator<Item = &'a str>) -> Self
  /// A mirror of a peer instance, of type `ty` from `artifact` (`None` for a static type).
  pub fn subscribe(self, name: &str, ty: &str, artifact: Option<&str>, peer: PeerSpec) -> Self
  ```

- `python/metor-config/metor_config/_version.py`: `IR_VERSION = 11`,
  `__version__ = "0.4.2"`; `python/metor-config/pyproject.toml` `0.4.2`.
  (The emitter is WP5; bumping the version here keeps `ingest_ir`'s exact
  match true for every commit between.)
- `tests/golden/target.json`, `deployment.json`, and any other golden with
  `"ir_version": 10` (`grep -l '"ir_version": 10' tests/golden`): 11.
- `docs/wiring.md` ("The current IR version is 10"), `docs/README.md`
  ("wiring IR v10"): 11.

Tests to add (`tests/ir_contract.rs`, `src/wiring/validate.rs`):

- `maximal()` gains a `peer: Some(PeerSpec {..})` system; the envelope
  round trip gains a `hosts` entry; a v10-shaped document (no `peer`, no
  `hosts`) deserializes with both defaulted after its version is edited.
- `validate`: each `PeerSpec` reason; a second `Downlink` on one state;
  `validate_deployment`: an unknown `hosts` key; a `peer.namespace` naming
  no member; a `peer.namespace` naming the member itself is caught by
  `validate`, not here.
- `WiringBuilder::publish`/`subscribe` render the expected specs.

What could go wrong: `SystemSpec` literals in tests across the crate
(`src/tests.rs`, `src/cli.rs` tests, `tests/*.rs`) all need `peer: None`;
let the compiler list them. `examples/adcs-fsw2/tests/bundle.rs` reads
`meta.json`'s `ir_sha256`, which changes with the version, as every bump
does. `--check-ir` on a bundle packaged under v10 fails with the version
message, as designed.

Green when:

```sh
cargo test -p metor-fsw-2 --test ir_contract && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP5: Python `Publish` / `Subscribe`

Files:

- `python/metor-config/metor_config/_model.py`:
  - `SystemHandle.__init__(self, name, target=None, spec=None)`; the two
    attributes are read by `Subscribe` only. `__getattr__` keeps its
    underscore guard; `target`/`spec` are real attributes, so it never sees
    them.
  - `StateHandle.__init__(self, name, spec=None)`.
- `_target.py`:
  - `add` returns `SystemHandle(full, self, spec)`; `slot` returns
    `SystemHandle(full, self, None)`; `state` calls `spec._bind(self)` then
    returns `StateHandle(name, spec)`.
  - `to_ir`: a system entry gains `"peer": spec._peer_json()` only when
    the spec is a `_Subscribe` (mirror serde's `skip_serializing_if`).
- `_builtins.py`:

  ```python
  def Publish(state: StateHandle, instances: list[SystemHandle]) -> Spec
  def Subscribe(of: H, via: SystemHandle | None = None, telemetered: bool = True) -> H
  ```

  `Publish` returns `_attached(static_system("Downlink", instances=[h.name for h in instances]), state)`,
  rejecting a handle whose `target` is not the caller's at `_bind` (the
  spec's `_bind` gets the target). `Subscribe` returns a `_Subscribe(Spec)`
  whose `ty`/`artifact` are `of.spec.ty`/`of.spec.artifact`,
  `artifact_decl` copied so the artifact registers, `params={}`, and holds
  `of`, `via`, `telemetered`; the annotation `-> H` is satisfied with a
  `typing.cast`. `_bind(target)` checks `of.target is not target`; the
  remaining checks need the deployment.
  `telemetered=False` records `"telemetered": false` inside `peer`
  (decision 1); WP6 clears the flag on the mirror's outputs when false.
- `_deployment.py`: `Deployment(targets, hosts: dict[str, str] | None = None)`;
  after membership checks, for every member's `_Subscribe` specs: the peer
  target is a member (`ValueError` naming the namespace); the peer instance
  is a `SystemHandle` with a `spec` (not an `ExprHandle`, not a slot);
  exactly one `Downlink`-typed system in the peer target lists the
  instance's bare name or has no `instances` (several → name them and ask
  for `via=`; none → name the peer's servers); the chosen downlink's
  `attach` state's spec has `addr` whose port is not `0`. Then fill
  `peer = {namespace, link, port, instance}` on the spec. `to_ir` emits
  `"hosts"` only when non-empty.
- `__init__.py`: export `Publish`, `Subscribe`.
- `python/tests/data/deployment.py`: `fsw.add("widget", Subscribe(widget))`
  plus `fsw.connect(sub.sensors, …)` into a second `Widget`'s `cmd`? `Cmd`
  and `Sensors` differ, so the positive fixture connects
  `sub.sensors` to a system whose input is `Sensors`; the demo pack has
  none, so add a `Sink` entry to `python/tests/data/demo.py` with
  `sensors: InPort[Sensors]`. A new `python/tests/data/deployment_bad.py`
  connects `sub.sensors` to `InPort[Cmd]`; `test_pyright.py` gains a case
  asserting `errorCount == 1` on that file alone.
- `python/tests/test_golden.py`: `build_deployment` gains a `peer` server,
  a `Publish`, and an `fsw` `Subscribe` of `plant`; the golden
  `tests/golden/deployment.json` is regenerated (it is the two-member
  fixture already) rather than adding a third file, and
  `tests/ir_contract.rs::golden_deployment_round_trips` keeps pinning it.
- `python/tests/test_recorder.py`: the six record-time errors (same
  target; outside the deployment; unpublished; ambiguous; port `0`;
  `@system`), the `peer` emission, `hosts` emission, and `telemetered=False`.
- `src/wiring/py.rs`: `EMBEDDED_PACKAGE` needs no new entry; both helpers
  live in `_builtins.py`.

What could go wrong: `Target.add`'s `H` overload returns the spec's class;
`Subscribe`'s `-> H` makes `fsw.add("sim", Subscribe(sim))` type as
`Plant` only if `sim` is typed `Plant`, which `plant.add("sim", Plant())`
guarantees. A `static_system` peer is `Spec`-typed and its ports are `Any`,
as today. The golden regeneration changes `deployment.json`'s member
`fsw`, so `ir_contract.rs`'s expectations about that member (it holds a
Python system and a preset) must survive the addition; add, do not
replace.

Green when:

```sh
(cd python && uv run python -m unittest discover tests) && cargo test -p metor-fsw-2 --test ir_contract --test py_eval
```

## WP6: the mirror at resolve

Files:

- `src/telemetry/subscribe.rs` (new), `mod subscribe;` in
  `src/telemetry/mod.rs`, re-exported from `src/lib.rs:116`:

  ```rust
  /// The mirror's status, published on change.
  #[metor_fsw(name = "peer_status")] pub struct PeerStatus { … }   // the design's fields
  /// The mirror's outputs: `peer_status`, the log, then one raw writer per mirrored port.
  pub struct SubscribeOut { status: Output<PeerStatus>, log: LogPort, ports: Vec<Writer<NoWake>> }
  /// A mirror of one peer instance: an async client writing into `ports`.
  pub struct SubscribeSystem { peer: PeerSpec, ports: Vec<PortDesc>, … }
  impl SubscribeSystem { pub(crate) fn new(peer: PeerSpec, ports: Vec<PortDesc>) -> Self }
  ```

  `SubscribeOut::decls()` lists `peer_status` and `log`; `BindPorts::bind`
  binds those two, then loops `try_next_output` into `ports`, the
  `UplinkOut` order. `AsyncSystem::instance_descriptor` returns
  `descriptor()` with `ports` appended to `outputs`. In this package `run`
  publishes one `PeerStatus { connected: 0, .. }` and awaits cancellation
  through `until_cancelled(pending())`; plan 2 replaces the body.
- `src/wiring/resolve.rs`:
  - a `resolve_mirror(spec, wiring, registry, &mut packs_described, &mut wasm, graph)`
    arm, matched first in the systems loop when `spec.peer.is_some()`:
    the peer type's descriptor from (a) `registry.factories[ty].descriptor.descriptor()`
    for `artifact == None`, (b) `WasmCache::open(..).entries` for a wasm
    artifact, (c) a new per-resolve `HashMap<String, Vec<PackEntryDesc>>`
    filled by `dl::describe_raw` + `dl::decode_pack_manifest` on
    `find_built_artifact(..).path` for a cdylib; select the entry by
    `spec.ty` (or the sole entry, `wasm_entry`'s rule). Then the mirror's
    ports: `desc.outputs` minus `system_status` (`host_status_port().name`),
    minus `"log"`, minus `!telemetered`, each with `telemetered` cleared
    when `peer.telemetered` is false; inputs dropped. Register with
    `init::async_node(spec.name.clone(), SubscribeSystem::new(..))`.
  - `LoadError::PeerType { system, ty, artifact }` when the entry is not
    found; the manifest-hash check already runs for the artifact.
- `src/wiring/registry.rs`: `EntryDescriptor::descriptor(&self) -> SystemDescriptor`
  (call the fn, or clone the value).
- `src/cli/ui.rs`: no change here (plan 2 adds the `mirror of` line).

Tests to add:

- `src/wiring/resolve` or `tests/dl_integration.rs` (which already builds
  the dl fixture through `tests/common/mod.rs`): a two-member deployment
  from `WiringBuilder`, member `b` with `.subscribe("counter", "DlCounter", Some("dl"), PeerSpec{..})`
  and `.connect("counter", "tick_out", "echo", "tick_in")` into a `DlEcho`;
  `resolve(b)` succeeds; the registry holds `b.counter.tick_out` and
  `b.counter.peer_status` and not `b.counter.tick_in`; `run_for(10)` ends
  with nothing stopped and `peer_status.connected == 0`.
- A mirror of a static type (`Alarms`) resolves with its `AlarmDefs`,
  `AlarmRaised`, `AlarmCleared` message outputs.
- `telemetered: false` in `PeerSpec` yields untelemetered mirror outputs
  (`AllOutputs` does not list them).
- An unknown `ty` for the artifact → `PeerType`.

What could go wrong: `push_node` appends `system_status` after the
instance descriptor's outputs, and `bind::staged_outputs` keeps the host
port out of the `Binder`'s output cursor, so the `try_next_output` loop
stops at the mirrored ports; verify with the `dl_integration` case before
trusting it. The async boundary needs a private ring per output
(`plan_async_io`); a mirror with many ports costs that many rings, sized
by the peer type's `max_size`. `describe_raw` dlopens the peer's pack in
the subscriber process, briefly; a pack whose load has side effects
(a `ctor`) is already loaded by any target using it, so nothing new.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets && cargo test -p adcs-fsw2
```

The example stays one member through this plan; `cargo test -p adcs-fsw2`
proves the IR bump and the builtin changes did not move it.
