# Links: implementation plan

Implements `05-links.md`. Ten tasks, in dependency order. Each task ends
with `cargo test -p metor-fsw-3` green, `cargo clippy --all-targets`
clean, and a commit. T1 and T2 extend the in-process crate with no
threads involved. T3 and T4 are the async surface and the thread
adapter. T5 to T7 are the link. T8 is Python, T9 the gate, T10 docs.

Shared state is designed in `05-links.md` and not built here; nothing in
this plan reserves room for it beyond keeping `Param` extensible.

Layout after this slice:

```
libs/metor-fsw-3/
  src/
    record.rs          RecordSchema, MsgCodec, Bytes, json and bytes codecs
    system.rs          InputBinding, OutputBinding, dynamic flags
    port.rs            Input<T, W>, Output<T, W>, DynInputs, DynOutputs
    async_system.rs    AsyncSystem, Stop
    thread/
      mod.rs           Thread (the Step), mirror copy, drop counts
      group.rs         ThreadGroup: spawn, executor, launch, stop, join
    link/
      mod.rs           register_builtins, LinkStatus
      transport.rs     Transport, Listener, Dialer with backoff
      conn.rs          Connections: slots, pending buffers, read and write halves
      wire.rs          append_packet, reroot_vtable, announce blob
      publish.rs       Publish
      subscribe.rs     Subscribe
    coordinator/
      config.rs        OutputConfig, thread
      build.rs         dynamic ports, thread grouping
      table.rs         Make::{Cyclic, Async}
  tests/
    link.rs            the gate: dial, announce, records, commands
    fixtures/echo-pack/target.py   gains a Publish and a Subscribe
```

New dependencies: `postcard-schema` with `derive` (in tree through
metor-proto); `metor-proto-stellar` by path, for `PacketStream` and the
client in tests. `stellarator::net` is already a transitive dependency.

## T1. Port schema

Files: `src/record.rs`, `src/system.rs`, `src/port.rs`, `src/log.rs`,
`macros/src/{frame,record}.rs`, `src/pack/mod.rs`, `src/lib.rs`,
`Cargo.toml`, `python/metor-fsw-abi/pyproject.toml`.

1. `record.rs`: `RecordSchema` and `MsgCodec` from the design doc.
   `Record` gains `fn schema() -> RecordSchema`, no default. Helpers:
   `RecordSchema::postcard::<T: postcard_schema::Schema>(name)` filling
   `id = T::ID` and the owned schema; `RecordSchema::msg(name, codec)`
   filling `id = msg_id(name)`; `RecordSchema::frame::<T: Frame>()`
   from `T::as_vtable()` and `T::metadata(T::NAME)`.
2. `record.rs`: `pub mod json` with `encode` and `decode` over
   `serde_json`, mirroring `postcard`'s two fns; `pub mod bytes` with
   the identity pair. `Bytes`, a record whose `Read<'a> = &'a [u8]`,
   `NAME = "bytes"`, `MAX_LEN = 0`, used only by `DynInputs` and
   `DynOutputs`; its `schema()` is `Msg { id: [0, 0], codec: Bytes }`
   and is never announced.
3. `macros/src/frame.rs`: emit `fn schema()` calling
   `RecordSchema::frame::<Self>()`. `macros/src/record.rs`: emit
   `RecordSchema::postcard::<Self>(NAME)`; a `#[derive(Record)]` message
   must now also derive `postcard_schema::Schema`. Add the derive to
   every test message and to `Ping` in the fixture.
4. `log.rs`: `LogEvent`'s `Record` impl fills `schema()` through
   `RecordSchema::postcard::<LogEvent>("log")`. `SystemStatus` gets it
   from the `Frame` derive.
5. `system.rs`: `PortDef` gains `schema: RecordSchema`; `Input::def`
   and `Output::def` fill it. `VTable` and `OwnedNamedType` already
   serialize and compare.
