# IO, second implementation: implementation plan

This plan implements `19-io2.md`. Read that first, and `18-io.md` for the
surface. `src/io2/` is written from scratch beside `src/io/`. It is
declared in `lib.rs` as `mod io2;`, with `#![allow(dead_code)]` at its
root until phase 5, and nothing outside it imports it until then.

Each phase ends with `cargo test -p metor-fsw-3`, clippy, and fmt clean,
and is one commit, or a few. Backend tests also run in the Linux VM (see
the memory note on the Linux test VM).

## Layout and budget

```
src/io2/
  mod.rs        re-exports, MAX_OPS, IO_SQ, IO_CQ, Grant, GrantDef, End     ~80
  config.rs     ResourceConfig, Backing, RealKind, SimKind, ... (moved)      ~130
  wire.rs       Span, Token, RawHandle, Opcode, Sqe, Cqe, Op, Record impls   ~350
  error.rs      IoStatus, IoError, Errno                                      ~200
  arena.rs      Arena, Buf, IoInit                                            ~250
  queue.rs      Io, Pending, IoOp markers, Slot                               ~300
  cap.rs        Interface, the markers, Handle<K>, Cap<K>                     ~170
  param.rs      Cap and DeviceSide as fn-system params                        ~90
  status.rs     ResourceStatus, Counters                                      ~60
  device.rs     DeviceSide, Model, UartModel, GpioModel, Reply, Held          ~300
  test.rs       test::cap, test::device, ScriptedUart                         ~280
  build.rs      IoBuild (was IoShared): members, arenas, quiesce entry points ~150
  host/         cfg(all(feature = "host", unix))
    mod.rs      Target, Class, OsOp, OsDone, Out, the Os trait, host()       ~90
    dispatch.rs Dispatch                                                      ~400
    steps.rs    io_submit and io_complete                                     ~300
    open.rs     open(RealKind), termios, CAN, GPIO, sockaddr                  ~300
    poll.rs     PollOs                                                        ~330
    uring.rs    UringOs                                                       ~330
```

That is about 3,600 lines. `config.rs` and `status.rs` are moved, not
written. Tests go in `src/io2/tests/` (one file per source file) and
`tests/`.

## Phase 1: wire types and the system side

### 1.1 `wire.rs` and `error.rs`

- `Span { off: U32, len: U32 }`, with `Span::end() -> Option<u32>`.
- `Token(U32)`: `new(slot: u16, generation: u16)`, `slot()`,
  `generation()`.
- `RawHandle(U32)`: `index | generation << 16`, with the same accessors.
- `#[repr(u8)] enum Opcode { Read, Write, Recv, Send, SendTo, Open,
  Close, Fsync, Accept, TtyConfigure, TtyFlush, GpioGet, GpioSet,
  Cancel }`, with `TryFrom<u8>`.
- `Sqe`, which is `FromBytes`, `IntoBytes`, and `Unaligned`, 56 bytes:

  ```rust
  #[repr(C)]
  pub struct Sqe {
      pub token: Token,
      pub handle: RawHandle,
      pub opcode: u8,
      pub aux: u8,        // tty flush queue, gpio level
      _p: [u8; 2],
      pub buf: Span,
      pub offset: U64,    // u64::MAX: the current position
      pub arg: [u8; 28],  // TtyConfig, OpenFlags, or RawAddr
  }
  ```

  It has one constructor per opcode (`Sqe::read(token, handle, span,
  offset)` and so on). For `Cancel`, `token` is the target's token.
- `Cqe { token: Token, status: IoStatus, value: U32 }`: 12 bytes,
  `FromBytes`.
