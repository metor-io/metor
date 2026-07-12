# Packs — many systems per crate, one authoring seam

A **pack** is a crate's set of system entries, built by one `fn pack() -> Pack` and served
to every loading mode from that single value: the static registry
(`Registry::register_pack`), the dlopen path (the pack ABI, v5), and the process-worker
path all consume the same entries. A pack is also where the authoring surfaces converge —
a system written as free functions over a state, as an `async fn` owning its ports, or as
a `#[system]`-annotated struct all lower to the same erased `PackEntry`, and a runtime
slot can host any of them.

```rust
// examples/adcs-fsw2/systems/adcs-systems/src/lib.rs, as shipped:
pub fn pack() -> Pack {
    Pack::new()
        .system("Plant", system(plant::plant_execute).init(plant::plant_init))
        .system("Nav", system(nav::nav_execute).init(nav::nav_init))
        // Deliberately struct-authored (`#[system]`), so the pack exercises both styles.
        .system_type::<CtrlSystem, _>("Ctrl")
}
metor_fsw_2::export_pack!(pack, feature = "export");
```

This is the final-state companion to `docs/design-packs-authoring.md` (the design record;
its plan is `docs/design-packs-authoring-plan.md`). The code lives in:

- `src/pack.rs` — `Pack`, `PackEntry`, the `Driver` trait, `Mount`, `EntryParams`, and the
  two-phase create/bind seam (`Pending`).
- `src/handler/` — the functional author surface: `ExecParam`/`ExecuteFn`/`InitFn`
  (`param.rs`, `tuples.rs`, `mod.rs`), the async `TaskParam`/`AsyncSystemFn` (`task.rs`),
  and the drivers (`driver.rs`).
- `src/abi/mod.rs` — the pack C ABI: the nine `fsw_pack_*` symbols, the `run_pack_*`
  helpers, `PackManifestMsg`, and `export_pack!`.
- `src/dl.rs` — the host loader: `DlPack`, `DlSystem`, `DlSlot`, and the teardown ordering.
- `src/wiring/` — `Registry::register_pack`, the per-resolve `PackCache`, and entry
  selection for `system` nodes and slot occupants.
- `src/testbench.rs` — `TestBench`, a one-entry harness over the same seam (no
  coordinator; write inputs, `step`, read outputs).

---

## 1. The model: `Pack`, `PackEntry`, `Driver`

`Pack` is a list of erased entries. Each `PackEntry` (`src/pack.rs`) carries what the host
needs *without constructing the system*:

```rust
pub struct PackEntry {
    name: &'static str,               // the registry `type=` / manifest key
    descriptor: SystemDescriptor,     // computed from the parameter TYPES at registration
    params_schema: &'static NamedType,
    params_default: Option<Vec<u8>>,  // declared defaults a config only overrides
    reloadable: bool,                 // false for a `.state(...)` entry
    create: CreateFn,                 // the two-phase constructor (§3)
}
```

The invariant that keeps describe cheap and safe: **`pack()` constructs no user state.**
Descriptors come from parameter types, so describing a pack (the registry's capability
ordering, `fsw_pack_describe`, a describe worker) never runs a line of author init code.
Effects belong in init fns and the create phase; `pack()` is registration only.

Every entry, however authored, binds to one runtime trait (`Driver`, `src/pack.rs`):

```rust
pub trait Driver {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp) -> StepStatus;
    fn shutdown(&mut self);
}

pub enum StepStatus {
    Running,        // a cyclic entry, always
    Done(Outcome),  // a future returned Ready; terminal, never stepped again
}
```

`Driver` is the unification seam: it replaces the old split between `CyclicRunner` (cyclic
systems) and the deleted `SeqSystem`/`SeqBound` stack (sequences). The coordinator wraps a
bound driver in `DriverSlot` (`src/pack.rs`), an ordinary `CyclicSlot`; the ABI shim steps
it through `fsw_pack_execute`. `Done` maps to `SlotState::Done` in a runtime slot and to
`FswStatus::Done` across the ABI.

Two restrictions hold on every path, so a pack behaves identically however it is mounted:

- **No capabilities.** An entry declaring `Capability::ReceiveAll` is rejected —
  `Pack::system_type` asserts at registration, and the loader rejects it at load
  (`DlError::UnsupportedCapabilities`, `src/dl.rs`). Capability systems (the telemetry
  downlink, the alarm engine) stay type-registered on the static registry.
- **Cyclic-scheduled.** An async entry (`Pack::task`) is polled once per cycle through its
  driver, like a sequence — not spawned on the executor. True self-pacing `AsyncSystem`s
  remain static-registry-only.

## 2. Authoring surfaces

### 2.1 Function style: `system(execute_fn)`

The primary surface (`system`, `src/handler/mod.rs`): a system is a state type, an execute
fn whose signature *is* the port set, and optionally an init fn from typed params.

```rust
struct NavState { mekf: mekf::State, sigma: f64 }

