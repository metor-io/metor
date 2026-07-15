# metor-fsw-2 — Multi-agent review findings (2026-07-01)

Six parallel reviews (ergonomics, internal cleanliness, architectural symmetry,
safety, bugs, Rust style) over `libs/metor-fsw-2`, `libs/metor-fsw-2/ring`, and
`examples/adcs-fsw2`. Findings were cross-checked between agents; the one
direct conflict (ring torn-read) was re-verified by hand against the code —
see R1. Each item has an ID for tracking fixes.

Overall posture: the core data plane (ring, ports, dl/abi handshake) is
disciplined — every unsafe block has a real SAFETY comment, the FFI layer is
panic-safe, docs are thorough, clippy is nearly clean. The issues cluster in
four places: (1) a handful of real correctness bugs at the newer seams, (2) a
torn-read window in the ring's overwrite mode, (3) the message plane having
been added as a *twin* of the frame plane rather than a unification, and (4)
authoring ceremony that the existing `#[sequence]` macro proves is derivable.

---

## 1. Correctness — confirmed bugs (fix first)

### R1 — [HIGH] Ring overwrite-mode torn read accepted as valid
`ring/src/lib.rs:994-1011` (`try_read_into` recheck) vs `:917-930` (`commit`).
The post-copy recheck discards the snapshot only when the **published**
`committed` moved past `r + capacity` (`c2 - r > cap`). But `write_record`
scribbles data bytes *before* the `committed` Release store. With the writer
exactly one lap ahead (`committed == r + capacity` — legal, not lapped), its
next write targets `phys(r)`; a reader copying concurrently rechecks against
the still-unpublished `committed` and returns `Ok(true)` with an old/new byte
mix presented as a valid frame. No UB, but silent data corruption; reachable
by any reader a full lap behind (async copy-in rings, telemetry taps).
**Verified by hand.**
Fix: seqlock-style pre-write reservation — writer Release-stores
`start_abs + rec` into the already-reserved `OFF_RESERVED_END` word (`:60`)
before `write_record`; reader rechecks against `reserved_end` instead of
`committed`.

### B1 — [HIGH] KDL slot occupant params can never resolve
`src/wiring/mod.rs:1147-1160` (`encode_kdl_params`) vs `:1442-1446`
(`parse_slot`). Params given as child nodes (`allow occupant="x" { gain 0.8 }`,
the syntax the parser test asserts) hit `encode_kdl_params`, which (a) rejects
the `occupant=` property as `DlUnknownParam`, and (b) reads only line
properties, never child nodes — so every non-`Option` field fails
`DlMissingParam`. Conversely, params written as line properties are classified
`ParamSource::None` and silently dropped. No test resolves a parametered
occupant, which is how this slipped through.
Fix: translate child param nodes into properties on a synthesized node (or
teach `encode_kdl_params` a per-node-kind reserved-key set + child reading);
treat line properties on `allow` as params; add the missing test.

### E1 — [HIGH] Registration order silently determines data freshness
`src/coordinator/mod.rs:1728` (cyclic step loop is registration order),
`src/wiring/mod.rs:786` (resolve registers in KDL document order). There is no
topological sort; cycle detection only rejects cycles. Declaring `nav` before
`plant` in mission.kdl builds fine, but `nav` reads `plant`'s previous-cycle
output forever — the exact staleness `connect_delayed` claims to make
explicit. Nothing documents a declaration-order requirement.
Fix: topologically sort cyclic systems from the non-delayed frame edges at
`build()` (the DAG already exists for cycle detection), or return a
`WireError` when a non-delayed edge points backward in registration order.

### B2 — [MEDIUM] `update_status` misses equal-length stopped-set changes
`src/coordinator/mod.rs:1832` — change detection is `cur.len() ==
self.stopped.len()`. Stops are no longer monotonic (slots can recover via
`Load`/`Reset`, `slot.rs:331-347,388-403`): if slot A recovers the same cycle
slot B stops, the status frame and B's `system_stopped` health error are never
published. Fix: compare membership (name + reason), not length.

