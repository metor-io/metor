# Implementation plan — slots & sequences

> **Since superseded:** the "commands flow as frames" plane below was replaced by a
> `SequenceCommand` **message** channel (`docs/messages.md` §4), then reframed again to
> name-addressed, explicitly-edged per-slot fan-in (`docs/design-command-slots.md`,
> `docs/review-fixes-plan.md` waves 4a-4c). The `#[sequence]` authoring surface also gained E7
> ergonomics (injected `Backing`, free `now()`) after this plan landed — see
> `docs/sequences-slots.md` §4.1 and `docs/design-system-macro.md`. Kept as the W1-W7
> implementation history.

Design: `docs/sequences-slots.md` (approved, decisions locked). Summary of what we build: a
runtime-loadable **slot** (a swappable `CyclicSlot` position with a fixed port contract,
pre-allocated rings, and an allowed occupant set) plus a `#[sequence]` decorator that turns
an `async fn(ports…) -> Outcome` into a future-driven dl occupant the slot polls once per
cycle. Commands flow as frames; `Stop` is hard-drop; one new ABI status (`Done`) and an ABI
version bump.

Built in **7 waves**. Dependency graph (strict edges →):

```
W1 (ring)  ─┐
            ├─▶ W4 (slot runtime) ─▶ W5 (wiring) ─▶ W6 (example)
W2 (abi) ─▶ W3 (macro) ─┘
W7 (tests) trails each wave (per-wave unit tests land with the wave; the integration test lands after W6)
```

- **W1 and W2 are independent** — can run in parallel (different crates: `metor-fsw-ring` vs
  `src/abi`).
- **W3 depends on W2** (the macro emits exports over the W2 `run_seq_*` helpers).
- **W4 depends on W1+W2+W3** (it drives sequence occupants over rings that reclaim, via the
  new ABI status and the macro-built `.so`s).
- **W5 depends on W4**; **W6 depends on W5**; **W7** trails (each wave ships its own unit
  tests; the end-to-end integration test is the W6/W7 gate).

**Critical path: W2 → W3 → W4 → W5 → W6.** W1 joins before W4. Build + test after each wave;
`cargo build -p metor-fsw-2 --no-default-features` must still drop the `kdl`/`cli` surface at
every wave (slots/sequences runtime + ABI are **not** `kdl`-gated; only the `slot` KDL node is).

---

## Wave 1 — ring reclamation: verify & harden (`metor-fsw-ring`)

**Independent of all other waves.** Foundational for the swap model (§2.3, Resolved Q2).

