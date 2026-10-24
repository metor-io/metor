# 26. Connecting as an op: implementation plan

A real resource's descriptor may be missing at boot, or lost while it runs: an
FTDI not plugged in, a CAN interface down, a TCP peer that hangs up. Today a
resource opens once at build, a failed open fails the build, and a lost
descriptor stays dead until restart.

Dialing already avoids this. A dial resource holds no descriptor; each
`Stream` op makes a new socket as a derived handle, and the link decides when
to try again. This plan makes that the rule for every real resource:

- A resource is a recipe: a path, an address, a baud rate. The IO layer opens
  nothing at build and holds no up, down, or retry state.
- `connect` on a capability opens a new descriptor as a derived handle.
  `listen` on a listening capability opens a listener handle, and `accept` on
  that opens streams. Connect and listen are the two ways a connection is
  made, side by side.
- A lost device is an op error on its connection's handle. The system closes
  the handle and connects again when it chooses, with its own backoff.

## Facts the plan relies on

- `Backed` is `Fd(OwnedFd, Class)`, `Listen(OwnedFd, Box<[Cidr]>)`,
  `Wire(usize)`, `Dial(SocketAddr)`, or `Unused` (`io/host/dispatch.rs`).
  `Resource::open` (`io/host/mod.rs`) opens each held real resource at build.
- `Dispatch::target` maps a handle to `Target::{Fd, Wire, Dial}`. A grant is
  live at generation 0. Derived handles come from `Stream` (accept, dial) and
  `Open` under a `Dir`, one slot per op of the member's budget, and `derive`
  installs them.
- `Close` on a grant ends the capability until the next make; on a derived
  handle it closes the descriptor.
- `open` (`io/host/open.rs`) opens and configures every kind synchronously:
  tty with termios, CAN with filters, GPIO through the chip ioctl, UDP, file,
  directory, listener. A dial already splits in two: both backends make the
  socket inline (`sys::stream_socket`), then wait for the connect, `PollOs`
  by parking on writable and `UringOs` with `IORING_OP_CONNECT`.
- io_uring opens a path asynchronously with `IORING_OP_OPENAT` (5.6). It
  has no op for termios or the GPIO line ioctl, and socket, bind, and
  listen ops only from 5.19 and 6.11.
- `Cap<K>` derefs to `Handle<K>`, so a granted capability takes the
  interface's data ops directly. Drivers that do: the ADCS `gps` and
  `gps_receiver`, echo-pack `gps`, `uart_script`, `chatter`, `grower`, and
  `tests/sim.rs`, `tests/io.rs`.
- `Conns` gives a dialer one slot, dials only into a free slot, and backs
  off after a failure. It asks for streams with `Handle<Endpoint>::stream`.
- `Sqe` op codes and `IoError` codes are part of the pack ABI
  (`ABI_VERSION`, wasm `io.rs`).

## The model

Every real resource is synchronous setup plus at most one step that waits.
For a path the wait comes first (the open) and the setup after (termios, the
GPIO line request). For a socket the setup comes first (socket, bind,
filters) and the wait after, only for a TCP stream's connect. The build
turns each `RealKind` into one of the two:

```rust
pub(crate) enum Backed {
    Real(Recipe),
    Wire(usize),
    Unused,
}

/// What a real resource's `connect` or `listen` does, resolved at build.
pub(crate) enum Recipe {
    /// Opened through the backend, then configured: tty, GPIO, file, dir.
    Path(PathKind),
    /// Made and bound by the io step; a TCP stream then connects through
    /// the backend. UDP, CAN, dial, listen.
    Socket(SocketKind),
}

/// Paths are made NUL-terminated at build, so an open points at them and
/// allocates nothing.
pub(crate) enum PathKind {
    Tty { path: CString, baud: Baud },
    GpioLine { chip: CString, line: u32, direction: GpioDirection },
    File { path: CString, access: Access },
    Dir { path: CString },
}

pub(crate) enum SocketKind {
    Udp { bind: SocketAddr, connect: Option<SocketAddr> },
    Can { iface: CanIface, filters: Box<[CanFilter]> },
    /// A dial's `host:port`, resolved at build.
    Dial(SocketAddr),
    Listen { addr: SocketAddr, allow: Box<[Cidr]> },
}
```

Each lowers one way:

| recipe | io step | backend waits for | then |
|---|---|---|---|
| `Path` | nothing | the open: `OPENAT` on io_uring, inline on `PollOs` | the io step configures it |
| `Socket`, not `Dial` | socket, bind, filters, listen | nothing | done |
| `Socket(Dial)` | socket | the connect | done |

