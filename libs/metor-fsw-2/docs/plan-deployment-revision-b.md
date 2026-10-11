# Plan: deployments, revision B (link filtering and frame subsets)

Implements plan B of
[design-deployment-revision.md](design-deployment-revision.md): changes 1 and
2 and decisions 1, 3, 4, on top of the landed comms plans
([1](plan-deployment-comms-1.md), [2](plan-deployment-comms-2.md)). Six work
packages. Each ends with a green tree. At the end a subscriber sends one
`LinkSubscribe` after the identity check and the server copies only its ids
into that connection's queue; `PeerSpec.frames` narrows a mirror to what its
edges read plus what `frames=` names; and the adcs example's fsw mirror of
the plant carries four of the plant's six frames. Paths are relative to
`libs/metor-fsw-2` unless they start with `libs/`, `examples/`, or
`python/`.

Test commands used throughout:

```sh
cargo test -p metor-proto-wkt                                   # WP1
cargo check -p metor-db -p metor-panel                          # WP1 (wkt consumers)
cargo test -p metor-fsw-2 --lib telemetry                       # WP2, WP4
cargo test -p metor-fsw-2                                       # WP3, WP6
cargo clippy -p metor-fsw-2 --all-targets                       # lints, house rule
cargo test -p metor-fsw-2 --test ir_contract --test py_eval     # WP3, WP5
(cd python && uv run python -m unittest discover tests)         # WP5; pyright optional
cargo test -p metor-fsw-2 --test comms --test gateway --test launch  # WP6
cargo test -p adcs-fsw2                                         # WP6 (skip w/o python3)
```

## Sequencing

```text
WP1 ─┬─> WP2 ──┐
     └─> WP4 ──┼─> WP6
WP3 ─────> WP5 ┘
```

WP1 (the wire message) and WP3 (the IR field and the resolve filter) touch
disjoint crates and start together. WP2 (the server) and WP4 (the client)
both need WP1's type and run in parallel after it. WP5 (Python) emits the
field WP3 reads, so its golden regeneration waits for WP3. WP6 (the
example, the integration case, docs) needs everything.

## Deviations from the revision doc

Found while grounding the plan. The revision doc is not edited here; this
file is the record.

1. **The connect-time seed is not filtered.** `ServerState::activate` builds
   `seed_replay(announce_blob, retained)` and hands it to the connection
   task as its first write (`src/telemetry/link/mod.rs:577–605`). The client
   cannot send its request before it has read `LinkInfo` (it does not know
   what answered), so the seed, announce plus retained snapshots, is always
   in flight before the request arrives. Retained *updates* ride inside the
   cycle batch (`src/telemetry/mod.rs:295–306`) and filter with it. The
   revision's "retained snapshots filter the same way" holds for every
   update after connect and not for the one-time seed, which is the same
   cost as the announce replay the revision already keeps full. Accepted.
2. **A feature bit, not a protocol version.** `LinkInfo.features` is
   "reserved (0 for now)" (`libs/metor-proto/wkt/src/msgs.rs:1136`). WP1
   defines `LINK_FEATURE_SUBSCRIBE = 1` and the server sets it; the client
   sends `LinkSubscribe` only when the bit is set, so a subscriber against
   a server built before this plan sends nothing and keeps today's full
   batch, which it already tolerates. `LINK_PROTOCOL_VERSION` stays 2 and
   `MIN_PROTOCOL_VERSION` (`src/telemetry/subscribe.rs:47`) stays 2.
3. **Message ids are subscribed as ids, and they are shared.** A `Msg`
   packet's id is the record's own leading id (`Wire::Msg`,
   `src/telemetry/mod.rs:225–229`), and the downlink announces one id once
   however many producers emit it (`collect_taps`, `taps.rs:138–153`). A
   subscriber naming a message id therefore receives every producer's
   records under it, which is exactly what the mirror's `routes.msgs` binds
   today (`subscribe.rs:487`). Decided: the allow-set is one
   `HashSet<PacketId>` keyed on the id alone, both packet types; the request
   carries `tables` and `msgs` as two lists for the reader's sake and the
   server unions them. Table ids and message ids are both 16-bit folds moved
   off row 224 (`off_reserved`, `libs/metor-proto/src/types.rs:607`), so a
   table id can equal a message id; a collision passes one extra packet the
   client's typed route map ignores. No harm, stated here.
