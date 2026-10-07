# IO surface: implementation plan (chunk B)

Implements "A system" and "Resources and capabilities" of `18-io.md`, up to
the queue pair. A system gets `Cap<K>`, `Io`, `Handle<K>`, `Pending<O>`,
`Slot<O>`, `Buf`, and `IoInit`. Nothing here executes an op. `test::cap`
executes them in tests, against a model the test steps by hand.

Eight tasks, in order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- `cargo build -p echo-pack --target wasm32-unknown-unknown`;
- one commit.

T3 and T7 also run `cargo +nightly miri test -p metor-fsw-3
--no-default-features --lib -- io::queue io::slot io::test`. Without
`host`, no FFI test is selected.

Order: T1 adds the record impls. T2 adds config fields
with no behaviour change. T3 to T5 build the queue, then handles, then the
params. T6 reorders make so `IoInit` reaches the queue. T7 adds the test
harness, and T8 is the exit gate.

Everything in this chunk is core (no `host` gate), so packs and wasm guests
get it. No task adds a re-export at an old path.

## Depends on (chunk A)

B needs only the op types and buffer table that A adds to fsw-3's core
(`src/io/{op,out,addr,handle,buf}.rs`), as `18a-io-stellarator-impl.md`
fixes them. It never names `Reactor`, `Os`, or stellarator, so it builds
without `host`.

```rust
// crate::io, core. Fields are zerocopy little-endian types; constructors and getters hide them.
pub struct Sqe { pub token: Token, pub handle: RawHandle, pub op: Op, .. }   // 48 bytes
pub struct Cqe { pub token: Token, pub status: IoStatus, pub out: Out, .. }  // 40 bytes
// Both: Copy + IntoBytes + TryFromBytes + KnownLayout + Immutable + Unaligned.

pub struct Token(..);         // Token::new(u32), .get(); B packs slot and generation into the u32
pub struct RawHandle(..);     // RawHandle::new(index, generation), .index(), .generation()
pub struct BufRef(..);        // BufRef::new(slot), .slot(); the reactor stamps the table
pub struct IoStatus(..);      // IoStatus::OK, IoStatus::err(IoError), .result()
pub enum IoError { Os(Errno), QueueFull, Stale, Canceled, Refused, Device, Unsupported, BadBuf, BadAddr }
pub enum Op { .. }            // Op::read(buf, Option<u64>), write, recv, send, open, tty, gpio, cancel; ACCEPT, CLOSE, FSYNC
pub enum Out { .. }           // Out::as_len(), as_handle(), as_addr()
pub enum TtyOp { Configure(TtyConfig), Flush { queue, .. } }
pub enum GpioOp { Get { .. }, Set { value, .. } }

pub struct BufTable;          // one arena plus its directory; shared by Rc; every method takes &self
impl BufTable {
    pub fn new(slots: usize) -> Self;
    pub fn alloc(&self, cap: usize) -> Result<u16, BufError>;   // appends; NoSpace once frozen and full
    pub fn alloc_from(&self, bytes: &[u8]) -> Result<u16, BufError>;
    pub fn release(&self, slot: u16);
    pub fn set_len(&self, slot: u16, len: u32) -> Result<(), BufError>;
    pub unsafe fn filled(&self, slot: u16) -> &[u8];
    pub unsafe fn filled_mut(&self, slot: u16) -> &mut [u8];
    pub unsafe fn whole_mut(&self, slot: u16) -> &mut [u8];
}

#[derive(Clone)]
pub struct Tables;            // Rc-shared; Tables::new(), add(&Rc<BufTable>) -> TableId, raw(BufRef) -> Option<RawBuf>
```

A read fills up to a slot's capacity and a write sends its filled length,
so a `BufRef` carries no length. A `Buf` is a slot index, never a
pointer, so the arena may move while the system is made. B never freezes
a table; C's `io_submit` does.

## Data first

```rust
// src/io/mod.rs
pub const IO_SQ: &str = "io_sq";
pub const IO_CQ: &str = "io_cq";
/// Ops one system may have in flight. Also the `Record::DEPTH` of `Sqe` and `Cqe`.
pub const MAX_OPS: usize = 16;

/// One `Cap<K>` parameter mapped to a resource, from a system's config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Grant { pub param: String, pub resource: String }

/// A `Cap<K>` or `DeviceSide<K>` parameter, as the definition lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantDef {
    pub param: Cow<'static, str>,
    pub interface: Cow<'static, str>,
    pub end: End,                     // Host (Cap) or Device (DeviceSide)
}
```

