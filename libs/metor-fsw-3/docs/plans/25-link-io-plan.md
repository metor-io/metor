# 25: Links on the IO queue, plan

Implements `25-link-io.md`. Chunks A–H replace today's links with the cycle
driver. Chunks I and J add the thread driver and are outlined only; each
gets its own plan when it starts.

Each chunk lands as one PR, gated on `cargo test`, `cargo clippy`, the Kani
proofs it touches, and `pytest` for chunks that change Python. A chunk ends
with an antagonistic review before the next one starts. G and H land as one
PR, since the integration tests need both.

Order: A → B → C → D → E → F → G+H. A, B, and E do not depend on each
other and can be written in parallel.

## Facts the plan relies on

- `Op` is the decoded `Sqe`. `fills`, `drains`, and `opens` drive buffer
  checks in `Io::queue_op` and the dispatch (`io/wire.rs`).
- The dispatch indexes ops as `m * MAX_OPS + s` (`Dispatch::op_index`,
  `raise`, `quiesce_member`, `member_idle`). `MAX_OPS` also sizes
  `Queue::ops`, `Member::derived`, `SQ_PER_PASS`, `held_for`, the backend
  in `IoState::build`, the wasm guest's read table, and the test harness.
- `Out::Opened(OwnedFd, Class)` is the only way a backend hands back a new
  descriptor. `UringOs` and `sys::accept` pass null address pointers.
- `SystemDef::io` is set by `Defs::append_io` (`fn_system/param.rs`) and
  offset by `sequence::def`. `DefCx::params` is the instance's params.
- Python's `Target._steps` already places `io_submit` and `io_complete`
  when any system declares `_caps`. A link that declares `_caps` gets them
  with no new primitive.
- `deploy` edits the envelope as JSON. It reads a link's listen address from
  `params.transport` and writes a resolved `Transport::Connect` back.

## A. `Buf` spans

Files: `io/arena.rs`, `io/ops.rs`.

- Add `start: u32` to `Buf`: the first filled byte, relative to the span.
  `deref` and `span_for` read from `start`. Construction sets it to 0.
- `advance(n)`: `start += min(n, len)`, `len -= that`.
- `extend(&[u8]) -> usize`: copies into `start + len..capacity`, returns
  the bytes taken.
- `clear()` also sets `start = 0`; `fill` is `clear` then `extend`.
- `spare(&self) -> usize`: `capacity - start - len`.
- `split_off(&mut self, at: usize) -> Option<Buf>`: the tail of the span
  from `at`, as an empty `Buf`. `None` if `at` is past the filled bytes'
  end or beyond the capacity. The head keeps `start..at`.
- `join(&mut self, tail: Buf) -> Result<(), Buf>`: rejoins a tail whose
  span starts where this one ends, in the same arena and epoch. Hands
  `tail` back otherwise.
- `moved` in `ops.rs` sets `len` relative to `start` on a fill. A fill
  always names the whole span, so a fill resets `start` first.

Tests (`io/tests/arena.rs`):

- Nominal: extend to capacity, advance past part, `span_for` names the
  tail; split, fill both, join, read the whole.
- Edge: advance past `len` clamps; extend into a full buffer takes 0;
  split at 0 and at capacity.
- Failure: join a non-adjacent span, a span from another arena, a span from
  an earlier epoch; each hands the tail back unchanged.

Kani (`arena.rs` proofs):

- For any `start`, `len`, and `advance`/`extend` sequence, `span_for` stays
  inside the original span.
- `split_off` then `join` gives back the original span.

## B. Per-system op budget

Files: `io/mod.rs`, `io/queue.rs`, `io/host/dispatch.rs`,
`io/host/steps.rs`, `io/host/mod.rs`, `io/test.rs`, `fn_system/param.rs`,
`sequence/mod.rs`, `coordinator/table.rs`, `coordinator/build.rs`,
`wasm/io.rs`.

