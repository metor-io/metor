# Packs: review fixes

The active plan addresses the six findings in `REVIEW.md` and descriptor and
export-table leaks. T7, allocation in steady-state logging, is deferred at the
user's request and retained below for future work. T1–T6 are implemented as ABI 2.
The implementation should preserve static/pack behavior parity and keep the
unsafe boundary small enough to review independently.

Implementation checks: the full Rust suite, Python suites, formatting, and
strict clippy for the affected crates pass. The ring ownership tests and scoped
logging tests pass under Miri; bounded Loom tests pass. Independent review covered
ring ownership, loading/staging, descriptor exports, and failure retirement.
Concurrent `pack_dev` calls are serialized so staging cannot replace an artifact
loaded by another call.
All 19 existing sequential Kani harnesses pass after correcting two stale helper
names. These harnesses check ring geometry and operations, not cross-library
ownership or concurrency.

## Design decisions

- Logging installs a port only for a synchronous callback. No public guard can
  escape or be forgotten.
- Every attached ring owns a lease on its backing allocation. Dropping a
  coordinator does not invalidate an escaped reader, writer, or grant.
- Loaded pack code remains resident for the process lifetime. Ring leases alone
  cannot prove that guest threads, TLS destructors, callbacks, or returning stack
  frames have stopped executing. Reclaim ordinary pack state independently of
  library residency; dynamic unloading requires a separate quiescence design.
- A failed entry is retired after fault reporting by taking and dropping its
  runner. The coordinator retains its name and status writer. Never reclaim a
  reader slot behind a live handle or grant.
- Descriptor strings own dynamic data. Built-in definitions can still borrow
  string literals without allocating copies.
- The host supplies bounded descriptor storage. Each pack's thread-local cache
  owns its table directly; table resources have thread lifetime, not `Pack`
  handle lifetime. No explicit export-context lifecycle is needed.
- Bounded, allocation-free logging is deferred to T7. T1's logging lifetime and
  reentrancy fixes remain in scope with the current event representation.

An ABI bump is required. Ship the ownership and descriptor-buffer changes together
as ABI 2, update the Python ABI distribution and dependency pins, and rebuild
fixtures and example packs. The ring's shared-memory layout need not change:
the lease is process-local ownership metadata, outside the region. Keep the
JSON configuration version unchanged.

## T1. Scope the logging port

Files: `src/log.rs`, `src/fn_system/mod.rs`, logging tests.

1. Replace `enter` and public `Guard` with
   `with_log_port<R>(port: &mut LogPort, now: Timestamp, f: impl FnOnce() -> R) -> R`.
   Keep cleanup private and create it inside this function. Its drop restores the
   previous TLS slot on success and unwinding. Stamp the port before installation.
2. Change `FnSystem::execute` to invoke the user function inside this callback.
   Borrow the other output fields separately so the callback never receives the
   borrowed log port. Migrate all callers and tests.
3. When borrowing the TLS port for a write, temporarily remove its pointer from
   the slot and restore it with private cleanup. A recursive tracing event then
   sees no active port and cannot create a second mutable reference. Keep the
   raw-pointer dereference in this one helper with its complete safety argument.
   Move `event.record` inside this protected section now: guarding only the final
   ring write would leave recursive formatting outside the protection.
4. Restore the prior slot even if drop reporting fails. Keep cleanup itself free
   of user formatting and unwinding paths. Nested scopes must route events back
   to the outer port after the inner scope ends.

Tests: normal timestamps and routing; nested scopes; callback panic followed by
logging outside the scope; an independently entered second thread; recursive
formatting that emits a tracing event. Add a compile-fail case showing that the
callback cannot move or drop its borrowed port. Run the focused safety cases
under Miri. A lifetime-only guard fix does not meet this task's acceptance gate.

## T2. Give descriptors owned data

Files: `src/system.rs`, `src/pack/def.rs`, `src/dl.rs`, `src/port.rs`,
`src/fn_system/`, `src/coordinator/`, `src/cli/module.rs`, `macros/src/`.

1. Change runtime `SystemDef`/`PortDef` names and record names to
   `Cow<'static, str>`. Do not mark these fields `serde(borrow)`: deserializing
   dynamic names must produce owned strings. Remove `Copy` from `PortDef` and
   update callers to borrow or explicitly clone only at initialization.