> **Finding that reshapes this wave (decide before coding — see Risks R1).** The design's Q2
> premise ("the writer flag and the reader-table CAS slot are claimed once and never cleared")
> is **inaccurate against the current tree**:
> - **Reader slots already free on `View::drop`** (`ring/src/lib.rs:1102-1107`:
>   `slot_cursor(slot).store(FREE_SLOT, Release)`), and reuse is already covered by
>   `reader_table_claim_free` (`ring/src/tests.rs:90-107`: drop a view → `reader_count` drops →
>   a fresh `view()` reuses the slot). `view()` claims via CAS `FREE_SLOT→start`
>   (`lib.rs:710-737`) and bumps the slot epoch (`:727`, already "reserved for future crash
>   reclamation").
> - **`Writer` holds no runtime claim at all** — `writer()` is `inner.clone()` + wake
>   endpoints (`lib.rs:697-703`); there is **no `Drop for Writer`** and **no writer flag in the
>   region header**. "At most one live writer" is a *discipline* (`lib.rs:844`), so a new
>   occupant's `Output` re-acquires a writer over the same ring for free once the old one drops.
>
> **So the release-on-drop the slot-swap needs already exists for readers and is unnecessary
> for writers.** The genuinely-missing thing — reclaiming a slot whose owner died **without
> running `Drop`** (crash reclamation) — is not needed in-process, because the slot teardown
> ordering (§6) guarantees `fsw_destroy` runs the occupant's port `Drop`s before any re-`Load`.

Given that, this wave is **verification + targeted swap-safety tests**, not a structural ring
change:

1. **No production code change expected** in `ring/src/lib.rs` for the in-process swap. If R1
   is decided as "defensive hardening wanted," the only candidate additions are: (a) a
   `debug_assert`-level single-writer guard (a writer-live flag in the header set by `writer()`
   and cleared by a new `Drop for Writer`), and/or (b) an epoch-checked stale-`View` guard.
   Both are optional and orthogonal; default recommendation is **(0) ship verification only**.
2. **Swap re-acquire test** (`ring/src/tests.rs`, new `swap_writer_and_reader_reacquire`):
   over one `RingBuffer` (overwrite, `max_readers` small), create writer `w1` + view `v1`,
   write/read a few records, `drop(w1); drop(v1)`, then create `w2` + `v2` and assert the new
   pair writes/reads cleanly and `v2` starts at the live edge (no stale/lapped reads). This is
   the unit-level proof of the slot Load→Stop→Load cycle at the ring layer.
3. **RawBacking swap test** (`ring/src/tests.rs`, sync, Miri-eligible): the same drop→reacquire
   over a `RawBacking` attached via `attach_raw` to a host region (`lib.rs:632`), proving the
   occupant-side (`.so`) reclaim path that `fsw_destroy`→re-`bind_init` exercises.

**Verify:**
- `cargo test -p metor-fsw-ring` (new swap tests green; `reader_table_claim_free` unchanged).
- Miri (per `ring/MIRI.md`), since the swap touches reader-table CAS/free under reuse:
  `cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin swap`
  and the stricter `-Zmiri-tree-borrows` + `-Zmiri-many-seeds=0..12` runs over `swap`/`concurrent`.

---

## Wave 2 — ABI: terminal status + the `run_seq_*` helpers (`src/abi/`)

**Independent of W1.** Adds the sequence-occupant lifecycle to the C-ABI (§8).

1. **`FswStatus::Done = 3`** (`src/abi/mod.rs:87`, the `#[repr(u32)]` enum): a terminal,
   non-error stop a sequence returns when its future is `Ready`. Document it next to
   `Running/StoppedLapped/Panicked`.
2. **`FSW_ABI_VERSION: u32 = 2`** (`src/abi/mod.rs:51`) — bump for the new status code (the
   version word is the load guard, dl-open.md §"Version word"). The `DlSystem::open` equality
   check (`src/dl.rs:155-160`) needs no edit; it already rejects a mismatch.
3. **`ExecuteFn` mapping in the host** (`src/dl.rs:51` type, `:322-337` `DlSlot::step`): extend
   the `match status` to map `FswStatus::Done` → a new terminal occupant outcome the slot layer
   reads (W4). For the existing `DlSlot` (build-time dl systems) a `Done` is unexpected but must
   be handled — map it to a benign stop (it cannot occur for an `export_system!` system, which
   never returns `Done`); the **slot** path (W4) is where `Done` is meaningful.
4. **`run_seq_*` helpers** — new generic helpers in `src/abi/mod.rs`, the sequence twins of
   `run_create`/`run_bind_init`/`run_execute`/`run_shutdown`/`run_destroy`/`run_describe`
   (`:428-613`). They are parameterized over a **generated sequence trait** the macro implements
   (call it `SeqSystem`, defined here in `abi` so the helpers can name it without `kdl`):
   - `SeqState<B>` — the opaque state (the sequence twin of `AbiState`, `:414`): the decoded
     params (until bind), the boxed `Pin<Box<dyn Future<Output=Outcome>>>` (built at bind), the
     `Rc<SeqClock>` (W3), the wrapper-owned `Out<SeqStatusOut, RawBacking>` tail, and a
     `poisoned` latch.
   - `run_seq_create<S>` — postcard-decode `S::Params` and box an unbound `SeqState` (mirrors
     `run_create`, `:428`, incl. the empty-params/null path).
   - `run_seq_bind_init<S>` — build a `RawBinder` over the `FswRing` arrays (`abi` ~`:345`,
     dl-open.md §"RawBinder") and bind ports **in `descriptor()` order**, then hand them to the
     macro-provided constructor: the user `Input`/`Output` ports + the implicit `SlotControlIn`
     input **move into the future**; the implicit `SequenceStatus` + health/log tail (an
     `Out<…>`) stays in `SeqState`. Sets the task-local clock around the future-builder closure
     (W3). This is the concrete answer to "where `fsw_bind_init` constructs owned ports and
     hands them to the future" (design §4.2).
   - `run_seq_execute<S>` — refresh `clock.now`, fold the `SlotControlIn` cancel into
     `clock.cancel`, set the `SEQ_CLOCK` task-local, poll the future once with
     `Context::from_waker(Waker::noop())`, write `SequenceStatus` + drive `Out::health().end_cycle`
     on the wrapper tail, and map `Poll::Ready(outcome)`→`FswStatus::Done` /
     `Poll::Pending`→`FswStatus::Running`. Catch-unwind exactly like `run_execute` (`:514`),
     latching `poisoned`→`Panicked`.
   - `run_seq_shutdown`/`run_seq_destroy`/`run_seq_describe` — mirror `run_shutdown`/
     `run_destroy`/`run_describe` (`:555-613`); `describe` lowers the macro's signature-derived
     `SystemDescriptorMsg`.
   - Every helper wraps its body in `catch_unwind` (no unwind across `extern "C"`), matching the
     module invariant (`src/abi/mod.rs:17`).

**New public surface (in `crate::abi`):** `FswStatus::Done`; `FSW_ABI_VERSION == 2`; the
`SeqSystem` trait; `run_seq_create`/`run_seq_bind_init`/`run_seq_execute`/`run_seq_shutdown`/
`run_seq_destroy`/`run_seq_describe`.

**Verify:**
- `cargo build -p metor-fsw-2` and `--no-default-features` (abi is not kdl-gated).
- `cargo test -p metor-fsw-2 abi::` — extend `src/abi/tests.rs`: a fixture sequence struct
  impl'ing `SeqSystem` driven through `run_seq_create`→`bind_init`→N×`execute`→`Done`, asserting
  the status transitions and that the `SequenceStatus` tail is written (no dlopen needed —
  helpers are testable in-proc, the house pattern).

---

## Wave 3 — `#[sequence]` proc macro + `src/sequence/` runtime (`macros/`, `src/sequence/`)

**Depends on W2.** Two parts: the runtime support module (framework crate) and the macro
(`macros/`). Land the runtime module first so the macro has something to emit against.

### 3a. `src/sequence/` runtime module (framework crate)

> **Decision (stated, per the brief): the runtime support lives in a new `src/sequence/`
> module in the framework crate**, re-exported from the crate root (ungated — sequences are an
> ABI/runtime feature, not kdl). Add `mod sequence;` to `src/lib.rs` (alongside `mod system;`,
> `:22`) and re-export the public items.

1. **`src/sequence/mod.rs`** (new):
   - `SeqClock { now: Cell<Timestamp>, cancel: Cell<bool>, progress: RefCell<Vec<String>> }`
     with `drain_progress()` (design §4.3). Default-constructible.
   - `thread_local! { static SEQ_CLOCK: ... }` and a `set(&Rc<SeqClock>, f)` scoped guard +
     a `with(f)` accessor. Sound because the poll is synchronous + single-threaded
     (coordinator.md §3.7) — the task-local is live only during a poll.
   - Free functions `wait(dur: Duration) -> Wait`, `progress(msg: impl Into<String>)`, and the
     `Wait` future whose `poll` compares a stored deadline to `SEQ_CLOCK.with(|c| c.now)` and
     checks `c.cancel` (design §4.3 code block). `Step { Elapsed, Aborted }` with
     `Step::aborted() -> bool`.
   - `Outcome { Completed, Aborted, Failed }` (design §4.5).
   - The optional explicit handle `Seq` (a thin wrapper over `Rc<SeqClock>`) exposing
     `seq.wait(..)`/`seq.progress(..)`, for the opt-in explicit form (design §4.3, Resolved Q5).
   - `SlotControlIn { cancel: bool }` frame (`#[derive(Frame, …)]`) and `SequenceStatus`
     frame (`run_state`, `progress` lines, terminal `Outcome` code) — the §7 occupant-side
     telemetry frame and the §4.4 cancel frame. (`SlotStatus`, the host-side frame, lands in W4.)
   - `SeqStatusOut` — the macro's implicit output bundle (the `SequenceStatus` port), wrapped by
     the framework's `Out<…>` so it carries health/log (`src/system/mod.rs:101-134`).
   - The `SeqSystem` impl glue / a `build_future` shape the macro fills in (the seam W2's
     `run_seq_*` bind against).

### 3b. The macro (`macros/`)

2. **`macros/src/sequence.rs`** (new) + wire into `macros/src/lib.rs` (next to the
   `export_system` entry, `macros/src/lib.rs`); `pub use metor_fsw_macros::sequence;` from
   `src/lib.rs` (mirroring `export_system`, `src/lib.rs:78`). An **attribute** macro
   `#[sequence]` / `#[sequence(name = "…")]` on an `async fn`:
   - **Signature scan / partition.** Walk the fn args; classify each by type head:
     `Input<T, B>` → input port; `Output<T, B>` → output port; a trailing `Seq` → the optional
     explicit handle. Reject anything else with a clear span error. Capture the `<B: Backing>`
     generic.
   - **Descriptor generation.** Emit `descriptor()` enumerating, **in a fixed order the
     `bind_init` walk mirrors**:
     - inputs = `[user Input<T> params in signature order, then the implicit SlotControlIn]`;
     - outputs = `[user Output<T> params in signature order, then SequenceStatus, then the
       implicit health/log (the Out tail)]`.
     **This resolves the implicit-ordering detail flagged in the design:** `SlotControlIn`
     (§4.4) is the **last input** and `SequenceStatus`+health/log are the **output tail**, so
     the host sizes/validates/announces every ring (`compatible()` `src/descriptor.rs:149`;
     `PortDesc::of::<F>()` `:90`) and `run_seq_bind_init` pops them in the same order.
   - **The future builder.** Emit a closure that binds the ports off the `RawBinder` in that
     order, moves the user ports + `SlotControlIn` into `the_async_fn(ports…)`, boxes/pins it,
     and stashes the `Out` tail in `SeqState` — the body of `run_seq_bind_init` (W2).
   - **`NAME`.** Default to the fn name (`stringify!`); `name = "…"` overrides.
   - **C-ABI exports.** Emit the `#[unsafe(no_mangle)] pub extern "C" fn fsw_*` one-liners
     delegating to the W2 `run_seq_*` helpers — structurally identical to
     `macros/src/export.rs` but pointing at `run_seq_*`. Same `SYM_*` names, so the host
     `DlSystem`/`DlSlot` (`src/dl.rs`) loads a sequence `.so` **indistinguishably** from any
     cyclic `.so`.
   - **Params.** A sequence may be paramless (`Params = ()`) or take params; honor the same
     `BuildSystem`/postcard contract `export_system!` uses (`Params: Serialize + Deserialize +
     Schema`). See Risk R3.

**New public surface:** `metor_fsw_2::sequence::{wait, progress, Seq, Step, Outcome,
SeqClock, SlotControlIn, SequenceStatus, …}`; the `#[sequence]` attribute macro.

**Verify:**
- `cargo build -p metor-fsw-2` and `--no-default-features` (sequence module ungated).
- `cargo test -p metor-fsw-2 sequence::` — macro-expansion unit tests: a `#[sequence] async fn`
  produces the expected `descriptor()` (inputs incl. trailing `SlotControlIn`; outputs incl.
  the `SequenceStatus`+health/log tail), and a `wait`/`aborted` timing test driven by a fake
  `SeqClock` under simulated time (deadline-vs-`now`, abort short-circuit). A `trybuild`-style
  compile-fail for a non-port param is a nice-to-have.

---

## Wave 4 — slot runtime (`src/coordinator/`, `src/dl.rs`)

**Depends on W1+W2+W3.** The host machinery that loads/drives/swaps occupants (§2, §3, §6).

1. **Slot-layer `SlotState`** (`src/coordinator/mod.rs`, new enum near the existing one at
   `:225-235`): `Empty | Loaded{occupant} | Running{occupant} | Done{occupant, outcome} |
   Stopped{occupant, reason}` (design §2, Resolved Q6). **Keep the existing 2-variant
   `SlotState`** (`:225`) for static/dl slots; name the new one `SlotPhase` (or
   `slot::SlotState` in a submodule) to avoid the collision. Carry an "has live future" bit so a
   post-`Stop` `Loaded` rejects `Start` (design §2 note).
2. **`SlotRunner`** — the third `CyclicSlot` impl (`CyclicSlot` trait `:247-253`). New file
   `src/coordinator/slot.rs` (or inline). Holds: the pre-built per-port `FswRing` arrays (host
   owns the rings), the allowed-occupant set (`Vec<{name, DlSystem, params}>`), the current
   `Option<DlSlot>` occupant, the `SlotPhase`, and the host-side `Out<SlotStatusOut>` telemetry
   writer. Its `CyclicSlot::step(now)` (design §2.2): publish `SlotStatus`; if `Running`, call
   the occupant's `fsw_execute` (reuse `DlSlot`'s forward, `src/dl.rs:322-337`) and fold the
   returned `FswStatus` (`Done`→`Done{outcome}`, `StoppedLapped/Panicked`→`Stopped`); else
   no-op. `init`/`shutdown` apply the `initial` occupant and tear the live one down.
