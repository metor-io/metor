# 25: Links on the IO queue

## Question

Links (`Publish`, `Subscribe`, `Db`) run as async systems on stellarator.
Each link spawns a transport task and one task per connection, which talk
to the link's loop through rings and a thingbuf channel. The IO subsystem
(`src/io`) gives a system a `Cap<K>`, a fixed arena, and ops that hand their
buffers back on completion. Can links move onto it, and what does IO need
first?

## Answer

Yes. Links become one `async fn` body over a sans-IO protocol core and a
fixed table of connection slots. Two drivers run that body:

- A `#[sequence]` polled once per cycle, with ops carried by `fsw.io`. This
  is the default. It is deterministic and works over sim resources.
- An async system on a thread group with its own reactor, chosen with
  `thread:` in config. It trades determinism for throughput and takes the
  link's CPU off the cycle.

The cycle driver replaces today's links. The thread driver follows with
group reactors (TODO §8) and adds no link code.

IO needs a few small additions first: dialing behind one `Endpoint`
interface that reports each peer's address, a peer allowlist on listeners,
a per-system op budget, partial sends, and appends into a `Buf`. A timeout
op is added with them.

## Today

### Links

- `transport.rs`: `Transport::bind` binds a listener or resolves a dial
  address in `new()`, so a taken port fails the build. `Endpoint::run` is
  its own task: it accepts or dials, with backoff from `stellarator::sleep`.
- `conn.rs`: `Connections` holds each slot's outbox writer and the counters.
  `Serving` spawns one task per connection. Its write half sends straight
  from the outbox ring through `GrantBuf`; its read half reads length
  delimited packets into the `Inbox`. Events go back to the loop on a
  thingbuf channel.
- `inbox.rs`: a ring between the read halves and `Subscribe`'s loop, plus
  the per-connection announce check (`ConnectionState`).
- `publish.rs`: drains the inputs into one batch (`fill`) and queues it on
  every outbox.
- `subscribe.rs`: routes inbox records to output ports by packet type and id
  (`Routes`).

Open problems, from TODO §4:

- `Connections::open` allocates three buffers per connection, because a
  cancelled task takes an in-flight buffer with it.
- A write stuck on a silent peer holds its slot until TCP gives up.
- `publish.rs` and `subscribe.rs` repeat the readiness race and status tail.
- `GrantBuf` relies on stellarator keeping a dropped op's buffer inline
  (the seven-word assert).

### IO

- A system's `Io` is a queue pair (`io_sq`, `io_cq`) and `MAX_OPS = 16`
  op slots. `Pending<O>` is polled once per cycle and registers no waker.
- `wire_io` feeds a system's SQ to the next `fsw.io` in the schedule and
  takes its CQ from the previous one. With
  `producers → fsw.io → link → fsw.io`, a send reaches the kernel in the
  cycle it is submitted, and data that arrived between cycles is routed in
  the cycle it is reaped.
- Each completion waits for a cycle before a dependent op can be
  submitted.
- Every op returns its `Buf`. Closing a handle or quiescing a system never
  loses memory.
- Real resources open in `IoState::build`, so a bad resource fails the
  build.
- `Op` has `Accept` on a `Listen` handle but no connect. `Stream` takes
  `Recv` and `Send`. A `Send` may write fewer bytes than asked, and a `Buf`
  cannot send from an offset.
- The derived-handle table, which holds accepted streams, has `MAX_OPS`
  entries per system.

## IO prerequisites

### Dialing

Add `RealKind::TcpDial { addr: SocketAddr }` and `Class::Dialer`. The
address lives in the resource, so a system dials only what it was granted.
A dial resource opens no socket at build. Each connect op creates one.

Host names resolve before the resource is written. A `Peer` transport
already resolves through `metor build` and the `--host` map; a hand-written
`Connect { addr }` resolves in the Python recorder, as `TcpListen` already
requires an IP literal.

### The `Endpoint` interface

`Publish` and `Subscribe` each take a listen or a dial transport. A link
only needs the next stream; how it was opened stops mattering once it is a
`Handle<Stream>`. So both transports provide one interface:

- `Interface` `Endpoint` (`"endpoint"`) replaces `Listen`. `tcp_listen` and
  `tcp_dial` each provide it and nothing else, so `interface()` keeps
  returning one name. Only the queue unit tests use `Listen` today.
