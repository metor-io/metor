# IO chunk A: IO core: ops, buffers, Os, reactor

Implements chunk A of `18-io-plan.md`, against `18-io.md`. The op types,
the buffer table, the `Os` trait, its two backends, and the `Reactor` land
in metor-fsw-3. stellarator gains two small additions. Seven steps, in
order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- from step 6, `cargo test -p stellarator`;
- `cargo clippy --all-targets` clean on the crates touched;
- `cargo check -p metor-fsw-3 --no-default-features --target
  wasm32-unknown-unknown`;
- one commit.

No crate outside metor-fsw-3 and stellarator changes. db, metor-panel,
metor-proto/stellar, fsw-2, inscriber, and video-streamer build unchanged
at every step. stellarator's executor, ops, and public API are not
rewritten.

## Decisions

1. **Ops carry `BufRef`, never pointers.** A `BufRef` names a slot in the
   submitting system's buffer table. Ops are plain bytes, so they can be
   logged, compared, and copied across the guest ABI.
2. **An `Os` issues its own handles.** `Os::register` takes an opened
   descriptor and its `FdClass`, and returns the `RawHandle` ops name. The
   class picks the path each op takes. Turning a system's handle into the
   `Os`'s handle is done above the reactor, by `io_submit` (chunk C). The
   reactor never looks at handles.
3. **The reactor owns tokens.** A source's `Token` is mapped to a
   reactor-wide token on the way down and back on the way up. The
   reactor's own ops (cancels, and closes pushed with `push_own`) take
   tokens from a separate range, marked by the high bit, so they never
   compete with systems' ops and never complete `QueueFull`.
4. **The reactor stamps buffer tables.** A source is registered with its
   table. The reactor writes that table's id into every `BufRef` of the
   source's ops, so a system can only name its own buffers.
5. **Results arrive in N+1.** `io_submit` pushes every op and calls
   `submit`, which hands them to the kernel at once. `io_complete` calls
   `reap` later in the same cycle and collects what has landed. Nothing
   completes during the call that submitted it.
6. **Nothing blocks.** `Os::submit` never waits. No thread sleeps on a
   reactor, so there is no wake handle.
7. **Errors are portable.** `IoStatus` carries Linux errno numbers. The poll
   backend maps host errnos to Linux numbers on the way up.
8. **Little-endian, alignment-one fields.** `Op` and `Out` are
   `#[repr(C, u8)]`. zerocopy's `IntoBytes` refuses an enum with padding,
   so every field is a zerocopy byte-order type (`U16<LE>`, `U32<LE>`,
   `U64<LE>`) of alignment one, and every variant is padded by hand to one
   size. Constructors and getters hide the wrappers.
9. **One arena per table.** A system's buffer table is one arena of
   bytes. A `BufRef` names a slot, never an address, so the arena may
   move while the system is made. `freeze` fixes it; only then does `raw`
   hand out pointers. A backend's constructor takes the `Tables` it
   resolves `BufRef`s through, all frozen by then. `UringOs` registers
   each table's arena as one io_uring fixed buffer, once. A table added
   later uses plain `Read`/`Write`.
10. **Fixed limits.** `Limits { ops, sources }`. A full token table
    completes a system's op with `QueueFull`. The reactor's own range
    holds `2 × ops` tokens: one cancel per system op and one close per
    derived handle, so it cannot fill. Nothing grows after `new`.
11. **A dial target is data, not a descriptor.** `register` is for opened
    descriptors only. Each backend's constructor takes the target's dial
    addresses, and dial target `i` gets the handle `RawHandle::new(i, 0)`:
    the first handles, issued before any `register`. On that handle,
    `Resolve` answers the stored address, and `Connect` creates a socket
    and connects it:
    - io_uring: an `IORING_OP_SOCKET` linked to a `CONNECT`, on a direct
      descriptor (Linux 5.19+);
    - poll: a nonblocking `socket()`, then `connect`, then a wait for
      writability and a read of `SO_ERROR`.

    The new socket completes as `Out::Handle`, a `Stream`. Other ops on a
    dial handle complete `Unsupported`.
12. **The backends live in fsw-3.** `UringOs` owns its own io_uring
    instance, apart from stellarator's. `PollOs` uses `polling` and
    nonblocking syscalls, and runs file ops inline and blocking, which is
    fine on development hosts. Both are `cfg(unix)`.
13. **stellarator gets only what kept work needs.** `Executor::turn` and a
    simulated clock serve chunk E's deterministic thread mode.
    `TcpListener::adopt` serves chunk F's links. It takes a
    `std::net::TcpListener`, so fsw-3 gains no dependency.

## Layout