```rust
// src/io/queue.rs: one per system instance, shared by Rc.
#[derive(Clone)]
pub struct Io(Rc<Queue>);

struct Queue {
    sq: Output<Sqe>,                       // its own drop counter; see T3
    state: Cell<Option<Box<State>>>,       // lent per call, like Output's scratch
    table: Rc<Table>,
}

struct State {
    cq: Input<Cqe>,
    ops: Box<[OpSlot]>,                    // MAX_OPS entries
    gens: u16,
    next_grant: u16,
}

enum OpSlot { Free, Waiting { buf: Option<Buf> }, Orphan { buf: Option<Buf> },
              Done { status: IoStatus, out: Out, buf: Option<Buf> } }

pub(crate) struct Table { bufs: Rc<BufTable>, shortfall: Cell<usize> }
```

```rust
// src/io/slot.rs (A owns src/io/buf.rs)
pub struct Buf { table: Rc<Table>, slot: u16 }   // not Clone; the length lives in the table
pub enum Slot<O: BufOp> { Idle(Buf), Busy(Pending<O>), Empty }
pub struct IoInit<'a> { io: &'a Io }

// src/io/pending.rs
#[must_use]
pub struct Pending<O: IoOp> { inner: Inner, _o: PhantomData<O> }
enum Inner { Queued { token: Token, io: Io }, Local { status: IoStatus, buf: Option<Buf> }, Taken }

pub trait IoOp { type Output; }          // plus a sealed `finish`
pub trait BufOp: IoOp<Output = (Result<Self::Ok, IoError>, Buf)> { type Ok; }

// src/io/cap.rs (A owns src/io/handle.rs)
pub trait Interface: 'static { const NAME: &'static str; }
pub struct Handle<K> { raw: RawHandle, io: Io, _k: PhantomData<K> }   // Clone
pub struct Cap<K: Interface>(Handle<K>);                              // Deref<Target = Handle<K>>
pub struct DeviceSide<K: Interface> {
    requests: Input<Sqe>,
    replies: Output<Cqe>,
    io: Io,
    tables: Tables,                       // the target's tables; resolves a driver's BufRef
    _k: PhantomData<K>,
}
```

Interior mutability follows `Lent` (`port/mod.rs:79-98`): each `Io` call
takes the boxed state out of its cell and puts it back on drop. Calls never
call user code while they hold it, so a nested take cannot happen. If it
did, the call answers as if the queue were full rather than panic.

## T1. Records for `Sqe` and `Cqe`

