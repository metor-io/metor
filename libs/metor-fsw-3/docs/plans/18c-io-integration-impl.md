# IO integration: implementation plan (chunk C)

Implements "The `io` system", the real half of "Resources and
capabilities", `io_status`, and the Python side of each, from `18-io.md`.
After this chunk a target file names a `Tty`, grants it to a system, and
the system's ops reach the OS through the coordinator's reactor.

Eight tasks, in order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- `cargo build -p echo-pack --target wasm32-unknown-unknown`;
- the `python/metor-config/tests` suite;
- one commit.

C3 and C8 also build and test `examples/adcs-fsw3`.

Order: C1 adds `MakeCx.io` with no behaviour change. C2 makes a channel
`Reset` or `Load` quiesce, so the remade system reuses its arena. C3 adds
resources to the config. C4 opens real resources at build. C5 adds
`io_submit`, `io_complete`, and `io_status`. C6 and C7 are the Python
builder and stubgen. C8 is the gate.

Links are untouched. `Publish`, `Subscribe`, and `Db` keep their
`Transport` params and their own sockets until chunk F.

## Depends on

From A (`18a-io-stellarator-impl.md`, "Interfaces other chunks use"):

```rust
// crate::io, host
pub trait Os {
    fn push(&mut self, sqe: Sqe);
    fn submit(&mut self);
    fn reap(&mut self, done: &mut Vec<Cqe>);
    fn register(&mut self, fd: OwnedFd, class: FdClass) -> RawHandle;
}
// A dial target is data for the backend's constructor (`io::os::host(.., dials)`);
// dial target `i` has the handle `RawHandle::new(i, 0)`.
pub enum FdClass { Stream, Datagram, Listener, File, Dir, Tty, Can, Gpio }
pub fn host(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError>;

impl Reactor {
    pub fn new(os: Box<dyn Os>, limits: Limits) -> Self;
    pub fn source(&mut self, table: Rc<BufTable>) -> SourceId;   // issued in order from 0
    pub fn push(&mut self, src: SourceId, sqe: Sqe);
    pub fn submit(&mut self);
    pub fn reap(&mut self, done: &mut Vec<(SourceId, Cqe)>);
    pub fn cancel_all(&mut self, src: SourceId);
    pub fn push_own(&mut self, sqe: Sqe);                       // reactor-own token range
    pub fn in_flight(&self, src: SourceId) -> usize;
}
// Drop: cancel, drain rounds, then leak.

// crate::io, core
pub struct Tables;        // Rc-shared; Tables::new(), add(&Rc<BufTable>) -> TableId
pub struct BufTable;      // one arena plus its directory; shared by Rc
impl BufTable { pub fn freeze(&self); pub fn arena(&self) -> Option<NonNull<[u8]>>; /* .. */ }
pub enum BufError { Full, TooLarge, BadSlot, NoSpace { need: usize, free: usize } }
```

From B: `Io`, `IoInit`, `Table`, `MAX_OPS`, `GrantDef { param, interface,
end }` on `SystemDef::grants`, `Grant` on `SystemConfig`,
`fn_system::bind(views, writers, &Tables)`, `DeviceSide` holding a
`Tables`, and `Ctor::make(params, &IoInit)`.

For E: `io_submit` routes each op by its resource's backing through one
match in `SubmitStep`; E adds the `Sim` arm. `SimKind` exists as an enum
with no variants.

## C1. `MakeCx.io`

Files: new `src/io/shared.rs`, `src/coordinator/{table,build}.rs`,
`src/channel/adapter/mod.rs`, `src/pack/mod.rs`, `src/fn_system/tests.rs`
(`MakeCx` sites at `:592` and `:765`), `src/thread/unsupported.rs`
(`MakeCx` site at `:45`).

1. ```rust
   /// What the io steps and the IO systems of one build share.
   #[derive(Default)]
   pub(crate) struct IoShared {
       /// The target's buffer tables. The `Os` and every `DeviceSide` resolve through it.
       pub tables: Tables,
       /// Every system with a `Cap`, in make order.
       pub members: Vec<Rc<Member>>,
   }

   /// One IO system, kept across its makes.
   pub(crate) struct Member {
       pub id: String,
       pub table: Rc<Table>,
       pub quiesce: Quiesce,            // C2
   }
   ```

   `MakeCx` gains one field, `io: &'a mut Option<Rc<RefCell<IoShared>>>`.
   Build owns the `Option` for the whole build. `MakeCx.threads` does not
   change. The pack's `THREADS` thread-local (`pack/mod.rs:96-101`) gains
   a sibling `IO` for the pack's own makes. The channel adapter keeps a
   clone of the `Rc` for its remakes.