6. `pack/mod.rs`: `ABI_VERSION` to 3, `python/metor-fsw-abi` to match.
   The descriptor grows; nothing else in the ABI changes.
7. `lib.rs`: re-export `postcard_schema` and its `Schema` derive.

Tests: `Imu::schema()` is `Frame` with one component per field plus the
timestamp op, leaves named `imu.sample` relative to the port;
`Fixed::schema()` is `Postcard` with `id == Fixed::ID`; a hand-written
JSON record round-trips through `record::json` and reports `Json` with
`id == msg_id("note_json")`; `LogEvent::schema().id` equals
`LogEvent::ID`, which differs from `msg_id("log")` (pins the split the
TODO names); the descriptor for the test table decodes and every port
carries its schema; `tests/golden/echo_pack.py` is unchanged, since the
renderer ignores schemas.

## T2. Bindings and dynamic ports

Files: `src/system.rs`, `src/port.rs`, `src/fn_system/{param,set}.rs`,
the bundle derives under `macros/src/`, `src/coordinator/{config,build,
error,table}.rs`, `src/pack/mod.rs`, `src/dl.rs`, `src/tests/utils.rs`.

1. `system.rs`: `InputBinding<W> { def: PortDef, views: Vec<View<W>> }`,
   `OutputBinding<W> { def: PortDef, writer: Writer<W> }`. `SystemInputs::
   bind(Vec<InputBinding<NoWake>>)`, `SystemOutputs::bind(Vec<Output
   Binding<NoWake>>)`. The bundle derives and `InSet`/`OutSet` ignore
   `def`. `Views` and `Writers` in `param.rs` become iterators over the
   bindings. `SystemDef` gains `dynamic_inputs: bool` and
   `dynamic_outputs: bool`, both false from `SystemDef::new`, with
   `SystemDef::dynamic_inputs()` and `dynamic_outputs()` builders.
2. `port.rs`: `DynInputs` (implements `SystemInputs` with empty `defs`
   and `dynamic_inputs = true`) holding `Vec<(PortDef, Input<Bytes>)>`
   with `iter_mut()`; `DynOutputs` likewise over `Output<Bytes>`.
   `Input<Bytes>::drain` yields `&[u8]`; `Output<Bytes>::write_bytes
   (&[u8])` skips `encode`.
3. `config.rs`: `SystemConfig` gains `outputs: Vec<OutputConfig { port,
   record }>` with `#[serde(default)]`, so existing configs and the
   golden still parse.
4. `build.rs`: in `resolve`, a type with `dynamic_inputs` accepts an
   `InputConfig` whose port is undeclared: it appends a `PortDef` to the
   planned system's instance def, cloned from the edge's ring spec (the
   spec now carries the full `PortDef`), and errors with
   `BuildError::DynamicFanIn` on a second edge. A type with
   `dynamic_outputs` takes each `OutputConfig`: look the record up in
   `table.records()`, a map from record name to `PortDef` built once
   from every registered port; `BuildError::UnknownRecord` when absent,
   `BuildError::RecordConflict` when two registrations disagree on id,
   `max_len`, alignment, or depth. An `OutputConfig` on a type without
   `dynamic_outputs`, or an undeclared input on one without
   `dynamic_inputs`, keeps today's `UnknownInput`/`UnknownOutput`.
   `PlannedSystem` gains `def: SystemDef` (the instance def); ring
   specs, `check_ids`, and `bind_rings` read it instead of `entry.def`.
5. `table.rs`: `SystemMakeFn` takes `Vec<InputBinding<NoWake>>` and
   `Vec<OutputBinding<NoWake>>` built by `bind_rings` from the instance
   def and the ring handles; the `view`/`writer` calls stay in `entry`.
   `SystemTable::records()` as above.
6. `pack/mod.rs` and `dl.rs`: the host serializes each instance `PortDef`
   next to its `RawPort`/`RawRing` (JSON in a `RawSlice`, one per port)
   so the pack builds its bindings with the same defs. This is what
   makes a pack-authored dynamic-port system possible; `Publish` itself
   is a built-in.
