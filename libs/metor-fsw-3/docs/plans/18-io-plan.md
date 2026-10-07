# IO: plan

This plan builds `18-io.md` in six chunks. Each chunk has its own
implementation plan, `18a` through `18g` (there is no D), and lands on
its own. A chunk ends when its exit criteria hold, its tests pass, and an
antagonistic review is done.

## Crate layout

This layout is fixed for every chunk, so they can be planned in parallel.
There is no new crate.

- **metor-fsw-3 core** (wasm32-capable), in `src/io/`:
  - the op types: `Sqe` (48 bytes), `Cqe` (40 bytes), `Op`, `Out`,
    `IoStatus`, `IoError`, `RawHandle`, `Token`, `BufRef`, `RawAddr`,
    `TtyOp`, `GpioOp`, `OpKind`, with zerocopy little-endian fields, and
    their `Record` impls;
  - the buffer table, `BufTable`, and `Tables`;
  - `Io`, `Handle`, `Pending`, `Slot`, `Buf`, `Cap<K>`, `DeviceSide<K>`,
    `IoInit`, the model traits, and `Rng`.
- **metor-fsw-3 `host`**, one file each: the `Os` trait (`push`,
  `submit`, `reap`, `register`) with `FdClass`; `Reactor`; `UringOs`,
  over its own io_uring instance; and `PollOs`, over `polling`, for Unix
  development hosts. Also `io_submit` and `io_complete`.
- **stellarator** keeps its executor, ops, and public API. It gains
  `Executor::turn` with a simulated clock, and `TcpListener::adopt`.
- **Settled rules every chunk follows:**
  - A system's buffer table is one arena with its directory inside it. A
    `Buf` is a slot index. The arena may move while the system is made,
    and freezes when the io steps are built.
  - A channel `Reset` or `Load` quiesces: it cancels the system's ops,
    and the system is remade, or the candidate loaded, once `in_flight`
    is zero, into the same arena from its start. A remake that needs
    more than the arena's capacity fails with `IoMemory`.
  - The target seed is `CoordinatorConfig.seed`. Model systems read it.
  - A system has at most `MAX_OPS` (16) ops in flight.
  - Async systems keep stellarator's own IO. They take no `Cap<K>`.
  - No layers. Faults are injected through models.
  - Only F bumps `CONFIG_VERSION` (2 to 3). Only G bumps the ABI (9 to
    10).

## Chunks

| Chunk | What | Depends on | Plan |
|---|---|---|---|
| A | IO core: ops, buffers, `Os`, reactor, small stellarator additions | none | `18a-io-stellarator-impl.md` |
| B | The system surface in fsw-3 | A's op types and buffer table | `18b-io-surface-impl.md` |
| C | `io_submit`/`io_complete`, `io_status`, real resources, Python | A, B | `18c-io-integration-impl.md` |
| D | Removed from this slice | | `18d-io-layers-impl.md` |
| E | Simulation and deterministic thread groups | A, B, C | `18e-io-sim-impl.md` |
| F | Links and names | A, B, C | `18f-io-links-impl.md` |
| G | Guests | A, B, C | `18g-io-guests-impl.md` |

Order: A, B, C, then E and G in parallel, then F.

### A. IO core

- **Op types and buffers** in fsw-3's core.
  - The op types as `#[repr(C)]`, `IntoBytes` + `TryFromBytes`, with
    `Sqe` 48 bytes and `Cqe` 40.
  - The buffer table (one arena, `freeze`) and `Tables`.
- **`Os` and reactor** behind `host`.
  - The `Os` trait: `push`, `submit`, `reap`, `register`, with `FdClass`.
    Nothing blocks. Each backend's constructor takes the buffer tables and
    the dial targets.
  - A dial target is data, not a descriptor. The constructor stores its
    address, and dial target `i` gets the handle `RawHandle::new(i, 0)`.
    `Resolve` on its handle answers the address, and `Connect` makes the
    socket: a linked socket and connect op on io_uring (Linux 5.19+), a
    nonblocking `socket()` and `connect` on poll.
  - `UringOs` and `PollOs`, one file each.
  - `Reactor`: sources, token mapping, `push`/`submit`/`reap`,
    `cancel_all`, `in_flight`, `push_own`, and teardown in `Drop`. The
    reactor's own ops use a token range of their own.
- **stellarator.** `Executor::turn`, which never moves a clock it does not
  own, `Clock::Sim` with a 1 µs tick, and `TcpListener::adopt` over a
  `std::net::TcpListener`. Tasks drop before their executor.
- **Exit.** Both backends pass their tests; every existing stellarator and
  fsw-3 test passes; the buffer table and the `EXEC` swap pass under Miri.

### B. The system surface

- **Queue and buffers.** `Io`, its SQ/CQ ports, and the `Record` impls for
  `Sqe` and `Cqe`. `Handle`, `Pending`, `Slot`, and `Buf`, over A's
  buffer table.
- **Params.** `Cap<K>` and `DeviceSide<K>` as `Param`s, with one `io_sq`/
  `io_cq` pair per system that holds a `Cap`, before `log`.
- **Constructors.** Forms taking `&IoInit`. Build binds before it makes.
- **Device ends.** `DeviceSide` holds the target's `Tables` and resolves a
  request's `BufRef`, which names the driver's buffer, through it. B
  passes a fresh `Tables`; C passes the target's.
