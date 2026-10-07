# IO as a queue

> Long draft. The current design is `18-io.md`. It replaces buffer pools with
> buffers allocated in the constructor through `IoInit` and held in `Slot`s.

A system's IO becomes data. A system puts ops on its own submission queue
and takes completions from its own completion queue. The queues are rings,
and an ordinary system, `io`, drains them into a reactor. A reactor holds
optional layers that can fault, delay, or corrupt an op, and one OS at the
bottom. The OS is io_uring in flight, a readiness emulation on
development hosts, or `SimOs`, which runs device models. A system cannot tell
which one it has.

Every resource a system touches is a capability that its target grants by
name: a serial port, a file, a directory, a socket, or another target's
endpoint. A system has no other way to do IO. Swapping a real GPS for a
simulated one changes one line of the target file. The same names serve
fault rules, status, and finding another target's endpoints across a
network.

The research behind this doc, and the options it rejects, are in
`18-io-queues-report.md`.

In scope:
- One data op model in stellarator, a driver and clock chosen at
  construction, and `Executor::turn`.
- The `io` system, which wires each system's queue rings to a reactor.
- `Io`, handles, pending tokens, and buffer pools for cyclic systems and
  sequences.
- Capabilities: kinds, interfaces, backings, sharing, and the config IR.
- The reactor: layers, the fault layer, `Os`, `SimOs`, interface models, and
  routing for mixed targets.
- Links on capabilities, and name resolution with the `static`, `harness`, and
  `sim` resolvers.
- IO from wasm and dylib guests.

Out of scope:
- I2C, SPI, and `tcdrain`. These come in a second pass.
- The `mdns` and `gossip` resolvers.
- Record/replay.
- IOCP on Windows.
- Real capability kinds written by users.
- Rebinding capabilities per deployment.

## Shape

```
 cycle N:  nav, gps, seq ──SQ rings──▶ io (last step) ─ Reactor: layers ─▶ Os ─┬─ io_uring  (flight)
                                                                              ├─ Poll      (dev host)
 cycle N+1: nav, gps, seq ◀──CQ rings── (loop edges)                          └─ SimOs ─ models (sim)
```

Rules:
1. **A system's IO is data.** Ops go in and completions come out, through
   the system's own `Io`. Nothing does IO behind its back.
2. **Completions are never seen during the cycle that submitted the op.**
   An op submitted in cycle N completes in cycle N+k, with k ≥ 1. It never
   completes during the system's call, so no waker is needed.
3. **Every resource is a granted capability.** Runtime IO derives from a
   granted capability too.
4. **Real and simulated swap at `Os`.** Faults are layers above it, so they
   work over either.
5. **Everything is sized at build.** This covers handle tables, buffer
   pools, token tables, and held queues. Nothing allocates in steady state.
6. **Sharing has the OS's semantics and nothing more.**

## Ops and completions

```rust
pub struct Sqe { pub token: Token, pub handle: RawHandle, pub op: Op }
pub struct Cqe { pub token: Token, pub res: Result<Out, IoError> }

pub enum Op {
    Read { buf: BufRef, offset: Option<u64> },
    Write { buf: BufRef, offset: Option<u64> },
    Recv { buf: BufRef },
    Send { buf: BufRef },
    SendTo { buf: BufRef, addr: SockAddr },
    Open { path: PathRef, flags: OpenFlags }, // the handle is a `Dir` capability
    Accept,
    Resolve,                                  // the handle is a `Dial` capability
    Connect { addr: SockAddr },
    Close,
    Fsync,
    Tty(TtyOp),
    Gpio(GpioOp),
    Cancel { target: Token },
}

pub enum Out { Unit, Len(u32), Handle(RawHandle), Addr(SockAddr) }

/// `Op` without its data, for matching.
pub enum OpKind { Read, Write, Recv, Send, SendTo, Open, Accept, Resolve, Connect, Close, Fsync, Tty, Gpio, Cancel }
```

- `BufRef` names a slot in the submitting system's pool. `PathRef` names
  bytes in a pool slot.
