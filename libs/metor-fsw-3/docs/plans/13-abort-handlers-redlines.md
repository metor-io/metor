# Abort handlers and redlines

Two additions to `#[sequence]` fns:

- An abort handler is a closure that runs when the sequence aborts. It
  safes hardware: close a valve, disarm an igniter.
- A redline is a condition checked every cycle. The first cycle it
  holds, the sequence aborts. A redline covers a block or the whole
  sequence.

In scope: `Seq::on_abort`, `redline!`, `&self` writes on ports,
the `Sample` param, aborts that cancel the body, and when handlers run.

Out of scope: redlines on plain systems, async handlers, samples of
postcard records, and changes to the wire or the panel.

## The borrow problem

A handler or redline outlives the statement that adds it, so whatever it
captures stays borrowed for its whole life. Today `Output::write` takes
`&mut self`, so a handler that writes `valve` holds `valve` exclusively
and the body cannot write it:

```rust
async fn burn(seq: &Seq, valve: &mut Output<Valve>) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|seq| { let _ = valve.write(&Valve::closed(seq.now())); });
    let _ = valve.write(&Valve::open(seq.now())); // error: `valve` is borrowed by the handler
    ..
}
```

If `write` takes `&self`, a closure captures only a shared borrow of
`*valve` (Rust 2021 captures the place it uses). The body's own writes
then compile beside it. Reads go through a `Sample`, which is shared
from the start. Nothing is duplicated or registered, and nothing
allocates after make.

## `&self` ports

### `Writer`

`Writer` holds `Arc<Inner>` and its wake source, and `try_write` works
only on the shared atomics. What `&mut self` guards is that two writes
never overlap. `!Sync` guards the same thing:

```rust
pub struct Writer<WD: WakeSource> {
    inner: Arc<Inner>,
    data: WD,
    _unsync: PhantomData<Cell<()>>,
}

impl<WD: WakeSource> Writer<WD> {
    pub fn try_write(&self, bytes: &[u8]) -> Result<(), WriteError>;
}
```

- Only one thread reaches a `!Sync` writer at a time, and `try_write`
  calls no user code, so it cannot re-enter itself. `WakeSource::notify`
  must not write to the ring it notifies for; `NoWake` does nothing, and
  cycle-thread outputs never wake.
- `Writer` stays `Send`, so the thread adapter still moves it.
- The loom, Kani, and Miri proofs are unchanged: writes still never
  overlap. The ring's `DESIGN.md` gains a line: `!Sync` is what keeps one
  writer.

### `Output`

```rust
pub struct Output<T: ?Sized, W: WakeSource = NoWake> {
    writer: Writer<W>,
    scratch: Cell<Option<Box<[u8]>>>,
    max_len: usize,
    _t: PhantomData<T>,
}

impl<T: Record + ?Sized, W: WakeSource> Output<T, W> {
    pub fn write(&self, value: &T) -> Result<(), SendError>;
}
```

- `write` takes the scratch out of its cell, encodes, and puts it back.
- A frame's `encode` ignores the buffer, so a frame never touches it.
- An `encode` that re-enters `write` on the same port finds the cell
  empty and gets a new `SendError::Busy`. It cannot alias, and it does not
  panic.
- `write_bytes` takes `&self` too.

### `Sample`

A redline must read the value the body reads. `drain` consumes, so a
body that drains leaves nothing for a redline, and `None` cannot say
"nothing new" apart from "nothing yet". A `Sample` is taken once per
cycle and held:

```rust
/// The newest record of an input as of this cycle, held until a newer one arrives.
pub struct Sample<T: Frame + Copy> {
    value: Rc<Cell<Option<T>>>,
}

impl<T: Frame + Copy> Sample<T> {
    /// This cycle's value: the newest record so far, or `None` before the first.
    pub fn get(&self) -> Option<T>;
}
```

`Sample<T>` is a `Param` that wraps `Input<T>`:

- It declares the same input port as `Input<T>`, so config, the def, and
  the ABI are unchanged.
- `bind_in` moves the bound `Input<T>` into a sampler, and the fn gets
  `&Sample<T>`. The two share the `Rc` cell. The sampler lives in the
  step's `Samplers`, a list of `Box<dyn Sampler>` built at make.
- At the start of each `execute`, before redlines and before the body
  or system fn, the step runs every sampler:
  `if let Some(r) = input.latest()? { value.set(Some(*r)) }`.
- A decode error keeps the held value and is logged as a fault.
- `Sample` has no `drain`. A fn that wants the stream takes an `Input`
  on another port.

What this gives:

- The body and every redline see one value for the whole cycle, however
  often they read it.
- Taking a sample calls `latest`, which releases all but the newest
  record. A sampled port holds at most one record whether or not
  anything reads it. It still stops the producer while its channel is
  stopped, as any input does.
- Staleness is the frame's own timestamp:
  `seq.now().0 - c.timestamp.0 > LIMIT` is itself a redline.
- `Input` is unchanged. `drain` and `latest` keep `&mut self`, and
  `View` needs no `Cell`.