3. **Command transitions** (on `SlotRunner`, design §2.1, all over existing `fsw_*`):
   - `Load(occ, params)`: pick from the allowed set, `DlSystem::into_slot`-style `fsw_create` +
     hand the slot `FswRing` arrays + `fsw_bind_init` (reuse the `into_slot`/`init` path,
     `src/dl.rs:232-314`). Empty→Loaded.
   - `Start`: Loaded(live)→Running (begin forwarding `fsw_execute`).
   - `Stop` (**hard-drop**, Resolved Q3): `fsw_destroy` the occupant (drops its future + owned
     ports → releases the ring roles via W1) → Running→Loaded(no live future).
   - `Abort`: write `SlotControlIn { cancel: true }` into the slot's control input ring; the
     occupant folds it at its next `fsw_execute` (§4.4). Cooperative; terminal `Done{Aborted}`
     arrives via `Done`.
   - `Reset`: `fsw_destroy` + `fsw_create` + `fsw_bind_init` (rebuild) from Done/Stopped/
     post-Stop Loaded.
   - `Unload`: `fsw_destroy` → Empty.
   Teardown ordering (§6): `fsw_destroy` before any re-`Load` and before `RingTable` frees; the
   `Arc<Library>` stays loaded across swaps (occupant pre-opened). Reuse `DlSlot`'s teardown
   discipline (`src/dl.rs:18-26, 357-369`).
