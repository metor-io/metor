# Phase 1 — running a sequence as a WASM slot occupant

Status: **design settled, implementation in progress.** Phase 0 is done
(`spikes/wasm-poll/README.md`) and the core split landed in `89c58b33`.

## Why

A sequence today is a native `cdylib` `dlopen`'d into the FSW address space.
That gives three problems the substrate can fix and a DSL cannot:

- **Unbounded cycle time.** A poll is a synchronous call inside
  `Coordinator::run_for`; nothing stops a sequence stalling the vehicle.
- **No isolation.** The object shares the host's address space, which is the
  arrangement ESA rejected by name when choosing how to run OBCPs on Euclid.
- **A per-triple cross-compile matrix.** `wiring/pack_dist.rs` ships
  `_libs/<triple>/` and assumes installed toolchains, which is what makes
  uplinking a sequence expensive.

Phase 0 measured the cost of the fix at **0.034% of an 8.333 ms cycle**, with
port marshalling at 7 ns, so performance does not constrain the design.

## Ground truth

`cargo build -p metor-fsw-2-seq-fixture --target wasm32-unknown-unknown --release`
already succeeds — a real sequence pack builds for wasm today, off the back of
the core split. Parsing the resulting module's export section gives:

```
EXPORT memory              mem
EXPORT fsw_abi_version     func ()->(i32)
EXPORT fsw_pack_open       func ()->(i32)
EXPORT fsw_pack_describe   func (i32,i32,i32)->(i32)
EXPORT fsw_pack_create     func (i32,i32,i32,i32,i32)->(i32)
EXPORT fsw_pack_bind_init  func (i32,i32,i32,i32,i32,i32,i32)->()
EXPORT fsw_pack_execute    func (i32,i64)->(i32)
EXPORT fsw_pack_shutdown   func (i32)->()
EXPORT fsw_pack_destroy    func (i32)->()
EXPORT fsw_pack_close      func (i32)->()
EXPORT __data_end, __heap_base   global
```

Two facts matter more than the rest:

1. **The module imports nothing.** It is a closed artifact — no host functions,
   no WASI. That is exactly what we want to uplink and to defend in a review.
2. **Every ABI entry point survives the port unchanged.** Pointers became `i32`
   linear-memory offsets and `now: u64` became `i64`, which is precisely what
   the ABI's own rule predicted: *"only serialized bytes and `repr(C)` handles
   cross the boundary… everything else is a `(pointer, length)` pair or a plain
   integer."* The ABI was accidentally well-designed for this.

## The three design problems, and their resolutions

### 1. `ByteSink` cannot cross to wasm — **change describe for everyone**

`fsw_pack_describe(pack, sink, ctx)` takes `ByteSink = extern "C" fn(ctx, buf,
len)` — a host **callback** the guest invokes to hand over manifest bytes. On
wasm a function pointer is an index into `__indirect_function_table`, and that
table is not exported, so a host cannot install a callback into it. Describe is
therefore unusable as-is.

Options considered:

- **(a) Export the indirect function table** and install a host funcref. Works,
  but needs a linker flag on every pack, and gives the module a host import —
  losing the closed-artifact property above.
- **(b) Add a second, wasm-only describe entry point.** Cheap, but it is exactly
  the bespoke carve-out this codebase avoids; two describe paths would drift.
- **(c) Replace `ByteSink` with a buffer return, on both paths.** The guest
  postcard-encodes the manifest as it already does, leaks it as a boxed slice,
  and returns `(ptr, len)`; the host copies and calls a new
  `fsw_pack_free_bytes(ptr, len)`. One path, native and wasm.

**Take (c).** It removes a callback from the ABI rather than adding a special
case, and the "each side frees only what it allocated" rule is preserved by the
explicit free. This bumps `FSW_ABI_VERSION` (10 → 11).

### 2. Where the rings live — **inside guest linear memory**

`FswRing { base, len, role }` is documented as self-describing: *"Capacity, data
offset, reader-table offset, and reader limits are all in the region header, so
the handle is just base, length, and role."* On wasm `base` is a guest offset.

So the rings live **in the guest's own linear memory**, and `fsw_pack_bind_init`
is reused verbatim. There is **no per-cycle marshalling protocol** — the ring is
still the shared medium, it has merely moved into guest memory, and the host
reads and writes the same records through `memory.data_mut()`. Backpressure,
grants and sequence numbers all keep working untouched.

The regions must be allocated by the *guest*, not carved out of host-grown
pages: Rust's wasm allocator discovers the heap through `memory.size`, so pages
the host grows behind its back can later be handed out by `dlmalloc`. The pack
ABI therefore gains `fsw_pack_alloc(len) -> ptr` (and a matching free), which is
standard practice for wasm interop and keeps ownership on the side that manages
the heap.

### 3. The wasmi borrow problem — **rebuild the accessor per access**

The host's ring `Writer`/`View` want a `&mut [u8]` over the region, but wasmi
hands out guest memory only as `memory.data_mut(&mut store)`, borrowed from the
store. A `RingBuffer` cannot be held across calls the way the native path holds
one over a mapped region.

Resolution: the host keeps only `(offset, len, role)` per port and re-derives
the slice on each access, attaching a `RingBuffer` over it for the duration of
that read or write. Phase 0 measured the whole port copy at 7 ns against a
2,873 ns cycle, so re-attaching per access is far below the noise floor. The
cost is that the host cannot cache a `Writer`'s internal cursor across cycles —
which is fine, because the cursor state lives in the region header, not in the
`Writer`.

## Fuel

`Config::consume_fuel(true)` plus `Store::set_fuel` before each poll. Fuel must
be granted **before instantiation** — the start section is itself metered, and
an ungranted store traps immediately (this cost the Phase 0 spike three runs).

Phase 0 measured **7,449 units for one poll** of a math-heavy commissioning
ladder. The slot's default budget should sit well above that with room for a
much heavier sequence; the knob exists so an operator can tighten it per slot.
Exhaustion is a trap, and a trap is a clean terminal state — never a host crash.

## Staging

- **Stage A** — the ABI change (1), the allocator entry point (2), and a host
  loader that drives `open → describe → create → bind_init → execute →
  shutdown → destroy → close` against the real `seq-fixture` module, proven by a
  test that reaches a terminal run state.
- **Stage B** — that loader as a third `OccupantBacking` beside in-process and
  `Artifact`; fuel as a slot knob; trap and exhaustion mapped to
  `SlotState::Stopped` / `Outcome::Failed` with a `SequenceChannelEvent`;
  slot tests mirroring `tests/slot_integration.rs`, **including fuel exhaustion
  and a sandbox violation**. Those two are the tests that justify the whole
  substrate.

## Deferred

- `wiring/pack_dist.rs` gaining a `wasm32` target to collapse the per-triple
  matrix. This is where the uplink win is realised, but it is independent of the
  runtime work.
- Python config surface in `metor_config`.
- `spikes/wasm-poll` still carries a hand-rolled ~30-line reimplementation of
  the sequence runtime, which the core split has made unnecessary — the guest
  could now use the real `sequence` module. Left alone deliberately: it is a
  measurement scaffold, and rewriting it proves nothing new.