Files: new `src/io/record.rs`; `src/io/mod.rs` (A's).

1. `src/io/record.rs`, hand-written, neither `Frame` nor postcard:

   ```rust
   impl Record for Sqe {
       const NAME: &'static str = "io_sqe";
       const MAX_LEN: usize = size_of::<Sqe>();
       const ALIGN: usize = align_of::<Sqe>();
       const DEPTH: usize = MAX_OPS;
       type Read<'a> = Sqe;

       fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
           Ok(self.as_bytes())
       }
       fn decode(bytes: &[u8]) -> Result<Sqe, DecodeError> {
           match Sqe::try_read_from_prefix(bytes) {
               Ok((sqe, _)) => Ok(sqe),
               Err(e) if bytes.len() < size_of::<Sqe>() => Err(DecodeError::Truncated),
               Err(_) => Err(DecodeError::Codec),
           }
       }
       fn schema() -> RecordSchema {
           RecordSchema::msg(Self::NAME, MsgCodec::Other("io.sqe".into()))
       }
   }
   ```

   `Cqe` is the same, named `io_cqe`. The schema is opaque: the ground sees
   a named message it does not decode.
2. `io` re-exports what systems name: `Sqe`,
   `Cqe`, `IoError`, `IoStatus`, `TtyConfig`, and, from later tasks, the
   surface types. The crate root re-exports `Cap`, `Io`, `IoInit`, `Buf`,
   `Slot`, `Pending`, `Handle`, `DeviceSide`.

Tests, in `io/record.rs`:
- nominal: a `Sqe` and a `Cqe` round-trip through `encode` and `decode`,
  and through a ring with `Output`/`Input`;
- off-nominal: trailing bytes are ignored; `MAX_LEN` equals the size, 48
  for `Sqe` and 40 for `Cqe` (a `const` assert);
- failure: a short record is `Truncated`; an invalid `Op` tag is `Codec`;
- the two records register in one table with no `RecordConflict` or
  `MessageIdConflict`.

`decode` is `try_read_from_prefix`; A's step 1 Kani proof already covers
it, so B adds none.

## T2. Config: `grants`

Files: `src/coordinator/config.rs`.

1. `SystemConfig` gains, after `outputs`:

   ```rust
   /// Maps each `Cap<K>` parameter to a resource.
   #[serde(default, skip_serializing_if = "Vec::is_empty")]
   pub grants: Vec<Grant>,
   ```

   It sits on the config, not in `params`, because `Params::decode`
   refuses unknown keys (`coordinator/params.rs:19-31`), and a grant must
   not reach the system's params. `SystemConfig::new` sets `Vec::new()`.
   Skipping the default keeps every golden and the ADCS envelope
   byte-identical; the config version stays 2.
2. `DefCx`, `DefCxOwned`, and the pack `Instance` do not change. A
   system's ports do not depend on its grants, and build reads the grants
   from `SystemConfig` (T5).

Tests:
- `config.rs`: a bare system decodes with no grants and encodes without
  the key; one with grants round-trips; a grant without `resource` fails
  to decode.
- Every existing test passes unchanged.

## T3. The queue, buffers, and pending ops

Files: new `src/io/{queue,pending,slot}.rs`, `src/io/mod.rs`.

1. **`Table`** holds A's `Rc<BufTable>`. `Table::alloc(len)` returns
   `Option<u16>`; when the table refuses it adds `len` to `shortfall`.
2. **`Buf`**:
   - `Deref<Target = [u8]>` gives the filled bytes; `DerefMut` too.
   - `capacity()`, `fill(&mut self, bytes) -> usize` (copies what fits and
     sets the length), `clear()`, `set_len(n)` clamped to capacity.
   - The bytes come from `BufTable::filled` and `filled_mut`. One `unsafe`
     block each, with a SAFETY comment: a slot is reached only through the
     one `Buf` that owns it, and while an op holds the slot the `Buf` sits
     inside the queue, out of reach.
   - `Drop` releases its slot.
   - `buf_ref()`: the `BufRef` an op carries.
3. **`Io`**:
   - `Io::new(sq: Output<Sqe>, cq: Input<Cqe>, table: Rc<Table>)`.
     `ops` is allocated here, `MAX_OPS` long. Nothing else allocates later.
   - `pump(&self)` drains `cq`. A `Cqe` whose token matches a `Waiting`
     slot becomes `Done`; an `Orphan` is freed, which drops its buffer; an
     unknown or stale token, or a record that does not decode, is
     skipped.
   - `submit(&self, raw, op, buf) -> Inner`: pumps, then takes a free
     slot, bumps `gens`, writes the `Sqe`. A free slot missing, or the ring
     refusing the write, gives `Inner::Local { QueueFull, buf }`, and the
     slot (if taken) is freed. The op's error path is the only report.
   - `take(&self, token) -> Option<(IoStatus, Out, Option<Buf>)>` pumps
     and takes a `Done` slot.
   - `orphan(&self, token)`: `Waiting` becomes `Orphan`; `Done` is freed.
   - `next_grant(&self) -> RawHandle`: slot `n` for the n-th grant, gen 0.
   - `raw(&self)` returns `(Token, RawHandle, Op)` building blocks for the
     base API: `submit_raw(raw, op, buf) -> Pending<Raw>` is the "`io:
     &Io`" surface `18-io.md` names.
4. **`QueueFull` and `publish_dropped`.** `sq` is built with
   `Output::new`, whose counter nothing reports. A full SQ ring is not
   counted toward `publish_dropped`: the op already fails with
   `QueueFull`, and counting it as well would report one fault twice.
5. **`Pending<O>`**:
   - `try_take(self) -> Result<O::Output, Self>` and `cancel(&self)`,
     which submits `Op::Cancel { target }` as an op of its own whose
     result is dropped.
   - `poll(&mut self) -> Option<O::Output>`, which leaves `Taken` behind.
   - `impl Future`: `poll` returns `Ready` once the result is in, and
     `Pending` otherwise. It registers no waker; a sequence's step polls
     every cycle.
   - `Drop`: a `Queued` op is orphaned; a `Local` drops its buffer.
6. **Op markers** in `pending.rs`, with `IoOp::Output`:

   | Marker | Output |
   |---|---|
   | `Read`, `Write`, `Recv`, `Send` | `(Result<usize, IoError>, Buf)` |
   | `Open` | `Result<Handle<File>, IoError>` |
   | `Accept` | `Result<Handle<Stream>, IoError>` |
   | `Close`, `Fsync`, `Tty` | `Result<(), IoError>` |
   | `Gpio` | `Result<u32, IoError>` |
   | `Raw` | `(IoStatus, Out, Option<Buf>)` |

   A completion whose `Out` does not fit its marker (a `Read` that
   completes with `Handle`) is `IoError::Device`. A successful read sets
   the buffer's length to `Len(n)`, clamped to capacity.
7. **`Slot<O: BufOp>`**: `take_idle(&mut self) -> Option<Buf>` leaves
   `Empty`; `poll(&mut self) -> Option<Result<O::Ok, IoError>>` puts the
   buffer back as `Idle`; `bytes(&self) -> &[u8]` is the idle buffer's
   bytes, or empty.
8. **`IoInit<'a>`**: `alloc(len) -> Buf` and `alloc_from(bytes) -> Buf`,
   both infallible. When the table refuses (a frozen arena with too
   little room, on a remake in C or G), or there is no table at all
   (`IoInit::empty`, for a system with no queue), it returns a
   zero-capacity `Buf` and records the shortfall. `finish(self) ->
   Result<(), ParamError>` turns a shortfall into `ParamError::IoMemory {
   need, reason }`, where `reason` names the bytes needed and available,
   or says the system has no queue. `IoInit` is built only by make (T6), so steady state never
   allocates IO memory.

Tests, in `io/queue.rs` and `io/slot.rs`, over in-memory rings from
`tests::utils::ring`, with the test playing the reactor by hand:
- nominal: a read submits one `Sqe` with the buffer's `BufRef`; a `Cqe`
  for its token completes it next pump, hands the buffer back with
  `Len(n)` bytes; a write's buffer comes back unchanged; `Slot` goes Idle,
  Busy, Idle;
- off-nominal: `MAX_OPS + 1` ops in flight make the last one complete
  `QueueFull` with its buffer; a dropped `Pending`'s completion frees its
  entry when it lands, not before; a stale token or a duplicate `Cqe` is
  skipped; `cancel` writes a `Cancel` naming the token; a completion
  never arrives during the call that submitted;
- failure: an SQ ring with no room gives `QueueFull` without bumping any
  drop counter; a garbage record on `cq` is skipped and later records
  still land; an `Out` that does not fit its marker is `Device`;
- allocation: with the counting allocator `tests/wasm_alloc.rs` uses,
  1000 submit/complete rounds after `Io::new` allocate nothing;
- Miri: `Buf` deref while another `Buf` of the same table is written;
  drop order of an `Io` with orphaned ops and live `Buf`s;
- Kani `token_slots_bounded`: any sequence of submit, complete, orphan,
  and take never holds more than `MAX_OPS` slots and never hands one
  token to two live ops. Under `cfg(kani)`, `MAX_OPS` is 4
  (`#[cfg(kani)] pub const MAX_OPS: usize = 4;` beside the normal one),
  so the proof stays small.

## T4. Handles and interfaces

Files: new `src/io/cap.rs`, `src/io/mod.rs`.

1. Interfaces, as unit structs implementing the one `Interface` trait:
   `Uart`, `Can`, `Udp`, `Dir`, `Listen`, `Dial`, `GpioLine`, `File` (granted
   by the real `File` kind, or derived by `Dir::open`), and `Stream`, which
   only derived handles have. `NAME` is the lower-snake name (`"uart"`);
   build and stubgen compare by it. No resource kind provides `stream`, so
   C3's interface check refuses a `Cap<Stream>` parameter, naming the
   parameter and the missing interface.
2. `Handle<K>` methods by interface, each returning a `Pending`:

   | Interface | Methods |
   |---|---|
   | `Uart` | `read(buf)`, `write(buf)`, `configure(TtyConfig)`, `flush(TtyQueue)` |
   | `Can`, `Udp`, `Stream` | `recv(buf)`, `send(buf)` |
   | `File` | `read(buf, offset)`, `write(buf, offset)`, `fsync()` |
   | `Dir` | `open(path: Buf, flags)` |
   | `Listen` | `accept()` |
   | `GpioLine` | `get()`, `set(u32)` |
   | any | `close()` |

   `Dir::open` takes the path in a `Buf` because ops carry no pointers.
   `Dial::connect` is left to chunk F, which issues `Resolve` and then
   `Connect`.
3. `Cap<K>` derefs to `Handle<K>`. It cannot be built outside the crate
   except through `test::cap`.

Tests, in `io/cap.rs`, over the T3 harness: each method writes the
`Op` the table above implies, with the handle's `RawHandle`; `open`
completing with `Out::Handle` gives a `Handle<File>` on the same queue
whose ops carry the new raw handle; `close` on a handle whose op completes
`Stale` returns `Err(Stale)`.

## T5. `Cap<K>` and `DeviceSide<K>` as params

Files: `src/io/param.rs` (new), `src/fn_system/{param,set,mod}.rs`,
`src/system.rs`, `src/def.rs`, `src/coordinator/{build,error,table}.rs`,
`src/sequence/mod.rs`, `src/channel/adapter/mod.rs` (keeps `grants` through
`def`), `src/log.rs` (no change; `fault` takes `Into<Cow>`).

1. **`Defs`** (`fn_system/param.rs:76-86`) gains `io: bool` and `grants:
   Vec<GrantDef>`.
2. **`Param for Cap<K>`**: `In = ()`, `Out = Cap<K>`, `Item<'a> = &'a
   Cap<K>`. `append_defs` takes the name, pushes a `GrantDef` (`end:
   Host`), and sets `io`. It declares no port, so
   several caps never repeat one. `bind_out` takes the next grant from
   `writers.io`. PANIC Safety: `bind` (step 5) gives a queue to every set
   whose `Defs::io` is set.
3. **`Param for DeviceSide<K>`**: `Out = DeviceSide<K>`, `Item<'a> = &'a
   mut DeviceSide<K>`. `append_defs` pushes a `GrantDef` with `end:
   Device`. It does not set `io`: a model system gets no queue pair of its
   own. Its two ports, an input `{param}.req` of `Sqe` and an
   output `{param}.rep` of `Cqe`, are appended by the sets (step 4). `bind_out` takes
   the next prebuilt `DeviceSide` from `writers.devices`. Its methods in
   B are the raw ones: `requests(&mut self)`, `bytes(&mut self, BufRef)
   -> Result<&mut [u8], IoError>`, and `reply(&self, Cqe) -> Result<(),
   SendError>`. `bytes` takes `&mut self`, so two slices from one
   `DeviceSide` never coexist. `serve` is chunk E's.
   - A request's `BufRef` names the driver's buffer, stamped with the
     driver's table by `io_submit`. `bytes` resolves it through the
     target's `Tables`, which the `DeviceSide` holds. A `BufRef` that does
     not resolve is `Err(IoError::BadBuf)`.
   - The one `unsafe` block in `bytes` has a SAFETY comment: the slot is
     lent to this request until its reply is written, models run on the
     coordinator thread, and no one else touches a lent slot.
   - Model systems in wasm or dylib guests are out of scope for v1. A
     guest cannot reach the host's `Tables`.
4. **Port order.** `Defs::walk` (`param.rs:109-112`) appends the IO
   ports right after `append_defs`, so `Defs::of` and `Defs::declared`
   both list them, and `declared` includes `io_sq` and `io_cq`. They
   follow everything the params declare and come before `log`, so
   `Bindings` (`param.rs:25-55`) is untouched and `log` stays the last
   output, which `OutSet::bind`'s PANIC Safety comment relies on
   (`fn_system/set.rs`):
   - inputs: params' inputs, then `io_cq` if `io`, then one request input
     per `DeviceSide`;
   - outputs: params' outputs, then `io_sq` if `io`, then one reply output
     per `DeviceSide`, then `log`.

   `io_sq`, `io_cq`, and the device ports take their depth from
   `Record::DEPTH`, which is `MAX_OPS`. A ring holds `depth * ring_depth`
   records (`build.rs:425-446`), so a system's SQ ring holds `MAX_OPS *
   ring_depth` `Sqe`s: 8 KiB at the default. The queue bounds ops in
   flight by `MAX_OPS` anyway, so the extra depth is slack, not a second
   limit. `Defs::declared` includes `io_sq` and `io_cq`, so `DynInputs`
   never claims the loop edge into `io_cq` (`port/mod.rs:321-341`).
5. **`fn_system::bind::<S>(views, writers, tables: &Tables) -> (InSet<S>,
   OutSet<S>, Option<Io>)`**, used by every fn-system and sequence make:
   - splits the IO tail off both lists, by the counts `Defs::declared`
     gives;
   - builds `Io` from the `io_cq` view and `io_sq` writer, and a fresh
     `Table`;
   - builds each `DeviceSide` from its request view, its reply writer, and
     a clone of `tables`. In B, makes pass a fresh `Tables`; C passes the
     target's, from `MakeCx.io`;
   - binds the params with `Views` and `Writers` that now carry `io:
     Option<Io>` and `devices: VecDeque<DeviceSide<Erased>>`.

   The trait `InSet::bind`/`OutSet::bind` stay for systems without IO.
   They assert `!Defs::declared::<S>().io`, with a PANIC Safety comment:
   every IO system binds through `fn_system::bind`.
6. **`SystemDef`** gains `#[serde(default, skip_serializing_if =
   "Vec::is_empty")] pub grants: Vec<GrantDef>`. `FnSystem::def` and
   `sequence::def` fill it from `Defs`. `PortDef` does not change.
7. **Build checks**, in `resolve` after `check_port_names`
   (`build.rs:119`), as `check_grants(&SystemConfig, &SystemDef)`. They
   read the config's grants against the def's `GrantDef`s:
   - every `Host` cap has exactly one grant: `BuildError::Ungranted
     { system, param }`, `BuildError::DuplicateGrant { system, param }`;
   - every grant names a `Host` cap: `BuildError::UnknownGrant { system,
     param }`.

   A user port named `io_sq` next to a `Cap` already fails
   `check_port_names` (`build.rs:310-319`) as `DuplicateOutput`. Interface
   checks need resources, so they land in C.
8. **Async.** `FnSystem`'s `AsyncSystem::def` (`fn_system/mod.rs:90-97`)
   refuses `Defs::io`, or any `Device` grant, with `DefError::IoOnAsync {
   system }`, next to `SampleOnAsync`. Async systems taking `Cap<K>` or
   `DeviceSide<K>` come later.
9. **`fn_entry`.** `register` stops going through the generic
   `entry::<FnSystem<S>>` (`table.rs:294-317`) and uses `fn_entry`,
   whose make binds through `fn_system::bind` before it builds the
   state. In T5 the state still comes from today's ctor; T6 adds
   `IoInit` to it. `sequence::entry` switches the same way. Without
   this, T5's port tests could not bind an IO system.
10. **Errors in the log.** `impl From<IoError> for Cow<'static, str>`,
    through `IoError`'s `Display`, so `log.fault("gps_read", e)` compiles
    (`log.rs:130-134`).