```
libs/metor-fsw-3/src/io/
  op.rs        // Sqe, Op, OpKind, OpenFlags, TtyOp, GpioOp          (core)
  out.rs       // Cqe, Out, IoStatus, IoError, Errno                 (core)
  addr.rs      // RawAddr, From/TryFrom core::net::SocketAddr        (core)
  handle.rs    // RawHandle, Token                                   (core)
  buf.rs       // BufRef, BufTable, Tables, TableId, RawBuf          (core)
  os.rs        // Os, FdClass, host(), quick ioctls                  (host, unix)
  reactor.rs   // Reactor, SourceId, Limits, Tokens                  (host)
  uring.rs     // UringOs                                            (host, linux)
  poll.rs      // PollOs, host errno mapping                         (host, unix)
  tests/{op,buf,reactor,uring,poll}.rs
```

`Cargo.toml`: `zerocopy` is already a dependency. The `host` feature adds
`polling` (unix) and `io-uring` (linux). `libc` is already in `host`.
Chunk B adds `io/mod.rs`'s surface types and the `Record` impls.

stellarator after A:

```
src/lib.rs     // EXEC holds Option<Rc<Executor>>; Executor::new, turn, spawn
src/time.rs    // new: Clock { Os, Sim }, sim_now, sim_set
src/net/tcp.rs // TcpListener::adopt
```

## Interfaces other chunks use

These signatures are fixed for B through G. Field types written `u32` in
prose are `U32<LE>` in the struct; each type has `new` constructors and
plain getters.

### Op types (`crate::io`, core)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(transparent)]
pub struct Token(U32<LE>);                 // Token::new(u32), .get()

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct RawHandle { index: U32<LE>, generation: U32<LE> }
impl RawHandle {
    pub const fn new(index: u32, generation: u32) -> Self;
    pub fn index(self) -> u32;
    pub fn generation(self) -> u32;
    /// `openat`'s current directory.
    pub const CWD: Self;
}

#[repr(C)] // IntoBytes, FromBytes, Unaligned
pub struct BufRef { table: U16<LE>, slot: U16<LE> }
impl BufRef { pub const fn new(slot: u16) -> Self; pub fn slot(self) -> u16; pub fn table(self) -> TableId; }

#[repr(C)] // IntoBytes, FromBytes, Unaligned; 28 bytes
pub struct RawAddr { family: U16<LE>, port: U16<LE>, flowinfo: U32<LE>, ip: [u8; 16], scope_id: U32<LE> }
impl From<core::net::SocketAddr> for RawAddr;
impl TryFrom<RawAddr> for core::net::SocketAddr; // Err(IoError::BadAddr)

