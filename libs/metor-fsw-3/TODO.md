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
`log` output with the `tracing` bridge. T1 to T6 landed. Left: the
review, T7 (the ADCS control loop as a statically linked example and
convergence test), T8 (docs, and folding shipped deviations back into
`03-authoring.md`).

- Restore `#[derive(Record)]` support for serde-compatible enums. Timestamp
  field parsing currently restricts the derive to structs; enum messages
  without timestamps should remain supported. Add a compile-pass regression test.

## 3. Packs

Systems from outside the binary: the pack ABI shrunk to the fsw-3
descriptor, the dylib adapter, `metor-build`, the Python recorder with a
new `to_ir()`, and `metor run`. Keep `copy_atomic` and the ABI marker
distribution from fsw-2 verbatim.

## 4. Links

Shared state, `TcpServer`, `Downlink`, `Uplink`. Needs an "every output
ring with its vtable" surface first. The wire format is fixed by
metor-panel and metor-db, so it is reproduced, not designed.

## 5. Sequences and slots

Async fns polled once per cycle with `wait_for` and `sleep`, and slots
that load, start, abort, and stop them at runtime. In-process occupants
only; `SequenceCommand` arrives as an ordinary message.

## 6. Process and wasm adapters

The control block and mmap ring files for worker processes; wasmi with
the guest-allocates-rings protocol and per-call fuel. Both port from
fsw-2 largely as-is once the ABI from slice 3 is settled.

## 7. Deployments and gateway

`Deployment` of several targets, `Publish` and `Subscribe` mirrors, and
the gateway target with the embedded db.