2. `IoShared::member(&mut self, id) -> Rc<Member>` creates the member on a
   system's first make, adds its table to `tables`, and returns the same
   member on later makes. `fn_system::bind` takes the member's table
   instead of a fresh `Table`, and `tables` instead of a fresh `Tables`.
   B's fn and sequence makes call `cx.io.get_or_insert_default()` and then
   `member(cx.id)` when the system has a `Cap`; a system with only a
   `DeviceSide` takes `tables` alone. Models run on the coordinator
   thread, so the `Rc` never crosses a thread.
3. Dylib and wasm systems, and `DlSlot`, do not call `member` in C. A
   guest with a `Cap` is refused by `io_submit`'s make (C5.6). G2 makes
   them members.

Tests: every existing test passes. New, in `fn_system/tests.rs`: a
`DeviceSide` made through `MakeCx.io` resolves a `BufRef` of another
system's table that was added after the `DeviceSide` was bound, once that
table is frozen; before `freeze` it is `BadBuf`; two makes of one system
return one member.

## C2. `Reset` and `Load` quiesce

Files: `src/io/{shared,queue,buf}.rs`, `src/fn_system/{mod,set}.rs`,
`src/sequence/mod.rs`, `src/coordinator/{table,params}.rs`
(`ParamError::IoBusy`), `src/channel/adapter/{mod,tests}.rs`,
`src/io/tests/buf.rs`.

A `Reset` or a `Load` cancels the system's ops in flight. The system is
remade, or the new candidate loaded, on the first cycle where none are
left: the next one at best, and a few later at most, since it has at most
`MAX_OPS` ops to cancel. Nothing is in flight then, so the new system
allocates into the same arena. A system with no member acts at once, as
today.

1. **Quiesce state.** `Member` gains:

   ```rust
   pub(crate) struct Quiesce { active: Cell<bool>, src: Cell<Option<SourceId>> }
   ```

   `src` is set by `io_submit`'s make (C5.6). `IoShared` gains three
   methods over a member:
   - `begin(&mut self, m: &Member)`: sets `active`, and calls
     `reactor.cancel_all(src)` when the host part exists;
   - `idle(&self, m: &Member) -> bool`: true with no host part; otherwise
     true once `io_submit` has drained the member's SQ since `begin`
     (C5.10) and `in_flight(src)` is zero;
   - `end(&self, m: &Member)`: clears `active`.
2. **`BufTable::rewind(&self) -> Result<(), BufError>`.** With no slot
   held, it clears the directory, and the next allocations start again at
   the arena's first data byte, in order. The arena keeps its address and
   capacity. A slot still held is `BufError::Busy`. A ctor that asks for
   the same sizes gets the same slots and addresses. One that asks for
   more than the capacity gets `NoSpace`, which B's `IoInit::finish`
   turns into `ParamError::IoMemory` naming the bytes needed and
   available.
3. **Rewind runs where the ctor runs.** `IoShared::member(id)` on a later
   make rewinds the member's table before returning it. The old system
   was dropped first, so no slot is held; `Busy` fails the make with
   `ParamError::IoBusy { system }`. G uses the same rule for guests: the
   pack rewinds, the host only reads.
