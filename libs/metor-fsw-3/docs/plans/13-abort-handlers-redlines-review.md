# Abort handlers and redlines review

Reviewed changes from `ae804991df32a0ae265e4f05578bc3c502029ce5` through
`4b40ff67`, covering redlines, abort handlers, samples, and shared output writes.

## Findings

### [P1] Redline registration permits use-after-free

Location: [`Seq::redline`](../../src/sequence/seq.rs), line 123.

Registration stores a pointer without retaining the closure's capture
lifetimes. Safe code can leak a pinned redline, free its borrowed input,
and leave the sequence evaluating a dangling reference:

```rust
let value = Box::new(42u32);
let redline = Box::pin(Redline::new("dangling", || *value == 42));
seq.redline(redline.as_ref());
core::mem::forget(redline);
drop(value);
seq.next_cycle().await;
```

Pinning keeps the redline's allocation alive, but does not keep its borrowed
captures alive after `forget`. The next cycle invokes the closure and causes
use-after-free.

Confirmed with Miri using an isolated reproduction containing the sequence
and redline implementations, with unrelated timestamp and logging dependencies
stubbed. Miri reports a dangling `&u32` through `redline::check` and
`Seq::tripped`.

Registration must enforce capture lifetimes even when a redline is forgotten.

### [P1] Finished sequences retain sampled inputs and block producers

Location: [`SequenceStep::execute`](../../src/sequence/step.rs), line 136.

Completion drops `run` but retains `samplers`. Each sampler owns an input
whose ring views keep their latest records pinned. The coordinator retains
ended steps and skips further execution, so these readers never advance or
release their slots.

A sequence taking `&Sample<T>` that completes, aborts, or returns an error
therefore eventually causes its upstream producer to return `WouldBlock`.
This interrupts delivery to other consumers of the same ring too. Ordinary
inputs live inside `run` and are released when it drops.

Confirmed by tracing sampler ownership, step termination, and ring view
destruction. Panicked steps are removed by the coordinator and release their
samplers.

Release the samplers alongside the sequence future when the sequence ends.

### [P1] Shared ring writes allow notification re-entry to overwrite records

Location: [`Writer::try_write`](../../ring/src/lib.rs), line 734.

`WakeSource::notify` can safely re-enter `try_write` now that writes take
`&self`. During wrap-padding publication, a nested write advances `committed`,
but the outer write resumes using its stale reservation and overwrites the
nested record. `!Sync` prevents concurrent access across threads; it does not
prevent re-entry on one thread.

Confirmed with a safe custom wake source and a 64-byte ring:

1. Write and consume an 8-byte payload.
2. Attempt a 48-byte payload, forcing wrap-padding publication.
3. In the notification, advance the reader past the padding and write an
   8-byte payload through the same writer.
4. Resume the outer write.

Both writes return `Ok(())`, but draining the ring yields only the outer
48-byte payload. The nested record has been overwritten.

Add a re-entry guard at the ring writer level. The typed output's scratch
buffer guard does not protect direct ring writes or `write_bytes`.

## Validation

- `cargo test -p metor-fsw-3 -p metor-fsw-3-ring --lib --offline` passed:
  429 framework tests and 81 ring tests.
- The redline lifetime issue was confirmed with Miri in an isolated harness.
- Notification re-entry and record loss were confirmed with a standalone
  reproduction using the ring crate.
- Sampler retention was confirmed by inspection. An attempted temporary
  regression test could not rebuild because the sandbox denied access to
  the workspace target directory's Cargo build lock; the test was removed.
- No implementation changes were retained from the review.
