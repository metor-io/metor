# Plan: deployments, cross-target comms (2 of 2: the client and the rest)

Implements the second half of
[design-deployment-comms.md](design-deployment-comms.md), on top of
[plan 1](plan-deployment-comms-1.md). Six work packages. Each ends with a
green tree. At the end two members exchange data through the launcher, and
the adcs example is a plant/fsw deployment. Paths are relative to
`libs/metor-fsw-2` unless they start with `libs/`, `examples/`, or
`python/`.

Test commands used throughout:

```sh
cargo test -p metor-fsw-2                       # Rust unit + integration
cargo clippy -p metor-fsw-2 --all-targets       # lints, house rule
cargo test -p metor-fsw-2 --lib telemetry       # WP1, WP2
cargo test -p metor-fsw-2 --lib cli             # WP3
cargo test -p metor-fsw-2 --test comms          # WP4
cargo test -p adcs-fsw2                         # WP6 (skip w/o python3)
```

## Sequencing

```text
WP1 ─┬─> WP2 ──┬─> WP4 ─> WP6
     └─> WP3 ──┴─> WP5
```

WP1 (the client) is the root. WP2 (mDNS browse) and WP3 (CLI) touch
disjoint code and run in parallel after it. WP4 (integration) needs the
client and the browse to exist, even though its fixture runs on loopback.
WP5 (docs) needs WP3's flag text. WP6 (the example split) needs WP4's
proof that two members exchange data under the launcher.

## Deviations from the design doc

1. The panel's `browse` runs forever on a thread and pushes into a
   registry (`libs/metor-panel/src/connections/discovery.rs:35`). The
   subscriber needs one bounded answer per round, so WP2 lifts
   `pick_addr` and `instance_name` and writes a bounded `browse_peer` over
   the same `mdns_sd` calls (`browse`, `recv_timeout`, `stop_browse`;
   both crates pin `mdns-sd = "0.13"`), not the loop.
2. The design's integration fixture used the `dl-fixture` pack's producer
   and consumer. That pack has no generated Python module; the launch
   fixtures are builtin-only for that reason (running plan, deviation 2).
   WP4's fixture mirrors a builtin instead: member `b` subscribes to `a`'s
   `Downlink`, whose `link_status` frame changes exactly when `b` connects.
   The data assertion reads `b`'s ground link with `identify` and a
   `PacketStream`, the shape `libs/db/src/remote/fsw.rs`'s tests use. The
   design doc's testing section is not edited; this file is the record.
3. The `SubscribeSystem::run` stub from plan 1 WP6 published `peer_status`
   once. WP1 replaces the body; the type, bundle, and descriptor stay.
4. `sequences.rs` keeps `interactive_load_then_abort_safes` on the fsw
   member: it loads, starts, and aborts inside 40 cycles and reads no plant
   data. The other three cases gate on plant-driven estimates and go
   (decision 8). The design named `sequences.rs` as a whole; this is the
   per-case call the review asked for.