Tests:
- `fn_system/tests.rs`:
  - a system with two `Cap<Uart>` params has one `io_sq` and one `io_cq`,
    after its own ports, with `log` still last, at depth `MAX_OPS`;
  - `grants` lists both params in order with their interfaces;
  - a system with `DynInputs` and a `Cap` keeps the `io_cq` edge out of
    its dynamic ports;
  - a `DeviceSide<Uart>` named `dev` gives input `dev.req` and output
    `dev.rep`, and no `io_sq` or `io_cq`;
  - an async fn with a `Cap`, and one with a `DeviceSide`, are each
    refused with `IoOnAsync`;
  - a system with a `Cap` registered through `fn_entry` binds, and its
    `io_sq` write reaches the ring;
  - `log.fault("k", IoError::Stale)` writes a line naming `Stale`.
- `build.rs` tests: a granted system builds; a missing grant, a grant to a
  non-cap param, and two grants for one param each fail with their error;
  a system with an `io_sq` param and a cap fails with `DuplicateOutput`.
- Every existing test passes; a system without IO has the same ports as
  before (pinned by `test_port_definition` and the descriptor tests).

## T6. Constructors that take `IoInit`, and bind before make

Files: `src/fn_system/ctor.rs`, `src/coordinator/{table,params,error}.rs`,
`src/sequence/mod.rs`, `src/thread/os.rs` (call site only).