4. **`CoordinatorBuilder::add_slot`** (`src/coordinator/mod.rs`, near `add_dl_cyclic` `:592`):
   register a slot from its **contract** descriptor (so existing `compatible()`/`WireError`
   validation + ring sizing/allocation run unchanged, coordinator.md §2), its allowed occupants
   (each a pre-opened `DlSystem` + params blob), and an optional initial occupant/state. Add a
   `Reg::Slot` variant (`Reg` enum `:466-471`); at `build()` allocate the slot's rings and the
   control ring and produce the `SlotRunner` into `cyclic` (`:1117` loop drives it like any
   `CyclicSlot`).
5. **Control plane** (design §3, Resolved Q1): a coordinator-owned `SlotControl` **input ring**
   + a `SlotCommand` frame `{ slot, command, occupant?, params? }`. In `run_for` (`:1101`),
   **before** the slot-step loop (`:1117`), drain the control ring (`Input::drain`, not
   `latest`) and dispatch each command to the addressed `SlotRunner` (the fsw-2 analogue of
   `handle_command`, `sequencer.rs:260`). Expose `Coordinator::control_handle() ->
   Output<SlotCommand>` (an in-proc writer, mirroring `Coordinator::progress()` `:1110`) and
   allow an uplink system to `connect` into it.