7. `utils.rs`: `Tap`, a `DynInputs` system recording `(port name, bytes)`
   per cycle; `Emit`, a `DynOutputs` system writing a fixed byte string
   to every output each cycle.

Tests: a `Tap` configured with `imu.imu` and `nav.nav` sees both port
names and the frames' bytes; two edges into one `Tap` port is
`DynamicFanIn`; `Emit` with `outputs: [{port: "imu", record: "imu"}]`
feeds a `nav` consumer and the frame decodes; an unknown record name
and a conflicting one error as named; a static bundle still rejects an
undeclared port; the pipeline, fault, and dl suites pass unchanged
apart from the `bind` signature.

## T3. Async surface

Files: `src/async_system.rs`, `src/port.rs`, `src/fn_system/{mod,param}.rs`,
`macros/src/system_attr.rs`, `src/coordinator/table.rs`, `src/lib.rs`,
`ring/src/lib.rs`.

1. `port.rs`: `Input<T, W: WakeSink = NoWake>` and `Output<T, W:
   WakeSource = NoWake>`. `Input<T, Notifier>::next(&mut self) -> impl
   Future<Output = Result<T::Read<'_>, RecvError>>` reading the first
   view with a record, waiting on the notifier otherwise (one
   `Notifier` per `Input`, cloned into each of its views). `drain` and
   `latest` stay available on both. `DynInputs<Notifier>::any_ready()`
   waits on one `Notifier` shared by every port's mirror.
2. `async_system.rs`: `AsyncSystem` as in the design doc; `Stop`, an
   `Arc<StopInner { flag: AtomicBool, queue: stellarator::sync::WaitQueue }>`
   with `async fn wait(&self)` and `fn is_set(&self)`, plus the
   `StopHandle` the adapter keeps to set and wake it.
3. `fn_system/mod.rs`: `AsyncSystemFn`, the `SystemFn` twin with `async
   fn call(&mut self, items, stop: Stop)`; `FnAsyncSystem<S>` implementing
   `AsyncSystem` with `InSet`/`OutSet` over `Notifier` bindings. `Param`
   gains `type InAsync` and `type OutAsync` with `bind_in_async`/
   `bind_out_async` over `Notifier` bindings; `Input<T>`, `Output<T>`,
   `DynInputs`, and `DynOutputs` map to their `Notifier` forms, every
   other param to `()`. `Cycle` for an async call carries `now =
   Timestamp::now()` at launch and the log; `&mut Log` inside `run`
   writes to the mirror log port through a `with_log_port` guard the
   adapter enters for the task's whole life.
4. `system_attr.rs`: accept either `fn execute(&mut self, ..)` or `async
   fn run(&mut self, .., stop: Stop)`; the last parameter of `run` must
   be `Stop` by type name and is excluded from `NAMES` and the `Param`
   tuple. Emit `AsyncSystemFn` for the `run` form. A block with both, or
   `run` without `Stop`, is a compile error with a `trybuild` case.
5. `table.rs`: `TableEntry.make` becomes `Make { Cyclic(Box<SystemMake
   Fn>), Async(Box<AsyncMakeFn>) }`. `AsyncMakeFn` takes the owned params
   `Value` and the `Notifier` bindings and returns a `Box<dyn Launch>`:
   `trait Launch: Send { fn launch(self: Box<Self>, stop: Stop) ->
   Result<LocalBoxFuture<'static, ()>, ParamError>; }`, so the system is
   constructed on the thread it runs on and nothing `Rc` crosses. Params
   decode inside `launch`; a `Ctor` for an async system is `Send + Sync`.
   `register_async` and `register_async_system` mirror the cyclic pair.
6. `ring/src/lib.rs`: `RingBuffer::config(&self) -> Config` (capacity and
   reader count from the geometry), for the adapter's mirror sizing.

