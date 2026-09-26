# Abort handlers and redlines: implementation plan

Implements `13-abort-handlers-redlines.md`. Six tasks, in order. Each
ends with `cargo test -p metor-fsw-3 --no-fail-fast`, `cargo clippy
--all-targets` clean, and one commit. T1 also runs the ring's Miri
suite (`ring/MIRI.md`), and T5 runs Miri over `sequence::`.

T1 and T2 are the `&self` write path. T3 is `Sample` and is independent
of them. T4 and T5 both edit `src/sequence/{seq,step}.rs`, so they run
after T1 to T3 and in that order.

No ABI change: `Sample` binds the ports `Input` does, and handlers and
redlines live inside the sequence.

## T1. `Writer::try_write(&self)`

Files: `ring/src/lib.rs`, `ring/DESIGN.md`, `ring/src/*tests*.rs`.

1. `Writer` gains `_unsync: PhantomData<Cell<()>>`, so it is `Send` and
   `!Sync`. `try_write` and the private `fits` and `commit` take `&self`.
2. `DESIGN.md`, beside the writer claim: "`Writer` is `!Sync`, so writes
   never overlap; that, not `&mut`, keeps one writer."
3. Existing `&mut writer` call sites compile unchanged. Fix any that
   needed `Writer: Sync` (none are expected; the compiler says).
4. `static_assertions`-style checks in a test:
   `fn assert_send<T: Send>()` passes for `Writer<NoWake>`, and a
   `compile_fail` doctest shows `&Writer` cannot cross threads.

Tests:
- two `&Writer` borrows of one writer, written in turn, read back in
  order;
- a view that only takes `latest` each round never blocks its writer
  over many laps of the ring;
- the existing loom and Kani suites still pass unchanged;
- Miri: the default and `-Zmiri-tree-borrows` runs.

## T2. `Output::write(&self)`

Files: `src/port/mod.rs`, call sites across `src/`.

1. `Output::scratch: Vec<u8>` becomes `Cell<Option<Box<[u8]>>>`, sized
   as now at `try_new`.
2. `write(&self, value)` takes the scratch, encodes, writes, and puts it
   back. If the cell is empty, it returns the new `SendError::Busy`.
3. `write_bytes(&self, ..)` needs no scratch.
4. Call sites keep `&mut Output` params. Clippy's `needless_pass_by_ref_mut`
   may flag some helpers; change those to `&Output`.
5. `LogPort` writes through the same `&self` path.

Tests (`port/mod.rs`):
- `&self` `write` round-trips a frame and a postcard record;
- a postcard `Record` whose `encode` calls `write` on the same port gets
  `Busy`, and the next write succeeds;
- a closure holding `&out` and the enclosing code both write, in order.

## T3. `Sample<T>`

Files: `src/port/sample.rs` (new), `src/port/mod.rs`,
`src/fn_system/{param,set,mod}.rs`, `src/sequence/{mod,step}.rs`,
`src/def.rs` or `src/fn_system/param.rs` (`Defs`), `src/lib.rs`.

1. `port/sample.rs`:
   - `pub struct Sample<T: Frame + Copy> { value: Rc<Cell<Option<T>>> }`
     with `get(&self) -> Option<T>`.
   - `pub(crate) trait Sampler { fn take(&mut self); }`, implemented by
     `Taker<T, W> { input: Input<T, W>, value: Rc<Cell<Option<T>>> }`.
     `take` sets the value from `input.latest()` when it returns a
     record. A decode error keeps the value and logs a `fault` line
     `("sample", reason)` through `log::Log`.
   - `pub(crate) struct Samplers(Vec<Box<dyn Sampler>>)` with `take_all`.
2. `Views<W>` becomes a struct wrapping `Bindings<InputBinding<W>>` and
   `Samplers`, with the same `next`, `take`, and `is_empty`, plus
   `sample(Box<dyn Sampler>)`. Existing `bind_in` impls change only
   their import.
3. `impl Param for Sample<T>`:
   - `append_defs` pushes `Input::<T>::def` and sets
     `Defs::sampled = true`.
   - `bind_in` binds an `Input<T, W>` as `Input` does, moves it into a
     `Taker`, pushes the taker, and returns the `Sample`.
   - `Item<'a, W> = &'a Sample<T>`.
4. `InSet<S, W>` gains `samplers: Samplers` beside `.0`. `bind` takes
   them from the `Views`.
5. `FnSystem::execute` calls `inputs.samplers.take_all()` inside the
   log scope, before `S::Params::get`.
6. `SystemDef::new_async` refuses `Defs::sampled` with
   `DefError::SampleOnAsync { system }`.
7. `SequenceStep::new` keeps the `Samplers` out of the `InSet` it moves
   into the future. `execute` takes them each cycle, before polling.
8. Re-export `Sample` from `lib.rs`. The macro needs no change: it
   strips the `&`, as for `&Seq`.

Tests:
- `port/sample.rs`: `None` before the first record, then the newest; a
  cycle with no record keeps the held value; three writes in one cycle
  sample as the last; a sampler nobody reads never blocks its writer;
- `fn_system/tests.rs`: a `#[system]` with a `&Sample<Imu>` param sees
  the value written earlier in the cycle; an async system with one is
  refused at def;
- `sequence/tests.rs`: a sequence holding `&Sample` across a
  `next_cycle` sees the new value on the next cycle.