4. **A `frames` entry may be a message token.** A `route(mirror, dst,
   msg="AlarmRaised")` edge records the message token as `out`
   (`python/metor-config/metor_config/_target.py:352–362`), and resolve
   binds it to a producer port by name first and by the consumer's input id
   second (`src/wiring/resolve/endpoints.rs:162–189`). The inferred set
   therefore holds tokens as well as port names, and WP3's filter matches a
   port by its name, by the token's id in `registry.msgs`, or by its
   Postcard schema name. An entry matching nothing is a resolve error
   naming the type's outputs.
5. **`@system` bindings are not inferred.** The revision's decision 4 says
   edges only; grounding confirms the recorder *cannot* do more: the
   decorator's arguments "are accepted and ignored here" and the host reads
   the bindings from source (`_program.py:137–149`). A mirror frame read
   only by a `@system` binding needs `frames=` or an edge; otherwise
   resolve fails loudly in `locate_producer`. The example's `gyro_norm`
   reads `plant.sensors.gyro_b`, covered because `sensors` is also an edge.
   The docs say so (WP6).
6. **The output-membership check runs at resolve, not validate.** Only
   `resolve_mirror` has the peer type's descriptor
   (`src/wiring/resolve.rs:455–528`); `validate` sees the `Wiring` alone.
   `validate` checks the list's shape (no empty names, no duplicates);
   resolve checks membership. The Python recorder is the earlier gate for
   generated classes (WP5). The revision says "validate checks each name is
   a telemetered output"; read "resolve" for that clause.