6. **`SlotStatus` host telemetry** (§7): a `SlotStatus` frame (current phase, occupant name,
   allowed set) the `SlotRunner` writes each cycle; lands in the `OutputRegistry` like any
   output (coordinator.md §2.4) so telemetry `All` taps it. Define the frame in `src/sequence/`
   or `src/coordinator/` (host-side).

**New public surface:** `CoordinatorBuilder::add_slot`; `Coordinator::control_handle`;
`SlotCommand`/`SlotStatus` frames; the slot-phase enum.

**Verify:**
- `cargo build -p metor-fsw-2` (+ `--no-default-features` — slot runtime is ungated).
- `cargo test -p metor-fsw-2 coordinator::` — a slot-unit test (no KDL): build a coordinator
  with one slot + a fixture sequence `.so` (or an in-proc `SeqSystem` fixture bound through the
  slot path), drive `control_handle` Load→Start, step N cycles, observe `SlotStatus` phases and
  a `Done` after `wait` elapses under a `Simulated` clock; then exercise hard-drop `Stop`
  (occupant gone, ring slot freed) and `Reset`.

---

## Wave 5 — wiring / KDL (`src/wiring/`)

**Depends on W4.** The declarative front-end for slots (§5).

1. **Model** (`src/wiring/model.rs`): add `pub slots: Vec<SlotSpec>` to `Wiring` (`:28-40`);
   define `SlotSpec { name, inputs: Vec<String>, outputs: Vec<String>, allow:
   Vec<AllowedOccupant>, initial: Option<InitialOccupant> }`, `AllowedOccupant { occupant:
   String, artifact: String, params: ParamSource }` (reuse `ParamSource` `:120-129`), and
   `InitialOccupant { occupant: String, state: SlotInitState }` where `SlotInitState =
   Empty|Loaded|Running`. All `Serialize/Deserialize/PartialEq` like the rest of the model.