- `IoPorts { cq, sq, ops: u16 }`. `DEFAULT_OPS = 16` replaces `MAX_OPS`
  outside Kani; Kani keeps 4. `append_io` sets `ops: DEFAULT_OPS`.
- `TableEntry::set_ops(f: fn(&DefCx) -> Result<u16, DefError>)` wraps
  `def_fn` so a type sets its budget from its params, and sizes `io_sq` to
  `Sqe::depth(ops)`. It fails with `DefError::NoIo` on a def with no `io`.
  `SystemTable::get_mut` reaches a registered entry.
- The CQ keeps `Cqe::DEPTH`. `held_for` scales with the budget, so a burst
  past the CQ waits a cycle instead of being lost.
- `Queue::ops: Box<[Cell<OpSlot>]>`, sized from `IoPorts::ops` in `Io::new`.
- `IoState::build` takes each member's budget alongside its grants.
  `Dispatch` keeps `Slots { base: Box<[usize]> }`, the prefix sum of
  budgets. `index(m, slot)` is `base[m] + slot` when `slot < ops[m]`.
  `owner(index)` finds `m` by `partition_point` over `base`. `Member::derived` takes the member's
  budget. The `u16::MAX + 1` check runs over the sum.
- `SQ_PER_PASS` and `held_for` scale by each member's budget.
- `wasm/io.rs` keeps a fixed `DEFAULT_OPS` table; a guest's def never sets
  a budget.
- `test::cap_with_ops::<K>(ops)`; `cap::<K>()` calls it with the default.

Tests:

- Nominal: two members with budgets 16 and 40 each fill every slot; every
  completion reaches its own member (`io/tests/dispatch.rs`).
- Edge: a member with budget 1; the last slot of the last member.
- Failure: budgets summing past `u16::MAX + 1` fail the build with
  `TooManyIoOps`; `with_ops` on a type with no `Cap` fails its def; a
  slot past the member's budget is dropped as `Rejected`.

Kani: replace `index_never_aliases` with a proof over two members of any
budgets: indexes never alias, and `owner` and `base` give back the member
and slot.

## C. `Endpoint`, dialing, and the allowlist

Files: `io/cap.rs`, `io/ops.rs`, `io/wire.rs`, `io/config.rs`,
`io/status.rs`, `io/host/{mod,dispatch,open,sys,poll,uring}.rs`,
`coordinator/build.rs`, Python `_io.py`.

Interface and op:

- `Endpoint` (`"endpoint"`) replaces `Listen`. `OpKind::Accept` becomes
  `OpKind::Stream`; `Op::Stream` `fills()` and `opens()`.
- `Handle<Endpoint>::stream(peer: Buf) -> Pending<StreamOp>`.
  `StreamOp::Output = (Result<Handle<Stream>, IoError>, Buf)`.
- `RawAddr` gets `RawAddr::LEN`; a `peer` shorter than it is `BadBuf` at
  submit.

Resources:

- `RealKind::TcpListen { addr, #[serde(default)] allow: Vec<Cidr> }` and
  `RealKind::TcpDial { addr: SocketAddr }`. Both report `"endpoint"`.
- `Cidr { addr: IpAddr, prefix: u8 }`, parsed from `"a.b.c.d/n"` or an
  IPv6 form, with `contains(IpAddr) -> bool`. An IPv4-mapped IPv6 peer
  matches an IPv4 block. A prefix past the family's width fails at parse.
- `Backed::Dial(SocketAddr)`: opens nothing at build. `Target::Dial` lowers
  `Op::Stream` to an `OsOp` with `class: Class::Dialer` and `fd: -1`, and a
  new `OsOp::dial: Option<SocketAddr>`.

Backends:

- `Out::Opened { fd, class, peer: Option<SocketAddr> }`.
- `sys::accept` passes a `sockaddr_storage` and returns the peer.
- `sys::connect(addr)`: nonblocking `socket`, `connect`. `PollOs` parks an
  `EINPROGRESS` connect on writability, then reads `SO_ERROR`.
