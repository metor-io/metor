# Plan: deployments, gateway (1 of 2: the substrate)

Implements the substrate half of
[design-deployment-gateway.md](design-deployment-gateway.md). Six work
packages. Each ends with a green tree. At the end no gateway member exists:
table packet ids are content hashes everywhere, a db refuses a conflicting
vtable, db-to-db sync carries message logs and forwards commands, the
downlink's tap set is two free functions, and a link advertises a role. The
member is [plan 2](plan-deployment-gateway-2.md). Paths are relative to
`libs/metor-fsw-2` unless they start with `libs/`, `examples/`, or
`python/`.

Test commands used throughout:

```sh
cargo test -p metor-proto                                                   # WP1
cargo test -p metor-proto -p metor-proto-wkt -p metor-proto-stellar -p metor-db -p metor-db-tests  # WP3–WP5
cargo check -p metor-panel                                                  # WP3, WP5, WP6
cargo test -p metor-fsw-2                                                   # WP2, WP6
cargo clippy -p metor-fsw-2 --all-targets                                   # lints, house rule
cargo test -p adcs-fsw2                                                     # WP2, WP6 (skip w/o python3)
```

## Sequencing

```text
WP1 ─┬─> WP2 ─────> WP6
     └─> WP3
WP4 ────> WP5
```

WP1 (`table_id`) is the root of the fsw and db id changes. WP2 (the
downlink) and WP3 (the db registry and stream) both need it and touch
different crates, so they run in parallel; each is its own commit. WP4 (the
protocol additions) and WP5 (the mirror and forwarder) are a chain in
`libs/db` and `wkt`, independent of WP1–WP3 except that WP3 and WP5 both
edit `libs/db/src/lib.rs` in different functions; rebase, do not merge by
hand. WP6 (taps, subscribe functions, role) edits `src/telemetry/` after
WP2 has landed there.

## Deviations from the design doc

Found while grounding the plan. The design doc was corrected where noted.

1. `VTable` derives `Debug, Serialize, Deserialize, Clone, Schema` and not
   `PartialEq` (`libs/metor-proto/src/vtable.rs:230`). Refuse-on-differ in
   `DB::insert_vtable` compares the postcard bytes of the registered and the
   arriving vtable, the same bytes `table_id` hashes. No derive is added.
2. `State::get_or_insert_msg_log` bumps no generation; only
   `set_component_metadata` bumps `metadata_generation`
   (`libs/db/src/lib.rs:596`). A `SyncMsgs` tail that waits on
   `metadata_gen` for new logs would never wake for a log created by
   `push_msg`. WP4 bumps the generation in the vacant arm; `with_state_mut`
   already republishes it to `db.metadata_gen` (`lib.rs:150–156`).
3. `DumpMetadataResp.msg_metadata` is `Vec<MsgMetadata>` with no ids
   (`libs/metor-proto/wkt/src/msgs.rs:213`), so a mirror cannot apply it.
   The design said nothing about where a mirrored log's name and schema
   come from; each `SyncMsgs` tail sends the log's `SetMsgMetadata` before
   its first record when the log has metadata.
4. The design listed "the link tests" among the pins of `[0, 0]`-style
   table ids. `src/telemetry/link/tests.rs` calls `set_announces(&[])` and
   pins no table id. The pins are `TICK_ID` and `table_announce(id, …)` in
   `src/telemetry/subscribe.rs`'s tests (lines ~754, ~780). The db test
   crate's `1u16`/`2u16` ids (`libs/db/tests/src/lib.rs:396,474,534`) are
   dial-in producers with one vtable each and are unaffected.
5. Latest-then-live needs a seam rule the design left implicit. The tail
   takes the WAL reader first, then reads `latest()`; a first drained WAL
   record whose timestamp and bytes equal the seeded latest is skipped once.
6. `handle_packet`'s `GetDbInfo` arm hardcodes `features: 0`
   (`libs/db/src/lib.rs:1562–1567`). WP4 reads the command set, namespace,
   and feature bits off `State`.
7. `RemoteDb::mirror` is one sequential function ending in the ingest loop
   (`libs/db/src/remote/db.rs:636–679`). WP5 splits the loop out so it can
   be raced against `forward`, keeping `MirrorEvent` and the reconnect
   supervisor untouched.