1. **`Ctor`**:

   ```rust
   pub trait Ctor<S, Marker> {
       fn make(&self, params: Params<'_>, io: &IoInit<'_>) -> Result<S, ParamError>;
       fn schema() -> Option<Box<RawValue>>;
   }
   ```

   Every make receives an `IoInit`. The three forms that exist ignore it.
   New impls, with a private marker `WithIo` so they never overlap them:

   | Form | Marker |
   |---|---|
   | `Fn(&IoInit) -> S` | `(WithIo,)` |
   | `Fn(P, &IoInit) -> S` | `(P, WithIo)` |
   | `Fn(P, &IoInit) -> Result<S, ParamError>` | `(P, WithIo, ParamError)` |

2. **Bind before make.** T5's `fn_entry::<S, M, C>` gains the ctor. Its
   make:

   ```rust
   refuse_thread(cx.adapter)?;
   let (views, writers) = bindings(cx.def, cx.inputs, cx.outputs);
   let (inputs, outputs, io) = fn_system::bind::<S>(views, writers, &tables);
   let state = make_state(&ctor, cx.id, cx.params, io.as_ref())?;
   Ok(Box::new(Runner { system: FnSystem::default(), state, inputs, outputs }))
   ```

   `tables` is a fresh `Tables` in B; C makes it the target's, from
   `cx.io`. `make_state` builds `IoInit` over the queue,
   or `IoInit::empty()` when
   the system has none, calls the ctor, then `IoInit::finish`.
   `sequence::entry` (`sequence/mod.rs:156-171`) does the same and hands
   the bound sets to `SequenceStep::new`, which no longer binds (T5
   switched it to bind first). `entry` for `register_system` keeps
   today's order; it has no IO.
