# `#[system]` — killing per-system authoring ceremony (E2, E3, E6, E7, E8a/c)

Status: **design** (2026-07-02). Companion to `docs/review-findings.md` §4 and the
in-flight `docs/design-port-unification.md` (§9 states exactly which emitted pieces
track that outcome). This doc specifies an attribute macro, the small framework API
changes it surfaces, the macro-crate move, and the landing order. No code changes
ride this doc.

---

## 1. Problem

A minimal cyclic system today is ~50 lines of which ~35 are derivable ceremony
(`examples/adcs-fsw2/systems/nav/src/lib.rs:33-114`):

| Ceremony | Lines | Derivable from |
|---|---|---|
| `#[derive(SystemInput)] struct NavIn<B: Backing = BoxBacking> { … }` | 5 | the ports `execute` uses |
| `#[derive(SystemOutput)] struct NavOut<B: Backing = BoxBacking> { … }` | 4 | same |
| `impl<B: Backing> System<B> for NavSystem { type Input; type Output = Out<…>; NAME }` | 5 | struct ident + ports |
| `impl<B: Backing> CyclicSystem<B> for NavSystem { fn execute … }` | 2 (header) | the fn itself |
| `impl BuildSystem for NavSystem { type Params; fn new }` (pure delegation) | 6 | the inherent `new` |
| `#[cfg(feature = "export")] metor_fsw_2::export_system!(NavSystem);` | 2 | one flag |
| `#![allow(clippy::not_unsafe_ptr_arg_deref)]` | 1 | the export macro should own it |

`#[sequence]` (`libs/metor-fsw/macros/src/sequence.rs`) already proves the model:
read the ports off an fn signature, in signature order, and emit
descriptor + bind + ABI from that one source of truth. `#[system]` applies the same
model to stateful systems.

---

## 2. Placement: on the inherent `impl` block

**Decision: `#[system]` annotates the system's inherent `impl` block**, and reads
ports off the `execute` (cyclic) / `run` (async) method signature.

Why not the alternatives:

- **On the struct**: the struct declares *state*, not ports. Ports would have to be
  spelled in attribute args (stringly, unchecked, and duplicated against how
  `execute` actually uses them) or in helper-attributed fields (conflating state
  with ports — a system's state fields are not ports).
- **On a free `execute` fn** (the literal `#[sequence]` shape): systems are
  stateful and constructed from params; a free fn has no `&mut self` and no `new`.
  Forcing state into a context struct parameter changes the programming model for
  no gain.
- **On the impl block**, one annotation sees everything it must derive from:
  `execute`/`run` (→ ports, cyclic-vs-async), `new` (→ `BuildSystem::Params`),
  optional `init`/`shutdown`, and the type ident (→ `NAME`, bundle idents). This is
  the only placement that collapses *all four* generated impls plus the export into
  one annotation.

`#[sequence]` stays fn-shaped (sequences genuinely are stateless fns); the two
macros share the signature→port classification code.

---

## 3. Authored form

### 3.1 Minimal system (the ~10-line target)

```rust
use metor_fsw_2::{system, Input, Output, Timestamp};

#[derive(Default)]
pub struct Echo;

#[system]
impl Echo {
    fn execute(&mut self, now: Timestamp, ping: &mut Input<Ping>, pong: &mut Output<Pong>) {
        if let Some(p) = ping.latest() {
            pong.publish(&Pong { timestamp: now, seq: p.get().seq });
        }
    }
}
```

10 lines including blanks. No `B: Backing`, no bundles, no `Out<>`, no `Result`
noise, no `BuildSystem`, no clippy allow. (`Timestamp` gets a root re-export from
`metor_fsw_2` as part of this work; today it is only reachable as
`metor_fsw_2::metor_proto::types::Timestamp`.)

### 3.2 nav — before / after

Before: `examples/adcs-fsw2/systems/nav/src/lib.rs` (114 lines, ~50 non-doc).
After (~60 lines total, ~28 non-doc — the body is unchanged except E3/E6 call
forms):

```rust
use adcs_contracts::{AttitudeEstimate, DT, Gps, MagneticModel, NavParams, Sensors, V3,
    epoch_at, mag_field_eci, sun_dir_eci};
use metor_fsw_2::{system, Input, Output, Timestamp};
use nox::tensor;

pub struct NavSystem {
    state: metor_fsw_adcs::mekf::State,
    sigma: f64,
    mag_model: MagneticModel,
    t_sim: f64,
}

#[system(name = "nav", export = "export")]
impl NavSystem {
    pub fn new(p: NavParams) -> Self {
        let state = metor_fsw_adcs::mekf::State::new(
            tensor![0.01, 0.01, 0.01], tensor![0.01, 0.01, 0.01], DT);
        Self { state, sigma: p.meas_sigma, mag_model: MagneticModel::default(), t_sim: 0.0 }
    }

    fn execute(
        &mut self,
        now: Timestamp,
        sensors: &mut Input<Sensors>,
        gps: &mut Input<Gps>,
        estimate: &mut Output<AttitudeEstimate>,
    ) {
        let epoch = epoch_at(self.t_sim);
        self.t_sim += DT;

        let Some(s) = sensors.latest() else { return };          // E3: no Result
        let s = s.get().clone();
        let Some(g) = gps.latest() else { return };
        let gps_pos: V3 = g.get().pos_eci;

        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut self.mag_model, epoch, &gps_pos).normalize();

        self.state.omega = s.gyro_b;
        self.state = self.state.clone().estimate_attitude(
            [s.sun_b, s.mag_b], [sun_eci, mag_eci], [self.sigma, self.sigma]);
        self.state.reset_if_invalid();

        estimate.publish(&AttitudeEstimate {                      // E6: infallible
            timestamp: now,
            q_hat_b_eci: self.state.q_hat,
            omega_b: s.gyro_b,
            b_hat_b: self.state.b_hat,
        });
    }
}
```

Gone versus today: `NavIn`/`NavOut` + their `B: Backing = BoxBacking` threading,
`impl System`, `impl CyclicSystem` header, `impl BuildSystem`, the `#[cfg]`'d
`export_system!`, the crate-level clippy allow, the `Out<>` wrapper (E8a), the
`use metor_fsw_2::ring::{Backing, BoxBacking}` import, and every
`let Ok(Some(_))` / `let _ = o.x.write(…)` (E3/E6).

Unchanged Cargo ceremony (a macro cannot remove it): `crate-type =
["cdylib", "rlib"]` and the `export` feature declaration. That stays a documented
3-line recipe.

### 3.3 Async system

```rust
#[system(name = "radio")]
impl Radio {
    async fn run(&mut self, cmds: &mut MsgIn<GroundCmd>, tm: &mut Output<RadioTm>) {
        loop {
            match cmds.recv().await { … }   // Result forms stay on async paths (E3)
        }
    }
}
```

`fn execute` ⇒ `CyclicSystem`; `async fn run` ⇒ `AsyncSystem`. Exactly one of the
two per annotated impl.

### 3.4 Optional pieces

```rust
#[system(name = "nav", export = "export")]
impl NavSystem {
    fn new(p: NavParams) -> Self { … }                 // optional: absent ⇒ Self: Default, Params = ()
    fn init(&mut self, health: &mut HealthPort) { … }  // optional; output ports + health only
    fn execute(&mut self,
        now: Timestamp,
        sensors: &mut Input<Sensors>,                  // frame input
        cmds: &mut MsgIn<ModeCmd>,                     // message input
        estimate: &mut Output<AttitudeEstimate>,       // frame output
        events: &mut MsgOut<NavEvent>,                 // message output
        health: &mut HealthPort,                       // optional, recognized by type
    ) { … }
    fn shutdown(&mut self) { … }                       // optional
    fn helper(&self) -> f64 { … }                      // any other method: passed through verbatim
}
```

### 3.5 commissioning (`#[sequence]`) — before / after (E7)

Before (`examples/adcs-fsw2/systems/commissioning/src/lib.rs`):

```rust
#![allow(clippy::not_unsafe_ptr_arg_deref)]
use metor_fsw_2::ring::Backing;

#[metor_fsw_2::sequence]
async fn commissioning<B: Backing>(
    mut att: Input<AttitudeEstimate, B>,
    mut mode: Output<ModeCmd, B>,
) -> Outcome {
    progress("warming up");
    if wait(Duration::from_millis(100)).await.aborted() {
        mode.write(&ModeCmd::safe()).ok();
        return Outcome::Aborted;
    }
    …
}
```

After:

```rust
use metor_fsw_2::sequence::{now, progress, wait};

#[metor_fsw_2::sequence]
async fn commissioning(mut att: Input<AttitudeEstimate>, mut mode: Output<ModeCmd>) -> Outcome {
    progress("warming up");
    if wait(Duration::from_millis(100)).await.aborted() {
        mode.publish(&ModeCmd::safe_at(now()));   // E6 publish + E7 now()
        return Outcome::Aborted;
    }
    …
}
```

Three E7/adjacent changes:
1. **`<B: Backing>` injected**: if the fn declares no `Backing`-bounded generic,
   the macro adds `__B: Backing` and rewrites each port *parameter* type
   `Input<T>` → `Input<T, __B>` (same for `Output`). A hand-written
   `<B: Backing>` is still accepted (the macro uses it instead) — no migration
   flag day.
2. **`now()`**: `SeqClock` already carries `now: Cell<Timestamp>`
   (`src/sequence/mod.rs:48`); add the free fn and the handle method beside
   `wait`/`progress`/`aborted`:
   ```rust
   pub fn now() -> Timestamp {
       current().expect("now() called outside a sequence poll").now.get()
   }
   impl Seq { pub fn now(&self) -> Timestamp { self.clock.now.get() } }
   ```
   Sequences can finally stamp the frames they emit (review E7,
   `adcs_contracts::ModeCmd` grows a stamped constructor or the body stamps
   directly).
3. **Clippy allow emitted by the macro**: both `#[sequence]` and `#[system]` put
   `#[allow(clippy::not_unsafe_ptr_arg_deref)]` on each generated `extern "C" fn`,
   deleting the crate-level allow from every system/sequence crate.

---

## 4. Attribute grammar (E8c)

```
#[system]                                 // name = snake_case ident (– "System" suffix), no ABI exports
#[system(name = "nav")]                   // explicit wiring name
#[system(export)]                         // emit fsw_* exports, gated #[cfg(not(test))] only
#[system(export = "export")]              // gated #[cfg(all(feature = "export", not(test)))]
```

- **`name`** — the `System::NAME` wiring name. Default: `snake_case` of the self
  type ident with a trailing `System` stripped (`NavSystem` → `"nav"`,
  `CtrlSystem` → `"ctrl"`). Explicit `name =` always wins; the doc examples always
  write it.
- **`export`** — absent: no C-ABI exports (static-link-only crate; nothing to
  gate, no `unexpected_cfgs` warnings). Bare: exports gated on `not(test)` (the
  `#[sequence]` precedent — pure-cdylib crates). `export = "<feature>"`: exports
  additionally gated on that cargo feature (the dual `cdylib + rlib` recipe nav
  uses today so the rlib the parity test links carries no `fsw_*` symbols).

**Namespace decision**: `#[system(...)]` takes ordinary attribute-macro arguments —
**not** `#[metor_fsw(...)]` helper attributes. Helper attrs are a *derive*
mechanism; an attribute macro owns its item and parses its own args, and this
design reads everything else off signatures, so `#[system]` needs no helper attrs
at all. The `metor_fsw`-vs-`metor_fsw_2` wart therefore only lives in the derives
(`#[derive(Frame)]`'s `#[metor_fsw(timestamp)]` etc.). Recommendation: when the
derives move to the new macro crate (§8), re-register their helper namespace as
**`#[fsw(...)]`** — short and version-free, so it survives any future
`metor-fsw-2` → `metor-fsw` rename — while still accepting `metor_fsw` during
migration (darling: `#[darling(attributes(fsw, metor_fsw))]`). The examples and
docs switch to `#[fsw(timestamp)]`.

---

## 5. Signature rules

Recognized methods in the annotated impl (all others pass through verbatim,
including doc comments and visibility):

| Method | Requirement | Drives |
|---|---|---|
| `fn execute(&mut self, …)` | sync; exactly one of execute/run | `CyclicSystem` |
| `async fn run(&mut self, …)` | async; exactly one of execute/run | `AsyncSystem` |
| `fn new(p: P) -> Self` or `fn new() -> Self` | optional; ≤1 param | `BuildSystem` (`Params = P` / `()`) |
| `fn init(&mut self, …)` | optional; params ⊆ {output ports by ident, `&mut HealthPort`} | `System::init` |
| `fn shutdown(&mut self, …)` | optional; same rule as init | `System::shutdown` |

Parameter classification for `execute`/`run`, by the **last path segment** of the
type (the `#[sequence]` technique, `sequence.rs:51`):

| Param form | Classified as |
|---|---|
| `now: Timestamp` (type-head `Timestamp`) | the cycle timestamp — **required** for `execute`, **rejected** on `run` (async systems have no coordinator `now`) |
| `x: &mut Input<T>` | frame input port |
| `x: &mut MsgIn<M>` | message input port |
| `x: &mut Output<T>` | frame output port |
| `x: &mut MsgOut<M>` / `&mut CommandOut<M>` | message output port |
| `x: &mut HealthPort` | the health handle (optional, at most one) |
| anything else | compile error (§10) |

Rules:

- Ports must be **`&mut` borrows**, not by-value. Cyclic/async ports are owned by
  the runner across cycles (scratch buffers, ring cursors) and lent per call. This
  intentionally differs from `#[sequence]`, where ports are **moved into** the
  future and are by-value — the error message for a by-value port in `#[system]`
  says exactly that (§10).
- **Descriptor order = signature order** within each kind: the generated input
  bundle lists input-classified params in signature order, the output bundle lists
  output-classified params in signature order. This is the positional order the
  binder walks and the KDL front-end validates against — same contract as the
  hand-written bundles today.
- Explicit backings are rejected: `&mut Input<T, RawBacking>` is an error. The
  macro owns `B` (it rewrites every port param to `PortType<T, __B>`); a system
  that genuinely needs backing-specific code writes the traits by hand (the
  escape hatch stays public and documented).
- Generic impl blocks (`impl<T> Foo<T>`) are rejected in v1 (no known use; the
  descriptor is per-concrete-type).

---

## 6. Expansion spec (nav)

Input:

```rust
#[system(name = "nav", export = "export")]
impl NavSystem {
    pub fn new(p: NavParams) -> Self { /* A */ }
    fn execute(&mut self, now: Timestamp, sensors: &mut Input<Sensors>,
               gps: &mut Input<Gps>, estimate: &mut Output<AttitudeEstimate>) { /* B */ }
}
```

Expansion (all paths `::metor_fsw_2::…` via `proc-macro-crate` resolution, as
today; abbreviated here):

```rust
// 1 ── The inherent impl, re-emitted. Unrecognized methods + `new` verbatim
//      (so `NavSystem::new` stays callable in tests). `execute` is re-emitted as a
//      hidden generic method: the macro injects `__B: Backing` and rewrites the
//      port parameter types; the BODY tokens are untouched (spans preserved).
impl NavSystem {
    pub fn new(p: NavParams) -> Self { /* A, verbatim */ }

    #[doc(hidden)]
    fn __fsw_execute<__B: metor_fsw_2::ring::Backing>(
        &mut self,
        now: metor_fsw_2::Timestamp,
        sensors: &mut metor_fsw_2::Input<Sensors, __B>,
        gps: &mut metor_fsw_2::Input<Gps, __B>,
        estimate: &mut metor_fsw_2::Output<AttitudeEstimate, __B>,
    ) { /* B, verbatim */ }
}

// 2 ── Hidden port bundles. CRUCIALLY these carry the EXISTING derives, so the
//      macro emits *no* descriptor/bind knowledge of its own — that stays in
//      #[derive(SystemInput)]/#[derive(SystemOutput)] (§9: the port-unification
//      insulation point). Field names = the execute param idents; field order =
//      signature order.
#[derive(metor_fsw_2::SystemInput)]
#[doc(hidden)]
pub struct __NavSystemIn<__B: metor_fsw_2::ring::Backing = metor_fsw_2::ring::BoxBacking> {
    pub sensors: metor_fsw_2::Input<Sensors, __B>,
    pub gps: metor_fsw_2::Input<Gps, __B>,
}

#[derive(metor_fsw_2::SystemOutput)]
#[doc(hidden)]
pub struct __NavSystemOut<__B: metor_fsw_2::ring::Backing = metor_fsw_2::ring::BoxBacking> {
    pub estimate: metor_fsw_2::Output<AttitudeEstimate, __B>,
}

// 3 ── System + the leaf trait. `Out<>` appears HERE and only here (E8a): the
//      user never names it. If port unification changes Self::Output's shape,
//      only these generated lines move.
impl<__B: metor_fsw_2::ring::Backing> metor_fsw_2::System<__B> for NavSystem {
    type Input = __NavSystemIn<__B>;
    type Output = metor_fsw_2::Out<__NavSystemOut<__B>, __B>;
    const NAME: &'static str = "nav";
}

impl<__B: metor_fsw_2::ring::Backing> metor_fsw_2::CyclicSystem<__B> for NavSystem {
    fn execute(&mut self, now: metor_fsw_2::Timestamp,
               input: &mut Self::Input, output: &mut Self::Output) {
        // `Out::split` (new, §7.3) yields (&mut __NavSystemOut, &mut HealthPort)
        // so a `health: &mut HealthPort` param and output ports can coexist.
        let (__ports, __health) = output.split();
        let _ = __health; // only passed if the signature asked for it
        self.__fsw_execute::<__B>(now, &mut input.sensors, &mut input.gps, &mut __ports.estimate)
    }
}

// 4 ── Construction. `new(p: P)` present ⇒ delegate; `new()` present ⇒ Params = ();
//      absent ⇒ `fn new(_: ()) -> Self { <Self as Default>::default() }`.
impl metor_fsw_2::BuildSystem for NavSystem {
    type Params = NavParams;
    fn new(params: NavParams) -> Self { NavSystem::new(params) }
}

// 5 ── The dl ABI, iff `export` was given. Byte-for-byte what export_system!
//      emits today (delegating one-liners into abi::run_*), except each item
//      additionally carries the cfg gate and the clippy allow:
#[cfg(all(feature = "export", not(test)))]
#[unsafe(no_mangle)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn fsw_abi_version() -> u32 { metor_fsw_2::abi::FSW_ABI_VERSION }
/* … fsw_describe / fsw_create / fsw_bind_init / fsw_execute / fsw_shutdown /
     fsw_destroy, exactly as libs/metor-fsw/macros/src/export.rs … */
```

Notes:

- **`init`/`shutdown` mapping**: a user `fn init(&mut self, health: &mut HealthPort)`
  becomes a hidden `__fsw_init<__B>` and `System::init` delegates through
  `output.split()`. Output-port params are matched to execute's output params **by
  ident** (an init naming a port execute doesn't declare is an error).
- **Async expansion** differs only in item 3 (`AsyncSystem`, `fn run` delegation,
  no `now`, no `split` timing) — the bundles/BuildSystem/exports are identical.
  `SystemKind::Async` flows from `AsyncSystem::descriptor()` as today.
- **Static-registry path unchanged**: `register_system!(reg, NavSystem => "Nav")`
  keeps working — the generated impls satisfy `AddToBuilder<CyclicKind>`'s
  `S: CyclicSystem<Output = Out<O>>` bound with `O = __NavSystemOut<BoxBacking>`
  inferred (`src/wiring/mod.rs:599-611`); `NavParams: FromKdlNode` remains the
  contracts crate's business.
- **What the macro does *not* generate**: ring sizing, wiring, KDL — all
  downstream of `descriptor()`, which flows from the derives as today.

---

## 7. The API-shape changes the macro surfaces

These are framework changes, not macro tricks — hand-written systems get them too.
The macro's authored form is simply designed around them.

### 7.1 E3 — cyclic `latest() -> Option<FrameRef>`

**Lives on the port type** (`src/port.rs`), not a macro-generated wrapper:

```rust
impl<F: …> Input<F, B, RD, RS> {
    /// Drain to the newest record. On a lap: resync, latch the lap internally
    /// (surfaced by `is_lapped()`, telemetered by the runner), keep going.
    pub fn latest(&mut self) -> Option<FrameRef<'_, F>> { … }

    /// Result forms stay for async / lossless callers:
    pub async fn recv(&mut self) -> Result<FrameRef<'_, F>, ReadError> { … }  // unchanged
    pub fn drain(&mut self, f: …) -> Result<(), ReadError> { … }              // unchanged
}
```

Rationale for port-type placement: (a) the coordinator already hard-stops a cyclic
system on lap *before* `execute` (`CyclicRunner::step`,
`src/system/mod.rs:289-307`), so inside `execute` the `Err` arm is unreachable for
same-thread producers and merely races an async producer's lap — in which case the
right cyclic policy is "resync and report", which the port can do itself since the
view already latches `is_lapped`; (b) a wrapper type would put the two flavors on
two nominal types and force the macro into the read path — churn §9 wants to avoid.
Lap telemetry: `CyclicRunner::step` gains a **post**-execute `any_lapped()` check
mirroring the pre-execute one, so a mid-execute lap is charged to health the same
cycle. No signature change anywhere else.

### 7.2 E6 — infallible `publish()` for cyclic outputs

```rust
impl<F: …> Output<F, B, WD, WS> {
    /// Publish; on failure (only `InsufficientCapacity` on the framework's
    /// Overwrite rings — a sizing bug) count the drop for the runner to telemeter.
    pub fn publish(&mut self, frame: &F) { if self.writer.try_write(…).is_err() { self.dropped += 1; } }
    pub fn publish_with(&mut self, fixed: &F, build: …) { … }   // write_with twin

    pub fn write(&mut self, frame: &F) -> Result<(), WriteError> { … }        // stays (sizing-aware callers)
    pub async fn write_async(&mut self, frame: &F) -> Result<(), WriteError>  // stays (async/lossless)
}
```

Failure routing: the port cannot see the health port, so it **counts**; the frame
that telemeteres it is the runner's. `SystemOutput` gains one derived method —

```rust
pub trait SystemOutput {
    fn descriptors() -> Vec<PortDesc>;
    /// Sum-and-clear the ports' publish-drop counters (derive-generated).
    fn take_dropped(&mut self) -> u64 { 0 }
}
```

— and `CyclicRunner::step` (plus the seq/dl execute paths) folds a nonzero count
into `health.error("publish_dropped")` before `end_cycle`. `MsgOut` gets the same
`publish`/counter; `CommandOut` inherits via Deref. Default method body keeps
existing hand-written bundles compiling.

### 7.3 E8a — `Out<>` off the user surface

With `#[system]`, `Out` appears only in generated code (§6 item 3). Two framework
additions make that clean:

- **`Out::split(&mut self) -> (&mut O, &mut HealthPort<B, WD, WS>)`** — disjoint
  borrows of the user ports and the health pair, so generated delegation can pass
  both to the user fn. Today `health()` borrows the whole `Out`, which would
  conflict with `&mut ports.estimate`.
- Health in the authored form is an **opt-in `&mut HealthPort` parameter** —
  systems that never report domain errors (nav, ctrl, plant today) don't see it at
  all; the standard counters are runner-maintained regardless.

The deeper "runner-owned health pair" option (delete `Out`, move
`HealthPort` into `CyclicRunner`/the abi state) is **deliberately deferred to the
port-unification design** — it changes `System::Output`'s meaning and
`Out::descriptors()`'s implicit tail, which is exactly the descriptor surface that
design owns. `#[system]` is agnostic: only §6 item 3 and the two `split` call
sites change if it lands (user code: zero churn). This is the point of doing the
macro first.

### 7.4 E7 — sequences: `now()` + injected `<B: Backing>`

Specified in §3.5. `SeqClock` already holds `now`; the additions are the free fn +
`Seq::now()`. The `#[sequence]` injection reuses `#[system]`'s port-param rewriter.
`#[sequence]` also adopts the per-item clippy allow and the `publish()` form.

---

## 8. Macro-crate plan

**Decision: create `libs/metor-fsw-2/macros` → crate `metor-fsw-2-macros`, and
move every fsw-2-targeting macro into it.** The old framework's crate keeps only
old-framework macros.

Why move:

- Today `metor-fsw/macros` (crate `metor-fsw-macros`) hosts *both* frameworks'
  macros, resolving `metor-fsw-2` by `proc-macro-crate` string lookup
  (`lib.rs:157-166`) with a silent `Err(_) => quote!(metor_fsw_2)` fallback. The
  new framework's fastest-moving surface (this macro) should not live in, version
  with, or `cargo publish` with the old framework.
- The repo memory about helper-attr resolution needing a separate crate was a
  facet-specific mechanism (module-namespaced attr grammars) and is **not** a
  blocker here — darling helper attrs resolve fine from any proc-macro crate. The
  move is motivated by ownership, not resolution.

What moves (all already emit `::metor_fsw_2::…` paths): `frame.rs` (+ the
`Frame`-bundled `as_vtable`/`componentize`/`decomponentize`/`metadatatize` *emit
paths* it drives), `system.rs` (`SystemInput`/`SystemOutput`), `sequence.rs`,
`export.rs` (`export_system!`, retained as the hand-written escape hatch),
`from_kdl.rs`, the shared `Field` struct, and the new `system_attr.rs` +
`sig.rs` (shared signature/port classification for `#[system]`/`#[sequence]`).

What stays in `metor-fsw-macros`: the old framework's own
`AsVTable`/`Componentize`/`Decomponentize`/`Metadatatize` derives (old `metor-fsw`
re-exports them, `metor-fsw/src/lib.rs:3`). The small helper duplication between
the two proc-macro crates (`Field`, crate-name resolution) is accepted — proc-macro
crates can't share non-macro items without a third crate, and the old framework is
frozen; a `metor-fsw-macros-shared` lib crate is not worth its existence for ~100
duplicated lines.

Migration (workspace is `publish = false`, so a clean cut):

1. New crate `libs/metor-fsw-2/macros`, `[lib] proc-macro = true`, deps: `syn`,
   `quote`, `proc-macro2`, `darling`, `proc-macro-crate`, `convert_case`. Move the
   files; `metor_fsw_2_crate_name()` comes along (fallback removed — hard error if
   `metor-fsw-2` is absent from the consumer's `Cargo.toml`).
2. `metor-fsw-2/Cargo.toml`: replace `metor-fsw-macros` dep with
   `metor-fsw-2-macros`; `src/lib.rs` re-exports switch
   (`pub use metor_fsw_2_macros::{Frame, SystemInput, SystemOutput, FromKdlNode,
   export_system, sequence, system};`). **User-facing paths are unchanged** —
   everything was already consumed via `metor_fsw_2::…`.
3. Delete the moved modules from `metor-fsw-macros`; drop old `metor-fsw`'s
   stray `Frame` re-export (it emits fsw-2 paths and is a trap).
4. Helper-attr namespace on the moved derives becomes `attributes(fsw, metor_fsw)`
   (§4); examples move to `#[fsw(…)]`; the `metor_fsw` spelling is dropped after
   the examples/docs sweep.

---

## 9. Interaction with port unification + phased landing

`docs/design-port-unification.md` (in parallel design) may merge the
frame/message twin stacks (A1), change `PortDesc`, and/or relocate the health pair
(A5/A6/E8a). The macro is designed so **user code is insulated**: the authored
form names only port types + element types; everything trait-shaped is emitted.

Emitted pieces and their unification exposure:

| Emitted piece | Depends on unification? |
|---|---|
| Hidden bundle structs | **No** — they carry `#[derive(SystemInput/SystemOutput)]`; descriptor/bind churn lands inside the derives, macro output is byte-stable |
| Port classification table (§5) | **Renames only** — if `Input`/`MsgIn` unify into one generic port type, the table shrinks; one match arm per accepted head |
| `type Output = Out<…>` + `split` delegation | **Yes** — the one line that moves if the health pair becomes runner-owned or `Out` dies |
| `BuildSystem` impl, `NAME`, exports (`abi::run_*`) | **No** — the dl ABI mirror is versioned by `FSW_ABI_VERSION`, orthogonal |
| E3/E6 port methods | **Carried over** — whatever the unified port types are, `latest()->Option` / `publish()` are requirements on them |

**Landing order: macro first, unification second.** Rationale: converting the
examples to `#[system]` *before* unification means the examples are rewritten
once, and unification's churn is then absorbed entirely inside the derives + two
generated lines — no example/mission-code diff at all. Landing the macro after
would force every example through two hand rewrites. The only coordination
needed: the unification design should treat §5's accepted type-head names and the
E3/E6 method contracts as inputs.

Phases:

- **P0 — API fixes (framework only)**: E3 `latest()`, E6 `publish()` +
  `SystemOutput::take_dropped` + runner folding, `Out::split`, E7 `now()`,
  root `Timestamp` re-export. Update examples mechanically
  (`let Ok(Some(x))` → `let Some(x)`, `let _ = w.write` → `w.publish`). Each is
  an independent commit.
- **P1 — macro-crate move** (§8, pure logistics, no behavior).
- **P2 — `#[system]`** in the new crate + `#[sequence]` B-injection/`now()`
  adoption + per-item clippy allows. Rewrite nav/ctrl/plant/commissioning/
  safe-mode; delete their ceremony; keep `closed_loop.rs` (static parity) and the
  dl integration tests green as acceptance.
- **P3 — port unification lands**: derives' internals + the §9 table rows marked
  "yes" update; no example churn.

---

## 10. Error-message design

All errors are `syn::Error::new_spanned` on the *narrowest* offending token, with
a "write this instead" tail. The macro never panics; multiple independent errors
are combined so a wrong signature reports everything at once.

| Condition | Span | Message |
|---|---|---|
| no `execute`/`run` | impl ident | `#[system] needs a `fn execute(&mut self, now: Timestamp, …ports)` (cyclic) or an `async fn run(&mut self, …ports)` (async) in this impl` |
| both present | second fn ident | `a system is cyclic or async, not both: remove `execute` or `run`` |
| `async fn execute` | `async` kw | `execute` is called synchronously once per cycle; for a self-driven loop write `async fn run`` |
| `fn run` (sync) | fn ident | ``run` must be `async` (a cyclic system's per-cycle entry point is `fn execute`)` |
| missing/by-value receiver | receiver (or fn ident) | ``execute` takes `&mut self` (the system state lives in the struct)` |
| missing `now: Timestamp` on execute | fn ident | `cyclic `execute` needs the cycle timestamp: add `now: Timestamp` (systems stamp outputs with the coordinator's `now`, not wall time)` |
| `now` on `run` | param | `async systems have no coordinator `now`; remove this parameter` |
| unrecognized param type | the type | `expected a port (`&mut Input<T>`, `&mut Output<T>`, `&mut MsgIn<T>`, `&mut MsgOut<T>`), `&mut HealthPort`, or `now: Timestamp`; found `Foo` — non-port state belongs in fields of the system struct` |
| by-value port (`sensors: Input<Sensors>`) | the type | `system ports are owned by the runner and lent per cycle: write `sensors: &mut Input<Sensors>` (only #[sequence] ports are moved by value)` |
| explicit backing (`Input<T, RawBacking>`) | 2nd type arg | `#[system] supplies the ring backing itself; drop the second type parameter` |
| `Input<>` missing element | the type | ``Input` needs an element type: `Input<MyFrame>`` (as `sequence.rs` today) |
| two `HealthPort` params | second one | `at most one `&mut HealthPort` parameter` |
| `new` with >1 param / non-`Self` return | offending token | ``new` must be `fn new(params: P) -> Self` or `fn new() -> Self`` |
| no `new` and no `Default` | — | surfaced as the natural `Self: Default` bound error **on the impl-ident span** via a `where Self: Default` in the generated shim, plus a `/// #[system]: no `fn new` found, requiring `Default`` doc note in the expansion |
| `init`/`shutdown` param not an execute output/health | param ident | ``init` may only take execute's output ports (by name) and `&mut HealthPort`; `sensors` is an input` |
| generic impl block | generics | `#[system] does not support generic impls; implement `System`/`CyclicSystem` by hand for this type` |
| unknown attribute arg | the arg | `unknown #[system] argument `foo`; expected `name = "…"`, `export`, `export = "feature"`` |
| duplicate port ident between init and execute mismatch types | init param | ``estimate` has type `Output<A>` in `execute` but `Output<B>` here` |

`#[sequence]` inherits the shared classifier, so its messages upgrade for free
(today e.g. a by-ref port there produces an opaque downstream type error).

---

## 11. Test plan

New crate `metor-fsw-2-macros` gets the test weight; `metor-fsw-2` keeps
integration-level parity tests.

1. **trybuild UI tests** (`metor-fsw-2/tests/ui/`, driven from a
   `metor-fsw-2` dev-dep on `trybuild` — the *consumer* crate hosts them so the
   generated code resolves `::metor_fsw_2` naturally). One `.rs` + `.stderr` per
   row of §10's table, plus a `pass/` set: minimal cyclic, async, msg ports,
   health param, no-`new`-Default, `export = "feature"`. trybuild is new to the
   workspace (verified: no current usage); pin expectations to the workspace
   toolchain and treat `.stderr` regeneration (`TRYBUILD=overwrite`) as part of
   toolchain bumps.
2. **Expansion parity tests** (preferred over token-snapshot tests, which churn
   on every formatting tweak — same reasoning as the existing in-crate
   `#[sequence]` tests): in `metor-fsw-2`, define the same system twice —
   hand-written trait impls vs `#[system]` — and assert
   `descriptor()` equality (name, kind, port order, ids) and behavioral equality
   under a 3-cycle mini coordinator (same outputs, same health counters,
   including the E6 dropped-count fold and the E3 lap-latch path with an
   undersized ring).
3. **dl round-trip**: convert one existing dl-integration fixture cdylib to
   `#[system(export)]` and run the existing `dl_integration` test unchanged —
   proves the emitted ABI is byte-compatible with `export_system!`'s.
4. **Example conversion as acceptance**: nav/ctrl/plant/commissioning/safe-mode
   rewritten; `closed_loop.rs` (static-registry parity) and the KDL/dlopen
   mission test pass with **zero mission-file changes** (descriptor order rule,
   §5, guarantees this as long as signature order matches today's field order —
   the conversion keeps it).
5. **P0 unit tests** (framework, macro-independent): `latest()` lap-resync
   semantics, `publish` drop counting + `take_dropped` fold, `Out::split`
   disjointness (compile test), `sequence::now()` against a stepped `SeqClock`,
   `#[sequence]` with and without a hand-written `<B: Backing>`.

---

## 12. Open questions (need a human call)

1. **Default `name`**: is stripping the `System` suffix (`NavSystem` → `"nav"`)
   acceptable magic, or should `name = "…"` be mandatory? (Doc recommends the
   strip + always writing it in examples.)
2. **Landing order**: confirm "macro before port unification" (§9) with the
   parallel design's author — it constrains that design to keep the §5 type-head
   names or hand this doc the renames.
3. **`take_dropped` on `SystemOutput`** (E6): approve the one-method trait
   growth, or prefer silent drops until the health pair becomes runner-owned?
4. **Helper-attr rename** to `#[fsw(...)]` when the derives move (§4/§8) — approve
   the namespace and the temporary dual-accept?
5. **Old `metor-fsw`'s `Frame` re-export**: confirm deleting it (it generates
   fsw-2 paths; any old-framework user of it is already broken-by-luck).
6. **`export` default**: confirmed no-exports-by-default (static-link crates stay
   warning-free) — or should bare `#[system]` gate on `feature = "export"` and
   accept the `unexpected_cfgs` lint in feature-less crates?