- `Op`: a plain Rust enum, decoded by `Sqe::op(&self) -> Result<Op,
  IoError>`. Its variants are `Read { offset: Option<u64> }`, `Write
  { .. }`, `Recv`, `Send`, `SendTo(SocketAddr)`, `Open(OpenFlags)`,
  `Close`, `Fsync`, `Accept`, `TtyConfigure(TtyConfig)`,
  `TtyFlush(TtyQueue)`, `GpioGet`, `GpioSet(bool)`, and `Cancel`. It
  also has `OpKind`, plus `fills()` (the op writes into its buffer) and
  `drains()` (the op reads from its buffer, including `Open`'s path).
  An unknown opcode, or an `arg` that does not decode, is `Unsupported`
  or `BadAddr`.
- `TtyConfig`, `OpenFlags`, and `RawAddr` are written fresh, as
  `FromBytes` structs that fit in `arg`.
- `Record` for `Sqe` (`DEPTH = 2 * MAX_OPS`, since cancels take no slot)
  and `Cqe` (`DEPTH = MAX_OPS`). They use `read_from_prefix`, with no
  `TryFromBytes`.
- `error.rs`: `IoStatus`, `IoError`, and `Errno`, with today's codes and
  names (Python and models use them). `IoError` drops nothing except
  what nothing can make.

Tests: round-trip each constructor through `op()`, unknown opcode, short
record, trailing bytes, and size asserts.

### 1.2 `arena.rs`

```rust
pub(crate) struct Arena {
    mem: UnsafeCell<Mem>,
    top: Cell<u32>,
    epoch: Cell<u32>,
    frozen: Cell<bool>,
}
enum Mem {
    Growing(Vec<u8>),
    Fixed { base: NonNull<u8>, len: usize, _keep: Keep },
}
pub(crate) enum Keep { Boxed(Box<[u8]>), Lease(Lease), Unowned }
```

- `Arena::new()` starts as `Growing`. `Arena::over(base, len, keep)` is
  `Fixed` and frozen, for a dylib region the host leases or a wasm
  mirror.
- `alloc(cap) -> Option<Span>` bumps `top`. While `Growing`, it resizes
  the `Vec`. Once `Fixed`, it fails past `len`.
- `freeze()` turns `Growing` into `Fixed { Boxed }`. `rewind()` sets
  `top = 0`, adds one to `epoch`, and zeroes nothing.
- `span(&self, Span) -> Option<NonNull<[u8]>>` is the one bounds check,
  used by `Dispatch` and `DeviceSide` on foreign spans.
  `region() -> (NonNull<u8>, usize)` exports it, and
  `owner() -> Option<RawOwner>` comes from an `Arc` the way `Mem` does
  today. The `Boxed` arm becomes `Arc<Box<[u8]>>`, so a lease can keep it
  alive.
- `Lease` moves here from `region.rs`, rewritten in about 20 lines.
- `Buf { arena: Option<Rc<Arena>>, span: Span, len: u32, epoch: u32 }`
  derefs to `span[..len]` only if the arena is frozen and the epochs
  match. Otherwise it is empty. Its methods are `capacity`, `fill`,
  `clear`, and `set_len`, and its `Drop` does nothing.
  - **Safety:** spans never overlap. While an op is in flight, its `Buf`
    sits inside the `Pending`. The arena never moves once frozen.
- `IoInit<'a> { arena: Option<&'a Rc<Arena>>, short: Cell<usize> }`:
  - `new` rewinds a frozen arena (a remake);
  - `alloc` and `alloc_from` record any shortfall;
  - `finish() -> Result<(), ParamError>` turns a shortfall into
    `IoMemory`;
  - `Drop` freezes the arena.

  It drops the `orphaned` and `making` cells.

Tests: allocate and freeze, a remake within the capacity, a remake over it
(`IoMemory`), a stale-epoch `Buf` derefs empty, a `Buf` derefs empty while
the arena is made, `span` out of bounds, and an `alloc_from` round trip.
Kani: `span` never returns memory outside the arena.

### 1.3 `queue.rs`

```rust
pub struct Io(Rc<Queue>);
struct Queue {
    sq: Output<Sqe>,
    cq: RefCell<Input<Cqe>>,
    ops: [Cell<OpState>; MAX_OPS],
    generation: Cell<u16>,
    arena: Rc<Arena>,
    next_grant: Cell<u16>,
}
#[derive(Clone, Copy)]
enum OpState { Free, Waiting { opens: bool }, Orphan { opens: bool }, Done(IoStatus, u32) }
```

- `submit(sqe, buf, opens) -> Pending<O>`:
  - claims the first `Free` slot and stamps the token;
  - a full table or a full ring gives `Local(QueueFull)`, with the buffer
    handed back;
  - a missing buffer for an op that needs one gives `Local(BadBuf)`.
- `pump()` drains the CQ (`try_borrow_mut`; a failure means no new
  completions). A matching generation goes `Waiting` to `Done`.
  `Orphan { opens: true }` with an ok handle writes `Sqe::close(token,
  handle)` and becomes `Orphan { opens: false }`. Any other `Orphan`
  goes to `Free`.
- `Pending<O: IoOp> { io: Io, token: Token, state: Queued | Local(IoStatus) | Taken, buf: Option<Buf> }`:
  - `poll`, `try_take`, `cancel` (writes `Sqe::cancel(token)`), and
    `Future`;
  - `Drop` turns `Queued` into `Orphan`, or frees the slot if it is
    `Done`;
  - a completion sets `buf.len` for an op that fills.
- The `IoOp` marker types stay (`Read`, `Write`, `Recv`, `Send`, `Open`,
  `Accept`, `Close`, `Fsync`, `Tty`, `Gpio`, `Raw`), each with
  `const OPENS: bool` and `finish(io, status, value, buf)`. `BufOp` and
  `Slot<O>` keep today's API.
- `Io::in_flight()` and `Io::submit_raw(sqe, buf) -> Pending<Raw>`.

### 1.4 `cap.rs`

`Interface`; the markers `Uart`, `Can`, `Udp`, `Dir`, `File`, `Listen`,
`GpioLine`, and `Stream` (no `Dial`); `Handle<K>` with today's methods by
interface; and `Cap<K>`, which derefs to its handle. `Handle<Dir>::open`
takes the path in a `Buf`.

Tests for 1.3 and 1.4 use a loopback rig in `tests/queue.rs`. The rig
reads the SQ ring and answers on the CQ ring by hand. Cases:

- submit, then complete;
- `QueueFull` from the table, and from the ring;
- a dropped `Pending` frees its slot when its completion lands;
- an orphaned accept closes its handle under the same token;
- cancel takes no slot;
- a stale generation is ignored;
- a read sets the buffer's length;
- `Slot` idle, busy, idle;
- no allocation in steady state, checked with a counting allocator in the
  test module.

**Commit:** `feat(fsw3): io2 wire types, arena, and system queue`.

## Phase 2: `Dispatch`

### 2.1 Types (`host/mod.rs`)

```rust
pub enum Class { Stream, Datagram, Listener, File, Dir, Gpio }
pub enum Target { Fd(RawFd, Class), Sim(u16) } // Sim: the resource's port index
pub struct OpId(u32);                       // index: u16 | generation << 16
pub struct OsOp { pub id: OpId, pub target: Target, pub op: Op, pub buf: Option<NonNull<[u8]>>, pub arena: u16 }
pub enum Out { Len(u32), Opened(RawFd, Class) }
pub struct OsDone { pub id: OpId, pub result: Result<Out, IoError> }
pub trait Os {
    fn push(&mut self, op: OsOp);
    fn cancel(&mut self, id: OpId, target: Target);
    fn submit(&mut self);
    fn reap(&mut self, done: &mut Vec<OsDone>);
    fn close(&mut self, fd: RawFd);
}
```

`OpId`'s generation is the system's token generation, so `raise` rebuilds
the token exactly: `slot = index % MAX_OPS`, `member = index / MAX_OPS`.
`Dispatch::new` refuses more than `65536 / MAX_OPS` members.

### 2.2 `dispatch.rs`

```rust
pub(crate) struct Dispatch {
    members: Box<[Member]>,
    ops: Box<[OpEntry]>,          // members × MAX_OPS
    resources: Box<[Resource]>,
    pending: Vec<Route>,          // cancels from begin(), closes from raise()
}
struct Member { arena: Rc<Arena>, handles: Box<[HandleSlot]>, grants: u16, quiescing: bool, drained: bool }
struct HandleSlot { generation: u16, res: u16, target: Option<Target> }
struct OpEntry { generation: u16, res: u16, kind: OpKind, live: bool, canceled: bool }
struct Resource { fd: Option<OwnedFd>, target: Target, counters: Counters, reported: Counters, name: FrameStr<64> }
pub(crate) enum Route { Os(OsOp), Cancel(OpId, Target), Sim(u16, Sqe), Close(RawFd), Reply(usize, Cqe) }
```

- `new(members: Vec<(Rc<Arena>, Vec<u16>)>, resources: Vec<(ResourceConfig, Option<(OwnedFd, Class)>)>)`
  sizes every table. Grant `n` of a member is handle `n`, then come
  `MAX_OPS` free derived slots.
- `lower(m, sqe, out: &mut Vec<Route>)` checks, in order:
  1. The member is quiescing: drop the op.
  2. `Cancel`: if the target entry is live and not yet canceled, mark it
     and emit `Cancel`, or `Sim(port, cancel sqe)` for a sim op.
  3. The handle (index, generation, live): otherwise reply `Stale`.
  4. `Close`: a grant slot goes dead. A derived slot emits `Close(fd)`.
     Either way, reply ok.
  5. `sqe.op()`: otherwise reply with its error.
  6. The span, through `arena.span`: otherwise reply `BadBuf`.
  7. `slot < MAX_OPS` and the entry is free: otherwise reply `Refused`.
     This is the one gate for guest input.
  8. Mark the entry live, then count `submitted`.
  9. Emit `Os(OsOp)` for a real target, or `Sim(port, sqe)` with the
     token rewritten to the `OpId` for a sim target.
- `raise(done: OsDone) -> Option<(usize, Cqe)>`. It skips a stale or dead
  entry and frees the entry. For a quiescing member, an `Opened` result
  goes to `pending` as a `Close`, and nothing is returned. Otherwise it
  counts the op. `Opened` takes a free derived slot (none free: close it
  and reply `Refused`), and the `Cqe` value is the new `RawHandle`.
- `raise_sim(cqe)` decodes the status and value into an `OsDone`, then
  calls `raise`.
- `begin(m)`, `idle(m) -> bool`, `drained(m)`, and `end(m)`:
  - `begin` marks the member and queues a cancel for each live op;
  - `drained` (on the first SQ drain after `begin`) closes the derived
    handles and revives the grants;
  - `idle` means drained and no live entries in `m`'s range.
- `report(now, out: &Output<ResourceStatus>)` writes the changed
  counters, at most 16 a cycle, round robin.

Tests (`tests/dispatch.rs`, a fake `Os` recording `OsOp`s): each `lower`
branch; derive and close; a full derived table; a stale generation after a
close; quiesce cancels each op exactly once; a close emitted for a
late-opened handle; `idle` only after the drain and the last completion;
counters and report round robin; no allocation in `lower` or `raise`.
Kani: `OpId` round trips, and `m * MAX_OPS + s` never aliases another
member's slot.

**Commit:** `feat(fsw3): io2 dispatch`.

## Phase 3: backends and resources

### 3.1 `open.rs`

`open(&RealKind) -> Result<(OwnedFd, Class), OpenError>` for `Tty`,
`Can`, `GpioLine`, `Udp`, `File`, `Dir`, and `TcpListen`. `TcpConnect`
returns `OpenError::Unsupported`, and build refuses it earlier anyway.
Also here, as free functions used by both backends: `configure(fd,
&TtyConfig)`, `flush(fd, TtyQueue)`, `gpio(fd, Option<bool>)`,
`open_at(dir, path, OpenFlags)`, `sockaddr(SocketAddr)`, and
`errno() -> IoError`.

### 3.2 Inline ops

Both backends run some ops in `push` and queue their `OsDone` at once:
tty configure and flush, GPIO, and `Open`. `Open` copies the path span to
a stack buffer, adds a NUL, and calls `openat`. This avoids a per-op
path buffer that the kernel would read later. `close` is `libc::close`.

### 3.3 `PollOs`

`new(cap)` preallocates `parked`, `done`, and `Events`.

- `push` tries the op nonblocking. On `EAGAIN` it parks the op and arms
  the fd for the union of its parked ops' interests, keyed by fd.
  Files and dirs run blocking (`pread`, `pwrite`, `fsync`).
- `submit` polls with a zero timeout and retries the parked ops on fds
  that fired.
- `reap` drains `done`.
- `cancel` removes a parked op (`Canceled`). An op already done is left
  alone.
- `close` cancels the fd's parked ops, deletes it from the poller, and
  closes it.

### 3.4 `UringOs` (Linux)

`new(cap, arenas: &[NonNull<[u8]>])` builds the ring
(`(2 * cap).next_power_of_two()`), probes the opcodes, and registers each
arena as fixed buffer `i`. It needs no fixed files.

| `Op` | uring opcode |
|---|---|
| `Read`, `Write` | `ReadFixed` or `WriteFixed` (`buf_index = arena`); for a stream, `offset = u64::MAX` |
| `Recv`, `Send` | `Recv` or `Send` |
| `SendTo` | `SendMsg`, using a per-op `MsgMem { addr, msghdr, iovec }` indexed by `OpId` index |
| `Accept` | `Accept` |
| `Fsync` | `Fsync` |

- `cancel` is `AsyncCancel` with `user_data = CANCEL | id`, and its CQE is
  dropped in `reap`.
- `reap` maps `res` to `Out::Len`, or for an accept to `Out::Opened(fd,
  Stream)`.
- A full SQ completes the op `QueueFull` in `done`.

### 3.5 Tests

`tests/os_cases/mod.rs` is rewritten over the new `Os` directly. Its rig
holds one `Arena`, the backend, and fds. The cases that stay:

- datagram send and recv;
- file write and read at offsets;
- tty configure (pty);
- a recv that parks until data arrives;
- two parked ops keep both interests;
- cancel a parked read;
- close cancels the parked ops;
- GPIO on a pty fails;
- accept gives a working stream;
- `open_at` under a dir;
- `QueueFull`.

Dropped with Dial: `resolve_on_dial`, `connect_and_send`,
`two_connects_give_two_streams`, `connect_refused`,
`read_on_dial_unsupported`, and `resolve_on_stream_unsupported`.
`tests/io_poll.rs` and `tests/io_uring.rs` wire the backends.
`tests/io.rs`'s open cases move to `open` (the `tcp_connect` and
unresolvable dial cases become one "unsupported" case).