3. **No queue, no memory.** A ctor that allocates on a system with no
   `Cap` or `DeviceSide` fails its make with `ParamError::IoMemory`, whose
   message names the system and says it has no queue. Make runs at build,
   so the mistake still surfaces at build. Registration refuses nothing.
4. `ParamError` gains `IoMemory { need: usize, reason }`.

Tests:
- `ctor.rs`: each new form builds with an `IoInit` over a T3 queue; the
  old forms ignore `io`; `Fn(P, &IoInit)` refuses a bad param before
  running.
- `fn_system/tests.rs`: the ctor's `alloc_from` buffer is in the same
  table the system's handle submits from (a write's `BufRef` resolves to
  the ctor's bytes); a ctor that fails leaves the rings' reader and writer
  slots free (build the same make twice over the same rings); a ctor that
  allocates on a system with no cap fails its make with `IoMemory` naming
  the system.
- A trybuild case in `tests/ui`: a ctor `fn(&IoInit) -> S` registers
  without a turbofish.
- The channel adapter's `Reset` remakes an IO system over the same rings;
  the remade system submits and completes. Its table is new in this
  chunk; C makes `Reset` quiesce and reuse it.

## T7. `test::cap`, `test::device`, and `UartModel`

Files: new `src/io/{model,test}.rs`.

