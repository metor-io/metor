# IO simulation: implementation plan

Implements the "Simulation" and "Threads" sections of `18-io.md`, chunk E
of `18-io-plan.md`. Seven tasks, in order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- from E2 on, the `python/metor-config/tests` suite;
- from E6 on, `cargo test -p adcs-fsw3`;
- one commit.

Order: E1 and E2 add the model side and the sim kinds, with no reactor.
E3 routes sim ops through `io_submit` and `io_complete`, and adds
`fsw.sim`. E4 adds the target seed and seeds models. E5 adds the
deterministic `Thread` mode and the fixed epoch. E6 is the GPS example.
E7 is the determinism test and the exit gate. An antagonistic review
follows E7.

## Depends on

Names are those of `18a-io-stellarator-impl.md` ("Interfaces other chunks
use") and of the other chunk plans.

From A:
- `Sqe`, `Cqe`, `Op`, `Out`, `IoStatus`, `IoError`, `RawHandle`, `Token`,
  `BufRef`, `OpKind`, `TtyOp`, `GpioOp`, `Tables`.
- `Reactor` with `push`, `submit`, `reap`, `cancel_all`, `in_flight`, and
  its private `Tokens`.
- **A1.** stellarator's `Executor::new(clock)`, `Executor::turn`, which
  runs one non-blocking pass, and `Executor::spawn`. During the pass the
  executor is the thread's current one, so `stellarator::spawn` and
  `stellarator::sleep` reach it, and the previous current executor is
  restored afterwards. `turn` never moves a clock it does not own.
- **A2.** `time::Clock::Sim`, which reads the thread-local `sim_now`, and
  `sim_set`.

From B:
- `Cap<K>`, `DeviceSide<K>`, `IoInit`, `Handle<K>`, `Pending<O>`, `Buf`,
  `MAX_OPS`, the interfaces `Uart` and `GpioLine`.
- **B1.** A `DeviceSide<K>` param binds two ports, a request input and a
  reply output, named from the param (`{param}.req`, `{param}.rep`). Both
  carry `Sqe`/`Cqe` records. It gives the system no `io_sq`/`io_cq` pair.
- **B2.** `DeviceSide::bytes(&mut self, BufRef) -> Result<&mut [u8], IoError>`
  resolves a request's `BufRef`, which names the driver's buffer, through
  the target's `Tables` the `DeviceSide` holds. One that does not resolve
  is `BadBuf`.
- **B3.** `GrantDef { param, interface, end: End::{Host, Device} }` on
  `SystemDef::grants`.
- `UartModel`, `serve_uart`, `ScriptedUart`, `test::cap::<K>(model)`, and
  `test::device::<K>()`.

From C:
- `ResourceConfig { id, backing: Backing::{Real(RealKind), Sim(SimKind),
  Peer(..)} }` and `SystemConfig.grants`.
- `io_submit` and `io_complete`, `MakeCx.io` with `IoShared` and its host
  part `IoHost`, the per-member op records, and the routing match in
  `SubmitStep` that E gives a `Sim` arm.
- C emits `SimKind` as an empty enum; E fills it.
- Python: `fsw.resource(name, kind)`, grant emission, and io emission
  after the last flight system, in `metor_config/_io.py`.

## Decisions taken here

1. **No sim `Os`.** The io steps route sim ops themselves. The reactor
   keeps one token table for real and sim ops, so `cancel_all` and
   `in_flight` cover both, and `Reset` quiesces the same way.
2. **A stopped or panicked model hangs its ops,** like an unpowered
   device. Nothing detects a dead device end. The driver's own timeout
   handles it.
3. **`DeviceSide` is refused on async systems.** A model runs on the
   coordinator thread, cyclic or as a sequence.
4. **One backing kind per target.** Build refuses a target whose io steps
   serve both a real and a sim resource.
5. **Determinism is a build option,** `BuildOptions { deterministic: bool
   }`, set by `metor run --deterministic` and by tests. It is not in
   `CoordinatorConfig`. The epoch is: it fixes the target's time.

## E1. Model traits and `serve`

Files: new `src/io/sim/mod.rs`, `src/io/sim/model.rs`,
`src/io/sim/serve.rs`, `src/io/sim/held.rs`, `src/io/sim/tests.rs`,
`src/io/mod.rs` (module and re-exports), `src/io/cap.rs` (`DeviceSide`
gains `serves: u64`), `src/io/shared.rs` (`grants_on`),
`tests/fixtures/echo-pack/src/{lib,gps}.rs` (`uart_script`),
`tests/golden/echo_pack.py`. Core, so it builds for wasm32.

1. **Traits**, one per v1 interface, in `model.rs`. `UartModel` is B's.
   The other:

   ```rust
   pub trait GpioModel {
       fn get(&mut self, now: Timestamp) -> Result<bool, IoError>;
       fn set(&mut self, now: Timestamp, level: bool) -> Result<(), IoError>;
       fn tick(&mut self, now: Timestamp);
   }
   ```

   Each trait has a latency default, `fn latency(&self, kind: OpKind) ->
   u32 { 0 }`, counted in serves. A `DeviceSide` counts its own serves in
   `serves`; a model system serves once a cycle, so a serve is a cycle.
2. **`Held`** in `held.rs`: a fixed queue of ops a model has not answered.
   Entries are `(Sqe, ready_at: u64)`, `ready_at` in serves. It is sized
   at bind to `MAX_OPS ×` the number of host grants on the resource,
   since each driver has at most `MAX_OPS` ops in flight. Build knows
   that number: `IoShared::grants_on(resource)` reads it from the
   config before any make. `push` refuses past capacity with
   `IoError::QueueFull`; it never grows. `Cancel` never enters `Held`.
3. **`serve`** in `serve.rs`, one method per interface, on `DeviceSide<K>`:

   ```rust
   impl DeviceSide<Uart> {
       /// Ticks `m` to `now`, then answers each request it can.
       pub fn serve(&mut self, now: Timestamp, m: &mut impl UartModel) { .. }
   }
   ```

   Each `serve`:
   1. counts the serve and calls `m.tick(now)`;
   2. reads new requests from the request port. A `Cancel { target }` is
      answered on arrival: a held target completes `IoError::Canceled`
      and leaves `Held`, and the cancel completes with success; a target
      not held makes the cancel `Os(ENOENT)`. Every other request enters
      `Held` with `ready_at = serves + m.latency(kind)`;
   3. for each due entry in order, calls the model; `None` keeps it held,
      and an answer writes a `Cqe` to the reply port;
   4. answers an op the interface does not take with `IoError::Unsupported`.

   Each step is its own short fn. Bytes come from the driver's buffer
   through `DeviceSide::bytes` (B2). A `BufRef` that does not resolve
   completes the op `BadBuf` without calling the model. A full reply port
   leaves the entry held for the next cycle.
4. **A model system for tests.** echo-pack's `gps.rs` gains
   `uart_script`: a `DeviceSide<Uart>` named `uart`, a `ScriptedUart`
   built from a `script: Vec<String>` param, and one `uart.serve(now, s)`
   per cycle. `tests/sim.rs` and `tests/golden/sim.py` use it, and
   `tests/golden/echo_pack.py` is re-blessed.

Tests, in `src/io/sim/tests.rs`, through `test::device::<K>()`:
- nominal: a read on a `ScriptedUart` with bytes queued completes with
  `Len(n)`, and the bytes are in the driver's buffer (`TestDevice` plays
  the driver); a write calls `tx` once with the driver's payload; a
  `Gpio(Get)` on a `GpioModel` answers its level;