- No op holds a pointer, so every op can be logged, compared in a test, and
  sent across the guest ABI.
- `TtyOp` covers the quick ioctls: `Flush`, `ModemBits`, `OutQueue`, and
  `Configure`. `GpioOp` is `Get` and `Set`.

## `Io`: the system's side

A cyclic system or a sequence takes `io: &Io`. Each instance has its own
queue pair: an SQ ring and a CQ ring, which build wires to the `io` system
(see "The `io` system").

The surface has two levels:
- **`Io` is the base API.** It does submission by raw handle and token, and
  buffer allocation.
- **Typed handles, pending tokens, and buffers each hold an `Rc` to their
  instance's queue or pool.** Their methods call `Io`. A system writes
  `uart.read(buf)` and `s.rx.poll()`.

On the wire, every one of them is still an index. `Sqe`, `Cqe`, and the guest
ABI carry `RawHandle`, `Token`, and `BufRef`. The `Rc` exists only on the side
where the system's code runs. In a guest, that is guest memory, so no host
pointer crosses the ABI. Cloning an `Rc` does not allocate.

`Io` takes `&self`, as `Output::write` does. The queue is single-threaded
and has no callbacks, so no call re-enters it. Its interior mutability needs
no borrow flag that could panic.

### Handles

```rust
#[derive(Clone)]
pub struct Handle<K> { raw: RawHandle, queue: Rc<Queue>, _k: PhantomData<K> }
// RawHandle { slot: u16, gen: u16 }

impl<K> Handle<K> {
    pub fn close(&self) -> Pending<Close>;
}

impl Handle<Uart> {
    pub fn read(&self, buf: Buf) -> Pending<Read>;
    pub fn write(&self, buf: Buf) -> Pending<Write>;
    pub fn configure(&self, cfg: Termios) -> Pending<Tty>;
    pub fn flush(&self, queue: TtyQueue) -> Pending<Tty>;
}
impl Handle<Dir> { pub fn open(&self, path: &str, opts: OpenFlags) -> Pending<Open>; }
impl Handle<Listen> { pub fn accept(&self) -> Pending<Accept>; }
impl Handle<Dial> { pub fn connect(&self) -> Pending<Connect>; } // Resolve, then Connect
impl Handle<Can> { pub fn recv(&self, buf: Buf) -> Pending<Recv>; pub fn send(&self, buf: Buf) -> Pending<Send>; }
```

- **Which methods a handle has depends on its interface.** A `Handle<Uart>`
  has no `accept`.
- **`raw` indexes the instance's handle table,** which is sized at build.
  - An entry for a granted capability points into the target's grant table.
  - An entry for a runtime handle belongs to the system.
- **`gen` makes a closed handle stale.** An op on a stale handle completes
  with `IoError::Stale`, and it never reaches whatever reused the slot.
- **A system reaches only what its table holds.** A handle carries its own
  queue, so a handle moved into another system still submits to its
  owner's queue. Only the owner sees the completion.

**Granted.** A `Cap<K>` parameter names a capability the way `Input<T>` names
a port, and it derefs to `Handle<K>`.

**Derived.** `dir.open(..)`, `listen.accept()`, and `dial.connect()` complete
with a new `Handle<K>` derived from the one they were called on.

### Pending ops

```rust
#[must_use]
pub struct Pending<O: Op> { token: Token, queue: Rc<Queue>, _o: PhantomData<O> } // not Clone

impl<O: Op> Pending<O> {
    /// The completion, once posted, or the token back.
    pub fn try_take(self) -> Result<O::Done, Self>;
    pub fn cancel(&self);
}

/// For a pending op held in state.
pub trait PendingSlot<O: Op> {
    /// Takes the completion out of the slot once it has been posted.
    fn poll(&mut self) -> Option<O::Done>;
}
impl<O: Op> PendingSlot<O> for Option<Pending<O>> { .. }
```

- **Submitting never fails at once.** A full queue, a stale handle, or a
  refused op completes with an error next cycle. A system has one error path,
  and a fault layer can produce every error on it.