4. **Channel adapter.** `State` (`channel/adapter/mod.rs:230-236`) gains
   `quiescing: bool`, and `Core` gains `pending: Option<Pending>`, with
   `enum Pending { Restart, Load(String) }`:
   - `Reset` sets `run` to `Stopped`, drops `inner`, calls `begin`, and
     sets `pending = Restart`;
   - `Load` and `restart()`'s reload (`mod.rs:280-288`) keep `inner`,
     call `begin`, and set `pending = Load(name)`. `inner.load` is
     deferred; for a `DlSlot` it drops the old instance before creating
     the new one (`dl/slot.rs:289-306`), so the new ctor rewinds;
   - `apply` (`mod.rs:336-342`) refuses `Start`, `Load`, and `Abort`
     while `quiescing`, with the reason "the system is quiescing". `Stop`
     is accepted; `Reset` replaces `pending` with `Restart`;
   - while quiescing, `step` skips `catch_step`, so the empty slot does
     not read as `Unwound` (`coordinator/step.rs:116-118`) and the
     channel does not end. It checks `idle`; once true, it runs `pending`
     (`restart()` or `inner.load(name)`), calls `end`, and emits
     `Loaded`, or `Failed` or `Refused` as today. No `Loaded` is emitted
     before then;
   - a system with no member runs `pending` at once, so non-IO channels
     behave as today.

   This changes behaviour: on an IO system, a `Reset` or a `Load` takes
   effect after the system's in-flight ops finish.
5. **Old completions.** `io_complete` drops every completion of a member
   whose `quiesce` is active: they belong to a dropped system. The new
   system's `io_cq` view starts at the ring's committed position
   (`ring/src/lib.rs:645-650`), so it never sees earlier records.

Tests:
- `io/tests/buf.rs`: nominal, `rewind` then the same `alloc`s give the
  same slots and pointers; off-nominal, a smaller second round fits inside
  the first's arena; failure, `rewind` with a slot held is `Busy`, and an
  `alloc` past the frozen capacity after `rewind` is `NoSpace` with the
  right `need` and `free`.
- `channel/adapter/tests.rs`, over a `Reactor` on a test `Os`, with the
  member's `src` set by hand:
  - nominal: `Reset` of an IO system with nothing in flight remakes it on
    the next cycle, with its ctor buffers at the same addresses; a
    non-IO system's `Reset` and `Load` act at once, as today;
  - off-nominal: with a read in flight at `Reset`, the remake waits until
    the read's cancel lands; the canceled read never reaches the new
    system; a completion written to the CQ before the `Reset` never
    reaches the new system; `Load` with a read in flight emits `Loaded`
    only after the cancel lands; `Start` while quiescing is refused with
    its reason, and `Stop` is accepted;
  - failure: an op that never completes (a test `Os` that ignores the
    cancel) keeps the system unmade and reports it as quiescing, never
    `Panicked`, without panicking; a remade ctor that asks for more than
    the arena holds fails with `IoMemory` naming both sizes, and the
    adapter reports `Failed`.

## C3. Resources in the config

Files: new `src/io/kind.rs`, `src/coordinator/{config,build,error,table}.rs`,
`src/thread/unsupported.rs` (`:45`), `src/fn_system/tests.rs` (`:592`,
`:765`), `python/metor-config/metor_config/{_config,__init__}.py`.

1. **Types**, in `io/kind.rs` (core, so a pack can name them):

   ```rust
   #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
   pub struct ResourceConfig {
       pub id: String,
       pub backing: Backing,
   }

   #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum Backing {
       Real(RealKind),
       Sim(SimKind),
       Peer(String),
   }

   /// The real resources. Closed: nothing registers more.
   #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum RealKind {
       Tty { path: PathBuf, baud: u32 },
       Can { iface: String, #[serde(default)] filters: Vec<CanFilter> },
       GpioLine { chip: PathBuf, line: u32, direction: GpioDirection },
       Udp { bind: String, #[serde(default)] connect: Option<String> },
       File { path: PathBuf, #[serde(default)] access: Access },
       Dir { path: PathBuf },
       TcpListen { addr: String },
       TcpConnect { addr: String },
   }

   /// Filled by chunk E.
   #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
   pub enum SimKind {}
   ```

   `RealKind::interface(&self) -> &'static str` maps `Tty` to `Uart`,
   `TcpListen` to `Listen`, `TcpConnect` to `Dial`, and every other kind
   to the interface of its own name. There is no resource-kind registry
   in `SystemTable`.
2. `CoordinatorConfig` gains `#[serde(default, skip_serializing_if =
   "Vec::is_empty")] pub resources: Vec<ResourceConfig>`. `MakeCx` gains
   `resources: &'a [ResourceConfig]`.