2. Make `PackDef` and `PackSystemDef` owned descriptor types, with `String` for
   the type, `Cow<'static, str>` for documentation, and `Option<Box<RawValue>>` for the schema.
   Remove the descriptor lifetime parameters and `'de: 'static` restrictions.
   Keep literal-to-definition constructors concise with `.into()`.
3. Decode from the written portion of the host-owned descriptor buffer. Remove
   `Box::leak` and the leaked-buffer test helper. A decode
   error must release all partially constructed data normally.
4. Keep the serialized JSON shape unchanged. Update table registration and the
   renderer to borrow names from the owned descriptor or clone them during
   initialization, never to manufacture `'static` references.

Tests: unchanged JSON round trip; escaped names/docs and schema; drop the source
buffer before reading the decoded descriptor; truncated/invalid input; repeated
successful and failed loads with allocation/drop accounting in an isolated test.

## T3. Use caller-owned descriptor storage and contain boundary panics

Files: `src/pack/{mod,raw}.rs`, `src/dl.rs`, ABI fixtures and documentation.

Keep the existing five exports. Replace the descriptor's returned guest pointer
with a write into host-owned storage, and replace the leaked table reference
with an owned thread-local table. This depends on T4 keeping library code
resident, including the code used by thread-local destructors.

1. Change the descriptor export to:

   ```rust
   unsafe extern "C" fn metor_fsw_pack_def(
       dst: *mut u8,
       capacity: usize,
       written: *mut usize,
   ) -> u32;
   ```

   Define separate descriptor status words for success, insufficient capacity,
   serialization failure, and panic. Initialize `written` to zero and set it
   only on success. The guest must not retain either host pointer. The caller
   supplies writable, nonoverlapping storage valid for the call.
2. Start with a named host limit of 1 MiB, allocated on the heap at load time.
   Serialize directly into the supplied slice with a bounded writer. Never
   truncate or parse partial output: insufficient capacity is a clear
   `PackError::DescriptorTooLarge { capacity }`. The limit is a proposed policy,
   not a claim about all future packs, and can be adjusted independently of the
   ABI. After success, validate `written <= capacity`, deserialize owned values
   as in T2, and free the temporary buffer on both success and failure.
3. Replace `OnceCell<&'static Exported>` with `OnceCell<SystemTable>` owned by
   TLS. Access it through a small `with_table(build, |table| ...)` helper; no
   table reference escapes the closure. Descriptor generation and create use
   the same table, preserving shared constructor captures. Remove `Box::leak`
   and the cached descriptor `Vec`; do not rebuild the table per instance.
4. Retain the existing owned TLS error buffer and its next-create lifetime.
   Keep the pack-local global logging subscriber under T4's library-residency
   policy. Neither needs an open/close context or additional reference counting.
5. Catch table-builder/schema and serialization panics around the entire
   descriptor export. Enclose create result conversion and error serialization
   in its boundary guard too, not just `make`. Map descriptor failure statuses
   to distinct host errors and reject unknown status words.
6. Make ordinary error paths return `Result` instead of the existing type-name
   `expect`s. Keep unavoidable panics documented with local `PANIC Safety`
   comments. Do not add generic unwind abstractions where two short helpers do.

Tests: nominal descriptor round trip; exact-fit, zero-length, and one-byte-short
buffers with surrounding canaries; descriptor exceeding the host limit; buffer
cleanup on decode failure; no cached table growth on repeated loads on one
thread; normal constructor-capture cleanup on a supported worker-thread exit;
unknown type and invalid params; builder/schema panic becomes a host error.
Use a subprocess fixture for ABI panic tests so an accidental abort is an
ordinary test failure. Verify rejection of ABI 1 before calling ABI 2 signatures.