- `UringOs`: accept writes into the op's `Msg::addr` and a new
  `Msg::addr_len`; reap converts it. A dial creates the socket in `push`
  and submits `opcode::Connect` from `Msg::addr`; a failed connect closes
  the socket. `accepts` becomes `opens: Box<[Option<Opening>]>`, which
  holds the dial socket until the connect lands.
- A wire answers `Op::Stream` with `Unsupported` until `SimListen` exists.
- `PollOs` keeps each dial's socket in `dialing` until the connect lands;
  `sys::connected` reads `SO_ERROR`, then `getpeername` to tell a finished
  connect from a spurious wake.
- The wasm `IoCopy` sizes its read table from the guest's budget, and
  copies a `Stream`'s whole peer span on success.

Dispatch:

- `InFlight::stream` keeps a `Stream`'s lowered `OsOp`, to write its peer
  and to lower it again. `Op::min_buf` (`RawAddr::LEN` for a `Stream`) is
  checked at submit and again in the dispatch.
- `raise` on `Out::Opened` checks the resource's `allow`. A rejected peer:
  `Route::Close(fd)`, `counters.rejected += 1`, and the same `OpId` is
  pushed again as a fresh `Route::Os`. An allowed peer has its `RawAddr`
  written into the op's buffer, then is derived as today.
- `ResourceStatus` gains `rejected: u64`; `Counters` with it.

Build: the grant check compares `Backing::interface()` as today. A grant of
`tcp_listen` to a `Cap<Listen>` no longer compiles, since `Listen` is gone.

Python: `TcpListen(addr, allow=())`, `TcpDial(addr)` in `_io.py`; `allow`
is emitted only when non-empty.

Tests:

- Nominal (`io/tests/dispatch.rs`, `tests/io_poll.rs`, `tests/io_uring.rs`):
  accept reports the client's address; dial connects and reports the
  server's; both resolve to working streams.
- Edge: an empty `allow` admits every peer; a `/32` block admits only its
  host; a v4-mapped v6 peer matches a v4 block.
- Failure: a rejected peer is closed, `rejected` counts it, and the next
  allowed peer completes the same op; a dial to a closed port fails
  `ConnRefused` and leaks no descriptor; a short `peer` buffer is `BadBuf`;
  `Op::Stream` on a wire is `Unsupported`; a malformed CIDR fails config
  parse.

Kani: `Cidr::contains` agrees with a bitwise reference for any address and
prefix.

## D. `Op::Timeout` on the cycle

Files: `io/wire.rs`, `io/ops.rs`, `io/cap.rs`, `io/host/dispatch.rs`,
`io/host/steps.rs`.

- `Op::Timeout(Duration)`, carried in `Sqe::offset` as microseconds,
  rounded up and saturating. No buffer. `TimeoutOp::Output = Result<(), IoError>`.
- `Handle<K>::timeout(dur)` on every interface.
- The dispatch never routes a timeout to the `Os`. It keeps
  the deadline in the op's `InFlight::due`, and a count of armed timers so
  `expire` skips the scan when there are none. `lower` takes `now` and sets
  the deadline. `IoStep::execute` calls `dispatch.expire(now)` before
  posting, which raises `Ok` for each deadline at or before `now`. A cancel
  raises `Canceled` at once.
- The deadline is on the cycle's clock. Under `Clock::Wall` that is system
  time, so a step in it moves pending timers.
- Quiesce cancels timers like any op. A timer is not a kernel op, so
  teardown never waits on one.
- `TestPort` lands timeouts as `TestIo::turn` advances its clock, timed
  from the turn the op was submitted in. One a full CQ refuses lands on a
  later turn.

Tests:

- Nominal: a 30 ms timeout under `TestIo`'s 10 ms turns lands on the third
  turn.