The `os_cases` tests use `io2` paths until phase 5. To keep them out of
the public API, export the backends as `#[doc(hidden)] pub mod
io2_host` until the swap, which renames it.

**Commit:** `feat(fsw3): io2 backends and resource opening`.

## Phase 4: sim and the test harness

### 4.1 `device.rs`

- `DeviceSide<K> { requests: Input<Sqe>, replies: Output<Cqe>, arenas: Rc<Arenas>, held: Held, outbox: Vec<Cqe>, serves: u64 }`.
  `Arenas` is the build's list of member arenas, filled at bind, behind a
  `RefCell<Vec<Rc<Arena>>>`, borrowed only for a lookup.
- A request's member is `OpId::index / MAX_OPS`. Its bytes come from
  `arenas[member].span(sqe.buf)`. A failed lookup replies `BadBuf`.
- `Model<K>` becomes `tick(now)`, `latency(OpKind)`, and `answer(now, &Op,
  bytes) -> Reply`. It is implemented for every `UartModel` (`rx`, `tx`,
  `configure`, `tick`, `latency`) and every `GpioModel`. A model sees the
  whole buffer for an op that fills, and the filled bytes otherwise.
- `serve(now, &mut impl Model<K>)` ticks the model, flushes the outbox,
  takes requests (answering a cancel at once), answers due ops, and
  flushes again.