### B3 — [MEDIUM] Simulated clock truncates cycle index to u32
`src/coordinator/mod.rs:1722` — `epoch + dt * k as u32`. In `Simulated` mode
the loop free-runs; past 2³² cycles `now` jumps back to `epoch`, breaking
monotonicity, telemetry ordering, and stalling any in-flight `Wait` forever.
Fix: accumulate `now += dt` per cycle or compute in u64 nanos. (Style agent
found the same cast independently, plus `(slot as u16)` packet-id wrap at
`src/telemetry/mod.rs:741`.)

### B4 — [MEDIUM] `resolve_static` silently discards typed builder params
`src/wiring/mod.rs:872-876` — `SystemSpecBuilder::params(NavParams{..})` on a
static system produces `ParamSource::Postcard`, which `resolve_static` matches
to `minimal()`, dropping the params (all-optional params silently run with
defaults). Fix: reject `Postcard` params on static systems with a dedicated
`LoadError`, or decode them.

### B5 — [MEDIUM] Slot occupant names never validated
`src/coordinator/slot.rs:338-340` and no check in `resolve_slot`/`add_slot`.
A misspelled `initial occupant="…"` boots the slot `Empty` with no
diagnostic; a runtime `Load` with an unknown name is swallowed with no event
or health error. Fix: validate `initial.occupant` against the allowed set at
resolve/build; emit an event/health error on unknown runtime `Load`.

### R2 — [MEDIUM] `FswStatus` built from a raw FFI return — invalid discriminant is UB
`src/dl.rs:51,332,367` — `fsw_execute` from a foreign `.so` is trusted to
return one of four `repr(u32)` discriminants; any other value is instant UB.
The one place a Rust validity invariant crosses the dlopen boundary.
Fix (trivial): declare the fn as returning `u32`, convert with
`match { 0..=3 => …, _ => FswStatus::Panicked }`.

### R3 — [MEDIUM] 32-bit target: garbage record length overflows to in-bounds
`ring/src/lib.rs:104-106,1081-1090` — under an ordinary lap race `read_len`
can return payload bytes as a length; on 32-bit, `round_up8(0xFFFF_FFFF)`
wraps to `rec = 8` in release, the straddle check passes, and `copy_payload`
copies ~4 GiB out of bounds (UB). Sound on 64-bit (verified). Flight targets
are plausibly 32-bit. Fix: do length/record math in u64 and reject
`len > capacity - 8 - phys` before constructing `Located`.

### R4 — [MEDIUM] `attach_mmap`/`attach_raw` don't validate geometry vs region length
`ring/src/lib.rs:605-671,807-838` — header magic/version/arch are checked but
not `total_size <= backing.len()`, capacity nonzero power-of-two, data region
+ reader table in bounds, or 8-alignment of `base`. A truncated-on-disk file
passes validation then walks past the mapping. Fix: a few compares in
`read_header`/`from_validated`, new `AttachError` variants.

### B6 — [LOW] Spurious permanent lap for a reader parked on a wrap-gap start
`ring/src/lib.rs:978,1065` — both lap checks run before the `r == hwm` gap
skip, so for `c ∈ (r+cap, r+cap+gap]` a reader whose next real byte is intact
is declared lapped — a spurious, unrecoverable hard-stop for a cyclic system.
Fix: perform the gap skip before the lapped test (treat `r == hwm` as
`r = lap_end` for the comparison).

### B7 — [LOW] Duplicate message edge delivers every record twice
`src/coordinator/mod.rs:867-872` — frame edges reject double-connect; message
edges append unconditionally, so a copy-pasted edge double-applies commands.
Fix: dedupe/reject exact-duplicate `(producer, out_idx)` per message input.

### B8 — [LOW] `cycle_rate <= 0` panics at run time
`src/coordinator/mod.rs:1710` — `Duration::from_secs_f64(1.0/rate)` panics on
inf/negative; computed even under `Simulated` where rate is documented as
ignored. Fix: validate at `build()`; compute the budget only in the `Wall` arm.