7. **The emission fold.** The place to fold `frames` is `Target._system_ir`
   (`_target.py:421–431`), which already folds a mirror's `peer` and an
   ingest's params; that is gateway plan 2's deviation 7, not a "deviation
   12" (neither comms plan has one). `Deployment._resolve_peer` is
   untouched: it fills what needs both members, and `frames` needs only the
   subscriber's own edges.

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| The cycle batch is one `Vec<u8>` of length-prefixed packets: `u32` LE length (= 4 + payload), then `ty`, `id[2]`, `req_id`, payload | `append_packet`, `src/telemetry/mod.rs:210–216`; `PACKET_HEADER_LEN = 4`, `libs/metor-proto/src/types.rs:564`; `PacketTy`, `:553–558` | a packet walker is ten lines and needs no decoder |
| `broadcast_buffer` swaps the batch into `pending_batch`; `flush` moves it into a `ServerCommand::Cycle`; `ServerState::command` calls `broadcast(&command.batch)`, which runs `enqueue_bytes` once per connection into that connection's own bbqueue | `src/telemetry/link/mod.rs:393–419, 557–564, 607–614` | the per-connection copy already exists; the filter is a different source slice per connection in `broadcast` |
| Each connection's bbqueue is `PENDING_CAP + 1` bytes, allocated in `activate`; `enqueue_bytes` drops a whole batch that would cross the cap | `link/mod.rs:50, 585, 625–643` | a filtered copy is smaller, so a subscriber drops less |
| `Rc<ConnStatus { queued, closed }>` is shared between the server loop and the connection task; `read_half` receives `inbound`, `metrics`, `rx` and not the status | `link/mod.rs:197–201, 588–604, 658–675, 715` | the allow-set lives on `ConnStatus`; `read_half` gains the `Rc` |
| `read_half` queues every `Msg` whose id is not `MsgStream::ID` and not in `NODE_PROTOCOL_MESSAGES` for the uplink | `link/mod.rs:719–724` | the request id must be intercepted before that arm and listed in `NODE_PROTOCOL_MESSAGES` (the db's catch-all records unlisted ids as telemetry) |
| `seed_replay` runs at `activate`, before any inbound byte | `link/mod.rs:577–582, 648–654` | deviation 1 |
| A retained tap frames into `retain_scratch`, appends it to the batch, then `retain`s it | `src/telemetry/mod.rs:295–306` | retained updates filter with the batch |
| `LinkInfo { protocol_version, features: 0, .. }` is built once in `set_announces` | `link/mod.rs:315–321` | one site sets the feature bit |
| `[224, 61]` is `LinkInfo`, `[224, 63]` is `SyncMsgs`; `[224, 62]` is bound to nothing in the tree | `msgs.rs:1152, 968`; `grep -rn '224, 62'` empty | the request takes `[224, 62]` |
| `impl<T: Serialize + Schema> Msg for T` is a blanket impl; `LinkInfo` derives no `Schema` and pins its id by hand | `types.rs:602`; `msgs.rs:1133` | the request derives `Serialize, Deserialize, Debug, Clone, PartialEq, Eq` and no `Schema` |
| `link_info_id_is_pinned_and_protocol_member` pins `[224,61]`/`[224,63]` and their `NODE_PROTOCOL_MESSAGES` membership; the sequence/alarm/preset/manifest/log id tests assert those ids are *not* protocol members | `msgs.rs:1636–1641, 1461–1475, 1515, 1529, 1570, 1622` | the new id joins the pinned pair; the others are untouched |
| `table_ids_are_hashes_of_the_announce` asserts a table id is never `[0, 0]` and equals `table_id(&vtable)` | `src/telemetry/mod.rs:419–462` | the client's precomputed ids equal the announced ones by construction |
| `table_id` is the fold of the vtable's postcard bytes off row 224; `PortDesc::announce(instance)` yields that vtable under a prefix; the client's `bind` already computes `port.announce(&format!("{ns}.{instance}"))` | `types.rs:621`; `core/src/descriptor.rs:328`; `subscribe.rs:443–448` | the request's table ids are `table_id(&expected)` from the same call |
| `session` binds the sink as `_tx` and never uses it; `PacketSink::send(&self, impl IntoLenPacket)` | `subscribe.rs:329`; `libs/metor-proto/stellar/src/lib.rs:58` | the request is one `send` after `check_identity` (`subscribe.rs:336`) |
| `fake_peer` in the client tests drops its read half (`_rx`) | `subscribe.rs:846–853` | WP4 gives it an inbound recorder, the `fake_fsw` shape in `libs/db/src/remote/fsw.rs:126` |
| `resolve_mirror` keeps `telemetered && name != system_status && name != "log"` outputs and clears `telemetered` per `peer.telemetered` | `src/wiring/resolve.rs:510–521` | the `frames` filter is one more `filter` in that chain, with `registry` in scope |
| `Registry.msgs` is a `MsgTable` with `get(&str) -> Option<(&'static str, PacketId)>`; `PortDesc::id()` is `PortId::Packet(id)` for a Postcard port | `core/src/message.rs:78`; `core/src/descriptor.rs:33–38`; `resolve.rs:331` | deviation 4's three-way match |
| `check_system`'s `peer` arm is one `LoadError::PeerSpec { system, reason }` with six reasons | `src/wiring/validate.rs:508–542`; `error.rs:285` | the shape checks add reasons, not variants |
| `PeerSpec` literals: `maximal()`, `peer_spec()`, `peer(..)`, `peer_at(..)`, the `--peer` tests, the preflight test; `subscribe(.., peer: PeerSpec)` takes the value | `tests/ir_contract.rs:160`, `validate.rs:701`, `subscribe.rs:635, 774`, `src/cli.rs:1205, 1295`, `src/cli/ui.rs:272`, `builder.rs:377` | every literal gains `frames: Vec::new()`; the compiler lists any missed |
| `ir_contract.rs` pins the mirror's `peer` JSON exactly and a v10-shaped `PeerSpec` decodes with defaults | `tests/ir_contract.rs:328–335, 361–380` | the exact pin gains `frames` once the golden carries one |
| `Target._edges` entries are `{from, out, to, in_, delayed, kind, src}`; `connect` writes `src.instance`/`src.port`, `route` writes `_handle_name(src)`/`msg` | `_target.py:343–375` | inference is one pass over `_edges` keyed on `from` |
| `_system_ir` folds `peer` from `mirror._peer_json()`, which returns the stored dict | `_target.py:421–431`; `_builtins.py:187–193` | fold `frames` into a copy; `test_two_publishers_need_via` re-resolves the same spec |
| `SystemHandle.spec` is the `Spec` the target registered; a generated class carries class-level `port: OutPort[Frame]` / `InPort[Frame]` annotations plus `system_status`, under `from __future__ import annotations` | `_model.py:175–178`; `src/wiring/pack_module.rs:54, 185–196` | `type(spec).__annotations__` values are strings; `startswith("OutPort[")` is the output test; a `static_system` or bare `System(..)` spec has none and is unchecked |
| `demo.py`'s `Widget` ports: `cmd: InPort`, `sensors: OutPort`, `events: OutPort[Msg]`, `system_status` | `python/tests/data/demo.py:75–78` | the pyright fixture can name `events` |
| The golden `fsw` member has no edge off its mirror; `build_deployment` uses a bare `System("Plant", ..)` | `tests/golden/deployment.json`; `python/tests/test_golden.py:255–296` | a `frames=["gps"]` pins the field without a consumer, and the Python port check does not run on a bare `System` |
| `test_recorder.py` has a `members()` helper building `plant`/`fsw` with a `Publish` | `python/tests/test_recorder.py:523–531` | the new cases reuse it |
| `metor-config` is `0.4.3`; the wheel pin is `>={major}.{minor},<{major}.{minor+1}` | `_version.py`, `pyproject.toml`; `src/wiring/pack_dist.rs:485` | `0.4.4`; the IR version stays 11 |
| `LinkStatus` is not exported from the crate root | `src/lib.rs:122–125` | the integration case learns the ids from the replay, as `mirrored_frame_arrives` does (`tests/comms.rs:111–147`) |
| The comms fixture binds `2253–2255`, the gateway fixture `2256–2258`, the launch fixtures `2250–2252` | `tests/fixtures/*.py` | the new case reuses `2254` |
| `link/tests.rs:55` drives a bare `LinkState` with raw `TcpStream`s, `broadcast_buffer` + `flush` | `src/telemetry/link/tests.rs:55–121` | the server test copies that harness |
| The db's `fsw_stream`, the panel's fsw loop, and the gateway's `source_loop` send nothing after `identify` but forwarded commands | `libs/db/src/remote/fsw.rs:36`; `libs/metor-panel/src/connections/target.rs:374`; `src/gateway/ingest.rs:357` | unchanged; they keep the full batch |
| Edges off the fsw mirror `sim`: `sensors` (404, 407), `gps` (405, 406, 411), `wheels` (408); the fsw dashboards and alarms reference `plant.sensors`, `plant.wheels`, `plant.gps`, and `plant.world` (the eclipse chip, line 186); `body` and `disturb` are referenced nowhere; `Plant` has six outputs | `examples/adcs-fsw2/target.py`; `examples/adcs-fsw2/systems/adcs-systems/src/plant.rs:348–353` | inferred `{gps, sensors, wheels}`; `world` is the one dashboard-only frame |
| Edges off the plant mirror `ctrl_in`: `torque_cmd`, `mtq_cmd` | `target.py:426–427` | the plant's mirror narrows too, with no annotation |

---

## WP1: the `LinkSubscribe` message

Files:

- `libs/metor-proto/wkt/src/msgs.rs`, after `impl Msg for LinkInfo`:

  ```rust
  /// A client's one request after the identity: serve this connection only these packet ids.
  pub struct LinkSubscribe { pub tables: Vec<PacketId>, pub msgs: Vec<PacketId> }
  impl Msg for LinkSubscribe { const ID: PacketId = [224, 62]; }
  /// `LinkInfo::features` bit: the server honours `LinkSubscribe`.
  pub const LINK_FEATURE_SUBSCRIBE: u64 = 1;
  ```

  Derives `Serialize, Deserialize, Debug, Clone, PartialEq, Eq`, no
  `Schema` (the blanket `Msg` impl). Doc on the struct: an empty pair means
  "nothing but the replay"; a connection that never sends one gets the
  whole batch; a later request replaces the set. `LinkInfo::features`'s doc
  loses "reserved (0 for now)" and names the bit.
- `NODE_PROTOCOL_MESSAGES`: `LinkSubscribe::ID` after `LinkInfo::ID`.

Tests to add (`link_info_tests`):

- `link_info_id_is_pinned_and_protocol_member` gains
  `assert_eq!(LinkSubscribe::ID, [224, 62])` and its membership.
- `link_subscribe_round_trips`: two ids in each list through
  `into_len_packet` and back.

What could go wrong: nothing else in the tree names `[224, 62]` (grep), but
`cargo check -p metor-db -p metor-panel` proves the `NODE_PROTOCOL_MESSAGES`
growth breaks no exhaustive match. A `Schema` derive by habit collides with
the blanket impl at compile time, which is the wanted failure.

Green when:

```sh
cargo test -p metor-proto-wkt && cargo check -p metor-db -p metor-panel
```

## WP2: the server's per-connection allow-set

Files:

- `src/telemetry/link/mod.rs`:
  - `ConnStatus` gains `filter: RefCell<Option<HashSet<PacketId>>>`; the
    doc says it is the connection's allow-set, `None` until a
    `LinkSubscribe` arrives. `Rc` already scopes the struct to one thread.
  - `read_half(inbound, metrics, status: Rc<ConnStatus>, rx)`: a new first
    arm, `m.id == LinkSubscribe::ID` → `parse::<LinkSubscribe>()` and
    replace `status.filter` with the union of both lists; a parse failure
    is ignored like any other stray protocol packet. `conn_task` passes
    `status.clone()` to both halves.
  - `ServerState` gains `scratch: Vec<u8>`; `broadcast` becomes: for each
    connection, with a filter, `select_packets(batch, filter, &mut scratch)`
    and `enqueue_bytes(.., &scratch)` when it is non-empty; without one,
    `enqueue_bytes(.., batch)` as today. The `conn_dropped` accounting is
    unchanged.
  - A free function, unit-tested:

    ```rust
    /// Copy every whole packet of `batch` whose id is in `allow` onto `out`, cleared first.
    fn select_packets(batch: &[u8], allow: &HashSet<PacketId>, out: &mut Vec<u8>)
    ```

    walks the `u32` length prefix, reads the id at offset 5, and copies
    `4 + len` bytes; a truncated tail ends the walk (the downlink frames
    whole packets, so it never happens).
  - `set_announces`: `features: LINK_FEATURE_SUBSCRIBE`.
  - Module doc: "serves every connection the same full stream" becomes
    "the same replay, then each cycle's batch, filtered to the ids a
    connection asked for"; one paragraph under "Fan-out and the slow
    consumer" on the allow-set and the unfiltered seed (deviation 1).
- `src/telemetry/mod.rs`, `TelemetrySystem` docs: no change; the batch it
  frames is the same.

Tests to add (`src/telemetry/link/tests.rs`):

- `select_packets_keeps_whole_packets_by_id`: a batch of three packets
  built with `LenPacket::table`/`msg` (ids A, B, A); allow `{A}` yields the
  first and third byte-for-byte; allow `{}` yields nothing; a batch whose
  last packet is cut short yields the whole packets before it.
- `a_subscribed_connection_gets_its_ids_only`, the `:55` harness: two
  clients; both read the seed; the first writes a `LinkSubscribe { tables:
  [A], msgs: [] }`; `broadcast_buffer` a batch of A then B, `flush`; the
  first reads exactly packet A within a bounded wait and nothing more in
  the next few ms; the second reads A then B. A third client connecting
  after a `retain` reads the seed with the retained record, unfiltered.
- `link_replays_fans_out_and_ingests…`: the identity it expects gains
  `features: LINK_FEATURE_SUBSCRIBE`.

What could go wrong: the request can arrive after a batch was already
copied, so one or two unfiltered batches reach a subscriber at connect; the
client ignores unbound ids (`route`, `subscribe.rs:521–564`), so nothing
misroutes. `RefCell` borrow in `broadcast` is short and the read half only
`replace`s, both on one thread; a `borrow_mut` across an await would be the
bug, and there is none. `select_packets` must clear `out` before copying,
else a second connection sees the first's packets.

Green when:

```sh
cargo test -p metor-fsw-2 --lib telemetry && cargo clippy -p metor-fsw-2 --all-targets
```

## WP3: `PeerSpec.frames`, the resolve filter, validate

Files:

- `src/ir.rs`, `PeerSpec`:

  ```rust
  /// The peer type's outputs this mirror carries, by port name or message
  /// token; empty means every telemetered output. Filled by the front end
  /// from the subscriber's edges and its `frames=` list.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub frames: Vec<String>,
  ```

  after `telemetered`, before `host`. `IR_VERSION` stays 11 (additive,
  defaulted, the `hosts` precedent).
- Every `PeerSpec` literal gains `frames: Vec::new()`: `tests/ir_contract.rs`
  (`maximal()`), `src/wiring/validate.rs` (`peer_spec()`),
  `src/telemetry/subscribe.rs` (`peer`, `peer_at`), `src/cli.rs` (`:1205`,
  `:1295`), `src/cli/ui.rs` (`:272`), and whatever else the compiler names.
- `src/wiring/resolve.rs`, `resolve_mirror`: after the existing
  `telemetered && name != status && name != "log"` filter and before the
  `telemetered` rewrite, when `!peer.frames.is_empty()`:

  ```rust
  /// Whether a `frames` entry names `port`: its name, a message token `registry.msgs` maps to its id, or its Postcard schema name.
  fn frame_names(entry: &str, port: &PortDesc, msgs: &MsgTable) -> bool
  ```

  keep the ports some entry names; every entry must name at least one
  port, else `LoadError::PeerSpec { system, reason: "frames names `x`,
  which is not a telemetered output of `Plant`; its outputs are a, b, c" }`.
  The doc comment on `resolve_mirror` gains one sentence.
- `src/wiring/validate.rs`, `check_system`'s `peer` arm: two more reasons,
  an empty or whitespace `frames` entry, and a duplicate entry.
- `src/wiring/builder.rs`: no signature change; `subscribe` takes the
  `PeerSpec` by value.
- `src/cli/ui.rs`, `system_detail`: no change.

Tests to add:

- `tests/ir_contract.rs`: `maximal()`'s mirror carries
  `frames: vec!["gps".into()]`; the exact `peer` pin at `:331–334` gains
  `"frames": ["gps"]`; `a_v10_document_defaults_the_new_fields` also
  decodes a `PeerSpec` JSON without `frames` to an empty list.
- `src/telemetry/subscribe.rs` `tests`: `frames_narrow_a_static_mirror`:
  `frames: ["AlarmRaised"]` on the `Alarms` mirror registers
  `fsw.alarms.AlarmRaised` and `peer_status`, not `AlarmDefs` or
  `AlarmCleared`; `an_unknown_frame_is_rejected`: `frames: ["Nope"]` →
  `PeerSpec` whose reason names `Nope` and lists the three outputs.
- `src/wiring/validate.rs`: an empty entry and a duplicate entry → the two
  reasons; `publish_and_subscribe_render_their_specs` (`:811`) sees
  `frames` round-trip through the builder.

What could go wrong: the `frames` order is the front end's; resolve keeps
descriptor order regardless, so the mirror's port order (and `SubscribeOut`'s
bind order) does not depend on the list. A static type's message ports are
named by token already (`AlarmRaised`), so the msgs-table and schema-name
arms matter only for pack entries; the test above exercises the name arm and
`ir_contract` the field. `examples/adcs-fsw2/tests/bundle.rs` writes
`meta.json`'s `ir_sha256`, which changes with any IR edit; it is not pinned.