8. The blanket `impl<T: Serialize + Schema> Msg for T` computes `ID` without
   the reserved-row remap that `msg_id` applies
   (`libs/metor-proto/src/types.rs:600–611`). Pre-existing; recorded, not
   changed. The wkt tests already assert no protocol id is reused.

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| `msg_id(name)` is `const_fnv1a_hash::fnv1a_hash_str_16_xor` with `[224, x] → [223, x]`; the crate also exports `fnv1a_hash_16_xor(bytes, limit)` | `libs/metor-proto/src/types.rs:604`, `const-fnv1a-hash-1.1.0/src/lib.rs:62` | `table_id` folds bytes with the sibling function and shares the remap |
| `metor-proto` has `postcard` with `alloc` | `libs/metor-proto/Cargo.toml:26` | `postcard::to_allocvec(&vtable)` is available for the hash input |
| `msg_id` is called by `libs/db/src/axum.rs:36,93` only | grep | its signature stays |
| `TelemetrySystem::init` numbers tables `(n_tables as u16).to_le_bytes()` and dedups message announces by id | `src/telemetry/mod.rs:249,258` | one line becomes `table_id(&vtable)`; the msg dedup set is the model for the table collision set |
| `PortDesc::announce(instance)` returns the prefixed vtable and metadata; `prefix_vtable` rewrites leaf component ids from the carried metadata | `core/src/descriptor.rs:328,382` | the hashed bytes carry the qualified component ids |
| `DB::insert_vtable` iterates `realize_fields`, inserts components, then `state.vtable_registry.map.insert(id, vtable)` unconditionally | `libs/db/src/lib.rs:288–320` | the refuse/no-op check goes before the loop |
| `handle_real_time_component` builds one `raw_field` vtable per component and ids it with `fastrand::u16(..)` | `libs/db/src/lib.rs:2015–2022` | `table_id(&vtable)` replaces the random draw |
| `ConnState` holds only `incoming_node` | `libs/db/src/lib.rs:1042` | nothing to delete; the design's first-draft map never existed |
| `DbInfoResp { protocol_version, features }`; `NODE_PROTOCOL_VERSION = 2`; bit 0 retired | `wkt/src/msgs.rs:925,939` | v3 adds two defaulted fields and bit 1 |
| `NODE_PROTOCOL_MESSAGES` ends with `[224,56]`, `[224,57]`, `LinkInfo::ID` | `wkt/src/msgs.rs:1160–1178` | `SyncMsgs::ID` joins it |
| Highest assigned protocol id is `[224, 61]`; `[224, 62]` was the deleted `SourceIdentity` | `wkt/src/msgs.rs:1132`, `37b01cbf` | `SyncMsgs` takes `[224, 63]` |
| The `MsgStream` arm spawns `handle_msg_stream(msg_id, req_id, log, tx.tx.clone())`, a wait→latest loop | `libs/db/src/lib.rs:1448,1769` | `SyncMsgs` spawns beside it with the same sink clone; its tails are WAL readers, not wait→latest |
| `handle_real_time_stream` rescans on `db.vtable_gen.wait()` with a visited set | `libs/db/src/lib.rs:1984` | the msg sync loop copies the shape over `metadata_gen` and `msg_log_iter` |
| `MsgLog` has `wal_reader()`, `latest()`, `wait()`, `metadata()`; `read_msg(buf) -> (rest, ts, msg)` | `libs/db/src/msg_log_2.rs`, `remote/fsw.rs:110–112` | the tail drains grants exactly as `forward` does, keeping `ts` |
| `LenPacket::msg_with_timestamp(id, ts, cap)` exists; the receiver's last `Msg` arm uses `m.timestamp.unwrap_or(now)` | `types.rs:662`, `lib.rs:1760` | source timestamps survive the mirror |
| `forward(ids, tx, db)` and `wait_any` are private in `remote/fsw.rs`; `fsw_stream` races `ingest` against it | `libs/db/src/remote/fsw.rs:44–145` | the move is a cut and paste plus `pub(crate)` |
| `fsw_stream(info, rx, tx, buf, db)` has two callers: the panel's `fsw_loop` and its own test | `libs/metor-panel/src/connections/target.rs:365`, `remote/fsw.rs:258` | the signature change touches two lines |
| `mirror` sends `GetDbInfo`, `DumpMetadata`, then `Stream { RealTime, id }` with request id 1, then loops on `handle_packet` | `libs/db/src/remote/db.rs:636–679` | `SyncMsgs` goes after `Stream`; `forward` needs the same `tx` |
| `libs/db/tests` (`metor-db-tests`) holds the `RemoteDb` integration tests and a `setup_test_db` helper | `libs/db/tests/src/lib.rs:283–330` | message-sync and forwarding round trips go there |
| `TelemetrySystem::init` lines 229–316 build taps and announces; `execute` lines 349–470 drain them; `Tap`, `Wire`, `Announce`, `TelemetryMode` are module-private | `src/telemetry/mod.rs` | the two loops move whole; the types move with them |
| `subscribe.rs` has `direct_candidates(&PeerSpec)`, `browse(&PeerSpec, &AsyncContext)`, and the identity check inline in `session` | `src/telemetry/subscribe.rs:582,603,340–355` | three small signature changes |
| `advertise(name, addr, namespace, link)` sets `(TXT_ROLE, "fsw")` | `src/telemetry/discovery.rs:27,52` | one more argument |
| The panel's `browse` builds `ConnectionTarget::tcp(instance_name(fullname), addr)`; `detail` is `pub` | `libs/metor-panel/src/connections/discovery.rs:63`, `target.rs:205` | the role reads off `info.get_property_val_str` and overwrites `detail` |
| The panel imports `TXT_*` from nothing today; `metor-proto-wkt` is a dependency | `libs/metor-panel/Cargo.toml` | `TXT_ROLE` imports directly |

