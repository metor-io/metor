# Running Kani on the ring buffer

Every `unsafe` deref in `src/lib.rs` cites one of two arguments: that a region
header which passed validation cannot put an offset outside the region, or that
a length field read out of the region cannot make a record straddle the wrap.
Both are pure integer claims, and both are defended in the unit tests by a
handful of hand-picked constants.
[Kani](https://github.com/model-checking/kani) is a bounded model checker: it
replaces those constants with symbolic values and proves the claim for every
input at once, including the ones nobody thought to write down.

Kani has no notion of threads. Nothing here says anything about the atomic
orderings — the registration handshake, the `hwm`-before-`committed` store
order — which belong to the loom models in `LOOM.md`. The three tools divide up
as: Kani for arithmetic and geometry, loom for orderings, Miri for provenance
and real allocation.

## What is covered

Harnesses live in `src/verify.rs` behind `#[cfg(kani)]`, grouped in four tiers.

**Position arithmetic**, over symbolic capacities, offsets and lengths, with
nothing concrete at all:

- `round_up8_correct`, `frame_len_correct` — a frame is 8-aligned, at least a
  bare header, and no more than 15 bytes larger than its payload.
- `straddle_bound_is_sufficient` — the bound `locate` puts on a length field is
  enough for the `slice::from_raw_parts` that follows it. Header, payload and
  padding all stay inside the data region. This is the safety contract of
  `View::try_read` and `View::try_latest`, discharged for every length a
  corrupt region could present.
- `straddle_bound_blocks_32bit_overflow` — the same bound also keeps `frame_len`
  from wrapping where `usize` is 32 bits. Miri needed a whole i686 run to
  *execute* this path for one input; this covers all of them on any host.
- `reserve_never_straddles` — a reserved record is contiguous, and reserving
  only moves the write position forward, by less than one lap. Discharges
  `Writer::commit`'s contiguity precondition.
- `fits_implies_no_lap` — backpressure is sound and not spurious: a write that
  passes leaves the slowest reader within a lap of the new committed position,
  and one that fails really did not fit.
- `fits_precondition_is_tight` — see the note below.

**Geometry**, against a `RegionHeader` with every field independently symbolic:

- `validate_header_hostile` — no header that validation accepts can put any
  offset the ring dereferences outside the region: capacity is maskable and
  holds a record header, the reader table sits behind the fixed header and ends
  at or before the data region, the data region ends inside the backing, and no
  intermediate arithmetic overflows.
- `slot_offsets_in_bounds`, `data_ptr_in_bounds` — the same, specialised to the
  offsets `Inner::slot` and `Inner::data_ptr` actually form.
- `layout_roundtrip` — creating and attaching agree: any config `layout`
  accepts produces a header validation accepts, with identical geometry.

**Operational**, driving a real heap-backed ring of capacity 32 with symbolic
payloads:

- `write_read_roundtrip` — a record reads back byte for byte, and consuming it
  advances the cursor by exactly its frame length.
- `backpressure_is_exact` — a write succeeds precisely when the record fits
  behind the reader, and a rejected write leaves the ring untouched.
- `wrap_gap_skip_reads_through` — a reader parked on a wrap gap reads through
  it to the record on the next lap, for every record size that can produce a
  gap. `tests.rs` covers the one size its hard-coded geometry allows.
- `try_latest_pins` — the newest record is served, the cursor parks at its
  start, and asking again with nothing new returns the same bytes.

**Corruption**, with the data region or the control words replaced by symbolic
bytes and a `Backing::raw`-style attach:

- `corrupt_data_never_ub`, `corrupt_latest_never_ub`, `corrupt_control_never_ub`
  — whatever a corrupt mapping contains, a read reports `ReadError::Corrupt`,
  reports caught-up, or hands back a slice wholly inside the data region. It
  never reads out of bounds and never panics. This is the property
  `ReadError::Corrupt` exists for, and the only way to check it exhaustively.

### The `fits` precondition

`fits` computes `committed.wrapping_sub(slowest) + need`, which is only total
while `slowest <= committed`. That is not a free assumption: `locate`'s wrap-gap
skip parks a cursor *ahead* of `committed` by design, because a wrap publishes
`hwm` before `committed`.

`fits_precondition_is_tight` proves the violation is never a safety problem —
outside the precondition the subtraction wraps and the result either overflows
the sum or reads as "full", so `fits` never reports space that is not there.
Whether a writer can observe the edge at all is a question about interleavings,
which Kani cannot answer; loom model `cursor_never_exceeds_committed_at_fits`
does, and finds it cannot.

That is why the assumption is asserted in `Writer::fits` under
`#[cfg(any(test, ring_loom))]` rather than unconditionally. The consequence of
a violation is a spurious `WouldBlock`, and turning a benign outcome into a
panic in every downstream debug build would be the worse trade.

## Running

Kani ships its own toolchain, so the `rust-toolchain.toml` pin does not apply —
the same arrangement as `cargo +nightly miri`:

```sh
cargo install --locked kani-verifier && cargo kani setup
```

The whole suite:

```sh
cargo kani -p metor-fsw-ring
```

One harness, which is what iterating looks like:

```sh
cargo kani -p metor-fsw-ring --harness validate_header_hostile
```

The whole suite takes about six minutes. The eleven arithmetic and geometry
harnesses are seconds between them; of the seven that drive a real ring, most
are under half a minute and the two slowest are `try_latest_pins` (~1m45) and
`corrupt_latest_never_ub` (~1m10).

## Keeping it fast

Nothing here is inherently expensive — the ring's loops are all bounded by
`max_readers` or by two. What blows up a harness is dragging **the allocator**
into the formula, and it is worth knowing the three ways that happens, because
each one cost real time before it was found:

- **Constructing the ring.** `create_in_memory` builds its backing by
  collecting an iterator into a `Box<[Word]>`, and behind that collect sit
  `RawVec` growth, reallocation and allocation-failure paths. The harnesses
  instead lay a region out in a stack-allocated `Region` and `attach_raw` to
  it, which exercises the same geometry, the same header validation and the
  same reader and writer paths, while leaving only `RingBuffer`'s own `Arc`
  allocating. This alone took `try_latest_pins` from over forty minutes to under
  two, and `corrupt_latest_never_ub` from not finishing at all to about a minute.
- **`try_read_into` rather than `try_read`.** The copying path resizes a `Vec`.
  The corruption harnesses use the borrowing read; `locate` is the whole of the
  pointer arithmetic all three read paths share, so bounding it bounds them all.
- **The unwind bound applies to every loop in the harness, including `std`'s.**
  A bound picked for the allocator's loops silently multiplies the cost of
  everything else. With the allocator gone, 12 is enough.

Two related habits. Fill symbolic memory a `u64` at a time rather than a byte
at a time — same arbitrary bytes, a quarter of the iterations to unroll. And
keep symbolic capacities capped (2^20 here): the predicates are mask
arithmetic, so every larger power of two behaves like the ones below it.

When a harness reports `N of M failed (M-1 undetermined)`, that is an unwinding
assertion, not a property violation — the bound is too low and CBMC could not
decide the rest. The failing check names the loop, which is often in `std`
rather than in this crate. `write_read_roundtrip` needed 12 rather than 6
because of the `memcmp` behind an `assert_eq!` on an 8-byte slice.

## Coverage

Kani reports which regions its proofs reached:

```sh
cargo kani -p metor-fsw-ring --coverage -Z source-coverage
```

Across the eighteen harnesses that reaches 50 functions in `lib.rs`. Exactly one
region is reached by no harness: the `min` closure inside
`Inner::slowest_active_cursor`, which only runs on the second active reader —
every harness here registers one. The multi-reader case is covered by the
`two_readers_slowest` loom model and the `multi_reader` unit test instead.

Worth reading alongside `cargo llvm-cov`, because the two measure different
things and their blind spots are nearly complementary. Line coverage cannot
tell "one input executed this" from "proved for every input", so it scores the
`BadVersion`, `ArchMismatch` and zero-`max_readers` rejections in
`validate_header` as uncovered — no unit test scribbles those particular
fields — when they are among the best-verified lines in the crate. Going the
other way, Kani never reaches the async paths at all.

Put together, the only lines in `lib.rs` that neither tool reaches are the
error `Display` impls.

## Why the harnesses are believable

- **No stubs.** The only `#[cfg(kani)]` in `src/lib.rs` is `owner_tag`, which
  returns `1` instead of asking for a process id that does not exist under
  verification. No function is swapped out for a model.
- **What the stack region does and does not skip.** Attaching to a `Region`
  instead of calling `create_in_memory` changes where the bytes come from, not
  what runs over them: `layout`, `init_region`, `read_header`, and every reader
  and writer path are the real ones, and `attach_raw` is itself a shipped entry
  point. What goes unexercised is `Backing::heap`'s allocation — which is
  covered by the unit tests and by Miri, where it belongs, since it is a
  provenance question rather than an arithmetic one.
- **The predicates are the shipped ones.** `reserve`, `fits` and `record_fits`
  are free functions in `lib.rs` that the writer and reader call; the harnesses
  do not re-implement them. Likewise `validate_header` is exactly what
  `read_header` runs after it reads the bytes.
- **Failures so far have been in the harnesses.** `round_up8_correct` first
  asserted `r < n + 8`, which itself overflows at the top of the input range.
  Kani reported it, correctly, as an arithmetic overflow. It is worth expecting
  that: an assertion over symbolic inputs is as much code as what it checks.