Tests: a `#[system]` block with `async fn run` yields `NAMES` without
`stop` and the same `defs()` as its cyclic twin; `Input<_, Notifier>::
next` resolves after a write from the same thread and stays pending
before; `Stop::wait` resolves after `set` on the same thread; the
`Make::Async` entry is registered with the right def; compile-fail
cases for both malformed blocks.

## T4. Thread adapter and placement

Files: `src/thread/{mod,group}.rs`, `src/coordinator/{config,build,mod,
run,error}.rs`, `src/pack/mod.rs`, `src/lib.rs`, `src/tests/utils.rs`,
`tests/fixtures/echo-pack/src/lib.rs`, `tests/dl.rs`.

1. `group.rs`: `ThreadGroup::spawn(name, launches: Vec<(String, Box<dyn
   Launch>)>) -> Result<GroupHandle, BuildError>`. The thread runs
   `stellarator::run` around: launch every system's future under
   `catch_unwind` as a task, recording a panic message in a per-system
   `Arc<Mutex<Option<String>>>` the adapter reads; a `launch` error is
   sent back over a `std::sync::mpsc` channel and surfaces as
   `BuildError::Params` for that system; return when every task ends or
   the group's `Stop` is set. `GroupHandle` holds the `StopHandle`, the
   `JoinHandle`, and the statuses. First check, before anything else in
   this task: `stellarator::run` on a `std::thread::spawn`ed thread
   initializes its own executor, and a `Notifier` written from another
   thread wakes a task parked in it. Write that test first; if it fails,
   fix stellarator before continuing.
2. `mod.rs`: `Thread`, the `Step`. Holds per input edge a `View<NoWake>`
   on the real ring and a `Writer<Notifier>` on its mirror; per output
   port a `View<NoWake>` on the mirror and the real `Writer<NoWake>`;
   the real `log` writer; a scratch `Vec<u8>` sized to the largest
   `max_len` at build; drop counters per port; the system's status slot
   and a clone of the `GroupHandle`. `execute`: for each input edge,
   `try_read_into` then `try_write`, counting `WouldBlock`; for each
   output, the same toward the real ring; if any counter is nonzero,
   one `Error` line on `log` with `kind = mirror_dropped` and a field per
   port, then clear. `latched()` reads the status; `fault` writes a
   `panic` line with the recorded message. `Drop` on the last `Thread`
   of a group sets stop, joins with `JOIN_TIMEOUT = 1 s`, and on timeout
   emits `tracing::error!` and detaches.
3. Mirror rings: `Thread::mirrors(defs, real: &[&RingBuffer])` allocates
   one `RingBuffer::create_in_memory` per edge and per output with
   capacity `real.config().capacity * MIRROR_FACTOR` (4) and one reader.
   The log mirror is sized from `LogEvent`'s def the same way.
4. `config.rs`: `SystemConfig.thread: Option<String>`, `#[serde(default)]`.
   `build.rs`: `group_threads` collects `Make::Async` systems by
   `thread.unwrap_or("default")`; `thread` on a `Make::Cyclic` system is
   `BuildError::ThreadOnCyclic`. `bind_rings` prepares every member's
   `Launch` and `Thread` step, spawns each group once, and stores the
   `GroupHandle`s on the `Coordinator` so drop order is systems, then
   groups, then rings.
5. `pack/mod.rs`: `make` on a `Make::Async` entry builds a private group
   of one and returns the `Thread` step, so a pack's async system is an
   ordinary instance to the host and `thread` on it is accepted and
   ignored (documented).
6. `utils.rs`: `Relay`, an async system copying `Input<Imu>` to
   `Output<Imu>` through `next().await`; `Sleeper`, which never reads
   and returns only on stop; `AsyncBoom`, which panics after its first
   record; `WhoAmI`, which writes `thread::current().id()`'s hash onto
   an output once.
7. Fixture: an async `relay` system in `echo-pack`; `tests/dl.rs` runs it
   across the boundary.

