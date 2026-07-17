# metor-fsw-2 Code Review

## Scope

This review covers the current `metor-fsw-2` crate, its proc-macro crate, the
shared-memory ring crate, and the integration tests. The review focused on
simplicity, correctness, and readability. Findings are ordered by severity.

## Implementation Status

All eight findings below were addressed on 2026-07-14. The findings retain
their original file references as a record of the reviewed code.

1. Bundle members and artifact names are restricted to one portable path
   component. Extraction rejects duplicates, non-files, invalid checksums,
   truncated data, invalid UTF-8, and arithmetic overflow.
2. Dynamic list and map writers carry their declared const bounds, reject and
   roll back overflowing members, and surface `DynamicWriteError` through
   `Output::write_with`. Framework-owned lists are bounded as well.
3. Simulated steps must be finite and positive. Timestamp calculation is
   checked and saturates rather than wrapping.
4. Bundle loading always verifies `ir_sha256` and requires a recorded manifest
   sidecar. Manifest hashing is now documented strictly as interface
   compatibility; it does not claim to authenticate shared-object bytes.
5. Async systems receive `AsyncContext`, cancellation-aware waits, named
   shutdown timeouts, and cooperative joining before deadline cancellation.
6. `AsyncSystem::run` is called exactly once and owns its loop. The built-in
   uplink follows that contract, and `#[system]` accepts an optional
   `&AsyncContext` run parameter.
7. `Input::latest` and `MsgIn::drain` return corruption errors. Runtime owners
   report read and framework-publication failures through health or terminal
   outcomes instead of treating them as missing data.
8. The normative design, system, coordinator, and telemetry documentation now
   describes the implemented APIs and queue architecture. Historical design
   material is explicitly labeled.

## Findings

### 1. High: `.metor` extraction permits writes outside the temporary directory

`src/wiring/bundle.rs:423-429` accepts every archive member name and passes it
directly to `tempdir.join(name)`. Absolute paths and `../` components therefore
escape the temporary directory. The same trust issue exists for
`Artifact::cdylib` at `src/wiring/bundle.rs:379-381`: a crafted `wiring.json`
can make the loader resolve and later load a library outside the bundle.

The custom tar reader also does not validate the header checksum or entry type
(`src/wiring/bundle.rs:465-485`) and uses unchecked arithmetic on attacker-
controlled sizes. A malformed bundle can consequently panic instead of
returning `BundleError`.

Recommendation: require every archive and `cdylib` name to be one normal path
component; reject absolute paths, prefixes, roots, `.` and `..`; reject
duplicates; validate regular-file type and checksum; and use checked arithmetic
for offsets and padded lengths. Add adversarial archive tests.

### 2. High: dynamic frame bounds are used for sizing but are not enforced

`FrameList<T, MAX>` and `FrameMap<V, MAX, MAX_KEY>` advertise hard bounds and
derive `Componentize::MAX_SIZE` from them (`src/dynamic.rs:161-168` and
`src/dynamic.rs:242-248`). However, `FrameWriter::list` and `FrameWriter::map`
erase all three const bounds (`src/writer.rs:120-146`), while
`ListWriter::push` and `MapWriter::insert` append without checking count or key
length (`src/writer.rs:252-258`, `287-309`).

This allows a record to exceed the maximum used to allocate its ring. The
write then fails as `InsufficientCapacity`; infallible publication paths reduce
that to a drop. Framework code is exposed too: sequence progress is unbounded
in `CycleClock`, but is published into `FrameList<_, 16>`
(`src/sequence/mod.rs:64-66`, `313-331`). Seventeen progress calls in one poll
can make the status record exceed its declared size and disappear.

Recommendation: make member writers carry the declared bounds, preferably via
derive-generated typed accessors, and return a specific overflow error before
mutating the frame. Until then, cap every framework-owned list before writing.
Test `MAX + 1` elements and `MAX_KEY + 1` bytes for both direct and port writes.

### 3. High: simulated-clock configuration can panic during a fallible resolve

`src/wiring/resolve.rs:980-982` calls `Duration::from_secs_f64(dt_secs)` without
validating the serialized value. Negative, NaN, and infinite values panic even
though wiring resolution returns `Result`. A zero duration is accepted but
breaks the documented monotonic clock and leaves `cycle().await` pending
forever because `NextCycle` requires `now > armed_at`
(`src/sequence/mod.rs:234-244`). Very large valid durations or cycle counts can
also wrap the final `i64` timestamp cast in `simulated_now`
(`src/coordinator/mod.rs:356-364`).

Recommendation: add `InvalidSimulatedStep`, require a finite positive step,
and make timestamp calculation checked or saturating with an explicit terminal
error. Cover all non-finite, non-positive, and overflow cases.

### 4. High: bundle integrity fields are written but not enforced on load

Packaging records `meta.ir_sha256` over the exact `wiring.json` bytes
(`src/wiring/bundle.rs:224-239`), but `load_bundle_dir` never compares it with
the file it reads (`src/wiring/bundle.rs:348-399`). The field currently gives a
false integrity guarantee.

Likewise, if an artifact records `manifest_hash`, verification only happens
when the sidecar also exists (`src/wiring/bundle.rs:387-397`). Removing the
sidecar bypasses the check. The comments claim every recorded hash is verified
and that tampered shared objects fail before `dlopen`, neither of which is true.
The sidecar hash describes the manifest, not the shared-object bytes.