- `Op::Accept` becomes `Op::Stream`, one opening op. The dispatch lowers it
  by class: a `Listener` accepts, a `Dialer` connects. Both resolve to a
  `Handle<Stream>` in a derived slot.
- `Handle<Endpoint>::stream(peer: Buf)` is its only method. It resolves to
  `(Result<Handle<Stream>, IoError>, Buf)`, with the peer's `RawAddr` in
  `peer`.

```rust
let (stream, peer) = ep.stream(peer).await;
let addr = SocketAddr::try_from(RawAddr::read_from_bytes(&peer)?)?;
```

A listener passes `peer`'s span as accept's address out-param. The op holds
the `Buf` until it completes, so the pointer stays valid for io_uring. The
address length lives in the dispatch's per-op storage, beside `SendTo`'s
`msghdr`. A dialer writes the address it connected to. The `Cqe` does not
change.

Sim endpoints, Unix sockets, or a TLS stream later provide `Endpoint` too,
and no link type changes.

### Peer allowlist

`tcp_listen` takes an optional `allow` list of CIDR blocks:

```json
"tcp_listen": { "addr": "0.0.0.0:2240", "allow": ["10.0.0.0/24", "192.168.1.7/32"] }
```

The dispatch checks each accepted address before raising the completion. A
rejected peer is closed, counted as `rejected` in `io_status`, and the
accept is lowered again. The system never sees it. An empty list allows
every peer.

Access control lives in the resource, beside the address, so it is
reviewed with the target and holds for a pack system as well. The address
`stream` reports is for identity and logs, and for checks too dynamic for
config: the system closes the stream. TCP completes the handshake before
`accept` returns either way, so accepting and closing is all a
listener-only API could do.

### Per-system op budget

A listening `Publish` with eight peers needs one accept plus a recv and a
send per peer: 17 ops, past `MAX_OPS`. The budget becomes part of the
definition:

- `IoPorts { cq, sq, max_ops }`. `Cap`-holding systems default to 16.
- A link's `def` sets `max_ops = 2 + 3 * slots`: the stream op and a
  backoff timer, then a recv, a send, and a write timer per slot.
- `Queue::ops`, `Member::derived`, and the dispatch's op table are sized
  from it at build. The dispatch's `m * MAX_OPS + s` indexing becomes a
  prefix sum over members' budgets, computed once.
- Kani proofs keep a small fixed budget.

### Partial sends and appends

- `Buf::advance(n)` drops the first `n` filled bytes, so `span_for` names
  only the unsent tail. A send loop resubmits until the buffer is empty.
- `Buf::extend(&[u8]) -> usize` appends into spare capacity and returns the
  bytes taken. `Buf::clear()` sets the start and length back to zero.

### Timeouts

`Op::Timeout(Duration)` on any handle resolves `Ok` when it expires, or
`Canceled`. Redial backoff and the stuck-write bound use it, so the link
body names no clock.

- The cycle's dispatch keeps timeouts in a fixed table and resolves them
  against cycle `now`. Runs stay deterministic.
- A timeout never reaches the cycle's `Os`. Under a group reactor,
  `UringOs` uses `IORING_OP_TIMEOUT` and `PollOs` keeps a deadline list
  that bounds its wait.

## The link core

The protocol moves out of the tasks into data structures that never touch a
handle. Each is unit tested without IO.

### `Deframer`

Reassembles `u32`-length-delimited packet bodies across reads, in one
`Buf` of `conn_cap` bytes.

```rust
struct Deframer { buf: Buf, start: u32 }

impl Deframer {
    /// The spare capacity a recv fills.
    fn spare(&mut self) -> Buf;
    /// Records `n` received bytes.
    fn filled(&mut self, n: usize);
    /// The next complete body, if one is buffered.
    fn next(&mut self) -> Option<Result<&[u8], Oversized>>;
    /// Moves the partial packet to the front.
    fn compact(&mut self);
}
```

A body longer than `conn_cap` is `Oversized`, which closes the connection,
as today. Kani proves that `next` never reads past the filled bytes and
that `compact` keeps a partial packet intact.

`spare` needs a `Buf` naming the tail of another `Buf`'s span. That is
`Buf::split_off(at) -> Buf`, and `Buf::join(self, tail)` to rejoin
adjacent spans. Both check that the spans are adjacent and from the same
arena.