Tests: `imu -> Relay -> control` delivers every frame, each on the cycle
after it was written; `Sleeper` behind a producer reports `mirror_
dropped` with a rising count once the mirror fills and the producer's
own writes never fail; `AsyncBoom` latches, its log carries the panic
line, and a `Relay` on the same thread keeps delivering; dropping the
coordinator joins every thread within the timeout (thread count returns
to baseline); two `WhoAmI`s with no `thread` report one id and a third
with `thread = "own"` another; `thread` on `imu` is `ThreadOnCyclic`;
the fixture's `relay` moves a record across the ABI.

## T5. Transport and connections

Files: `src/link/{mod,transport,conn,wire}.rs`, `src/cli/run.rs`,
`Cargo.toml`.

1. `wire.rs`: `append_packet(batch, ty, id, payload)` as fsw-2's
   `telemetry/mod.rs:210`.
2. `mod.rs`: `LinkStatus`, a `Frame` named `link_status` with
   `connections: u32`, `bytes_out: u64`, `batches_dropped: u64`,
   `inbound_dropped: u64`; `register_builtins(table)` under `fsw.`, filled
   by T6 and T7. `cli/run.rs::load` calls it before the packs.
3. `transport.rs`: `Transport { Listen { addr, max_connections: usize =
   8 }, Connect { addr } }` with `Deserialize + JsonSchema` so the params
   schema renders. `Listener::bind(addr)` in the system constructor
   (`ParamError::Decode` naming the address on failure). `Dialer` with
   `async fn connect(&mut self, stop: &Stop) -> Option<TcpStream>`
   backing off `BACKOFF_INITIAL = 500 ms` to `BACKOFF_MAX = 10 s` and
   returning `None` on stop.