- `Held` is a fixed `Vec<(Sqe, u64)>` with `push`, `remove(token)`, and
  in-order iteration of the due ops.

### 4.2 `test.rs`

- `test::cap::<K, M: Model<K>>(model) -> (Cap<K>, TestIo<M>)`:
  - the `Cap`'s SQ ring is the `DeviceSide`'s request ring, and its reply
    ring is the CQ, with no `Dispatch`;
  - for member 0, `OpId` equals the system's token, so nothing is
    rewritten;
  - `TestIo` exposes `init()`, `turn()` (serves once at the next
    timestamp), `model()`, `model_mut()`, `submitted()`, and `now()`.
- `test::device::<K>() -> (DeviceSide<K>, TestDevice)` keeps today's
  `TestDevice` methods.
- `ScriptedUart` keeps its builders.

Tests carried from `sim/tests.rs`:

- a read fills the driver's buffer;
- a write reaches `tx` once;
- GPIO get and set;
- latency;
- an empty read holds;
- cancel while held;
- cancel of an unknown token;
- a full reply port defers to the outbox;
- `Held` capacity;
- a bad span replies `BadBuf`;
- no allocation per serve.

Also the 18-io.md driver example through `test::cap`.

**Commit:** `feat(fsw3): io2 sim devices and test harness`.

