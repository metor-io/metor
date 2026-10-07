# IO as a queue: research report

> Superseded by `18-io-queues.md`, which renames "devices" to capabilities
> (`Cap<K>`, `fsw.cap`). This report keeps the names used while it was written.

This report asks how metor-fsw could treat a system's IO as data. The idea is
to put the syscalls a system makes on a submission queue. In flight, io_uring
drains that queue. In a test, a simulated OS drains it, and it can inject
faults. Decisions are collected below. Questions keep their **Q** numbers
where the text still refers to them.

## Decisions so far

- **Target.** fsw-3.
- **v1 scope.** UART read and write, quick termios ioctls, GPIO, CAN
  (SocketCAN), network links, and files. Timers stay on the cycle clock and
  stellarator's wheel.
- **Deferred to a second pass** (not descoped): I2C, SPI, and `tcdrain`.
  These are the ops that block on a bus, and they need per-bus threads. v1
  runs only ops io_uring executes natively, plus ioctls short enough to run
  inline. The io_uring backend therefore stays thread-free. See §8.
- **OS.** Only Linux flies. The kqueue and Windows backends favor simplicity:
  inline syscalls plus a fixed worker pool. The sim backend matters more than
  either.
- **Latency.** IO that cannot wait a cycle per step runs on a background
  thread as an async system. Cyclic systems keep the submit-N, reap-N+k rule,
  with no same-cycle turns. So the async path must be mockable too: a
  swappable stellarator reactor and clock are part of v1, not an extra.
- **Surface.** A cyclic system takes `io: &mut Io`. The coordinator owns the
  SQ and CQ, as it owns `status` and `log`. Each instance has its own queue,
  so fault rules and counters are per system.
- **Sim.** One simulated OS, `SimOs`, serves both cyclic `Io` and async
  systems. Peripherals are realistic device models, such as a GPS or an IMU.
  Deterministic fault injection is a separate layer that a target stacks
  over any OS, simulated or real, and configures. See §5.
- **Named devices.** A target declares each device once, under a name, as
  either a real kind (`Tty("/dev/ttyTHS0")`) or a sim model
  (`GpsReceiver(..)`). Systems, fault rules, and status refer to the name.
  Switching between real and sim changes one declaration. See §4.
- **Buffers.** A `Buf` returns to its pool on drop.
- **Shared devices.** Several systems may bind one device, with the OS's
  semantics. Two readers split bytes, and writers may interleave. Avoiding
  that is the author's job. See §4.
- **Record/replay.** Not built now, and not designed out. Ops and completions
  are already plain data. They get stable wire schemas when replay is built.
- **Deterministic sims** run the plant and the device models on the flight
  target, under one coordinator.
- **Discovery is name resolution.** Targets refer to other targets' devices
  by name. Each deployment orders resolvers (`static`, `harness`, `mdns`, `sim`).
  Resolving is its own op, so it can be faulted. There is one DNS-SD service
  type per wire protocol. See §6.
- **Stacks** are per thread. `SimOs` is one per target, owned by the
  coordinator thread, and other threads reach it through a forwarding proxy.
  A deterministic run keeps every thread on the coordinator thread.
- **Layers only decide.** They never create ops: no retries, splits, or
  watchdog cancels.
- **The default resolver chain** is `["static"]`, matching today. It can
  change later.
- **Layers in flight** are allowed. Whether a target may carry them is a
  config policy.
- **Guests.** Wasm and dylib systems do IO through the queue. Ops are plain
  data over handles and a buffer pool: indices, never pointers.
- **stellarator.** One data op model replaces the two `OpCode` traits.
  `TcpStream`, `File`, and serial move onto it, so async systems can be
  mocked through the same backends.

## 1. What exists today

### stellarator

- **Ambient reactor.** `EXEC` is a thread-local `Executor<DefaultReactor>`.
  Every op reaches it through `Executor::with_reactor` (`src/lib.rs`), so
  `TcpStream::read` never names a queue. `Executor<R: Reactor>` is generic,
  but `with`, `with_reactor`, `spawn`, and `sleep` exist only for
  `DefaultReactor`. Every resource is therefore bound at compile time to the
  platform reactor, and no reactor can be swapped in.
- **Two op models behind one name.** Both backends export `Completion<O>` and
  `ops::*`, but the `OpCode` traits have nothing in common:
  - uring (`src/uring/mod.rs`): `unsafe fn sqe(&mut self) -> squeue::Entry`
    plus `output(cqe)`. The op is a description, and the kernel executes it.
  - poll (`src/poll/mod.rs`): `fn poll(Pin<&mut Self>, cx)` plus
    `event() -> Option<polling::Event>`. The op is a future that retries a
    nonblocking syscall whenever the fd is ready.

  Neither op is plain data, so neither can be recorded, compared in a test,
  or executed by anything other than its backend.
- **Owned buffers.** `BufResult<T, B> = (Result<T>, B)` and `IoBuf`/`IoBufMut`
  follow the completion model. When a uring op is dropped in flight, its
  buffer is parked in `OpState::Ignored` until the CQE arrives. This is the
  cancellation-safety rule every design here must keep.
- **Readiness emulation already exists.** The poll backend sends sockets
  through `polling`, which is kqueue on macOS and AFD/IOCP on Windows. It
  sends files, `open`, and serial reads to the `blocking` thread pool, since
  regular files are always "ready". This is a crude form of the emulation Q3
  asks about.
- **Clock.** `Timer::new(os_clock())` is hardcoded in both `try_new`s. A
  simulated clock would have to be injected here.
- **The run loop** (`Executor::run`) is `timer.try_turn` → `process_io` →
  `scheduler.tick` → poll the main task → `wait_for_io(next deadline)`. Every
  part of a single non-blocking "turn" is there, but only the blocking loop
  is public.

### metor-fsw-3

- **Cyclic systems** are `#[system]` fns. Every parameter is injected through
  the `Param` trait (`src/fn_system/param.rs`): ports, `Timestamp`, `Log`,
  `Sample`, `Seq`, `Abort`. Explicit injection is already the house style.
- **Sequences already poll once per cycle.** `SequenceStep::execute` polls the
  body with `Waker::noop()` once each cycle (`src/sequence/step.rs`), and
  `seq.sleep` and `wait_for` are futures that compare the cycle time against
  a deadline. This is the "poll_once" model from Q2, working today, without
  wakers.