5. The design keeps the host out of the member `Wiring` and gives it to
   the leaf as `--peer`. The override has to land somewhere `resolve` can
   read, so `PeerSpec` carries an optional `host` that only
   `apply_overrides` sets; it is never emitted and `path_stripped` leaves
   it alone. Same shape as `--serve` writing `addr` into a `TcpServer`
   state the file declared without one.

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| `identify`/`Peer` live in `metor-proto-stellar` after plan 1 WP1; the fsw crate already depends on it | `Cargo.toml:21` | the client dials with them, no new dependency |
| `fsw_loop`'s backoff is 500 ms doubling to 10 s | `libs/metor-panel/src/connections/target.rs:352–353` | the constants are copied, not shared |
| The announce replay is `LinkInfo`, then per table tap `VTableMsg` + one `SetComponentMetadata` per component, then `SetMsgMetadata` per msg id, then retained snapshots | `src/telemetry/link/mod.rs:288–330` | the client reads until the first `Table` or non-announce packet, then has the whole map |
| A `Table` packet's payload is the ring record; a `Msg` packet's is the postcard body after the id | `src/telemetry/mod.rs`, `append_record` | a frame record is copied verbatim; a message record is `id ++ payload` |
| `realize_set` is private; `compatible` takes two `PortDesc`s | `core/src/descriptor.rs:532,554` | WP1 adds a `pub fn` over an announced vtable |
| `PortDesc::announce(instance)` yields the prefixed vtable and metadata | `core/src/descriptor.rs:328` | the mirror port's expected form under the peer prefix |
| `apply_overrides` edits `TcpServer` params in place; `check_serve` rejects `--serve` without `--target` on several members | `src/cli.rs:714,579` | `--peer` follows both |
| `print_preflight` renders `detail` per system from `source_of` plus ` · process` | `src/cli/ui.rs:145–160` | ` · mirror of <ns>/<link>` appends the same way |
| `tests/launch.rs` runs its cases in one `#[test]` from `tests/fixtures` with `RUST_LOG=info` and `have_python` | `tests/launch.rs:14–45` | `tests/comms.rs` copies that harness |
| The launch fixtures bind `127.0.0.1:2250–2252` | `tests/fixtures/launch_*.py` | the comms fixture takes `2253–2255` |
| `build_sim_coordinator` selects `deployment.target(None)` and clears `process` | `examples/adcs-fsw2/src/lib.rs` | WP6 selects `fsw` |
| `bundle.rs`, `sequences.rs`, `python_system.rs` select `target(None)` and tap `cube_sat.*` names | `examples/adcs-fsw2/tests/*.rs` | WP6 renames to the fsw namespace |
| `common::link_statically` is used by `closed_loop`, `eclipse`, `momentum` only | `examples/adcs-fsw2/tests/common/mod.rs:40` | deleted with them |
| `adcs-fsw2`'s dev-deps `serde_json` (eclipse/momentum/sequences patches), `adcs-systems` rlib (static parity) | `examples/adcs-fsw2/Cargo.toml` | WP6 trims what no surviving test uses |
| `telemetry.md` opens with a paragraph pointing at a deleted `peer-contracts.md` | `docs/telemetry.md:3–5` | WP5 replaces it |
| `Plant`'s inputs are `torque_cmd`/`mtq_cmd`; outputs `sensors`, `gps`, `wheels`, `body`, `world`, `disturb` | `examples/adcs-fsw2/systems/adcs-systems/src/plant.rs:345` | the split's mirror carries all six telemetered outputs; fsw connects three |

---

## WP1: the client

Files:

- `core/src/descriptor.rs`:

  ```rust
  /// Whether an announced (prefixed) vtable carries every component of `expected` with equal type and shape.
  pub fn announced_covers(announced: &VTable, expected: &VTable) -> bool
  ```

  the subset half of `compatible` over two realized sets, so the mirror
  compares `PortDesc::announce(peer_prefix).0` against the peer's
  `VTableMsg.vtable`.