- **Dropping a `Pending` does not cancel its op.** The completion is
  discarded when it lands, and its buffer returns to the pool then.
  `cancel` submits `Cancel`, which completes with `Canceled` or with the
  real result.
- **In a sequence, `Pending` is a future** that `SequenceStep` polls once
  per cycle: `let (res, buf) = uart.read(buf).await;`.

### Buffers

- **One pool per instance,** sized at build: `bufs × buf_len`.
  `#[system(io(bufs = 4, buf_len = 256))]` sets the default, and an
  instance's config can override it. Under io_uring, the pool is registered
  as fixed buffers.
- **Taking a slot.** `io.buf() -> Option<Buf>` takes a free slot.
  `io.copy_buf(&[u8])` takes one and fills it.
- **Bytes.** A `Buf` derefs to its bytes, and to `[u8]` mutably.
- **Return on drop.** A `Buf` holds an `Rc` to its pool and returns its slot
  on drop.
- **Ops take buffers by value.** An op takes its `Buf`, and the completion
  hands it back: `ReadDone = (Result<usize, IoError>, Buf)`.

### Example

```rust
#[derive(Default)]
struct GpsState {
    rx: Option<Pending<Read>>,
    tx: Option<Pending<Write>>,
    configured: bool,
    nmea: Nmea,
}

/// Sets the receiver to 10 Hz, then publishes each fix it reports.
#[system(io(bufs = 4, buf_len = 256))]
fn gps(#[state] s: &mut GpsState, io: &Io, uart: Cap<Uart>, fix: &mut Output<Gps>, log: &mut Log) {
    if let Some((res, _)) = s.tx.poll() {
        match res {
            Ok(_) => s.configured = true,
            Err(e) => log.fault("gps_config", e),
        }
    }
    if let Some((res, buf)) = s.rx.poll() {
        match res {
            Ok(n) => s.nmea.push(&buf[..n]),
            Err(e) => log.fault("gps_read", e),
        }
    }
    while let Some(f) = s.nmea.next() {
        let _ = fix.write(&Gps::from(f));
    }
    if !s.configured && s.tx.is_none() && let Some(cmd) = io.copy_buf(PMTK_RATE_10HZ) {
        s.tx = Some(uart.write(cmd));
    }
    if s.rx.is_none() && let Some(buf) = io.buf() {
        s.rx = Some(uart.read(buf));
    }
}
```

## Capabilities

A **capability kind** is registered in the `SystemTable`, beside system
types. A **capability** is an instance that a target grants under a name.
Passing it to a system is the grant.

```python
gps1 = fsw.cap("gps1", Tty("/dev/ttyTHS0", baud=9600))
# or
gps1 = fsw.cap("gps1", GpsReceiver(baud=9600, truth=plant.gps_truth))

gps = fsw.add("gps", GpsDriver(uart=gps1))
```

```rust
pub struct CapKind {
    pub name: &'static str,
    pub provides: Interface, // Uart, Can, GpioLine, Udp, Listen, Dial, Dir
    pub params: RecordSchema,
    pub inputs: fn(&DefCx<'_>) -> Result<Vec<PortDef>, DefError>,
    pub bind: fn(Params<'_>, Vec<InputBinding>) -> Result<Backing, ParamError>,
}

pub enum Backing {
    /// Data the real `Os` opens at build: a path and its setup, an address,
    /// or an inherited fd.
    Real(RealOpen),
    /// Another target's capability, resolved at connect.
    Peer(CapName),
    /// A model that runs in `SimOs`.
    Sim(Box<dyn Model>),
}
```

- **Built-in real kinds:** `tty`, `can`, `gpio_line`, `udp`, `file`, `dir`,
  `tcp_listen`, and `tcp_connect`. A log directory or a telemetry socket is
  as much a capability as a serial port.
- **Sim kinds come from packs:**
  `table.register_cap::<GpsReceiver>("gps_receiver", GpsReceiver::new)`.