## T4. `on_abort`, `seq.abort()`, and cancel on abort

Files: `src/sequence/{mod,seq,step,tests}.rs`, `src/lib.rs`,
`src/fn_system/param.rs` (docs only).

1. `seq.rs`:
   - `pub fn on_abort<F: FnOnce(&Seq)>(&self, f: F) -> OnAbort<'_, F>`.
     `OnAbort` is `#[must_use]`. Its `Drop` runs `f` when `aborted()`,
     through `crate::panic::catch`, and logs a caught panic as a
     `("handler", message)` fault.
   - `pub fn abort(&self) -> SeqError`, which sets the flag and returns
     `SeqError::Aborted`.
   - `set_aborted(true)` only ever raises the flag. The step no longer
     writes `false` over a flag the body raised.
2. `step.rs`:
   - `execute`: set `now`; if `abort.requested()`, raise the flag. If
     the flag is up, `end_aborted(now)`: drop `run` inside
     `with_log_port`, then `finish(Err(Aborted))`. Otherwise take the
     samples and poll, as now.
   - `fault`: log the panic as now, raise the flag, and drop `run`
     inside `with_log_port`. `catch_step` already wraps `fault` in
     `panic::catch`.
   - `SequenceStep` has no `Drop` of its own: dropping it drops `run`
     with the flag down, so `Reset`, `Load`, and shutdown run no
     handlers.
3. Module docs: an abort cancels the body at the next cycle's start, and
   handlers run then. Update the `abort` paragraph.

Tests (`seq.rs` units, then `sequence/tests.rs` through a coordinator):
- a guard runs its handler on an abort and not on a normal drop;
- handlers run in reverse order of being added;
- a panicking handler logs a fault and the next handler still runs;
- an `Abort` during a `next_cycle` loop runs the handler that cycle and
  ends `Aborted`;
- `seq.abort()` after a timeout runs the handlers and ends `Aborted`;
- a handler in a finished block does not run on a later abort;
- a panic runs the handlers, and the step still ends latched;
- `Reset` of a stopped sequence runs no handlers;
- a sequence returning `Ok` runs none.

## T5. `redline!`

Files: `Cargo.toml` (`cordyceps`), `src/sequence/{mod,redline,seq,step,tests}.rs`
(new `redline.rs`), `src/lib.rs`.

1. `Cargo.toml`: `cordyceps = { version = "0.3.4", default-features = false }`
   (the version `Cargo.lock` has). Add `alloc` if `List` needs it.
2. `redline.rs`:
   - `Node { links: Links<Node>, name, check: unsafe fn(NonNull<Node>) -> bool, seq: Cell<Option<NonNull<Seq>>>, _pin: PhantomPinned }`
     and `unsafe impl Linked<Links<Node>> for Node`.
   - `#[repr(C)] pub struct Redline<C: Fn() -> bool> { node: Node, cond: C }`
     with `new(name, cond)`. `check::<C>` casts the node pointer to
     `Redline<C>` and calls `cond`.
   - `Drop for Redline<C>` unlinks it from its `seq` if linked.
   - Each `unsafe` block has a `// SAFETY:` comment naming the pin and
     `repr(C)` arguments the design gives.
3. `seq.rs`: `Seq` gains `redlines: UnsafeCell<List<Node>>` and
   `walking: Cell<bool>`.
   - `pub fn redline<C>(&self, redline: Pin<&Redline<C>>)` links the
     node unless `walking`; it records `seq` in the node.
   - `pub(crate) fn tripped(&self) -> Option<&'static str>` sets
     `walking`, walks in link order, and returns the first name whose
     condition holds.
4. `redline!` is `#[macro_export]`ed from `sequence/mod.rs` as in the
   design, and re-exported with `Redline` from `lib.rs`.
5. `step.rs::execute`: after the samples, if `seq.tripped()` names a
   redline, log a `("redline", name)` fault, raise the flag, and end as
   T4's abort path does. Otherwise poll.

Tests:
- `redline.rs` units: a linked redline is walked, and unlinks on drop;
  link order is walk order; the first holding condition is the one
  named; a condition that links a redline leaves it unlinked; a
  `mem::forget` of the pinned frame's owner leaves no dangling node;
- `sequence/tests.rs`: a redline at the top of the body trips, writes
  the handler's record, and logs a fault naming it; a block redline
  does not trip after its block ends; a stopped sequence whose
  condition holds does not trip; a redline holding on the cycle it is
  linked trips at the next cycle's start, before the body runs again;
- Miri: `cargo +nightly miri test -p metor-fsw-3 --lib sequence::`, and
  again with `-Zmiri-tree-borrows`.

## T6. Docs

Files: `src/sequence/mod.rs`, `DESIGN.md`, `TODO.md`,
`docs/plans/13-abort-handlers-redlines.md`.

- The `sequence` module doc example gains a handler and a redline.
- `DESIGN.md`'s sequences section: handlers, redlines, `Sample`, and a
  stopped channel being inert.
- `TODO.md`: postcard samples as a follow-up.
- Shipped deviations, if any, at the end of the design doc.

## Agents

T1 and T2 go to one agent, and T3 to another in parallel, in separate
worktrees. T4, T5, and T6 go to one agent after both merge. A reviewer
on another model reads the whole slice at the end.