Green when:

```sh
cargo test -p metor-fsw-2 --test ir_contract && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP4: the client's request

Files:

- `src/telemetry/subscribe.rs`:
  - `session`: `_tx` becomes `tx`; right after `check_identity` succeeds
    and before the replay loop, when
    `info.features & LINK_FEATURE_SUBSCRIBE != 0`,
    `tx.send(&request(peer, ports)).await`; a send error is logged at
    debug and returns `Session::Rejected` (the socket is gone; the next
    candidate or round handles it).
  - A pure function beside `bind`:

    ```rust
    /// The ids this mirror wants: each table port's `table_id` under the peer prefix, each message port's own id.
    fn request(peer: &PeerSpec, ports: &[PortDesc]) -> LinkSubscribe
    ```

    `bind` already computes `port.announce(&prefix)` per table port; lift
    the prefix and the `expected` vtable into one helper both call so the
    two ids cannot drift.
  - Module doc: one sentence after "binds the announce replay": the client
    asks the server for its ids first, so a wide publisher costs it only
    its subset.

Tests to add (`client_tests`):

- `fake_peer` gains an inbound recorder: the accept task reads packets off
  `_rx` into an `Rc<RefCell<Vec<OwnedPacket>>>` the test can inspect, the
  `fake_fsw` shape.
- `a_connected_peer_asks_for_its_ids`: `identity(.., features: LINK_FEATURE_SUBSCRIBE)`;
  after `records == 2` the first inbound packet parses as
  `LinkSubscribe { tables: [tick_id(&port)], msgs: [Beat::ID] }`.
- `a_peer_without_the_feature_gets_no_request`: `features: 0`; the inbound
  list is empty after the same drive.
- `request_ids_match_bind`: pure; `request(..).tables[0] == tick_id(&port)`.

What could go wrong: `identity(..)` in the existing tests passes
`features: 0`, so they keep exercising the no-request path unchanged; only
the new cases set the bit. `PacketSink::send` takes `&self`, so `tx` need
not be `mut`. The request goes out before the replay is read, so a peer that
closes during its replay (`:349–352`) may have a half-written request in
flight; `Rejected` either way.

Green when:

```sh
cargo test -p metor-fsw-2 --lib telemetry && cargo clippy -p metor-fsw-2 --all-targets
```

## WP5: Python `frames=` and edge inference

Files:

- `python/metor-config/metor_config/_builtins.py`:

  ```python
  def Subscribe(of: H, via: SystemHandle | Spec | None = None, telemetered: bool = True, frames: list[str] | None = None) -> H
  ```

  `_Subscribe.__init__` takes `frames` and stores `self.frames: list[str]`
  (`[]` for `None`). At construction, when `type(of.spec)` carries
  annotations (walk the MRO's `__dict__["__annotations__"]`), every name
  must be one whose annotation string starts with `"OutPort["` and is not
  `system_status`: an `InPort` name raises `ValueError` ("`cmd` is an input
  of `Widget`; a mirror carries outputs"), `system_status` raises ("the
  mirror writes its own"), and an unknown name raises listing the outputs.
  A spec with no annotations (a `static_system`, a bare `System(..)`) is
  not checked; resolve is its gate. The docstring: what `frames` adds, that
  edges are inferred, that a `@system` binding is not an edge (deviation 5).
- `_target.py`, `_system_ir`: for a mirror, `frames = sorted({e["out"]
  for e in self._edges if e["from"] == name} | set(mirror.frames))`; emit
  `{**entry, "peer": {**mirror._peer_json(), "frames": frames}}` when
  non-empty, else the `peer` dict as today. Sorted, so reordering edges
  never moves the IR.
- `_version.py`, `pyproject.toml`: `0.4.4`.
- `python/tests/data/deployment.py`: `Subscribe(widget, via=plant_publish, frames=["events"])`,
  so the pyright gate types the parameter and the recorder's check sees a
  generated class.
- `python/tests/test_golden.py`, `build_deployment`:
  `Subscribe(sim, via=publish, frames=["gps"])`; regenerate
  `tests/golden/deployment.json` (the mirror's `peer` gains
  `"frames": ["gps"]`). `deployment_gateway.json` has no mirror and does
  not move.
- `python/tests/test_recorder.py`, in the mirror test class:
  - `test_frames_are_inferred_from_edges`: `members()`, a `connect(mirror.gps, nav.gps)`
    and a `route(mirror, nav, msg="AlarmRaised")` → `peer["frames"] == ["AlarmRaised", "gps"]`.
  - `test_explicit_frames_union_the_edges`: `frames=["wheels"]` plus the
    `gps` edge → `["gps", "wheels"]`.
  - `test_no_edges_and_no_frames_omits_the_field`: `"frames" not in peer`.
  - `test_frames_are_checked_against_a_generated_class`: a local
    `System` subclass with `sensors: OutPort[..]`, `cmd: InPort[..]`,
    `system_status: OutPort[..]` annotations (the `demo.py` shape,
    declared inline); `frames=["cmd"]`, `["system_status"]`, `["nope"]`
    each raise with the wording above; `["sensors"]` records.
  - `test_a_bare_system_spec_is_not_checked`: `static_system("Plant")` with
    `frames=["anything"]` records it.
  - `test_emission_does_not_mutate_the_peer_spec`: `to_ir()` twice yields
    equal `peer` dicts.
- `tests/ir_contract.rs`: `golden_deployment_round_trips` already round
  trips the regenerated file; add an assertion that the `fsw` mirror's
  `peer.frames == ["gps"]` decodes.

What could go wrong: `_peer_json` returns the stored dict; folding into it
in place would leak `frames` into a second emission and into
`test_two_publishers_need_via`'s re-resolve; build a new dict. The
`members()` helper's `sim` is a `static_system("Plant")`, so the inference
tests use it and the check tests declare a class. The golden line numbers
(`src`) are normalized away by `normalize()`. `test_pyright.py`'s negative
fixture (`deployment_bad.py`) is untouched.

Green when:

```sh
(cd python && uv run python -m unittest discover tests) && cargo test -p metor-fsw-2 --test ir_contract --test py_eval
```

## WP6: the example, the integration case, docs

Files:

- `examples/adcs-fsw2/target.py:94`:
  `sim = fsw.add("plant", Subscribe(plant_sim, frames=["world"]))`, with a
  comment: the edges below pull `sensors`, `gps`, `wheels`; `world` feeds
  the eclipse chip on the ops dashboard until plan D moves the dashboards
  to `gw`, when this list goes. The plant's `ctrl_in` mirror (`:425`) gets
  no annotation and narrows to `torque_cmd`, `mtq_cmd`.

  Recommendation, of the two the revision leaves open: add `frames=["world"]`
  now rather than state the dependency. One line keeps the example's
  dashboard whole between B and D, exercises the new parameter in the one
  file users copy, and plan D deletes it with the chip it serves. Stating
  the dependency instead leaves `fsw.plant.world.illuminated` unregistered
  and the eclipse chip blank on a checked-in example.
- `tests/comms.rs`, one more case inside `peers_exchange_data`, after the
  data assertion while the child still runs: `filtered_connection_gets_its_id`
  dials `a`'s peer link (`127.0.0.1:2254`), reads the replay until the
  first non-announce packet, remembering the `VTableMsg` id whose first
  `SetComponentMetadata` name is `a.downlink.link_status.connections`
  (learned from the replay, the `mirrored_frame_arrives` shape), sends
  `LinkSubscribe { tables: [that id], msgs: [] }`, waits 300 ms, then dials
  `a`'s ground link (`2253`) with `dial`, which moves
  `a.downlink.link_status.connections` and so forces one record on the
  allowed id; then reads for 2 s: at least one `Table` with that id arrives
  and no packet with another id (`a.downlink.system_status`, the log) does.
  Forcing the change keeps the positive half independent of how often the
  slot republishes `system_status`; the grace covers batches copied before
  the request landed.
- `tests/fixtures/comms_target.py`: no change (a `Downlink` mirror has one
  output, so inference changes nothing there).
- `docs/telemetry.md`:
  - "Peers": after the dial list, one paragraph: the client sends
    `LinkSubscribe` with its ids after the identity check; the server
    copies only those packets into that connection's queue; the announce
    replay and the connect-time retained snapshots stay full; a client that
    sends nothing (the panel, the db, a gateway ingest) gets the whole
    batch. Then the `frames` rule: edges are inferred, `frames=` adds, both
    empty means everything, `@system` bindings are not edges.
  - "Bounded loss" (`:133`): "that client misses the whole batch" gains
    "its filtered copy, for a subscriber".
  - "Connection start": `LinkInfo.features` bit 0.
- `docs/wiring.md`:
  - "Deployments" example (`:85`): `mirror = fsw.add("plant", Subscribe(sim, frames=["gps"]))`
    and one sentence after the `Publish`/`Subscribe` paragraph: the mirror
    carries what fsw's edges read plus `frames=`.
  - "The wiring IR" (`:153`): "a `peer` on one makes it a mirror of another
    member's instance, narrowed to `peer.frames`".
- `docs/cli.md`: no flag text changes; no edit.
- `docs/README.md:118`: "…and reads its ports like a local system's, paying
  only for the ports it reads."

Tests: the new comms case, and the reruns the batch change touches:

```sh
cargo test -p metor-fsw-2 --test comms --test gateway --test launch
cargo test -p adcs-fsw2
```

`gateway` proves the ingest path (no request) still fills the db from both
members; `launch` proves the wall-clocked fixtures see no change;
`adcs-fsw2` resolves the narrowed mirrors and packages `fsw`.

What could go wrong: the comms case's negative half is timing-based; 300 ms
of grace at 100 Hz is thirty batches, and a flake there means the request
was not honoured, not a slow host. The `bundle.rs` `ir_sha256` moves. The
example's `commissioning` occupant reads `gps` from the slot's input, which
is edged (`:411`), so nothing in the sequence loses a frame. `adcs-ground-track`'s
`Map("plant.gps.lla")` reads `gps`, edged. If a plant-side dashboard is ever
added it needs `frames=` on `ctrl_in`; none exists.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets && cargo test -p metor-fsw-2 --test comms --test gateway --test launch && cargo test -p adcs-fsw2
```