B owns `UartModel` and `ScriptedUart`, because its tests need them. E
builds the other model traits and `DeviceSide::serve` on them.

1. `io/model.rs`: `UartModel`, exactly as `18-io.md` gives it, and
   `pub fn serve_uart(model, now, sqe, bytes) -> Reply` where `Reply` is
   `Ready(IoStatus, Out)` or `Later`. This maps `Read`, `Write`,
   `Tty(Configure)`, `Tty(Flush)`, and `Close`; any other op is
   `Unsupported`. Chunk E adds the other model traits and builds
   `DeviceSide::serve` on `serve_uart` and its siblings.
2. `io/test.rs`, public so pack crates can use it in their own tests:

   ```rust
   pub fn cap<K: Interface, M: Model<K>>(model: M) -> (Cap<K>, TestIo<M>);
   pub fn device<K: Interface>() -> (DeviceSide<K>, TestDevice);

   impl<M> TestIo<M> {
       pub fn init(&self) -> IoInit<'_>;
       /// Advances time by `dt`, ticks the model, and answers every queued op.
       pub fn turn(&mut self);
       pub fn model(&self) -> &M;
       pub fn model_mut(&mut self) -> &mut M;
       pub fn submitted(&self) -> &[Sqe];   // every op since the last turn, for asserts
   }
   ```

   `Model<K>` is implemented for `M: UartModel` with `K = Uart`. `TestIo`
   owns an SQ and a CQ ring, a view and a writer on them, and a held list
   for `Later` ops, retried each turn. An op on the cap completes no
   earlier than the next `turn`.

   `TestDevice` plays the driver. It owns a driver `BufTable`, added to a
   `Tables` that the `DeviceSide` shares. `request(op, bytes) -> Token`
   allocates the bytes in the driver table, stamps the op's `BufRef` with
   that table, and writes the request into the device's input ring.
   `replies()` drains its reply ring. `driver_bytes(token)` reads the
   driver's buffer after a reply.