---

## WP1: `table_id` beside `msg_id`

Files:

- `libs/metor-proto/src/types.rs`:

  ```rust
  /// Fold a 16-bit hash onto a packet id, off the reserved protocol row: `[224, x]` becomes `[223, x]`.
  pub const fn off_reserved(bytes: [u8; 2]) -> PacketId
  /// A table's packet id: the fold of its announced vtable's postcard bytes, off the reserved row.
  pub fn table_id(vtable: &VTable) -> PacketId
  ```

  `msg_id` becomes `off_reserved(fnv1a_hash_str_16_xor(name).to_le_bytes())`.
  `table_id` is `off_reserved(fnv1a_hash_16_xor(&postcard::to_allocvec(vtable).expect(..), None).to_le_bytes())`
  under `#[cfg(feature = "alloc")]` beside `IntoLenPacket`. The `VTable`
  type parameter is the default `VTable` (owned buffers), which is what the
  announce and the db registry hold. A doc line on the blanket `Msg` impl
  records that it does not apply the remap (deviation 8).

Tests to add (`libs/metor-proto/src/types.rs` test module):

- `table_id_is_stable_and_schema_keyed`: the same vtable twice; a vtable
  built through `vtable::builder` with two different component ids
  differs; two different shapes differ.
- `off_reserved_moves_the_protocol_row`: `[224, 7]` maps to `[223, 7]`,
  `[223, 7]` and `[1, 0]` are identities; `msg_id` of a name whose fold
  lands on the row (find one by brute force in the test) equals
  `off_reserved` of the raw fold.

What could go wrong: `fnv1a_hash_16_xor` takes `Option<usize>` for a
length limit; pass `None`. `to_allocvec` of a `VTable` cannot fail for
owned buffers; the `expect` documents that. Nothing else in the workspace
calls `msg_id`'s internals.

Green when:

```sh
cargo test -p metor-proto
```

## WP2: the downlink announces under `table_id`

Files:

- `src/telemetry/mod.rs`, `TelemetrySystem::init`: replace the
  `n_tables` counter with `let packet_id = table_id(&vtable)`; keep a
  `HashSet<PacketId>` of announced table ids; on a repeat, push
  `(entry key, first entry key)` onto a deferred list and skip the tap
  (the later entry in registry order, decision 9). After the loop, one
  `output.log().fault(LogLevel::Error, "telemetry_table_id_collision",
  "two tables hash to one packet id; the later is not downlinked",
  &[("refused", ..), ("kept", ..)])` per pair, beside the
  `telemetry_reader_slot` report. `n_tables` and its comment go.