4. `conn.rs`: `Connections::new(slots, pending_cap)` pre-allocating each
   slot's `pending: Vec<u8>`; `Slot { pending, queued, wake: WaitQueue,
   writer: Option<OwnedWriter<TcpStream>> }`. `enqueue(&mut self,
   batch: &[u8])` reserving against `pending_cap - queued` per open
   slot, dropping whole and counting; `open(&mut self, stream) ->
   Option<usize>`; `close(slot)`. Free fns `write_half(slot: &RefCell
   <Slot>)` waiting on `wake`, swapping the pending buffer with a spare,
   `write_all`; `read_half(reader, on_packet: impl FnMut(OwnedPacket))`
   over `PacketStream::next_grow` with a buffer reused across packets.
   `stats() -> LinkStatus`. Everything here is single-threaded and lives
   inside one async system's `run`; `Rc<RefCell<Slot>>` per slot.
5. `Cargo.toml`: `metor-proto-stellar` by path.

Tests, unit: `enqueue` past `pending_cap` drops whole and counts, and a
smaller batch after it lands; `open` past the slot count returns `None`;
`stats` reflects both; `Dialer` against a closed port returns `None`
once stop is set, within one backoff step (`stellarator::test`).
Integration (`tests/link.rs`, started here): `Listener::bind` on
`127.0.0.1:0` reports its port; a `Transport` params schema renders a
`listen`/`connect` union in the module renderer.

## T6. Publish

Files: `src/link/{publish,wire,mod}.rs`.

1. `wire.rs`: `reroot(vtable: &VTable, metadata: &[ComponentMetadata],
   prefix: &str) -> (VTable, Vec<ComponentMetadata>)`, ported from
   fsw-2's `core/src/descriptor.rs:382` `prefix_vtable` and its dynamic-
   name pass, returning the prefixed metadata beside the vtable.
   `announce(defs: &[PortDef], namespace, link: &str) -> Result<(Vec<u8>,
   Vec<Wire>), Collision>` building the blob with `LenPacket`: `LinkInfo`
   first (`command_ids` empty, `link` = the system id), then `VTableMsg`
   and `SetComponentMetadata`s per table, then `SetMsgMetadata` per
   distinct message id with `metadata["codec"]` per the design doc, and
   one `Wire::{Table { id }, Msg { id }}` per port. `table_id` is
   `metor_proto::types::table_id`.
2. `publish.rs`: `Publish`, `#[system] async fn run(&mut self, inputs:
   &mut DynInputs<Notifier>, status: &mut Output<LinkStatus>, log: &mut
   Log, stop: Stop)`. Params `{ transport: Transport, namespace: Option
   <String>, link: String, pending_cap: usize = 1 << 20 }`; Python fills
   `link` with the system id and `namespace` from the target. On entry:
   `announce`, faulting `table_id_collision` and dropping the later port
   on a collision; a batch `Vec<u8>` reserved to the sum of the ports'
   `max_len * depth`. Loop over a `select` of: accept or dial (write the
   blob to a new connection, spawn its halves; the read half discards
   every packet), `inputs.any_ready()` (drain every port with
   `append_packet`, `enqueue`, clear), and `stop`. Writes `status` when
   `stats()` changed.
3. `mod.rs`: register `publish`.

Tests, unit: golden announce bytes for one `Imu` port under namespace
`cube_sat` and one `Fixed` message port, checked against a hand-built
`LenPacket` sequence, with the `Imu` leaves resolving to
`ComponentId::new("cube_sat.plant.imu.sample")`; a batch from two
records is two packets with the right lengths and ids; the batch buffer
does not reallocate across a thousand batches (capacity pinned before
and after). Integration: `Publish(listen)` over `imu`; connect with
`metor_proto_stellar::identify`, get `Peer::Fsw` with `LinkInfo {
protocol_version: 2, namespace, link }`, then the announce in order,
then `Table` packets whose realized timestamp matches the cycle stamps;
`Publish(connect)` toward a test listener receives the same sequence
after the listener comes up late.

## T7. Subscribe

Files: `src/link/{subscribe,mod}.rs`.

1. `subscribe.rs`: `Subscribe`, `#[system] async fn run(&mut self,
   outputs: &mut DynOutputs<Notifier>, status: &mut Output<LinkStatus>,
   log: &mut Log, stop: Stop)`. Params `{ transport: Transport, link:
   String, pending_cap }`. A `Frame` output is `ParamError` at
   construction, since this slice routes messages only. On entry: the
   id set from the outputs' schemas and, for `Listen`, a `LinkInfo` blob
   with `command_ids` = that set and no announces. Loop over a `select`
   of: accept or dial (send the blob when listening; spawn the read
   half, which routes each `Msg` whose id is in the set to its port via
   `write_bytes`, counting a full mirror per port, and drops `Table`
   packets counted), and `stop`. One fault line per loop with nonzero
   counts. Writes `status` on change.
2. `mod.rs`: register `subscribe`.

Tests, unit: a routed payload lands on the right port, an unmatched id
is dropped uncounted, a `Table` packet is counted. Integration:
`Subscribe(listen)` with `Ping`; a client reads `LinkInfo` with
`command_ids == [Ping::ID]`, sends `Ping { n }` as a `Msg`, and a
consumer after `Subscribe` sees `n` on the next cycle; `Subscribe
(connect)` toward a `Publish(listen)` of a `Fixed` message port in a
second coordinator receives its records (two targets in one process,
each under `stellarator::run` on its own thread).

## T8. Python and config

Files: `python/metor-config/metor_config/{_builtins,_model,_target,__init__}.py`,
`python/metor-config/tests/`, `tests/golden/target.json`,
`tests/fixtures/echo-pack/target.py`, `src/cli/module.rs`.

1. `_model.py`: `Record` classes gain `_name`, the record name the
   builtins read; `System.__init__` gains `outputs: list[dict]`.
2. `_target.py`: `Target.add(name, system, thread=None)` emitting
   `thread`; `to_config()` fills `namespace` and `link` into any system
   that declares `_wants_target`, which `Publish` and `Subscribe` do.
3. `_builtins.py`: `Publish(items, listen=None, connect=None,
   max_connections=8, pending_cap=..., all=False)` expanding handles to
   every output including `log` and `status`, naming inputs `{system}.
   {port}`, `all` expanding at emit time to every system added before
   it, and exactly one of `listen`/`connect` or `ConfigError`;
   `Subscribe(records, listen=None, connect=None, ...)` emitting
   `outputs` and a handle resolving any listed record name. The builtin
   pack has id `fsw` and is skipped by `to_config().packs`.
4. `module.rs`: emit `_name` on record classes. `Transport`'s schema is a
   `oneOf`; the renderer already walks `$defs`, so a pack-authored system
   with a `Transport` param renders a dataclass union; a test pins it.
5. `tests/golden/target.json` and `test_target.py`: a `Subscribe(listen)`
   first, a `Publish(listen)` last, one system on `thread="io"`; a Rust
   test builds the golden's coordinator against the test table plus
   builtins.
6. `tests/fixtures/echo-pack/target.py`: `cmds = Subscribe(listen=..,
   records=[Ping])`, `echo(input=cmds.ping)`, `Publish(listen=.., items=
   [echo])`.

Tests, Python: the golden byte for byte; `Publish` of a handle lists
`log` and `status`; both or neither transport raises; `Subscribe` of a
non-record raises; `Publish(all=True)` lists only earlier systems;
`thread` lands on the system. Rust: the golden builds; `module.rs` on
the fixture is unchanged except `_name`.

## T9. Gate

Files: `tests/link.rs`, `tests/run.rs`, `src/cli/run.rs`.

1. `metor run --print-ports`: after build, print each listening system's
   bound address as `id addr` on stdout, so tests bind `:0`. `tests/
   link.rs` collects the integration tests from T5 to T7 behind one
   `OnceLock` fixture build and one running target per test.
2. Closed loop: send `Ping { n }` to `cmds`, receive `echo.output` as a
   `Msg` from `pub` with the same `n`.
3. Slow client: a `pub` connection that never reads; assert `exec_time_
   ns` for `pub` stays under a bound while a second client keeps
   receiving, and `batches_dropped` rises for one slot.
4. Simulated clock: `--sim-dt 0.001 --cycles 20000`; assert the cycle
   count, that `mirror_dropped` lines appear, and that a client
   connected before the run still gets the announce and some records.
5. `tests/run.rs`: `metor run --cycles 3` on the fixture target still
   exits zero with both links present.

Done when `tests/link.rs` passes and, from `tests/fixtures/echo-pack`,
`metor run` accepts a metor-panel connection that shows `echo.output`
and the log pane.

## T10. Docs

Files: `docs/plans/05-links.md`, `DESIGN.md`, `TODO.md`, `src/lib.rs`,
module docs.

1. `05-links.md`: fold in the `Make` enum, the `Launch` construction-on-
   thread rule, `--print-ports`, and any deviation the tasks forced.
2. `DESIGN.md`: the adapters paragraph gains the thread adapter and
   placement; the systems paragraph says state is private and sharing
   is a held parameter kind; the cross-target section names `Publish`
   and `Subscribe` with their transports.
3. `TODO.md`: slice 4 done; shared state and message id unification stay
   listed under it.
4. Crate docs: an async system example and a link example.

## Review

After T10, an antagonistic review by a second agent against
`05-links.md` and the style guide, focused on: nothing on the cycle
thread blocks or allocates after build (the copy step, `enqueue`, the
log merge); every cross-thread wake is proven by a test, not assumed;
`Rc` never crosses a thread; drop order between `Thread` steps, groups,
mirror rings, and real rings; a panicking async task cannot leave a
mirror writer claimed; the announce bytes match what metor-panel parses;
a slow or dead peer never stalls a `select` arm the other connections
share; and that the ABI change is only the descriptor's growth plus the
per-port defs.