fn nav_init(p: NavParams) -> NavState { /* fail-fast construction */ }

fn nav_execute(
    nav: &mut NavState,                       // leading &mut State, required
    time: MissionTime,                        // now + elapsed-since-first-cycle
    sensors: &mut Input<Sensors>,
    estimate: &mut Output<AttitudeEstimate>,
) {
    let Some(s) = sensors.latest() else { return };
    // FrameGrant derefs to Sensors: s.gyro_b, no .get()
}

Pack::new().system("Nav", system(nav_execute).init(nav_init))
```

Three construction forms on `SystemDef` (`src/handler/mod.rs`):

- bare `system(f)` — the state is `Default` and the entry takes no params;
- `.init(fn)` — `fn(P) -> S` (P becomes the entry's params type, decoded from KDL or
  postcard) or `fn() -> S` (`InitFn`). `.defaults(value)` on top declares default params
  (canonical postcard bytes in the entry and its manifest), so a config need spell only
  its overrides;
- `.state(value)` — a prebuilt state moved in, the shared-handle case (§4). Move-once:
  the entry is marked `reloadable: false` and a second instantiation is a resolve-time
  error, never a mid-mission one.

The parameters an execute fn may take, each an `ExecParam` (`src/handler/param.rs`):

| Parameter | Meaning |
|---|---|
| `&mut Input<F>` / `&mut MsgIn<M>` | a wired input port, declared in parameter order |
| `&mut Output<F>` / `&mut MsgOut<M>` | a wired output port |
| `now: Timestamp` | the cycle timestamp (no port) |
| `time: MissionTime` | `now` plus elapsed since the entry's first cycle |
| `&mut HealthPort` | the implicit health handle; at most one |

The trait machinery is the standard function-parameter pattern (Bevy's
`SystemParamFunction`): a marker generic keeps the per-arity blanket impls coherent
(`ExecuteFn`, `src/handler/tuples.rs`, tuples up to arity 16), and **one** tuple expansion
generates declaration and bind in the same order, so descriptor order and bind order
cannot drift — the invariant positional binding rests on. The leading `&mut S` is required
(a stateless fn is `system(f).init(|| ())`); free fns infer cleanly, closures may need
their parameter types spelled out.

The entry's descriptor is the declared ports in parameter order plus the implicit
health/log tail — the same shape `Out<O>` gives a struct system, so a fn-authored entry
and a struct-authored one produce identical ring layouts for the same port set. The driver
is `FnDriver` (`src/handler/driver.rs`): per step it times the call, folds dropped
publishes into `health.error("publish_dropped")`, and runs `end_cycle` —
`CyclicRunner::step` generalized to the parameter-tuple world.

### 2.2 Async style: `Pack::task`

An `async fn` whose ports are **by value**, moved into the future at bind; state lives in
locals (`Pack::task`, `src/pack.rs`). This is the sequence authoring model made a general
system surface:

```rust
async fn commissioning(
    Params(p): Params<CommissionParams>,     // typed params, axum-Json style
    mut att: Input<AttitudeEstimate>,
    mut mode: Output<ModeCmd>,
) -> Outcome {
    progress("warming up");
    if wait(Duration::from_millis(100)).await.aborted() {
        mode.publish(&ModeCmd::safe().stamped(now()));
        return Outcome::Aborted;
    }
    /* ... */
    Outcome::Completed
}

Pack::new().task("commissioning", commissioning)
```

The parameters are `TaskParam`s (`src/handler/task.rs`): `Input<F>`, `Output<F>`,
`MsgIn<M>`, `MsgOut<M>` by value; `Params<P>` for the entry's typed params — the nominal
replacement for the deleted `#[sequence]` macro's "the one unrecognized parameter is the
params" rule (`Pack::task_with_defaults` declares defaults for it); and `Seq` for an
explicit clock handle. The future may resolve to `Outcome` (a sequence) or `()` (a task
that simply completed) via `IntoOutcome`; arities up to 12 (`AsyncSystemFn`).

The driver is `FutureDriver` (`src/handler/driver.rs`): once per cycle it refreshes the
ambient `CycleClock`, polls once with a no-op waker, folds the shared drop counter (§7),
and runs the health tail. `wait()`/`now()`/`progress()`/`aborted()` work unchanged; the
general-system suspension point is `cycle().await` (`src/sequence/mod.rs`):

