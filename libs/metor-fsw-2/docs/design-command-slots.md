# Design — explicit command plane + slots as ordinary systems (A2 + A3)

> **Status: DESIGN (D4 of `review-fixes-plan.md`, wave 0).** Covers findings **A2**
> (command edges become explicit dataflow), **A3** (slots stop being a third system
> kind; name-based addressing), and the attached items **A8** (uplink dispatch), **A9**
> (coordinator as system #0), **B5-runtime** (unknown-occupant Load surfaces), and
> **A11(b)** (telemetry-last enforcement). Written against the code as of the wave-1
> working tree (2026-07-02).
>
> **Dependency:** `docs/design-port-unification.md` (D3, in flight) unifies frame/message
> ports into one core with schema × delivery × cardinality axes. §6 states which parts of
> this design are independent of D3 (can land first) and which assume it. Every shape here
> is chosen to work in both worlds.

---

## 1. Problem restatement

Three places in `build()` special-case `PortId::Msg(SequenceCommand::ID)`
(`src/coordinator/mod.rs:961-965`, `:1144-1155`, `:1304-1315`): any system that declares a
`MsgOut<SequenceCommand>` silently becomes a command source for **every** slot, with no
edge and no opt-out — the one place dataflow is invisible in the wiring. The
coordinator-owned `command_ring` behind `control_handle()` (`:1666-1671`) lives outside
every model surface and mints a fresh writer per call, abandoning the single-writer
discipline.

Slots are a third system kind held together by side channels: a `SlotAux` pass
(`:425-435`, `:1020-1109`) allocates five undeclared resources (control ring, `SlotStatus`
ring, events `MsgOut`, a self-view on `SequenceStatus`, a `ChannelId`); the registered
descriptor is produced by **popping** the occupant's trailing `SlotControlIn` input
(`:709-712`) and the bind arm **re-appends** its region positionally (`:1284-1291`); the
lifecycle is tracked twice (`SlotPhase` + a shadow `SlotState` via `sync_slot_state`,
`src/coordinator/slot.rs:59-88`, `:274-279`). Worst, `ChannelId` — the slot's build-order
index — leaks into the wire protocol: reordering `mission.kdl` silently re-addresses
commands from the ground.

---

## 2. Target model

### 2.1 One port-connection axis: `PortConn`

The single new descriptor concept, from which everything else falls out. `PortDesc` gains
one field:

```rust
/// Who provides the other end of this port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortConn {
    /// Edge-connected (the default): wired by `connect` / KDL edges. Frame inputs
    /// require exactly one producer; message inputs accept 0..N.
    Edge,
    /// Host-connected: the system's *runner* (SlotRunner, or the coordinator itself)
    /// holds the port's counterpart — the writer of an input ring, or the writer of an
    /// output ring the bind loop would otherwise hand to the system. Outputs are still
    /// ring-allocated and registry-tapped like any output; inputs get a dedicated ring.
    /// An edge targeting a Host port is a `WireError::HostPort`.
    Host,
    /// A declared reader over one of this system's own outputs, named by `PortId`.
    /// Allocates no ring; adds +1 to that output's fan-out; the view goes to the runner.
    SelfTap(PortId),
}

pub struct PortDesc {
    pub id: PortId,
    pub name: &'static str,
    pub max_size: usize,
    pub kind: PortKind,
    pub conn: PortConn,        // NEW — default Edge everywhere today
}
```

`build()` rules per `conn`:

| conn | as input | as output |
|---|---|---|
| `Edge` | unchanged (exactly-one frame / 0..N msg producers) | unchanged (ring + registry entry) |
| `Host` | dedicated ring; **exempt** from `UnconnectedInput`; writer handed to the runner | ring + registry entry as normal; **writer handed to the runner**, not the bind walk |
| `SelfTap(p)` | no ring; +1 fan-out on own output `p`; view handed to the runner | (not allowed) |

This is deliberately **not** slot-specific: the coordinator's own bundle (§2.6) uses the
same axis, and in the D3 world `conn` is simply a fourth orthogonal axis beside
schema/delivery/cardinality (§7).

One supporting addition: `PortDesc::msg_named::<M>(name: &'static str)` — an explicit
channel-name override for message ports, so a registry key like `"<instance>.sequences"`
survives the move from hand-built `MessageEntry`s to descriptor-driven allocation (and
chips at A10's `type_name` identity for these ports).

### 2.2 The slot's registered descriptor (A3)

`add_slot` derives the registered descriptor **by extension, not surgery** — no pop, no
positional re-append. For a v1 sequence occupant whose own (`fsw_describe`d, unchanged)
descriptor is `inputs = [user…, SlotControlIn]`, `outputs = [user…, SequenceStatus,
health, log]`:

```text
registered.inputs:
  0..n   user frame inputs           Edge          (occupant-bound)
  n      slot_control  SlotControlIn Host          (occupant reads; RUNNER holds the writer)
  n+1    commands      MsgIn<SequenceCommand>  Edge, fan-in   (RUNNER drains)
  n+2    seq_status    SelfTap(Frame(SequenceStatus))          (RUNNER's own-output view)

registered.outputs:
  0..m   user outputs                Edge/tapped   (occupant-bound)
  m      sequence      SequenceStatus              (occupant-bound, tapped)
  m+1,2  health, log                                (occupant-bound, tapped)
  m+3    slot_status   SlotStatus    Host          (RUNNER writes; tapped: "<slot>.slot_status")
  m+4    sequences     MsgOut<SequenceChannelEvent>, msg_named "sequences", Host, telemetered
                                                    (RUNNER writes: "<slot>.sequences")
```

**Invariant replacing the pop/re-append:** the occupant's ports form the *prefix* of each
list, in the occupant descriptor's own order; runner ports are the *tail*. `SlotReg`
records the split indices (`n_occ_inputs`, `n_occ_outputs`), and the bind arm builds the
occupant `FswRing` arrays as a straight prefix map — the occupant-side positional bind
contract is untouched, so **the dl ABI does not change**.

Whether a port is occupant- or runner-held is slot-internal metadata (the split index),
**not** a `PortDesc` field — the descriptor only carries `conn`, which is all `build()`
needs.

What this kills:

- `SlotAux` (struct + the whole `:1020-1109` allocation pass). Control/status/events rings
  come out of the one uniform per-output/per-Host-input allocation loop; the events and
  status registry entries are produced by the same `registry_entry`/`message_entry` calls
  every output gets.
- The descriptor pop at `:709-712` and the input re-append at `:1284-1291`.
- The `SlotStatus` ring's fake `BufferRole::Output { port: outputs.len() }` off-by-the-end
  index (it is now a real output index).
- The `expect("a v1 slot occupant publishes a SequenceStatus output")` positional hunt at
  `:1089-1093` (the SelfTap names it by `PortId`).

### 2.3 Name-based slot addressing — the wire-format change (A3)

`ChannelId` (= the slot's build-order index among slots) leaves the protocol. Slots are
addressed by **instance name** — the same unique, human-legible key the wiring, the
telemetry prefix, and the registry already use.

**wkt changes** (`libs/metor-proto/wkt/src/msgs.rs`):

```rust
/// Human-readable, unique channel (slot instance) name, e.g. "adcs". ≤ 48 bytes (§ cap).
pub type SequenceChannelName = String;

pub struct SequenceChannelSpec {
    pub name: String,                  // `id: ChannelId` REMOVED
    pub available: Vec<SequenceName>,
}
pub struct SequenceChannelEvent {
    pub channel: SequenceChannelName,  // was `channel_id: ChannelId`
    pub kind: SequenceEventKind,
}
pub struct SequenceCommand {
    pub channel: SequenceChannelName,  // was `channel_id: ChannelId`
    pub command: SequenceCommandKind,
}
// `pub type ChannelId = u64;` — deprecated, deleted once consumers migrate.
```

**Encoding & PacketIds.** The payloads stay `serde`/postcard; a name is an ordinary
postcard `String` (varint length + bytes). Because the postcard layout of all three types
changes (u64 → String), the **`Msg::ID`s are re-assigned** — `SequenceRegistry [224,45]`,
`SequenceChannelEvent [224,46]`, `SequenceCommand [224,47]` — retiring `[224,41..43]`.
Rationale: metor-db persists raw `Msg` packets keyed by `PacketId` and the panel's
catch-all pub/sub decodes by id; changing a struct **under** its old id would make every
historical recording mis-decode (or decode to garbage) silently. New ids make old
recordings cleanly *unmatched* instead (a panel legacy decoder for the old ids is optional
future work). `ReloadSequences [224,44]` and `SequenceRunState` are unchanged.

**The 48-byte cap.** The name field is capped at **48 bytes**, unified with the existing
`SLOT_NAME_CAP = 48` / `STATUS_NAME_CAP = 48` (one shared `NAME_CAP` constant + one
`pack_str` helper, per C4). Justification: instance names must round-trip losslessly into
the fixed zerocopy frames that carry them (`SlotStatus.occupant`,
`CoordinatorStatus.stopped[].name`) — a name longer than the pack cap would telemeter
truncated while addressing untruncated, a correctness split; 48 bytes is generous for a
dotted instance identifier and keeps those frames compact. Enforcement is a **validation
invariant, not a wire encoding**: `add_slot`/`resolve` reject instance names > 48 bytes
(`WireError`/`LoadError` — replacing today's silent truncation), and an inbound command
whose `channel` exceeds the cap simply matches no slot and is dropped by the fan-in filter
(bounded work: decode allocation is already bounded by the ring record ≤ `MAX_MSG_BYTES`).
A fixed `[u8; 48]` on the wire was considered and rejected: postcard's varint `String` is
smaller in the common case, serde-natural for every consumer, and the cap invariant
delivers the same boundedness.

**Runtime:** `SlotRunner` filters by `cmd.channel == self.name` (it already carries its
instance name); `channel_id`, `channel_map`, `Coordinator::channel_map()`, and
`Coordinator::channel_id(name)` are all deleted. The boot `SequenceRegistry` payload is
built from slot names + allowed sets only. **Regression property:** reordering slot nodes
in `mission.kdl` no longer re-addresses ground commands.

### 2.4 One lifecycle enum (A3)

`SlotPhase` and the 2-variant `SlotState` merge into the coordinator-level enum (the name
`SlotState` stays; `SlotPhase` dies):

```rust
pub enum SlotState {
    Empty,
    Loaded,
    Running,
    Done { outcome: u8 },
    Stopped { reason: StopReason },
}

impl SlotState {
    /// The projection the coordinator's stopped-systems status uses: only a
    /// lapped/panicked stop is an error-stop (Done/Empty/Loaded are not).
    pub fn stop_reason(&self) -> Option<StopReason>;
    /// The wire phase code published in `SlotStatus::phase` (Empty=0 … Stopped=4).
    pub fn code(&self) -> u8;
}
```

Static `CyclicRunner` and build-time `DlSlot` only ever inhabit `Running`/`Stopped`;
`SlotRunner` uses all five. `update_status` switches from `matches!(…, Stopped)` to
`state.stop_reason()`. The shadow field `SlotRunner::slot_state` and `sync_slot_state()`
are deleted — one enum, one source of truth, a projection instead of a mirror. (This
composes with wave-1 B2, which fixes `update_status`'s change detection to membership.)

### 2.5 Explicit command edges (A2)

The slot's `commands` input (§2.2) is an ordinary fan-in message input, so command wiring
is ordinary message wiring — **per-slot explicit edges, no broadcast sugar** (decision):

```kdl
connect "uplink"      -> "adcs" msg="SequenceCommand"
connect "coordinator" -> "adcs" msg="SequenceCommand"
```

```rust
b.connect(PortRef::msg::<SequenceCommand>(uplink),
          PortRef::msg::<SequenceCommand>(adcs))?;
```

Why explicit-only: (a) explicitness is the entire point of A2 — an autonomy emitter that
should command only the recovery slot now *cannot* reach the ADCS slot, an opt-in that the
type-keyed collection made impossible; (b) real missions have O(1–3) slots and O(1–2)
producers, so the line count is trivial; (c) a `connect "uplink" -> * msg=…` fan-out sugar
is a pure front-end addition later if it proves a papercut — adding sugar is cheap,
removing magic is not. Fan-in of several producers into one slot and fan-out of one
producer to several slots both come free from the existing message-edge rules
(`docs/message-wiring.md` §3.2); name-addressed commands make over-broad fan-out harmless
(only the addressed slot acts). Wave-1 B7 (duplicate-edge rejection) covers the
copy-paste double-delivery hazard.

**All three type-keyed `build()` checks are deleted** (§3). A slot with zero command edges
is legal (an autonomy-free, wiring-frozen slot); a mission that forgets the coordinator
edge simply has an inert `control_handle` — visible in the wiring, diagnosable from the
graph, exactly the property A2 wants. The e2e test in §8 pins the opt-in behavior
(a `MsgOut<SequenceCommand>` system with **no** edge commands nothing).

**Same-cycle dispatch** is preserved where it exists today: the uplink is async (fills its
ring out of band; the slot's head-of-`step` drain sees it the same cycle). A *cyclic*
producer registered after the slot delivers next cycle — ordinary registration-order
dataflow, now visible as an edge and subject to wave-1 E1's backward-edge diagnostics.

### 2.6 `control_handle` + the coordinator as system #0 (A2 + A9)

The coordinator registers itself as **system #0** at `CoordinatorBuilder::new`, under the
reserved instance name `"coordinator"`, with an ordinary declared bundle:

```text
inputs:
  status_view   SelfTap(Frame(CoordinatorStatus))      (read_status)
outputs:
  health        SystemHealth        Host, tapped        → "coordinator.health"
  log           SystemLog           Host, tapped        → "coordinator.log"
  status        CoordinatorStatus   Host, tapped        → "coordinator.coordinator_status"
  sequences     MsgOut<SequenceRegistry>, msg_named "sequences", Host, tapped
                                                        → "coordinator.sequences"
  commands      CommandOut<SequenceCommand> Host, untelemetered   (the operator channel)
```

All registry keys are byte-identical to today's hand-rolled ones, so nothing downstream
(Subset filters, db paths) moves. `Reg::Coordinator` is a marker registration: it is
**not** pushed into `cyclic` (the coordinator *is* the loop); its bind arm wraps the
allocated rings into the existing `Coordinator` fields (`coord_health`, `status_out`,
`status_view`, `seq_registry_out`) — deleting the `coord_ring` block (`:998-1018`), the
boot-registry block (`:1111-1126`), and the `command_ring` block (`:1131-1142`).

`"coordinator"` (and the uplink's instance name) join the KDL instance namespace; a user
`system "coordinator"` is now a `DuplicateInstance` error instead of a silent registry-key
collision.

**Single-writer restored:** the `commands` output's writer is bound once at `build()` and
held as `operator_commands: Option<MsgOut<SequenceCommand>>`:

```rust
/// The pre-bound writer over the coordinator's declared operator-command output.
/// TAKE-ONCE: the first call returns it, subsequent calls return None — the ring's
/// single-writer discipline is enforced by ownership, not by a doc comment.
pub fn control_handle(&mut self) -> Option<MsgOut<SequenceCommand>>;
```

The host / CLI / a test takes it once and owns it (mirroring the telemetry transport's
take-once pattern). Commands flow to slots only over the explicit
`"coordinator" -> <slot>` edges, so the in-proc path and the uplink path are the same
mechanism *and* the same wiring surface. The CLI runner takes the handle at startup; tests
that minted two writers are misuses and get fixed.

**A9 is separable:** the full #0 bundle (health/log/status/sequences) can land as its own
commit; A2 minimally needs only the `commands` port + take-once handle. §7 phases it.

### 2.7 Uplink: real multi-output dispatch (A8 — decision)

**Decision: implement dispatch-by-declared-output, not document-as-`SequenceCommand`-only.**
The subscription side (`uplink_subscribe_ids`, `src/telemetry/mod.rs:494-502`) already
derives from the declared outputs; the delivery side hard-filters `SequenceCommand::ID`
and emits on the one known port (`:530-533`). Completing the promise is small and makes
the next command type (`ReloadSequences`, `AlarmAck`) a one-line bundle change.

Mechanism: a routing seam on the output bundle, decode-validated per port (ground bytes
are never forwarded unparsed):

```rust
/// Route one received wire Msg to the declared output whose id matches.
/// Returns false (and the uplink bumps `uplink.unroutable` health) when no port matches.
pub trait RouteMsg {
    fn route(&mut self, id: PacketId, bytes: &[u8]) -> bool;
}

impl RouteMsg for UplinkPorts {
    fn route(&mut self, id: PacketId, bytes: &[u8]) -> bool {
        match id {
            SequenceCommand::ID => decode_emit::<SequenceCommand>(&mut self.commands, bytes),
            _ => false,
        }
    }
}
```

`UplinkSystem::run` becomes: `recv → route(m.id, m.bytes)`. Hand-written per bundle today;
the E2 `#[system]` derive (D5) can generate it from the declared message outputs later.
The subscribe-ids fn moves onto the same trait's bundle so subscription and routing derive
from one list and cannot diverge again.

Also in scope: a KDL surface for the uplink (closing A11(c)'s gap *and* required so command
edges can name it):

```kdl
uplink { transport "tcp" addr="127.0.0.1:2241" }   // instance name "uplink"
```

`Wiring.uplink: Option<UplinkSpec { addr }>` gains the KDL node; `resolve` registers it
via the existing `add_uplink` before the edges pass so `connect "uplink" -> …` resolves.

### 2.8 Unknown-occupant `Load` surfaces (B5 runtime half)

Build-time validation of `initial.occupant` and occupant compatibility lands in parallel
(wave 1, W1b). The runtime half — a ground `Load` naming an occupant outside the allowed
set is currently swallowed (`slot.rs:338-340`) — becomes observable:

```rust
let Some(idx) = self.allowed.iter().position(|a| a.name == occupant) else {
    self.emit_event(SequenceEventKind::Failed {
        reason: format!("unknown occupant '{occupant}'"),
    });
    return;
};
```

The slot's phase and `SlotStatus` are unchanged (nothing was loaded); the panel sees the
failure on the channel it commanded. A generalized "command rejected in phase X" event
would need a new `SequenceEventKind` (wire change) and is noted as future work, not done
here.

### 2.9 Telemetry "registered last" (A11(b) — decision)

**Decision: enforce, don't reorder.** `build()` returns
`WireError::ReceiveAllNotLast { system }` if any cyclic system *without* a `ReceiveAll`
port is registered after one *with* it (async systems are exempt — they are not in the
step order). Rationale: silently reordering registrations would change step order — the
exact thing wave-1 E1 is making *diagnosable* (backward non-delayed edges become errors);
one part of the system must not shuffle what another part is validating. The error message
names the fix ("register '<system>' before the telemetry downlink").

---

## 3. Deleted surface (the A2/A3 scorecard)

`build()` passes / special cases:

| Deleted | Where | Replaced by |
|---|---|---|
| `n_slots` count + `cmd_readers` type-keyed reader bump | `mod.rs:804-811`, `:961-965` | ordinary fan-out counting from explicit msg edges |
| `command_producers` collection-by-type | `:1144-1155` | `msg_cons_edges` (ordinary fan-in) |
| Slot command fan-in hand-construction | `:1304-1315` | the declared `commands` `MsgIn` port's normal bind |
| `SlotAux` struct + allocation pass | `:425-435`, `:1020-1109` | uniform allocation over the extended descriptor (§2.1/§2.2) |
| Descriptor pop / FswRing re-append | `:709-712`, `:1284-1291` | prefix/tail invariant + split indices (§2.2) |
| `command_ring` field + per-call writer mint | `:1131-1142`, `:1666-1671`, `Coordinator.command_ring` | coordinator #0 `commands` port + take-once `control_handle` (§2.6) |
| Coordinator hand-rolled health/log/status/seq-registry rings | `:998-1018`, `:1111-1126` | coordinator #0 declared bundle (§2.6) |
| `channel_map`, `channel_map()`, `channel_id()` | `:1030-1043`, `:1581-1584`, `:1637-1651` | name addressing (§2.3) |

Types / fields: `SlotPhase` (merged, §2.4), `SlotRunner::{channel_id, slot_state}` +
`sync_slot_state`, `ChannelId` in wkt (deprecated → deleted), `SlotAux`.

Kept (out of scope here): `CommandOut` as the untelemetered spelling (D3's `telemetered`
field on every port may fold it later, per A6); the `Box::leak`ed slot name (tied to
`SystemDescriptor.name: &'static str` crate-wide — a D3-adjacent cleanup, noted not fixed).

---

## 4. KDL surface + example

New/changed nodes: `uplink { … }` (§2.7); command edges are ordinary `msg=` edges (no new
edge syntax); `"coordinator"` becomes a connectable, reserved instance name.

```kdl
coordinator cycle_rate=100.0

artifact "commissioning" crate="adcs-seqs" lib="adcs_commissioning" type="commissioning"
artifact "safe_mode"     crate="adcs-seqs" lib="adcs_safe_mode"     type="safe_mode"

system "imu"   type="ImuDriver" i2c_bus=1
system "plant" type="AdcsPlant"

slot "adcs" {
    input  frame="sensors"
    output frame="mode"
    allow occupant="commissioning"
    allow occupant="safe_mode"
    initial occupant="commissioning"
}

// Data edges (unchanged).
connect "plant" -> "adcs"  frame="sensors"
connect "adcs"  -> "plant" frame="mode" delayed=#true

// Command edges — the previously-invisible dataflow, now explicit.
uplink { transport "tcp" addr="127.0.0.1:2241" }
connect "uplink"      -> "adcs" msg="SequenceCommand"   // ground commands
connect "coordinator" -> "adcs" msg="SequenceCommand"   // in-proc control_handle

telemetry {
    transport "tcp" addr="127.0.0.1:2240"
    mode "all"
}
```

Builder parity: `b.coordinator_handle() -> SystemHandle` (system #0) so the Rust front-end
can wire the operator edge; `add_uplink` unchanged.

---

## 5. Ground-side migration inventory

The wire change (§2.3) is: `SequenceCommand`/`SequenceChannelEvent` gain `channel: String`
(name) replacing `channel_id: u64`; `SequenceChannelSpec` drops `id`; all three get fresh
`PacketId`s. Every consumer below compiles against the same workspace wkt crate, so the
breakage is compile-time-visible and lands in one coordinated commit.

Architectural facts (verified by a full workspace sweep): the wire types live **only** in
`libs/metor-proto/wkt/src/msgs.rs` — no crate mirrors or re-exports them under an alias;
fsw-2's `SlotStatus`/`SlotControlIn`/`SequenceStatus` are internal frames with **zero
consumers outside `libs/metor-fsw-2`** (plus the `#[sequence]` macro, which touches the
internal port types only — immune to the wire change); the old framework
(`libs/metor-fsw`) defines **no** parallel sequence wire protocol; `apps/metor` and
`apps/inscriber` have no touchpoints.

**Encoders of `SequenceCommand` (ground → FSW) — every construction site changes
`channel_id: <u64>` → `channel: <name>`:**

| File | Lines | What |
|---|---|---|
| `libs/metor-panel/src/sequences/mod.rs` | 292-300 | `publish()`: builds `SequenceCommand`, postcards it, `db.push_msg(.., SequenceCommand::ID, ..)` — the core encoder (new `ID` picked up on recompile) |
| same | 303-326 | `load()`/`start()`/`abort()`/`stop()`/`reset()` — per-kind constructors, all take a channel id today |
| `examples/adcs-fsw2/tests/sequences.rs` | 160, 170-193 | `control_handle()` + three `SequenceCommand { channel_id, .. }` emits (also hits the take-once `control_handle` change, §2.6) |
| `examples/cube-sat/src/sequencer.rs` | 260-311 | `handle_command(SequenceCommand)` — the hand-rolled control-side *decoder*; its slot lookup switches from id to name |

**Decoders of registry/event telemetry (FSW → ground):**

| File | Lines | What |
|---|---|---|
| `libs/metor-panel/src/sequences/mod.rs` | 264-275 | `ingest_loop`s on `SequenceRegistry::ID` / `SequenceChannelEvent::ID` (new ids on recompile) |
| same | 75-158 | `apply_registry` / `apply_event` — fold state **keyed on `channel_id`**; re-key on `channel` name |
| `libs/metor-panel/src/views/sequence_panel.rs` | 14, 45, 64-118, 398 | `ChannelId`-typed UI fields (`arming_stop`, per-channel rows) → `SequenceChannelName` |
| `libs/metor-panel/src/views/sequence_grid.rs` | 10, 32, 35 | `id: ChannelId` grid field → name |
| `examples/adcs-fsw2/tests/sequences.rs` | 276-320 | drains + asserts `SequenceRegistry` / `SequenceChannelEvent` streams |

**Encoders of registry/event telemetry outside fsw-2** (the cube-sat example is a
standalone, non-fsw-2 control system speaking the same protocol):

| File | Lines | What |
|---|---|---|
| `examples/cube-sat/src/sequencer.rs` | 246-256 | `registry()` builds `SequenceRegistry`/`SequenceChannelSpec` (drops `id`) |
| same | 339-387, state fields at 191-370 | `emit()`/`drain_events()` build `SequenceChannelEvent { channel_id, .. }`; `ChannelId` state fields → names |
| `examples/cube-sat/src/main.rs` | 555-564, 690-701 | `MsgStream { msg_id: SequenceCommand::ID / ReloadSequences::ID }` subscriptions + command routing (ids on recompile) |

**Persisted / serialized form:**

| File | Lines | What |
|---|---|---|
| `libs/db/tests/src/lib.rs` | 47-68 | constructs `SequenceChannelEvent { channel_id: 3, .. }`, asserts it lands in the DB **msg log keyed by `PacketId`** — confirms recordings persist under the id (the basis for the fresh-`PacketId` decision, §2.3) and must be updated to the new shape/ids |
| `libs/metor-proto/wkt/src/msgs.rs` | 660-769, 1086-1152 | the canonical definitions + postcard round-trip / `packet_ids` pin tests — fail loudly on the change; updated with it |

Unaffected: `ReloadSequences` (`[224,44]`, no channel field), `SequenceRunState`,
`AlarmAck` and every other wkt Msg; all `.kdl` files (none encode `SequenceCommand`); the
old-framework `#[sequence]` macro.

**Migration shape:** one coordinated workspace commit (wkt + panel + cube-sat + db test +
adcs-fsw2 test) — every touchpoint is compile-time-visible since all consumers import
`metor_proto_wkt` directly. Panel state is per-session in-memory projections
(`SequenceStore`/`SequenceState`), so no stored ground-side state migrates; the only data
casualty is historical DB msg-log recordings under the retired ids (§9 Q1).

---

## 6. Both worlds: interaction with port unification (D3)

**Independent of D3 — can land before it (shapes identical either way):**

- The wkt wire-format change + panel migration (§2.3, §5): pure protocol, no port model.
- Name-based `SlotRunner` filtering, `channel_map` deletion (§2.3).
- The `SlotState` merge (§2.4).
- B5-runtime event (§2.8), A11(b) enforcement (§2.9), the 48-byte name-cap validation.
- The uplink `RouteMsg` seam + KDL `uplink` node (§2.7) — routes over `PacketId`s, which
  survive unification (they become the `Postcard` schema axis's key).
- Explicit command edges themselves: they use the *existing* `PortId::Msg` edge machinery,
  which D3 subsumes but does not remove — an edge declared now stays declared after.

**Assumes / lands with D3 (or needs a shim if landed first):**

- `PortConn` on `PortDesc` (§2.1). In the unified world it is a fourth orthogonal axis on
  the one `PortDesc` (schema × delivery × cardinality × **conn**) and the uniform
  allocation loop is D3's single ring-allocation pass — the natural landing spot. If A2/A3
  must land first, `conn` is added to today's `PortDesc` and D3 carries it across (the
  field is identical in both; only the struct it sits on changes).
- The slot descriptor extension (§2.2) and coordinator #0 bundle (§2.6) *mechanically*
  depend on wherever the allocation loop lives; both are specified against the axes-model
  loop per the fixes-plan ordering (wave 4: **A1 first, then A2+A3**).
- `MsgIn` fan-in bind for the slot's `commands` port: D3's cardinality axis (`Many`)
  formalizes what `BoundInput::Many` does today; the slot bind arm uses whichever exists.

**Recommended order stays as planned:** D3/A1 lands first; this design's phases 2–5 (§7)
build on the unified model; phase 1 and the small items land before/parallel to A1.

---

## 7. Phased landing sequence

Each phase is independently shippable and committed at its boundary.

1. **P1 — name addressing end-to-end** (pre-A1 safe). wkt type + id changes; panel +
   cube-sat + any CLI consumers migrate in the same commit; `SlotRunner` filters by name;
   `channel_map`/`channel_id()` deleted; name-cap validation; `SlotState` merge; B5-runtime
   event; A11(b) enforcement. *No descriptor/build changes yet* — the type-keyed command
   collection still runs, now carrying name-addressed commands.
2. **P2 — explicit command edges (A2 core)** (post-A1). Slot descriptor gains the
   `commands` `MsgIn` (Edge); the three type-keyed checks die; KDL `uplink` node +
   `"coordinator"` reserved name; minimal coordinator #0 (the `commands` port only) +
   take-once `control_handle`; examples/tests re-wired with explicit edges.
3. **P3 — slot descriptor completion (A3 core)** (post-A1). `PortConn::Host`/`SelfTap`;
   control/status/events/self-view move into the registered descriptor; `SlotAux` +
   pop/re-append die; prefix/tail split indices in `SlotReg`.
4. **P4 — coordinator #0 full bundle (A9, separable).** health/log/status/sequences move
   onto the declared bundle; hand-rolled ring blocks die; registry-key golden test guards
   the byte-identical keys.
5. **P5 — uplink dispatch (A8, separable).** `RouteMsg` + unroutable health counter;
   subscription derived from the same table.

---

## 8. Test plan

**Wire / addressing (P1):**
- wkt round-trip tests for the three new-format Msgs; a pinned-bytes vector per type
  (postcard golden) so the encoding can't drift silently.
- New `PacketId`s are distinct from every existing wkt id (extend the wkt id-collision test).
- `SlotRunner` name filter: command for `"other"` ignored; for `self.name` applied.
- **Reorder regression:** build the same mission with slot declaration order swapped;
  assert a `SequenceCommand { channel: "adcs" }` still drives the `adcs` slot.
- Name-cap: 49-byte slot instance name → build error; 49-byte inbound `channel` → dropped,
  no panic, no slot acts.
- Unknown-occupant `Load` → `Failed { reason: "unknown occupant …" }` event on the slot's
  channel; phase unchanged.
- `SlotState` merge: a `Done` slot absent from stopped-systems; `Stopped{Lapped}` present
  (with wave-1 B2's membership compare).
- A11(b): registering a cyclic system after `add_telemetry` → `ReceiveAllNotLast`.

**Command edges (P2):**
- **Opt-in regression (the A2 headline):** a system with `MsgOut<SequenceCommand>` and *no*
  edge to a slot commands nothing; adding the edge makes the same emit drive the slot.
- Fan-in: uplink + autonomy + coordinator edges into one slot; all three producers'
  commands apply; per-producer order preserved.
- Fan-out: one producer edged to two slots; only the addressed slot acts.
- Duplicate edge rejected (wave-1 B7); edge to a frame port with `msg=` → `WireError`.
- `control_handle`: first call `Some`, second `None`; commands flow only when the
  coordinator edge exists; e2e KDL mission drives Load/Start/Stop/Reset through it.
- Same-cycle: mock-uplink `SequenceCommand` visible to the slot within the cycle after the
  async emit (matches today's latency test).

**Slot descriptor (P3):**
- Registered descriptor golden: port list/order/conn exactly as §2.2 for a two-occupant slot.
- Edge targeting a `Host` port (`slot_control`, `slot_status`) → `WireError::HostPort`.
- Occupant prefix bind: Load→Stop→Load swap cycle still re-acquires rings (existing swap
  tests re-run over the new arrays); `Abort` cancel frame still reaches the occupant.
- Registry keys `<slot>.slot_status` / `<slot>.sequences` unchanged (golden).
- Reader budgets: fan-out-derived `max_readers` suffices with `READER_SLACK` removed from
  the equation for command rings (explicit edges count exactly).

**Coordinator #0 (P4):** registry-key golden (`coordinator.health/log/coordinator_status/
sequences`); boot `SequenceRegistry` still observed by a post-build tap; downlink `All`
e2e byte-compare against pre-change capture.

**Uplink (P5):** routed second output (add `ReloadSequences` to a test bundle — subscribe
ids include it, records route to it); unknown id → dropped + `uplink.unroutable` health;
malformed payload for a known id → dropped, link stays up.

**Ground e2e (workspace):** panel↔FSW loop against a live coordinator — registry renders
channels by name, Load/Start round-trip, events update the named channel; cube-sat example
compiles and its sequencer drives with name-addressed types.

---

## 9. Open questions (need a human decision)

1. **Old-recording decode:** with fresh `PacketId`s, historical db recordings of the old
   sequence Msgs become unmatched. Accept the loss, or add a legacy decoder in the panel
   for `[224,41..43]`?
2. **Take-once `control_handle` signature** (`&mut self -> Option<MsgOut<…>>`): OK to
   break the current `&self` callers (CLI runner, tests), or should it panic-on-second-call
   instead of returning `Option`?
3. **Fan-out sugar:** confirmed out of scope for v1 (explicit per-slot edges only)?
   (Inventory note: panel per-channel state is confirmed in-memory/per-session, so the
   `SequenceChannelSpec.id` removal has no ground-side persistence impact.)
