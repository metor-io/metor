# Copy-in, direct attach, and the wasm bridge

Two questions, investigated against `sphw/script` at `77fcede5`. Nothing here
was changed; this is findings only.

1. Is it safe for an async system to attach directly to a producer's log ring,
   on the grounds that `LOG_DEPTH` is "deep enough to absorb a slow tap"?
2. Should the wasm `RingBridge` share machinery with the coordinator's
   `CopyIn`?

Short answers: **no, the justification is wrong** (though the blast radius is
usually hidden by an accident of sizing), and **no, do not merge the bridge
with `CopyIn`** — but there is a genuinely shared five-line rule, and it is
shared with a *third* site neither question mentioned.

---

## Q1 — the direct-attach assumption

### What the code says

`plan_copy_ins` (`src/coordinator/init.rs:932`) gives a private buffer only to
snapshot inputs:

```rust
if port.delivery == Delivery::Log || port.conn != PortConn::Edge {
    continue;
}
```

with the doc comment *"log inputs read the producers' rings directly, with no
copy-in"*, justified by `ring_config` (`init.rs:1167`): *"a log port at
`LOG_DEPTH` (an every-record stream must absorb a slow tap)"*.

`LOG_DEPTH` is `64` (`core/src/message.rs:124`). Its own doc says only *"Deep,
because a log ring must hold every record its slowest reader has not yet
seen"*. There is no rate, no lag budget, no reference to `cycle_rate`. It is a
round number, not a derived one.

### What actually happens when the ring fills

The ring is lossless with backpressure. `Writer::try_write`
(`ring/src/lib.rs:1315`) scans the reader table and refuses to lap the slowest
cursor:

```rust
if !self.fits(c, gap + rec) {
    return Err(WriteError::WouldBlock);
}
```

Nothing overwrites, and nothing blocks. The failure lands on the **producer**:

```rust
pub fn publish(&mut self, frame: &F) {
    if self.writer.try_write(frame.as_bytes()).is_err() {
        self.dropped.bump();
    }
}
```
(`core/src/port.rs:232`, and the same shape in `MsgOut::publish`,
`core/src/message.rs:177`)

and the runner folds the count into the producer's own health
(`core/src/system/mod.rs:401`, `core/src/handler/driver.rs:111`):

```rust
if self.output.take_dropped() > 0 {
    self.output.health().error("publish_dropped");
}
```

So: **the producer drops the record and the producer is charged for it.** The
slow reader loses nothing it was owed — its backlog is intact — and the cycle
loop is never held up, because every write on every path is `try_write`.

### The part the justification misses

There is one ring per output port and every consumer attaches a `View` to it
(`bind.rs:55-71` — `FanIn::One` falls through to
`BoundPort::new(alloc.output_rings[prod_id][out_idx].clone())` when there is no
private input, `FanIn::Many` always does). `Writer::fits` takes the **minimum**
cursor over the whole reader table (`ring/src/lib.rs:744`). A write refused on
behalf of one lagging reader is a write that never happens **for anybody**.

So a lagging async log consumer does not degrade its own stream. It degrades
*every* consumer of that producer's port, including cyclic ones with hard
deadlines, and it does so by silently deleting records from their input.

The codebase already knows this. The downlink drains its taps every cycle even
when no client is connected, with the reason stated in the source
(`src/telemetry/mod.rs:660`):

> With no connections the batch is skipped entirely, but the taps still drain
> below — records are consumed and DISCARDED — because **an undrained tap view
> stalls its producer's ring and freezes every consumer of that output, not
> just telemetry.**

That is the hazard, named, in this repo, in the module that defends against it.
The async direct-attach path has no equivalent defence: nothing drains an async
system's log input except the async system itself, on its own schedule, and
`Notifier` appears exactly once in `init.rs` — on the copy-in writer — so an
async system parked on a snapshot `recv` is not woken by a log record at all.

### Demonstrated

A scratch test (run, not kept): a cyclic `MsgProducer` emitting one record per
cycle, wired to *both* a healthy cyclic drainer and an async system whose
`run` never touches its log input. 60,000 cycles, simulated clock:

```
SCRATCH: cyclic peer saw 32768 of 60000 records
SCRATCH: last seq the healthy peer saw = 32768
```

The healthy peer saw a contiguous prefix and then **nothing, permanently**.
Both systems stayed "running"; no system stopped. This is not graceful
degradation — for a permanently stuck reader the port is dead for everyone,
forever, and recovery only happens if that reader eventually drains.

### How much lag does `LOG_DEPTH` actually buy?