- `src/telemetry/subscribe.rs`: `SubscribeSystem::run` becomes the loop:
  1. `candidates(&self.peer, override)`: the override alone, else
     `[::1]:port`, `127.0.0.1:port`, then WP2's `browse_peer` results
     (empty until WP2; the call site exists now).
  2. Per candidate, `identify(addr)`: `Peer::Db` → `peer_identity` fault,
     next; `Peer::Fsw { info, .. }` with `info.protocol_version < 2 ||
     info.namespace.as_deref() != Some(&peer.namespace) || info.link != peer.link`
     → `peer_identity`, next.
  3. Read the replay: for each `VTableMsg`, remember `(packet_id, vtable)`;
     for each `SetComponentMetadata`, attach the name to the last vtable's
     group (the server sends them right behind their `VTableMsg`); for each
     `SetMsgMetadata`, remember `(id, schema)`. The replay ends at the
     first packet that is none of these. Then map: a group whose first
     metadata name starts with `"{ns}.{instance}.{port}."` (or equals the
     port's prefix) binds to that mirror port if `announced_covers`, else
     `peer_schema_mismatch`; a message port binds when an announced id
     matches and the schemas are equal, else the same fault; an unbound
     port gets `peer_channel_missing` once. `sessions += 1`,
     `connected = 1`, publish status.
  4. Stream: `Table` packets with a bound id → `writer.try_write(payload)`;
     `Msg` packets with a bound id → `id ++ payload`; anything else is
     counted in `dropped` only if its id was refused, otherwise ignored
     (the retained wiring manifest, the peer's other frames). A write
     failure counts in `dropped`. `records += 1` and `last_rx_cycle` from
     `AsyncContext::cycle()`, a new accessor over the coordinator's
     existing `progress: Arc<AtomicU64>`: `LaunchCtx`
     (`src/coordinator/async_tasks.rs:23`) gains `cycle: Arc<AtomicU64>`,
     cloned from `self.progress` where the ctx is built (line 108), and
     `AsyncSlot::launch` moves it into the `AsyncContext` beside `cancel`
     and `status`. One field, three lines.
  5. On error: `peer_disconnect` fault once, `connected = 0`, publish,
     sleep the backoff (500 ms doubling to 10 s, reset on a verified
     connect), next candidate. All waits go through
     `until_cancelled`.

  Status publishes on change only, like `link_status`. Faults go through
  `output.log().fault(..)` and `flush(now)`, which an async system does
  itself (`system.md`, "Logs").

Tests to add (`src/telemetry/subscribe.rs` test module, `#[stellarator::test]`
in the style of `src/telemetry/link/tests.rs`):

- A hand-written server on `127.0.0.1:0` (the `fake_fsw` shape now in
  stellar's tests) that sends a v2 `LinkInfo`, one `VTableMsg` for a
  `TickOut`-shaped frame under prefix `a.counter`, its metadata, one
  `SetMsgMetadata`, then a `Table` packet and a `Msg` packet. A
  `SubscribeSystem` for `peer { namespace: "a", link: "peer", instance: "counter" }`
  bound over test rings (`RingBuffer` + `Writer`, as `link/tests.rs` builds
  them) receives both records byte-for-byte; `peer_status` reads
  `connected = 1, records = 2`.
- The same server with a renamed field → `peer_schema_mismatch` and no
  record; with no announce for the port → `peer_channel_missing`.
- `LinkInfo` with the wrong namespace → `peer_identity` and the loop moves
  to the next candidate (two listeners, the second right).
- `candidates` ordering: override alone; no override → both loopbacks first.
- Announce-replay end detection: a replay followed by a retained snapshot
  `Msg` whose id is unbound is ignored, not treated as a channel.

What could go wrong: the announce-to-port mapping keys on the component
name prefix; a frame whose port name is a prefix of another (`gps` and
`gps_raw`) must match on `"{prefix}."` with the trailing dot. Packet ids are
per connection; the map is rebuilt on every connect. The raw `Writer`'s
`try_write` returns `WouldBlock` on a full private ring; the boundary drains
it once per cycle, so a peer faster than the subscriber's cycle drops here,
counted. Under a simulated clock the coordinator yields once per cycle, so
the client's socket task gets one poll per cycle (the known starvation);
the unit tests use short runs and the integration fixture uses `sim_dt`
with enough cycles.

Green when:

```sh
cargo test -p metor-fsw-2 --lib telemetry && cargo test -p metor-fsw-2-core && cargo clippy -p metor-fsw-2 --all-targets
```

## WP2: mDNS browse

Files:

- `src/telemetry/discovery.rs`:

  ```rust
  /// Addresses of every `_metor-fsw._tcp` instance advertising `ns=<namespace>` and `link=<link>`, within `timeout`.
  pub(crate) fn browse_peer(namespace: &str, link: &str, timeout: Duration) -> Vec<SocketAddr>
  ```

  `ServiceDaemon::new`, `browse(FSW_SERVICE_TYPE)`, `recv_timeout` until
  the deadline, `ServiceResolved` filtered on the two TXT properties
  (`info.get_property_val_str`), addresses through `pick_addr` lifted from
  the panel (first non-loopback IPv4, else any non-loopback), then
  `stop_browse` and `shutdown`. Blocking; the client calls it from its
  task through `stellarator::blocking` if one exists, else on a
  `std::thread` with a channel, the way the panel isolates the daemon.
  Two seconds is the default timeout; a candidate round that finds nothing
  costs that much once per backoff.
- `src/telemetry/subscribe.rs`: `candidates` appends `browse_peer`'s
  result after the loopback pair.

Tests to add: `pick_addr` is pure; move its cases beside it (the panel
keeps its own copy and tests). `browse_peer` against a real daemon is not
unit-testable on loopback (the server skips loopback binds); it is covered
by hand on a LAN and by the identity check catching a wrong answer.

What could go wrong: two `ServiceDaemon`s in one process (the server's
advertiser and the client's browser) each open a multicast socket;
`mdns_sd` allows it. A host with no multicast interface yields an empty
list and a warning, and the loop keeps its loopback candidates.

Green when:

```sh
cargo test -p metor-fsw-2 --lib telemetry && cargo clippy -p metor-fsw-2 --all-targets
```

## WP3: CLI

Files:

- `src/cli.rs`:
  - `RunArgs` gains

    ```text
    /// Dial this host for the named peer member, overriding loopback and
    /// mDNS: `--peer plant=10.0.0.5[:2242]`. Repeatable; requires
    /// `--target` when there are several members.
    #[arg(long, value_name = "NS=HOST[:PORT]")] peer: Vec<String>
    ```

  - `apply_overrides`: for each `ns=host[:port]`, every system whose
    `peer.namespace == ns` gets `peer.host = Some(host)` and, when given,
    `peer.port = port`, before `resolve`, exactly as `--serve` rewrites
    `addr`. `PeerSpec` gains
    `#[serde(default, skip_serializing_if = "Option::is_none")] pub host: Option<String>`
    for it (deviation 5); the emitter never writes it. An `NS` naming no
    subscription in the member is an error before any build.
  - `check_peer(members, args)` beside `check_serve`, same wording with
    `--peer`.
  - The `RunArgs` literals in the test module gain `peer: Vec::new()`.
- `src/cli/launch.rs`: `Overrides` does not carry `--peer`; the doc comment
  on `member_argv` says the renderer emits it per member.
- `src/cli/ui.rs`, `print_preflight`: a system with `peer` set gets
  `detail = format!("{} · mirror of {}/{}", source_of(..), peer.namespace, peer.link)`
  and the magenta dot of a loaded system.
- `src/telemetry/subscribe.rs`: `candidates` reads `peer.host` first.

Tests to add (`src/cli.rs` test module):

- `peer_override_edits_the_mirror`: a `WiringBuilder` member with a
  `subscribe(..)` and `peer: vec!["plant=10.0.0.5:2300"]` → `host` and
  `port` set; `"plant=10.0.0.5"` keeps the port; `"other=…"` → the error.
- `peer_needs_a_target_on_several_members`, the `check_serve` twin.
- `print_preflight` is not unit-tested today; the `mirror of` line is
  checked in WP4's stderr.

What could go wrong: clap's `Vec<String>` with `value_name` renders in
`--help` as repeatable; `command_tree_is_well_formed` catches a conflict.
`PeerSpec.host` is in the IR type but never emitted; `ir_contract`'s
`maximal()` sets it so the round trip covers it.

Green when:

```sh
cargo test -p metor-fsw-2 --lib cli && cargo test -p metor-fsw-2 --test ir_contract && cargo clippy -p metor-fsw-2 --all-targets
```

## WP4: integration

Files:

- `tests/fixtures/comms_target.py` (new): member `a`
  (`namespace="a"`, `sim_dt=0.01`) with `link = a.state("link", TcpServer(addr="127.0.0.1:2253"))`,
  `peer = a.state("peer", TcpServer(addr="127.0.0.1:2254", name="a-peer"))`,
  `dl = a.add("downlink", Downlink(link))`, `a.add("publish", Publish(peer, [dl]))`.
  Member `b` (`namespace="b"`) with its own `link` on `2255`,
  `b.add("downlink", Downlink(b_link))`, and `b.add("a_link", Subscribe(dl))`.
  `Downlink` is a static type, so the mirror carries `link_status`; its
  `connections` count moves from 0 to 1 the moment `b` connects, which is
  the one record the test needs.
- `tests/comms.rs` (new), the `tests/launch.rs` harness (one `#[test]`,
  `have_python`, `current_dir(fixtures)`, `RUST_LOG=info`,
  `env!("CARGO_BIN_EXE_metor-fsw")`), cases in order:
  1. `run comms_target.py --cycles 400`: exit 0; stderr has both
     preflights, `b`'s shows `a_link    Downlink` with `mirror of a/peer`;
     a `b │` line carries the client's `peer connected` info event.
  2. While case 1's process runs (spawn rather than `output()`), dial
     `127.0.0.1:2255` with `metor_proto_stellar::identify`, read packets
     until a `SetComponentMetadata` named `b.a_link.link_status.connections`
     and then one `Table` with that announce's packet id, within 10 s; kill
     the process after. This is the "data arrived" assertion.
  3. `run comms_target.py --target b --cycles 100`: exit 0; stderr carries
     no `peer connected`; the mirror ran disconnected.
  4. `package --target a` and `--target b`, then `run a.bundle b.bundle --cycles 400`
     with `b` listed first: exit 0 and `peer connected` under `b │`.
  5. `run comms_target.py --peer a=127.0.0.1:2254 --target b --cycles 100`
     with nothing listening: exit 0, a `b │` line with `peer_identity` or
     a connect failure, no panic.
- `tests/common/mod.rs`: nothing; the socket reader is local to
  `tests/comms.rs`.

What could go wrong: case 2 races the process start; retry the dial with
the panel's backoff for up to 10 s before reading. A `--cycles 400` run at
`sim_dt=0.01` takes well under a second of wall time, and the client's
socket task is polled once per cycle, so 400 cycles is enough for connect +
replay + one record; raise it before lowering expectations. Ports
`2253–2255` must stay clear of the launch fixtures (`2250–2252`) and the
example (`2240`). A restart case (kill `a`, restart it) needs the launcher
to survive one member's exit, which it does not (fail-fast); that scenario
is a unit test in WP1 (two listeners) and is not repeated here.

Green when:

```sh
cargo test -p metor-fsw-2 --test comms && cargo test -p metor-fsw-2
```

## WP5: docs

Files:

- `docs/telemetry.md`: replace lines 3–5 (the dangling
  `peer-contracts.md` pointer) with one sentence pointing at the new
  "Peers" section. Add "Peers" after "Local discovery": `Publish` is a
  `Downlink` on a server; `Subscribe` mirrors one instance; `peer_status`;
  the three candidates; the connect-time checks and their fault kinds;
  what a consumer sees when the peer is down. "Local discovery" gains the
  `ns`/`link` TXT keys and the name default (plan 1 WP3 may have added
  them; merge). "Connection start" notes `LinkInfo` now carries the
  namespace and link name.
- `docs/cli.md`: "Run a target" flag list gains `--peer NS=HOST[:PORT]`;
  "Deployments" gains one paragraph: members find each other on loopback
  or mDNS; `--peer` for a routed network; the renderer emits it from
  `hosts`.
- `docs/wiring.md`, "Deployments": the example gains a `Publish` on the
  plant and a `Subscribe` on the fsw with one `connect`, and two sentences
  on the mirror. "The wiring IR": `peer` on a system and `hosts` on the
  envelope in the field lists.
- `docs/README.md`, "Built-in services": one sentence on peers.
- `docs/design-deployment-comms.md`: no change.

Green when the links resolve and:

```sh
cargo test -p metor-fsw-2
```

## WP6: the adcs example split

Files:

- `examples/adcs-fsw2/target.py`: two targets, both `cycle_rate=120.0`
  with no `sim_dt` (decision 4), `namespace="plant"` and `"fsw"`.
  - `plant`: `link = TcpServer("[::]:2240", name="plant")`,
    `peer = TcpServer("[::]:2242", name="plant-peer")`, `plant = Plant(…, process=True)`
    with today's params, `Downlink(link)`, `Publish(peer, [plant])`; after
    the fsw block, `ctrl_in = plant.add("ctrl", Subscribe(ctrl))` and the
    two delayed edges into `plant.torque_cmd`/`plant.mtq_cmd`.
  - `fsw`: `link = TcpServer("[::]:2241", name="fsw")`,
    `peer = TcpServer("[::]:2243", name="fsw-peer")`,
    `sim = fsw.add("plant", Subscribe(plant))` first, then `nav`, `ctrl`,
    `gyro_norm`, `alarms`, `presets`, `uplink`, `mode`, the edges from
    `sim.sensors`/`sim.gps`/`sim.wheels` in place of `plant.*`, the
    unchanged nav/ctrl/mode edges, `Downlink(link)`,
    `Publish(peer, [ctrl])`, the routes.
  - The `mode` slot drops `initial="commissioning"` and
    `initial_state="running"` (decision 9): it starts empty, and
    `commissioning` is loaded and started from the panel's sequence list.
    The commissioning params on the `allow` line stay.
  - The dashboards and alarms keep their `plant.…` component paths: the
    mirror is named `plant` in `fsw`.
  - `Deployment(targets=[plant, fsw])`.
- `examples/adcs-fsw2/src/lib.rs`: `build_sim_coordinator` selects
  `deployment.target(Some("fsw"))`; the `process = false` loop stays (a
  no-op for fsw, harmless); the doc comment drops the convergence-test
  sentence.
- `examples/adcs-fsw2/tests/`, per file:

  | File | Call | Reason |
  | --- | --- | --- |
  | `alarms.rs` | delete | the raise/clear assertion needs the plant's boot tumble and the controller's detumble in one coordinator |
  | `bundle.rs` | stay | packages one member; `target(None)` becomes `target(Some("fsw"))`, `cube_sat.` names become `fsw.` |
  | `closed_loop.rs` | delete | static-vs-dlopen convergence parity over the whole loop |
  | `eclipse.rs` | delete | nav under sun loss driven by the patched plant orbit |
  | `momentum.rs` | delete | desat over preloaded wheels needs plant and ctrl in one loop |
  | `python_system.rs` | stay, trimmed | keeps resolve (the `gyro_norm` binding resolves to the mirror through `locate_producer`) and a 60-cycle run with nothing stopped; the nox-oracle comparison over live samples goes with the loop tests |
  | `sequences.rs` | stay, one case | `interactive_load_then_abort_safes` reads no plant data; `commissioning_auto_runs_to_completion`, `commissioning_emits_ordered_sequence_messages`, `detumble_times_out_to_failed_with_wheels_idle` gate on plant-driven estimates and go; `drain_msgs` goes with them |
  | `common/mod.rs` | stay | `link_statically` deleted (no user left); `ensure_stubs` and `link_port_guard` stay |

- `examples/adcs-fsw2/Cargo.toml`: drop the dev-deps only the deleted
  tests used (`adcs-systems` rlib for static parity, `serde_json` for
  param patching, `postcard` for `drain_msgs`); keep `adcs-contracts`
  (`ModeCmd` in `sequences.rs`), `stellarator`, `tempfile`, `zerocopy` if
  `python_system.rs` keeps its host mirror frame. Let the compiler and
  `cargo udeps`-by-hand decide; the comments above the dev-deps describe
  the deleted tests and go with them.
- `examples/adcs-fsw2/README.md`: the run line becomes
  `run target.py --build` (every member) and notes `--target fsw`; the
  architecture paragraph gains two lines on the split; remove any sentence
  naming a deleted test. Lines 48 and 156 say the slot auto-runs
  `commissioning`; they change to say it starts empty and is started from
  the panel (decision 9).
- `examples/adcs-fsw2/pyrightconfig.json`: no change; `target.py` still
  type-checks, now with `Subscribe`.

Tests: the surviving three files, run by `cargo test -p adcs-fsw2`, plus
by hand:

```sh
cd examples/adcs-fsw2 && uv run -- cargo run -p metor-fsw-2 --bin metor-fsw -- run target.py --build
```

with the panel connected to `2241` showing `fsw.plant.sensors.gyro_b`
moving, and to `2240` showing `plant.plant.sensors.gyro_b`; the ADCS
dashboard preset opens unchanged.

What could go wrong: `Plant` steps a fixed timestep internally (the
deleted `plant.py` said 1/120 s); under the wall clock at 120 Hz that is
real time, so nothing changes in the physics, but verify the constant in
`adcs-systems/src/plant.rs` before trusting it. The slot starts empty, so
nothing races the plant's startup; `commissioning`'s warm-up
(`warmup_timeout_s=10.0`) begins when the user starts it. `mode`'s `gps`
input now comes from the mirror; a `None` before the first record is what
the occupant's warm-up already tolerates. `bundle.rs` writes `meta.json`; its `ir_sha256` changes, as
every IR edit does.

Green when:

```sh
cargo test -p adcs-fsw2 && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```
