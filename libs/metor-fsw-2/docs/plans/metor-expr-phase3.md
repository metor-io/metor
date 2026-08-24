# metor-expr Phase 3 — Python systems in the target file

2026-08-24. Parent design: `python-expressions.md` (revision 4). Prior
phases: `metor-expr-phase0.md` (compiler), `metor-expr-phase1.md`
(panel runtime + `=` fields), `metor-expr-phase2.md` (unified canvas,
node editor deleted).

## Goal

A `@system`-decorated function written **directly in `target.py`** runs
on the vehicle as a first-class cyclic system: ports from the signature,
output as real telemetry, wired to native systems by ordinary edges,
compiled to WASM at the init gate, executed under wasmi with fuel.

## The pivot: no `ExprSystem`

Revision 4 staged Phase 3 as "`ExprSystem`, `target.systems(Path)`" — a
named built-in wrapper system plus path-based module registration. Both
are gone (user decision, 2026-08-24). The registration surface is the
decorator itself:

```python
from metor_config import Target, system, node

target = Target(cycle_rate=100.0)
imu = target.add("imu", adcs_pack.Imu())

@system("imu.omega_b")
@node(x=420, y=180)
def omega_norm(omega_b):
    return (omega_b @ omega_b) ** 0.5
```

No `ExprSystem` type in the registry, no separate module file, no
`Path` indirection. The target file *is* the program. Consequences:

- The decorator is the capture point: it records the function's source
  at config-eval time; `Target.to_ir()` assembles the captured
  declarations into one module per target and threads it through the IR.
- There is no user-visible wrapper to parameterize, so **live source
  replacement loses its vehicle** (`ExprSystem` params were the uplink
  path). Phase 3 recompiles at init; the panel stays the live-iteration
  surface. Runtime swap returns as its own arc if wanted (see Open
  Questions).
- The host machinery still exists, but as internal init plumbing (a
  bind arm and a runner struct), not a registered `type=`. Standard
  path: the runner follows the existing dynamic-port precedents
  (`SlotRunner` drives a foreign descriptor as a `CyclicSlot`;
  `UplinkSystem` mints ports from config via `instance_descriptor`),
  it does not invent a parallel one.

The promotion path survives intact and gets shorter: prototype in the
panel canvas, then paste the same function into `target.py` — the
decorator spelling, frame classes, and semantics are identical because
the vehicle compiles the same source with the same compiler.

## What already exists (survey 2026-08-24)

- **wasmi 1.1 is already a host dependency** (`metor-fsw-2/Cargo.toml`)
  with a proven sandbox: `src/wasm.rs` has the fuel-per-call, memory
  ResourceLimiter (64 MiB default), and trap policy, plumbed from
  `CoordinatorSpec.wasm_fuel_per_poll` / `wasm_memory_limit_bytes`. It
  is pack-ABI-shaped (guest rings, `RingBridge`); Phase 3 writes a much
  smaller expr-ABI host reusing the same policy. The panel's
  `dynamic/ops/program.rs` (`Compiled`/`Running`) is the reference
  implementation of the expr-ABI hosting sequence.
- **`metor-expr` is host-ready**: `compile_module(src, &dyn Resolver)`,
  `Manifest::declarations()` build order, `pub mod state` (snapshot /
  restore keyed `(system, field, ty)`, `@rng` host-seeded), `Layout`
  with the `@node` rewrite machinery. `Resolver::frame()` returning
  `Some` is the explicitly reserved FSW hook (panel returns `None`).
  Note the manifest has **four** fields — `{compiler, systems, stages,
  functions}` — and `declarations()` interleaves systems and stages; a
  host that ignores stages silently drops resamples.
- **Dynamic Table ports are constructible**: `PortDesc` fields are all
  `pub`; `PortSchema::Table` builds from `metor_proto::vtable::builder`.
  `announce()` derives prefixed component ids from the metadata list, so
  metadata is load-bearing.
- **No source capture exists in the recorder** — nearest precedent is
  `_source_ref()`'s frame-walk provenance. `inspect.getsource` is new
  ground.