### `Batcher`

Today's `publish::fill`, writing into a `Buf` through `extend`. It encodes a
cycle's records once into a scratch `Buf`.

### `Routes` and `ConnectionState`

`Routes` stays as it is. `ConnectionState` moves from `Inbox` to the
connection slot. The `Inbox` ring is deleted: the link reads completed
recvs itself, so it routes bodies straight to its output ports. A full
output ring counts as `inbound_dropped`.

## Connection slots

```rust
struct Conn {
    stream: Option<Handle<Stream>>,
    rx: Slot<RecvOp>,
    deframe: Deframer,
    state: ConnectionState,
    tx: Tx,
}

/// Two `conn_cap` buffers: the link appends to one while the other sends.
struct Tx {
    filling: Buf,
    sending: Slot<SendOp>,
    deadline: Option<Pending<TimeoutOp>>,
}

struct Conns {
    endpoint: Cap<Endpoint>,
    next: Slot<StreamOp>,
    slots: Box<[Conn]>,
    stats: Stats,
}
```

Every buffer comes from `IoInit` at make: three of `conn_cap` per slot, the
scratch batch, and the `RawAddr`-sized buffer `next` lends for each peer
address. Nothing allocates afterwards.

`Conns::poll` does one pass:

1. Reaps `next`. A new stream takes a free slot, which is reset and handed
   the seed (announce or `LinkInfo`) as its first send, and its peer's
   address is logged. With no free slot the stream is closed. A dial
   failure arms a backoff timeout, doubling from 500 ms to 10 s, as today.
   `next` is resubmitted.
2. Reaps each recv into the deframer, yields the bodies to the caller, and
   resubmits.
3. Reaps each send. A partial send advances and resubmits. A finished send
   swaps `filling` and `sending` if `filling` holds bytes.
4. A send in flight past `write_timeout` (default 5 s) is cancelled.
5. A recv of zero bytes or any error closes the connection: cancel the
   other op, close the stream, and keep the slot until both ops have
   returned their buffers.

`Conns::broadcast(batch)` appends the batch to every open slot's `filling`.
A slot without room counts one `batches_dropped`.

A dialing link has one slot. It redials when that slot closes.

This is the shared helper TODO §4's "Link loops" item asks for. It also
closes the "Connection buffers" item: a closed connection's buffers come
back with its last completion and stay in the slot.

## The link body

```rust
async fn run_publish(
    conns: &mut Conns,
    batcher: &mut Batcher,
    inputs: &mut PublishInputs,
    status: &mut LinkStatusPort,
    log: &mut Log,
) -> ! {
    loop {
        conns.poll(|_slot, _body| {});
        if let Some(batch) = batcher.fill(inputs) {
            conns.broadcast(batch);
        }
        status.report(conns.stats(), 0, log);
        next_turn().await;
    }
}
```

`Conns::poll` takes a callback for each received body; `Publish` ignores
them. `next_turn` is the driver's wait: the next cycle, or the next wake.

`run_subscribe` has the same shape, routing each body through its
connection's `ConnectionState` and `Routes`. It reports
`peer_schema_mismatch` straight from the state rather than through the
`missing` ring.

`Db` keeps its ingestion as it is. Its metor-db query server stays on
stellarator until the thread driver lands.

## Drivers

### Cycle (default)

`Publish` and `Subscribe` become `#[sequence]` systems taking
`ep: &Cap<Endpoint>`. The sequence polls the body once per cycle, and
`next_turn` waits for the next one. Ops go through `fsw.io`.

- Deterministic, and works over `SimListen` and `SimDial` once they exist.
- Throughput: one recv and one send completion per connection per cycle.
  At 100 Hz and the 1 MiB default `conn_cap`, that is 100 MiB/s per
  direction per connection.
- The link's encoding and routing run on the cycle.

### Thread (opt-in)

Naming `thread:` on a link places it as an async system. Its `Cap`s bind to
its thread group's reactor rather than to `fsw.io`. The body is unchanged.

The reactor is TODO §8's "Group reactors":

- `IoState::build` partitions members by placement. Each thread group gets
  its own `Dispatch` and `Os`, and opens the resources its members hold.
  `fsw.io` serves the members in the cycle.