2. **KDL parse** (`src/wiring/mod.rs`, `parse` `:615`, beside `parse_artifact` `:1048`): a new
   `parse_slot(node)` for the `slot "name" { input frame=…; output frame=…; allow occupant=…
   [artifact=…] [params…]; initial occupant=… state=… }` surface (design §5). Edges already
   resolve a slot name like any instance — `parse_connect` is unchanged. Add span-aware
   `LoadError` variants for slot mistakes (unknown occupant artifact, dup occupant name,
   missing contract frame).
3. **`resolve`** (`src/wiring/mod.rs:688`): for each `SlotSpec`, build the contract
   `PortDesc`s, **open each allowed occupant's `DlSystem`** once (`DlSystem::open`, `src/dl.rs:142`),
   `compatible()`-validate each against the contract (reuse `src/descriptor.rs:149`), encode
   each occupant's params (the existing `encode_kdl_params`/postcard path), and call
   `CoordinatorBuilder::add_slot` (W4). A slot artifact is an ordinary `cdylib` artifact
   (`Artifact`, `:73`) — the sequence `.so` built by `#[sequence]`; the build driver
   (`build_artifacts`) and bundle/package path (`src/wiring/bundle.rs`) need **no change** (a
   sequence cdylib packages like any other; note this portability in the wave).
4. **`WiringBuilder`** (`src/wiring/builder.rs:36`): a `.slot(name)…` builder mirroring
   `.system(..)`, so the Rust front-end can express slots too (parity with KDL).

**Verify:**
- `cargo build -p metor-fsw-2` (kdl on) and `--no-default-features` (the `slot` node is
  kdl-gated; the runtime from W4 still builds without kdl).
- `cargo test -p metor-fsw-2 wiring::` — a `slot` KDL round-trips to a `Wiring`, `resolve`
  opens+validates the occupants and produces a coordinator with the slot registered; a contract
  mismatch is a clean `LoadError`.

---

## Wave 6 — example: sequences end-to-end (`examples/adcs-seqs/` + `adcs-fsw2` mission)

**Depends on W5.** The lightest credible demo.

1. **`examples/adcs-seqs/`** (new crate of sequence cdylibs, or a `systems/seqs` subdir under
   the existing `adcs-fsw2` example): at least **two** sequences via `#[sequence]`, reusing the
   existing `adcs-fsw2/contracts` frames so they wire to the live plant/nav/ctrl:
   - a **commissioning-style** sequence (settle → enable → point, gated on `wait`, with an
     abort→safing branch) — the design §4.1 body;
   - a **safe-mode-style** sequence (the abort target / standalone safing).
   Each is its own `cdylib` (`crate-type=["cdylib"]`) with one `#[sequence]` fn (one export per
   `.so`, like `export_system!`).
2. **`mission.kdl`** (extend the example's, `examples/adcs-fsw2/mission.kdl`): add `artifact`
   nodes for the two sequence `.so`s and a `slot "adcs" { input/output …; allow occupant=…
   (×2); initial … }` wired with `connect` into/out of the plant (design §5 KDL).
3. **Driving Load/Start/Stop/Abort.** The lightest path: use `Coordinator::control_handle()`
   from the example's test/harness to write `SlotCommand`s. (A CLI `metor-fsw` sub-surface for
   slot commands is **out of scope** for v1 — note it as future work; the control handle is
   enough to demonstrate and test.)

**Verify:**
- `cargo build -p adcs-seqs` (the sequence cdylibs compile).
- A bounded run that loads+starts the commissioning sequence and observes it reach `Completed`
  in telemetry (the W7 integration test, below). The existing `closed_loop.rs` parity test
  stays green (sequences are additive).

---

## Wave 7 — tests & final gate

**Per-wave unit tests land with their wave** (W1 swap/Miri, W2 abi helper, W3 macro/descriptor +
wait/abort timing, W4 slot-unit + control-frame drain, W5 wiring round-trip). This wave adds the
**end-to-end integration test** and the final multi-crate gate.