#[repr(C, u8)] // IntoBytes, TryFromBytes, Unaligned; every variant 32 bytes
pub enum Op {
    Read { buf: BufRef, _p: [u8; 4], offset: U64<LE>, _q: [u8; 16] }, // u64::MAX: current position
    Write { buf: BufRef, _p: [u8; 4], offset: U64<LE>, _q: [u8; 16] },
    Recv { buf: BufRef, _p: [u8; 28] },
    Send { buf: BufRef, _p: [u8; 28] },
    SendTo { buf: BufRef, addr: RawAddr },
    Open { path: BufRef, flags: OpenFlags, _p: [u8; 16] },
    Accept { _p: [u8; 32] },
    Resolve { _p: [u8; 32] },
    Connect { addr: RawAddr, _p: [u8; 4] },
    Close { _p: [u8; 32] },
    Fsync { _p: [u8; 32] },
    Tty { op: TtyOp, _p: [u8; 20] },
    Gpio { op: GpioOp, _p: [u8; 24] },
    Cancel { target: Token, _p: [u8; 28] },
}
impl Op {
    pub fn read(buf: BufRef, offset: Option<u64>) -> Self; // None -> u64::MAX
    pub fn write(buf: BufRef, offset: Option<u64>) -> Self;
    pub fn recv(buf: BufRef) -> Self;
    pub fn send(buf: BufRef) -> Self;
    pub fn send_to(buf: BufRef, addr: RawAddr) -> Self;
    pub fn open(path: BufRef, flags: OpenFlags) -> Self;
    pub fn connect(addr: RawAddr) -> Self;
    pub const ACCEPT: Self; pub const RESOLVE: Self; pub const CLOSE: Self; pub const FSYNC: Self;
    pub fn tty(op: TtyOp) -> Self;
    pub fn gpio(op: GpioOp) -> Self;
    pub fn cancel(target: Token) -> Self;
    pub fn kind(&self) -> OpKind;
    /// The buffer the op reads or fills, if any.
    pub fn buf(&self) -> Option<BufRef>;
    pub(crate) fn buf_mut(&mut self) -> Option<&mut BufRef>;
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpKind { Read, Write, Recv, Send, SendTo, Open, Accept, Resolve, Connect, Close, Fsync, Tty, Gpio, Cancel }
impl OpKind { pub const ALL: [OpKind; 14]; pub fn name(self) -> &'static str; pub fn parse(&str) -> Option<Self>; }

#[repr(C)] // 12 bytes: portable bits, translated by each Os
pub struct OpenFlags { bits: U32<LE>, mode: U32<LE>, _p: [u8; 4] }
bitflags-style consts: READ, WRITE, APPEND, CREATE, TRUNCATE, EXCLUSIVE, NONBLOCK, NOCTTY, DIRECTORY.

#[repr(C, u8)] // 12 bytes: tag plus 11-byte variants
pub enum TtyOp {
    Configure(TtyConfig),     // baud u32, data bits u8, parity u8, stop bits u8, flow u8, vmin u8, vtime u8, _p [u8; 1]
    Flush { queue: u8, _p: [u8; 10] }, // 0 input, 1 output, 2 both
}
#[repr(C, u8)] // 8 bytes: tag plus 7-byte variants
pub enum GpioOp { Get { _p: [u8; 7] }, Set { value: u8, _p: [u8; 6] } }

#[repr(C)] // IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned
pub struct Sqe { pub token: Token, pub handle: RawHandle, pub op: Op, _p: [u8; 3] } // 48 bytes

#[repr(C)] // 4 bytes: 0 ok, 1..=0xFFFF a Linux errno, 0x1_0000.. framework codes
pub struct IoStatus(U32<LE>);
impl IoStatus {
    pub const OK: Self;
    pub fn err(e: IoError) -> Self;
    pub fn result(self) -> Result<(), IoError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoError { Os(Errno), QueueFull, Stale, Canceled, Refused, Device, Unsupported, BadBuf, BadAddr }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(u16);  // Linux numbering
impl Errno {
    pub const EIO: Self; pub const EAGAIN: Self; pub const EINVAL: Self; /* ~30 names used by the backends */
    pub fn name(self) -> Option<&'static str>;
    pub fn parse(name: &str) -> Option<Self>; // "EIO"
}

#[repr(C, u8)] // IntoBytes, TryFromBytes, Unaligned; every variant 28 bytes
pub enum Out { Unit { _p: [u8; 28] }, Len { len: U32<LE>, _p: [u8; 24] }, Handle { handle: RawHandle, _p: [u8; 20] }, Addr { addr: RawAddr } }
impl Out {
    pub const UNIT: Self; pub fn len(n: u32) -> Self; pub fn handle(h: RawHandle) -> Self; pub fn addr(a: RawAddr) -> Self;
    pub fn as_len(&self) -> Option<u32>; pub fn as_handle(&self) -> Option<RawHandle>; pub fn as_addr(&self) -> Option<RawAddr>;
}

#[repr(C)] // 40 bytes
pub struct Cqe { pub token: Token, pub status: IoStatus, pub out: Out, _p: [u8; 3] }
impl Cqe { pub fn ok(token: Token, out: Out) -> Self; pub fn err(token: Token, e: IoError) -> Self; }
```

Sizes are pinned by `const _: () = assert!(size_of::<Sqe>() == 48)` and
the like. B's `Record` impls use `Sqe::try_read_from_bytes` and
`IntoBytes::as_bytes`.

### Buffer table (`crate::io::buf`, core)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableId(u16);

/// One slot's memory, as a backend sees it.
#[derive(Clone, Copy)]
pub struct RawBuf { pub ptr: NonNull<u8>, pub cap: u32, pub len: u32 }

/// One system's IO memory: one arena, with its directory at the start.
/// Shared by `Rc`; every method takes `&self`.
pub struct BufTable { /* RefCell<Arena>, id */ }
impl BufTable {
    /// A table with room for `slots` directory entries.
    pub fn new(slots: usize) -> Self;
    /// The id `Tables::add` gave it.
    pub fn id(&self) -> Option<TableId>;
    /// Appends `cap` zeroed bytes. Init only. Before `freeze` the arena grows;
    /// after it, the bytes must fit the arena's capacity.
    pub fn alloc(&self, cap: usize) -> Result<u16, BufError>;
    pub fn alloc_from(&self, bytes: &[u8]) -> Result<u16, BufError>;
    /// Fixes the arena's size and address. Idempotent.
    pub fn freeze(&self);
    /// The whole arena once frozen. `UringOs` registers it as one fixed buffer.
    pub fn arena(&self) -> Option<NonNull<[u8]>>;
    /// Frees a slot. Its bytes are not reused before a rewind (C).
    pub fn release(&self, slot: u16);
    /// The slot's memory. `None` before `freeze`.
    pub fn raw(&self, slot: u16) -> Option<RawBuf>;
    pub fn set_len(&self, slot: u16, len: u32) -> Result<(), BufError>;
    /// # Safety
    /// The caller holds the slot, and no op using it is in flight.
    pub unsafe fn filled(&self, slot: u16) -> &[u8];
    pub unsafe fn filled_mut(&self, slot: u16) -> &mut [u8];
    pub unsafe fn whole_mut(&self, slot: u16) -> &mut [u8];
}
pub enum BufError { Full, TooLarge, BadSlot, NoSpace { need: usize, free: usize } }

/// Every table a backend resolves against. Filled at init, shared with the backend.
#[derive(Clone)]
pub struct Tables(Rc<RefCell<Vec<Rc<BufTable>>>>);
impl Tables {
    pub fn new() -> Self;
    /// Adds a table and sets its id. A table belongs to one `Tables`.
    pub fn add(&self, table: &Rc<BufTable>) -> TableId;
    pub fn raw(&self, buf: BufRef) -> Option<RawBuf>;
    pub fn set_len(&self, buf: BufRef, len: u32) -> Result<(), BufError>;
}
```

The arena starts with a directory: a header `{ slots, used, top }` and
`slots` entries `{ offset, cap, len, held }`, each a `U32<LE>`. Data
follows. While the system is made, the arena is a `Vec<u8>`, and `alloc`
appends at `top`; the `Vec` may reallocate. `freeze` turns it into a boxed
slice of exactly its length, held as a raw `NonNull<[u8]>` and freed in
`Drop`. That length is the arena's capacity for good. No `&mut` to frozen
memory is formed outside the unsafe accessors, so pointers handed to the
kernel stay valid under Stacked and Tree Borrows. A slot is `held` from
`alloc` to `release`. Whether an op is in flight is the holder's protocol
(B's `Pending`); `Tables::raw` marks nothing.

### `Os` and the reactor (`crate::io`, host)

```rust
/// What a registered descriptor is. It picks the path each op takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdClass { Stream, Datagram, Listener, File, Dir, Tty, Can, Gpio }

pub trait Os {
    /// Queues an op. Never fails: a full queue comes back from `reap` as `QueueFull`.
    fn push(&mut self, sqe: Sqe);
    /// Hands queued ops to the kernel. Never blocks.
    fn submit(&mut self);
    /// Takes every completion that has landed. Never blocks.
    fn reap(&mut self, done: &mut Vec<Cqe>);
    /// Takes ownership of an opened resource; ops name the handle it returns.
    fn register(&mut self, fd: std::os::fd::OwnedFd, class: FdClass) -> RawHandle;
}

/// The host backend: `UringOs` on Linux, `PollOs` on other Unix hosts.
pub fn host(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError>;

/// A submitter: one system. Issued in order from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u16);

pub struct Limits { pub ops: usize, pub sources: usize }

pub struct Reactor { /* os, tokens, sources: Vec<Rc<BufTable>>, queued, raw */ }
impl Reactor {
    pub fn new(os: Box<dyn Os>, limits: Limits) -> Self;
    /// Adds a submitter whose `BufRef`s name `table`. Init only.
    pub fn source(&mut self, table: Rc<BufTable>) -> SourceId;
    /// Maps the op's token, stamps its table, and queues it.
    pub fn push(&mut self, src: SourceId, sqe: Sqe);
    /// Hands the queued ops to the Os.
    pub fn submit(&mut self);
    /// Collects what landed and maps each token back to its source's.
    pub fn reap(&mut self, done: &mut Vec<(SourceId, Cqe)>);
    /// Cancels every op `src` has in flight. Their completions still come back from `reap`.
    pub fn cancel_all(&mut self, src: SourceId);
    /// Queues an op under a reactor-own token. `reap` drops its completion.
    pub fn push_own(&mut self, sqe: Sqe);
    /// Ops of `src` not yet returned by `reap`.
    pub fn in_flight(&self, src: SourceId) -> usize;
}
impl Drop for Reactor { /* cancel all, drain rounds, then leak */ }
```

Build registers each opened resource with the concrete `Os` before boxing
it into the reactor, so `Reactor` has no `register`.

`push` never fails. A full token table, an unknown source, a `BufRef`
whose slot does not resolve, or a `Cancel` for an unknown token completes
with `QueueFull`, `Refused`, `BadBuf`, or `Os(ENOENT)` in the next `reap`.
A `Cancel` maps its `target` through the same source's tokens. `cancel_all`
pushes one `Cancel` per live op of the source, under reactor-own tokens.
Reactor-own tokens have the high bit set and live in their own slab of
`2 × limits.ops`, apart from the systems' slab of `limits.ops`; `reap`
drops their completions. Every `Vec` the reactor fills is sized in `new`.

`Drop` cancels every source's ops, then calls `submit` and `reap` until
nothing is in flight or three rounds pass. The table of a source with ops
still in flight is leaked with `mem::forget` on an `Rc` clone, never
freed, so the kernel never writes into freed memory.

### stellarator

```rust
pub struct Executor { /* unchanged fields, plus its Clock */ }
impl Executor {
    /// An executor with its own reactor and the given clock.
    pub fn new(clock: time::Clock) -> Result<Self, Error>;
    /// One pass: timer, IO, one scheduler tick. Never blocks and never moves the
    /// clock. Installs `self` in this thread's EXEC for the pass and restores what was there.
    pub fn turn(self: &Rc<Self>) -> Turned;
    /// Spawns onto this executor, installed the same way.
    pub fn spawn<F: Future + 'static>(self: &Rc<Self>, f: F) -> JoinHandle<F::Output> where F::Output: Send + 'static;
}
pub struct Turned { pub runnable: bool, pub next_deadline: Option<Duration> }

pub mod time {
    pub enum Clock { Os, Sim }
    /// The simulated time on this thread, in microseconds. `Clock::Sim` reads it.
    pub fn sim_now() -> u64;
    pub fn sim_set(micros: u64);
}

impl TcpListener {
    /// Takes a listener that is already bound and listening.
    pub fn adopt(listener: std::net::TcpListener) -> io::Result<Self>;
}
```

`EXEC` holds `Option<Rc<Executor>>`, so an executor's timer keeps its
address while it is swapped out, and `sleep`'s `&'static Timer` stays
valid while the executor lives. `run` and `spawn` behave as today.

**Drop order.** `sleep` builds a `Sleep<'static>` over `&e.timer`
(`lib.rs:220-223`), and maitake's `cancel` only begins cancellation. So
every task and `JoinHandle` spawned on a turned executor is dropped
before the executor. The owner drops its handles first, then the
executor. `Drop for Executor` cancels its tasks and ticks the scheduler
once, so each canceled task drops its future, and its `Sleep`, while
the timer is alive; only then do its fields drop.

## Steps

### Step 1. The op types

Files: new `src/io/{mod,op,out,addr,handle}.rs`, `src/io/tests/op.rs`,
`src/lib.rs` (`pub mod io;`).

1. The types in "Op types", with `const` size asserts: `Sqe` 48, `Cqe` 40,
   `Op` 33, `Out` 29, `RawAddr` 28, `OpenFlags` 12, `TtyOp` 12,
   `GpioOp` 8, and each `Op` variant 32.
2. `RawAddr` conversions for V4 and V6. `IoError` to and from `IoStatus`.
   `OpKind::name` gives the snake-case names Python uses (`"read"`,
   `"send_to"`, ...).

Tests (`io/tests/op.rs`):
- nominal: each `Op` constructor round-trips through `as_bytes` and
  `Sqe::try_read_from_bytes`; `kind()` and `buf()` agree with the variant;
  `Cqe` round-trips for each `Out`; V4 and V6 addresses round-trip.
- off-nominal: `offset: None` encodes as `u64::MAX`; a scope id survives;
  `IoStatus::OK.result()` is `Ok`.
- failure: a tag byte past the last variant fails `try_read_from_bytes`; a
  short slice fails; an unknown `IoStatus` code decodes to
  `IoError::Os(Errno(code))` for errnos and to `Refused` for an unknown
  framework code; `RawAddr` with family 0 fails `TryFrom`.
- Kani (`#[cfg(kani)]`): for any 48 bytes, `Sqe::try_read_from_bytes`
  either fails or re-encodes to the same bytes. Same for `Cqe` over 40.

### Step 2. The buffer table

Files: new `src/io/buf.rs`, `src/io/tests/buf.rs`.

1. `BufTable`, `RawBuf`, `TableId`, `BufRef::table`, `Tables`, as in
   "Buffer table". The arena sits in a `RefCell<Arena>`, borrowed only
   inside one method. `Arena` is `Making(Vec<u8>)` or `Frozen(NonNull<[u8]>)`.
   The directory is read and written in place, through zerocopy, in both
   forms; there is no second entry list.
2. `alloc` takes the first entry that is not `held` and appends `cap`
   zeroed bytes at `top`. Before `freeze` the `Vec` grows. After it, a
   request past the capacity is `NoSpace { need, free }`. `release` clears
   `held`; its bytes stay unused until a rewind (C2).
3. `freeze` moves `Making` to `Frozen`. `raw` and `arena` are `None`
   before it. The unsafe accessors work in both forms.
4. Past `slots`, `alloc` returns `BufError::Full`.
5. `Tables::add` sets the table's id once.

Tests:
- nominal: `alloc_from` then `filled` gives the bytes; `whole_mut` has
  `cap` bytes; `set_len` changes `filled`; `Tables::raw` resolves a
  `BufRef` stamped with the table's id, and `id()` is that id.
- off-nominal: a released slot index is reused by the next `alloc`; an
  `alloc` that reallocates the `Vec` before `freeze` keeps every earlier
  slot's bytes; after `freeze`, an `alloc` that fits gives a pointer
  inside `arena()`, and earlier pointers do not move; a table added after
  a backend took `Tables` resolves.
- failure: the slot past `slots` is `Full`; an `alloc` past a frozen
  arena's capacity is `NoSpace` with the right `need` and `free`;
  `set_len` past `cap` is `TooLarge`; `raw` before `freeze`, and of a
  released or unknown slot, is `None`; a `BufRef` naming an unknown table
  is `None`.
- Miri (`cargo +nightly miri test -p metor-fsw-3 --no-default-features
  --lib io::tests::buf`): a pointer from `raw` is written through after
  later `alloc`s and `filled` reads, under `-Zmiri-tree-borrows` and the
  default model; a table drops with held slots and frees its arena once.

### Step 3. The `Os` trait and the reactor

Files: new `src/io/{os,reactor}.rs` (host), `src/io/tests/reactor.rs`,
`Cargo.toml` (`polling` for unix and `io-uring` for linux, under `host`).

1. `Os` and `FdClass` in `os.rs`. `host()` is declared here and returns
   `Err(OsError::Unsupported)` until steps 4 and 5 fill it.
2. `Tokens`, private to `reactor.rs`: a slab of `(SourceId, Token)` keyed
   by a reactor token (`u32`), with a free list. `map(src, token) ->
   Option<Token>`, `unmap(reactor_token) -> Option<Entry>`, `find(src,
   token)` for `Cancel`, and `live(src)` for `cancel_all` and `in_flight`.
   `find` and `live` scan the live entries; ops in flight are bounded by
   `limits.ops`. A second, smaller slab of free flags, `2 × limits.ops`
   long, issues the reactor-own tokens `0x8000_0000 | i`.
3. `Reactor` as in "`Os` and the reactor". Each function does one thing:
   - `push` calls `lower` (map token, stamp table, map a `Cancel`'s
     target) and queues the op.
   - `submit` hands each queued op to `os.push`, then calls `os.submit()`.
   - `reap` calls `os.reap` into `raw`, then `raise` maps each back into
     `done`. A `Cqe` whose reactor token is unknown, or belongs to
     `cancel_all`, is dropped. Ops refused in `push` are answered here.
   - `cancel_all` pushes a `Cancel` for each of `live(src)`, under
     reactor-own tokens.
   - `push_own` queues an op under a reactor-own token.
   - `Drop` runs the teardown in "`Os` and the reactor".

Tests use `ScriptOs`, a test-module `Os` that records each pushed `Sqe`,
counts `submit` calls, issues handles in order from `register`, and posts
`Cqe`s the test queues on the next `reap`:
- nominal: an op from source 1 reaches the `Os` with a reactor token and
  its `BufRef` stamped with source 1's table; its completion comes back as
  `(1, cqe)` with the source's token; two sources using the same `Token`
  value do not collide; a pushed op reaches the `Os` only at `submit`; a
  `Cqe` that lands after `submit` is returned by the next `reap`, never
  during `submit`.
- off-nominal: `Cancel { target }` reaches the `Os` with the target's
  reactor token; completions in an order other than submission are routed
  right; a token is reusable after its completion; `cancel_all(1)` pushes
  one `Cancel` per live op of source 1 and none for source 2, and their own
  completions never reach `done`; `in_flight(1)` falls to zero as source
  1's completions are reaped.
- off-nominal: with every system token taken, `cancel_all` and
  `push_own` still reach the `Os`, and their completions are dropped.
- failure: an op past `limits.ops` completes `QueueFull` on the next
  `reap` and never reaches the `Os`; an unknown `SourceId` is `Refused`; a
  `Cancel` naming no live op is `Os(ENOENT)`; an unknown reactor token
  from the `Os` is dropped; dropping a reactor whose `ScriptOs` never
  answers ends after three rounds, and the source's table is still alive
  afterwards (a `Weak` upgrades).
- alloc: with a counting global allocator in the test binary, a cycle of
  64 ops through `push`/`submit`/`reap` allocates nothing after `new`.
- Miri (`cargo +nightly miri test -p metor-fsw-3 --no-default-features
  --features host --lib io::tests::reactor::drop`): the drop test, to
  check the leaked table is never freed. `ScriptOs` makes no syscalls.

### Step 4. `PollOs`

Files: new `src/io/poll.rs` (host, `cfg(unix)`), `src/io/os.rs` (quick
ioctls, `host()` off Linux), `src/io/tests/poll.rs`.

1. **Descriptor table.** `register` stores `(fd, class, generation)` in a
   slab and returns `RawHandle::new(index, generation)`. The constructor
   first stores each dial target as `Dial(addr)` in slots `0..n`. A handle
   the table did not issue, or a closed one, completes `Stale`.
2. **Readiness.** `Parked { sqe, raw: RawBuf }` per reactor token, and a
   per-fd interest union, so two ops on one fd never clobber each other's
   interest. `try_op(sqe) -> Option<Cqe>` makes the nonblocking syscall;
   `EAGAIN` parks and registers with the `Poller`.
3. **Routing by class.** On `Stream`, `Datagram`, `Listener`, `Tty`, and
   `Can`, ops try and then wait for readiness. On `File` and `Dir`,
   `Read`, `Write`, `Open`, and `Fsync` run inline and blocking. `Tty` and
   `Gpio` ioctls and `Close` run inline. Inline results wait for `reap`.
4. **Dialing** (decision 11). On a dial handle, `Resolve` completes
   `Out::Addr(addr)`. `Connect { addr }` makes a nonblocking, close-on-exec
   socket of the address's family and calls `connect`. `EINPROGRESS`
   parks the op for writability; then `SO_ERROR` gives the result. On
   success the socket is registered as `Stream` and the op completes
   `Out::Handle`. Any other op on a dial handle is `Unsupported`.
   `Resolve` on any other handle is `Unsupported`.
5. **`Cancel`** removes a parked op and completes it `Canceled`. An op
   that already ran inline has its result posted, and the cancel completes
   `Os(EALREADY)`.
6. **`submit`** runs each queued op. **`reap`** polls with a zero timeout,
   retries every op whose fd fired, and drains inline results.
7. **Errnos.** One table of `(Errno, libc::E*)` pairs maps host numbers to
   Linux numbers. An unknown host errno maps to `EIO`.
8. **Quick ioctls** in `os.rs`, shared by both backends: `Tty::Configure`
   reads `tcgetattr`, applies the config, and `tcsetattr(Now)`.
   `Tty::Flush` is `tcflush`. `Gpio` uses the Linux GPIO v2 line ioctls and
   is `Unsupported` off Linux.
9. `host()` returns `PollOs` on Unix hosts other than Linux.

Tests (they construct `PollOs` directly, so they run on Linux too):
- nominal: a UDP pair registered as `Datagram` sends and receives; a
  `File` write then read at offsets round-trips; a pty registered as `Tty`
  takes `Tty::Configure` with 9600 baud, read back through `tcgetattr`;
  `Resolve` on a dial handle completes `Out::Addr`; `Connect` to a local
  listener completes `Out::Handle`, and a `Send` on it is read by the
  listener's peer.
- off-nominal: a `Recv` parks until data arrives, and lands on a later
  `reap`; two parked ops on one fd keep both interests; two `Connect`s on
  one dial handle give two streams; a `Cancel` of a parked read completes
  it `Canceled`.
- failure: an op on a handle `register` never issued completes `Stale`;
  `Connect` to a closed port completes `Os(ECONNREFUSED)`; a `Read` on a
  dial handle is `Unsupported`; `Resolve` on a `Stream` is `Unsupported`;
  a `Gpio` op on the pty fails `Os(ENOTTY)` (Linux) or `Unsupported`;
  every errno pair round-trips.

### Step 5. `UringOs`

Files: new `src/io/uring.rs` (host, Linux), `src/io/os.rs` (`host()` on
Linux), `src/io/tests/uring.rs`.

1. `UringOs { ring: io_uring::IoUring, slots, tables: Tables, fds }`. Its
   own ring, sized from `limits.ops`.
2. Per reactor token, the memory the kernel reads after submit: a socket
   address, a `msghdr`, one `iovec`. Sized from `limits.ops` at `new`.
3. `register` keeps the descriptor in `fds` and returns its handle, as
   poll does. The constructor keeps each dial target as `Dial(addr)` in
   `fds` slots `0..n`.
4. **Dialing** (decision 11). The constructor registers a sparse fixed-file
   table of `limits.ops` slots. `Connect` on a dial handle takes a free
   slot and pushes a `Socket` SQE with that `file_index`, linked
   (`IO_LINK`) to a `Connect` SQE on `Fixed(slot)`. The socket's CQE is
   the `Os`'s own and is dropped, unless it failed: then its error is the
   connect's result. On success the slot is a `Stream` entry in `fds`, and
   later ops on it use `Fixed(slot)`. `Close` frees the slot. `Resolve` on
   a dial handle answers `Out::Addr` inline.
5. The constructor probes for `IORING_OP_SOCKET` and fails with
   `OsError::Unsupported` naming Linux 5.19 when it is missing.
6. The constructor registers each table's `arena()` in `tables` as one
   fixed buffer, indexed by its `TableId`, once. An op on one uses
   `ReadFixed`/`WriteFixed` with that index. A table not frozen at
   construction gets no fixed buffer.
7. `push` builds one SQE per `Op` with `user_data` = reactor token,
   resolving each `BufRef` through `tables`. A full SQ calls `submit()`
   once and retries, then completes `QueueFull`. `Tty` and `Gpio` run
   inline through `os.rs`. `Cancel` is `AsyncCancel`.
8. `submit` calls `submit()`. `reap` drains the CQ into `Cqe`s, mapping
   `res` to `Out::Len`, `Out::Handle` (an accepted or opened fd,
   registered in `fds`), or an errno. Offsets of `u64::MAX` map to
   io_uring's `-1`.

Tests (Linux only):
- the step 4 cases, on `UringOs`;
- an `AsyncCancel` on a read parked on an idle pipe completes the read
  with `Canceled` and the cancel with `Unit`;
- a read into a table given at construction goes out as `ReadFixed`, and
  one into a table added later as `Read`;
- failure: a full fixed-file table completes the connect with `QueueFull`
  and leaks no slot; a stream's `Close` frees its slot for the next
  `Connect`.

### Step 6. stellarator: `Executor::turn` and the sim clock

Files: `libs/stellarator/src/lib.rs`, new `libs/stellarator/src/time.rs`.

1. `time::Clock { Os, Sim }`. `Sim` builds
   `maitake::time::Clock::new(Duration::from_micros(1), sim_now)`, where
   `sim_now` reads a `thread_local! Cell<u64>` of microseconds, the unit
   of fsw-3's `Timestamp`. maitake's `Clock::new` takes a plain `fn`,
   which is why the time is thread-local.
2. `EXEC` holds `Option<Rc<Executor>>`. `Executor::with` and `run` read
   through the `Rc`. `run` takes the executor on return and drops it, as
   today.
3. `Executor::new(clock)` builds today's default reactor and a `Timer` on
   the clock.
4. `Executor::turn`: swaps this executor into `EXEC`, runs one pass
   (`timer.try_turn`, `process_io`, `scheduler.tick`), and swaps the
   previous value back, so several executors can be turned on one thread
   in turn. It never blocks and never calls `sim_set`. The caller that
   owns the clock moves it.
5. `Executor::spawn` installs and spawns the same way, so a caller can
   seed tasks before the first `turn`.
6. Dropping an executor that was turned runs `run`'s teardown: cancel the
   tasks, tick the scheduler once so each drops its future, then drain
   and cancel its IO. Only then does the timer drop ("Drop order").

Tests:
- nominal: `Executor::new(Clock::Sim)` with a 10 ms sleep finishes after
  the test calls `sim_set` past the deadline and turns, with no wall-clock
  wait; two executors turned alternately on one thread each finish their
  own tasks, and a `stellarator::spawn` inside one lands in that one.
- off-nominal: `turn` with nothing spawned returns `runnable: false` and
  no deadline; under `Clock::Sim`, `turn` with a sleeping task leaves
  `sim_now` unchanged and reports the deadline; an executor turned inside
  a `stellarator::run` spawns onto itself, not the outer executor; a
  `stellarator::run` after another on the same thread works.
- failure: `turn` under `Clock::Os` with a sleeping task does not block
  and reports the deadline; dropping an executor with a task still
  sleeping cancels it without a panic, and its `Sleep` is dropped before
  the timer.
- Miri (Linux, `cargo +nightly miri test -p stellarator --lib
  time::tests`): the alternating-executors test and the drop test, to
  check the `EXEC` swap, the `&'static Timer` held by a `Sleep` across a
  swap, and that no `Sleep` outlives its timer.
- Every existing stellarator test passes.

### Step 7. stellarator: `TcpListener::adopt`

Files: `libs/stellarator/src/net/tcp.rs`.

1. `adopt` takes a `std::net::TcpListener`, sets close-on-exec and the
   backend's blocking mode (nonblocking on poll, blocking on uring, as
   `bind` does), and wraps its descriptor.

Tests:
- nominal: a `std::net::TcpListener` bound by std, adopted, accepts a
  `TcpStream::connect`.
- off-nominal: adopting a listener that already has a client queued in its
  backlog accepts it.
- failure: adopting a listener whose descriptor was shut down fails its
  first `accept` with an error, not a panic.

## Exit criteria

- The op types, the buffer table, and their tests build for
  `wasm32-unknown-unknown` without `host`.
- `PollOs` passes its tests on Linux and macOS; `UringOs` on Linux.
- The step 2 and step 3 Miri tests and the step 6 `EXEC` swap and drop
  tests pass.
- The step 3 allocation test shows no allocation in a steady cycle.
- Every existing stellarator and fsw-3 test passes, and the workspace
  builds.

## Versions

- stellarator: the workspace version does not move. Its public API only
  gains `Executor::new`, `turn`, `spawn`, `time`, and
  `TcpListener::adopt`.
- No fsw-3 ABI or config version changes in A.

## Not in this chunk

- fsw-3's `Record` impls, `Io`, `Handle`, `Buf`, `Slot`, `IoInit` (B).
- Translating system handles to `Os` handles, opening and registering real
  resources at build, `io_submit`/`io_complete`, freezing tables in
  `io_submit`'s make, `BufTable::rewind` (C).
- Resolvers that turn a peer name into an address (F). In A, `Resolve`
  works only on a dial handle, which answers its stored address.
- Deterministic thread groups (E). A gives them `Executor::turn` and
  `Clock::Sim`.
- Deferred to `TODO.md` §8: stellarator executing this op type, group
  reactors, async systems taking `Cap<K>`, layers, Windows in the `Os`.
- io_uring fixed files, other than the dial sockets of step 5.
- I2C, SPI, and `tcdrain`.

## Open questions

1. **Inline file IO on poll.** `PollOs` runs file ops inline and blocking.
   This is fine on development hosts. A slow disk stalls the coordinator
   thread there, never in flight, where io_uring runs.
2. **Teardown leaks.** A table whose op is still in flight after three
   drain rounds is leaked, not freed. The alternative is blocking teardown
   on the kernel.