The table intentionally remains alive for its thread's lifetime, even after the
last host `Pack` drops. TLS destruction is best-effort at process shutdown, so
do not rely on it for external resource shutdown or flight behavior. This trades
per-handle cleanup for a much smaller API. See Rust's
[TLS lifecycle guarantees](https://doc.rust-lang.org/std/thread/struct.LocalKey.html#initialization-and-destruction).

Unwinding containment applies to `panic=unwind`. It cannot recover from aborts,
foreign crashes, a panic hook that aborts, or a second panic during unwinding.
Panic payload destruction can itself panic: explicitly handle payload disposal
inside the boundary helper, with a documented containment policy for a second
payload, rather than accidentally dropping it outside the guard.

## T4. Retain ring backing across the ABI

Files: `ring/src/{backing,lib}.rs`, `src/pack/{raw,mod}.rs`, `src/dl.rs`,
`src/cli/{pack_dev,run}.rs`, pack/CLI integration tests.

1. Add a process-local C-compatible lease descriptor containing an opaque owner
   pointer and `unsafe extern "C"` retain/release callbacks. Extend `RawRing`
   with this ownership metadata. Never pass `Arc`, trait objects, or Rust drop
   glue across the ABI as a shared Rust representation.
2. Expose a ring API that creates an owning export token for its backing and a
   borrowed raw descriptor valid for that token's lifetime. The host builds
   these tokens alongside its input/output arrays and holds them until create
   returns, including failure paths. Implement the token with the originating
   side's standard `Arc`, using `Arc::into_raw` and matching strong-count
   increment/decrement operations in its callbacks. Do not implement a custom
   counter. Only those callbacks interpret the opaque pointer, so the receiving
   library does not depend on the originating library's Rust allocation layout.
3. Add an owned attachment path to the ring crate. It acquires one lease before
   accessing the region and releases it on validation failure. On success, store
   the lease in `BackingOwner`; the attachment's existing shared `Arc` then
   carries it through every derived `RingBuffer`, `View`, and `Writer`. Release
   only when the last local backing owner drops, after claim cleanup. Grants
   already borrow these handles and remain covered by the same ownership.
4. Keep `attach_raw` explicitly unsafe for genuinely caller-owned storage. Packs
   must exclusively use the owned attachment path. State that lease callbacks
   are thread-safe, non-panicking, and valid through the final release. The host
   executable provides the callbacks; if this API is later exported from another
   unloadable module, that provider needs the same code-residency guarantee.
5. Retain successfully mapped libraries in a small process-lifetime registry,
   separate from instance ownership. Pin before calling exports that run user code:
   even a failed constructor can spawn a worker. Reuse a registry entry for
   repeated opens of the same loaded artifact. Keep this deliberate code
   residency distinct from owned descriptors, instances, and the TLS table cache.
   Do not unload in a lease callback: it may return into that library.
   Initialization or ABI validation failure still releases temporary data owners;
   it does not establish that the mapped image is safe to unload.
6. Keep descriptor inspection in the current process. No inspection helper or
   new subprocess protocol is needed. Make the lifecycle explicit: build and
   stage a pack before loading it, then keep that artifact fixed for this
   process. A fresh CLI invocation is required to rebuild an already-loaded pack.
   In `pack_dev`, atomically copy the built library to its final `.metor` path
   before opening it, then render the descriptor from that staged library.
   `metor run` later opens that same staged path and reuses the resident mapping
   instead of loading separate build-directory and staged copies. Deduplicate
   dev-pack paths during startup so aliases cannot trigger a second build.
7. Reject a `pack_dev` call targeting an already-loaded staged path before
   rebuilding or replacing it, with an error explaining that the process must
   restart. Reopening an unchanged loaded pack for execution remains supported.
   Document that loaded artifacts must not be replaced for a running host;
   there is no automatic `dlclose` or in-process hot reload. Change the repeated
   build integration test to invoke the ordinary CLI twice, as an author would.
   Change the descriptor between those invocations and verify the second command
   observes the new data. Also test the in-process rebuild rejection and reuse
   of a loaded artifact between inspection and execution.

Tests: escaped input and output survive coordinator/host-pack drop; a handle moved
to a worker remains usable until released; final release happens once; multiple
attachments release independently; invalid attachment and partial create failure
balance retains/releases. Exercise forgotten handles as a permitted resource
leak, never a use-after-free. Use local callback fixtures under Miri and native
cdylib integration tests for the actual ABI. Verify that library residency does
not itself keep ring allocations or instance state alive. Constructor captures
have the explicit TLS lifetime described in T3.

## T5. Retire failed systems without reclaiming live grants

Files: `src/coordinator/{mod,run,build}.rs`, `src/pack/mod.rs`, `src/dl.rs`.

1. Store `Entry.step` as `Option<Box<dyn Step>>`. On failure, latch immediately,
   invoke fault reporting once inside an unwind guard, take the step, and drop
   it inside a separate guard. Retain the entry's status writer and name.
2. Share the execute/fault containment behavior between static runners and pack
   execution. Guard `Step::latched()` too, or replace this overridable query with
   an explicit internal outcome so user code is never called outside a guard.
   A fault-handler panic must preserve the original failure and still retire the
   runner; it must not skip later entries in the cycle.
   Borrow the message from the caught panic payload while reporting it instead
   of allocating a second `String` in `message_of`. The Rust panic machinery's
   own allocations remain outside the framework's no-allocation guarantee.
3. When the pack returns `Panicked`, the host retires its `DlStep` in that cycle,
   causing exactly one destroy call. Pack-side execute reports its fault before
   host-side destruction. Later status messages have zero execution time, as
   today, and the first failing cycle retains its measured execution time.
   Give the guest instance wrapper a terminal state so repeated direct execute
   calls cannot run a failed step again. Preserve this behavior for the public
   `DlStep` adapter outside the coordinator too, or make that adapter private.
   Distinguish retirement of the inner step from destruction of its opaque
   wrapper; the host must still destroy that wrapper exactly once.
4. Ordinary runner destruction drops input views and releases their slots.
   Remove teardown comments that imply coordinator field order is sufficient
   for escaped handles. Do not use unsafe dead-process reclamation here, advance
   somebody else's cursor, or discard a live claim behind a retained grant.

Tests: one producer feeding a failing and a healthy consumer, ring depth two,
for at least 20 cycles; healthy consumption continues past repeated wraparound.
Run the case for static and pack consumers, plus execute-and-fault panics, a
destructor panic, exact-once destruction, and continued status reporting. Include
an escaped reader/grant case proving it is not reclaimed and its bytes remain
valid until release.

This fixes failure isolation for ports owned by the runner. Arbitrary custom
bindings can deliberately retain readers or leak them; a live escaped reader
may still apply backpressure. Safe forced disconnection would require a new
revocable/copying port model. State that limit in the API documentation instead
of claiming that arbitrary safe guest code can always be isolated. Worker-owning
systems must arrange for their teardown to stop/join workers; containment cannot
make a blocking destructor finish.

## T6. Generate valid, independent Python defaults

Files: `src/cli/module.rs`, `src/cli/module.py.jinja`, CLI integration fixtures.

1. Separate a field's semantic default from its final Python spelling. Use a
   small default enum or equivalent typed view, with separate renderers for a
   dataclass field and a system constructor keyword.
2. Emit `@dataclass(kw_only=True)` and import `field` when needed. Mutable list
   and dictionary defaults use `field(default_factory=lambda: <literal>)`, so
   nested defaults are newly constructed for each instance. Required fields may
   follow defaulted fields without import-time errors.
3. Retain ordinary system constructor defaults: `System.__init__` already passes
   their values through `_json`, which recursively copies lists and dictionaries.
   Test that this still isolates instances, including nested values, and
   preserves the distinction between an omitted value and explicit `None`.
4. Replace `escape` with a Python string-literal encoder used for values and
   dictionary keys. A serialized JSON string is also a valid Python string
   literal for Rust/JSON Unicode strings; use `serde_json` for string quoting,
   while retaining the recursive Python renderer for `None`/`True`/`False`.
   Route other template string values through the same literal function where
   they currently interpolate unquoted raw text. Keep docstring formatting
   separate or emit a complete quoted string expression as the class docstring.

Tests: import generated modules with required-after-default fields; empty and
nonempty mutable defaults; two instances do not share nested lists/dicts;
explicit `None` is preserved. Round-trip newline, carriage return, tabs, NUL,
quotes, backslashes, other controls, and non-ASCII characters through generated
defaults and dictionary keys. Instantiate classes and run `to_config()` to
confirm values reach the emitted configuration unchanged. Use supported Python
3.12 explicitly in this environment; retain the package's declared 3.11 minimum.

## T7. Deferred: bound logging storage and eliminate framework event allocations

Not part of this implementation or its completion gate. Resume only when the
user brings this work back into scope. The design below is retained for reference;
T1 still supplies the lifetime and reentrancy protections independently.

Files: `src/log.rs`, `src/port.rs`, the log record encoder, focused allocation tests.

1. Introduce a bounded event builder with a byte arena and field-range table,
   sized at `LogPort` initialization. Derive explicit limits from
   `LogEvent::MAX_LEN` and document the maximum field count. Reuse it per event;
   neither formatting nor a later event may grow a vector or string.
2. Format tracing values into a checked `fmt::Write` adapter. Keep metadata
   borrowed and store dynamic text in the arena. Serialization uses a borrowed
   event view with the same field order and postcard representation as
   `LogEvent`; the receiving API can continue decoding owned `LogEvent` values.
   Add a crate-private encoding helper that writes into `Output`'s existing
   scratch buffer, checks the record bound, and publishes only a successful
   encoding. This lets the borrowed log representation share the typed port's
   writer without constructing an allocating `LogEvent` or exposing an
   unrestricted public raw-write API.
3. Provide direct logging methods taking borrowed strings/format arguments and
   borrowed field pairs, eliminating mandatory `Vec`/`Cow<'static, str>` creation
   for normal callers. Route fault and dropped-event reporting through the same
   builder; format counts into bounded storage instead of `to_string()`.
4. On text, field-count, encoding, or ring-capacity overflow, drop the event and
   saturating-increment the drop count. Retry the summary on the next scope exit;
   clear the count only after successful publication. Do not recursively count
   failed drop summaries as new dropped events.
5. Build/record the tracing visitor only while a port is active. Hold the TLS
   slot disabled during formatting so user `Debug` implementations cannot
   re-enter the mutable builder. User formatting may allocate on its own; the
   framework cannot promise allocation-free arbitrary user code.

Tests: byte-compatible decode of direct and tracing events; numeric/debug fields;
exact capacity and overflow for text and field count; failed and successful drop
summaries; recursive formatting. After initializing the dispatcher, callsites,
ports, and test counters, assert zero allocations in the framework's normal
event, bounded overflow, and drop-summary paths using a thread-local counting
allocator in an isolated integration test. Do not include owned decoding or
user-supplied allocating formatting in the measured interval.

## Implementation order and completion gate

Implement T1 and T2 first. Land T3 and T4 as one ABI migration, then T5. T6 can be
implemented independently after the descriptor type migration. T7 is deferred;
its implementation and allocation tests are not completion requirements.
Each active task adds focused regression tests before expanding the implementation.
Keep helpers short and responsibility-specific; avoid a single object combining
descriptor storage, ring ownership, runner state, and logging scratch buffers.

Run the affected crate tests as each task lands. At completion run Rust library,
ring, macro, loader/pack/CLI integration tests, the Python suite with the supported
interpreter, formatting, and clippy for affected targets. Run the ring's existing
Miri and concurrency verification instructions for changed ownership paths;
extend their harnesses where feasible and report unavailable tools explicitly.
Rebuild the existing echo-pack fixture and compare static and pack output
streams. The ADCS fsw-3 example is not present in this checkout; its separate
porting work is not a completion dependency for these fixes. Review the
final ownership and panic boundaries with a second agent before declaring the
fixes complete. Update `04-packs.md`, ABI documentation, ring ownership docs, and
the findings' status in `REVIEW.md` to match the implementation actually shipped.

Set `METOR_PYTHON` to an installed Python 3.11+ executable before validation.
The main checks, from this crate's directory, are:

```sh
cargo test -p metor-fsw-3 -p metor-fsw-3-ring -p metor-fsw-3-macros
"$METOR_PYTHON" -m unittest discover -s python/metor-config/tests
"$METOR_PYTHON" -m unittest discover -s python/metor-build/tests
cargo fmt --check -p metor-fsw-3 -p metor-fsw-3-ring -p metor-fsw-3-macros
cargo clippy -p metor-fsw-3 -p metor-fsw-3-ring -p metor-fsw-3-macros --all-targets
```

Require no new clippy warnings. Use `ring/MIRI.md`, `ring/LOOM.md`, and
`ring/KANI.md` for supported verification commands and their coverage limits;
do not present native cdylib tests as evidence of Miri coverage across an ABI.

Remaining risks to report are deliberate library residency, arbitrary escaped
reader backpressure, user/destructor aborts or hangs, and the existing logging
allocations deferred to T7. None should be hidden behind a claim that all
guest code can be safely unloaded or forcibly disconnected.
