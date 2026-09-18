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
T7 (the ADCS control loop as an example and convergence test) and T8
(docs, and folding shipped deviations back into `03-authoring.md`) are
deferred to slice 3, since the example has to be rewritten for packs
anyway.

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

- Shared state: the `&Shared<S>` parameter kind and the one-thread rule
  are designed in `05-links.md` and held until two systems must hold
  one socket (bi-directional commanding on one port).

- Link loops: `publish.rs` and `subscribe.rs` repeat the `Event` enum,
  `next()`, and the prune/re-arm tail, and their params repeat four fields;
  fold them into one helper in `conn.rs` when a third link arrives.

- Message ids: postcard messages hash the schema name, other codecs the
  record name. Unify in metor-proto-wkt (the panel's matchers depend on
  the current ids), then drop `Msg::ID` from the `Record` derive.

## 5. Sequences and slots

Async fns polled once per cycle with `wait_for` and `sleep`, and slots
that load, start, abort, and stop them at runtime. In-process occupants
only; `SequenceCommand` arrives as an ordinary message.

## 6. Process and wasm adapters

The control block and mmap ring files for worker processes; wasmi with
the guest-allocates-rings protocol and per-call fuel

## 7. Deployments and gateway

`Deployment` of several targets, `Publish` and `Subscribe` mirrors, and
the gateway target with the embedded db.