A grant's capability is never a descriptor. Its ops are `Connect`, `Listen`
(a `Listen` recipe only), `Timeout`, and `Close`; data ops on it are
refused `Unsupported`. Every descriptor is a derived handle:

| resource       | on the capability     | gives            | then                    |
|----------------|-----------------------|------------------|-------------------------|
| `tty`          | `connect`             | `Handle<Uart>`   | read, write, configure  |
| `can`          | `connect`             | `Handle<Can>`    | recv, send              |
| `gpio_line`    | `connect`             | `Handle<GpioLine>` | get, set              |
| `udp`          | `connect`             | `Handle<Udp>`    | recv, send              |
| `file`         | `connect`             | `Handle<RegularFile>` | read, write, fsync |
| `dir`          | `connect`             | `Handle<Dir>`    | open                    |
| `tcp_dial`     | `connect`             | `Handle<Stream>` | recv, send              |
| `tcp_listen`   | `listen`              | `Handle<Listener>` | accept → `Handle<Stream>` |

Today two systems can hold one resource and share its descriptor. Under
this model each connects its own. That is harmless for a file or a dial;
for a tty it means two descriptors on one device (see Decisions).

## A. Connect and listen ops

- `wire.rs`: add `Op::Connect` and `Op::Listen`; rename `Op::Stream` to
  `Op::Accept`. `opens()` covers all three, `min_buf` keeps `RawAddr::LEN`
  for `Accept` and `Connect` on a stream kind. Bump `ABI_VERSION`; mirror in
  `wasm/io.rs`.
- `cap.rs`, `ops.rs`:
  - `Cap<K>` stops dereffing to `Handle<K>`. It has `timeout` and `close`,
    and for a `Device` interface `connect() -> Pending<ConnectOp<K>>`.
    `Cap<Endpoint>` has `connect(peer: Buf) -> Pending<StreamOp>` for a
    dial and `listen() -> Pending<ListenOp>`.
  - `ConnectOp<K>` resolves to `Handle<K>`, and for a stream fills the peer
    `RawAddr` as `StreamOp` does today. `ListenOp` resolves to
    `Handle<Listener>`; `Handle<Listener>::accept(peer)` is today's `stream`.
  - The data ops move from `Handle<K>` on the capability to the derived
    `Handle<K>`, unchanged.