- **Config.** `grants` on `SystemConfig`; `GrantDef`s on `SystemDef`. Build
  checks grants against them.
- **`test::cap`** and `test::device`.
- **Models.** The `UartModel` trait and `ScriptedUart`, which B's tests
  need. E builds `GpioModel` beside it.
- **Exit.** The GPS driver from `18-io.md` passes its unit tests against a
  `ScriptedUart`, with no reactor or OS. Every existing fsw-3 test passes.

### C. Integration

- **`MakeCx.io`.** One new field shares `IoShared` between the io steps
  and the IO systems: the target's `Tables` and the members.
- **The io systems.** `io_submit` and `io_complete` as builtin systems.
  `io_submit`'s make opens the real resources and builds the reactor.
- **`io_status`.** Counted in the io steps, one `ResourceStatus` per
  changed resource.
- **Reset and Load quiesce.** The channel adapter cancels the system's
  ops, waits in a `quiescing` state, and remakes it or loads the
  candidate once `in_flight` is zero. The ctor's side rewinds the arena
  (`BufTable::rewind`). On an IO system, a `Reset` or `Load` takes
  effect a cycle or more later. `io_submit`'s make freezes every
  member's table.
- **Real resources.** `resources` on `CoordinatorConfig`, and the real kinds
  as a closed enum (with `TcpListen` and `TcpConnect`), opened at build and
  handed to the `Os` with `register`. A `TcpConnect` is not opened: its
  address goes to the `Os`'s constructor.
- **Python.** `fsw.resource`, grant emission, io emission, stubgen `Cap`
  kwargs, and `ResourceStatus`.
- **Versions.** None. New fields are defaulted.
- **Exit.** The GPS driver runs against a pty. The ADCS example builds and
  runs unchanged.

### E. Simulation

- **Models.** `DeviceSide::serve`, `GpioModel` beside B's `UartModel`,
  latency in serves, and `uart_script`, a model system for tests.
- **Sim kinds.** `SimUart` and `SimGpioLine`. Build refuses a target that
  mixes real and sim resources.
- **Routing.** `io_submit` writes sim ops to device-end rings and
  `io_complete` drains the replies, through the reactor's one token table.
  `fsw.sim` placement.
- **The target seed** (`CoordinatorConfig.seed`, `MakeCx.seed`,
  `Target(seed=..)`), and `Rng`, the model systems' RNG seeded from it.
- **Threads.** The `Thread` adapter's deterministic mode, which turns a
  group on the coordinator thread and sets its clock to the cycle's time.
  It makes async scheduling and timing deterministic, not async IO.
- **Epoch.** `Clock::Simulated { dt, epoch }`, `Target(epoch=..)`, and
  `metor run --epoch`.
- **Examples.** A `GpsReceiver` model and a plant-driven GPS scenario in
  `examples/adcs-fsw3`.
- **Determinism.** A test runs a sim target twice on one seed and epoch,
  and compares the op and completion streams and an async system's
  outputs.
- **Exit.** The determinism test passes, with one async system in a
  deterministic thread group.

### F. Links and names

- **Links on resources.**
  - `via=` on `Publish`, `Subscribe`, and `Db`, naming a resource.
  - A `TcpListen` is opened at build and adopted by the link with
    `TcpListener::adopt`. A `TcpConnect` is an address the link dials.
  - `Transport`'s variants map onto `TcpListen`, `TcpConnect`, and the
    `Peer` backing.
  - Deploy rewrites resources.
  - The `listen=`, `connect=`, and `peer=` shorthands stay.
- **Cyclic dialing.** `Dial::connect` issues `Resolve`, then `Connect`.
- **Name resolution.**
  - The `static` resolver.
  - The `harness` resolver, with `LISTEN_FDS` and `READY=1`, replacing
    `METOR_DEPLOY_PORTS` in tests, and the `--resolve harness` switch.
- **Versions.** `CONFIG_VERSION` 3.
- **Exit.**
  - `examples/adcs-fsw3/target.py` is unchanged and runs.
  - The deploy tests pass under `harness` without fixed ports.

### G. Guests

- **ABI 10.** The buffer-table region on `metor_fsw_create`, shared for
  dylibs and copied for wasm. `GrantDef`s in the definition JSON. G adds
  the region form of the buffer table. The pack freezes and rewinds the
  arena; the host only reads it.
- **`DlSlot` joins the io system,** and every candidate gets its region.
- **Guests quiesce** on `Reset` and `Load` like host systems.
- **No guest models.** A guest with a `DeviceSide` is refused.
- **Exit.** A fixture pack with a `Cap<Uart>` system passes the same test
  built in, as a dylib, and as a wasm module.

## Every implementation plan

Each chunk's plan:
- names every file it touches, and what changes there;
- orders its steps so each step builds and its tests pass;
- lists tests for each step, in the categories `CLAUDE.md` names:
  - nominal, off-nominal, and failure cases;
  - Kani or Miri proofs where they apply;
- states its exit criteria and its version bumps;
- lists what it does not do, and what it leaves for a later chunk;
- follows `CLAUDE.md`:
  - short functions, and data first;
  - allocation only at init;
  - no `unwrap` without a `PANIC Safety` comment;
  - sparse doc comments in plain English.