3. **Build checks** in `build.rs`, beside B's `check_grants`, reading
   `SystemConfig.grants`, the def's `GrantDef`s, and `resources`:
   - resource ids are unique: `BuildError::DuplicateResource { id }`;
   - every grant names a resource: `BuildError::UnknownResource { system,
     param, resource }`;
   - the resource provides the cap's interface: `BuildError::Interface {
     system, param, resource, expected, found }`, the way `IdMismatch`
     checks a port's record;
   - `Sim` and `Peer` backings are refused until E and F:
     `BuildError::Backing { resource, backing }`.
4. **No version bump.** Every new field has a default, so a version-2 file
   decodes unchanged. Goldens change only where a target gains resources;
   none does in C3.

Tests:
- `io/kind.rs`: every `RealKind` round-trips; an unknown kind and a
  `Tty` without `baud` fail to decode; `interface` per kind.
- `build.rs` tests: nominal, a `Tty` granted to a `Cap<Uart>` builds;
  failure, a `Dir` granted to a `Cap<Uart>` is `Interface`; a system that
  declares `Cap<Stream>` is `Interface` naming the parameter and `stream`,
  whatever it is granted, since no kind provides it; an unknown resource
  and a duplicate id fail with theirs, and a `Peer` backing is `Backing`.
- off-nominal: a version-2 config with no `resources` key decodes, and
  every golden is byte-identical.

## C4. Opening real resources

Files: new `src/io/open.rs` (host), `src/coordinator/error.rs`,
`Cargo.toml` (`libc` as a dev-dependency), new `tests/io.rs`.

1. ```rust
   /// A real resource ready for the reactor's `Os`.
   pub(crate) enum Opened {
       /// An opened descriptor, for `Os::register`.
       Fd { fd: OwnedFd, class: FdClass },
       /// A dial target, for the `Os` constructor. Nothing is opened.
       Dial { addr: RawAddr },
   }

   pub(crate) fn open(kind: &RealKind) -> Result<Opened, OpenError>;
   ```

   One short fn per kind:
   - `Tty`: `open(O_RDWR | O_NOCTTY | O_NONBLOCK | O_CLOEXEC)`, then
     `cfmakeraw`, the baud, `CLOCAL | CREAD`, and `tcsetattr`. A baud
     with no `B*` constant is `OpenError::Baud`. Class `Tty`.
   - `Can` (Linux): a `PF_CAN` raw socket bound to the interface's index,
     with `CAN_RAW_FILTER`. Elsewhere `OpenError::Unsupported`. Class
     `Can`.
   - `GpioLine` (Linux): a GPIO v2 line request. Elsewhere `Unsupported`.
     Class `Gpio`.
   - `Udp`: `std::net::UdpSocket` bound, and connected if `connect` is
     set, then made nonblocking. Class `Datagram`.
   - `File` and `Dir`: `OpenOptions` and `O_DIRECTORY`. Classes `File` and
     `Dir`.
   - `TcpListen`: `std::net::TcpListener::bind`, nonblocking. Class
     `Listener`.
   - `TcpConnect`: not opened. Its `addr` is resolved once
     (`ToSocketAddrs`, first result) into a `RawAddr`, and it returns
     `Opened::Dial`. An address that does not resolve is
     `OpenError::Addr`. Each `connect` makes its own socket in the `Os`.
2. `io_submit`'s make (C5) opens every `Real` resource granted to one of
   its members. A failure is `ParamError::Resource { id, message }`, which
   fails the build, and nothing is half-built: every fd opened before it
   is closed by drop.

Tests, in `tests/io.rs` (integration, IO), calling `open` through a
test-only re-export:
- nominal: a `Tty` on a pty slave opens, and `tcgetattr` on it reads back
  raw mode and the baud; `Udp` binds `127.0.0.1:0`; `Dir` and `File` on a
  temp dir; `TcpListen` on port 0; each has the class above; a
  `TcpConnect` to `127.0.0.1:2240` is `Opened::Dial` with that address and
  opens no descriptor;
- off-nominal: `Udp` with `connect` connects; a `File` with `access =
  read` refuses a write at the OS;
- failure: a missing tty path fails with `Resource` naming it; baud 12345
  is `Baud`; a `TcpConnect` to an unresolvable host fails naming the
  resource and the address; on macOS a `Can` resource fails with
  `Unsupported`; a CAN test on `vcan0` runs only on Linux where the
  interface exists.

## C5. `io_submit`, `io_complete`, and `io_status`

Files: new `src/io/builtins.rs` (host), `src/io/system.rs` (host),
`src/io/handles.rs`, `src/io/status.rs`, `src/io/shared.rs`,
`src/link/mod.rs` (one call in `register_builtins`),
`src/coordinator/{build,error}.rs`, `src/lib.rs` (re-exports
`ResourceStatus`).

1. **Types.** `IO_SUBMIT_TYPE = "fsw.io_submit"` and `IO_COMPLETE_TYPE =
   "fsw.io_complete"`. `io/builtins.rs` registers both with
   `table.insert(.., io::system::submit_entry())`, like
   `dl::slot::entry()`; `register_builtins` calls it once. They are
   hand-written `TableEntry`s because their makes read `MakeCx.io`, which
   a ctor cannot. Neither takes params.
2. **Ports.**
   - `io_submit`: inputs are `DynInputs`, one per member, named after it
     and fed by its `io_sq`; outputs are `log`.
   - `io_complete`: outputs are `log`, `io_status`, then one per member,
     named after it, from config `OutputSource::Record("io_cqe")`. They
     bind through `DynOutputs`. `io_cqe`'s `Record::DEPTH` is `MAX_OPS`,
     so each port has that depth. Each system's `io_cq` reads its port
     through a loop edge.
3. **`ResourceStatus`**, the `io_status` record, in `io/status.rs`:

   ```rust
   #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, Default, PartialEq)]
   #[frame(name = "io_status")]
   #[repr(C)]
   pub struct ResourceStatus {
       #[frame(timestamp)]
       pub timestamp: Timestamp,
       pub resource: FrameStr<64>,
       pub submitted: u64,
       pub completed: u64,
       pub failed: u64,
       pub bytes_in: u64,
       pub bytes_out: u64,
   }
   ```

   Its `DEPTH` is 16. `io/status.rs` also holds `Counters`, one per
   resource, indexed by its position in `CoordinatorConfig.resources`, and
   `changed(&self, other)` in the `LinkStatus::changed` idiom
   (`link/mod.rs`).
4. **Handle maps** (`io/handles.rs`). Each member maps its system's
   handles to the `Os`'s:

   ```rust
   pub(crate) struct Handles { slots: Box<[HandleSlot]>, grants: usize }
   struct HandleSlot { os: RawHandle, res: u16, generation: u32, live: bool }
   ```

   Slot `n` is the system's n-th grant. A derived handle takes a free slot
   past the grants when its `Out::Handle` comes back, and inherits its
   parent's resource. The map is sized from `MAX_OPS` plus the grants.
   With no free slot, the completion is rewritten `Refused`, and the new
   `Os` handle is closed with `push_own`.
   The member also keeps, per op in flight, its resource and kind,
   indexed by its token's slot, so `io_complete` can count it.
5. **`IoShared`'s host part**, added behind `host`:

   ```rust
   pub(crate) struct IoHost {
       reactor: Reactor,
       members: Box<[HostMember]>,   // in io_submit's input order
       counters: Vec<Counters>,      // per resource
       reported: Vec<Counters>,      // what io_status last wrote
       early: Vec<(usize, Cqe)>,     // ops io_submit answers itself
   }
   struct HostMember { member: Rc<Member>, src: SourceId, handles: Handles, drained: bool }
   ```

   `IoShared` gains `host: Option<IoHost>`.
6. **`io_submit`'s make**, in order:
   - refuses `thread=` (`refuse_thread`);
   - takes `MakeCx.io` (`ParamError::NoIoSystems` if absent, which means
     no system with a `Cap` was made before it);
   - matches each input port name to a member. An input with no member,
     such as a dylib or wasm system with a `Cap` before G, fails with
     `ParamError::NoIoMember { system }`, whose message says guests take
     caps from chunk G. ADCS has no caps;
   - opens each `Real` resource granted to a member (C4);
   - freezes each member's table. From here on no arena moves, and only
     now are pointers handed to the kernel;
   - builds the `Os` with `io::os::host` over `IoShared::tables` and the
     addresses of every `Opened::Dial`, in order, so dial target `i` has
     the handle `RawHandle::new(i, 0)`. It then hands each `Opened::Fd` to
     `register`, and maps every resource to its `RawHandle`;
   - `Reactor::new(os, limits)`, with `limits.ops` the number of members
     times `MAX_OPS`, then `source(table)` per member in order. Closes and
     cancels use the reactor's own token range (A), so they need no
     source and never take a system's token;
   - fills each member's handle map: for each `GrantDef` with `end: Host`,
     in def order, it finds the config grant of the same `param`, by name,
     and maps slot `n` to that resource's handle. Python emits grants from
     a dict, so their order on the config means nothing;
   - sets each member's `quiesce.src`;
   - stores `IoHost` and binds its inputs.
7. **`io_complete`'s make** reads `IoHost` from `MakeCx.io`
   (`ParamError::NoIoSubmit` if absent) and binds its outputs.
8. **`channel=`** is refused in `resolve` for either type:
   `BuildError::IoAdapter { id }`, since `channel::make` runs before an
   entry's make (`build.rs:473-476`).
9. **Wiring checks** in `build.rs`, the one gate: a config with any `Host`
   grant needs exactly one `io_submit` followed later by exactly one
   `io_complete`; every system with a `Host` grant comes before
   `io_submit`; and `io_submit`'s inputs and `io_complete`'s per-system
   outputs name exactly those systems, in the same order. Each failure is
   `BuildError::IoSystems { message }`. A config without grants needs
   neither, so every existing config builds unchanged.
10. **Steps.**
    - `SubmitStep::execute(now)`:
      1. for each member, in order: if its `quiesce` is active, drains
         its SQ input and drops the records, which belong to the dropped
         system, and sets `drained`; on the first such cycle it also
         pushes `Close` with `push_own` for each derived handle and
         truncates its map to the grants. `end` clears `drained`.
         Otherwise it drains its SQ input, translates each `Sqe`'s handle, records the op's resource
         and kind, counts `submitted` and, for a `Write`, `Send`, or
         `SendTo`, `bytes_out` from the buffer's length, and routes the op
         by its resource's backing: a `Real` op is `push`ed. A stale or
         unknown handle is answered `Stale` in `early`, not pushed. A
         record that does not decode logs one `io_corrupt` fault per cycle
         on `io_submit`'s log;
      2. calls `reactor.submit()`, which hands every op to the kernel this
         cycle.
    - `CompleteStep::execute(now)`: `reap(&mut done)`, with `done` sized at
      make to the number of members times `MAX_OPS`. `reap` already drops
      reactor-own completions; those of a member whose `quiesce` is
      active are dropped here.
      An `Out::Handle` is rewritten to a new slot in the member's map.
      Each completion counts `completed`, `failed` on an error, and
      `bytes_in` for a successful `Read` or `Recv`. Every other completion,
      and every `early` answer, is written to its member's CQ port. Then
      one `ResourceStatus` is written per resource whose counters changed,
      at most 16 a cycle, starting after the last one written. A failed CQ
      write counts toward `publish_dropped`, which `report_dropped`
      (`set.rs:50-55`) reports on `io_complete`'s log. The CQ ring holds
      `MAX_OPS * ring_depth`, so a failed write means a system stopped
      running.
    - Results that land between the two steps reach their system in N+1.
      Results that land later go out on a later cycle's `io_complete`.
    - `fault`: logs `panic` like `FnSystem::fault`.
11. **Teardown** is the reactor's `Drop` (A): cancel, drain rounds, then
    leak.
12. **Thread groups.** Async systems still cannot take a `Cap` (B's
    `IoOnAsync`), so no group needs a reactor.

Tests:
- `io/system.rs` unit tests, over a test `Os` that answers each op on the
  next `reap` with a scripted `Cqe`:
  - nominal: two systems' ops reach the `Os` in step order, with the
    handles `register` gave their grants' resources; completions land on
    the right CQ, and the systems see them the cycle after they
    submitted; an `Accept` completing with an `Os` handle reaches the
    system as a new derived handle, and an op on it reaches the `Os` with
    the `Os` handle;
  - off-nominal: a system that submits nothing costs nothing; a
    quiescing member's completions never reach its CQ, its SQ records
    written before the `Reset` never reach the `Os`, and its derived
    handles are closed with `push_own`; an op on a closed derived handle
    completes `Stale` without reaching the `Os`; grants listed on the
    config in another order than the params still map each param to its
    own resource; every member's table is frozen after the make;
  - failure: a garbage `Sqe` logs `io_corrupt` and the next record still
    submits; a CQ ring that is full logs `publish_dropped` once;
    `thread=` and `channel=` are refused; a grant with no `io_submit`, a
    granted system after `io_submit`, and `io_complete` outputs in another
    order each fail `IoSystems`; an `io_submit` input with no member fails
    `NoIoMember` naming the system; an `Accept` completing when the
    member's handle map is full reaches the system as `Refused`, and the
    `Os` sees a `Close` for the new handle;
  - status: one read and one write give the right counts and bytes; a
    failed read counts `failed` and no bytes; a resource with no ops
    writes no record; 20 changed resources are written over two cycles, 16
    then 4; a name longer than 64 bytes is truncated, not refused;
  - dial: a `TcpConnect` resource reaches the test `Os` through its
    constructor's dial list, never through `register`; a `Resolve` on its
    grant reaches the `Os` with the dial target's handle;
  - allocation: 1000 cycles after build allocate nothing on the submit
    and complete paths (counting allocator).
- `tests/io.rs`: a target with a `Udp` resource granted to a fixture
  system that sends to itself: the datagram comes back as a completed
  `recv` within a few cycles on the host `Os`, and `io_status` reports it.

## C6. Python: resources, grants, io emission, and `ResourceStatus`

Files: new `python/metor-config/metor_config/_io.py`, `_target.py`,
`_model.py`, `__init__.py`, new `python/metor-config/tests/test_io.py`.

1. `_io.py`:
   - kinds as small classes with `interface` and `to_json()`: `Tty(path,
     baud=9600)`, `Can(iface, filters=())`, `GpioLine(chip, line,
     direction="input")`, `Udp(bind, connect=None)`, `File(path,
     access="read_write")`, `Dir(path)`, `TcpListen(addr)`,
     `TcpConnect(addr)`;
   - `ResourceHandle(name, kind, target)`;
   - `class ResourceStatus(Record)` with `_name = "io_status"` and `_kind =
     "frame"`. `__init__.py` exports it.
2. `Target.resource(name, kind) -> ResourceHandle` raises `ConfigError` on
   a repeated name or a name that is not an identifier.
3. `System.__init__` takes `grants: dict[str, ResourceHandle]`. At `add`
   time, each grant must be a handle of the same target, and its
   `interface` must match the class's `_caps[param]`; every name in
   `_caps` must be granted. Each raises `ConfigError` naming the system
   and param.
4. `_system` emits `"grants": [{"param", "resource"}]` when the system has
   any. A system with grants gets an `io_cq` input from `io_complete`'s
   port named after it.
5. **Emission.** When any system has grants, `to_config` appends, after
   the last added system:
   - `{"id": "io_submit", "ty": "fsw.io_submit", "inputs": [{"port": id,
     "from": [{"system": id, "port": "io_sq"}]} ..]}`;
   - `{"id": "io_complete", "ty": "fsw.io_complete", "outputs": [{"name":
     id, "record": "io_cqe"} ..]}`.

   Both list each granted system in step order. `io_submit` and
   `io_complete` are reserved names for `add`. `coordinator.resources`
   lists every resource, and is left out when there are none, so the ADCS
   envelope does not change.
6. `Publish(all=True)` skips `io_sqe` and `io_cqe` ports.

Tests, in `test_io.py`:
- nominal: one `Tty` granted to one system emits the resource, the grant,
  the `io_cq` loop edge, and the two io systems last, in that order;
  `ResourceStatus` carries `io_status`;
- off-nominal: two systems granted one resource share it; a target with
  no grants emits neither io system nor `resources`;
- failure: a `Dir` granted to a `uart` param, a resource of another
  target, a missing grant, a repeated resource name, and `add("io_submit",
  ..)` each raise `ConfigError`.
- `test_target.py` and `test_deployment.py` goldens match unchanged.

## C7. Stubgen: `Cap` keywords

Files: `src/cli/{module.rs,module.py.jinja}`, `tests/pack_dev.rs`,
`tests/golden/echo_pack.py`.

1. `Entry` gains `caps: Vec<CapKw { name, interface }>`, from
   `PackSystemDef::def.grants` with `end: Host`. `PackSystemDef` itself
   does not change: its `def` already carries the grant defs (B).
2. The template adds `{{ cap.name }}: ResourceHandle` keywords after the
   inputs, sets `_caps = {..}`, and passes `grants={..}` to
   `super().__init__`.
3. `wired_inputs` (`module.rs:175-181`) also skips `io_cq` and device
   request ports; `ports` skips `io_sq` and device reply ports, so no
   `OutPort` is generated for them.
4. A cap named like a param or an input is `PackDevError::NameClash`,
   as `entry` (`module.rs:258-272`) does for params.
5. `echo_pack.py` is re-blessed: `Gps(uart=..)` appears.

Tests:
- `module.rs`: a system with two caps renders two keywords and `_caps`;
  a cap and a param of one name is `NameClash`; `io_sq` and `io_cq` are
  not wired or exposed.
- `tests/pack_dev.rs`: the golden matches and type-checks
  (`test_golden_module_type_checks`).

## C8. The gate: GPS on a pty

Files: `tests/fixtures/echo-pack/src/{lib,gps}.rs`, `tests/io.rs`.

1. echo-pack gains `until_fix`, a `#[sequence]` in `gps.rs` that
   completes on the first record on its `fix` input and fails after
   `timeout_s` of cycle time. `gps.rs` names only `metor_fsw_3::` paths
   and its own items, never `crate::` items of echo-pack.