- nominal: latency 3 answers on the third `serve`, not before;
  `uart_script` built from a two-line script answers two reads;
- off-nominal: a read on an empty FIFO holds, then completes the cycle
  bytes arrive; ops answer in submission order across several handles; a
  full reply port keeps the op held and loses nothing;
- failure: `tx` returning `Err` completes the op with that error; a
  `Held` at capacity completes the next op with `QueueFull`, and a
  `Cancel` arriving then is still answered; `Held` with two host grants
  holds `2 × MAX_OPS`; a `Gpio(Set)` on a `DeviceSide<Uart>` is
  `Unsupported`; `Cancel` of a held read completes it `Canceled` in the
  same serve; a `Cancel` of an unknown token is `Os(ENOENT)`; a request
  whose `BufRef` names no table completes `BadBuf` and the model sees no
  call;
- a counting-allocator test (as in `tests/wasm_alloc.rs`) shows `serve`
  allocates nothing after bind.
- Kani `held_never_exceeds_capacity`: any sequence of `push`, `take_due`,
  and `remove` keeps `len <= cap` and never panics.

## E2. Sim kinds

Files: `src/io/kind.rs` (C's `SimKind`), `src/coordinator/{build,error}.rs`,
`src/coordinator/tests.rs`, `python/metor-config/metor_config/_io.py`,
`__init__.py`, `python/metor-config/tests/test_io.py`.

1. **`SimKind`**:

   ```rust
   #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
   #[serde(rename_all = "snake_case")]
   pub enum SimKind { Uart, GpioLine }
   ```

   `SimKind::interface(&self)` returns the one interface it provides on
   both ends.
2. **Build checks** in `build.rs`, a new pass `check_device_ends(&plan,
   &config.resources)`, which replaces C3's refusal of `Sim` backings:
   - every `Backing::Sim` resource has exactly one grant with `End::Device`;
     none is `BuildError::NoDeviceEnd { resource }`, two is
     `BuildError::TwoDeviceEnds { resource, first, second }`;
   - a device-end grant names a sim resource, or
     `BuildError::DeviceEndOnReal { system, resource }`;
   - a device-end grant on an async system is
     `BuildError::AsyncDeviceEnd { system }` (decision 3);
   - host grants to both a real and a sim resource in one target are
     `BuildError::MixedBackings { real, sim }` (decision 4).

   C3's interface check covers host and device ends alike.
3. **Python** in `_io.py`: `SimUart()` and `SimGpioLine()`, each with
   `_backing()` returning `{"sim": ...}`. `Target.to_config` raises
   `ConfigError` for the same four rules, naming the resource.

Tests:
- `coordinator/tests.rs`: nominal, one model and two host grants builds;
  failure, each of the five errors, naming the resource or system.
- `kind.rs`: each `SimKind` round-trips through JSON.
- `test_io.py`: each kind's JSON; a sim resource with no model, with two,
  and a target mixing a `Tty` and a `SimUart` each raise; a `DeviceSide`
  param granted a `Tty` raises.

## E3. Routing sim ops, and `fsw.sim`

Files: `src/io/reactor.rs`, `src/io/system.rs` (C's io steps),
`src/coordinator/build.rs`, `python/metor-config/metor_config/_target.py`,
`_io.py`, `python/metor-config/tests/test_io.py`, new `tests/sim.rs`
(includes echo-pack's `gps.rs` by `#[path]`, as C8 does, for `gps` and
`uart_script`), `tests/golden/sim.json` with `tests/golden/sim.py`.

1. **The reactor's sim path.** A sim op uses the reactor's tokens but not
   its `Os`:
   - `lower(src, route: u16, sqe) -> Result<Sqe, Cqe>` maps the token and
     stamps the table, as `push` does, and marks the token as routed to
     `route`, the resource's index. A refused op returns its `Cqe`.
   - `raise(cqe) -> Option<(SourceId, Cqe)>` unmaps a reply's token, as
     `reap` does. A reply to an unknown or reactor-own token is `None`.
   - `cancel_all` also queues one `Cancel` per live routed op, under a
     reactor-own token (A's separate range), and `sim_cancels(&mut
     Vec<(u16, Sqe)>)` drains them. `requeue_sim_cancel(route, sqe)` puts
     one back. The queue is sized with the own range, so it cannot fill.
2. **Device-end edges.** `io_submit` gains one output per sim resource,
   `{resource}.req`, through `DynOutputs`. `io_complete` gains one input
   per sim resource, `{resource}.rep`, reading the model's reply port,
   through `DynInputs`. The Python builder emits them.
3. **`io_submit`'s `Sim` arm.** A sim op goes through `lower` and is
   written to its resource's request output. A full ring, or a refused
   op, is answered in `early`. After the members, `io_submit` writes
   `sim_cancels` to their resources' outputs. A cancel the `.req` ring
   refuses is requeued and retried next cycle; it is never dropped. A sim resource's handle in
   the member's map is `RawHandle::new(resource, 0)`; the model never
   reads it. A target with only sim resources still builds the host `Os`,
   which never receives an op.
4. **`io_complete`** drains each reply input before it reaps, and passes
   each through `raise`. From there a reply is a completion like any
   other: counted, and written to its member's CQ.
5. **`fsw.sim(name, system)`** in `_target.py`: `fsw.add` that records the
   system in `self._sims` instead of `self._systems`, and gives it a
   `_handles` entry like any system, so edges and channels resolve it. It
   takes no `thread=`. `to_config` appends, after `_systems`:
   `io_submit`, the sims in add order, then `io_complete`. A model added
   with `fsw.add` is emitted in place; its request edge from `io_submit`
   is then a loop edge, which the coordinator does not check.

Tests:
- `io/reactor.rs`: nominal, a lowered op's reply raised by `raise` comes
  back with the source's token; off-nominal, `cancel_all` on a source
  with one real and one routed op pushes one `Cancel` to the `Os` and
  queues one in `sim_cancels` for the route, even with every system token
  taken; failure, `raise` of an unknown token is `None`.
- `io/system.rs`: off-nominal, a sim cancel whose `.req` ring is full is
  written on the next cycle, and the held read then completes
  `Canceled`.
- `tests/sim.rs` (integration; in-process, no IO):
  - nominal: a GPS-driver-shaped system and a `ScriptedUart` model under
    `fsw.sim` order: a read submitted in cycle N completes in N+1, and
    `io_status` counts it;
  - nominal: the same model emitted before `io_submit` (as `fsw.add`
    does): the read completes in N+2, every run;
  - off-nominal: a channel `Reset` of the driver with a read held by a
    silent model remakes it a cycle later, once the model answers the
    cancel;
  - failure: a model that panics in cycle 5 ends alone; the driver's
    queued read never completes, and the rest of the target keeps
    cycling.
- `test_io.py`: emission order is flight, `io_submit`, sims, `io_complete`;
  the golden `tests/golden/sim.json` matches `sim.py`.

## E4. The target seed and seeded models

Files: new `src/io/rng.rs` (core); `src/coordinator/{config,table,build}.rs`,
`src/fn_system/{param,tests}.rs`, `src/pack/mod.rs` (pack makes pass
`seed: None`), `src/thread/unsupported.rs`, `src/io/sim/tests.rs`;
`python/metor-config/metor_config/_target.py`, `tests/test_target.py`.

1. **`Rng`** in fsw-3's core:

   ```rust
   pub struct Rng(u64); // SplitMix64
   impl Rng {
       pub fn seeded(seed: u64, name: &str) -> Self; // mix(seed, fnv1a64(name))
       pub fn next_u64(&mut self) -> u64;
       /// True with probability `p`, where `p` is a fraction of `u32::MAX`.
       pub fn chance(&mut self, p: u32) -> bool;
       pub fn below(&mut self, n: u32) -> u32;
   }
   ```

   `chance(p)` compares the high 32 bits of the next draw with `p`.
   `below(n)` uses the widening-multiply method.
2. **The seed.** `CoordinatorConfig` gains `#[serde(default,
   skip_serializing_if = "is_zero")] pub seed: u64`. `MakeCx` gains `seed:
   Option<u64>`, `Some` from the config on the host and `None` in a
   pack's make, where the ABI does not carry it, and `namespace: &'a
   str`. Skipping the default keeps every golden and the ADCS envelope
   byte-identical.
3. **`Rng` param.** A system fn may take `rng: &mut Rng`. It takes no
   port. `Writers` gains `seed: Option<u64>` and the instance name
   `"{namespace}.{system_id}"`, filled by make from `MakeCx`. `bind` seeds
   it with `Rng::seeded(seed, name)`.
4. `Rng` is core, so wasm guests get the same draws. Until the ABI
   carries the seed, a `None` seed fails the make with
   `ParamError::GuestRng`.
5. **Python.** `Target(seed=0)` stores the seed and emits
   `coordinator.seed` when it is not 0.

Tests:
- nominal: two `Rng`s seeded the same give the same stream; `below(8)`
  stays below 8 over 10k draws; two binds with one seed and one name draw
  the same sequence; a config with `seed: 7` reaches `MakeCx.seed`;
  `Target(seed=7)` emits `"seed": 7`.
- off-nominal: `chance(u32::MAX)` is always true and `chance(0)` always
  false; an empty name seeds as well as any other; two names on one seed
  draw different sequences, and adding a third system does not change the
  first two's draws; a config without `seed` decodes as 0 and encodes
  without it; the default `Target` emits no `seed`.
- failure: a negative seed raises `ConfigError`; the guest refusal is
  tested in `fn_system/tests.rs`.
- Kani: `below(n)` is below `n` for every state and every `n > 0`.

## E5. The deterministic `Thread` mode and the fixed epoch

This is the largest task. It changes how a group runs, not what a group
is. With async systems on stellarator's own IO, it makes an async
system's scheduling and timing deterministic, not its IO.

Files: `src/thread/os.rs`, `src/thread/os/group.rs`, new
`src/thread/os/turned.rs`, `src/thread/os/tests.rs`,
`src/coordinator/{build,config,run,mod,error}.rs`, `src/cli/run.rs`,
`tests/run.rs`, `python/metor-config/metor_config/_target.py`,
`python/metor-config/tests/test_target.py`.

1. **`BuildOptions`**:

   ```rust
   #[derive(Clone, Copy, Debug, Default)]
   pub struct BuildOptions {
       /// Turn every thread group on the coordinator thread, in step order.
       pub deterministic: bool,
   }
   ```

   `CoordinatorConfig::build(table)` calls `build_with(table,
   BuildOptions::default())`. `deterministic` with a `Wall` clock is
   `BuildError::DeterministicWall`. `MakeCx.threads` carries the option.
2. **The epoch.** `Clock::Simulated { dt, #[serde(default)] epoch:
   Option<Timestamp> }` (`coordinator/config.rs`). With an epoch, the
   simulated clock starts there; without one it starts at
   `Timestamp::now()`, as today. Python `Target(.., epoch=..)` emits it
   when set. `metor run --epoch <ns>` overrides it for one run, and needs
   a simulated clock.
3. **Groups as an enum.** `Threads` keeps one registry; each entry is
   `Group::Spawned(Arc<GroupHandle>)` (today's) or
   `Group::Turned(Rc<TurnedGroup>)`.
4. **`TurnedGroup`** in `turned.rs`:

   ```rust
   pub(crate) struct TurnedGroup {
       stop: StopHandle,
       /// Declared before `executor`, so the handles drop first.
       tasks: RefCell<Vec<stellarator::JoinHandle<()>>>,
       executor: Rc<stellarator::Executor>,
       /// The cycle time this group was last turned at; later steps that cycle skip it.
       turned_at: Cell<Option<Timestamp>>,
   }
   ```

   - `add(member)` builds the member immediately on this thread, inside
     the executor's scope (A1), and spawns its task. A panicking
     constructor is a `ParamError::Panicked`, as in `group::build`.
   - `turn(now)` sets the group clock (step 6), and calls
     `executor.turn()` once per cycle. Several `Thread` steps share a
     group, so the first one each cycle turns it and the rest skip it.
     `Step::execute(now)` has no cycle number (`coordinator/step.rs:19`),
     so the key is `now`: deterministic mode refuses `dt == 0` at build
     with `BuildError::DeterministicZeroDt`, and each cycle's `now` is
     then new.
   - `Drop` sets stop, turns until every task finishes or 64 turns pass,
     then clears `tasks`, then drops the executor. A's `Drop for
     Executor` cancels the rest and ticks once, so their futures, and
     any `Sleep` over its timer, drop before the timer (A step 6).
5. **The step.** `Thread::execute` in deterministic mode:
   1. `drain_inputs` (real rings into mirrors, which wakes tasks);
   2. `group.turn(now)`;
   3. `drain_outputs`;
   4. `report_drops` and `check_panic`, as now.

   Mirrors copy as they do today. A group with members early and late in
   the schedule turns at its first member's step; later members' inputs
   arrive next cycle. This is fixed by step order, so it is deterministic.
6. **Group clock.** In a sim run, the group's executor is built with A's
   `Clock::Sim`. The coordinator owns that clock: `TurnedGroup::turn`
   calls `sim_set(now)` before `executor.turn()`, and `turn` never moves
   it (A1). `stellarator::sleep(d)` in a turned group wakes on the first
   cycle whose time passes the deadline. In a wall run the group keeps the
   OS clock.
7. **`metor run --deterministic`** sets the option. It requires
   `--sim-dt` or a simulated target clock.
8. **The coordinator's own executor.** `Coordinator::run` still runs
   under `stellarator::run`. A1's scoped current executor is what lets a
   turned group's `spawn` and `sleep` reach the group, not the outer
   executor. `catch_step` already isolates a panic out of `turn`.

Tests, in `thread/os/tests.rs` unless noted:
- nominal: an async echo system in a turned group answers a record
  written in cycle N with an output read in cycle N+1, on the coordinator
  thread (assert `std::thread::current().id()` from inside the system);
- nominal: `stellarator::sleep(50 ms)` inside a turned group with
  `dt = 10 ms` wakes in cycle 5, every run;
- nominal: two systems on one group are turned once per cycle (count
  turns through a test hook);
- nominal: a config with `epoch` starts its first cycle at that time;
  `Target(epoch=0)` emits it; `--epoch` overrides it (`tests/run.rs`);
- off-nominal: a member that never yields is bounded by the tick cap, and
  the cycle still ends;
- off-nominal: a turned group inside a coordinator running under
  `stellarator::run` spawns onto the group, not the outer executor (a
  task spawned by the system is polled only during the step);
- off-nominal: a config without `epoch` decodes and encodes unchanged;
- failure: a panicking async system ends alone with `EndReason::Panicked`
  and a `panic` log line, as in threaded mode; a panicking constructor is
  `ParamError::Panicked`; `--deterministic` and `--epoch` with a wall
  clock are refused (`tests/run.rs`); deterministic mode with `dt = 0` is
  `DeterministicZeroDt`;
- drop: a group dropped with a task still sleeping drops that task's
  future before its executor (a drop flag in the future, checked after
  the group drops). A's step 6 Miri test covers the executor itself;
- every existing thread test passes in both modes (run the threaded tests
  through a helper that builds with each option).

## E6. GPS receiver model and scenario

Files: new `examples/adcs-fsw3/systems/adcs/src/gps.rs` (driver and
`Nmea` parser), new `examples/adcs-fsw3/systems/adcs/src/gps_receiver.rs`
(model), `systems/adcs/src/lib.rs` (register `gps_driver` and
`gps_receiver`), `contracts/src/frames.rs` (`GpsFault`, `FifoStats`), new
`examples/adcs-fsw3/gps_sim.py`, new `examples/adcs-fsw3/tests/gps.rs`,
`examples/adcs-fsw3/README.md` (one paragraph), `examples/adcs-fsw3/TODO.md`
(tick "GPS outage" under fault injection as a model input).

`target.py` and `sim.py` do not change.

1. **`GpsReceiver`** implements `UartModel` and holds a `GpioModel` for
   PPS:
   - `tick` encodes the true position (`BodyState.pos_eci`, `vel_eci`,
     converted to geodetic as `plant.rs` does) as `$GPGGA` and `$GPRMC` at
     the configured rate (1 Hz default, 10 Hz after `PMTK220,100`), into a
     fixed 4 KiB FIFO;
   - a driver that stops reading overflows the FIFO; overflow is counted
     in `FifoStats`;
   - `configure` with a baud other than the receiver's makes `rx` return
     bytes passed through a fixed scrambling map, so the driver sees
     garbage, not silence;
   - `GpsFault::{Outage, ColdStart { seconds }}` on its `faults` input:
     an outage stops sentences; a cold start emits no fix for the given
     time, then fixes again;
   - noise from its `Rng` (E4).
2. **`gps_driver`** is the system from `18-io.md`, writing `Gps` frames.
   `gps.rs` holds `Nmea` (push bytes, iterate fixes) as data plus short
   fns.
3. **`gps_sim.py`**: `sim.py`'s loop with `nav` and `ctrl` reading
   `gps_driver.fix` instead of `plant.gps`, the receiver attached with
   `uart, pps = GpsReceiver.attach(fsw, "gps", truth=plant.body)`, where
   `plant.body` is the plant's real `BodyState` output
   (`systems/adcs/src/plant.rs:331`; `sim.py` reads only `gps`,
   `sensors`, and `wheels` today), and a
   scenario sequence (in the seqs pack) that raises an outage at 60 s and
   clears it at 90 s.

Tests:
- unit, in `gps.rs`: `Nmea` parses a known GGA/RMC pair; rejects a bad
  checksum; resyncs after garbage;
- unit, in `gps_receiver.rs` through `test::device::<Uart>()`: rate change
  after the PMTK command; overflow counted when unread; wrong baud yields
  bytes that fail checksum; outage stops fixes;
- integration, `tests/gps.rs`: `gps_sim.py` runs 120 s of sim time;
  `nav`'s position error stays under 50 m before the outage, `gps_driver`
  writes no fix during it, and fixes resume within 2 s after it.

## E7. Determinism test

Files: new `tests/determinism.rs`, `tests/common/mod.rs` (a `tap` system
and `build_with` helper).

1. The target, on a simulated clock with `epoch: 0`: the GPS driver and
   receiver from E6's fixtures copied into `echo-pack` (a
   `ScriptedUart`-like `Chatter` model and a `reader` driver), and an
   async system on a turned group (`thread="io"`) that sleeps a fixed
   time per record and echoes `reader`'s output to an output port.
   `Chatter` takes an `Rng` (E4) and draws the bytes it sends, so the
   stream depends on the seed.
2. A `tap` system, added last, reads through `DynInputs` every `io_sq`
   port, `io_complete`'s CQ ports, every device end's `req` and `rep`
   port, and the async system's output, and appends `(cycle, port,
   bytes)` to a shared `Vec`.
3. Run 500 cycles twice with `BuildOptions { deterministic: true }` and
   seed 7, and compare the streams byte for byte. `status` and `log` ports
   are not tapped (they carry wall times).

Tests:
- nominal: the two seed-7 streams are equal and non-empty, and include
  the async system's outputs on the same cycles;
- off-nominal: seed 8 gives a different stream (`Chatter` draws different
  bytes), with the same `io_sq` traffic up to the first read of `Chatter`'s
  bytes;
- failure: a build with `deterministic: false` still runs, and the test
  documents that its streams are not compared.

## Exit criteria

- The determinism test passes, with one async system in a deterministic
  thread group.
- `tests/sim.rs` passes, including the +1 and +2 cycle placements and the
  panicked-model case.
- The GPS scenario test passes.
- Every existing fsw-3 test passes in threaded and deterministic modes
  where both apply.
- `examples/adcs-fsw3/target.py` and `sim.py` are unchanged.

## Versions

- No `CONFIG_VERSION` bump. `SimKind`, device-end edges, `seed`, and
  `epoch` are defaulted values inside version 2.
- ABI unchanged by E. Guest models are out of scope for v1: G's adapters
  refuse a `DeviceSide` in a wasm or dylib guest, because a guest cannot
  reach the target's `Tables`.
- metor-config: minor bump for `fsw.sim`, the sim kinds, `Target(seed=)`,
  and `Target(epoch=)`.

## Not done here

Deferred, in `TODO.md` §8:
- Sim kinds beyond `SimUart` and `SimGpioLine` (`SimCan`, `SimUdp`,
  `SimDir`, `SimListen`, `SimDial`), each with its driver.
- Targets that mix real and sim resources.
- Async systems doing IO through the reactor, and so deterministic async
  IO and sim links.

Also out of scope:
- Detecting a dead device end (decision 2).
- Async models (decision 3).
- Model systems in wasm or dylib guests (v1 scope).
- A threaded group clock that follows sim time across threads: a threaded
  sim run keeps the OS clock in its groups and is not deterministic.
- Model sugar for every kind; only `GpsReceiver.attach` is written.

## Open questions

1. **Async models** (decision 3) are refused. The design does not say
   either way.
2. **Determinism as a build option** (decision 5), not a field of the
   target. The design says "set per run".
3. **Settled: a sim-only target builds the host `Os` anyway.** It costs
   one idle io_uring instance. Linux 5.19 is the minimum for every target,
   so no `Option<Box<dyn Os>>` path is needed.
