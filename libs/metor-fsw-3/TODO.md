# TODO

The rewrite lands in slices. Each slice has a design doc and an
implementation plan under `docs/plans/`; a slice ends with an
antagonistic review before the next one starts. Plans for landed slices
are deleted; git history keeps them.

## 1. Base, done

Ring copied and frozen, fixed frames, typed ports, the `System` trait,
and a coordinator that builds from a low-level config and steps systems
in list order.

## 2. Authoring surface, in progress

`Record` with the codec on the type, one `write` and `latest` and `drain`
for frames and messages, `#[system]` fns, JSON params, and the
`log` output with the `tracing` bridge. T1 to T6 landed and reviewed.
T7's ADCS example landed under `examples/adcs-fsw3` on 2026-09-22;
T8 (docs) is still open.

- Restore `#[derive(Record)]` support for serde-compatible enums. Timestamp
  field parsing currently restricts the derive to structs; enum messages
  without timestamps should remain supported. Add a compile-pass regression test.

## 3. Packs

Systems from outside the binary: the pack ABI shrunk to the fsw-3
descriptor, the dylib adapter, `metor-build`, the Python recorder with a
new `to_config()`, and `metor run`. Written fresh: fsw-2 is read for its
lessons, nothing is copied. Takes over the ADCS example and the slice-2
docs pass from T7 and T8.

- Logging allocations: a log event builds its fields in a `String` and
  `Vec` per call. Give `LogPort` a byte arena and field-range table sized
  at init, format through a checked `fmt::Write`, and encode a borrowed
  event view into `Output`'s scratch buffer, so logging allocates nothing
  after make.

## 4. Links, done

`Publish` and `Subscribe`, each over a listen or connect transport, the
`Thread` adapter and named thread placement for async systems, and
dynamic ports with a schema on every `PortDef`. The wire format is
fixed by metor-panel and metor-db, so it is reproduced, not designed.
Landed 2026-09-18; the fixture's
`metor run` closes the loop from a `Subscribe` through `echo` to a
`Publish`.

`Publish` and `Subscribe` run as sequences on the IO queue: a link holds a `Cap<Endpoint>`, its
transport lowers to a `tcp_listen` or `tcp_dial` resource, and its
connections allocate nothing after make. A listener listens once and
accepts; a dialer connects.

- Connecting as an op (plan 26): chunks A to C landed: resources are
  recipes, `connect` and `listen` open handles, `Reconnect` and
  `IoError::is_lost`. Left: sim wire ends that unplug and replug (D), and
  `connects` and `connect_failures` in `io_status` (E). Opens under a
  `Dir` still run inline on io_uring, and two systems holding one tty
  each open it.

- Links on threads: the thread driver over group reactors (§8), so
  `thread:` places a link off the cycle. The link bodies already run under
  any executor that polls `Pending` with a waker: add an async driver beside
  the sequence, accept `thread:` in the link types' `Adapters`, reject it in
  a group holding a sim resource, and run `tests/link.rs` under both
  drivers. Move `Db`'s query server onto the reactor.

- Definitions from config: a type computes its `SystemDef` from its
  instance config, dynamic markers and build's completion pass are gone,
  and ABI 5 adds `metor_fsw_def`. Landed 2026-09-20.

- Shared state, held until two systems must hold one socket
  (bi-directional commanding on one port). A shared instance is declared
  in config under its own id with its own params, no ports and no
  lifecycle, and registered with `table.register_state::<S>`. A system
  takes it as a `&Shared<S>` (`Rc<RefCell<S>>`) parameter mapped to the
  instance id. A state and every system naming it live on one thread, so
  the borrow never contends; a state named by both a cyclic and an async
  system is a build error.

- Message ids: postcard messages hash the schema name, other codecs the
  record name. Unify in metor-proto-wkt (the panel's matchers depend on
  the current ids), then drop `Msg::ID` from the `Record` derive.


## 5. Sequences and slots, done