- **A `Peer` is not a kind anyone picks.** A `via=` that names another
  target's capability binds to one.
- **Interface checks.** Build checks that a `Cap<Uart>` parameter is granted
  a capability that provides `Uart`, the way it checks a port's record. The
  generated Python types the parameter `Cap[Uart]`.
- **The kind decides the side.** A real kind opens through the real backend,
  and a sim kind runs in `SimOs`. Build knows every capability's side.
- **One name everywhere.** Config, fault rules, `status`, and the panel all
  use `gps1`. Across targets the name is `{namespace}.{cap}`.
- **Setup happens at open.** Build opens a granted capability: termios, CAN
  filters, a GPIO line request, a bind. A failure fails the build, as a
  taken link address does today.
- **IR.** `CoordinatorConfig` gains `caps: Vec<CapConfig { id, ty,
  params, inputs }>`. A `Cap<K>` parameter maps to a capability id, the way an
  input maps to a producer.

### What makes it a capability system

- **Unforgeable.** A handle is an index into its owner's table, checked by
  generation, and it submits only to its owner's queue. A system cannot
  name a resource it was not granted.
- **Derivation.** Runtime handles come only from methods on a held
  capability: `open` on a `Dir`, `accept` on a `Listen`, `connect` on a
  `Dial`. No op takes a bare path or address.
- **Revocation.** A closed handle is stale. Its generation makes every later
  op on it fail with `IoError::Stale`.
- **No ambient authority, where it is enforced.** A wasm guest has no
  imports, so its capabilities are its only IO. A host or dylib system could
  still call `std::fs`. There the rule is a convention, as with `cap-std`,
  and review keeps to it.

The prior art is seL4's per-thread capability space, Capsicum's
`openat`-style derivation, Fuchsia's handles, and WASI preopens.

### Sharing and lifetimes

- Several systems may hold one capability, on any threads. Sharing has the OS's
  semantics:
  - two readers of a stream split its bytes;
  - two writers may interleave.

  Avoiding that is the author's job.
- The target owns a granted capability for its whole life.
  - `close()` on its handle detaches only the caller.
  - A channel `Reset` leaves it open.
  - Runtime handles close on `Reset`, on drop, and on a fault.
- Under io_uring, a granted capability is one process fd. It is registered in
  the fixed file table of each ring that uses it.

## The `io` system

The IO turn is an ordinary system, and build adds one per target, as it adds
`status` outputs.

- **Each `Io` is a pair of rings.** A system's `Io` writes `Sqe` records to an
  SQ ring and reads `Cqe` records from a CQ ring.
- **`io` reads every SQ and writes every CQ** through `DynInputs` and
  `DynOutputs`. It reads the SQs in step order, so submission order is
  deterministic.
- **`io` is the last step of the cycle.** A system reads its CQ through a
  loop edge, so it sees the completions `io` posted at the end of the
  previous cycle. That is rule 2, with no new mechanism.
  - Ops reach the OS in the same cycle they were submitted. The kernel works
    on them in the gap before the next cycle.
- **Its state is the coordinator thread's `Reactor`.** `io_commands` is an
  ordinary input, and `io_status` an ordinary output. Both appear on the
  panel like any other port. `io` has its own `status` and `log`.
- **Records are fixed-size frames.** An `Sqe` carries indices: a `BufRef` into
  the pool, never bytes. A ring's depth is the instance's `max_ops`, so a CQ
  cannot fill.
  - If an SQ write fails, the `Io` façade completes the op locally with
    `QueueFull` on the next poll. Submitting still never fails at once.
- **Pools are shared memory** between the system and `io`, allocated at
  build. Under io_uring, `io` registers them as fixed buffers. For a wasm
  guest, the adapter copies submitted and completed slots, as it copies
  rings.
- **The coordinator loop is unchanged.** `io` is a `Step` like any other.

Async systems do not go through `io`. Each thread group's executor holds a
`Reactor` of its own, and stellarator ops submit to it directly.

Layers stay inside the reactor and are not systems of their own. A chain of
layer systems would need a ring per direction per layer. Completions would
also flow against step order, so every layer would add a cycle on the way
up.

