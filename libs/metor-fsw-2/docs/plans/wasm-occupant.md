# Phase 1 — running a sequence as a WASM slot occupant

Status: **Stage A done.** Phase 0 is done (`spikes/wasm-poll/README.md`) and
the core split landed in `89c58b33`.

A real sequence pack, compiled to wasm, now runs the whole ABI lifecycle to a
terminal state under an interpreter, and **fuel exhaustion and sandbox
containment are both proven by tests** — the two properties the substrate
exists for. Stage B (the third `OccupantBacking`, the slot knob, slot tests) is
next.

### What wasm actually cost, beyond the design below

Three guest-side assumptions had to go, all the same shape: the target has no
operating system, and the framework quietly assumed one.

- **`std::process::id`** stamps a ring's writer and reader claims
  (`owner_tag`). Unsupported on wasm, so *every* port bind panicked. A guest is
  not a process anyway — its regions live and die with the instance, so there
  is no peer to outlive them and nothing to reclaim.
- **`Instant::now`** timed every execute for `SystemHealth`. Unsupported, so
  every cycle panicked. A guest now reports zero; a sandboxed occupant's real
  cost is better read from its fuel draw, which the host meters anyway.
- **`Timestamp::now`** is the fallback when the ambient clock is unset, which
  it is during `bind`/`init`. Merely inaccurate in a `.so`, fatal in wasm.
  Hence `fsw_pack_set_now`, which the host calls before bind.

None of these were visible as themselves. A guest panic surfaces only as
`unreachable`: the module imports nothing, so a panic message has nowhere to
go, and `wasm32-unknown-unknown` aborts rather than unwinds, so the
`catch_unwind` in `run_pack_bind_init` cannot convert it into a status word.
Diagnosis was by elimination — bisecting on mount kind, on entry, and on
oversupplied rings — and **that is the technique to reach for first next time**,
not host-side inference.

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
- **(c) Replace `ByteSink` with a buffer the pack owns, on both paths.** One
  path, native and wasm.

**Take (c)**, in a two-call form:

```
fsw_pack_describe(pack)     -> i64   // encode + stash; byte length, or -1
fsw_pack_manifest_ptr(pack) -> *const u8
```

The obvious single-call shape — return a `repr(C) { ptr, len }` — does not work
here. Rust returns an aggregate through a caller-supplied out-pointer, so the
host would need to allocate *guest* memory to receive it, and it cannot: the
allocator entry point is discovered from the manifest that describe has not
handed over yet. Splitting the call breaks that circularity, and both halves are
plain scalars.

The pack keeps the encoded bytes alive until `fsw_pack_close`, so "each side
frees only what it allocated" still holds without an explicit free. This bumps
`FSW_ABI_VERSION` (10 → 11).

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

## Stage B's crux: connecting a guest occupant to the graph

Stage A bound an occupant to rings in its *own* linear memory and cycled it in
isolation. Stage B has to join it to the rest of the target, and that turns out
to be the hard part — harder than the design above assumed.

**The "re-derive per access" resolution of §3 is only half right.** It holds for
writes. It does not hold for reads: a `View` claims a reader slot, its `Drop`
frees it, and a freshly attached view **joins at the live edge** rather than
replaying a backlog (`ring::tests::create_raw_formats_a_caller_owned_region`
pins exactly this). A reader's cursor is slot state, so re-attaching each cycle
silently drops every record written since the last one. Handles that read must
therefore persist across calls.

That rules out the obvious shape and leaves one real constraint: **the host must
hold `RingBuffer` handles over guest linear memory for the occupant's whole
life**, which is sound only while that memory does not move. `wasmi` reallocates
its backing buffer on `memory.grow`, and a guest's allocator will grow if it
needs heap — a sequence calling `progress()` allocates a `String`, so this is
not hypothetical.

### Can a guest be given host memory? No — and the asymmetry decides this

A wasm guest can address only its own linear memory; nothing maps host memory
into it. The privilege runs one way: the *host* can read and write the guest's
entire memory (`Memory::data_mut`), which is how `WasmPack` stages rings and
reads the manifest already.

So the two shapes are:

- **(a) Copy bridge.** The coordinator keeps its own host rings. Each cycle the
  host drains new records from a host input ring into the guest's, and from the
  guest's output rings back out. Persistent handles both sides; a copy per
  record; every ring duplicated.
- **(b) The slot's rings *are* guest regions.** The guest allocates them and
  attaches normally; the host attaches to the same bytes through the backing
  buffer, and coordinator-side systems read and write them directly. No copy.
  Legal only because `arch_tag` stopped encoding pointer width (`a7a28986`).

**Take (a).** (b) is the more elegant shape and was the initial preference, but
it puts the rings *inside the sandbox*: a buggy or hostile guest can corrupt
headers and cursors that native systems are consuming. Reads stay bounds-checked
against the region, so it is not host memory-unsafe, but data integrity is gone
and a corrupted cursor feeds garbage downstream. That trades away the property
the substrate exists for — a fault contained to the faulting occupant. Under (a)
the coordinator's rings are host-owned, a guest can corrupt only its own copy,
and the host validates at the copy boundary.

The same objection sinks the variant where the host creates a `Memory` and the
module imports it: still a guest-addressable memory, same exposure, and it costs
the closed-artifact property (the module currently imports nothing).

The copy is affordable. Phase 0 measured marshalling at 7 ns against a 2,873 ns
cycle.

Either way the guard from `dfde295f` is required, since (a)'s guest-side handles
must persist too. A pack built with a large enough initial memory would never
trip it, which is the hardening to reach for if growth shows up in practice.

## Staging

- **Stage A** — the ABI change (1), the allocator entry point (2), and a host
  loader that drives `open → describe → create → bind_init → execute →
  shutdown → destroy → close` against the real `seq-fixture` module, proven by a
  test that reaches a terminal run state. *Bind is the open blocker.*

  Design point (3) needed one correction in practice: **the guest must format
  its own ring regions**, via a new `fsw_pack_ring_init`. A region header
  records the writing target's pointer width, and a wasm guest's `usize` is
  four bytes where its 64-bit host's is eight, so a host-formatted region is
  rejected on attach as `ArchMismatch`. This also means the host cannot
  currently attach to a guest-formatted region either — reading a guest's
  output rings host-side is unsolved, and Stage B needs it. The likely
  resolution is narrowing the architecture tag to what actually affects the
  layout (endianness), since every header field is explicit-width and the
  capacity-fits-`usize` case is already caught separately as `BadGeometry`.
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