```rust
async fn pump(mut cmds: MsgIn<GroundCmd>, mut tm: Output<Telemetry>) {
    loop {
        let now = cycle().await;             // resolves next cycle with that cycle's now
        if aborted() { return }              // cancellation is explicit, not automatic
        while let Some(cmd) = cmds.try_next() { /* ... */ }
    }
}
```

`cycle()` is wakerless and deterministic under a simulated clock (`NextCycle`,
`src/sequence/mod.rs`) — the driver re-polls every cycle anyway. It deliberately does
not observe cancellation: a cancellable loop checks `aborted()` after each await.
`SeqClock` survives as a type alias for `CycleClock` (`src/sequence/mod.rs`). A wired
(non-occupant) task has no `SequenceStatus` output; its progress lines land on the
ordinary log (`src/handler/driver.rs`).

### 2.3 Struct style: `Pack::system_type`

`#[system]`-authored (or hand-written) struct systems ride into packs unchanged
(`Pack::system_type::<T, _>("name")`, `src/pack.rs`): the entry's descriptor is the type's
static one, `BuildSystem` supplies params and construction, and the driver wraps an
ordinary `CyclicRunner`. The adcs example keeps `ctrl` authored this way on purpose so
both styles stay exercised end to end. `#[system]` itself no longer takes `export` args —
a system reaches a cdylib through its crate's pack, not a per-system export.

## 3. Two-phase construction: create, then bind

Entry construction mirrors the ABI's create/bind split (`src/pack.rs`):

```rust
pub type Pending = Box<dyn FnOnce(&mut AnySource, Mount) -> Box<dyn Driver>>;
// PackEntry::create(params: EntryParams<'_>) -> Result<Pending, MakeError>
```

- **Create** decodes the params and builds the user state — fail-fast, no rings exist yet.
  `EntryParams` is the two-encoding surface: `Postcard(&[u8])` on the dl/worker path,
  `Kdl { node, .. }` on the static path; both decode to the same value by construction
  (the KDL front-end encodes against the same schema, overlaying any declared defaults).
  Failures are `MakeError`: bad postcard, a spanned KDL error, or `StateTaken` for a
  re-created `.state(...)` entry.
- **Bind** runs the returned `Pending` over a ring source, binding ports positionally in
  descriptor order and yielding the runnable `Driver`.

The ring seam is `AnySource` (`src/binder.rs`), a two-armed enum so the boxed closure
can take one concrete type without giving up `RingSource`'s generic methods:

```rust
pub enum AnySource<'a, 'b> {
    Host(&'a mut Binder<'b>),               // the static path's pre-allocated rings
    Raw(&'a mut abi::RawBinder<'b>),        // the .so-side cursor over host ring handles
}
```

The enum (rather than a `dyn` wrapper) keeps the host `Binder`'s matched wake endpoints
intact, which leaves the door open for async entries on the static path.

On the static path, `CoordinatorBuilder::add_pack_entry` (`src/coordinator/mod.rs`)
runs create at registration (so a bad config fails there, not at `build()`) and the bind
phase at `build()` like any static system.

## 4. Shared state between entries

`pack()` runs **once per load**. Entries capture clones of whatever it built —
`Rc<RefCell<_>>` in the single-threaded cyclic world — via `.state(shared)` or by closing
over the handle in an init closure. That construction point in common is what the
framework never had: two systems can now share one owned resource (a socket, a bus
handle), which is the in-process answer to DESIGN.md's shared uplink+downlink-connection
limitation.

`.state(v)` stores `Option<S>` and the first create takes it (`src/handler/mod.rs`). That
makes the entry non-reloadable by construction — a second instance or a slot `Reset` has
no state left to move — so it carries `reloadable: false` and the **resolver** rejects it
as a slot occupant or a second instance with a spanned error
(`LoadError::OccupantNotReloadable`), a config-time diagnostic rather than a mid-mission
failure.

The boundary, stated as loudly as the feature: a `process=#true` worker runs `pack()` **in
its own address space**, so two entries mounted in different processes each see a fresh
pack and share nothing. Pack-level shared state is an in-process answer; the cross-process
form still needs a "shared owned resource" abstraction and stays future work.

## 5. The pack ABI (v5)

One cdylib exports one **pack** — any number of systems — through nine fixed, versioned
`extern "C"` symbols (`FSW_ABI_VERSION = 5`, `src/abi/mod.rs`), replacing the
single-system `fsw_describe`/`fsw_create` family entirely. The lifecycle, in call order
(`src/abi/mod.rs`):