- **`metor_config.Frame` already exists** as the base of pack-generated
  frame classes (typed fields, `InPort[F]`/`OutPort[F]` markers). This
  is a unification opportunity, not a collision: an expr signature that
  annotates a parameter with a pack-generated frame class *is* the
  `bind=`-to-host-frame case, checked one-to-one at compile.
- **`metor-fsw-2` does not depend on `metor-expr` today.** Adding it
  pulls rustpython-parser + wasm-encoder + the checked-in
  `prelude.wasm` into the flight binary — priced in Phase 0 (parser +
  encoder, 136 µs compiles, no toolchain on target).

## Design decisions

**D1 — Capture unit: one synthetic module per target.** Each `@system`
records `{source (dedented, decorators included), file, firstlineno}`
via a module-scoped registry the decorator appends to; user-defined
`Frame`/`State` subclasses capture the same way through
`__init_subclass__` (pack-generated frames are marked and skipped —
they are *host* frames, resolved not compiled). `to_ir()` assembles
captured classes + functions in definition order into one module
string. Rationale: `Binding::Produced` (one system reading another's
output) and `Manifest::declarations()` ordering only work inside one
compilation unit, and the panel's program pane has the same unit.
Diagnostics must map back: the IR carries per-declaration
`SourceRef`s and the synthetic module's per-decl offsets so an
init-gate error prints `target.py:LINE`, never a synthetic-module
line.

**D2 — Tiers on the vehicle: decorated forms only.** `@system` defs
(both sugar and canonical frame form) and `Frame`/`State` classes
execute harmlessly under CPython (defined, never called) and are
capturable. Bare expressions and top-level `name = expr` bindings would
actually *evaluate* under CPython and fail — they remain panel-only
tiers, which matches the promotion gradient (`=` field → module binding
→ `@system` def; the def is the flight form).

**D3 — IR shape: uniform `SystemSpec` + one program blob.** Each
captured system emits an ordinary `SystemSpec` (`ty: "expr"`, no
artifact) so name/scope/edge validation, namespacing, and the broadcast
manifest treat Python systems like any other; a new `Wiring.program:
Option<ProgramSpec>` carries `{source, decls: [{name, src: SourceRef,
offset}]}` once. `IR_VERSION` 8 → 9 in **both** `ir.rs` and
`metor_config/__init__.py` (also fix the stale v2–v6 comment block),
golden fixture `tests/golden/target.json` updated for **both** the Rust
`ir_contract.rs` and Python `test_golden.py` suites. Broadcasting the
source in `WiringManifest` is deliberate: the panel canvas can show
vehicle Python systems with their real source (read-only in this
phase). Size is KBs against a manifest ring that already special-cases
its size.

**D4 — Compile at the init gate, in the resolve systems pass.**
`resolve_with` gains an `"expr"` arm: build an `FswResolver` over the
already-resolved graph (component paths → `Ty` from `PortDesc`
vtable metadata; `frame(name)` → `FrameSchema` from host frame
definitions — the reserved hook; **ids are carried from the registry,
never re-derived** — `ComponentId::new` masks a bit, re-hashing
"works" for half of all names), call `compile_module`, and construct
one `Node` per manifest system with a hand-built `SystemDescriptor`.
This must complete inside the pass because `instance_descriptor` runs
at node construction, before `build()`. A bad program fails
construction with a `target.py`-line-numbered diagnostic — the design
doc's init-gate promise.

**D5 — One instance, N cyclic nodes.** The compiled module instantiates
once (one `Store`, fuel set per eval from `wasm_fuel_per_poll`, memory
limit from config, `@rng` seeded at instantiation); each system is its
own cyclic `Node` sharing the instance through the runner. Rationale:
the cyclic loop is single-threaded, buffers are per-system static
addresses, and N instances would cost N linear memories for nothing.
Each system being its own node keeps step order, edge validation
(`StaleFrameEdge`), health, and telemetry uniform.