Not 64 records, usually. The capacity is sized for the port's *worst-case*
record and then rounded up to a power of two
(`core/src/port.rs:55`, `capacity_for = (frame_len(max_size) * depth).next_power_of_two()`),
so the effective depth is `capacity / frame_len(actual_record)`:

| port kind | `max_size` | capacity | typical record | effective depth | at 100 Hz, 1 rec/cycle |
|---|---|---|---|---|---|
| fixed frame, Log delivery | = actual | `(frame_len(R)*64)` rounded up | = `max_size` | 64–127 | 0.64–1.27 s |
| message (`MsgOut`/`MsgIn`) | `MAX_MSG_BYTES` = 4096 | 512 KiB | tens of bytes | ~32,768 (measured) | ~5.5 min |

The 32768 in the scratch run is exactly `524288 / 16`. So on the message path —
which is where nearly every real log edge lives — the tolerance is roughly
**500× what `LOG_DEPTH` names**, purely because `MAX_MSG_BYTES` is a generous
worst case. That is why this has not bitten yet.

It is an accident, not a design. It evaporates for any Log-delivery *frame*
port (where `max_size` is the real size), and it scales down linearly with
records-per-cycle and up with `cycle_rate`.

### Verdict on Q1

**The assumption is not sound as stated.** Depth does not "absorb" anything; it
only sets how long the boundary takes to arrive. What is actually true, and
what the doc should say, is narrower and still defensible:

- Hitting the boundary costs **data, never timing** — the cycle loop is
  strictly non-blocking, so a slow async system cannot degrade the cycle rate.
  That much of the design is genuinely safe, and the existing
  `idle_consumer_backpressures_producer` test pins it.
- The loss is borne by the producer and is **not confined to the lagging
  reader**. Every consumer of that port loses the same records. That is the
  part the comment does not say and the part that makes direct attach a
  different risk class from the copy-in path.

Contrast the snapshot path, which is genuinely decoupled and for a reason that
is not depth at all: `run_copy_ins` (`src/coordinator/mod.rs:712`) calls
`upstream.try_latest()` every cycle, which *consumes* the backlog and frees it
for the producer. The upstream ring is drained unconditionally by the
coordinator, exactly as the downlink drains its taps. The private ring absorbs
the loss instead, and only that consumer's samples are affected. Isolation
there comes from **unconditional draining by the cycle loop**, not from ring
depth — which is the property the log path lacks.

### The accounting hole is on the other side

Q1 asked whether a lost log record is reported. It is, weakly: the producer
raises `publish_dropped` on its own health. But:

- It is not attributed. `take_dropped` sums *every* output port of the bundle
  (`macros/src/system.rs:247`), so `publish_dropped` says "this system dropped
  something somewhere", not which port or which peer.
- The doc comment at `system/mod.rs:398` reads it as "an undersized ring or a
  backpressuring reader" — the two causes are indistinguishable at the report.
- The **consumer** learns nothing. A log stream with a hole in it is
  indistinguishable from one without; nothing marks the discontinuity. For a
  delivery mode whose contract is *"every record is read, in order, never
  coalesced"*, that is the weak point.
- A handful of emitters bypass the counter entirely with `let _ = …emit(…)` —
  notably the slot's operator-facing event channel
  (`src/coordinator/slot.rs:704`) and the boot registry/manifest emissions
  (`src/coordinator/mod.rs:423,431`). Those losses are silent even on the
  producer side.

And the **copy-in path is fully silent**: `run_copy_ins` at
`src/coordinator/mod.rs:724` is

```rust
let _ = c.writer.try_write(&grant);
```

A snapshot mirror dropped because the async consumer is behind is counted
nowhere and logged nowhere. The wasm bridge, which does the same job, counts
its drops (`RingBridge::dropped`) and is strictly better here.

### `reader_slack` / `max_readers`

`max_readers = fan_out + n_receive_all + reader_slack`
(`src/coordinator/init.rs:813`); `reader_slack` defaults to 4
(`src/coordinator/mod.rs:56`). It does not touch depth. What it does do is
**license up to four more backpressure sources per ring**: `fits` scans every
claimed slot, and `RingBuffer::view` claims a slot at the current live edge, so
any tap claimed through the `Registry` after build — a recorder, a debugger, a
panel connection — and then left idle will freeze the producer for everyone
after `capacity` bytes. That is the same failure as the async log consumer,
reachable from outside the graph, and it is why the downlink's unconditional
drain is load-bearing.

### Unanticipated: async snapshot `recv` serves the *oldest* sample