| Symbol | Signature | Purpose |
|---|---|---|
| `fsw_abi_version` | `() -> u32` | the ABI word, checked for equality first |
| `fsw_pack_open` | `() -> *mut c_void` | run `pack()` once, box it; null on panic |
| `fsw_pack_describe` | `(pack, ByteSink, ctx) -> i32` | postcard `PackManifestMsg` to a host sink |
| `fsw_pack_create` | `(pack, index: u32, mount: u32, params, len) -> *mut c_void` | one entry's create phase; opaque instance state |
| `fsw_pack_bind_init` | `(state, in_rings, n_in, out_rings, n_out)` | positional `RawBinder` bind + driver init |
| `fsw_pack_execute` | `(state, now: u64) -> u32` | one step; an `FswStatus` word (`Done` for a finished future) |
| `fsw_pack_shutdown` | `(state)` | the driver's shutdown |
| `fsw_pack_destroy` | `(state)` | drop one instance's state inside the `.so` |
| `fsw_pack_close` | `(pack)` | drop the `Pack`, after every instance is destroyed |

`create`..`destroy` repeat per instance (two `system` nodes over one entry, or a slot
occupant reloaded); `open`/`close` bracket the whole load. Entries are addressed by
**manifest index** — position in `PackManifestMsg::systems` (`src/abi/mod.rs`), stable
because `pack()` is deterministic registration code; the host resolves names to indices
from the manifest, and the ABI never carries a name after describe. Each manifest entry
(`PackSystemMsg`, `src/abi/mod.rs`) is the descriptor wire mirror plus `reloadable`
and the optional `params_default` blob.

The three soundness rules are retained verbatim (`src/abi/mod.rs`): only serialized
bytes and `repr(C)` handles cross; no unwind crosses `extern "C"` (every `run_pack_*`
helper catches panics — null pointer, non-zero describe code, or `FswStatus::Panicked`
with a poisoned latch); each side frees only what it allocated. An unknown `mount` word
folds to `Wired` (`src/abi/mod.rs`); the host routes every returned status word
through `FswStatus::from_raw` (`src/abi/mod.rs`).

`export_pack!(pack)` (`src/abi/mod.rs`) emits the nine exports as one-liners
delegating to the `run_pack_*` helpers — **one invocation per crate** (the symbols are
un-namespaced C names, which is exactly why a crate exports one pack rather than one
system). `export_pack!(pack, feature = "...")` additionally gates on a cargo feature so
the rlib a host links for tests stays symbol-free. The old `export_system!` is gone.

## 6. The loader: `DlPack` → `DlSystem` → `DlSlot`

`DlPack::open(path)` (`src/dl.rs`) loads the object, checks the version word, opens the
pack (`fsw_pack_open` — the one call of `pack()` for this load), resolves every symbol,
and decodes the manifest into per-entry descriptors (host capabilities rejected).
`DlPack::system(name)` selects one entry as a `DlSystem` — the unit `add_dl_cyclic` and a
slot's allowed-occupant list consume — or errors with
`DlError::UnknownPackSystem { available }` listing the pack's exports.
`DlPack::sole_system()` is why a one-entry pack's `system` node may omit `type=`.

Teardown extends the dl ordering contract by one link (`src/dl.rs` module docs):
**destroy every instance state → `fsw_pack_close` → `dlclose` → host frees the rings.**
The ordering is structural, not conventional: every `DlSystem`/`DlSlot` holds an
`Rc<PackLib>`, whose field order closes the pack before the `Library` unloads, and a
`DlSlot`'s `Drop` destroys its state before its `Rc` clone can release. So no pack code
runs after unload, and the shared state the entries captured drops while its code is still
mapped.

## 7. The dropped-publish counter

A cyclic entry's runner owns its ports, so dropped publishes fold into health directly. A
future-owned port has no runner to ask, which was an open TODO in the old sequence stack.
The fix touches only the failure path (`Drops`, `src/port.rs`):

```rust
pub(crate) enum Drops {
    Local(u64),                          // the common, runner-owned case
    Shared(Arc<AtomicU64>),              // future-owned ports: clones of one per-driver cell
}
```

`TaskParam::bind` adopts the driver's shared cell before the port moves into the future
(`share_drops`, `src/port.rs`); per step, `FutureDriver` swaps the cell and folds a
nonzero count into `health.error("publish_dropped")` — the same fold `CyclicRunner::step`
does. The design sketched `Rc<Cell<u64>>`; the shipped cell is an `Arc<AtomicU64>`, still
touched only on the failure arm.

## 8. Wiring