### B9 — [LOW] Telemetry misses frames emitted during other systems' `init`
`src/telemetry/mod.rs:721-753` + `src/coordinator/mod.rs:1755-1796` —
telemetry's views start at the live edge after earlier inits ran; init-time
emits are never downlinked (the boot `SequenceRegistry` carefully dodges this,
suggesting it's a known hazard). Fix: claim views at `build()`, or document.

### B10 — [LOW] Self-loop frame edge bypasses the feedback-cycle doctrine
`src/coordinator/mod.rs:862` — `p.system.id != c.system.id` exempts
self-edges from cycle detection, so plain `connect` of a system to itself
builds without `connect_delayed`. Fix: require `connect_delayed` for
self-edges (or document).

### B11 — [LOW] Repeated `run_for` re-runs every cyclic `init`
`src/coordinator/mod.rs:1755-1789` — a second `run_for` re-inits everything
(telemetry re-claims reader slots, transport already taken → silently no
sender; async systems from the first run are gone). Fix: guard/latch if
multiple runs are unsupported.

### R5 — [LOW] Null `fsw_create` builds a permanently-"Running" zombie slot
`src/dl.rs:269-282,362-364` — the build-time `CyclicSlot::step` path
early-returns on null leaving `SlotState::Running`; the failure is never
telemetered. Fix: set `Stopped(Panicked)` when `state.is_null()`.

---

## 2. Ring crate API soundness (latent — framework doesn't hit these today)

### R6 — [CRITICAL, latent] Lossless `View` registration races the writer → OOB read from safe code
`ring/src/lib.rs:710-737,1035-1092` — `view()` loads `start = committed` then
CAS-claims a slot; until the writer observes the claim, `fits()` is vacuously
true and it may lap past `start + capacity`. Lossless `locate` has no lap or
straddle check (both gated on `Overwrite`), so `read_len` on overwritten bytes
yields an arbitrary length → `from_raw_parts`/`copy_nonoverlapping` OOB (UB),
all via safe API. Also a store-buffer variant: no SeqCst fence orders the
cursor store vs the committed load. **Not exercised: the framework only
creates `Overwrite` rings.**
Fix: after CAS, re-load `committed` and fail/resync if `committed - start >
capacity`; add SeqCst fences both sides; add the straddle/length sanity check
to the lossless `locate` path. — Or see C1: delete lossless mode.

### R7 — [HIGH, latent] Single-writer rule unenforced — two writers on a lossless ring are a data race (UB)
`ring/src/lib.rs:697-703,844-931` — `RingBuffer` is `Clone`, `writer()` is
safe and callable N times; the `Send/Sync` justification assumes a discipline
nothing enforces. Overwrite mode degrades to logical corruption (atomic
stores); lossless mode is genuine UB (plain writes). Framework call sites are
correct today. Fix: CAS a `writer_claimed` control word (freed on
`Writer::drop`), or make `writer()` one-shot/unsafe.

### R8 — [LOW] Reader-claim CAS ordering + unvalidated epoch word
`ring/src/lib.rs:718-727` — fine today, but if epoch-based stale-handle
reclamation is ever implemented the claim must be AcqRel/SeqCst and the epoch
checked per cursor store. Bump the ordering or leave a comment now.

---

## 3. Architecture — conceptual simplification (biggest leverage)

### A1 — [HIGH] Frames vs messages are two parallel type stacks, not one concept
`src/message.rs:69,205`, `src/registry.rs:26-185`,
`src/telemetry/mod.rs:319-416,654-670`, `src/coordinator/mod.rs:1481-1525`.
~10 twin types/functions, near-verbatim copies (`Output`/`MsgOut`,
`Input`/`MsgIn`, `OutputRegistry`/`MessageRegistry`, `HandOff`/`MsgHandOff`,
`Tap`/`MsgTap`, `RegistryEntry`/`MessageEntry`, `capacity_for`/`msg_capacity`,
`coord_ring`/`msg_ring`, `matches`/`matches_message`). The real behavioral
differences (fan-in 0..N vs exactly-1, lap = resync vs stop, cycle-detection
inclusion, every-record vs latest-wins) are *independent axes* bundled into
the frame/message split.
Proposal: factor into orthogonal primitives — schema (`Table(VTable)` |
`Postcard(PacketId)`) × delivery (`Snapshot` | `Log`) × cardinality (`One` |
`Many`) on one `PortDesc`; one generic `Registry<E>` (C4 is the mechanical
first step); one generic hand-off. Frames and messages become two
configurations of one port concept.

### A2 — [HIGH] Command plane survives as type-keyed magic inside `build()`
`src/coordinator/mod.rs:961-965,1131-1155,1304-1315,1669-1671` — three
hard-coded `PortId::Msg(SequenceCommand::ID)` checks: any system that declares
`MsgOut<SequenceCommand>` silently becomes a command source for **every**
slot, with no edge, no `connect_msg`, no opt-out — the one place dataflow is
invisible in the wiring. The coordinator-owned `command_ring` behind
`control_handle()` lives outside every model surface and explicitly abandons
the single-writer discipline (the only writer-side invariant violation in the
crate). DESIGN.md:406-411's "no coordinator-side command stage" is true of
the runtime loop but false of build.
Proposal: give slots a declared `MsgIn<SequenceCommand>` in their descriptor;
make command edges explicit (per-slot `connect_msg` or a first-class broadcast
connect); make `control_handle` an ordinary coordinator-owned output port;
delete all three type-keyed checks.

### A3 — [HIGH] Slots are a third system kind with five undeclared aux resources
`src/coordinator/mod.rs:425-435,709-712,1020-1109,1259-1329`,
`src/coordinator/slot.rs:59-88,181-230` — slot support needed a special
`SlotAux` pass (control ring, `SlotStatus` ring, events `MsgOut`, self-status
read view, `ChannelId`), a position-sensitive descriptor rewrite (pop/re-append
of the trailing `SlotControlIn`), a duplicated lifecycle enum
(`SlotPhase`/`SlotState` + `sync_slot_state`), and a leaked `&'static str`
name per build. None of the aux resources appear in the slot's descriptor —
"a system is its descriptor" breaks exactly here. Worst: `ChannelId` is the
slot's *build-order index*, leaking into the wire protocol — reordering the
wiring file silently re-addresses commands from the ground.
Proposal: put `SlotControlIn`/`SlotStatus`/events *in* the registered
descriptor as ordinary host-connected ports; address slots by instance name or
registry key, not a positional integer; collapse the two lifecycle enums.

### A4 — [MEDIUM] `ReceiveAll` is a pseudo-port with a sentinel identity and placeholder ring
`src/descriptor.rs:202-210`, `src/coordinator/mod.rs:920-925,981-991`,
`src/telemetry/mod.rs:608-630` — its id is `PortId::Frame(ComponentId::new(""))`,
`build()` allocates a throwaway ring to keep the positional binder aligned,
and the read capability is declared in the **output** bundle because
`System::init` only receives outputs. `n_reg` counts it in inputs too, but an
input-bundle `ReceiveAll` would be rejected as `UnconnectedInput`.
Proposal: lift capabilities out of the port lists — a `capabilities:
Vec<Capability>` field on `SystemDescriptor` with its own bind path.

### A5 — [MEDIUM] Lap/backpressure policy is a hidden 2×2 matrix keyed on (port kind × system kind)
`src/system/mod.rs:293-300`, `src/coordinator/mod.rs:1163-1212`,
`src/message.rs:245-251`, `src/port.rs:176-184` — frame+cyclic = permanent
stop; frame+async = invisible coordinator-inserted copy-in ring; message =
silent resync, with `MsgIn::is_lapped()` hard-coded `false` to lie to the
framework's check. Proposal: make loss policy an explicit per-port parameter
(`OnLap = Stop | Resync`) declared where the port is declared.

### A6 — [MEDIUM] `telemetered` flag is message-only, advisory, and duplicated by a second opt-out
`src/message.rs:134-155`, `src/descriptor.rs:60-81,186-197`,
`src/registry.rs:121-123,198-203` — opting out of downlink requires a whole
wrapper type (`CommandOut<M>` = `MsgOut<M>` + one bool); frames have no
opt-out; enforcement is consumer convention ("skip `!telemetered` entries");
the coordinator's own command ring is untelemetered by simply never being
registered. Proposal: plain `telemetered` field on every `PortDesc` (kills
`CommandOut`), filtered at the registry/`AllOutputs` source.

### A7 — [MEDIUM] `connect_msg` is byte-identical to `connect`; edge rules enforced by port id, not API
`src/coordinator/mod.rs:744-797` — both call `push_edge(p, c, false)`;
`connect_delayed` on a message edge is accepted and silently meaningless.
Proposal: delete `connect_msg` (kind inferred from ports, as the KDL front-end
already does); reject `delayed` on message ports as a `WireError`.

### A8 — [MEDIUM] Uplink generality is illusory; doc and code diverge
`src/telemetry/mod.rs:490-502` vs `:530-533` — `uplink_subscribe_ids()`
derives subscriptions from declared outputs and promises "a second output
just works", but `run()` hard-filters `SequenceCommand::ID` and emits only on
the single `commands` port — a second output would subscribe then drop every
record. Proposal: dispatch received Msgs by declared output id, or delete the
generic machinery and state the uplink is `SequenceCommand`-only.

### A9 — [LOW] Coordinator is a half-system with hand-rolled ports
`src/coordinator/mod.rs:998-1018,1111-1142,1396-1405` — health/log/status/
registry/command channels are manually allocated, registered under a
synthetic instance, and wrapped — a non-derived reimplementation of what
every system gets. Proposal: model the coordinator as system #0 with an
ordinary declared bundle.

### A10 — [LOW] Message wire identity derives from `std::any::type_name`
`src/descriptor.rs:136-146` — a message channel's registry key and KDL token
come from the Rust type's last path segment: renaming a type silently changes
wire identity and breaks mission files; generics yield broken tokens like
`"Baz>"` (style agent, same line); `type_name` format isn't guaranteed.
Proposal: require an explicit name on `Msg` (like frames' `name = "…"`), or
key on the hex `PacketId`.

### A11 — [LOW] Doc/model divergences
(a) docs/system.md + DESIGN.md say inputs are "read-only views";
`execute` takes `&mut Self::Input`. (b) "downlink registered last" is stated
but unenforced — `add_telemetry` before other systems silently yields stale
snapshots (relates to E1's ordering theme). (c) `Wiring::uplink` has no KDL
surface, so the "both front-ends are equivalent" claim fails.
(d) "no coordinator-side command stage" vs A2. Fix docs for (a)/(d); enforce
or error for (b); add a KDL `uplink` node or drop the claim for (c).

---

## 4. Ergonomics

### E2 — [HIGH] ~35 lines of per-system ceremony that `#[sequence]` proves is derivable
`examples/adcs-fsw2/systems/nav/src/lib.rs:33-114` — two bundle structs
threaded with `B: Backing`, `System` + `CyclicSystem` impls, a
pure-delegation `BuildSystem`, feature-gated `export_system!`, a clippy
allow, cdylib Cargo ceremony. The `#[sequence]` macro already derives
descriptor+bind+ABI from an fn signature; a `#[system]` attribute could
collapse the bundles, the trait split, the `Backing` generic, and
`BuildSystem` into one annotation. Related: E6 (`Out<>` wrapper), E7
(hand-written `<B: Backing>` on sequence fns).

### E3 — [MEDIUM] `Input::latest()` returns `Result<Option<_>>`; every caller swallows both axes
`src/port.rs:216-225` — all example code writes
`let Ok(Some(s)) = x.latest() else { return }`, conflating "no data yet" with
a lap fault; for cyclic systems the coordinator already hard-stops on lap
before `execute`, so the `Err` arm is unreachable noise taxing every call
site. Proposal: cyclic-flavored `latest() -> Option<FrameRef>` (lap routed to
health internally); keep `Result` on async `recv`/`drain`.

### E4 — [MEDIUM] Frame definition: six derives, `#[repr(C)]`, hand-written padding; omissions fail far away or never
`examples/adcs-fsw2/contracts/src/lib.rs:148-150,373-384`, `src/frame.rs:14`,
`src/port.rs:97,207` — `Frame` has no zerocopy supertraits, so forgetting
`FromBytes` on a contract type errors opaquely in some *consumer* crate.
Missing `#[metor_fsw(timestamp)]` silently stamps everything `Timestamp(0)`
(`metor-fsw/macros/src/frame.rs:52-58`). Proposal: zerocopy bounds on `Frame`
itself; derive errors on implicit padding and on missing timestamp (explicit
`no_timestamp` opt-out).

### E5 — [MEDIUM] KDL surface rough edges (cluster)
(a) `type=` mandatory yet never validated for dl systems and duplicates the
artifact's (`src/wiring/mod.rs:901-944,1340`) — make it optional/validated.
(b) `lib=` means *file stem* on `artifact` nodes but *artifact id* on `system`
nodes — rename the latter to `artifact=`. (c) Unknown top-level KDL nodes
silently ignored (`:704-756`) — a typo'd `telemetry` node vanishes; add a
final unknown-node check with a spanned error. (d) Builder-path `WireError`s
print indices and id hashes instead of names (`src/coordinator/mod.rs:185-192`)
— carry system/port names in all variants.

### E6 — [MEDIUM] Every `write` returns a `Result` all example code discards
`src/port.rs:105` — on the framework's `Overwrite` rings the only reachable
error is `InsufficientCapacity` (a sizing bug), so `let _ =` is trained into
users and nobody routes failures to health. Proposal: infallible `publish()`
for cyclic outputs (failure recorded on the system's health port); keep
`Result` for lossless/async paths.

### E7 — [MEDIUM] Sequences cannot timestamp the frames they emit
`examples/adcs-fsw2/contracts/src/lib.rs:397-402` — occupant-emitted frames
carry `Timestamp(0)` because sequences have no `now`. The runtime has it
(`step(now)`). Proposal: expose `now()` on the ambient `SeqClock` beside
`wait`/`progress`, or auto-stamp occupant writes.

### E8 — [LOW] Smaller ergonomics items
(a) `Out<NavOut<B>, B>` wrapper user-visible for framework-internal reasons
(`src/system/mod.rs:174-189`; also A9-adjacent — the health pair could be
runner-owned). (b) `READER_SLACK = 4` is hidden but load-bearing; exhaustion
panics (`src/coordinator/mod.rs:56`, `src/port.rs:199`) — surface on
`CoordinatorConfig`, make exhaustion a diagnosable error. (c) Derive
attribute namespace is `#[metor_fsw(...)]` under `metor_fsw_2::Frame` —
cosmetic but the first thing a new user copies. (d) No static-linking
example: adcs-fsw2 exercises only the dlopen/KDL path; cube-sat is
old-framework. (e) `add_slot` takes `Vec<(String, DlSystem, Vec<u8>)>`
instead of the public `AllowedOccupant` (`src/coordinator/mod.rs:678`).

---

## 5. Internal cleanliness

### C1 — [MEDIUM→HIGH leverage] Unused speculative machinery: lossless mode, mmap, rate_hint
- `Overrun::Lossless` + `WouldBlock` + async wait-for-space + `try_read`/
  `ReadGrant` + `slowest_active_cursor` + space-wake direction: zero
  references outside ring's own tests. **Deleting or feature-gating lossless
  removes R6 entirely and downgrades R7.**
- `mmap` feature: never enabled by any workspace consumer.
- `rate_hint`/`PortDesc::of_at`/half of `Hz`: carried, documented, serialized
  across the dl ABI (`src/abi/mod.rs:174`) — and read by nothing
  (`src/descriptor.rs:101,169-174`). Delete (bump `FSW_ABI_VERSION`) or
  actually wire into ring sizing.
- Matched `space`-notifier plumbing in the copy-in path is dead
  (`src/coordinator/mod.rs:1189-1200`, `src/binder.rs:51-57`) — the private
  ring is Overwrite/`try_write`; pass `NoWake`, carry one optional wake.

### C2 — [HIGH] `build()` is a ~630-line monolith doing eight jobs
`src/coordinator/mod.rs:801-1428` — edge validation, cycle detection, fan-out
counting, ring allocation, coordinator-own rings, slot-aux, command-producer
collection, copy-in setup, bind loop; a dozen intermediate maps. The section
comments already mark the seams — extract named pass functions over one
context struct. (Do this alongside A1-A3, which delete some passes.)

### C3 — [HIGH] Same validation implemented twice across wiring and coordinator
(a) Slot contract derivation + occupant-compat: `src/wiring/mod.rs:1015-1048`
and `src/coordinator/mod.rs:683-718` — wiring produces a clean `LoadError`,
then `add_slot` re-does it with a panic. Make `add_slot` return
`Result<_, WireError>` (also E8/style S4) and delete the wiring copy.
(b) `resolve_dl` vs `resolve_slot` duplicate the artifact-open pipeline
(`src/wiring/mod.rs:901-944` vs `:980-1013`) — extract one `open_occupant()`.

### C4 — [MEDIUM] Mechanical duplication to fold (mostly subsumed by A1 if done)
- `OutputRegistry`/`MessageRegistry` line-for-line duplicates
  (`src/registry.rs:56-100` vs `:141-185`) → generic `Registry<E>`.
- The `try_read_into`/`resync` drain loop hand-rolled at six sites
  (`src/port.rs:216-240`, `src/message.rs:260-280`,
  `src/coordinator/mod.rs:1801-1818`, `src/telemetry/mod.rs:827-873`) → one
  `drain_records` helper on `View`.
- `HandOff`/`MsgHandOff` identical wake scaffolding
  (`src/telemetry/mod.rs:319-416`).
- `run_seq_*` vs `run_*` ABI scaffolding (`src/abi/mod.rs:446-522,611-631,
  659-730,837-857`) → `params_from_raw`/`rings_from_raw`/`describe_common`,
  ~80 lines.
- Fixed-buffer name packing ×4 (`src/health.rs:57-69`,
  `src/sequence/mod.rs:242-253`, `src/coordinator/slot.rs:115-121`,
  `src/coordinator/mod.rs:1860-1869`) + two separate 48-caps → one
  `pack_str<const N>` + one constant.

### C5 — [MEDIUM] Dead/redundant state
- `CoordinatorBuilder.kinds` always equals `descs[i].kind`
  (`src/coordinator/mod.rs:514-525`) — delete; ideally collapse the four
  parallel vectors into one `Vec<Registration>`.
- `Name` newtype + `FrameMap`'s `K` parameter dead (`src/dynamic.rs:224-240`);
  `validate_key` (`src/writer.rs:217-225`) is the single real guard.
- `RegisteredSystem` marker trait unused (`src/wiring/mod.rs:532-543`).
- `Tap.slot` duplicates the tap index (`src/telemetry/mod.rs:657,740-747`).
- Stale `#[allow(dead_code)]` on `RingEntry`/`BufferRole` payloads
  (`src/coordinator/mod.rs:315-334`).
- `Edge` duplicates `EdgeSpec` field-for-field (`src/wiring/mod.rs:657-666`
  vs `src/wiring/model.rs:216-230`).
- `DlSystem::into_slot` is a verbatim forward to `make_slot`
  (`src/dl.rs:232-241`).

### C6 — [LOW] Structure
- `wiring/mod.rs` (1749 lines) is a grab-bag: errors (~310 lines), KDL scalar
  decoding, factory registry, resolver, node parsers, dl schema-encoder —
  split into `error.rs`/`kdl_params.rs`/`parse.rs`.
- `parse()` makes six passes over `doc.nodes()` (`src/wiring/mod.rs:693-768`)
  — one pass with a `match`.
- `TelemetrySystem` holds five parallel `Option`s all set together in `init`
  (`src/telemetry/mod.rs:674-691`) — bundle into one `Option<Started>`.

---

## 6. Rust style

### S1 — [MEDIUM] Public error types without `Display`/`Error` impls
`ring/src/lib.rs:137-166` (`WriteError`, `ReadError`, `FullReaderTable`,
`AttachError`), `src/telemetry/mod.rs:64-70` (`TransportError`),
`src/writer.rs:25-31` — can't `?` into anyhow/miette; the crate already works
around it (`format!("{e:?}")` at ring:615). Inconsistent with `DlError`
(thiserror) and `WireError` (manual impls). Fix: thiserror in metor-fsw-2;
manual impls in the dependency-free ring crate. Also `TransportError::Io(String)`
is stringly-typed with the conversion copy-pasted 8× — carry the source.

### S2 — [MEDIUM] Two public `WriteError`s; the crate root re-exports the wrong one
`src/lib.rs:48` re-exports `writer::WriteError` (frame-builder key errors),
but `Output::write`/`MsgOut::emit` return `metor_fsw_ring::WriteError`, which
is not at the root. Fix: rename the frame-builder error (`KeyError`) and/or
re-export ring's error distinctly.

### S3 — [MEDIUM] Flat crate-root re-export surface with collision-prone names
`src/lib.rs:45-128` — ~90 names including `parse`, `resolve`, `compatible`,
`Name`, `Level`, `Step`, `wait`, `progress`, while docs recommend glob
imports. Fix: keep free functions namespaced; re-export only types at root.

### S4 — [MEDIUM] `wait()` returns an un-annotated future; missing `.await` compiles silently
`src/sequence/mod.rs:144-170,204` — `wait(Duration::from_secs(5));` without
`.await` is a no-op sequence bug that compiles clean. Fix:
`#[must_use = "futures do nothing unless .awaited"]` on `Wait` and both fns.
Related (A-cluster): the free-fn + `Seq`-method dual API and its TLS
(`src/sequence/mod.rs:167-216`) — keep only the `Seq` handle.

### S5 — [LOW] Consolidated polish
- 4 outstanding clippy warnings (derivable `Default` for `ClockMode`,
  `needless_range_loop` at coordinator:1149, `collapsible_if` at :1472,
  `is_multiple_of` at writer.rs:145); no `[workspace.lints]` — pin clippy in
  workspace lints.
- Panicking accessors on public `PortId::frame_id`/`PortDesc::vtable`/
  `announce` (`src/descriptor.rs:46-52,215-234`) — `Option` forms with
  `expect` at frame-only call sites (ties into A1's PortKind unification).
- Per-cycle allocations in the hot loop against the crate's own scratch-reuse
  convention (`src/coordinator/slot.rs:411-429,483`,
  `src/coordinator/mod.rs:1823,1846`).
- `add_slot` `Box::leak`s each slot name per build (`:707`) — leaks in a
  rebuild loop.
- `Input::resync(&self)` vs `latest/drain(&mut self)` mutability asymmetry
  (`src/port.rs:182`, ring:983).
- Small idioms: `is_some_and` (descriptor.rs:287), `split_first_chunk`
  (message.rs:51), `let-else` (telemetry:820), `const fn arch_tag` (ring:87).

---

## Suggested sequencing

1. **Bugs + ring hardening** (§1, R6-R7 or C1's lossless deletion): R1, B1,
   E1, B2-B5, R2-R4 are user-visible correctness; most are small, isolated
   fixes. Decide C1 (delete lossless?) first since it moots R6 and shrinks R7.
2. **Architecture** (§3): A1 (unify frame/message via orthogonal axes) is the
   big rock — it subsumes most of C4, half of S5's panicking accessors, and
   A7. A2/A3 (explicit command edges, slots as ordinary descriptors) are
   independent and can go next. C2/C3 fall out naturally while touching
   `build()`.
3. **Ergonomics** (§4): E2 (`#[system]` macro) is the highest-leverage
   user-facing change; E3-E7 are incremental API fixes.
4. **Style** (§6): mechanical; batch S1-S4 in one pass.
