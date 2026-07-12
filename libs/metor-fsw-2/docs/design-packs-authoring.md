# Packs & functional authoring (`design-packs-authoring`)

> **Status: LANDED** (2026-07-11, commits `d4ef52be`/`3fbd6721`/`d103958e` — WP2/WP3/WP4).
> The final-state doc is `docs/packs.md`; this document is the design record. Deviations
> from the prose below, noted per-WP in `docs/design-packs-authoring-plan.md`: the erased
> `make` split into a two-phase `create -> Pending` (fail-fast params/state, bind later);
> there is no separate sequence registrar (`Pack::task` subsumes it — the occupant tail is
> purely a mount property); the occupant-mount bind order puts `SequenceStatus` **last**,
> after the entry's health/log tail (§9.2's sketch had it before); `Drops::Shared` is an
> `Arc<AtomicU64>`, not `Rc<Cell>`; `SeqClock` survives as a type alias for `CycleClock`;
> `#[sequence]` is a passthrough shim slated for deletion; sync occupants get
> stop-on-cancel → terminal `Aborted`; `PackLib` is shared behind `Rc`, not `Arc`.

Status when written: **design** (2026-07-11). The approved authoring-ergonomics overhaul:
multi-system crates (**packs**), an axum-style functional author surface as the primary
style, and sequence/system **unification** behind one `Driver` seam, with dl ABI v5
carrying it all. Companion plan: `docs/design-packs-authoring-plan.md`.

---

## 1. Problem

metor-fsw-2 today encourages one crate per system, and it splits the runtime into two
parallel stacks that agree only by convention:

- **One system per cdylib.** The seven fixed, un-namespaced `fsw_*` C symbols
  (`src/abi/mod.rs:170-182`) admit exactly one exported system per shared object, and
  `Artifact.system_type` is singular (`src/wiring/model.rs:100`). A mission with five small
  systems is five crates, five artifacts, five `cargo build -p` invocations.
- **Struct + impl ceremony.** Even with `#[system]` (docs/design-system-macro.md), the
  minimal system is a struct, an annotated impl block, and a `new`. For a ten-line filter
  that is still scaffolding around one function.