`Param::bind_in` gains the `&mut Samplers` it pushes to. The fn-system
step and the sequence step own one. An async system taking a `Sample`
is refused at def with `DefError`, since its step never runs the
samplers.

## Abort handlers

```rust
impl Seq {
    /// Runs `f` if the sequence aborts while the guard lives.
    pub fn on_abort<F: FnOnce(&Seq)>(&self, f: F) -> OnAbort<'_, F>;
}

#[must_use = "a handler is disarmed when its guard drops; bind it with `let _name`"]
pub struct OnAbort<'a, F: FnOnce(&Seq)> {
    seq: &'a Seq,
    f: Option<F>,
}

impl<F: FnOnce(&Seq)> Drop for OnAbort<'_, F> {
    fn drop(&mut self) {
        if self.seq.aborted() && let Some(f) = self.f.take() {
            let _ = catch_unwind(AssertUnwindSafe(|| f(self.seq)));
        }
    }
}
```

A handler is a guard in the sequence's frame. It needs no registry and
no allocation: the frame is allocated once, when the step is made.

- The handler covers the scope that binds its guard. When the block
  ends normally, the guard drops unaborted and the handler is disarmed.
  This gives block handlers for free.
- Handlers run innermost first, in reverse order of being added, which
  is the order Rust drops locals in.
- Guards drop before the ports they capture, because the ports are the
  outer frame's.
- Each handler runs under `catch_unwind`. A panicking handler neither
  skips the handlers after it nor aborts the process with a panic during
  unwinding. It is logged as a fault.
- `let _ = seq.on_abort(..)` drops the guard at once. Enable
  `clippy::let_underscore_must_use` for sequence crates to catch this.

A handler needs no list: it runs when its guard drops, so nothing has
to reach it between polls. That is why it is a method returning a guard,
while a redline, which the step checks every cycle, is a macro.

## Aborts cancel the body

Today an abort makes `sleep` and `wait_for` return `Aborted`, and the
body chooses what to do. A body in a `next_cycle` loop never sees it.
With handlers doing the safing, the step cancels instead:

- `execute` reads the `abort` input as now. If the sequence is aborted,
  the step does not poll the body. It drops the body's future inside
  the log scope, which runs every armed handler, and ends `Aborted`.
- A redline trip takes the same path: the step checks redlines before
  it polls, so a trip also drops the body without polling it.
- `?` on an `Aborted` wait ends the body in the poll that sees it. Its
  guards drop on the way out, with the flag set.
- The waits keep their abort checks, for a body that catches an
  `Aborted` and awaits again.

A failure does not safe on its own. The body aborts explicitly when it
should:

```rust
impl Seq {
    /// Aborts the sequence: sets the abort flag and returns the error to end it with.
    pub fn abort(&self) -> SeqError;
}

seq.wait_for(IGNITION, lit).await.map_err(|_| seq.abort())?;
```

The `?` drops the body's guards with the flag set, so every handler in
scope runs, and the sequence ends `Aborted`.

## Redlines

```rust
#[sequence]
async fn burn(
    seq: &Seq,
    valve: &mut Output<Valve>,
    pc: &Sample<Chamber>,
    bus: &Sample<Bus>,
    temp: &Sample<Throat>,
) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|seq| {
        let _ = valve.write(&Valve::closed(seq.now()));
    });
    redline!(seq, "chamber pressure", || pc.get().is_some_and(|c| c.pressure > MAX_PC));
    redline!(seq, "bus voltage", || bus.get().is_some_and(|b| b.volts < MIN_V));
    let _ = valve.write(&Valve::open(seq.now()));
    seq.sleep(BURN).await?;
    {
        redline!(seq, "throat temp", || temp.get().is_some_and(|t| t.kelvin > MAX_T));
        seq.sleep(TAIL).await?;
    }
    let _ = valve.write(&Valve::closed(seq.now()));
    Ok(())
}
```

A redline covers the rest of the block it is declared in. At the top of
the body it covers the whole sequence; in a nested block, only that
block. Redlines stay flat however many there are, and each keeps its
name.

### The macro

```rust
#[macro_export]
macro_rules! redline {
    ($seq:expr, $name:expr, $cond:expr $(,)?) => {
        let __redline = ::core::pin::pin!($crate::Redline::new($name, $cond));
        $seq.redline(__redline.as_ref());
    };
}
```

- It expands to two statements in the caller's block. The hygienic
  `__redline` lives until that block ends, which is what scopes the
  redline.
- It exists because only `pin!` can pin a value in the caller's frame.
  The unsugared form works too, but
  `seq.redline(pin!(Redline::new(..)))` compiles and drops the node at
  the `;`. The macro rules that out.
- The name is `&'static str`, so naming never allocates.

### The list