## Phase 5: the swap

One commit. The order below is the working order, not separate commits.

1. **`build.rs` (`IoBuild`, which replaces `IoShared`).** It holds
   `resources`, `grants`, `members: Vec<MemberInfo { id, arena: Rc<Arena>,
   caps }>`, `arenas: Rc<Arenas>`, and `host: Option<Host { dispatch,
   os: Box<dyn Os> }>`.
   - `member(id, caps) -> Rc<Arena>` returns the same arena on a
     remake.
   - `adopt(id, caps, Arena)` is for a guest's region.
   - `held_cap(id, param)` is kept.
   - `begin(id) -> bool`, `idle(id)`, and `end(id)` forward to
     `Dispatch` by member index, or are trivially idle before
     `io_submit` is made.

   `SharedIo` stays the `Rc<RefCell<_>>` alias, so `MakeCx` and the
   recipes change only in their types.
2. **`host/steps.rs`.**
   - The definitions are the same as today's (Python and goldens
     unchanged).
   - `make_submit` maps its inputs to member indices, opens the wanted
     real resources, freezes the arenas, builds the `Os` from
     `host(cap, arenas)` and the `Dispatch`, and stores the `Host`.
   - `SubmitStep` drains each SQ into `lower`, routes `Os`, `Cancel`,
     `Sim`, and `Close`, keeps the replies for `io_complete` (in
     `Dispatch.pending`), and calls `os.submit()`.
   - `CompleteStep` reaps, drains `{res}.rep`, raises, writes the CQs and
     the early replies, then reports `io_status` and `publish_dropped`.
   - `refuse_thread` is kept.