- **Async systems** own all real IO today (links, db gateway). The `Thread`
  adapter runs them on a stellarator executor on another thread and copies
  mirror rings each cycle. They are not deterministic: when a mirror copy
  lands depends on thread scheduling.
- **Wasm and dylib guests** cross an ABI. A guest cannot hand the host a
  pointer. Any IO it does has to be described as bytes and indices.
- **Rings** are single-producer, lossless queues with per-reader cursors,
  which is the same shape as an io_uring SQ or CQ.

## 2. Prior art

| System | Queue | Completion delivery | Faults | Handle |
|---|---|---|---|---|
| sans-IO (quinn-proto, str0m, rustls unbuffered) | none: a state machine | returned from `poll_*` | the caller mangles the byte stream | explicit |
| smoltcp | `phy::Device` tokens | `iface.poll(now)` returns | `FaultInjector` device wrapper | explicit |
| TigerBeetle | caller-owned `Completion` over uring, kqueue, IOCP | callback on the next `tick` | sim `Storage` and packet models, seeded | explicit (comptime generic) |
| FoundationDB | `INetwork` / `IAsyncFile`, Net2 or Sim2 | Flow futures | Sim2 plus `BUGGIFY` sites | ambient global |
| madsim, turmoil | drop-in tokio | futures | kill, partition, fail rate; libc clock overrides | ambient thread-local |
| compio, monoio | one `OpCode` trait, per-backend methods | futures over `push` → key, `pop(key)` | none | ambient |
| cFS UT-Assert | stub libraries linked in place of OSAL | direct return | `UT_SetDefaultReturnValue`, `UT_SetDeferredRetcode` | link time |
| F Prime | typed ports and a topology | sync return or async port | the tester's `from_*` handler returns an error | explicit wiring |

What carries over:

- **One op type, per-backend decisions.** compio's `OpCode` has a method per
  backend:
  - `create_entry` for uring;
  - `pre_submit -> Completed | Wait(fd, interest) | Blocking` for poll;
  - `operate(*mut OVERLAPPED)` for IOCP.

  "Blocking" ops go to a thread pool. The driver owns pinned in-flight ops,
  which makes cancellation safe. This is the closest match to stellarator.
- **Readiness emulation has one recipe.** TigerBeetle's `darwin.zig`, monoio's
  legacy driver, and compio's poll driver all do the same thing:
  1. Try the nonblocking call at submit.
  2. On `EAGAIN`, park the op and register interest with kqueue.
  3. On readiness, retry and push the op to a local completed queue.

  File IO, `fsync`, and `close` run synchronously, because regular files are
  always "ready". macOS has no usable `EVFILT_AIO`.
- **Simulate at two layers.** TigerBeetle's simulator does not mock syscalls.
  It swaps whole `Storage` and `MessageBus` models, which carry latency
  distributions, torn writes on crash, misdirected writes, and a fault
  atlas that keeps faults recoverable. Syscall-level stubs, the cFS style,
  suit unit tests. Device models suit system sim. Both are useful, and they
  sit at different layers.