**D6 — Ports are real rings, even Python→Python.** Every binding
becomes an edge: `Binding::Component(path)` → an edge from the
producing port (consumer `PortDesc` is the single-component subset —
Table compatibility is component-subset by design);
`Binding::Produced{system, field}` → an edge between the two Python
systems' rings. Inputs are `Snapshot × One` (latest-wins is the run
rule; `Snapshot × Many` is a hard error anyway). The output port is one
Table port per system: frame name from the manifest, vtable +
metadata built from `Frame{fields}` (8-byte slots, f64-aligned, bool in
the low 4 — the Phase 1 layout), components named `{system}.{field}`
and prefixed by instance/namespace exactly like native ports. This is
what makes a Python system's output first-class telemetry with zero new
announce machinery.

**D7 — Run rule on the coordinator clock.** `on=`/default: fire when
the driving input has a fresh sample this cycle (drain non-driving
inputs, hold latest), skip the cycle otherwise; a port that has never
published skips — identical to the panel. `rate=`: the coordinator has
one global `cycle_rate` and no per-system division, so the runner
decimates with a step counter; **validate at the gate that `rate`
divides `cycle_rate`** (error, not silent rounding). A system with no
inputs requires `rate=` (same diagnostic as the panel).

**D8 — Faults degrade, never kill.** A non-zero eval return or a trap
(fuel exhaustion included — `while True:` burns its grant) marks the
system's `SystemHealth` degraded with the fault code and a `LogEvent`,
skips the publish, and keeps draining inputs so upstream never backs
up — the panel's park behavior, expressed in FSW health vocabulary.
The vehicle never stalls on a bad expression.

**D9 — Ordering.** Python systems are pushed after the native systems
pass, before the deferred receive-all block (receive-all must be last;
being before it keeps downlink telemetry same-cycle fresh). They can
therefore read any native output same-cycle; feeding a native system
declared earlier requires `delayed=True` on the edge, and the existing
`StaleFrameEdge` diagnostic already says so.

**D10 — Cross-wiring surface.** The decorator returns a handle usable
where a `SystemHandle` is: `omega_norm.out` in
`target.connect(omega_norm.out, nav.some_input)` — legal when the
output frame is (or binds one-to-one to) a host-defined frame, checked
by the same Table compatibility rule. Path-bound sugar outputs (an
anonymous frame) connect only component-subset-wise, which the rule
also already handles.

**D11 — Layout lands in the IR, for both sources.** `SystemSpec` gains
`layout: Option<(f32, f32)>` (same IR bump as D3). Python systems get
it from `@node(x=, y=)`; native systems get a placement kwarg on
`Target.add(..., node=(x, y))` — "position lives at the declaration
site" finally covers both. The panel canvas prefers IR layout over
auto-layout; the per-view manual override map still wins locally (the
Phase 2 decision), and re-layout still clears overrides only.

## Work packages

**WP1 — Recorder: capture + IR.** `metor_config`: export `system`,
`node`, `State`, and the annotation vocabulary (`Tensor`, dtypes) —
`Frame` unifies with the existing pack-frame base (D1); decorator
captures source + provenance; handle object for `target.connect`
(D10); `Target.add(node=)` (D11); `to_ir()` assembles the module and
emits `SystemSpec`s + `ProgramSpec` (D3). IR_VERSION 9 both sides,
both golden suites, recorder version bump, stale comment fixed.
Stubgen/type-stub updates so `target.py` type-checks.

**WP2 — Rust IR + validate.** `ir.rs`: `ProgramSpec`, `SystemSpec.
layout`, version bump; `validate.rs`: `ty == "expr"` systems must
reference a program decl, program decls must be unique, rate-divides-
cycle_rate check (D7 — needs the coordinator spec, so it may live in
resolve; put it wherever the diagnostic is best), `check_system`
relaxed for artifact-less expr systems.

**WP3 — Init-gate compile.** `metor-fsw-2` ← `metor-expr` dependency.
`FswResolver` (D4) over the resolved graph: `component()` from
announce-shaped metadata with carried ids, `suffix()` for authoring-
time names, `frame()` from host frames — the Phase 1 reserved hook
comes alive. Compile in the resolve systems pass; map diagnostics to
`target.py` lines via `ProgramSpec` offsets; fail construction cleanly.