1. **Integration test** (`examples/adcs-fsw2/tests/sequences.rs` — it has the cdylibs): build
   the mission with a slot, `resolve`, `run_for` under a `Simulated` clock; via
   `control_handle`: `Load`→`Start` the commissioning sequence, step until it emits
   `Progress`→`Completed` on `SequenceStatus`; then a second scenario exercising **hard-drop
   `Stop`** mid-run (assert the occupant is gone and its ring reader slot is freed/reusable),
   **`Reset`** (rebuild + re-run from the start), and **`Abort`** (cooperative cancel →
   `Done{Aborted}` via the safing branch). Assert `SlotStatus` phase transitions throughout.
2. **Final gate:**
   - `cargo build -p metor-fsw-ring && cargo test -p metor-fsw-ring` (+ the Miri swap run).
   - `cargo build -p metor-fsw-2` and `cargo build -p metor-fsw-2 --no-default-features`
     (slots/sequences/ABI present without kdl; only the `slot` KDL node + `cli` drop out).
   - `cargo test -p metor-fsw-2`.
   - `cargo build -p adcs-seqs && cargo test -p adcs-fsw2` (parity + the new integration test).
   - Then commit (task boundary).

---

## Notes, invariants, and open implementation risks

**Risks to decide / watch during impl:**

- **R1 — Wave 1 is smaller than the design assumed (DECIDE).** As documented in Wave 1, reader
  slots already free on `View::drop` (`ring/src/lib.rs:1102`, tested `ring/src/tests.rs:90`) and
  writers hold no claim (no `Drop for Writer`, no header flag). The slot-swap reclaim therefore
  **works today with no structural ring change** — Wave 1 reduces to verification + swap tests.
  *Decision needed:* ship verification-only (recommended), **or** add optional defensive
  hardening (a debug-assert single-writer-live guard + epoch-checked stale-`View` guard). I
  recommend verification-only and a one-line correction to `docs/sequences-slots.md` §2.3/Q2
  noting the premise was inaccurate; flag if you want the doc edited.
- **R2 — Descriptor/bind ordering (watch).** The macro's `descriptor()` order and
  `run_seq_bind_init`'s `RawBinder` pop order **must** agree exactly, including the implicit
  trailing `SlotControlIn` input and the `SequenceStatus`+health/log output tail (W3 step 2,
  design §4.2/§4.4). A mismatch is a silent mis-bind (a port attached to the wrong ring). Guard
  it with a bind-order assertion test in W3 and an end-to-end frame-identity check in W7.
- **R3 — Sequence params decode (watch).** Sequences reuse the `BuildSystem`/postcard params
  contract; a paramless sequence is `Params = ()` (zero bytes, like `export_system!`). If a
  sequence takes params, `run_seq_create` must decode them before `bind_init` builds the future
  — confirm the macro threads `Params` through and that `fsw_describe` exports the
  `params_schema` so KDL `params…` on an `allow` line encode without the host linking the type
  (the existing `encode_kdl_params` path). Decide whether v1 sequences are paramless-only
  (simplest) or params-capable (uniform with systems). Recommend params-capable for uniformity.
- **R4 — `Done` on a non-sequence occupant (watch).** A slot may be loaded with an ordinary
  cyclic `.so` (the design allows any compatible occupant), which never returns `Done`; and the
  build-time `DlSlot` path now sees a `Done` arm it must handle benignly (W2 step 3). Keep the
  mapping total and tested.

**Invariants:**
- Slots/sequences runtime + ABI are **ungated** (no `kdl`); only the `slot` KDL node and the CLI
  ride the `kdl` feature. `--no-default-features` must build the runtime at every wave.
- One `#[sequence]` per `cdylib` (one `#[no_mangle]` export set), same as `export_system!`
  (`docs/dl-open.md` §"export_system!").
- The host drives a sequence `.so` through the **same** `SYM_*`/`FswRing`/`FswStatus` contract
  as any cyclic occupant — `src/dl.rs` `DlSystem`/`DlSlot` are reused; the future-vs-`CyclicRunner`
  choice is invisible behind the `.so`.
- Teardown ordering (`fsw_destroy` before library unload and before `RingTable` frees) is the
  reused `DlSlot` discipline (`src/dl.rs:18-26`), now per occupant **and per swap**.