`Input::recv` is `View::read` (`core/src/port.rs:345`), which is strictly FIFO.
The copy-in ring is filled latest-wins *per cycle*, but it is a ring of
`default_depth` (8) records, and a lagging async consumer reads out of it in
order. A second scratch test — producer at 1000 Hz, async consumer that sleeps
60 ms before its first `recv()`:

```
SCRATCH2: after ~100 cycles of production, the async snapshot consumer's first
recv() returned omega = 1 (freshest would be ~100)
```

It gets the **stalest** buffered sample, not the freshest. The port's declared
latest-wins semantics hold only within a cycle; across cycles a snapshot input
behaves as a short FIFO. For a control-adjacent async consumer that is a
correctness hazard, not just latency: it acts on a sample up to
`default_depth` cycles old and has no way to skip to the live edge (`recv` has
no `latest`-flavoured await). Worth deciding deliberately — either document it,
or give the async snapshot path a `recv_latest` that drains to the edge.

---

## Q2 — should the wasm bridge share with `CopyIn`?

### The overlap is real but smaller than it looks

`CopyIn` (`src/coordinator/mod.rs:245`) and `wasm::bridge::Leg` are
structurally near-identical, and the *snapshot* arm of `Leg::pump` is the same
five lines as the body of `run_copy_ins`. But `Leg` has a second arm (drain for
`Log`) with no coordinator analogue, and `CopyIn` is snapshot-only by
construction, so the literal shared surface is one of `Leg`'s two arms.

### Could inbound legs just *be* `CopyIn`s in `run_copy_ins`? No — three
reasons, in order of severity

**Ordering (fatal).** `run_copy_ins` runs *after* every `slot.step(now)`
(`src/coordinator/mod.rs:635-638`). A wasm occupant's inbound copy must happen
*before* its own `execute`, in the same cycle. Pumping it from `run_copy_ins`
adds a full cycle of latency to every wasm input and breaks the same-cycle
producer→consumer ordering that `two_system_end_to_end` pins. The bridge's
`pump_in`/`pump_out` split exists precisely because the two halves straddle the
guest call; a single global stage cannot express that.

**The memory-stability guard.** Every touch of a guest region must be preceded
by `WasmPack::check_memory_stable`, and a failure has to fail the *occupant*
(`SlotState::Stopped`), not the coordinator. `run_copy_ins` has no handle to
the `WasmPack` and no vocabulary for occupant failure. Threading it in means a
wasm-specific branch in the coordinator's per-cycle loop — exactly the bespoke
carve-out this repo avoids.

**Ring ownership and lifetime.** `copy_ins` is a `Vec` fixed at `build()`, over
rings allocated by `alloc_ring` and owned by `RingTable` for the process's
life. A guest ring is allocated by the guest's allocator at occupant load,
formatted by `fsw_pack_ring_init`, and destroyed and *replaced* on every
occupant swap. Making `copy_ins` mutable and slot-keyed is a real structural
change to the coordinator in service of one backing.

Two things that are **not** blockers, contrary to expectation:

- **The wake type.** `Writer<Notifier>` vs `Writer<NoWake>` is just a type
  parameter; both impl `WakeSource`. A shared `Copy<WD: WakeSource>` would
  monomorphise cleanly with no `dyn`.
- **Pinning / `unsafe`.** `RingBuffer::attach_raw` returns an ordinary
  `RingBuffer`; once a `View`/`Writer` is minted the copy body is entirely
  safe. The `unsafe` and the "memory must not move" obligation stay in
  `RingBridge::new`, which is where they belong. A shared type that *takes*
  already-built handles inherits nothing.

### The outbound direction

Outbound (guest ring → coordinator ring) has no analogue today and would not
gain one. Its `to` is the slot's occupant-output ring, whose writer a native
occupant claims through `RawBinder`; for a wasm occupant the *host* claims it
instead. That works — `Drop for Writer` releases the claim
(`ring/src/lib.rs:1373`) — but the claim has to be handed over correctly on
occupant swap, which is slot-lifecycle logic, not copy logic. One integration
note to keep in mind for Stage B, not an argument for sharing.

### A generalised "copy one ring to another by policy" would need too many knobs

Direction, wake type, latest-vs-drain, skip-on-unchanged, count-vs-ignore
drops, and *when in the cycle it runs*. Six knobs over a five-line body, where
the sixth cannot be expressed as a knob at all. **Recommendation: keep `Leg`
and `CopyIn` separate.**

### But there is a shared rule, and it has three instances, not two