2. `tests/io.rs` includes the driver with `#[path =
   "fixtures/echo-pack/src/gps.rs"] mod gps;` and registers `gps` and
   `until_fix` in the host's table, so their IO goes through the host's
   io steps. echo-pack stays a `cdylib`, and no `Cargo.toml` changes. A
   dylib pack's systems reach the io steps with G.
   - **in-process:** open a pty with `libc::openpty`; build a config with
     a `gps1` `Tty` resource on the slave at 9600 baud, `gps` granted it,
     and `until_fix`; step it on a wall clock while a thread writes GGA
     sentences to the master every 10 ms. Assert `until_fix` ends
     `Completed`, and that the master read `PMTK_RATE_10HZ` exactly once.
   - **off-nominal:** a silent pty: `until_fix` fails with its timeout;
     `gps` itself never faults.
   - **failure:** the master closed before the run: reads complete with
     an error, `gps` logs `gps_read` faults, and nothing panics.
3. **ADCS.** `examples/adcs-fsw3/target.py` is unchanged, builds, runs,
   and its tests pass. Its envelope is byte-identical.

## Exit criteria

- C8 passes on Linux (io_uring) and macOS (poll).
- `examples/adcs-fsw3` builds and runs unchanged.
- Every existing fsw-3 and Python test passes.
- The link tests (`tests/link.rs`, `tests/deploy.rs`) pass untouched:
  links keep their own sockets.