- **Explicit beats ambient for determinism.** The ambient designs (FDB,
  madsim, S2's "mad-turmoil") must override `clock_gettime` and `getrandom`
  in libc to keep hidden inputs out. S2 also runs a meta-test that repeats a
  seed and byte-compares the trace logs. The explicit designs (TigerBeetle,
  F Prime, smoltcp, sans-IO) put every input in the signature.
- **Completions are next-tick inputs.** Every stepped design delivers a
  completion on a later tick, never re-entrantly from submit. This holds
  whether the delivery is a callback (TigerBeetle), a return value (sans-IO,
  Polar Signals), or a key polled later (compio).
- **Windows.** The `polling` crate's Windows backend is AFD readiness built
  on IOCP. Using it to emulate completions stacks two emulations. `IoRing`
  covers files only: no sockets and no timeouts. RIO covers sockets only.

Sources: TigerBeetle's io_uring/kqueue post and `src/io/darwin.zig`,
`src/testing/storage.zig`; apple.github.io/foundationdb/testing.html;
github.com/madsim-rs/madsim; s2.dev/blog/dst;
polarsignals.com/blog/posts/2025/07/08/dst-rust;
docs.rs/compio-driver `OpCode`; github.com/bytedance/monoio;
github.com/nasa/osal (ut_assert), nasa/cFE#997; nasa.github.io/fprime;
windows-internals.com IoRing vs io_uring.

## 3. Q1: how a system gets its queue

### Options

**A. Ambient.** The executor installs a thread-local IO handle, as stellarator
does now and as madsim and turmoil do. Existing code (`TcpStream::connect`)
runs unchanged against a simulator.
- Against: the handle is hidden, and it is per thread rather than per system.
  Several systems share one thread (`thread=` groups), so a fault rule cannot
  say "fail nav's reads" without extra tagging. It also conflicts with the
  style guide's composability rule, and nothing in the `Param` surface works
  this way.

**B. Explicit parameter.** `io: &mut Io` is a `Param` like `Output<T>`. The
coordinator binds it per instance to a driver named in config.
- For: it matches how ports work. The fault or sim wiring is visible in the
  target file. Each system has its own queue, so per-system accounting and
  per-system fault rules come free.
- Against: resources such as a `Uart` or `Socket` must keep a handle to the
  queue they were opened on. They cannot be free-floating.

**C. IO as ports.** This is B taken literally. A system writes `IoReq` records
to an output and reads `IoDone` records from an input. The "OS" is another
step in the schedule:
- a uring driver in flight,
- a device-model system from a sim pack in test,
- an interposer system for fault injection.

This makes the OS just another system. Record/replay becomes telemetry: the
IO rings are recorded like any other port.
- Against: request payloads are copied into ring records, which limits
  their size to the record's `max_len`. Every IO pays a record encode.

### Chosen

B, with the pair hidden: `Io` is a typed façade over two coordinator-owned
rings. §4 covers how resources outlive a cycle.

## 4. Resources across cycles

A system holds a file descriptor across cycles as a **handle**, not an fd.
Its in-flight ops are **pending tokens**, and its memory is **pool
buffers**. All three are plain indices, which lets them live in `#[state]`,
cross the guest ABI, and mean the same thing to every backend.

### Handles

```rust
#[derive(Clone, Copy)]
pub struct Handle<K> { slot: u16, gen: u16, _k: PhantomData<K> }
// K: File, Socket, Uart, Can, Gpio (I2c, Spi in the second pass)
```

- A handle indexes the instance's handle table. The table is sized at build
  (`max_handles`).
  - Under io_uring, a slot is a direct descriptor: `openat` and `accept` with
    `file_index`, and ops use `IOSQE_FIXED_FILE`. No process fd is exposed.
  - Under the sim, a slot names a device model.
  - In a guest, it is two `u16`s.
- `gen` makes a closed handle stale. An op on a stale handle completes with
  `IoError::Stale`, and it never reaches the resource that reused the slot.
- A handle is local to one `Io`. Another system's handle is out of range or
  stale there.

There are two ways to get one.

**Declared.** A `Dev<K>` parameter names a device the way an `Input<T>`
names a port. Build binds it to one of the target's named devices and opens
it. "Named devices" below covers how. `Dev<K>` derefs to a `Handle<K>`.

**Runtime.** `io.open(dir, path, opts)`, `io.connect(dev)`, and
`io.accept(dev)` complete with a `Handle<K>`. These are for reconnects, log
rotation, and servers. Each takes a declared device to start from:
- a `Dir` device for `open`, as with WASI preopens or `cap-std`;
- a `TcpConnect` or `TcpListen` device for `connect` and `accept`.

So even runtime IO reaches only what the target named.

### Named devices

A **device kind** is registered in the table, beside system types. A
**device** is an instance the target declares under a name. This matches
systems: the table holds types, and config holds named instances.

```rust
// Built-in real kinds: tty, can, gpio_line, dir, tcp_listen, tcp_connect, udp.
table.register_device::<GpsReceiver>("gps_receiver", GpsReceiver::new)?; // a sim kind; provides Uart
```

§5 covers what a kind registers and what a sim model implements.

```python
gps1 = fsw.device("gps1", Tty("/dev/ttyTHS0", baud=115200))
# or, with nothing else in the file changed:
gps1 = fsw.device("gps1", GpsReceiver(model="m9", truth=plant.gps_truth))

gps = fsw.add("gps", GpsDriver(uart=gps1))
```

- **Interface checks.** Every kind provides one interface.
  - `Tty` and `GpsReceiver` both provide `Uart`.
  - `CanSocket` and a sim `CanBus` both provide `Can`.

  Build checks that a `Dev<Uart>` parameter is bound to a device providing
  `Uart`, the way it checks that a port's record matches. The generated
  Python types `uart` as `Device[Uart]`, so pyright catches a mismatch too.
- **Kinds decide the OS.** A real kind opens through the real backend, and a
  sim kind lives in `SimOs`. Build knows the side of every device, so a
  mixed target's router is fixed at build (§5).
- **One name, everywhere.** Fault rules (`resource="gps1"`), `status`
  counters, the panel, and the recorder later all use the device's name.
  None of them use a path or a port.
- **Several systems may bind one device**, with the OS's semantics. See
  "Sharing a device" below.
- **IR.** `CoordinatorConfig` gains `devices: Vec<DeviceConfig>`, where
  `DeviceConfig { id, ty, params, inputs }`. A system's `Dev<K>` parameter
  maps to a device id the way an input maps to a producer.
- **Rust tests** skip the table: `TestIo::new().device("gps1",
  ScriptedUart::new(script))`.

Prior art:
- Zephyr's devicetree aliases and Linux udev's stable names both give
  hardware a logical name.
- WASI preopens and `cap-std` grant a name instead of ambient access.
- F Prime's topology names component instances and wires them. It does the
  same for drivers.

Lifetimes:
- `io.close(h)` submits a close, and `h` is stale at once.
- A channel `Reset` rebuilds the system over the same rings. Declared handles
  stay open, because the binding holds them. Runtime handles are closed, as
  if the process exited.
- A dropped or faulted system closes everything it opened.

### Pending ops

```rust
#[must_use]
pub struct Pending<O: Op> { token: u32, gen: u32, _o: PhantomData<O> }  // not Clone

impl Io {
    pub fn read(&mut self, h: impl AsHandle<Readable>, buf: Buf) -> Pending<Read>;
    /// Takes the completion out of `slot` if it has been posted.
    pub fn poll<O: Op>(&mut self, slot: &mut Option<Pending<O>>) -> Option<O::Done>;
}
```

- **Submission never fails synchronously.** A full SQ, a stale handle, or a
  refused op all come back as a completion on the next cycle. The system has
  one error path, and a fault rule can inject any of them.
- **Dropping a `Pending` does not cancel the op.** Its CQE is discarded when
  it lands, and its buffer returns to the pool then. This is the uring
  backend's `Ignored` rule, moved into the driver. `io.cancel(p)` submits
  `AsyncCancel` and completes with `Canceled` or the real result.

### Buffers

- Each instance gets a pool at build: `bufs × buf_len`, set in its config.
  It is the only IO memory the instance has, so the "no allocation after
  init" rule holds. Under io_uring, the pool is registered as fixed buffers.
- `io.buf() -> Option<Buf>` takes a free slot. A `Buf` holds an `Rc` to its
  instance's pool and returns its slot on drop. Cloning the `Rc` does not
  allocate. The pool lives on the side the system's code runs on, so a
  guest's `Buf` points at a pool in guest memory, and no host pointer
  crosses the ABI. A `Buf` an op holds is dropped by the driver when the
  CQE lands, or when it is discarded. `io.bytes(&buf)` and
  `io.bytes_mut(&mut buf)` give access to its bytes.
- An op takes the `Buf` by value. Its completion returns it:
  `ReadDone = (Result<usize, IoError>, Buf)`, the same rule as `BufResult`.
- In a guest, the pool lives in guest memory. The adapter copies the slots
  of submitted and completed ops across around each step, as it does for
  rings.

### Example

```rust
#[derive(Default)]
struct GpsState { rx: Option<Pending<Read>>, parser: Nmea }

/// Reads NMEA from a UART and publishes fixes.
#[system]
fn gps(#[state] s: &mut GpsState, io: &mut Io, uart: Dev<Uart>, fix: &mut Output<Gps>, log: &mut Log) {
    if let Some((res, buf)) = io.poll(&mut s.rx) {
        match res {
            Ok(n) => s.parser.push(&io.bytes(&buf)[..n]),
            Err(e) => log.fault("uart", e),
        }
    }
    while let Some(f) = s.parser.next() {
        let _ = fix.write(&f);
    }
    if s.rx.is_none()
        && let Some(buf) = io.buf()
    {
        s.rx = Some(io.read(uart, buf));
    }
}
```

In a sequence, `Io` takes `&self`, as `Output::write` does, and the same
calls return futures:

```rust
let (res, buf) = io.read(uart, buf).await;
```

The future is the `Pending`, polled once per cycle by `SequenceStep`.

In an async system, `stellarator::net::TcpStream` becomes a `Handle<Socket>`
plus the thread's reactor. It issues the same ops.

### Sharing a device

Several systems may bind one device. Sharing has the OS's semantics and
nothing more. This is the same contract as two threads holding one fd:
- Two readers of a stream split its bytes, in whatever order their reads
  complete.
- Two writers may interleave, and a short write's remainder can land after
  another system's write.
- Avoiding those is the author's job, as it is with threads and an fd.
  Usually that means one reader, and writers whose writes are small enough
  to complete whole.

What the framework does:
- **Opens a declared device once per target.** Each binding system's handle
  table gets an entry pointing at it. A system still reaches only what it
  was bound to.
- **Ties the device's lifetime to the target.** `io.close` on a declared
  device detaches the calling system only. A channel `Reset` of one user
  leaves the device open. Runtime-opened handles (`accept`, `open`) stay
  per system.
- **Allows sharing across threads.** One fd can be used from several rings,
  so a cyclic system and an async system may share a device. No framework
  state is shared, because declared devices live for the whole target.
- **Keeps the sim's order.** Several handles reach one model through the
  interface adapter, in submission order. Within one stack, that is step
  order, so a sim run is reproducible. In flight, the order is whatever the
  kernel does.
- **Leaves faults per system.** Rules still match by system. A fault can
  target the sequence's writes to `gps1` and leave the driver's alone.

The cost is one level of indirection from a system's handle table to the
target's device table.

### Not designed out

- **Record/replay.** SQEs and CQEs are already plain data that passes
  through one place, the driver turn. A recorder is one more layer (§5),
  which needs stable schemas once it exists.

## 5. Simulation and fault layers

An `Io` reaches its OS through a stack. Layers sit on top and one OS sits at
the bottom:

```
nav ─┐
gps ─┼─ Io ─▶ [ faults ] ─▶ [ recorder, later ] ─▶ Os: Uring | Poll | SimOs
imu ─┘                                                       │
                                                  devices: GpsReceiver, Imu, ..
```

A layer is a decision function over plain data. It never calls the next
layer. It returns a verdict, and the stack applies it. This keeps a layer
deterministic and testable alone, and no callback is threaded through it.

### Ops

```rust
pub struct Sqe { pub token: Token, pub handle: RawHandle, pub op: Op }
pub struct Cqe { pub token: Token, pub res: Result<u32, IoError> }

pub enum Op {
    Read { buf: BufRef, offset: Option<u64> },
    Write { buf: BufRef, offset: Option<u64> },
    Recv { buf: BufRef }, Send { buf: BufRef }, SendTo { buf: BufRef, addr: SockAddr },
    Accept, Connect { addr: SockAddr },
    Open { path: PathRef, flags: OpenFlags }, Close, Fsync,
    Tty(TtyOp), Gpio(GpioOp),
    Cancel { target: Token },
}

/// `Op` without its data, for matching.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum OpKind { Read, Write, Recv, Send, SendTo, Accept, Connect, Open, Close, Fsync, Tty, Gpio, Cancel }
```

### Layers

```rust
pub trait Layer {
    /// Decides what happens to a submission on its way down.
    ///
    /// `bytes` is the write payload, if the op has one.
    fn submit(&mut self, cx: &mut Cx<'_>, sqe: &Sqe, bytes: Option<&mut [u8]>) -> Down {
        Down::Pass
    }

    /// Decides what happens to a completion on its way up.
    ///
    /// `bytes` is the filled part of a read buffer.
    fn complete(&mut self, cx: &mut Cx<'_>, cqe: &mut Cqe, bytes: Option<&mut [u8]>) -> Up {
        Up::Pass
    }

    /// Applies a command from the target's `io_commands` input.
    fn command(&mut self, cmd: &LayerCommand) -> Result<(), CommandError> {
        Err(CommandError::Unsupported)
    }
}

pub enum Down {
    Pass,
    /// Answers here. The op never reaches the layers below.
    Answer(Result<u32, IoError>),
    /// Holds the op, then passes it on after this many cycles.
    Defer(u32),
}

pub enum Up {
    Pass,
    Defer(u32),
    /// Drops the completion. The op hangs until it is cancelled.
    Swallow,
}

pub struct Cx<'a> {
    pub now: Timestamp,
    pub cycle: u64,
    /// The submitting system's instance id, as an index.
    pub system: SystemIx,
    /// The device the handle came from, by its name.
    pub resource: ResourceIx,
    pub op: OpKind,
    /// This layer's stream, seeded from the target seed and the layer's name.
    pub rng: &'a mut Rng,
}
```

A layer may change `cqe.res`, for a short read or an errno, and it may change
`bytes`, for corruption. It cannot create ops. A layer that retries or splits
ops would need a richer trait. Nothing in v1 needs one.

### The stack

```rust
pub struct Stack {
    layers: Vec<Box<dyn Layer>>,
    os: Box<dyn Os>,
    /// Deferred entries, ordered by (release cycle, sequence).
    held: Held,
    /// Per token: owner, resource, and the depth of the layer that answered it.
    tokens: Tokens,
}

pub trait Os {
    /// Takes an op. A full queue becomes an immediate `Cqe`.
    fn submit(&mut self, sqe: Sqe, done: &mut Fifo<Cqe>);
    /// Collects completions. `SimOs` advances its devices to `now`.
    fn turn(&mut self, now: Timestamp, done: &mut Fifo<Cqe>);
}
```

Every container is sized at build. Held entries and tokens are bounded by
the in-flight limit, the sum of every instance's `max_ops`, so they cannot
overflow.

One turn, at a fixed point in the cycle:
1. Release the held entries that are due, in `(cycle, seq)` order.
2. Take each system's new SQEs, in step order. Run each down the layers
   from the top. The first verdict other than `Pass` stops it there, and
   the stack records that layer's depth on the token.
3. `os.turn(now)` collects the completions.
4. Run each completion up through only the layers above the one that
   answered it. A layer never sees a completion for an op it never saw.
5. Post the survivors to each system's CQ.

A `Cancel` of a swallowed or deferred op is answered by the stack itself,
with `Canceled`.

### Fault layer

A fault layer is a `Layer` over a rule table:

```rust
#[derive(Deserialize, JsonSchema)]
pub struct FaultParams { pub rules: Vec<Rule> }

#[derive(Deserialize, JsonSchema)]
pub struct Rule {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub when: Match,
    pub act: Act,
}

#[derive(Deserialize, JsonSchema)]
pub struct Match {
    pub system: Option<String>,
    pub resource: Option<String>,
    pub op: Option<OpKind>,
    pub window: Option<(f64, f64)>,
    pub every: Option<u32>,
    pub p: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
pub enum Act {
    Fail(Errno),
    Short { max: u32 },
    Corrupt { bits: u32 },
    Delay { cycles: u32 },
    Hang,
    Refuse,
}
```

- Build resolves names to `SystemIx` and `ResourceIx`, so matching at run
  time compares integers.
- `submit` checks the rules in order. The first match maps its `Act` to a
  verdict:
  - `Fail` and `Refuse` give `Answer(Err)`;
  - `Delay` gives `Defer`;
  - `Short` and `Corrupt` mark the token for `complete` to apply on a read
    (or apply to `bytes` now on a write);
  - `Hang` marks the token for `complete` to `Swallow`.
- `command` toggles a rule by name. A rule can be declared disabled and
  enabled later by a scenario or the panel. No rule is allocated at run time.

Tests drive a layer directly:

```rust
let mut faults = Faults::new(&params, &names, seed);
let verdict = faults.submit(&mut cx("gps1", OpKind::Read), &sqe, None);
assert_eq!(verdict, Down::Answer(Err(IoError::Os(Errno::EIO))));
```

### Device kinds, bindings, and models

"Device" means two things, and they are kept apart:
- A **device** is a named entry in config (§4). It is the same for real and
  sim.
- A **model** is the behavior of a simulated peripheral. Only sim devices
  have one.

Nothing behavioral is shared between a `Tty` and a `GpsReceiver`. Opening,
closing, status, counters, and interface checks are the same for both. The
stack and build do those, not the kind. So the shared contract is
registration data, and it ends in a binding that names its side:

```rust
/// Registered in the table. Describes a kind and binds its instances.
pub struct DeviceKind {
    pub name: &'static str,
    pub provides: Interface,                   // Uart, Can, GpioLine, Listen, Dial, Dir, ..
    pub params: RecordSchema,
    pub inputs: fn(&DefCx<'_>) -> Result<Vec<PortDef>, DefError>,
    pub bind: fn(Params<'_>, Vec<InputBinding>) -> Result<Binding, ParamError>,
}

pub enum Binding {
    /// Opened by the real backend: a path and its setup, a socket address,
    /// or an inherited fd.
    Real(RealOpen),
    /// Another target's device, resolved through the resolver chain (§6).
    Peer(DeviceName),
    /// Runs in `SimOs`.
    Sim(Box<dyn Model>),
}
```

- **Real kinds are built in** for v1: `tty`, `can`, `gpio_line`, `dir`,
  `tcp_listen`, `tcp_connect`, `udp`. Each one's `bind` returns a `RealOpen`,
  data the real `Os` acts on at build: `open`, termios, CAN filters, a GPIO
  line request. Kinds a user writes for real hardware, such as a vendor
  ioctl sequence, wait until there is a case for them.
- **Sim kinds come from packs.** Their `bind` builds a model from params
  and bound inputs, so a `GpsReceiver` reads plant truth from a port.
- **Peers** are a third side, not a kind a user picks. A `via=` that names
  another target's device binds to `Peer`.

### Models are written per interface

The op-level contract (`op(&Op) -> Reply`) makes every model handle every
op, so a UART model would have to reject `Accept`. A model is written
against its interface instead. `SimOs` holds one adapter per interface that
turns ops into calls:

```rust
pub trait UartModel {
    /// Copies received bytes into `buf`. `None` means the FIFO is empty: not ready.
    fn rx(&mut self, now: Timestamp, buf: &mut [u8]) -> Option<usize>;
    /// Takes bytes the system wrote, and returns how many it accepted.
    fn tx(&mut self, now: Timestamp, bytes: &[u8]) -> Result<usize, IoError>;
    /// A termios change. A baud mismatch is the model's to simulate.
    fn configure(&mut self, now: Timestamp, cfg: &Termios) -> Result<(), IoError>;
    /// Advances the model to `now`. A GPS emits NMEA into its FIFO here.
    fn tick(&mut self, now: Timestamp);
}

pub trait CanModel { /* recv, send, set_filters, tick */ }
pub trait ListenModel { /* accept, tick */ }
```

- Implementing `UartModel` is what makes a kind provide `Uart`. The interface
  check follows from the trait, with no separate declaration.
- The adapter owns readiness and latency. `rx` returning `None` parks the
  read until a later `tick`. A model may also report a per-op latency. The
  adapter turns that into the CQE's release cycle.
- Unit-test doubles implement the same traits, for example a
  `ScriptedUart` that replays bytes and errors.
- Device-level faults, such as an outage, a cold start, or a stuck
  register, are params and commands of the model. The fault layer stays
  transport-only.

Models never wrap real hardware. Real and virtual swap at `Os` (§5), and a
real peripheral completes through io_uring, not through a synchronous call.

### End to end: a GPS

A sketch. Names in the test harness are not settled.

**The flight system** is the same for every binding:

```rust
#[derive(Default)]
struct GpsState {
    rx: Option<Pending<Read>>,
    tx: Option<Pending<Write>>,
    configured: bool,
    nmea: Nmea, // sans-IO parser, fixed capacity
}

/// Sets the receiver to 10 Hz, then publishes each fix it reports.
#[system(io(bufs = 4, buf_len = 256))]
fn gps(#[state] s: &mut GpsState, io: &mut Io, uart: Dev<Uart>, fix: &mut Output<Gps>, log: &mut Log) {
    if let Some((res, _buf)) = io.poll(&mut s.tx) {
        match res {
            Ok(_) => s.configured = true,
            Err(e) => log.fault("gps_config", e),
        }
    }
    if let Some((res, buf)) = io.poll(&mut s.rx) {
        match res {
            Ok(n) => s.nmea.push(&io.bytes(&buf)[..n]),
            Err(e) => log.fault("gps_read", e),
        }
    } // `buf` returns to the pool here.

    while let Some(f) = s.nmea.next() {
        let _ = fix.write(&Gps::from(f));
    }

    if !s.configured
        && s.tx.is_none()
        && let Some(cmd) = io.copy_buf(PMTK_RATE_10HZ)
    {
        s.tx = Some(io.write(uart, cmd));
    }
    if s.rx.is_none()
        && let Some(buf) = io.buf()
    {
        s.rx = Some(io.read(uart, buf));
    }
}
```

- Each cycle, the system takes what completed, acts on it, and submits what
  is next.
- A failed config write is retried next cycle, because `configured` is
  still false.
- A failed read is logged, and a new read is submitted.
- `io(bufs, buf_len)` is the pool's default, and an instance's config can
  override it.

**The model** lives in a sim pack:

```rust
pub struct GpsReceiver {
    truth: Input<GpsTruth>,
    fifo: ByteFifo<4096>,
    baud: u32,      // what the receiver speaks
    line_baud: u32, // what the driver configured
    rate: Duration,
    next: Timestamp,
}

impl UartModel for GpsReceiver {
    fn rx(&mut self, _now: Timestamp, buf: &mut [u8]) -> Option<usize> {
        (!self.fifo.is_empty()).then(|| self.fifo.read(buf))
    }

    fn tx(&mut self, _now: Timestamp, bytes: &[u8]) -> Result<usize, IoError> {
        if let Some(rate) = pmtk::rate(bytes) {
            self.rate = rate;
        }
        Ok(bytes.len())
    }

    fn configure(&mut self, _now: Timestamp, cfg: &Termios) -> Result<(), IoError> {
        self.line_baud = cfg.baud;
        Ok(())
    }

    fn tick(&mut self, now: Timestamp) {
        while self.next <= now {
            if let Ok(Some(t)) = self.truth.latest() {
                let line = nmea::gga(&t);
                let garbled = self.line_baud != self.baud;
                self.fifo.push_lossy(if garbled { garble(&line) } else { &line });
            }
            self.next = self.next + self.rate;
        }
    }
}

table.register_device::<GpsReceiver>("gps_receiver", GpsReceiver::new)?;
```

The model reproduces the realistic failures by itself:
- A driver that stops reading overflows the FIFO.
- A wrong baud gives garbage.
- The rate command takes effect.

**The target** chooses the binding, and the system does not change:

```python
# flight
gps1 = fsw.device("gps1", Tty("/dev/ttyTHS0", baud=9600))

# sim, with transport faults
gps1 = fsw.device("gps1", GpsReceiver(baud=9600, truth=plant.gps_truth))
fsw.io.layer(Faults("transport", rules=[
    Rule("gps_bitflip", resource=gps1, op="read", act=Corrupt(bits=1), p=0.01),
    Rule("gps_dropout", resource=gps1, op="read", act=Hang(), enabled=False),
]))

gps = fsw.add("gps", GpsDriver(uart=gps1))
```

**A unit test** uses a scripted model and drives a few cycles. It covers
the nominal, off-nominal, and failure cases:

```rust
#[test]
fn test_gps_reassembles_a_split_sentence() {
    let mut t = TestIo::new().device("uart", ScriptedUart::new([
        Rx::at(0, b"$GPGGA,123456.00,3723.2475,N"),
        Rx::at(1, b",12158.3416,W,1,08,0.9,545.4,M,,M,,*hh\r\n"),
    ]));
    let mut gps = t.system(gps(), GpsState::default);
    t.step(&mut gps, 3);
    assert!(gps.output::<Gps>("fix").latest().is_some());
}

#[test]
fn test_gps_retries_config_after_eio() {
    let mut t = TestIo::new()
        .device("uart", ScriptedUart::silent())
        .layer(Faults::once(OpKind::Write, Act::Fail(Errno::EIO)));
    let mut gps = t.system(gps(), GpsState::default);
    t.step(&mut gps, 3);
    assert_eq!(t.device::<ScriptedUart>("uart").written(), [PMTK_RATE_10HZ]);
    assert!(gps.log().faults().any(|f| f.code == "gps_config"));
}
```

- The second test runs the production fault layer, so its rules and
  verdicts are tested where they are used.
- A cold-start sequence can bind `gps1` too. Its command and the driver's
  rate command are each small, so each completes whole in practice. The
  sequence must not read, or it would steal the driver's bytes.

### Mixed targets

A target can mix real and sim devices, for example in a HIL rig with a real
GPS and a simulated IMU. The bottom of the stack is then a router:

```rust
/// Sends each handle's ops to the OS that opened it.
pub struct Routed {
    real: Box<dyn Os>,
    sim: SimOs,
    /// Per handle slot, which OS owns it. Fixed by the device's kind.
    owner: Vec<Side>,
}
```

`Routed` implements `Os`. Layers above it see one completion stream and
cannot tell the two sides apart. Such a run is not deterministic: the real
side's timing is real. A fully virtual target uses `SimOs` alone, and a
fully real one uses `Uring` alone, so neither pays for routing.

### Target config

```python
fsw = Target(cycle_rate=100.0, seed=7)
plant = fsw.add("plant", Plant(..))

gps1 = fsw.device("gps1", GpsReceiver(model="m9", truth=plant.gps_truth))
gps = fsw.add("gps", GpsDriver(uart=gps1))
fsw.io.layer(Faults("transport", rules=[
    Rule("gps_bitflip", resource=gps1, op="read", act=Corrupt(bits=1), p=0.01),
    Rule("gps_dropout", resource=gps1, op="read", act=Hang(), enabled=False),
]))
```

- Layers stack in the order added, top first.
- A deterministic sim runs the plant and the devices on the flight target,
  under one coordinator and one seed (Q7).
- Whether a target may carry layers at all is a config policy, not a build
  restriction (Q8). For example, a `Target(io_layers=False)` that build
  enforces.

### Where stacks live

There is one stack per executing thread: the coordinator thread, and each
thread group.
- In a deterministic sim, thread groups turn on the coordinator thread, so
  everything shares one stack and one `SimOs`.
- In flight, each thread's reactor has its own stack, and the layers are
  built per stack from the same config. A rule naming a system lands in the
  stack of the thread that runs that system.
- **`SimOs` is not per thread.** A sim may run thread groups on their own
  threads, for interactive runs or HIL-like timing. A system on one thread
  still talks to the same simulated GPS as a system on another. So there
  is one `SimOs` per target, owned by the coordinator thread.
  - Every other thread's stack ends in a `SimProxy: Os`. It forwards SQEs
    to the owner over a channel, and takes CQEs back at its next turn.
  - Models stay single-threaded, and so do the `Input` rings they read.
  - A run with threads crossing to `SimOs` is not deterministic. A
    deterministic run turns every thread group on the coordinator thread,
    so no proxy is involved.
- Layers stay per stack, so a fault rule always acts on the thread that
  submitted the op. That keeps layers lock-free.

## 6. Links, names, and discovery

### Today

A link's address lives in its params:
- `Publish` and `Subscribe` each carry a `Transport`: `Listen`, `Connect`,
  or `Peer`.
- `metor build` rewrites a `Peer` into a `Connect`, using the deployment's
  `hosts`.
- fsw-3 has no discovery.
- Deployment tests pick free ports up front and pass them in through
  `METOR_DEPLOY_PORTS`. A port can be taken between the pick and the bind.

### Devices are already a naming system

Discovery is not a layer on top of devices. A named device already has the
three parts of a name service:
- a **name**: `{namespace}.{device}`;
- a **binding**: the name maps to something that implements it. That is a
  path, an address, or a sim model;
- a **resolution time**: when the binding becomes known.

Service discovery is the case where the binding of a *remote* name becomes
known at run time. So a target never says how a peer is found. It says
which name it talks to, and the deployment says how names resolve.

```python
# plant target: provides a name
down = plant.device("down", TcpListen("0.0.0.0:0"))
plant.add("pub", Publish([sim], via=down))

# fsw target: refers to it; nothing about addresses or mDNS
fsw.add("plant", Subscribe([sim], via=down))
```

A device of another target, passed as `via`, is a `Peer` reference. There
is no `Discover` kind.

### Resolvers

A deployment lists resolvers in order, like `nsswitch.conf`. The OS uses that
list to resolve a `Peer`:

| Resolver | Resolves when | Provider side | Serves |
|---|---|---|---|
| `static` | build: `metor build` bakes `name → addr` from `hosts` and fixed ports | nothing | fully static flight networks |
| `harness` | spawn: `metor run` binds every listener before it starts any child | the child adopts its inherited listener | ephemeral tests |
| `mdns`, later `gossip` | run: query, plus a cache on a resolver thread | the listener advertises `{name}` | the target named, not its IP |
| `sim` | run, deterministically: the `SimOs` registry | the sim listener registers | deterministic sims |

```python
deploy = Deployment(resolve=["static", "mdns"])   # flight-like
deploy = Deployment(resolve=["harness"])          # tests and local dev under metor run
```

- **Resolving is an op (Q11).** `io.connect(peer)` issues `Resolve`, then
  `Connect`.
  - A `static` name resolves at build, so its `Resolve` completes on the next
    turn without a lookup.
  - A fault rule can target `op="resolve"` alone: a missing name, a stale
    address, a slow responder.
  - Every reconnect resolves again.
- **Providers publish to every resolver that can be written.** A listener
  starts its mDNS advertisement as it opens, and ends it as it closes. `static` and `harness` need nothing.
- **The four cases:**
  1. *Real or sim addresses.* A device's own binding (`Tty`, `TcpListen`,
     `GpsReceiver`) is the static case, declared inline in the target.
  2. *Name the target, not the IP.* Use `mdns` or `gossip` in the chain.
  3. *Fully static.* Use `resolve=["static"]`. Nothing runs at run time, and
     every address is checked at build.
  4. *Ephemeral.* `harness` handles anything `metor run` starts. It
     replaces `free_ports` and `METOR_DEPLOY_PORTS`, along with the race
     between picking a port and binding it.

     Processes started by hand use fixed ports through `static`, or use
     `mdns`. A file registry for that case was considered and dropped,
     because it is not worth its locking and staleness rules. If it is ever
     needed, the design is a `flock` held on a never-unlinked lock file per
     entry, and a nonce checked in the handshake.