- A reactor task on the group's executor lowers the SQs, submits, and waits
  until the backend's fd is readable, then raises the completions and wakes
  their tasks. Until stellarator moves onto `Os` (TODO §8), the task awaits
  the io_uring eventfd, or the `PollOs` fd, through stellarator.
- `Queue` gains `[Cell<Option<Waker>>; max_ops]`, filled at make. `Pending`
  stores its waker when it returns `Pending`. The cycle driver never stores
  one.
- Timeouts resolve against the wall clock.

What it buys: dependent ops (partial sends, back-to-back recvs, accept then
first recv) run at kernel speed rather than once per cycle, and the link's
CPU leaves the cycle. Record-to-wire latency does not change, because the
thread adapter copies ports once per cycle either way.

What it costs: the link is no longer deterministic. The build rejects
`thread:` on a link in a group that holds a sim resource.

The link types' `Adapters` accept bare, channel, and thread placement.

## Configuration

The Python surface does not change:

```python
pub = plant.add("pub", Publish(transport=Listen("0.0.0.0:2240"), items=[sim]))
sub = fsw.add("plant", Subscribe(transport=Peer(pub), items=[sim]))
```

`to_config` lowers a transport into a resource and a grant:

```json
"resources": { "pub_ep": { "real": { "tcp_listen": { "addr": "0.0.0.0:2240" } } } },
"systems": [ { "id": "pub", "ty": "fsw.publish", "grants": { "ep": "pub_ep" }, ... } ]
```

- `Listen` lowers to `tcp_listen`. `max_connections` stays a link param
  and sets the slot count. `Listen(addr, allow=[...])` sets the resource's
  allowlist.
- `Connect` lowers to `tcp_dial`.
- `Peer` lowers to a `tcp_dial` whose `addr` `metor build` fills in, as it
  fills `Transport::Connect` today (`deploy/mod.rs`).
- `LinkParams::transport` is removed from the runtime config. The deploy
  envelope keeps `DeployTransport` and resolves it into the resource.
- A link declares `_caps = ("ep",)`, so `Target` places `io_submit` and
  `io_complete` for it as for any other `Cap` holder. `Target.add` gains
  nothing link-specific beyond creating the grant.

## Testing

- Unit: `Deframer` (split packets, many packets per read, oversized,
  compaction), `Batcher`, `Conns` against the io test harness
  (`io/test.rs`): accept into a full table, dial backoff, partial sends,
  the write timeout, close with ops in flight.
- Unit, IO: `Op::Stream` on both classes reports the peer's address; an
  allowlist rejects an outside peer, counts it, and re-arms the accept; an
  empty list allows every peer; a malformed CIDR fails at parse.
- Kani: `Deframer` bounds, `Buf::split_off` and `join`, the dispatch's
  prefix-sum indexing, and CIDR matching.
- Integration: `tests/link.rs` over TCP, unchanged in what it checks. Each
  test runs once per driver once the thread driver lands.
- A steady-state allocation test: a publish and subscribe pair that runs
  1000 cycles after make without allocating.

## Slices

1. IO prerequisites: `tcp_dial`, `Endpoint` and `Op::Stream` with peer
   addresses, the `tcp_listen` allowlist, per-system `max_ops`,
   `Buf::advance`, `extend`, `split_off`, and `join`, `Op::Timeout`.
2. The link core: `Deframer`, `Batcher`, `ConnectionState` per connection.
3. `Conns` against the io test harness.
4. `Publish` and `Subscribe` on the cycle driver; config lowering in
   Python and `metor build`. Delete `conn.rs`, `transport.rs`, `inbox.rs`,
   and the stellarator and thingbuf dependencies of `link`.
5. Group reactors and `Pending` wakers (TODO §8).
6. `thread:` on link types; `Db`'s query server onto the reactor.

Slices 1–4 replace today's links. Slices 5 and 6 add the thread driver and
help every async system, not only links.

## Open questions

- Whether the allowlist also filters by source port, or only by address.
- Whether `max_connections` belongs on the link or on the `tcp_listen`
  resource. It sizes the link's slots, so this doc keeps it on the link.
- `write_timeout`'s default, and whether a timed-out connection is closed
  or only its batch is dropped.
- Whether `inbound_cap` survives. With no inbox ring, the output port depth
  bounds what a subscriber holds.
