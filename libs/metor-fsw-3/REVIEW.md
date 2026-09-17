Review of metor-fsw-3 changes since `1b4b640fac887fc174652a85634d42b503c17080`, originally through `71c25c66`.

The original findings below are fixed by T1–T6. ABI 2 requires rebuilding
existing packs. T7, allocation-free steady-state logging, is deferred.

1. **High — Safe logging could access freed memory. Fixed.** The public
   guard did not constrain the port's lifetime or thread. A synchronous
   `with_log_port` callback now holds the borrow; private cleanup restores
   the previous TLS slot during nesting and unwinding. Formatting removes
   the active pointer to suppress recursive access. See [logging](src/log.rs).

2. **High — Pack ring handles could outlive backing memory. Fixed.**
   Attached handles now retain backing through opaque owner callbacks.
   The allocating side uses standard `Arc` reference counting; its Rust
   layout stays private. Escaped handles and grants keep storage alive.
   Libraries remain resident so guest callbacks and TLS destructors stay
   callable. See [ownership](ring/src/owner/mod.rs) and [loader](src/dl.rs).

3. **High — Failed consumers blocked healthy consumers. Fixed.**
   Skipped runners retained readers and eventually filled producer rings.
   Fault handling now takes and drops the runner, preserving only the
   coordinator's name and status writer. Native and guest regressions
   verify continued fanout beyond ring capacity. See [retirement](src/coordinator/run.rs).

4. **Medium — Panic containment missed fault hooks and descriptor exports.
   Fixed.** Fault hooks, retirement, table/schema construction, descriptor
   serialization, and create/error conversion now have unwind containment.
   Panic payload destruction is guarded too; a second payload is forgotten
   if destroying the first panics. Abort-mode panics and double panics
   during unwinding remain unrecoverable. See [pack exports](src/pack/mod.rs)
   and [panic handling](src/panic.rs).

5. **Medium — Nested defaults generated unimportable Python. Fixed.**
   Dataclasses are keyword-only and mutable defaults use factories.
   System parameter conversion recursively copies mutable defaults into
   each instance's JSON data. Import/runtime tests cover field order
   and independent defaults. See [template](src/cli/module.py.jinja).

6. **Medium — String defaults were incompletely escaped. Fixed.**
   Generated strings and docs now use a complete literal encoder. Tests
   cover multiline text, control characters, quotes, and Unicode.
   See [renderer](src/cli/module.rs).

Descriptor leaks are also fixed: the host supplies a bounded 1 MiB buffer,
decodes owned data, and frees the buffer on success or failure. Guest
tables are owned by TLS and normally drop at thread exit; shutdown cleanup
retains Rust's TLS limitations. `pack dev` inspects the final staged copy
and rejects rebuilding a path already loaded in that process. A fresh CLI
invocation can rebuild it.

Remaining debt: logging still allocates strings and vectors per event.
T7 is explicitly deferred; the initialization-only allocation requirement
is not yet met by logging. See the [fix plan](docs/plans/04-packs-review-fixes.md).

Validation: 250 Rust tests, six trybuild cases, 18 Python tests, strict
Clippy, and formatting checks pass. Coverage includes native and guest
fault/fanout regressions, escaped guest handles, bounded and owned
descriptors, Python imports, and fresh-process CLI rebuilds. All nine
focused logging tests pass Miri; the logging borrow rejection also passes
its compile-fail doctest.
The ring's 60 Miri tests, 15 bounded Loom/concurrency tests, and 19 sequential
Kani harnesses pass. Kani required correcting two stale helper names; its
coverage does not establish cross-library ownership or concurrent correctness.