- Edge: a zero timeout lands in the pass that lowers it, since `expire`
  runs after `lower`; two timers with the same deadline both land.
- Failure: a cancelled timeout lands `Canceled` and never lands `Ok`; a
  quiesced member's timers are cleared.

## E. The link core

Files: new `link/deframe.rs`, `link/batch.rs`; `link/inbox.rs` shrinks to
`link/peer.rs`.

- `Deframer` as in the design, over one `Buf`, with `spare`, `filled`,
  `next`, and `compact`. `spare` returns a `split_off` tail; `filled`
  `join`s it back. A length prefix past capacity is `Oversized`.
- `Batcher { ids: Vec<PacketId>, scratch: Buf }` with
  `fill(&mut self, inputs) -> Option<&[u8]>`: today's `publish::fill`
  writing through `extend`.
- `ConnectionState` and `packet` move to `link/peer.rs`.
  `ConnectionState::route(tables, body, missing: impl FnMut(PacketId))`
  returns `Option<(ty, id, bytes)>`: `None` for an announce or a malformed
  body. The first data packet calls `missing` for each unannounced table.
  It holds `unseen: Box<[bool]>` per table, sized at make and refilled by
  `reset(watch)`, so a reconnect does not allocate.
- `Deframer::spare` compacts first and returns `None` while a tail is
  lent. `filled` takes the tail `Buf` back rather than a length, and
  refuses any buffer that is not the lent tail. `reset` returns false and
  forgets nothing while a tail is lent, so `Conns` returns every recv's
  buffer through `filled`, errors and EOF included, before reusing a slot.
- `Batcher::fill` drains nothing from a stale scratch buffer, and drops a
  record that does not fit whole rather than writing part of it.
- F and G remove the `expect(dead_code)` on `batch` and `deframe`.
- `Inbox`, `conn.rs`, and the old `publish::fill` stay until G replaces
  their users. `Inbox` routes through `ConnectionState`, and `Batcher`
  and `append_packet` share `wire::packet_head`.

Tests (moved from `inbox.rs` where they still apply):

- Nominal: one packet per read; three packets in one read; a packet split
  over three reads; the batch layout test from `publish.rs`.
- Edge: a zero-length body; a length prefix split across reads; compaction
  with nothing left over.
- Failure: an oversized prefix is `Oversized` and the deframer yields
  nothing further; a malformed body is `Skip`; a missing table is reported
  once per connection.

Kani: for any sequence of `filled` lengths, `next` never yields bytes
outside the filled region, and `compact` preserves the unread bytes.

## F. `Conns`

Files: new `link/conns.rs`.

- `Conn`, `Tx`, and `Conns` as in the design. Every arena buffer comes
  from `IoInit`.
- `ConnsMem::make(init, ConnsConfig { slots, conn_cap, dialing,
  write_timeout })`, then `Conns::new(mem, ep, seed, tables)` (split in G).
  A seed past `conn_cap` is `MakeError::SeedTooLarge`; a short budget is
  `MakeError::IoOps`.
- `Conns::poll(&mut self, on_body: impl FnMut(&mut ConnectionState,
  &[u8]))`. No `now`: the timers are ops. Each slot owns its
  `ConnectionState`, reset with `watch = dialing` on open, so `Subscribe`
  routes through it in the callback. The pass polls the slots first, then
  takes a new stream, so a slot freed in a pass is reused in it; a new
  connection's recv and send start on the next pass, or at the next
  `broadcast`. `Conns::broadcast(&[u8])` also starts idle sends.
  `Conns::stats()`.
- A dialer has one slot and asks for a stream only while it is free; a
  listener always has a stream op out, and closes one with no free slot.
  A lost dialed connection waits out the backoff before redialing. A
  dialer's backoff doubles from 500 ms to 10 s; a listener's stays at
  500 ms, as today's accept pause.
- An op the full queue refuses is retried, not treated as a failure: a
  recv, a send, the stream op, a close, or the backoff timer.