`channel=` wraps a cyclic system in the channel adapter, which applies
`Start`, `Stop`, `Abort`, `Reset`, and `Load` from `SequenceCommand`
messages and reports its whole state in a `channel_status`
frame. `#[sequence]` async fns are polled once per cycle through a `Seq`
handle. `DlSlot` loads pack systems from a content-addressed store that
`metor build` fills, and `ChannelRegistry` publishes the target's
channels. Landed 2026-09-24.

- Channels on async systems: a per-member stop and cancel in the thread
  group, and a `Threads` handle the recipe can re-place with.
- `Target.add` returns the system's own type, so a checker does not see
  a channel's `channel_status` handle.
- Postcard samples: `Sample<T>` holds only `Frame + Copy` records. A
  postcard record needs its decoded value held across the cycle without
  allocating after make.

## 6. Process and wasm adapters

The control block and mmap ring files for worker processes; wasmi with
the guest-allocates-rings protocol and per-call fuel

## 7. Deployments and gateway, done

`Deployment` of several targets in one envelope, `metor build` resolving
peer references into addresses and baked definitions, frame
subscription routed by table id with a connect-time announce check, the
`Db` built-in behind the `db` feature, and `metor run` spawning every
target. Landed 2026-09-22.

- `--host NS=ADDR` on `run` and `build`, overriding the envelope's host
  map for a run away from where it was built.
- A `loop()`-style forward link reference, so a target can name a peer
  link added later in the file.
- Per-port message identity: a message record from a peer arrives on
  one port however many ports publish it, because a `Msg` packet
  carries only its record id (postcard messages hash the schema name,
  other codecs the record name).
- The NixOS renderer over the resolved envelope, once packs ship as
  built wheels the renderer can map to store paths.
- `PacketStream::next_grow` from a 1 KiB buffer failed a read with
  `EINVAL` on a publish whose announce runs to tens of kilobytes;
  reading from a 64 KiB buffer, as `identify` does, did not. The gate
  reads that way; the growth path in stellarator wants a look.

## 8. IO as a queue, planned

Design `docs/plans/18-io.md`, plan `18-io-plan.md`, chunk plans
`18a`–`18g` (chunk D removed).

- Quiesce timeout. A channel `Reset` waits until the system has no ops
  in flight. A cancel the OS never answers keeps it waiting for good. That
  happens with file IO stuck in the kernel on a hung storage device. Add
  a deadline per resetting system. Past it, remake anyway, keep the stuck
  ops' buffer slots marked in flight so they are never reused, and log a
  `quiesce_timeout` fault.
- Timeouts on a monotonic clock. The dispatch resolves `Op::Timeout`
  deadlines against the cycle's `now`, which under `Clock::Wall` is
  `Timestamp::now()`, so it follows system time. When the clock steps (NTP,
  an operator setting it), every pending timer fires on the next pass, or
  waits out the step backwards. A link's backoff and write timeout misfire
  with it. Give the cycle a monotonic clock for deadlines, kept beside the
  wall timestamp that records carry, so a sim run stays deterministic.
- Layers from config. Target files install layers (fault injection over
  real hardware for HIL, transport faults in sims) through metor-expr or
  WASM rather than a built-in rule language. The layer mechanism itself
  (the stack, its verdicts, and a counting layer) returns with this work.
  Until then faults are injected through models.
- stellarator on the one op type. Its executor, ops, and backends move
  onto `Op`, `Sqe`, and the `Os` trait, and its public types keep their
  API.
- Group reactors, and async systems taking `Cap<K>` as reactor
  capabilities. Each thread group gets its own reactor; link systems then
  hold capabilities, and a capability can be adopted into a stellarator
  type.
- Deterministic async IO, through a proxy `Os` that carries a group's sim
  ops to the coordinator's device ends.
- The `sim` resolver and links over simulated resources (`SimListen`,
  `SimDial`). Needs async systems on the reactor.
- Targets that mix real and sim resources. Build refuses them today.
- More sim kinds (`SimCan`, `SimUdp`, `SimDir`, `SimListen`, `SimDial`),
  each with its driver.
- Windows in the `Os`. `PollOs` is Unix-only.