#### `harness`: the parent binds

`metor run` already reads the whole deployment, so it can bind every listen
device itself before spawning any child:
1. It binds `127.0.0.1:0` per listen device and reads `local_addr()`.
2. It bakes each peer's real address into the dialers' resolved config, as
   `static` does.
3. It passes each child its listeners with systemd's convention:
   `LISTEN_FDS`, `LISTEN_PID`, and `LISTEN_FDNAMES` set to device names.
   The same binary then also runs under real socket activation.

What this gives:
- No registry, no race, and no staleness. Every address lives exactly as
  long as the harness.
- A peer that dials early waits in the listen backlog, so start order stops
  mattering for connect.
- On the child's side, a `TcpListen` device adopts an inherited fd named
  after it, instead of binding.
- Inherited fds are Unix only. Windows would need `WSADuplicateSocket`,
  which is fine while Windows is a dev host.

Prior art: systemd socket activation, `systemfd` (which `cargo watch` uses),
and einhorn.

#### Readiness is not registration

An inherited fd means "accepting connections". A whole target
being up is a separate signal from child to harness: `READY=1` on a
notify socket, as `sd_notify` does. File existence does not mean ready.

#### Not for local tests

mDNS on loopback and in CI containers is unreliable. Multicast is often
missing on `lo` and in container networks, so `mdns` is never the local
fallback.