- `src/telemetry/subscribe.rs` tests: `TICK_ID` becomes a function of the
  announced vtable, `fn tick_id(port: &PortDesc) -> PacketId` calling
  `table_id` on `port.announce("a.counter")`'s vtable; `table_announce`
  takes the port only and computes the id. `UNKNOWN_ID = [9, 9]` stays
  (an id nothing announces).
- `docs/telemetry.md`, "Downlink records": one sentence: a table packet's
  id is a hash of its announced vtable, stable across connections and
  links; two tables hashing alike are reported by
  `telemetry_table_id_collision` and the later is dropped.

Tests to add (`src/telemetry/mod.rs` test module or `src/tests.rs`, beside
the existing downlink tests):

- `table_ids_are_hashes_of_the_announce`: a `WiringBuilder` target with
  one frame producer and a `serve`d downlink; read the replay through a
  `TcpStream` as `link/tests.rs` does; the `VTableMsg.id` equals
  `table_id` of the announced vtable and is not `[0, 0]`.
- `colliding_tables_fault_and_drop_the_later`: build two `Announce::Table`
  candidates from two entries whose vtables are engineered to collide
  (construct the second by search over a component-name suffix until
  `table_id` matches; a few thousand iterations); run `init`; exactly one
  announce and one fault line naming both.

What could go wrong: `subscribe.rs`'s `bind` maps announced ids to ports by
matching the vtable, not the id, so the client is untouched; only its test
helpers pinned ids. The search-for-a-collision test must bound its
iterations and skip if none is found within them, so it cannot hang; note
the found suffix in the test as a constant once known. The adcs example's
dashboards name components, not ids, and are untouched.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets && cargo test -p adcs-fsw2
```

## WP3: the db refuses a conflicting vtable and streams under `table_id`

Files:

- `libs/db/src/error.rs`: `#[error("vtable id {0:?} is registered with a different vtable")] VTableConflict(PacketId)`.
- `libs/db/src/lib.rs`, `DB::insert_vtable`: before the field loop, under
  `with_state_mut`, if `state.vtable_registry.get(&vtable.id)` is `Some`:
  equal postcard bytes → `return Ok(())` (no component re-insert, no
  `vtable_gen` bump, a `debug!` line); different → `Err(Error::VTableConflict(vtable.id))`.
- `libs/db/src/lib.rs`, `handle_real_time_component`: `let vtable_id = metor_proto::types::table_id(&vtable);`
  in place of the `fastrand` draw (decision 4).
- `libs/db/src/remote/fsw.rs`: nothing; `ingest` already warns and
  continues on a `handle_packet` error, so a refused vtable is one warn
  line and every table under that id then fails `VTableNotFound` in
  `ingest_table`, also warned.

Tests to add (`libs/db/src/lib.rs` test module; `libs/db/src/remote/fsw.rs`
tests):

- `insert_vtable_is_idempotent_on_equal_bytes`: insert, insert again,
  `vtable_gen` unchanged the second time, one component.
- `insert_vtable_refuses_a_different_vtable_under_one_id`: two vtables
  built with different component ids, the second forced to the first's
  id; `VTableConflict`; the registry still holds the first.
- `two_links_announce_into_one_db`: two `fake_fsw`-style servers (extend
  the helper to push a `VTableMsg` and a table) announcing different
  vtables; both components hold their sample; a third server re-announcing
  the first vtable is a no-op.
- `metor-db-tests`, `remote_db_mirrors_live_data`: assert the mirrored
  `VTableMsg` id received by a raw client equals `table_id` of the
  server's per-component vtable (build the same vtable in the test).

What could go wrong: `insert_vtable` logs `info!` per call today; a
reconnect replay would log once per table per reconnect; move the line
under the "new" branch. `ingest_table` errors on `VTableNotFound` for a
refused id, which is the loud failure the design wants; make sure the
`fsw_stream` warn line includes the id. The real-time stream's per-component
vtable includes the component id, so two components never share an id
except by hash collision, now refused rather than overwritten.

Green when:

```sh
cargo test -p metor-db -p metor-db-tests && cargo check -p metor-panel
```

## WP4: `DbInfoResp` v3, `SyncMsgs`, the server-side tails

Files:

- `libs/metor-proto/wkt/src/msgs.rs`:
  - `NODE_PROTOCOL_VERSION = 3`.
  - `DbInfoResp` gains `#[serde(default)] pub command_ids: Vec<PacketId>`
    and `#[serde(default)] pub namespace: Option<String>` after `features`;
    the struct drops `Copy` (a `Vec`); every construction site gains the
    two fields (`libs/db/src/lib.rs:1563`, `metor-proto-stellar` tests,
    `libs/db/src/remote/fsw.rs` tests via `identify_tells_a_db_server`,
    found by the compiler).
  - `pub const FEATURE_MSG_SYNC: u64 = 1 << 1;` with a doc line on the
    `features` field.
  - `pub struct SyncMsgs;` with `const ID: PacketId = [224, 63];` and a doc
    comment: asks a db to stream every message log not in its command set,
    latest record first then live, over the requesting connection; gated
    on `FEATURE_MSG_SYNC`. Added to `NODE_PROTOCOL_MESSAGES`. The existing
    id-hygiene tests (`msgs.rs:1439–1460`) cover it automatically; add
    `assert_eq!(SyncMsgs::ID, [224, 63])` beside the `LinkInfo` pin.
- `libs/db/src/lib.rs`:
  - `State` gains `command_ids: Vec<PacketId>` and `namespace: Option<String>`
    (runtime, not in `DbConfig`); `DB::set_identity(&self, namespace: Option<String>, command_ids: Vec<PacketId>)`
    under `with_state_mut`; `DB::add_command_ids(&self, ids: &[PacketId])`
    with id-dedup, the `add_uplink_msgs` shape.
  - The `GetDbInfo` arm sends `protocol_version: NODE_PROTOCOL_VERSION, features: FEATURE_MSG_SYNC, command_ids, namespace` from `State`.
  - `State::get_or_insert_msg_log`'s vacant arm bumps `metadata_generation` (deviation 2).
  - A `SyncMsgs` arm beside `MsgStream`: `stellarator::spawn(handle_msg_sync(tx.tx.clone(), db.clone(), m.req_id))`.
  - ```rust
    /// Stream every message log not in the command set: `SetMsgMetadata` when known, the latest record, then live records; new logs join as `metadata_gen` moves.
    async fn handle_msg_sync<A: AsyncWrite + 'static>(sink: Arc<Mutex<PacketSink<A>>>, db: Arc<DB>, req_id: RequestId) -> Result<(), Error>
    /// One log's tail: reader first, latest second (skipping the reader's copy of it), then drain-then-wait.
    async fn sync_msg_log<A: AsyncWrite>(sink: Arc<Mutex<PacketSink<A>>>, id: PacketId, log: MsgLog, req_id: RequestId) -> Result<(), Error>
    ```
    `handle_msg_sync` loops: under `with_state`, for each `(id, _)` of
    `msg_log_iter()` not visited and not in `command_ids`, clone the log and
    spawn `sync_msg_log`; then `db.metadata_gen.wait().await`. `sync_msg_log`
    sends `SetMsgMetadata { id, metadata }` if `log.metadata()` is `Some`,
    takes `log.wal_reader()`, sends `latest()` (if any) as
    `LenPacket::msg_with_timestamp(id, ts, len)`, remembers `(ts, bytes)`,
    then loops: drain `reader.try_next()` grants through `read_msg`,
    skipping the first record equal to the remembered pair, sending each as
    `msg_with_timestamp`; `log.wait().await`. A send error that
    `is_stream_closed` returns `Ok`.

Tests to add (`libs/db/src/lib.rs` test module, using `crate::Server` and a
raw `Client` as `identify_tells_a_db_server` does):

- `db_info_advertises_commands_and_features`: after `set_identity`, a
  `GetDbInfo` reply carries them and bit 1.
- `sync_msgs_streams_latest_then_live_and_skips_commands`: two logs with
  two records each, one log in the command set; a `SyncMsgs` client
  receives the other log's metadata, its latest record with the original
  timestamp, then a record pushed afterwards, and never the command log's;
  a third log created after the request arrives with its first record.
- `sync_msgs_does_not_duplicate_the_seed`: push, request, push: exactly
  two records received.
- wkt: `DbInfoResp` v3 decodes under a local v2-shaped struct, and a v2
  encoding decodes to v3 with defaults.