3. **`fn_system`.**
   - `set.rs::bind` takes `member()` and builds `Io::new(sq, cq, arena)`
     and `DeviceSide::new(req, rep, arenas, held)`.
   - `ctor.rs::make_state` gives `IoInit::new(io.arena())`.
   - `param.rs` imports from `io2`.
   - `Defs` is unchanged.
4. **The channel adapter.** `member()`, `begin`, `idle`, and `end` call
   `IoBuild` by id. The `Quiescing` state and the events are unchanged.
5. **Guests.**
   - `pack/mod.rs`:
     - `adopt` makes the pack-side member over the given region with
       `Arena::over(.., Keep::Lease)`;
     - `region_of` exports `arena.region()` and `owner()`;
     - `host_caps` is kept.
   - `dl/mod.rs`: `adopt` makes the host member over the dylib's region
     under a lease.
   - `wasm/pack.rs`:
     - on create, give a remade guest a zeroed region of the mirror's
       length (no directory to copy);
     - on attach, the host member is `Arena::over(mirror bytes, Boxed)`.
   - `wasm/io.rs` (`IoCopy`, rewritten at about 120 lines):
     - `guest: (NonNull<u8>, usize)`, `mirror: Rc<Arena>`, the two
       second readers, and `reads: [Option<(Token, Span)>; MAX_OPS]`;
     - submissions copy each draining op's span from the guest to the
       mirror, and note each filling op;
     - completions copy `min(n, span.len)` bytes from the mirror to the
       guest;
     - every span is checked against both lengths;
     - Kani: `copy` stays in bounds for any span and lengths.
   - `ABI_VERSION` goes from 10 to 11.
