# Deployment review fixes

Fixes for `REVIEW_NOTES.md` (the slice-7 and ADCS-example review). Eight
tasks in dependency order; each ends with `cargo test -p metor-fsw-3`,
`cargo test -p adcs-fsw3`, the Python tests, and clippy green, and one
commit. Finding numbers are the review's.

Two findings are scoped rather than fixed here, at the end.

## T1. Child supervision (findings 1, 9)

Files: `src/cli/children.rs`, `src/cli/run.rs`, `tests/run.rs`.

- `prefix` locks stderr per line, not for the reader's life: `drain`
  passes `std::io::stderr()` and `prefix` takes `impl Write` it locks
  inside the loop.
- `Child` gains `Drop` that kills and reaps, so a spawn failure drops
  the children already started. `spawn_all` in `run_children` keeps a
  `Vec<Child>` it pushes into, so a failed spawn returns with the vec
  dropping.
- `wait_all` polls `INTERRUPTED` beside `try_wait`; when set it sends
  `SIGINT` to every running child once (`libc::kill`) and keeps
  waiting. `run.rs` exposes the flag through a `fn interrupted() ->
  bool` beside `watch_sigint`.

Tests: `prefix` over a reader that blocks after one line does not hold
the sink's lock while blocked (a unit test with a channel-backed reader
and a sink that records lock spans); `kill -INT` on the parent of a
deployment with `--cycles` far off ends every child and the parent exits
nonzero naming none of them as failed; a `Child` dropped while running
is reaped, checked with `try_wait` after the drop.

## T2. Frame layout identity (finding 2)

Files: `src/system.rs`, `src/record.rs`, `src/port/mod.rs`,
`src/pack/mod.rs`, `python/metor-fsw-abi`, `python/metor-config/
metor_config/_config.py`, goldens.

`PortDef` gains `layout: u64`: for a frame, the FNV-1a hash of the
postcard bytes of the vtable built at the record's own name
(`T::schema()`, root-independent); for a message, zero. `Output::def`
and `Input::def` fill it from `T`. `PortDef::same_record` compares it
beside record, id, length, alignment, and depth; `RecordSchema::
same_record` loses its frame arm. Two frames of one name whose fields
differ in type or order now differ in `layout`, whatever port they sit
on. ABI 8, since `PortDef` crosses in the descriptor.

Tests: two frames named `imu` with fields `f64` and `[f32; 2]` (same
size) are not `same_record`; the same frame on two ports with different
roots is; a table registration of both under one record name is
`RecordConflict`; the resolve pass refuses a peer output whose layout
differs from the consumer's (this falls out of edge validation, pinned
with a test in `deploy/tests.rs`).

## T3. Python source identity (findings 3, 10)

Files: `python/metor-config/metor_config/{_target,_builtins}.py`,
tests.