- **Local devices** (`Tty`, `Dir`, GPIO) are names too, but no network
  resolver serves them. Their binding is the inline declaration. A
  deployment-level overlay could rebind them later, as devicetree overlays
  do. That stays out until needed (Q4b chose per target).

### DNS-SD service type (Q13)

- Use one service type per wire protocol, as RFC 6763 intends: a service
  type names the protocol spoken on the port.
- Every metor link speaks metor-proto, so there is one type. Keep fsw-2's
  `_metor-fsw._tcp`, which the panel already browses.
- TXT records carry `ns`, `device`, `pv` (protocol version), and `role`
  (`publish`, `subscribe`, or `db`).
- A device that speaks another protocol, such as a raw instrument socket,
  is not advertised under this type.
- Per-interface types (`_metor-listen._tcp`) would encode a fact already in
  the TXT records, and the panel would have to browse several types.

### What this gives the sim

`SimListen` and `SimDial` join in memory through `SimOs`. The `sim` resolver
can withhold a name, serve a stale address, or flap a peer, and the fault
layer can partition, delay, or corrupt a link by name. Discovery itself
becomes testable deterministically.

### Cost

- **Slice 4 is reworked.**
  - `Transport` and `Endpoint` move into device kinds.
  - `Publish` and `Subscribe` take `via`.
  - Build's peer rewrite becomes the `static` resolver.
  - The wire format does not change.
