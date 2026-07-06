# Erase the `Backing` generic — one concrete backing with a drop fn pointer

> **Status: COMPLETE** — landed on `sphw/metor-fsw-2` (code commit `eb7f4019`,
> docs follow-up). All test suites (236 tests incl. `dl_integration`'s real
> `.so`), the mmap/no-default feature matrices, clippy, and the full Miri
> matrix (host tree-borrows, x86_64 many-seeds, i686 with leak check) pass.

Goal (user request): the ring's `Backing` trait had three impls — `BoxBacking`
(heap, drop frees), `MmapBacking` (drop unmaps; feature-gated), `RawBacking`
(non-owning dlopen attach, no-op drop) — that differed **only in how they
drop**, yet the `B: Backing` parameter was pure plumbing threaded through the
whole stack. Store the drop behavior as data (a fn pointer) instead of a type,
and delete the generic everywhere.

## The erased type (ring/src/lib.rs)

```rust
pub struct Backing {
    base: *mut u8,
    len: usize,
    ctx: *mut (),                                      // null for heap/raw
    drop_fn: Option<unsafe fn(*mut (), *mut u8, usize)>, // None = non-owning
}
```

- `Backing::heap(size)` — zeroed leaked `Box<[Word]>` (8-aligned interior-
  mutable words); `Box::into_raw` hands over the whole-allocation pointer with
  no live `Box` retained; `drop_fn` reconstructs and frees it.
- `unsafe Backing::raw(base, len)` — the dlopen attach; `drop_fn = None`.
- `Backing::mmap(map)` (feature `mmap`) — the `MmapMut` boxed behind `ctx`;
  `drop_fn` drops the box, unmapping.
- `unsafe Backing::from_raw_parts(base, len, ctx, drop_fn)` — the open
  extension point (custom arenas/allocators), paired with the new
  `unsafe RingBuffer::attach(backing)`.

One consolidated `unsafe impl Send/Sync for Backing` (the region-discipline
argument plus: `drop_fn` may run on whichever thread drops the last
`Arc<Inner>`); `Inner` auto-derives both. `RingBuffer` constructor names and
signatures are unchanged; the region layout (VERSION 2), the `FswRing` C-ABI
struct, and `FSW_ABI_VERSION` (4) are untouched.

## What the erasure deleted

- Ring: the trait + 3 structs + 6 per-type `unsafe impl`s → one struct; the
  `B` parameter left `Inner`/`RingBuffer`/`Writer`/`View`/`ReadGrant`.
- fsw-2: one type parameter removed from `Input`/`Output`/`FrameGrant`/
  `MsgIn`/`MsgOut`/`CommandOut`/`HealthPort`/`Out`/`SeqStatusOut`/
  `publish_status`/`CyclicRunner`; the `System`/`CyclicSystem`/`AsyncSystem`
  traits and `BindPorts` lost their parameter entirely (`impl System for Foo`
  is the only spelling; the same impl serves host and dlopen'd instances);
  `RingSource` lost its associated `type B`.
- Macros: the entire `__B: Backing` injection — `sig.rs`'s `backing_param`/
  `append_type_args`/`injected_backing_ident`, the `PhantomData<fn() -> __B>`
  anchors for port-less bundles (now genuinely empty structs),
  `strip_defaults`, and `#[sequence]`'s BoxBacking-descriptor vs
  RawBacking-build split. `#[sequence]` now rejects generic parameters with a
  targeted error.
- Fixtures: `dl-fixture` stopped hand-writing `impl<B: Backing> System<B>`;
  the seq fixtures dropped their phantom generics.
- One whole monomorphization axis: each dl-path bundle compiles once, not
  twice (host + occupant backing).

Net: −132 lines of code, and every port/bundle/trait signature one parameter
shorter. The wake generics (`WD`/`WS`/`RD`/`RS`) were left as-is (out of
scope; only the async coordinator path uses non-default wakes).

## Provenance note

The heap backing captures `base` **once** from `Box::into_raw` (whole-
allocation provenance, no live `Box` retained) instead of re-deriving from a
live `Box` per access. The pre-existing `raw_attach_*` tests were precedent
that a once-captured pointer over the heap allocation is tree-borrows-clean;
the post-change Miri matrix re-confirmed it, and the i686 run's leak check
proves the reconstructing `drop_fn`s free correctly.