- `io/host/recipe.rs`: `Recipe`, `PathKind`, `SocketKind`, and
  `Recipe::from_kind(&RealKind)`, which resolves a dial's `host:port`.
  `open.rs` splits along the same line:
  - `make_socket(&SocketKind) -> Result<(OwnedFd, Class), OpenError>`:
    socket, bind, CAN filters, `listen`; for a dial, the unconnected socket.
  - `open_flags(&PathKind)`: the `open` flags and mode a path opens with.
  - `configure(&PathKind, fd) -> Result<Class, OpenError>`: termios, the
    GPIO line request (which swaps the chip descriptor for the line's),
    nothing for a file or dir.
- `dispatch.rs`:
  - `Backed` becomes `Real(Recipe) | Wire | Unused`. A grant resolves to
    `Target::Recipe(res)`, a derived handle to `Target::Fd`.
  - `Connect` on a `Path` recipe lowers an `OsOp` with `Op::Connect`
    pointing at the recipe's `CString`, so lowering allocates nothing.
  - `Connect`/`Listen` on a `Socket` recipe routes `Route::Socket(OpId,
    res)`. The io step runs `make_socket`; a non-dial raises the descriptor
    at once, a dial lowers its connect to the backend as today.
  - `raise` of an opened `Path` descriptor runs `configure` before `derive`.
    A failed configure closes the descriptor and completes the op with the
    error.
  - `allows` reads `allow` from the `Listen` recipe, for `Accept` on a
    listener handle, which records its resource.
- Backends: `UringOs` lowers `Op::Connect` with a path to `OPENAT` with
  `AT_FDCWD`, and completes it with `Out::Opened`; its dial path is keyed by
  `dial.is_some()`. `PollOs` opens a path inline. `Open(flags)` under a
  `Dir`, inline in `sys::run` today, moves to `OPENAT` with the dir's
  descriptor on io_uring, so both opens of a path wait the same way.
- `io/host/mod.rs`: `Resource::open` stops opening. It builds the `Recipe`;
  a bad recipe or an unresolvable dial still fails the build, a missing
  device does not.
- Delete `Backed::{Fd, Listen, Dial}`, `Target::Dial`, `Class::Dialer`, and
  `open`'s single-call form; `OsOp::waits_to_write` checks `dial.is_some()`.

Tests (`io/tests/dispatch.rs`, `tests/io.rs`):
- nominal: `connect` on a pty's tty resource gives a handle that reads and
  writes; `listen` then `accept` gives a stream; `connect` on a dial gives a
  stream with its peer.
- edge: two `connect`s on one file give two handles; closing one leaves the
  other; `connect` when every derived slot is taken completes `Refused`; a
  path open on io_uring goes through `OPENAT` (Linux, in OrbStack) and on
  `PollOs` inline, with the same result.
- failure: `connect` on a missing tty path completes with the open's errno
  and creates no handle; a tty whose termios fails (a regular file at the
  path) closes the descriptor and completes with the error; a data op on the capability completes
  `Unsupported`; a build with a missing device succeeds; `connect`
  allocates nothing (`allocations`).

## B. Lost connections

No IO state. A lost device shows up as the op error the OS gives: `ENODEV`,
`EIO`, `ECONNRESET`, or a read of 0 bytes. The handle stays valid until the
system closes it; ops on it keep failing as the OS fails them.

- Add `IoError::is_lost(&self) -> bool` for the errors above, so drivers
  and `Conns` agree on what ends a connection.
- `io/reconnect.rs`: a `Reconnect<K>` helper for drivers: the current
  connection (`Option<Handle<K>>`), a `Slot<ConnectOp<K>>`, and a
  backoff timer, polled once per cycle. `Conns`' `Backoff` moves here and
  `Conns` uses it.

Tests:
- nominal: a driver using `Reconnect` reads a pty, the master closes, the
  driver sees 0 bytes, closes, and reconnects to a new pty at the same path.
- edge: a failed connect backs off 500 ms, doubling to the cap; a close
  during a connect in flight lands once.
- failure: `is_lost` is false for `EAGAIN`, `QueueFull`, `Canceled`.

## C. Links and drivers

- `conns.rs`: a listener calls `listen` once into a `Slot<ListenOp>`, then
  `accept` per stream; a dialer calls `connect`. A lost listener (an
  `accept` that fails with `is_lost`) closes it and listens again after the
  backoff. Slot and announce logic are unchanged.
- Drivers move to `connect`: ADCS `gps`, `gps_receiver`; echo-pack `gps`,
  `uart_script`, `chatter`, `grower`; `tests/sim.rs`, `tests/io.rs`,
  `tests/ui/pass/io_ctor.rs`, the io test harness. The GPS drivers use
  `Reconnect`.

Tests: `tests/link.rs` and `tests/sim.rs` pass; a new link test restarts the
listening peer and checks the dialing link reconnects; a listener whose
address is taken at boot builds, fails `listen`, and comes up once it is
free.

## D. Sim wires

- `io/host/wire.rs`: each end is connected or not. `Connect` on an end
  connects it; `Close` on a connected end disconnects it, and ops parked on
  the other end land with the error a hung-up tty gives (a 0-byte read, or
  `EIO` on a write). `Connect` on the host end while the device end is not
  connected completes `ENODEV`.
- A model unplugs by closing its end and replugs by connecting. No new op.

Tests:
- nominal: a model unplugs a sim UART; the driver reads 0, reconnects once
  the model connects again, and reads.
- edge: unplug with a read parked lands it; connect on both ends in either
  order works.
- failure: host connect with the device end down completes `ENODEV`.

## E. Status, Python, docs

- `ResourceStatus` gains `connects: u64` and `connect_failures: u64`. The
  table id changes; regenerate goldens.
- Python: nothing changes for resources. Stubgen renders `Cap` types as
  before.
- DESIGN.md: the resource model (recipes, `connect`/`listen`, derived
  handles, systems own retry). Drop the `required` question; there is no
  build-time open.

Tests: status counts on connect and failed connect; the golden and stub
tests.

## Order

A and B are one PR: the ops and the helper, with the drivers and links
moved in C in the same PR so nothing uses the old capability ops. D and E
follow.

## Decisions to make first

- **Sharing a device.** Two systems holding one tty would each open it.
  Options: the build refuses a device kind (tty, CAN, GPIO) held by two
  systems; or one system connects and the other gets its handle through a
  shared connection, which TODO's "shared state" item already wants. The
  plan has the build refuse it for device kinds.
- **Checking a recipe at build.** The plan checks only that it parses. A
  missing device path could be a build warning.
- **Listening at boot.** A listener now binds on its first `listen`, in the
  first cycle, not at build. A taken port is a runtime error the link backs
  off from, instead of a build failure.
- **Device paths that move.** A replugged FTDI may come back as
  `/dev/ttyUSB1`; the plan expects `/dev/serial/by-id/...` paths.
