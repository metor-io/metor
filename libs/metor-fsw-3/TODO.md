# TODO

The rewrite lands in slices. Each slice has a design doc and an
implementation plan under `docs/plans/`; a slice ends with an
antagonistic review before the next one starts.

## 1. Base, done

Ring copied and frozen, fixed frames, typed ports, the `System` trait,
and a coordinator that builds from a low-level config and steps systems
in list order. `01-base.md`, `02-ring-review.md`.

## 2. Authoring surface, in progress

`Record` with the codec on the type, one `write` and `latest` and `drain`
for frames and messages, `#[system]` impl blocks, JSON params, and the
`log` output with the `tracing` bridge. T1 to T6 landed and reviewed.
T7's ADCS example landed under `examples/adcs-fsw3` on 2026-09-22
(`09-adcs-example.md`); T8 (docs, and folding shipped deviations back
into `03-authoring.md`) is still open.

- Restore `#[derive(Record)]` support for serde-compatible enums. Timestamp
  field parsing currently restricts the derive to structs; enum messages
  without timestamps should remain supported. Add a compile-pass regression test.

## 3. Packs

Systems from outside the binary: the pack ABI shrunk to the fsw-3
descriptor, the dylib adapter, `metor-build`, the Python recorder with a
new `to_config()`, and `metor run`. Written fresh: fsw-2 is read for its
lessons, nothing is copied. Takes over the ADCS example and the slice-2
docs pass from T7 and T8. `04-packs.md`.

## 4. Links, done

`Publish` and `Subscribe`, each over a listen or connect transport, the
`Thread` adapter and named thread placement for async systems, and
dynamic ports with a schema on every `PortDef`. The wire format is
fixed by metor-panel and metor-db, so it is reproduced, not designed.
`05-links.md`, `05-links-impl.md`. Landed 2026-09-18; the fixture's
`metor run` closes the loop from a `Subscribe` through `echo` to a
`Publish`.

- Definitions from config: a type computes its `SystemDef` from its
  instance config, dynamic markers and build's completion pass are gone,
  and ABI 5 adds `metor_fsw_def`. Landed 2026-09-20,
  `06-def-from-config.md`.

- Shared state: the `&Shared<S>` parameter kind and the one-thread rule
  are designed in `05-links.md` and held until two systems must hold
  one socket (bi-directional commanding on one port).

- Link loops: `publish.rs` and `subscribe.rs` repeat the readiness race
  and the status tail, and their params repeat four fields; fold them
  into one helper in `conn.rs` when a third link arrives.

- Message ids: postcard messages hash the schema name, other codecs the
  record name. Unify in metor-proto-wkt (the panel's matchers depend on
  the current ids), then drop `Msg::ID` from the `Record` derive.

- Connection buffers: `Connections::open` allocates a connection's three
  buffers (outbox, in-flight write, receive) because a closed connection
  ends by cancellation, and a completion-style read or write dropped
  mid-flight takes its buffer with it. Reuse them so a connection allocates
  nothing: both halves watch a close flag and return their buffers as the
  task's output, and `prune` joins the task before the slot is free. A
  write stuck on a silent peer then holds its slot until TCP gives up,
  which needs a bound.

## 5. Sequences and slots

Async fns polled once per cycle with `wait_for` and `sleep`, and slots
that load, start, abort, and stop them at runtime. In-process occupants
only; `SequenceCommand` arrives as an ordinary message.

## 6. Process and wasm adapters

The control block and mmap ring files for worker processes; wasmi with
the guest-allocates-rings protocol and per-call fuel

## 7. Deployments and gateway, done

`Deployment` of several targets in one envelope, `metor build` resolving
peer references into addresses and baked definitions, frame
subscription routed by table id with a connect-time announce check, the
`Db` built-in behind the `db` feature, and `metor run` spawning every
target. Landed 2026-09-22, `08-deployments.md`, `08-deployments-impl.md`.

- `--host NS=ADDR` on `run` and `build`, overriding the envelope's host
  map for a run away from where it was built.
- A `loop()`-style forward link reference, so a target can name a peer
  link added later in the file.
- Per-port message identity: a message record from a peer arrives on
  one port however many ports publish it, because a `Msg` packet
  carries only its record id (`05-links.md`, decision 5 is where the id
  rule lives).
- The NixOS renderer over the resolved envelope, once packs ship as
  built wheels the renderer can map to store paths.
- `PacketStream::next_grow` from a 1 KiB buffer failed a read with
  `EINVAL` on a publish whose announce runs to tens of kilobytes;
  reading from a 64 KiB buffer, as `identify` does, did not. The gate
  reads that way; the growth path in stellarator wants a look.
