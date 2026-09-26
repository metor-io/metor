# Abort handlers and redlines: review fixes

Fixes for `13-abort-handlers-redlines-review.md`.

## Finding 3: notify re-entry (fixed, `34cf87ad`)

`try_write` no longer notifies after publishing wrap padding mid-write. It
notifies only as its last act, on `WouldBlock` after padding or after
`commit`, once `committed` is final. A `notify` that writes again sees a
settled writer. `WakeSource`'s doc says so. No flag or error variant.

## Finding 2: ended sequences pin their samples (fixed, `fed34140`)

`SequenceStep` drops its `Samplers` with the body when the body ends.

## Finding 1: redline captures outlive the redline

Pinning keeps a redline's memory, not what its condition borrows. Leaking
the redline (`mem::forget` of a `Pin<Box<_>>`, or of a nested future that
linked one) lets the borrowed value drop while the condition stays
linked. `redline!` alone does not close this.

Fix: `Seq` carries an invariant lifetime brand, and a condition must
outlive it.

```rust
pub struct Seq<'a> {
    clock: Clock,           // now and aborted, which the waits read
    polling: Cell<bool>,
    redlines: UnsafeCell<List<Node>>,
    walking: Cell<bool>,
    _brand: PhantomData<fn(&'a ()) -> &'a ()>,
}

impl<'a> Seq<'a> {
    pub fn redline<C: Fn() -> bool + 'a>(&self, redline: Pin<&Redline<C>>);
}
```

- The sequence receives `&'a Seq<'a>`, where `'a` is the lifetime its
  port items are borrowed for. A condition can capture ports, `seq`, and
  values moved in. Borrowing a body local fails to compile, since the
  local does not live for `'a`. Invariance stops a caller shortening `'a`.
- The step owns an `Rc<Seq<'static>>`. `Param for Seq<'static>` hands the
  body `&'a Seq<'a>` through one `unsafe` cast of the brand, which is
  phantom. It is sound because the step never checks redlines once the
  body, which holds the `'a` borrows, is dropped: `execute` returns before
  the walk when `run` is `None`, and `Seq`'s `Drop` only clears the
  back-pointers of nodes still linked.
- The waits borrow a `Clock` (`now`, `aborted`) rather than the `Seq`, so
  `Sleep`, `WaitFor`, and `NextCycle` keep one lifetime. `on_abort`
  handlers take `&Seq<'a>`.
- `#[sequence]` users keep writing `seq: &Seq`. The macro writes
  `Seq<'static>` for a bare `Seq` in the generated `Params`.

Checks:
- Miri, default and tree borrows: a redline capturing a port is linked
  inside a boxed nested future, polled once, and the future is forgotten;
  the sequence then runs on and ends, and more cycles are executed after
  the end. Also a `Box::pin`ned redline over a port, forgotten.
- `compile_fail` doctests: the review's reproduction (a condition
  borrowing a body local, then `mem::forget` and drop), and the nested
  future version.