What could go wrong: `DbInfoResp` losing `Copy` may break a `*resp` or a
`Copy` bound in `metor-proto-stellar`'s `Peer::Db(DbInfoResp)` users; the
compiler finds them. `msg_log_iter` borrows `State`; clone the `MsgLog`
handles out before spawning. `metadata_gen.wait()` is the same
`AtomicCell` wait the component stream uses; a lost wake is caught by the
next rescan, and the first pass runs before any wait. A log that is created
and immediately in the command set (a command arriving before `SyncMsgs`)
is excluded by id, not by order.

Green when:

```sh
cargo test -p metor-proto-wkt -p metor-proto-stellar -p metor-db -p metor-db-tests && cargo check -p metor-panel
```

## WP5: the mirror syncs messages and forwards commands

Files:

- `libs/db/src/remote/mod.rs`: `forward` and `wait_any` move here from
  `remote/fsw.rs` as `pub(crate) async fn forward(ids: Vec<PacketId>, tx: Arc<Mutex<PacketSink<OwnedWriter<TcpStream>>>>, db: &Arc<DB>) -> Error`,
  doc comment intact.
- `libs/db/src/remote/fsw.rs`:

  ```rust
  /// Stream an identified fsw link into `db` until the connection drops; forward `command_ids`' logs up it from the live edge.
  pub async fn fsw_stream(command_ids: Vec<PacketId>, rx: PacketStream<..>, tx: PacketSink<..>, buf: Vec<u8>, db: &Arc<DB>) -> Error
  ```

  `info` is no longer a parameter; the test passes `info.command_ids`.
- `libs/metor-panel/src/connections/target.rs:365`: `fsw_stream(info.command_ids.clone(), rx, tx, buf, &db)`.
- `libs/db/src/remote/db.rs`, `mirror`: after `Stream`, if
  `info.features & FEATURE_MSG_SYNC != 0`, `client.send((&SyncMsgs).with_request_id(2))`.
  Then split `Client { tx, rx, .. }`, wrap `tx` as today, and
  `futures_lite::future::race(ingest(rx, tx.clone(), db), forward(info.command_ids, tx, db))`,
  where `ingest` is the existing loop moved into a private fn returning
  `Error` like `fsw.rs`'s. `mirror`'s `Result<(), Error>` return maps the
  raced `Error` to `Err`, so the supervisor's `is_stream_closed` branch is
  unchanged.

Tests to add (`libs/db/tests/src/lib.rs`, beside `remote_db_mirrors_live_data`):

- `remote_db_mirrors_message_logs`: the server db has one log with a
  record and metadata; a `RemoteDb` into a temp db; the local log holds the
  record with the server's timestamp and the metadata; a record pushed on
  the server afterwards arrives.
- `remote_db_forwards_advertised_commands_and_never_echoes`: the server db
  has `set_identity(None, vec![CMD])`; a `RemoteDb` mirror; `push_msg(CMD)`
  into the local db lands in the server's `CMD` log; the local `CMD` log
  holds exactly one record after a settle (no echo); a `CMD` record pushed
  on the server before connect never reaches the local db.
- `libs/db/src/remote/fsw.rs`: `fsw_stream_ingests_and_forwards_live_edge`
  updated for the new signature; a second case passing a subset forwards
  only the subset.

What could go wrong: `forward` pends forever on an empty id set, so a
plain db mirror races an ingest loop against `pending()`, which is what
`fsw_stream` does today for a link with no uplink. The `Client` type owns
`tx`/`rx`; the split already happens at `db.rs:661`. A server older than
WP4 answers v2 with no bit set and never receives `SyncMsgs`; a client
older than WP5 never sends it. Request id 2 for `SyncMsgs` is arbitrary;
replies are never expected.

Green when:

```sh
cargo test -p metor-db -p metor-db-tests && cargo check -p metor-panel
```

## WP6: taps, subscribe functions, the advertised role

Files:

- `src/telemetry/taps.rs` (new), `mod taps;` in `src/telemetry/mod.rs`:
  `Tap`, `Wire`, `Announce`, and `TelemetryMode` move here as
  `pub(crate)`, with

  ```rust
  /// The tap set one system claims: views, announces, and the entries no reader slot was left for.
  pub(crate) struct Taps { pub taps: Vec<Tap>, pub announces: Vec<Announce>, pub retained: usize, pub exhausted: Vec<String>, pub collisions: Vec<(String, String)> }
  /// Filter `all` by `mode`, claim one view per entry, and build its announce under `table_id`.
  pub(crate) fn collect_taps(all: &AllOutputs, mode: &TelemetryMode) -> Taps
  /// Walk every tap once: a snapshot tap yields its newest record when `committed` moved, a log tap every record. Returns the count of corrupt reads.
  pub(crate) fn drain_taps(taps: &mut [Tap], mut on_record: impl FnMut(&Tap, &[u8])) -> usize
  ```

  `collect_taps` is `TelemetrySystem::init`'s loop verbatim, including the
  WP2 collision set, minus the `link.set_announces`/`set_retained_slots`
  calls, which stay in `init` after the call. `drain_taps` is `execute`'s
  `for tap in &mut self.taps` loop with the retained-slot branch expressed
  as the callback seeing the tap (`tap.retain_slot`) and the record; the
  downlink's callback does what the loop body does today
  (`append_record` into the batch, `link.retain` for a retained tap).
- `src/telemetry/mod.rs`: `init` and `execute` call the two functions;
  `LinkStatus`, batching, `append_record`, `append_packet`, and the
  `link_*` faults stay.
- `src/telemetry/subscribe.rs`:

  ```rust
  pub(crate) fn direct_candidates(host: Option<&str>, port: u16) -> Vec<SocketAddr>
  pub(crate) async fn browse(namespace: &str, link: &str, context: &AsyncContext) -> Option<Vec<SocketAddr>>
  /// `Err` carries the `peer_identity` detail for a wrong version, namespace, or link.
  pub(crate) fn check_identity(info: &LinkInfo, namespace: &str, link: &str) -> Result<(), String>
  ```

  `session` calls the third where the two `if`s are today (lines
  340–355); `run` calls the first two with `self.peer`'s fields.
- `src/telemetry/discovery.rs`: `advertise(name, addr, namespace, link, role: &str)`;
  `LinkState::start` passes `"fsw"`.
- `libs/metor-panel/src/connections/discovery.rs`: in `ServiceResolved`,
  after `ConnectionTarget::tcp(..)`, `if let Some(role) = info.get_property_val_str(TXT_ROLE) && role != "fsw" { target.detail = format!("{addr} · {role}").into() }`;
  `use metor_proto_wkt::{FSW_SERVICE_TYPE, TXT_ROLE}`.
- `docs/telemetry.md`, "Local discovery": the role record may read
  `gateway` for a db a gateway member serves.

Tests to add:

- `src/telemetry/taps.rs`: `collect_taps` over a `WiringBuilder` target
  with one frame producer, one message producer, and a `Subset` mode:
  the expected entries, one `Announce::Table` under `table_id`, one
  `Announce::Msg`, `retained == 1` for a snapshot message tap.
  `drain_taps` over hand-built rings: a snapshot tap yields once per
  change and nothing when unchanged; a log tap yields every record in
  order; a corrupt view counts.
- The existing downlink and link tests pin behaviour: `link/tests.rs`,
  the downlink cases in `src/tests.rs`, `tests/comms.rs`, and the adcs
  suites must pass unchanged.
- `subscribe.rs` tests: `check_identity` on version 1, a wrong namespace,
  a wrong link, and a match; `direct_candidates(None, p)` is both loopback
  families, `Some("10.0.0.5:2242")` is that address alone.
- Panel: `libs/metor-panel/src/connections/tests.rs` gains a case that a
  resolved `ServiceInfo` with `role=gateway` yields `detail` ending in
  `· gateway` and one with `role=fsw` keeps the bare address (build the
  `ServiceInfo` in the test; no daemon).

What could go wrong: `AllOutputs::entries()` borrows the output bundle
while `output.log()` needs `&mut`; `collect_taps` returns the deferred
reports as data and the caller logs them after, the pattern `init` uses
today. `drain_taps`'s callback needs `&mut` access to the link and the batch
at once; closure captures of two `&mut` fields of `self` are fine when
destructured first (`let Self { taps, batch, .. } = self`). The subscribe
functions stay `pub(crate)`; plan 2's `src/gateway/` is in the same crate.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets && cargo check -p metor-panel && cargo test -p adcs-fsw2
```

The example stays two members through this plan; `cargo test -p adcs-fsw2`
proves the id change and the tap split did not move it.