## The reactor

`Reactor` replaces stellarator's `Reactor` trait. It is the per-thread object
that takes ops through the layers to the OS and brings completions back.

```rust
pub struct Reactor {
    layers: Vec<Box<dyn Layer>>,
    os: Box<dyn Os>,
    held: Held,     // deferred entries, ordered by (release cycle, seq)
    tokens: Tokens, // per token: owner, capability, and the depth that answered it
}

pub trait Os {
    /// Takes an op. A full queue becomes an immediate `Cqe`.
    fn submit(&mut self, sqe: Sqe, done: &mut Fifo<Cqe>);
    /// Collects completions, and advances sim models to `now`.
    fn turn(&mut self, now: Timestamp, done: &mut Fifo<Cqe>);
}
```

One turn:
1. Release the held entries that are due, in `(cycle, seq)` order.
2. Take the new ops. Run each one down the layers. The first verdict other
   than `Pass` stops it there.
3. Call `os.turn(now)`.
4. Run each completion up through the layers above the one that answered
   it.
5. Post the completions.

A `Cancel` of a held or swallowed op is answered by the reactor.

`Held` and `Tokens` are bounded by the sum of the instances' `max_ops`, so
they cannot overflow.

### Layers

```rust
pub trait Layer {
    fn submit(&mut self, cx: &mut Cx<'_>, sqe: &Sqe, bytes: Option<&mut [u8]>) -> Down { Down::Pass }
    fn complete(&mut self, cx: &mut Cx<'_>, cqe: &mut Cqe, bytes: Option<&mut [u8]>) -> Up { Up::Pass }
    fn command(&mut self, cmd: &LayerCommand) -> Result<(), CommandError> { Err(CommandError::Unsupported) }
}

pub enum Down { Pass, Answer(Result<Out, IoError>), Defer(u32) }
pub enum Up { Pass, Defer(u32), Swallow }

pub struct Cx<'a> {
    pub now: Timestamp,
    pub cycle: u64,
    pub system: SystemIx,
    pub cap: CapIx,
    pub op: OpKind,
    pub rng: &'a mut Rng, // seeded from the target seed and the layer's name
}
```

- **A layer only decides.** It returns a verdict and the reactor applies it.
  It may change `cqe.res` and the op's bytes. It never creates ops.
- **Layers stack in the order the target adds them,** top first. A target
  may carry layers in flight. Whether it is allowed to is a config policy,
  for example `Target(io_layers=False)`.

### Faults

`Faults` is a layer over a rule table:

```python
fsw.io.layer(Faults("transport", rules=[
    Rule("gps_bitflip", cap=gps1, op="read", act=Corrupt(bits=1), p=0.01),
    Rule("gps_dropout", cap=gps1, op="read", act=Hang(), enabled=False),
]))
```

- **Match** on system, capability, op kind, time window, every Nth op, or a
  seeded probability. Build resolves names to indices.
- **Act** maps to a verdict:

  | Act | Verdict |
  |---|---|
  | `Fail(errno)`, `Refuse` | `Answer(Err)` |
  | `Delay { cycles }` | `Defer` |
  | `Short { max }`, `Corrupt { bits }` | applied in `complete`, or to a write's bytes in `submit` |
  | `Hang` | `Swallow` |

- **`io_commands` toggles rules by name.** A scenario or the panel can
  start a fault mid-run. No rule is allocated at run time.
- **Peripheral faults belong to the model.** An outage, a cold start, or
  a stuck register is a param or command of the model. The fault layer
  covers transport faults only.

### Threads

- **Each executing thread has one reactor:**
  - the coordinator thread's, inside `io`;
  - one per thread group, inside its executor.

  Layers are built per reactor from the same config. A rule acts in the
  reactor of the thread that runs its system.
- **A target has one `SimOs`,** owned by the coordinator thread, inside
  `io`'s reactor. Models and the rings they read stay single-threaded.