- **Async systems need devices too.** Links are async systems, so their
  thread's reactor resolves through its stack. This is the stellarator work
  in §8.
- **The security caveat carries over.** mDNS is unauthenticated and links
  are plaintext. `mdns` stays out of the default chain until an
  authenticated handshake exists.

## 7. Q2: completions without async

### Per-cycle SQ/CQ

For cyclic systems, the natural rule is: **submit during cycle N, reap during
cycle N+k, k ≥ 1.**
- At the cycle boundary, an IO step drains every system's SQ into the backend
  and posts every CQE that has arrived. Nothing completes during a system's
  call.
- This is deterministic by construction. The backend can only move a
  completion to a later cycle, never an earlier one. The simulator picks k
  per op. That choice is the fault-injection knob for latency and timeout.
- No wakers are needed. The coordinator polls every system every cycle
  anyway, so "am I done" is a lookup by token, not a notification.

Three surfaces sit on that one substrate:
1. **State-machine surface** for plain cyclic systems.
   `io.submit(op) -> Token` returns a token, and
   `io.take(token) -> Option<Done>` returns the completion once it is
   posted. This is the sans-IO shape.
2. **Future surface** for sequences. `io.read(..).await` submits on the
   first poll and returns `Pending` until the CQE is posted. It reuses the
   poll-once machinery `SequenceStep` already has.