```kdl
// one crate, N systems — no type= on the artifact node
artifact "adcs" crate="adcs-systems" lib="adcs_systems"
artifact "seqs" crate="adcs-sequences" lib="adcs_sequences"

system "nav"  artifact="adcs" type="Nav" meas_sigma=0.02   // type= selects the pack entry
system "solo" artifact="adcs"                              // legal iff the pack has one entry

slot "mode" {
    allow occupant="commissioning" artifact="seqs"   // artifact= optional: omitted searches
    allow occupant="safe_mode"                       // every artifact for a unique entry name
}
```

- An `artifact` node has no `type=` (a pack exports many); the legacy spelling is a
  pointed error (`LoadError::ArtifactType`, `src/wiring/parse.rs`).
- A `system` node's `type=` names the **pack entry**; omitted, the pack must export
  exactly one (`select_entry`, `src/wiring/mod.rs`; otherwise `PackTypeRequired` listing
  the choices).
- A slot's `allow` gains an optional `artifact=`; absent, resolve searches every artifact
  for a unique entry of that name, and an ambiguous or missing name is a clean spanned
  error (`occupant_artifact`, `src/wiring/mod.rs`).
- Packs are opened **once per resolve** via a cache keyed by artifact id (`PackCache`,
  `src/wiring/mod.rs`). This is correctness, not just speed: reopening would re-run
  `pack()` and fork the shared state its entries captured.
- Non-reloadable (`.state(...)`) entries are rejected as slot occupants at resolve (§4).

On the static path, `Registry::register_pack(pack)` (`src/wiring/mod.rs`) lands every
entry in the same `type=` table as type-registered systems — the same `pack()` a cdylib
exports serves a statically-linked mission, and the KDL surface cannot tell the difference.

## 9. Mounts and the occupant tail

How an entry is mounted is a property of the **mount**, not of the entry's descriptor
(`Mount`, `src/pack.rs`): `Wired` (an ordinary system for the whole run) or `SlotOccupant`
(loaded into a runtime slot). The framework's occupant tail — the `SlotControlIn` cancel
input and the `SequenceStatus` output — is appended around the entry's own ports by the
mount, which is why **slots accept any entry** and no entry pays for slot support it does
not use.

The two sides that must agree on the appended ports:

- The host's `add_slot` derives the slot contract by extending the occupant descriptor:
  the cancel input after the entry's inputs (`src/coordinator/mod.rs`), the status
  output after the entry's outputs — which already end in health/log
  (`src/coordinator/mod.rs`). An occupant that declares either itself is a pre-pack
  artifact and is rejected.
- The occupant-mount bind appends in the same order (`mount_driver`,
  `src/handler/driver.rs`): the entry's own ports bind first, unchanged, then the
  cancel input, then the status output. So the bind order is user inputs →
  `SlotControlIn`, and user outputs → health → log → `SequenceStatus` — status binds
  **last**, after the entry's health/log tail, so the inner driver binds exactly as it
  would wired.

Occupant behavior per style:

- An async occupant (`OccupantFuture`, `src/handler/driver.rs`) folds a latched cancel
  into the clock before each poll and publishes a `SequenceStatus` record per cycle —
  cooperative cancellation (`aborted()` between awaits) with a safing branch.
- A sync occupant (`OccupantCyclic`, `src/handler/driver.rs`) gets **stop-on-cancel**:
  a latched cancel stops stepping the inner driver and reports a terminal
  `Done(Aborted)` — the hard-but-clean stop; a sync entry has no await points to observe
  a cancel cooperatively.

Everything above the occupant — the slot state machine, `Load`/`Start`/`Stop`/`Abort`/
`Reset` commands, `SlotStatus` and the events channel — is unchanged from
`docs/sequences-slots.md`; only the "occupants must be sequences" restriction is gone.

## 10. Process workers

Process mode composes with packs at the manifest (`WorkerManifest`,
`src/proc/worker.rs`):

- A **describe** worker's output bytes are the pack manifest, decoded host-side by the
  same `decode_pack_manifest` (`src/dl.rs`) the dl path uses. One describe run covers
  every entry of an artifact.
- A **run** worker's manifest carries `system: String` — the pack entry name
  (`src/proc/worker.rs`) — which the worker resolves through its own
  `DlPack::open(...).system(name)` before driving the ordinary `DlSlot` lifecycle. A
  process slot's occupant runs the same way in `RunMode::Sequence`, where a terminal
  `Done` is latched worker-side (`DlSlot::step_seq`, `src/dl.rs`) so a `Ready` future is
  never polled again.

And per §4: each run worker executes `pack()` in its own address space, so process-mode
entries share no pack state — a mission that wants sharing keeps those entries in-process.