- **`SimProxy` is how a thread group reaches it.** It is two rings and
  nothing else, wrapped so that it implements `Os` at the bottom of the
  group's reactor:

  ```rust
  pub struct SimProxy {
      requests: Writer<NoWake>, // Sqe frames to SimOs
      replies: View<GroupWake>, // Cqe frames from SimOs
  }
  ```

  - `submit` writes an `Sqe` to `requests`. If the ring is full, the op
    completes with `QueueFull`.
  - `turn` drains `replies` into the reactor.
  - The rings are the same `metor-fsw-3-ring` type, with the same `Sqe` and
    `Cqe` frames, as a cyclic system's SQ and CQ. To `io`, a thread group is
    one more queue pair.
  - `GroupWake` is a `WakeSource` that calls the group executor's external
    waker, so a parked group wakes when a completion lands.
  - A proxied op passes its buffer as a pointer and a length. The in-flight op
    owns that memory until it completes, the same contract io_uring has with
    the kernel.
- **Both kinds of run use `SimProxy`.** Only the place its rings are drained
  changes:
  - In a threaded run, the group runs on its own thread, and the rings cross
    threads. The run is not deterministic.
  - In a deterministic run, the `Thread` adapter turns the group's executor in
    step order, through `Executor::turn`. The rings fill and drain in a fixed
    order on one thread.
- **A thread group stops before `io` drops.** Its reactor cancels its
  proxied ops and waits for each cancellation to complete before it frees
  any buffer. If `io` is already gone, the parked buffers leak rather than
  free memory that `SimOs` might still touch.
- **A mixed target ends in `Routed: Os`.** It sends each capability's ops to
  the real backend or to `SimOs`, by the side of the capability's kind. A mixed run
  is not deterministic.

## `SimOs` and models

A model is written against its interface. `SimOs` holds one adapter per
interface that turns ops into calls:

```rust
pub trait UartModel {
    /// `None`: the FIFO is empty, and the read waits for a later tick.
    fn rx(&mut self, now: Timestamp, buf: &mut [u8]) -> Option<usize>;
    fn tx(&mut self, now: Timestamp, bytes: &[u8]) -> Result<usize, IoError>;
    fn configure(&mut self, now: Timestamp, cfg: &Termios) -> Result<(), IoError>;
    fn tick(&mut self, now: Timestamp);
}
// CanModel, GpioModel, UdpModel, ListenModel, DialModel, DirModel
```

- A kind provides `Uart` by implementing `UartModel`.
- The adapter owns readiness and latency. A model reports nominal latency,
  and fault rules add to it.
- Several handles reach one model, in submission order.
- `ListenModel` and `DialModel` join through in-memory pipes. The `sim`
  resolver is part of `SimOs`, so it can withhold a name, return a stale
  address, or flap a peer.
- Unit tests use the same traits, through a `TestIo` harness:

```rust
let mut t = TestIo::new()
    .cap("uart", ScriptedUart::silent())
    .layer(Faults::once(OpKind::Write, Act::Fail(Errno::EIO)));
let mut gps = t.system(gps(), GpsState::default);
t.step(&mut gps, 3);
assert_eq!(t.cap::<ScriptedUart>("uart").written(), [PMTK_RATE_10HZ]);
```

## Links and names

`Publish`, `Subscribe`, and `Db` take a capability through `via=` instead of a
`Transport`:

```python
down = plant.cap("down", TcpListen("0.0.0.0:2240"))
plant.add("pub", Publish([sim], via=down))
fsw.add("plant", Subscribe([sim], via=down))   # a Peer of plant's `down`
```

`listen="..."` and `connect="..."` stay as shorthand for a capability named after
the link. `Transport` and `Endpoint` are removed. The wire format does not
change.

**The deployment decides how names resolve.** It lists resolvers in order:

| Resolver | When | How |
|---|---|---|
| `static` (default) | build | `metor build` writes `name → addr` from `hosts` and fixed ports. |
| `harness` | spawn | `metor run` binds each listen capability on `127.0.0.1:0`. It passes the listeners with `LISTEN_FDS`, `LISTEN_PID`, and `LISTEN_FDNAMES`, and writes the addresses into the dialers' config. |
| `sim` | run | the `SimOs` registry |