3. **Async systems.** stellarator gains a reactor that can be swapped
   (`Reactor` behind the ambient handle, or `dyn`). Tests use a
   `SimReactor` with a simulated clock. For determinism, a thread group in
   sim mode runs on the coordinator thread with a public
   `Executor::turn()` called once per cycle, rather than `run` on a thread.

### Is it worth avoiding async?

For cyclic systems, yes. Their contract is a bounded call once per cycle. A
future adds nothing there except an allocation and a hidden state machine.
Sequences are already futures polled per cycle, so they get `.await`
cheaply. The one case where async wins is unbounded request/response chains
on links. Those are already async systems.

### Costs

- **Latency.** A chained transaction takes at least one cycle per step. One
  example is an I2C "write register address, then read". At 100 Hz, a
  three-step device transaction is 30 ms. Mitigations:
  - compound ops, such as one `I2C_RDWR` with two messages, or linked SQEs
    (`IOSQE_IO_LINK`);
  - several IO turns per cycle, interleaved between systems;
  - letting a system spin its own turns, which gives up the fixed latency.
- **Buffers.** Buffers must outlive a cancelled op. With a per-instance pool
  allocated at build, ops name a pool slot, not a pointer. A slot stays
  in-flight until its CQE arrives, even if the system is dropped. Index-based
  ops are also the only form that crosses the wasm and dylib ABIs, and they
  map onto io_uring fixed buffers.