- `_resolve` refuses a source whose `target` is not the target being
  emitted, loops included (the producer's target), with a `ConfigError`
  naming the port and saying to subscribe through a link and use
  `port()`. A source with no target (a hand-built `OutPort`) is
  accepted as today.
- `Subscribe._port` matches messages by `record._name`, the identity
  `_copies` deduplicates by, not by class.

Tests: an input fed from another target's port raises; a loop connected
to another target's port raises at emit; `port()` on a built-in
publisher's log finds the copy made from a generated pack's log class.

## T4. Announce watch per connection, bounded (findings 5, 6)

Files: `src/link/{inbox,conn,subscribe}.rs`.

- `Inbox` keeps `announced: Vec<bool>` sized to `tables` at
  construction and marks by index; unknown and repeated ids touch
  nothing. `unannounced` becomes `for_each_unannounced(&self, f: impl
  FnMut(PacketId))` over the flags, allocation-free; `subscribe.rs`
  faults inside the closure.
- The watch is a property of the inbox (`watching: bool`, set once for
  a dialer) and its state is per connection: `Connections::serve` calls
  `inbox.connected()` before its halves start, which clears the flags
  and `seen_data` and re-arms the check. A reconnect to a rebuilt peer
  faults again.

Tests: a peer that announces one table, sends data, disconnects, and
reconnects announcing a different table faults on the second connection
(inbox unit test through `connected()`, and a link integration test that
drops and restarts the publisher target on the same port); a thousand
announces of an unknown id leave the inbox's capacity unchanged.

## T5. The Db serves its own connections (findings 7, 8)

Files: `src/link/db.rs`, `src/link/transport.rs`.

`Db` keeps the `Endpoint` and its slot count instead of taking the
listener out. `run` spawns an accept loop on its own executor: each
accepted stream runs `metor_db::handle_conn(stream, db.clone())` as a
task under a drop guard held in a slot vector of `max_connections`
entries; a peer past the limit is closed at once, as a link's is. UDP
runs `handle_udp` the same way. `stop` ends the loop, and dropping the
guards ends the connections, so nothing outlives the system.
`into_listener` goes.

Tests: a `Db` with one slot refuses a second client (the first stays
served); dropping the coordinator closes a connected client's socket
within a bound.

## T6. Ingestion without the store's lock (finding 4)

Files: `../db/src/lib.rs`, `src/link/db.rs`.

`DB::ingest_table` sinks a static table under the read lock, borrowing
its vtable from the registry instead of cloning it, so the steady state
allocates nothing. A dynamic table, which realizes members on first
sample, keeps the write lock and the clone. Both walk the vtable through
one `for_each_realized` and push each field through `Pushed::push`, so
the network path and fsw-3 share one ingest. `Db::announce` keeps each
frame port's table id; `ingest` calls `ingest_table` with it. Messages
keep `push_msg`.

Tests: in metor-db, a static table lands in its components, bumps
`vtable_gen` on first sample, and refuses an unknown id or a short
buffer; in fsw-3, the existing `Db` unit test passes through
`ingest_table`.

## T7. The ADCS example (findings 12, 13, 14, unwraps, plant split)

Files: `examples/adcs-fsw3/contracts/src/lib.rs`, `systems/adcs/src/
{plant,nav}.rs`, `tests/convergence.rs`.

- `point_minus_y_at`: when `1 + dot` is within `1e-9` of zero, return
  the 180° rotation about body X. `nadir_point` passes `-pos_eci`, as
  its doc says. A unit test rotates body `-Y` by each target and checks
  it lands on `-pos` (nadir), `+vel` (HIL), and, for the antiparallel
  input, a finite unit quaternion whose rotated axis is the request.
  The convergence test's tracking reference is `target_for`, so it
  keeps passing; its README line about the target says which axis
  points at Earth.
- `nav`: the reference epoch comes from the sensors sample's stamp,
  `epoch_at((s.timestamp - first).as_secs())` with `first` latched on
  the first sample; a sample whose stamp equals the last processed one
  is skipped (the retained latest), so the filter sees each measurement
  once. `t_sim` goes.
- `plant`: the two `Normal` distributions are built in `new` from the
  params and stored; `noise` samples them. `execute` splits into
  `actuate`, `sense`, `telemeter`, `integrate`, each under forty lines.

Tests: nav's unit tests cover a repeated sample and a gap in samples; the
plant's existing tests stay.

## T8. Style (the review's last section)

Files: `src/deploy/{index,mod}.rs`, comments across the reviewed files.

- `At` becomes `struct At { target: usize, system: usize }`.
- Comments the review quotes are rewritten in the direct form; historic
  and editorial phrases go (`grep -n "legit\|used to\|previously\|fsw-2"`
  over `src/` and the example finds them).

## Scoped out

- Finding 11, WMM allocation per call: `MAG_Geomag` allocates its
  Legendre and spherical-harmonic workspaces inside the vendored C
  (`GeomagnetismLibrary.c:1137, 1244`), so the wrapper cannot reuse
  them without patching the library. That is a change to `libs/wmm`,
  planned separately: patch the two functions to take caller-owned
  workspaces, expose them on `MagneticModel`, and pin with a test that
  counts allocations. Until then nav and plant allocate once per cycle
  in that call, noted in the example's README.
- The review's validation ran the unit suites only. T1 to T7 each run
  the integration suites too; the last commit of this plan re-runs
  `cargo test -p metor-fsw-3` and `-p adcs-fsw3` end to end and
  deletes `REVIEW_NOTES.md`.