```python
Deployment(resolve=["static"])    # flight
Deployment(resolve=["harness"])   # tests and local dev under metor run
```

- **`Resolve` is its own op,** so a fault can target name resolution apart
  from dialing.
  - A `static` name resolves at build, and its `Resolve` completes without
    a lookup.
  - Every reconnect resolves again.
- **`harness` replaces `free_ports` and `METOR_DEPLOY_PORTS`.** The harness
  holds every listener before any child starts. That ends the race between
  picking a port and binding it, and a dialer that comes early waits in the
  backlog.
  - A `TcpListen` capability adopts the inherited fd named after it.
  - Readiness is a separate signal from child to harness: `READY=1` on a
    notify socket.
- **Processes started by hand** use fixed ports through `static`.

## stellarator

- **One data op.** The uring and poll `OpCode` traits are replaced by `Op`.
  Each backend interprets it.
  - The public API does not change: `TcpStream`, `TcpListener`,
    `UdpSocket`, `File`, serial, `AsyncRead`/`AsyncWrite`, and `BufResult`.
    db, the panel, metor-proto/stellar, fsw-2, and fsw-3 are untouched.
  - Host code keeps `IoBuf` pointers. Pool slots are turned into pointers
    at submit.
- **A reactor chosen at construction.** `EXEC` holds the thread's `Reactor`,
  not a `DefaultReactor` fixed by `cfg`. The clock is a parameter too. maitake's
  `Clock::new` takes a plain `fn`, so a sim clock reads a thread-local.
- **`Executor::turn`** runs one pass without blocking: timer, IO, scheduler.
  When nothing is runnable, a sim jumps its clock to the next deadline.
- **Async systems take `Cap<K>`.** It resolves to a handle in their thread's
  reactor. stellarator types adopt handles, for example
  `TcpListener::adopt(cap)`. From there the async API is stellarator's own,
  as today.
- **Poll backend rewrite.** The work moves into the driver:
  1. try the call;
  2. on `EAGAIN`, park the op and register interest;
  3. on readiness, retry and post the result.

  Files and blocking calls go to a fixed worker pool, not `blocking`. The
  interest-union bookkeeping carries over.
- **Cancellation safety.** A dropped op's buffer stays parked until its
  completion lands. This is proven once in the driver, under Miri and Loom.

## Backends

| | Sockets, CAN | Files, tty read/write | Quick ioctl (termios, GPIO) | Timers |
|---|---|---|---|---|
| io_uring | native | native | inline in the turn | wheel |
| poll (macOS, Windows) | try, then readiness | inline or worker pool | inline | wheel |
| `SimOs` | models | models | models | sim clock |

- io_uring has no generic ioctl op. Quick ioctls run inline during the turn,
  and their completion is posted at once. The system still sees it next
  cycle.
- Only Linux flies. The poll backend favors simplicity over throughput.
- The io_uring backend starts no threads in this slice.

## Guests

- A wasm or dylib system takes `io: &Io` and `Cap<K>` like any other
  system. Ops are
  data, and handles are indices.
- A guest's pool lives in guest memory, and `Buf` points at that pool.
- Around each step, the adapter copies:
  - the slots of submitted ops out;
  - the slots of completed ops in.

  This is the same copy-both-ways rule the rings follow.
- The ABI gains the queue pair per instance, alongside the rings.

## Determinism

- Systems submit in step order, and `io` runs last in every cycle. Each
  layer and model draws from its own RNG, seeded from the target seed and
  its name. Adding a rule to one layer does not shift another layer's
  draws.
- A deterministic run has `SimOs` alone at the bottom, and every thread
  group turned on the coordinator thread.
- **A determinism test** runs a target twice on one seed and compares the
  op and completion streams.

## Decisions

1. **Explicit `Io` per instance**, not an ambient handle.
2. **Completions are never seen during the cycle that submitted the op.**
   Cyclic systems poll tokens and use no wakers. Sequences await the same
   tokens.