The thing that actually repeats is not the copy. It is the **source-side
read-by-delivery policy** and its non-obvious correctness rule:
`try_latest` re-serves the pinned newest record, so a snapshot reader must skip
on unchanged `committed` or it re-forwards (and re-wakes) every cycle;
`u64::MAX` is "nothing yet". That rule is written out three times:

| site | file | shape |
|---|---|---|
| downlink tap | `src/telemetry/mod.rs:424`, drained at `:667-712` | `View` + `Delivery` + `last_committed` (+ `wire`, `retain_slot`) |
| async copy-in | `src/coordinator/mod.rs:245`, `:712` | `View` + `Writer<Notifier>` + `last_committed`, Snapshot only |
| wasm leg | `src/wasm/bridge.rs:44`, `:77` | `View` + `Writer<NoWake>` + `Delivery` + `last_committed` |

Their *sinks* are unrelated — a framed byte batch plus a retain store, a ring
write plus a wake, a ring write plus a drop count — so there is no shared pump.
Their *sources* are identical.

This is the case where sharing earns its keep, and not because of line count.
The rule is subtle, getting it wrong is silent (a re-forwarded snapshot every
cycle), and it demonstrably *has* been got wrong: `77fcede5` exists because the
first bridge forwarded uniformly and had to be corrected to split on delivery.
That is empirical evidence of a rule being re-derived, not a hypothetical.

### The concrete shape, if it is taken

One `pub(crate)` type next to `drain_view`, in `metor-fsw-2-core`'s `port`
module — where `Delivery`, `View` and `drain_view` already live, so no new
dependency edge:

```rust
/// A read view plus the delivery policy governing how it is consumed.
pub struct DeliveryTap {
    view: View<NoWake>,
    delivery: Delivery,
    /// The ring's `committed` at the last take, for snapshot taps.
    /// `u64::MAX` means nothing taken yet.
    last_committed: u64,
}

impl DeliveryTap {
    pub fn new(view: View<NoWake>, delivery: Delivery) -> Self;

    /// Hand `f` each record this tap owes: every pending record for a log
    /// tap, the newest — once per new commit — for a snapshot tap.
    pub fn take(&mut self, f: impl FnMut(&[u8])) -> Result<(), ReadError>;
}
```

Call sites become:

- `telemetry::Tap` = `DeliveryTap` + `wire` + `retain_slot`; the `execute` arm
  collapses to one `take` with the framing closure, keeping the
  `telemetry_input_corrupt` health error on the returned `Err`.
- `wasm::Leg` = `DeliveryTap` + `Writer<NoWake>`; `pump` becomes
  `take(|rec| if to.try_write(rec).is_err() { dropped += 1 })`.
- `CopyIn` = `DeliveryTap` (always `Snapshot`) + `Writer<Notifier>`; same body,
  which as a side effect **gives the copy-in path the drop counter it is
  missing** (see Q1's accounting hole).

Ownership is unchanged: every caller keeps owning its own rings and its own
writer, and `DeliveryTap` owns only the read view it was handed. No wake type,
no direction, no ordering — the three knobs that made the pump-level
abstraction wrong are all outside it.

**How strongly to recommend it:** moderately. It is a real consolidation of a
real rule across three sites, and it closes an accounting hole for free. It is
also not urgent — nothing is broken today. If it is not taken now, the trigger
to take it is a fourth site.

---

## Uncertain / not established

- **No production async system exists in-tree.** `impl AsyncSystem` appears
  only in tests and the macro (`Downlink`/`Uplink` are cyclic since the
  normalised-links work). So the Q1 failure mode is proven against the
  substrate but has no in-repo instance; how it bites depends entirely on
  user-written async systems, which I cannot survey.
- **I did not determine whether any shipped target actually wires a
  Log-delivery *frame* port** (as opposed to a message port). That is the
  configuration where effective depth really is ~64 and the tolerance is
  sub-second. Every log edge I found goes through `MsgOut`/`MsgIn`.
- **I did not measure the executor coupling.** The cycle loop and async tasks
  share one cooperative `stellarator` runtime; under `Wall` the loop sleeps out
  its budget and under `Simulated` it `yield_now`s once per cycle. Whether an
  async system doing real socket IO gets enough turns to keep a 100 Hz log
  stream drained is an empirical question I did not answer.
- The `reader_slack` / idle-registry-tap hazard is reasoned from
  `slowest_active_cursor` and `RingBuffer::view`, not demonstrated end to end
  through the `Registry`.

Both scratch tests were run and discarded; neither is committed. They are
easily rebuilt from `src/coordinator/tests.rs`'s existing `MsgProducer` /
`MsgConsumer` / `AsyncConsumer` fixtures if either failure mode is worth
pinning.