Recommendation: always verify `ir_sha256`; require a sidecar whenever
`manifest_hash` is present; and either add a digest for each library or describe
the manifest hash strictly as an interface-compatibility check, not artifact
integrity.

### 5. Medium-high: common async systems cannot shut down cooperatively

The coordinator sets a stop flag and notifies input waiters, but a notifier
only completes when its ring-ready predicate is true. The code acknowledges
that a task parked in `Input::recv` cannot observe shutdown
(`src/coordinator/mod.rs:825-843`). After 20 ms the task guard is dropped and
the future is cancelled, so `System::shutdown` at `src/coordinator/mod.rs:309`
does not run for the most common recv-driven case.

This is especially risky for flight-facing resources whose shutdown hook must
safe hardware or flush state. It also makes `run_for(...).await` report a
completed lifecycle without guaranteeing that async shutdown hooks ran.

Recommendation: make cancellation part of the awaited condition. A shared
cancellation token selected with input/timer waits is simpler and testable.
Join completed tasks and report which tasks exceeded the shutdown deadline
before force cancellation.

### 6. Medium: the `AsyncSystem` contract and implementation disagree

The public docs say the coordinator spawns `run` once and that `run` owns its
loop (`src/lib.rs:60-65`, `src/system/mod.rs:329-339`). `AsyncSlot` actually
calls `run` repeatedly whenever it returns (`src/coordinator/mod.rs:303-308`).
The built-in uplink depends on this undocumented pass-based behavior
(`src/telemetry/mod.rs:590-599`). A user following the trait docs can write a
one-shot `run` and accidentally create a hot restart loop; a user following the
uplink example will implement a different lifecycle than the trait describes.

Async struct systems also bypass the cyclic drivers' timing, drop folding, and
automatic health publication. Unless each implementation manually calls
`HealthPort::end_cycle`, errors and output drops may never be observable.

Recommendation: choose one contract. Prefer calling `run` once and putting the
loop plus cancellation token inside the implementation. If repeated passes are
intentional, rename the method to `step_async`, document it, pace empty passes,
and move health/drop folding into `AsyncSlot`.

### 7. Medium: runtime errors are repeatedly collapsed into missing data

`Input::latest` converts `ReadError::Corrupt` to `None`
(`src/port.rs:326-335`), while `MsgIn::drain` discards the same error
(`src/message.rs:355-375`). Copy-in, alarm evaluation, and telemetry repeat the
pattern (`src/coordinator/mod.rs:695-708`, `src/alarm/mod.rs:535-561`,
`src/telemetry/mod.rs:957-984`). At higher layers, framework status, health,
logs, and sequence status ignore write failures (`src/health.rs:176-212`,
`src/sequence/mod.rs:313-331`, `src/coordinator/mod.rs:772-809`).

Corruption or observability backpressure therefore looks like “no new sample”
or a healthy cycle. That is a poor failure mode for flight software and makes
root-cause analysis harder.

Recommendation: make `latest` return `Result<Option<_>, ReadError>` and
propagate or count errors at the owning driver. Give framework-generated ports
their own drop/error counters, with a non-ring fallback for failures of the
health ring itself.

### 8. Medium-low: documentation describes APIs and architectures that no longer exist

`DESIGN.md:116` refers to `src/reader.rs` and `ListReader`/`MapReader`, none of
which exist. `docs/telemetry.md:231-280` describes a two-lane hand-off, while
the implementation has one `thingbuf` batch queue (`src/telemetry/mod.rs:679-712`,
`933-987`). The async lifecycle conflict in finding 6 appears in the crate-level
docs. These are central design documents, so drift directly reduces readability
and encourages incorrect integrations.

Recommendation: test intra-doc source paths in CI, mark historical plan files
as such or move them out of `docs/`, and update design documents in the same
change that replaces an architecture. Keep one normative overview and link to
implementation-specific documents from it.

## Simplification Priorities

1. Make bounded dynamic writers typed and checked. This removes scattered
   truncation/drop workarounds and restores one trustworthy sizing rule.
2. Unify async lifecycle semantics around one cancellation-aware runner. This
   removes special health behavior and the repeated-pass/run-once ambiguity.
3. Replace the custom tar subset with a well-tested archive library, or isolate
   it behind a small validated `BundleMemberName` and checked parser.
4. Standardize ring consumption on `Result`; decide loss policy once in each
   driver instead of independently in ports, alarms, telemetry, and copy-in.
5. Split the largest mixed-responsibility modules after these contracts settle:
   `wiring/stubgen.rs`, `coordinator/slot.rs`, and `macros/system_attr.rs` are
   each over 1,000 lines and currently combine parsing, validation, rendering,
   and runtime adaptation.

## Verification

- `cargo test -p metor-fsw-2 --all-features`: 232 library tests passed, one
  ignored; all dynamic-library, slot, wiring, UI, and doc tests passed.
- The process fixture builds directly, and all six `proc_integration`
  scenarios execute and pass rather than taking the harness's skip path.
- `cargo test -p metor-fsw-2-macros`: 10 tests passed; one doc test ignored.
- `cargo clippy -p metor-fsw-2 --all-targets --all-features --no-deps` passes
  after applying its one archive-name simplification.
- `cargo check -p metor-fsw-2 --tests --all-features` and `git diff --check`
  pass.

The existing test suite is broad and the ring crate is unusually well
documented around its unsafe invariants. The main residual risk is at contract
boundaries: declared versus enforced sizes, fallible loaders that panic or
trust paths, and runtime errors that are intentionally erased before health can
observe them.