Who picks k: the device model sets nominal latency, and a fault layer's
rules can add to it.

A determinism check belongs in the design from the start. It runs a target
twice on one seed and compares the recorded SQE and CQE streams.

## 8. Q3: backends

A backend is a function from `&[Sqe]` to the CQEs posted at a later turn.
With data-only ops, one op enum serves every backend:

| Backend | Sockets, CAN | Files, tty read/write | Quick ioctl (termios, GPIO) | Bus ioctl (I2C, SPI), `tcdrain`: second pass | Timers |
|---|---|---|---|---|---|
| io_uring | native | native (a tty is armed by poll) | inline | per-bus thread | `Timeout` op or wheel |
| kqueue | try now, else `EVFILT_READ`/`WRITE` and retry | inline, or thread pool | inline | n/a on macOS | wheel |
| Windows | IOCP overlapped (native completion) | IOCP overlapped | n/a | n/a | wheel |
| sim | modeled | modeled | modeled | modeled | sim clock |

io_uring has no generic ioctl op. `IORING_OP_URING_CMD` only reaches drivers
that implement `uring_cmd`: NVMe, ublk, and some socket commands. The tty,
I2C, and SPI drivers do not.

- **Setup termios** (baud, raw mode) runs synchronously when a `Dev<Uart>`
  opens at build. It is never an op.
- **Runtime quick ioctls** (`TCFLSH`, `TIOCMSET`, `TIOCOUTQ`, GPIO line
  set/get) are ops. The driver executes them inline during its turn and
  posts the CQE at once. The system still sees it next cycle.

For the second pass:
- An I2C or SPI bus is serial, so each bus gets one thread, started at build.
  A pool buys nothing.
- A bus thread posts its CQE into the main ring with `IORING_OP_MSG_RING`
  (5.18+). The reactor sees one completion stream.
- Placement is per device in config, with a default per kind. SPI at MHz
  clocks may run inline. The sim ignores placement and models the latency.
- Blocking I2C inline in the cycle is the simpler alternative. It lets a
  stuck bus stall the schedule, and a stuck bus is one of the faults this
  work exists to inject.

- **Readiness emulation.** On submit, try the nonblocking syscall once. On
  `EAGAIN`, register interest. On readiness, retry and post the CQE. The
  poll backend's union-of-interests bookkeeping (`fds`) carries over.
- **Blocking ops on the dev backends** (files on kqueue) go to a fixed worker
  pool that posts to the CQ. The pool is sized at init, which follows the
  allocation rule. They should not go to `blocking`'s unbounded pool.
- **Windows** fits completions better than readiness. `polling` on Windows
  is already AFD readiness layered on IOCP. Going straight to IOCP
  `OVERLAPPED` ops for files and sockets is the compio and TigerBeetle route.
  `IoRing` has no sockets, so it is not worth targeting.
- **stellarator's change.** Replace the two `OpCode` traits with one data
  op, which each backend interprets. Put the executor's reactor and clock
  behind a handle chosen at construction, not `DefaultReactor`. The poll
  backend's `fds` interest union and the uring backend's `Ignored` buffer
  parking carry over unchanged.


## 9. Open questions

None at present.