- The write timer restarts on every finished send: a connection closes
  when a send stays in flight `write_timeout` with none finishing. A
  canceled timer is reaped before the next is set.
- `Conns::events()` drains a fixed `ArrayVec`-style queue of `Opened(addr)`,
  `Closed`, `Refused`, and `Failed(IoError)` for the link to log. It is sized at make and
  drops past capacity, counting the drops.
- Backoff and the write timeout use `Op::Timeout` on `ep`.
- `ops_for(slots) = 3 + 4 * slots`: the stream op, a backoff timer, a
  refused stream's close, and per slot a recv, a send, a write timer, and
  a close. `Conns::new` fails if `Handle::budget` is short of it. The
  budget is the system's, so the link holds no other cap. A slot stays
  taken until its close lands too, and a refused stream's close lands
  before the next stream op, so the bound is hard; the tests check it every
  turn.

Tests, all against `io::test::cap_with_ops::<Endpoint>`:

- Nominal: accept, send the seed, receive two packets, broadcast twice
  (ping-pong); dial, lose the connection, redial after backoff.
- Edge: a full table closes the extra stream; a partial send resubmits the
  tail; a batch that does not fit counts `batches_dropped`; the same seed
  goes to each new connection.
- Failure: a recv error closes the slot and the slot is reused only after
  both ops return; a send past `write_timeout` is cancelled and the
  connection closed; repeated dial failures back off 500 ms, 1 s, 2 s up to
  10 s and reset after a success.
- A steady-state test: 1000 turns of broadcast and receive with the
  allocator counting, after make, zero allocations.

## G. `Publish` and `Subscribe` on the cycle

Files: `link/{mod,publish,subscribe,db,tests}.rs`, `link/conns.rs`; delete
`link/conn.rs`, `link/transport.rs`, `link/inbox.rs`.

- `LinkParams { namespace, link, max_connections, dialing, conn_cap,
  write_timeout_ms }`, shared by both links; `transport` and `inbound_cap`
  are gone. `max_connections` defaults to 8, `conn_cap` to 1 MiB, and
  `write_timeout_ms` to 5000. A link cannot see its resource's class, so
  `dialing` comes from lowering (`Connect` and `Peer` set it).
- `publish` and `subscribe` are `#[sequence]` fns taking `seq: &Seq`,
  `ep: &Cap<Endpoint>`, their dynamic ports, `link_status`, and `log`. Each
  turn: `conns.poll`, the batch or route step, the status report, then
  `seq.next_cycle().await`.
- A constructor sees neither its `Cap` nor its ports, so `Conns` is built in
  two steps. `ConnsMem::make(init, config)` allocates the arena buffers in
  `Publish::new` / `Subscribe::new` (plus the batch buffer for `Publish`).
  The body's first turn takes them and calls `Conns::new(mem, ep, seed,
  tables)`, which allocates the slots, the seed, and the event queue on the
  heap and checks the seed and the budget. A failure there ends the
  sequence with `SeqError::Failed`.
- `register` uses `register_sequence`, then `set_ops(link_ops)`, which
  reads `max_connections` and `dialing` from the instance's params and sets
  the budget to `ops_for` the slots. Params that do not decode are
  `DefError::Params`.
- `Subscribe` routes each body through the slot's `ConnectionState` and
  `Routes::deliver`, and logs `peer_schema_mismatch` straight from the
  `missing` callback. A full output ring counts as `inbound_dropped`.
- `StatusReporter::report` also logs `Conns::events` (connected,
  disconnected, refused, stream failures) and dropped events. The status
  timestamp is the cycle's `now`.
- `Db` stays on stellarator with its own private `accept`.

Tests: the `publish::fill` tests moved to `Batcher` in E. `subscribe.rs`
tests `Routes::deliver` through `ConnectionState`. `link/tests.rs` runs a
publisher and a dialing subscriber on two `TestIo`s joined by a byte pipe:
1000 turns, every record in order, no table missing, and no allocation
after the first 20 turns. The `#[sequence]` bodies themselves run over TCP
in `tests/link.rs`; a sequence on `TestIo` needs a sim `Endpoint`.