## Versions

- No `CONFIG_VERSION` bump: every new field defaults, so version-2 files
  decode unchanged. Chunk F bumps it.
- ABI stays 9. `SystemDef::grants` is skipped when empty. Chunk G bumps
  it.
- `io_status` is a new record name.

## Not in this chunk

- Layers, and layers installed from a target file (later, `TODO.md` §8).
- `SimKind` variants, the `Sim` arm of the io steps' routing, and
  `fsw.sim` placement (E).
- `Backing::Peer`, `via=`, `Dial::connect`, and resolvers (F).
  `TcpConnect` opens nothing at build; it reaches the `Os` as a dial
  target.
- A reactor per thread group, and `Cap` on async systems (deferred,
  `TODO.md` §8).
- Running `gps` from a dylib pack under `metor run`, and `DlSlot` as an
  io member (G).
- I2C, SPI, and `tcdrain`.

## Open questions

1. **One reactor per thread.** `18-io.md` says the io steps share "the
   coordinator thread's `Reactor`". This plan builds it in `io_submit`'s
   make and leaves the coordinator's stellarator executor with its own
   ring. They never share a ring, which costs one extra io_uring instance.
2. **A cancel the OS never answers** keeps a reset system unmade for
   good. C2 reports it as quiescing. This is deferred, and tracked in
   `TODO.md`: a quiesce timeout.

## Risks to check early

- C4: termios differs between Linux and macOS (`speed_t` and the `B*`
  set). Test both in CI before C5.
- C5: the coordinator thread owns every `Rc` in `IoShared`. Check that
  neither backend clones `Tables` onto another thread.