3. `ScriptedUart` in `io/test.rs`: replies from a script of byte chunks
   and records every write. `silent()` never has bytes to read.
   `fail_write(errno)` fails the next write with that errno. A chunk
   longer than the read buffer gives a short read and keeps the rest.
   Tests inject faults through it.

Tests, in `io/test.rs`:
- nominal: a scripted read lands on the turn after it is submitted; a
  write's bytes appear in `written()`;
- off-nominal: a `silent()` read stays pending across 10 turns and lands
  when bytes are scripted; `configure` reaches the model;
- failure: `fail_write(EIO)` completes the next write with `Os(EIO)`,
  the buffer comes back, and the write after it succeeds;
- `test::device`, nominal: a request reaches `DeviceSide::requests`; the
  model reads a driver's write payload through `bytes`; the model fills a
  driver's read buffer through `bytes`, and `driver_bytes` shows it after
  the reply reaches `replies()`;
- `test::device`, off-nominal: two requests on two driver buffers resolve
  to their own bytes;
- `test::device`, failure: a request whose `BufRef` names no table, or a
  released slot, gets `Err(BadBuf)` from `bytes`, and the model replies
  `BadBuf`.

## T8. The GPS driver

Files: `tests/fixtures/echo-pack/src/{lib,gps}.rs`,
`tests/golden/echo_pack.py`.

1. `gps.rs`: the `gps` system and `GpsState::new(&IoInit)` from
   `18-io.md`, verbatim, with a minimal GGA parser (`Nmea`) and a `Gps`
   frame. `register("gps", gps(), GpsState::new)`.
   `tests/golden/echo_pack.py` is re-blessed for the new class. It has no
   `uart` keyword until chunk C's stubgen.
2. Unit tests in `gps.rs`, through `test::cap::<Uart>`:
   - nominal: three turns write `PMTK_RATE_10HZ` once, and a scripted GGA
     sentence becomes one `Gps` record on `fix`;
   - off-nominal: a sentence split across two reads gives one fix; a bad
     checksum gives none;
   - failure: `ScriptedUart::silent().fail_write(EIO)` gives one
     `gps_config` fault on the log, and the command is written again on a
     later cycle.

Gate:
- the GPS tests pass with no reactor and no OS;
- every existing fsw-3 test passes, and `examples/adcs-fsw3` builds and
  its tests pass;
- `echo-pack` builds for wasm32 with `gps` in it.

## Exit criteria

- T8's gate holds.
- `grep -rn "unwrap()" src/io` prints nothing; every `expect` has a PANIC
  Safety comment.
- No allocation after `Io::new` on the submit and complete paths (T3).

## Versions

- No ABI bump: `SystemDef::grants` defaults and is skipped when empty.
  Chunk G bumps to 10.
- No config version bump: `SystemConfig::grants` is skipped when empty.
  Chunk F bumps to 3.

## Not in this chunk

- `io_submit`, `io_complete`, the reactor, resources, and `MakeCx.io`
  (C). Until C, nothing drains an `io_sq` in a built target, so a
  granted system builds but its ops never complete.
- Interface checks against resources (C).
- The target's shared `Tables`, which `DeviceSide` resolves drivers'
  buffers through in a built target (C).
- Model systems in wasm or dylib guests (out of scope for v1).
- `Reset` quiescing and the remade system reusing its table's memory (C).
- `DeviceSide::serve` and every model trait but `UartModel` (E).
- `Dial::connect` and `Resolve` (F).
- The ABI (G).
- `Cap` on async systems (deferred; TODO §8).

## Open questions

None.

## Risks to check early

- T6: inference with the `WithIo` impls. If `register("gps", gps(),
  GpsState::new)` is ambiguous, the trybuild case in T6 fails first.
- T5: the IO ports sit before `log`. Code that finds `log` by index or
  name: `OutSet::bind` (`set.rs:86-108`), `Mirrors::log_port`, and the
  wasm step. Grep for `outputs.last()` and `LOG_PORT` before T5.