6. **Build and errors.**
   - `check_resources` refuses `RealKind::TcpConnect` with
     `BuildError::Backing { backing: "tcp_connect" }`.
   - `ParamError::IoBusy` is deleted (a stale `Buf` derefs empty).
   - `CorruptTable` is renamed `BadRegion` (a region of the wrong
     length on a remake).
   - `link/mod.rs` registers the new step entries.
7. **Moves.** `Rng` goes to `src/rng.rs`, re-exported at the crate root.
   Fix the imports, including echo-pack's `chatter.rs`. `cli/module.rs`
   changes only its imports.
8. **Delete and rename.** Delete `src/io/` and rename `src/io2/` to
   `src/io/` (and `io2_host` to `io::host`). Fix the paths in `tests/`,
   `tests/fixtures/echo-pack`, and `examples/adcs-fsw3` (rebuild only).
   `lib.rs` re-exports `Buf`, `Cap`, `DeviceSide`, `Handle`, `Io`,
   `IoInit`, `Pending`, `ResourceStatus`, `Slot`, and `Rng`.

**Exit:**

- These tests pass, with only path and type edits: `tests/io.rs`,
  `sim.rs`, `dl.rs`, `determinism.rs` (same records, deterministic),
  `wasm.rs`, `slot.rs`, `sequences.rs`, and `run.rs`.
- The echo-pack GPS works through a pty, and a guest's `Reset` waits for
  its ops.
- The Python tests (`python/metor-config/tests/test_io.py`) are
  unchanged and pass.
- The Linux VM runs `io_uring`.
- `wc -l` of `src/io/` (outside its tests) is at most 3,500.

**Commit:** `refactor(fsw3): replace io with io2`.

## Phase 6: review

An antagonistic review by another model, over the phase 1 to 5 commits,
focused on:

- the `unsafe` around `Arena`, `Buf`, and `IoCopy`;
- the guest gate in `lower`;
- quiesce ordering;
- the uring buffer lifetime on teardown.

Teardown: dropping the `Host` cancels every live op and reaps for a
bounded number of rounds. If ops are still live after that, it leaks the
arenas (`mem::forget` of their `Rc`), as today.

Fixes go in their own commit.

## Notes for the implementer

- Follow the crate's style guide and the doc and comment memory rules:
  short doc comments, `PANIC Safety` notes, no `#[cfg(test)]` hooks on
  production types, and test from outside.
- No allocation after build in `lower`, `raise`, `serve`, the backends'
  `push`, `submit`, and `reap`, or `IoCopy`. Every vector is sized in a
  constructor.
- Read nothing from the old `src/io/` for code. Its tests are a checklist
  of behaviors, not code to port.