**WP4 — The runner.** Expr-ABI host (small sibling of `wasm.rs`, same
fuel/limit/trap policy): instantiate once per program (D5), per-system
cyclic slots with hand-built `SystemDescriptor`s (D6), raw ring
writers/views (the `SlotRunner` precedent), the run rule + decimator
(D7), fault policy (D8), `@rng` seeding, state slots initialized from
manifest defaults. Bind arm mirroring `bind_slot`. Ordering per D9.
Stages: per Open Question Q1's answer.

**WP5 — Edges from bindings + handles.** Resolve `Binding::Component`
and `Binding::Produced` to real edges with subset `PortDesc`s (D6);
`target.connect` edges from decorator handles type-check through the
existing compatibility rule (D10).

**WP6 — Panel: vehicle Python systems on the canvas.** The canvas
already renders manifest systems; teach it that an `"expr"` system
carries source in `ProgramSpec` — show it read-only (open the card,
see the function), position from IR layout (D11). No editing of
vehicle systems in this phase.

**WP7 — Example + differential.** `examples/adcs-fsw2/target.py` gains
a real Python system (e.g. `omega_norm` off the IMU). Integration
test: eval target → init → run N cycles → assert the output component
telemeters with correct schema and values against the nox oracle.
**Bit-parity test**: the same module, same input samples, evaluated by
the panel host and the FSW runner produce bit-identical output frames
(the design doc's promise at line 627). Fault-path test: a
fuel-exhausting system degrades health and the vehicle keeps cycling.

Gates: WP1+WP2 land together (IR contract). WP3+WP4+WP5 are the core
and land together behind the integration test. WP6 and WP7's
differential can trail. Commit at each gate.

## Decided questions (user, 2026-08-24)

**Q1 — Resample stages: rejected this phase.** `resample_zoh`/
`resample_linear` are top-level bindings — under CPython they'd need
exported marker functions to be capturable at all. Not exported; a
program whose manifest contains stages fails the gate with "resample
is panel-only for now". Add the host resampler when a vehicle use case
shows up.

**Q2 — Runtime source replacement: deferred.** With `ExprSystem` gone
there is no parameter surface to send new source through; a config
change is a rebuild + restart, the normal FSW config flow. The panel
remains the sub-second iteration surface. Revisit as its own arc (it
wants a cycle-boundary swap protocol and a port-topology-change story,
which is re-init territory anyway).

**Q3 — `rate=` that doesn't divide `cycle_rate`: hard error** (D7).

## Risks

- **The multi-field frame path has never run.** The panel's port layout
  hard-errors on ≠1-field frames ("a multi-field frame is a Phase 3
  shape") — on the vehicle every host frame is multi-field. The
  canonical-form path (declared frames, `bind=`) gets its first real
  exercise; the vtable-vs-manifest one-to-one check is new code. The
  differential test in WP7 is the guard.
- **Descriptor/binding order is a silent-misbind trap** (minted ports
  must trail statics on the `UplinkSystem` path; positional bind is in
  declaration order). The runner uses raw rings precisely to keep this
  explicit; a pinned test asserts port order round-trips through the
  descriptor.
- **Flight-binary weight**: rustpython-parser + wasm-encoder + prelude.
  Phase 0 priced compile speed, not binary size — measure the delta in
  WP3 and record it in Results; if it's ugly, feature-gating is the
  escape hatch (default-on, since the whole point is no toolchain on
  the target).
- **Id re-derivation footgun** (`ComponentId::new` masks a bit) now has
  a second host. `FswResolver` carries ids from the registry — stated
  in D4, tested in WP7.

## Out of scope

- Sequences (Phase 4, deferred decision point).
- Editing vehicle Python systems from the canvas / uplink swap (Q2).
- Per-system rates that don't divide the coordinator clock.
- Reclaiming the panel's hidden `expr.<hash>` components (existing
  deferred item, unrelated).