## H. Config lowering

Files: Python `_builtins.py`, `_io.py`, `_model.py`, `_target.py`;
`coordinator/config.rs`; `deploy/{config,error,mod}.rs`; delete
`deploy/part.rs`; `tests/{common,link,deploy,module}.rs`, goldens.

Python:

- `Listen(addr, max_connections=8, allow=())`, `Connect(addr)`, `Peer(link)`
  keep their names. `Link._caps = ("ep",)` (the builtins stub renders no
  caps).
- `Target._add` calls `system._bind(self, name)`, which for a `Link`
  creates resource `{name}_ep`. The grant is per registration through
  `System._grants_for(target, index)`, as params are, so one `Link` added
  twice gets two grants. `Listen` makes `TcpListen(addr, allow)`; `Connect`
  makes `TcpDial` after resolving a host name with `socket.getaddrinfo`;
  `Peer` makes `PeerDial`, whose JSON is `{"peer": {"target", "link"}}`.
- `Db` is no longer a `Link`: it shares only the `_Sink` mixin for `items`
  and `all`. `Subscribe` drops `inbound_cap`.

Deploy:

- `CoordinatorConfig<S, R = Backing>` takes its resource type, and
  `try_map` maps systems and resources. The envelope's resources are
  `DeployBacking`: `Peer(PeerLink)` or a runtime `Backing`, as
  `DeploySource` does for outputs.
- A system's listen address is its `listen` param, or the `tcp_listen`
  resource it is granted.
- A `Peer` resource granted to a system is rewritten to
  `tcp_dial { addr }` with the peer's port on the host map's host, resolved
  to a `SocketAddr` (`DeployError::Address` if it does not resolve). One no
  system holds is left alone.
- `DeployTransport`, `Part`, and `LINK_TYPES` are deleted. A peer dial left
  unresolved fails `into_runtime` with `DeployError::UnresolvedDial`.

Tests:

- `tests/link.rs` keeps every behaviour it checked, configured through
  `common::lower_links`, which lowers a test's `transport` param as Python
  does. Added: a peer outside `allow` is closed and counted `rejected` in
  `io_status`, and the link counts no connection; `link_status` and
  `io_status` both count. The sim-clock test now checks that a reading
  peer keeps its connection through 50,000 fast cycles; the async
  adapter's `mirror_dropped` fault it waited for no longer exists.
- Python: a link emits its resource and grant; the allowlist reaches the
  resource; a target with only links gets `io_submit` and `io_complete`;
  `Peer` emits a peer dial; a host name in `Connect` resolves; a link's
  endpoint name collides with a taken resource name.
- Deploy unit tests resolve `Peer` resources, host names, and unresolved
  dials. Goldens regenerate. `deploy.json` keeps key order (regenerated
  from `metor build` output with Python, not a `serde_json::Value`).

Docs: DESIGN.md "Links and gateways" names the `ep` grant and the
allowlist. TODO §4 drops the "Link loops" and "Connection buffers" items
and adds links on threads.

## I. Group reactors (outline)

TODO §8. `IoState::build` partitions members by placement; each thread
group gets a `Dispatch` and an `Os`. A reactor task lowers, submits, waits
on the backend's fd through stellarator, raises, and wakes. `Queue` gains a
waker table sized from the budget. Timeouts go to the `Os`
(`IORING_OP_TIMEOUT`, a deadline list in `PollOs`). Its own design doc.

## J. Links on threads (outline)

The link bodies already run under any executor that polls `Pending` with a
waker. Add an async driver beside the sequence, accept `thread:` in the
link types' `Adapters`, and reject it in a group holding a sim resource.
Run `tests/link.rs` under both drivers. Move `Db`'s query server onto the
reactor.