- **No shared state between systems.** Two systems that want to share an owned resource — the
  acknowledged shared uplink+downlink-connection limitation (DESIGN.md §"Limitations and
  future work") — have no construction point in common: each is built independently by its
  factory or its `fsw_create`.
- **Two runner stacks.** A cyclic system runs behind `CyclicRunner`
  (`src/system/mod.rs:371-384`) and the `run_*` ABI helpers; a sequence runs behind
  `SeqBound`/`SeqSystem` (`src/sequence/mod.rs:333-358`) and a parallel `run_seq_*` family
  (`src/abi/mod.rs:761-947`). They duplicate the create/bind/execute/poison lifecycle and
  differ in ways that leak: sequences are slot-only, cyclic systems cannot occupy a slot,
  and the sequence stack has an open dropped-publish TODO (`src/sequence/mod.rs:326-332`)
  the cyclic stack solved.
- **Read ceremony.** `input.latest()` then `.get()` is a two-step for every read
  (`src/port.rs:272`), and `MsgIn` had no consuming single-message read.

The user-confirmed goals, in priority order: (1) multiple systems per crate; (2) functional
authoring — free `execute`/`init` fns over an abstract state — as the **primary** surface,
with a crate-level `fn pack()` registering all systems and able to share state between
them; (3) sequences and systems unified, with both the sync execute-fn style and the async
fn style first-class, swappability the only real difference; (4) kill the
`latest()`+`.get()` two-step.

What exploration established, and this design builds on:

- Binding is **positional** over `RingSource` (`src/binder.rs:113-166`): every port stack
  pops rings in `descriptors()` order. Any new authoring surface must target that seam.
- Sequences are polled once per cycle with a noop waker against the ambient `SeqClock`
  (`src/sequence/mod.rs:66-107`); `Wait` resolves purely by comparing a stored deadline to
  `clock.now` (`src/sequence/mod.rs:159-172`). Deterministic, wakerless, cheap.
- `Deref` is impossible on `Input` (a read advances the cursor; `deref` takes `&self`) but
  sound on `FrameGrant`, whose borrow is already tied to the grant.
- The static registry is a `type`-string → factory table (`src/wiring/mod.rs:126`,
  `:213-295`) — exactly the shape a pack can feed.
- dl params have no defaults (`src/wiring/kdl_params.rs:93-95`), which is why mission.kdl
  spells out every field (`examples/adcs-fsw2/mission.kdl:17`).

---

## 2. Architectural spine: `Pack` and `Driver`

A **`Pack`** value — a `Vec` of erased entries — is the single seam serving all three
loading modes: the static registry (`Registry::register_pack`), the new multi-system dl ABI
(v5, §7), and the process-worker path (§8.3). Every authoring style converges on one
internal trait that replaces the `CyclicRunner`/`SeqBound` bifurcation behind the ABI and
the registry:

```rust
pub trait Driver {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp) -> StepStatus;
    fn shutdown(&mut self);
}

pub enum StepStatus {
    Running,
    Done(Outcome),   // a future returned Ready; terminal, stop stepping
}
```

`CyclicSlot` (`src/coordinator/mod.rs:535`) stays the coordinator's interface; a thin
`DriverSlot` adapter wraps a `Box<dyn Driver>` in it, mapping `Done` into the slot state
exactly as the runtime slot runner maps `FswStatus::Done` today. The coordinator, the
telemetry tap, health folding — none of it changes; the unification happens one layer
below, where today two stacks live.

### 2.1 `PackEntry`

```rust
pub struct Pack { entries: Vec<PackEntry> }

pub struct PackEntry {
    pub name: &'static str,
    pub descriptor: SystemDescriptor,
    pub params_schema: OwnedNamedType,
    /// Canonical postcard bytes of the default params value. `None` until the
    /// defaults extra lands (plan WP6a); the field exists from day one so the
    /// manifest never needs a v6 for it.
    pub params_default: Option<Vec<u8>>,
    /// `false` for a `.state(prebuilt)` entry (§2.4): one instance, ever.
    pub reloadable: bool,
    make: Box<dyn FnMut(EntryParams<'_>, &mut AnySource<'_>, Mount)
        -> Result<Box<dyn Driver>, MakeError>>,
}

/// Where an entry's params come from — mirroring how the two construction
/// paths already split: the registry factory decodes a KDL node in place
/// (`wiring::factory`), the ABI decodes postcard bytes (`abi::run_create`).
pub enum EntryParams<'a> {
    Postcard(&'a [u8]),
    Kdl { node: &'a KdlNode, src: &'a str, name: &'a str },
}

/// How the entry is being mounted (§9.2): as an ordinary wired system, or as
/// a slot occupant with the occupant tail appended.
pub enum Mount { Wired, SlotOccupant }
```

The invariant that makes describe cheap and safe: **the descriptor is computed from the
param *types* at `.system()` registration time — `pack()` constructs no user state.** All
user init runs inside `make`. So `fsw_pack_describe` just calls `pack()` and lowers each
entry's descriptor; a describe never executes a line of author init code, matching today's
`fsw_describe` (which lowers a static descriptor without constructing the system).

### 2.2 The authoring surface

```rust
// one crate, many systems
struct NavState { mekf: mekf::State, sigma: f64 }
fn nav_init(p: NavParams) -> NavState { /* ... */ }
fn nav_execute(state: &mut NavState, now: Timestamp,
               sensors: &mut Input<Sensors>, estimate: &mut Output<AttitudeEstimate>) {
    let Some(s) = sensors.latest() else { return };
    // FrameGrant derefs to Sensors (WP1): s.gyro_b, no .get()
    /* ... */
}

async fn pump(mut cmds: MsgIn<GroundCmd>, mut tm: Output<Telemetry>) {  // state = locals
    loop {
        let now = cycle().await;
        if aborted() { return }
        while let Some(cmd) = cmds.try_next() { /* ... */ }
    }
}

pub fn pack() -> Pack {
    let bus = Rc::new(RefCell::new(UartBus::open()));        // shared across entries
    Pack::new()
        .system("nav",  system(nav_execute).init(nav_init))
        .system("ctrl", system(ctrl_execute).state(CtrlState::with_bus(bus.clone())))
        .task("pump", pump)
        .system_type::<LegacySystem>("legacy")                // #[system] structs ride in
}
metor_fsw_2::export_pack!(pack);
```

`system(f)` wraps a sync execute fn (leading `&mut S`, §4); `.init(fn)` supplies the
`P -> S` constructor; `.task(name, f)` registers an async fn whose state is its locals
(§4.4); `.system_type::<T>()` lowers an existing `#[system]`-authored struct through its
generated trait impls, so both styles coexist in one pack (and one crate keeps exercising
both on purpose — plan WP5).

### 2.3 Shared state between entries

`pack()` runs **once per load**. Entries capture clones of whatever `pack()` built —
`Rc<RefCell<_>>` for the single-threaded cyclic world, `Arc<Mutex<_>>` where a pack
carries an async entry that might migrate — via `.state(shared)` or by closing over the
handle in an init closure. That construction point in common is what the framework never
had: it directly addresses DESIGN.md's shared uplink+downlink-connection limitation for
**in-process** mounts (static, dl, or a slot occupant loaded from the same pack instance).

The boundary must be stated as loudly as the feature: a `process=#true` worker runs
`pack()` **in its own address space**, so two entries mounted in different processes each
see a fresh pack and share nothing. Pack-level shared state is an in-process answer; the
cross-process form still needs the "shared owned resource" abstraction and stays future
work. The docs and DESIGN.md say so explicitly.

### 2.4 `.state(prebuilt)` is move-once

`.state(v)` stores `Option<S>` and the first `make` takes it. That makes the entry
non-reloadable by construction: the second instance, a slot `Reset`, or a slot `Load` has
no state left to move. Rather than police this at runtime, the entry carries
`reloadable: false` and the **resolver** rejects it as a slot occupant or a second
instance with a spanned `LoadError` — the failure is a config-time diagnostic pointing at
the offending node, not a mid-mission `make` error.

*Alternative rejected:* requiring `S: Clone` and cloning per instance. It silently forks
exactly the state the author reached for `.state()` to share (a cloned socket handle is
two sockets or one confused one), and it taxes every plain state type with a bound it
does not need. Move-once with a clean resolve-time rejection keeps the semantics honest.

---

## 3. Where packs sit: three loading modes, one seam

| Mode | Today | With packs |
|---|---|---|
| Static | `register_system!(reg, T => "ty")`, one type per call (`src/wiring/mod.rs:299-304`) | `reg.register_pack(my_crate::pack())` — each entry lands in the same `type=` table; `SystemFactory` widens from a plain `fn` pointer (`src/wiring/mod.rs:126`) to `Box<dyn FnMut>` so a factory can own its pack entry |
| dlopen | one system per `.so`, seven `fsw_*` symbols | one **pack** per `.so`, nine `fsw_pack_*` symbols (§7); entries addressed by index |
| Process worker | `WorkerManifest::Run` drives one artifact = one system (`src/proc/worker.rs:53`) | `Describe` returns the pack manifest; `Run` gains the entry name (§8.3) |

The erased `make` closure is the one body all three share: the static factory calls it
with `EntryParams::Kdl` over a host `Binder`; the ABI calls it with `EntryParams::Postcard`
over a `RawBinder`. One construction path, two parameter encodings — the same split
`wiring::factory` vs `abi::run_create` embodies today, now behind one signature.

---

## 4. Handler machinery

The functional surface is Bevy's `SystemParamFunction` pattern — a marker generic to guide
inference, per-arity `macro_rules!` impls, **no proc macro**. Everything is readable,
steppable trait code; the macro crate is not involved.

### 4.1 `ExecParam` — one trait per port kind

```rust
pub trait ExecParam {
    /// What the driver owns between cycles (the port itself, usually).
    type State: 'static;
    /// What the user fn receives for one cycle.
    type Item<'r>;
    /// Contribute this param's PortDecls to the entry descriptor.
    fn decl(sink: &mut DeclSink);
    /// Bind the state off the ring source (at make).
    fn bind(cx: &mut BindCx<'_>) -> Self::State;
    /// Project the per-cycle item out of the state.
    fn get<'r>(state: &'r mut Self::State, cx: &mut CycleCx<'r>) -> Self::Item<'r>;
}
```

| Param (as written) | `State` | `get` |
|---|---|---|
| `&mut Input<F>` | `Input<F>` | `&mut` projection |
| `&mut MsgIn<M>` | `MsgIn<M>` | `&mut` projection |
| `&mut Output<F>` | `Output<F>` | `&mut` projection |
| `&mut MsgOut<M>` / `&mut CommandOut<M>` | the port | `&mut` projection |
| `Timestamp` | `()` | `cx.now` |
| `&mut HealthPort` | `()` | `cx.health` (`Option::take`; at most one per fn) |

`decl` and `bind` for a whole parameter list are generated by **one** `macro_rules!` tuple
walk, so descriptor order and bind order come from the same expansion and *cannot* drift —
this is the positional-bind invariant `src/binder.rs` and `abi::RawBinder`
(`src/abi/mod.rs:454-509`) both stake correctness on, and it is the design's top
correctness surface (§10, risk 2).

### 4.2 `ExecuteFn` / `InitFn` — the double-`FnMut` trick

```rust
pub trait ExecuteFn<S, Marker>: 'static {
    type Params: ExecParamSet;
    fn call(&mut self, state: &mut S, params: <Self::Params as ExecParamSet>::Item<'_>);
}

// per-arity impls, 0..=16 params, generated by the same tuple walk:
impl<Func, S, P1, P2> ExecuteFn<S, fn(P1, P2)> for Func
where
    P1: ExecParam, P2: ExecParam, S: 'static,
    Func: FnMut(&mut S, P1, P2)
        + for<'r> FnMut(&'r mut S, P1::Item<'r>, P2::Item<'r>)
        + 'static,
{ /* call = self(state, P1::get(..), P2::get(..)) */ }
```

The first `FnMut` bound is the trick (Bevy's proven one): it pins `P1`/`P2` by inference
from the fn type the author actually wrote — `fn nav_execute(&mut NavState, Timestamp,
&mut Input<Sensors>, ...)` unifies `P2 = Timestamp`, `P3 = &mut Input<Sensors>` — while the
second, HRTB-quantified bound is the one `call` actually uses, with the items' lifetimes
tied to the cycle borrow. Without the first bound, inference has nothing to anchor the
GATs to and every call site is an annotation festival.

`InitFn<S, Marker>` is the same shape over `FnMut(P) -> S` and `FnMut() -> S`
(`Params = ()`), with `P: DeserializeOwned + Schema + Serialize` — deserialize for both
params encodings, `Schema` so the entry can lower `params_schema` without an instance,
`Serialize` so WP6a's `.defaults(value)` can encode a default blob.

### 4.3 Why `system(f)` requires a leading `&mut S`

A stateless impl family — `Func: FnMut(P1, P2)` with `P1: ExecParam` — **overlaps** the
stateful family: for a fn like `fn f(Timestamp, &mut Input<A>)`, the compiler can also
read the first parameter as the `&mut S` of the stateful family (with `S = Timestamp`'s
referent, etc.), and coherence has no way to prefer one. So v1 ships **stateful-only**:
`system(f)` requires the leading `&mut S`, and a stateless system is written as
`system(f).init(|| ())` with `S = ()`. If the ergonomics itch hard enough later, a
*distinct* constructor (`stateless_system(f)`) sidesteps coherence without touching the
shipped family. Not overloading `system()` is the decision (§11, D3).

### 4.4 The async style: `TaskParam` and `Pack::task`

Async fns take their ports **by value** — the future owns them for its whole life, exactly
the `#[sequence]` model (docs/sequences-slots.md §4) — so the async style gets its own,
simpler trait:

```rust
pub trait TaskParam: Sized + 'static {
    fn decl(sink: &mut DeclSink);
    fn bind(cx: &mut BindCx<'_>) -> Self;
}
```

Impls: `Input<F>`, `MsgIn<M>`, `Output<F>`, `MsgOut<M>`, `CommandOut<M>`,
`sequence::Seq` (the explicit clock handle), and `Params<P>`:

```rust
/// Decoded entry params in an async signature, axum-Json style:
/// `async fn commission(Params(p): Params<CommissionParams>, mut mode: Output<ModeCmd>)`.
pub struct Params<P>(pub P);
```

`Params<P>` replaces `#[sequence]`'s "the one unrecognized parameter type is the params
parameter" rule — a rule only a macro scanning a signature can have. A trait-resolved
world needs a nominal marker, and the newtype is self-documenting where the convention was
invisible.

```rust
pub trait AsyncSystemFn<Marker>: 'static {
    type Params;   // the TaskParam tuple
    type Fut: Future + 'static;
    fn build(self, params: Self::Params) -> Self::Fut;
}
// impls over FnOnce(P1..Pn) -> Fut, Fut::Output: IntoOutcome
```

`IntoOutcome` is implemented for `Outcome` (identity) and `()` (`Completed`), so a plain
`async fn pump(..)` that runs forever-or-returns needs no ceremony. Entry is the
**distinct** `Pack::task(name, f)` — not an overload of `system()`, whose `Marker`
inference the `FnOnce`-returning-`Fut` family would fight.

### 4.5 Diagnostics

`#[diagnostic::on_unimplemented]` on `ExecuteFn`, `InitFn`, `AsyncSystemFn`, `ExecParam`,
and `TaskParam` turns the classic Bevy wall-of-generics into "`&Input<Sensors>` is not an
execute parameter: ports are taken `&mut`". A trybuild UI suite pins the messages (the
`#[system]` suite's precedent, docs/design-system-macro.md §11). One documented limitation
stays: **closures** with `&mut` port params may need type annotations under the HRTB
bound. The primary surface is free fns, where inference is anchored by the fn item's type;
the escape hatch, if closures ever matter, is a tiny annotation-only attribute macro that
does not change the API (§10, risk 1).

---

## 5. The ring-source seam: `AnySource`

`RingSource` (`src/binder.rs:113`) has generic methods (`next_output<WD, WS>`), so it is
not object-safe — and the erased `make` closure needs a concrete argument type. The
decision is a two-armed enum, not a lossy dyn wrapper:

```rust
pub enum AnySource<'a> {
    Host(Binder<'a>),
    Raw(RawBinder<'a>),
}
impl RingSource for AnySource<'_> { /* delegate per arm, all five methods */ }
```

Why not a `DynRingSource` that erases the wake generics to `NoWake`: the host `Binder`
carries **matched data-wake endpoints** for async inputs (`src/binder.rs:11-22`, the
`BoundPort::matched` machinery), and flattening them away would foreclose async entries on
the static path forever. The enum keeps wake-endpoint fidelity at the cost of one `match`
per pop, and leaves that door open. If the enum ever fights the boxed closure in practice,
the dyn form is the documented fallback — but v1 does not need it.

Two v1 restrictions ride this seam, both consistent with what dlopen'd systems already
live with:

- **Pack entries are cyclic-scheduled.** An async entry (`Pack::task`) is polled once per
  cycle through its driver, like a sequence — not spawned on stellarator. True
  `AsyncSystem`s (self-pacing, awaiting ring notifiers) remain static-registry-only.
- **Pack entries cannot hold `Capability::ReceiveAll`** (`src/descriptor.rs:150`). The dl
  loader already rejects capabilities (`src/dl.rs:97-104`); packs keep the same rule on
  every path, so a pack behaves identically however it is mounted.

---

## 6. Drivers

### 6.1 `FnDriver` — the sync generalization of `CyclicRunner`

```rust
struct FnDriver<S, F, P: ExecParamSet> {
    state: S,
    func: F,                    // the ExecuteFn
    ports: P::State,            // the bound param states, in decl order
    tail: HealthTail,           // health + log, bound last
}
```

The tail binds **after** the user ports, the same order `Out::bind` uses today
(`src/system/mod.rs:172-177`), so a fn-authored entry and a struct-authored one produce
byte-identical ring layouts for the same port set. `step` is a faithful generalization of
`CyclicRunner::step` (`src/system/mod.rs:371-384`): build the `CycleCx` (now, the
health-port `Option`), `get` the items, call the fn, fold `take_dropped` into a
`publish_dropped` health error, `end_cycle`. `StepStatus::Running` always — a sync entry
never completes.

### 6.2 `FutureDriver` — the async twin

Owns the `Pin<Box<dyn Future<Output = Outcome>>>`, the `CycleClock` (§9.1), and the same
tail. Per step: refresh the clock, fold the cancel input when mounted as an occupant, poll
once with `Waker::noop()`, publish `SequenceStatus` when occupant-mounted, fold the shared
drop counter (§9.3), `end_cycle`; `Ready(outcome)` becomes `StepStatus::Done(outcome)`.
This is `abi::run_seq_execute` (`src/abi/mod.rs:847-899`) rehomed behind `Driver`, where
both the ABI and the static path can drive it. The Wired form lands in WP2 (an async
entry as an ordinary system); the occupant form completes in WP4 (§9.2).

---

## 7. dl ABI v5: index-parameterized symbols

### 7.1 Shape decision: fixed symbols, not a vtable

The alternative — `fsw_pack_open` returns a `repr(C)` struct of function pointers — was
rejected. The ABI's soundness rests on three rules (`src/abi/mod.rs:24-37`), the first of
which is *only serialized bytes and `repr(C)` handles cross the boundary*. A table of
callable pointers crossing **by value** weakens exactly that rule, adds a second trust
surface (every pointer in the table is attacker-shaped data from a stale or hostile
artifact, where today `dlsym` failures surface as clean `MissingSymbol` errors at load),
and buys nothing: the host still dlopens, still versions, still validates. Fixed symbols
keep dlsym-time validation, keep the `DlSlot` pointer-field shape (`src/dl.rs:346-367`),
and make the diff mechanical. `FSW_ABI_VERSION` 4 → 5 (`src/abi/mod.rs:77`).

### 7.2 The nine symbols

| Symbol | Signature | Notes |
|---|---|---|
| `fsw_abi_version` | `() -> u32` | unchanged; checked for equality first |
| `fsw_pack_open` | `() -> *mut c_void` | runs `pack()` once, boxes the entry vec; null on panic |
| `fsw_pack_describe` | `(pack, ByteSink, ctx) -> i32` | postcard `PackManifestMsg` (§7.3) |
| `fsw_pack_create` | `(pack, index: u32, mount: u32, params: *const u8, len) -> *mut c_void` | bounds-checks `index` (null on OOB); decodes params; sync init runs here fail-fast; async entries stash decoded params pending — ports do not exist yet |
| `fsw_pack_bind_init` | `(state, in_rings, n_in, out_rings, n_out)` | `RawBinder` walk, unchanged contract |
| `fsw_pack_execute` | `(state, now: u64) -> u32` | `FswStatus` word; `StepStatus::Done` → `FswStatus::Done` |
| `fsw_pack_shutdown` | `(state)` | |
| `fsw_pack_destroy` | `(state)` | drops one entry's driver |
| `fsw_pack_close` | `(pack)` | drops the pack (and the shared state its entries captured) |

All `run_pack_*` helpers `catch_unwind` with a poisoned latch mirroring `AbiState`
(`src/abi/mod.rs:524-528`); an unknown `mount` word folds to `Wired` (the host is newer or
older, either way the safe default is the ordinary shape); the host routes every returned
status word through `FswStatus::from_raw` (`src/abi/mod.rs:136-143`) as today.

`export_pack!(pack)` (plus the optional `feature = "..."` gate, the `#[system(export =
"...")]` precedent) emits the nine `extern "C"` one-liners delegating into `run_pack_*` —
the same shape `export_system!` has (`macros/src/export.rs`). One `export_pack!` per
crate, so the symbol-collision problem that forced one-system-per-cdylib is structurally
gone rather than policed.

### 7.3 The manifest

```rust
#[derive(Serialize, Deserialize)]
pub struct PackManifestMsg {
    pub systems: Vec<PackSystemMsg>,
}
#[derive(Serialize, Deserialize)]
pub struct PackSystemMsg {
    pub descriptor: SystemDescriptorMsg,      // reused verbatim (src/abi/mod.rs:242-254)
    pub reloadable: bool,
    /// Canonical postcard bytes of the default params value. None until the
    /// defaults extra (plan WP6a) lands; present in the wire shape from day
    /// one so defaults need no v6.
    pub params_default: Option<Vec<u8>>,
}
```

Entry **index = position in `systems`**, stable for the life of the loaded pack because
`pack()` is deterministic registration code (no user state, §2.1). The host resolves names
to indices from the manifest; the ABI itself never carries a name after describe.

### 7.4 Legacy deletion, same phase

The 7-symbol single-system surface, `export_system!`, `#[system(export)]`'s export args,
and the whole `run_seq_*` family (`src/abi/mod.rs:761-947`) are **deleted in the same
work package**, not deprecated alongside. One ABI, one loader path, one thing to reason
about at the trust boundary. The workspace is `publish = false`, so there is no external
consumer to stage for; a stale v4 artifact fails closed on the version word
(`src/dl.rs:189-194`), which is precisely what the word is for.

---

## 8. Loader, wiring, workers

### 8.1 Loader: `DlPack`

```rust
pub struct DlPack {
    lib: Arc<PackLib>,
    systems: Vec<PackSystemMeta>,   // name, descriptor, params_schema, reloadable, default
    create: PackCreateFn, bind_init: PackBindInitFn, execute: PackExecuteFn,
    shutdown: PackShutdownFn, destroy: PackDestroyFn,
}

struct PackLib {
    lib: Library,
    pack: *mut c_void,          // from fsw_pack_open
    close: PackCloseFn,
}
impl Drop for PackLib {
    // fsw_pack_close BEFORE the Library field drops (declaration order):
    // no pack code runs after unload, and the shared state the entries
    // captured drops while its code is still mapped.
}

pub(crate) struct DlPackSystem {   // replaces DlSystem at the two mount points
    lib: Arc<PackLib>,
    index: u32,
    meta: PackSystemMeta,
}
```

`DlPackSystem` slots into `add_dl_cyclic` (`src/coordinator/mod.rs:1073`) and
`AllowedOccupant::dl` (`src/coordinator/slot.rs:148`) where `DlSystem` sits today. The
teardown contract extends `src/dl.rs:29-40`'s ordering by one link: **destroy every entry
state → `fsw_pack_close` → `dlclose` → host frees the rings.** The `Arc<PackLib>` chain
makes destroy-before-close structural — every entry state holds the `Arc`, so `PackLib`'s
`Drop` (close, then unload) cannot run while any entry is alive.

### 8.2 Wiring surface

```kdl
// one crate, N systems — no more type= on the artifact
artifact "adcs" crate="adcs-systems" lib="adcs_systems"
artifact "seqs" crate="adcs-sequences" lib="adcs_sequences"

system "nav"  artifact="adcs" type="nav"  meas_sigma=0.02
system "solo" artifact="adcs"                       // type optional iff the pack has one entry

slot "mode" {
    allow occupant="commissioning" artifact="seqs"  // artifact= optional: omitted searches
    allow occupant="safe_mode"                      // all artifacts for a unique entry name;
}                                                   // ambiguity is a clean error
```

- `Artifact` drops `system_type` (`src/wiring/model.rs:100`); `SystemSpec.ty`
  (`src/wiring/model.rs:114-119`) now names the **pack entry**, optional when the pack has
  exactly one.
- `AllowedOccupantSpec` (`src/wiring/model.rs:234`) gains `artifact: Option<String>`.
- Resolve keeps a **per-resolve `DlPack` cache** keyed by artifact id — today
  `open_occupant` reopens the object per system instance (`src/wiring/mod.rs:551-582`, the
  `DlSystem::open` at `:563`); with packs, reopening would also re-run `pack()` and fork
  the shared state, so the cache is correctness, not just speed.
- New spanned errors: `UnknownPackSystem { available }` (lists the pack's entry names,
  the `UnknownMsg` precedent), non-reloadable entry named as a slot occupant or second
  instance (§2.4), ambiguous occupant name across artifacts.
- `parse_artifact` (`src/wiring/parse.rs:145`) gives the legacy form a pointed error:
  `artifact … type=` → "packs export many systems; name the system on the `system` node".
- The bundle format version bumps with the model change.

### 8.3 Process workers

`WorkerManifest::Describe` (`src/proc/worker.rs:46`) already ships raw describe bytes to
the host; those bytes simply become the pack manifest, decoded host-side by the same
manifest decoder the dl path uses. `WorkerManifest::Run` (`src/proc/worker.rs:53`) gains
`system: String` — the entry name the worker resolves to an index after its own
`fsw_pack_open`. Describe-per-artifact is cached at resolve (N process systems from one
pack = one describe worker). And per §2.3: each run worker executes `pack()` in its own
address space, so **process-mode systems cannot share pack state** — documented at every
surface that mentions sharing.

---

## 9. Unification: sequences become entries (WP4)

### 9.1 `CycleClock` and `cycle().await`

`SeqClock` (`src/sequence/mod.rs:66-74`) is renamed `CycleClock` (old name re-exported) —
it was never sequence-specific, it is "the cyclic schedule, visible to a future". One new
awaitable joins `wait`/`now`/`progress`/`aborted`:

```rust
/// Suspend until the next cycle; resolves with that cycle's `now`.
pub fn cycle() -> NextCycle {
    NextCycle { armed_at: current().expect("cycle() outside a poll").now.get() }
}

impl Future for NextCycle {
    type Output = Timestamp;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Timestamp> {
        let now = current().expect("NextCycle polled outside a poll").now.get();
        if now > self.armed_at { Poll::Ready(now) } else { Poll::Pending }
    }
}
```

No waker — the driver re-polls every cycle anyway, the same shape as `Wait`
(`src/sequence/mod.rs:159-172`), and `armed_at` comparison makes it deterministic under a
simulated clock. `cycle()` deliberately does **not** auto-abort: an `async fn` that loops
on `cycle()` is usually a *system*, whose cancel semantics should be explicit; the idiom
is `if aborted() { return }` after each await, and the docs teach exactly that. (A
`Tick | Cancelled` enum return was considered and dropped — it taxes the common
non-occupant loop with a match that means nothing to it.)

### 9.2 The occupant tail is a mount mode, not descriptor content

Today the sequence-shaped tail — the implicit `SlotControlIn` cancel input and the
`SequenceStatus`/health/log outputs — is baked into every sequence's descriptor
(`src/sequence/mod.rs:343-358`), which is why v1 slots accept only sequences and sequences
fit only slots. The unification moves it to the **mount**: one shared

```rust
pub(crate) fn occupant_tail() -> (PortDesc /* SlotControlIn */, [PortDesc; 3] /* status, health, log */)
```

is consumed by all three parties that must agree on it: (a) the host's `add_slot`
contract derivation (`src/coordinator/mod.rs:1155-1167`, which today re-marks the
occupant's trailing `SlotControlIn` as `PortConn::Host` at `:1236-1246`); (b) the pack
driver's occupant-mode bind — order: user inputs → `SlotControlIn` → user outputs →
`SequenceStatus` → health → log, exactly today's `SeqSystem::build` order; (c) the
process worker. One function, three consumers, zero drift.

With the tail on the mount, **slots accept any entry**: a sync (`FnDriver`) occupant gets
**stop-on-cancel** — the driver reads the cancel input at the head of `step` and returns
`Done(Aborted)` without calling the fn — while cooperative cancel (`aborted()` between
awaits) stays the async style's richer option. `SeqSystem`, `SeqBound`, `SeqState`, and
`run_seq_*` are absorbed into the occupant-mode `FutureDriver` and deleted.

*Alternative rejected:* appending the tail to **every** system universally, so any system
is slot-loadable with no mount distinction. That costs two rings plus telemetry keys per
system, mission-wide, for a slot-only need — the wrong default for a framework whose
per-system footprint is otherwise exactly what the descriptor declares.

### 9.3 The dropped-publish fix

Closes the TODO at `src/sequence/mod.rs:326-332`. The blocker was cost: threading a shared
counter into every `Output` would tax every publish on every cyclic system for a
sequence-only need. The fix touches only the failure path:

```rust
enum Drops {
    Local(u64),                 // today's field, the common case
    Shared(Rc<Cell<u64>>),      // future-owned ports: clones of one per-driver cell
}
```

replacing `dropped: u64` on `Output`/`MsgOut` (`src/port.rs:95-97`). Only the publish
*failure* arm and `take_dropped` (`src/port.rs:125-127`) ever touch the enum, so the hot
path is unchanged. `FutureDriver`'s `TaskParam::bind` runs under a `BindCx` carrying the
shared cell, so every port that moves into the future reports into it; per step, a nonzero
sum folds into `health.error("publish_dropped")` — the same fold `CyclicRunner::step` does
(`src/system/mod.rs:380-382`). `Rc` suffices: pack entries are single-threaded by contract
(§5).

### 9.4 What happens to the macros

- `#[sequence]` deprecates to an fn-passthrough plus a deprecation note pointing at
  `Pack::task` — the async fn body is already the surviving artifact; only the wrapper
  generation dies.
- `#[system]` (`macros/src/system_attr.rs`) **survives unchanged** as struct-state sugar.
  Its generated trait impls are the lowering target for `Pack::system_type::<T>()`, so
  struct-authored systems ride into packs with no macro rewrite. The adcs example keeps
  `ctrl` authored this way on purpose (`examples/adcs-fsw2/systems/ctrl/src/lib.rs:33`),
  so both styles stay exercised end to end.

---

## 10. Risks

1. **HRTB/GAT inference on handler fns.** The double-bound pattern is Bevy-proven at far
   larger param counts than ours; free fns infer cleanly. Closures may need annotations
   (documented, §4.5); the escape hatch is an annotation-only attribute macro later, with
   the API unchanged.
2. **Positional-bind drift** is now the top correctness surface: three binders (host, raw,
   worker) against one order. Mitigation is structural — one tuple-macro walk generates
   decl and bind together (§4.1), one `occupant_tail()` feeds all three consumers (§9.2) —
   plus a counting-`RingSource` property test asserting decls per direction == rings
   consumed, for every entry shape.
3. **ABI soundness.** The three rules (`src/abi/mod.rs:24-37`) are retained verbatim. The
   new aliasing fact — the pack pointer is reachable from every entry state — is exactly
   why destroy-before-close is enforced by the `Arc<PackLib>` chain (§8.1) rather than by
   convention. `index` and `mount` words are validated on entry; the host keeps the
   `FswStatus::from_raw` discipline on every returned word.
4. **`'static` futures owning ports.** Unchanged contract from sequences — on the dl path
   the rings outlive the state by the teardown ordering; on the static path the futures
   own `RingBuffer` clones and are genuinely `'static`.
5. **WP3 is a flag day.** Everything in-repo rebuilds together (workspace, examples,
   fixtures in one train); anything stale fails closed on the version word.
6. **Perf.** `Box<dyn Driver>` replaces `Box<dyn CyclicSlot>` one-for-one; `ExecParam::get`
   inlines to field projections; the `Drops` enum touches only the failure path. The
   closed_loop example's timing (`examples/adcs-fsw2/tests/closed_loop.rs`) is the canary.

---

## 11. Resolved decisions

Each entry states the decision and keeps the trade-off prose so the rationale survives.

1. **ABI shape — DECIDED: index-parameterized fixed symbols, NOT a `repr(C)` fn-pointer
   table.** *Trade-off:* nine symbols instead of one open + a table, and every call
   carries the pack pointer — versus a table that crosses callable pointers by value,
   weakening the bytes-and-handles rule (`src/abi/mod.rs:24-37`) and trading dlsym-time
   validation for trust in decoded data. (§7.1.)

2. **Handler machinery — DECIDED: Bevy's `SystemParamFunction` pattern (marker generic +
   double `FnMut` bound), `macro_rules!` tuple impls, no proc macro.** *Trade-off:* a
   known inference wrinkle on closures and a bounded arity (0..=16) — versus a proc macro
   that could accept anything but hides the descriptor/bind contract in generated code,
   which is exactly where drift breeds. (§4.)

3. **Stateless fns — DECIDED: v1 requires a leading `&mut S`; stateless is
   `.init(|| ())`.** *Trade-off:* one line of ceremony for the truly stateless case —
   versus an overlapping impl family that coherence cannot disambiguate (§4.3). A distinct
   constructor can lift this later without breakage.

4. **Async params — DECIDED: by-value `TaskParam` + a `Params<P>` newtype; entry via
   distinct `Pack::task`.** *Trade-off:* the newtype is one more name to learn — versus
   `#[sequence]`'s "unrecognized type is params" rule, which is inexpressible in trait
   resolution and invisible at the call site. (§4.4.)

5. **Ring seam — DECIDED: `enum AnySource<'a> { Host(Binder), Raw(RawBinder) }`
   implementing `RingSource` by delegation.** *Trade-off:* a `match` per port pop — versus
   a dyn wrapper that erases the matched wake endpoints (`src/binder.rs:11-22`) and
   forecloses async entries on the static path. Fallback to the dyn form only if the enum
   fights the boxed `make`. (§5.)

6. **Occupant tail — DECIDED: a mount mode via one shared `occupant_tail()`, not
   descriptor content and not a universal tail.** *Trade-off:* the bind path forks on
   `Mount` — versus per-system ring/telemetry cost mission-wide (universal tail) or the
   status quo's sequences-only slots. (§9.2.)

7. **`.state(prebuilt)` — DECIDED: move-once, `reloadable: false`, spanned resolve-time
   rejection as occupant/second instance; no `S: Clone`.** *Trade-off:* prebuilt-state
   entries cannot be slot occupants — versus a Clone bound that silently forks shared
   resources. (§2.4.)

8. **`cycle().await` — DECIDED: resolves to `Timestamp`, separate `aborted()`; no
   auto-abort, no `Tick | Cancelled` enum.** *Trade-off:* occupant loops must write the
   abort check themselves (taught as the idiom) — versus taxing every non-occupant loop
   with a meaningless match arm. (§9.1.)

9. **Dropped-publish counter — DECIDED: `enum Drops { Local(u64), Shared(Rc<Cell<u64>>) }`
   on the failure path only.** *Trade-off:* an enum where a field was — versus the TODO's
   alternative of taxing every publish on every system (`src/sequence/mod.rs:326-332`),
   which is why it sat unfixed. (§9.3.)

10. **Legacy surface — DECIDED: delete the 7-symbol ABI, `export_system!`,
    `#[system(export)]`'s export args, and `run_seq_*` in the same phase as v5.**
    *Trade-off:* a flag day — versus two live loader paths and two trust surfaces during a
    deprecation window nobody external needs (workspace is `publish = false`). (§7.4.)

11. **`#[system]` / `#[sequence]` — DECIDED: `#[system]` survives as struct-state sugar
    (`Pack::system_type` lowers it); `#[sequence]` deprecates to a passthrough.**
    *Trade-off:* two authoring styles live on — deliberately, since struct-state systems
    with many ports read better than a wide fn signature, and the example suite keeps one
    of each to prove parity. (§9.4.)

12. **Process workers — DECIDED: packs supported (`Run` gains the entry name); pack state
    is never shared across processes.** *Trade-off:* a mission that wants sharing must
    keep those entries in-process — documented rather than papered over with IPC that the
    "shared owned resource" future work owns. (§8.3.)