3. **Latency-critical IO runs in async systems** on their own threads.
   Cyclic systems get no IO turns within a cycle.
4. **Handles, tokens, and buffers are indices on the wire.** Their typed
   forms hold an `Rc` to their instance's queue or pool, so the methods live
   on them: `uart.read(buf)`, `s.rx.poll()`. `Io` stays the base API. A
   `Buf` returns to its pool on drop.
5. **IO is capability-based.** Targets grant named capabilities, systems
   hold them, and runtime handles derive from them. The kind decides real
   or sim. Interfaces are checked at build. Grants are per target.
6. **Sharing has OS semantics.** No reader or writer rules, and no write
   queues.
7. **Layers only decide.** They never create ops.
8. **One `SimOs` per target.** Reactors are per thread.
9. **Models are written per interface.** Peripheral faults live in models, and
   transport faults live in layers.
10. **Discovery is name resolution.** It is a resolver chain per deployment,
    `["static"]` by default. `Resolve` is an op.
11. **One DNS-SD service type per wire protocol,** `_metor-fsw._tcp`, when
    `mdns` lands.
12. **No file registry.** Processes started by hand use `static`.
13. **Layers are allowed in flight,** subject to config policy.
14. **The IO turn is a system.** `io` runs last and drains SQ rings into
    the reactor. Systems read their CQ rings through loop edges.

## Phases

1. **stellarator.** `Op`, the `Reactor` and clock chosen at construction,
   `Executor::turn`, and the poll backend rewrite. Every existing test
   passes on uring and on poll, and no downstream crate changes.
2. **Core `Io`.** Handles, tokens, pools, `Cap<K>`, the grant table, the
   built-in real kinds, the config IR, and the `io` system with no layers. The GPS
   driver runs against a real pty.
3. **Layers.** `Layer`, `Faults`, `io_commands`, and `io_status`.
4. **`SimOs`.**
   - The interface adapters, `TestIo`, and the GPS model.
   - `SimProxy`, `Routed`, and the determinism test.
5. **Links.**
   - `via=`, `Peer`, and the `static`, `harness`, and `sim` resolvers.
   - Async systems take `Cap<K>`.
   - `METOR_DEPLOY_PORTS` is removed.
6. **Guests.** The queue pair across the ABI, and the pool copy.

## Open decisions

- **The group clock in a threaded sim.** A thread group's `sleep` should
  follow sim time. maitake's `Clock::new` takes a plain `fn`, so the group's
  clock needs a per-thread hook that reads the coordinator's published time.
  It also has to work when two coordinators share a process.
- **Threaded sims in v1.** The deterministic mode is enough for tests.
  Threaded runs serve interactive panel sessions and HIL rigs. Supporting
  them adds cross-thread rings and the clock hook. `SimProxy` has the same
  shape either way.

- **The shape of `io_status`.** It could hold per-capability counters (ops,
  errors, in flight) and free pool slots per instance. It could be one
  frame per capability or one message per cycle.
- **`Out` on the guest ABI.** A fixed layout, or postcard like other
  messages.
- **Where `PathRef` bytes live.** A pool slot costs a `Buf` for every
  `open`. A small per-instance path arena is the alternative.
- **Real kinds over UDS.** Unix domain sockets would keep `harness`
  addresses off TCP entirely. This waits for a link that needs it.

## Future

- **Writing straight into io_uring's rings.** On a real target with no
  layers, `io` could give systems kernel SQ slots and skip the translation
  from `Sqe`. This saves copying one 64-byte SQE and one 16-byte CQE per op,
  which is noise next to the syscall each cycle pays anyway. It would also
  add a second path that bypasses layers, while `SimOs` and guests keep our
  own rings. Hold it until profiling says the copy matters. To keep it
  possible:
  - keep `Sqe` within 64 bytes;
  - map `Token` one-to-one onto `user_data`;
  - keep capabilities and pools registered as fixed files and fixed buffers.