```rust
/// One redline, pinned in the sequence's frame and linked into its `Seq`.
#[repr(C)]
pub struct Redline<C: Fn() -> bool> {
    node: Node,
    cond: C,
}

/// The type-erased head of a `Redline`, which the list links.
struct Node {
    links: cordyceps::list::Links<Node>,
    name: &'static str,
    /// Calls the `cond` of the `Redline` this node heads.
    check: unsafe fn(NonNull<Node>) -> bool,
    /// The `Seq` it is linked into, if any, to unlink from on drop.
    seq: Cell<Option<NonNull<Seq>>>,
    _pin: PhantomPinned,
}

impl Seq {
    /// Links a pinned redline; it unlinks when dropped.
    pub fn redline<C: Fn() -> bool>(&self, redline: Pin<&Redline<C>>);
}
```

- `Seq` keeps a `cordyceps::List<Node>` in an `UnsafeCell`. `Seq` is
  `!Sync`, so only the cycle thread touches the list.
- `Redline` is `!Unpin`. The pin API promises that its memory is not
  reused until its `Drop` has run, and `Drop` unlinks it. The list
  cannot hold a dangling node, even if the frame is leaked.
- `check` points at a monomorphized fn that casts the node back to its
  `Redline<C>` and calls `cond`. `#[repr(C)]` with `node` first makes
  that cast sound.
- The list is only walked between polls. At that point the body is
  suspended, and the conditions take only shared borrows (`Sample::get`
  is a `Cell` read). This relies on the same rule as every `async fn`
  that borrows a local across an `.await`: a `&mut` to a type that is
  not `Unpin` does not claim exclusive access. That rule is what RFC
  3467's `UnsafePinned` formalises.
- `Seq` has a `walking` flag. A condition that links a redline while
  the list is being walked is refused, and the node stays unlinked. A
  condition cannot unlink one, because it cannot drop what it captures.
- `cordyceps` is already in `Cargo.lock` through maitake. `metor-fsw-3`
  takes it as a direct dependency.

### Checking

At the start of each `execute`, after the samplers and the abort read,
the step walks the list in link order:

- The first condition that holds trips. The step logs a `redline` fault
  carrying its name, sets the abort flag, and drops the body inside the
  log scope. Every redline and handler guard drops. Handlers run and
  redlines unlink.
- If none holds, the step polls the body.
- A redline linked during a poll is first checked at the next cycle's
  start, before the body runs again.
- Conditions run once per cycle, on the cycle thread. They should read
  ports and compare, not wait.

## When handlers run

| How the sequence ends | Handlers run |
|---|---|
| returns `Ok` | no |
| returns a failure or timeout | no |
| `seq.abort()` | yes |
| an `Abort` command | yes |
| a redline trips | yes |
| panics | yes, on the step's `fault` |
| dropped unfinished (`Reset`, `Load`, shutdown) | no |

On a panic, `fault` sets the abort flag and drops the body inside a
`catch_unwind`.

A stopped channel is inert. Its step is not executed, so samples are not
taken, redlines are not checked, and handlers do not run. `Abort` on a
stopped channel stays refused, as `apply` does today. `Reset` or `Load`
of a stopped sequence drops its body without running handlers: the
wire's `Stopped` already says a hard stop may leave the system unsafe.
An operator who wants to safe a stopped sequence starts it and aborts
it.

## Tests

- `seq.rs` units:
  - a guard runs its handler on an abort and not on a normal drop;
  - handlers run in reverse order of being added;
  - a panicking handler still lets the next one run;
  - a redline unlinks when its block ends, and a later trip of its
    condition does nothing;
  - redlines are checked in link order, and the first that holds is
    named;
  - a condition that links a redline while the list is walked leaves it
    unlinked;
  - a leaked frame never leaves a dangling node (Miri).
- `sequence/tests.rs`, through a coordinator:
  - an `Abort` during a `next_cycle` loop runs the handler in that
    cycle and ends `Aborted`;
  - a redline trip writes the handler's record and logs a `redline`
    fault carrying its name, or `"redline"` when unnamed;
  - a handler in a finished block does not run on a later abort;
  - `Reset` of a stopped sequence runs no handlers;
  - `seq.abort()` after a timeout runs the handlers and ends `Aborted`;
  - a stopped sequence whose redline condition holds does not trip;
  - a panic runs its handlers;
  - `Ok` runs none.
- `ring`:
  - two `&Writer` calls in turn read back in order, under Miri;
  - a view that only takes `latest` each cycle never blocks its writer.
- `port`:
  - `&self` `write` writes what the `&mut` one did;
  - a re-entrant postcard `encode` gets `Busy`.
- `Sample`:
  - `None` before the first record, then the newest one;
  - a cycle with no new record keeps the held value;
  - three writes in one cycle sample as the last;
  - a sample nobody reads never fills its ring;
  - a redline and the body read the same value in the same cycle;
  - an async system taking one is refused at def.

## Decisions

1. Failures do not safe on their own. The body calls `seq.abort()` to
   abort, which runs its handlers.
2. A stopped channel is fully inert. `Abort` on it stays refused, and
   dropping a stopped sequence (`Reset`, `Load`, shutdown) runs no
   handlers.
3. Redlines are not checked while stopped.
4. `Sample` is for frames. Postcard samples are deferred.
