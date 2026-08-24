# metor-expr Phase 3 — Python systems in the target file

2026-08-24, **revision 2** (pack-ABI unification — user direction: "why
can they not fit in the same spot as normal WASM? Ideally these are
very similar to slots and sequences"). Parent design:
`python-expressions.md` (revision 4). Prior phases:
`metor-expr-phase0.md` (compiler), `metor-expr-phase1.md` (panel
runtime + `=` fields), `metor-expr-phase2.md` (unified canvas, node
editor deleted).

## Goal

A `@system`-decorated function written **directly in `target.py`** runs
on the vehicle as a first-class cyclic system: ports from the
signature, output as real telemetry, wired to native systems by
ordinary edges, executed under wasmi with fuel — **as an ordinary wasm
pack artifact through the existing wasm machinery**, not through an
expression-shaped sibling of it.

## The two pivots

**No `ExprSystem`** (user, 2026-08-24): the decorator is the
registration surface. `@system` functions live in `target.py`, captured
at config-eval time; no wrapper type, no `target.systems(Path)`.

**No expr-shaped carve-out on the vehicle** (user, 2026-08-24):
revision 1 planned a parallel expr-ABI host (a second wasm driver, a
`ty: "expr"` special case in resolve, a bespoke runner). Instead, the
compiler emits modules that **speak the pack ABI**, so a Python program
compiles to a pack artifact exposing N systems — exactly the shape a
Rust pack cdylib has — and flows through `WasmPack`/`WasmSlot`/
`RingBridge` like any other wasm. Python becomes *a pack whose build
step is metor-expr instead of cargo*. What revision 1's carve-out
would have duplicated, this pre-pays instead: the pack-ABI guest layer
is exactly what Phase 4 sequences need.

Status note: revision 1's gate A landed as `cd67aabe` (recorder
capture, IR v9); its host-side draft (`src/wiring/expr.rs`,
`src/coordinator/expr.rs`) was stopped **untracked and unwired** — no
`mod` declaration, `resolve_exprs` never called. Gate A's capture
machinery survives this revision; its IR emission shape is reworked in
place (v9 is branch-local, no v10). The draft files are mined for
their vtable-construction code and deleted.

## What the unification buys

- **One wasm host on the vehicle.** `WasmSlot`'s open/bind/pump/execute
  machinery drives Python systems; no expr-ABI sibling to maintain.
- **The compiler stays off the vehicle.** Compilation happens at
  build/provision time — the same seam that builds path-source cdylibs
  — so the flight binary never links rustpython-parser or
  wasm-encoder. The binary-size risk from revision 1 disappears; "no
  toolchain on the target" becomes literal.
- **Slots work today.** A Python system mounted in a slot is an
  ordinary wasm occupant: `Load`/`Start`/`Stop`/`Reset` occupant
  swapping already exists and is tested for wasm (fresh-bytes reload
  gated by `entry_identity`). Revision 1's deferred hot-swap question
  partially dissolves into standard machinery.
- **Sequences converge.** A wasm sequence path already exists and is
  tested (`seq-fixture` runs a `task` entry to `Done` under the
  bridge). Phase 4 compiles Python coroutines into the same artifact
  shape this phase establishes.
- **Provisioning, hashing, bundling come free-ish.** The `.so` and
  `.wasm` paths share `PackManifest` bytes exactly; a build step that
  emits the `.manifest` sidecar inherits `check_manifest_hashes` and
  the sidecar tooling unchanged.

## Survey facts this plan builds on (2026-08-24)

- **The pack ABI guest contract is satisfiable by a minimal guest.**
  Required exports (all resolved once in `WasmPack::open`):
  `fsw_abi_version` (= 11, checked first), `fsw_pack_open/close`,
  `describe`/`manifest_ptr` (postcard `PackManifest` — can be a baked
  data segment), `create(pack, index, mount, params)` (entry by
  manifest position), `bind_init`, `execute(state, now) -> FswStatus`,
  `shutdown`, `destroy`, `alloc` (host allocates params, ring regions,
  `FswRing` arrays, name — all sizes statically known, so a bump
  pointer over a compile-time-sized arena suffices; align 8),
  `ring_init` (guest formats the ring header), `set_now`. `free` is
  exported by the macro but never called by the host. **Zero imports**
  — the linker is empty, any import fails instantiation.
- **Rings are guest-allocated and guest-formatted** (the allocator and
  arch-tag lessons are already encoded in the design). The guest must
  speak the real ring format — magic/version/control/reader slots,
  8-byte aligned records, Release-committed positions, the SeqCst
  registration fence. `metor-fsw-ring` is already
  `default-features = false`-capable for wasm: **link the real crate
  into the prelude rather than reimplementing a safety-critical
  format** (fall back to a minimal hand impl only if it drags in
  weight the prelude can't carry; it creates/attaches on raw regions,
  no allocator needed).
- **Multiple systems per artifact already works** end to end
  (manifest `Vec<PackEntryDesc>`, create-by-index, name→index lookup
  in `resolve_wasm_occupant`; the seq-fixture ships three entries).
- **The host pumps**: `RingBridge` forwards host↔guest per cycle —
  `Log` legs drain the backlog, `Snapshot` legs forward `try_latest()`
  only when `committed` moved. Memory is pinned after bind
  (`ResourceLimiter` denies growth; `check_memory_stable` before every
  pump). Fuel is granted **per call**; setup fuel and poll fuel are
  separate budgets. Marshalling measured at 7 ns against a 2,873 ns
  cycle.
- **Wired wasm systems don't exist yet — and nothing structurally
  prevents them.** `resolve_with` sends any `(Some(artifact), false)`
  spec to `resolve_dl` with **no kind check** (a wasm artifact would
  be `dlopen`'d — silent fallthrough to guard). The wired arm is
  ~120–180 mechanical lines: `SystemBind::Wasm` + `WasmReg` (path,
  entry name, identity, params — mirroring `ProcReg`), a bind arm
  that is `WasmSlot::bind_opened` with `Mount::Wired` (0), no mount
  tail, no delivery padding, and a thin `CyclicSlot`. Fuel/memory
  plumbing already reaches binds via `ProcBindCtx`.
- **Provisioning has the seam but not the arm**: `provision_artifacts`
  never inspects `artifact.kind`; `ArtifactKind::Wasm` exists in the
  IR (since v7) but the recorder never emits it and bundling's member
  naming assumes cdylibs. The recorder's `Artifact` also has no way to
  name a single arch-neutral file.
- **`entry_identity` is postcard of (descriptor, params_schema,
  reloadable)** — byte-fragile by design. Codegen must be
  deterministic to the metadata level or every reload is rejected.
- **No state carryover exists in the pack path** (`#[fsw(snapshot)]`
  is a delivery marker, not persistence; occupant swaps destroy the
  instance). metor-expr's `state` module is the right shape for a
  future pack-ABI addition; not this phase.
- **Guest panics are invisible on wasm32** (abort, not unwind; no
  imports ⇒ no message). The generated guest must have no panicking
  paths; validate ring configs before formatting rather than trusting
  `catch_unwind`.

## Design decisions

**D1 — Capture: unchanged from revision 1 (landed).** `@system` /
`@node` decorators and `Frame`/`State` `__init_subclass__` capture
source with file/line provenance in the recorder; `to_ir()` assembles
one synthetic module per target in definition order;
`Wiring.program` carries `{source, decls}` — now as **build input and
provenance/canvas display**, not as an init-gate compile input.
Decorated forms only on the vehicle (bare expressions and top-level
bindings would evaluate under CPython; they stay panel-only). Stages
(`resample_*`) rejected: markers not exported, a manifest containing
stages fails the build (user decision, unchanged).

**D2 — The compiler emits pack-ABI modules.** metor-expr's template/
prelude layer grows the pack entry points as thin adapters over the
existing static-buffer machinery: baked postcard `PackManifest` data
segment (one `PackEntryDesc` per `@system`: full `SystemDescriptor`
with real vtables built from `Frame` layouts — 8-byte slots,
f64-aligned, bool low 4; metadata list is load-bearing for announce;
`params_schema` empty/unit; `reloadable: true`), `fsw_abi_version` =
11, bump-arena `alloc`, `ring_init`/ring I/O via linked
`metor-fsw-ring`, `create` by entry index, `execute` dispatching to
the per-system eval. **The `expr_*` export family stays** — one module,
two hosts: the panel keeps its fine-grained per-sample surface
(`_arg_ptr`/`_eval`/`_state_ptr`), the vehicle speaks pack. Forcing
the panel onto per-cycle pump semantics would regress its streaming
model for no gain; the two families share every body and buffer. The
mined vtable-construction code from the untracked draft seeds the
manifest baking. **Codegen determinism is a contract** (entry_identity):
pinned test — compile twice, byte-identical module.

**D3 — Run rule lives in the guest.** Pack `execute` runs every cycle;
the guest decides. Driving input: fire when the driving ring's
committed position moved since last execute, read latest from the
rest, skip (return Ok, publish nothing) otherwise; never-published
driving input skips. `rate=`: fire when `now` crosses the next tick
(guest-side, from the `execute(now)` argument); the gate still
validates rate-divides-cycle_rate (hard error, user decision Q3).
This keeps the host driver completely generic — it cannot tell a
Python pack from a Rust one.

**D4 — Compile at build/provision time.** `provision_artifacts` gains
`ArtifactKind` dispatch (also fixing the silent dl fallthrough for any
misdeclared artifact): the wasm-from-program arm runs
`metor_expr::compile_module` against a resolver built from the other
artifacts' decoded pack manifests (paths → `Ty` from `PortDesc`
vtable metadata, **ids carried, never re-derived** —
`ComponentId::new` masks the FNV top bit; `frame()` answered from
host frame definitions — the reserved hook), writes the `.wasm` next
to the built cdylibs, sets `artifact.path`, and emits the
`.manifest` sidecar so `check_manifest_hashes` covers it unchanged.
Diagnostics map to `target.py` lines via `ProgramSpec` offsets — the
line-numbered-failure promise moves from init gate to build gate,
which is strictly earlier. Bundling's member naming gets a wasm arm.
`--no-build` (`locate_artifacts`) treats the compiled `.wasm` like a
located cdylib.

**D5 — IR: uniform artifact + entries, no `"expr"` type.** The
recorder emits one `Artifact { kind: Wasm }` for the program (recorder
`Artifact` gains `kind` and drops the cdylib-only field requirements
for wasm; the stale "v7 added the wasm artifact kind" comment finally
becomes true) and one ordinary `SystemSpec { name, ty: <entry name>,
artifact }` per `@system` — exactly how cdylib pack entries are
addressed. Gate A's `EXPR_TYPE`/artifact-less validation carve-outs
are removed; program-decl validation (unique names, every system
references a decl) stays. IR stays at v9, goldens amended in place.
`SystemSpec.layout` (D11 of revision 1) is unchanged: `@node(x=,y=)`
for Python systems, `Target.add(node=)` for native ones, canvas
prefers IR layout, local overrides still win.

**D6 — Resolve: the wired wasm arm.** `resolve_with` dispatches on
`ArtifactKind`: the wasm arm opens the artifact (setup fuel), finds
the entry by name, registers `SystemBind::Wasm`/`WasmReg`, and the
bind pass instantiates via the `Mount::Wired` variant of the slot's
bind sequence with a thin `CyclicSlot`. This is a **general
capability** — any wasm pack becomes mountable as a plain wired
system, Rust-authored ones included; Python is just its first
producer. Kind guard added to the dl arm. `resolve_wasm_occupant`
refactors to share the describe step. One wasmi instance per artifact
is the natural outcome (entries share a module instance the way slot
occupants each get one; prefer one instance per artifact serving all
its entries if the bind sequence allows, else one per entry — decide
by measurement, both are correct).

**D7 — Edges.** Explicit `target.connect(handle.out, ...)` edges ride
the IR as today (Gate A's handles). Path bindings
(`Binding::Component`) and Python→Python bindings
(`Binding::Produced`) are synthesized into edges at resolve by
reading the expr manifest baked in the artifact (`expr_manifest_ptr`
— already exported): input port descriptors are in the pack manifest
(the compiler built them from resolved types), so synthesis is only
"find the producing (instance, port) for each bound path and connect".
Ordering: Python systems register after native systems, before the
deferred receive-all block; feeding an earlier native system needs
`delayed=True` (`StaleFrameEdge` already says so).

**D8 — Faults degrade, never kill.** The wired runner maps
trap/fuel-exhaustion/pump failure to degraded `SystemHealth` + a
`LogEvent`, keeps pumping inputs (drops counted like
`wasm_boundary_dropped`), never re-enters a dead instance — the slot
path's `dead` latch, surfaced in system-health vocabulary. The
vehicle never stalls on a bad expression.

**D9 — `@rng` seeding.** No imports means no guest entropy. The seed
rides the params channel: resolve injects a host-entropy seed into
the entry's params at vehicle init (fresh per boot), and the
generated `create` stores it into the `@rng` state slot — the same
observable behavior the panel host produces by writing the slot
directly.

**D10 — Slots: free, state carryover deferred.** A Python system named
in a slot's `allow` set works through the existing occupant machinery
today (including fresh-bytes reload gated by `entry_identity` —
another reason D2's determinism contract matters). State carryover
across swaps doesn't exist for any pack and is out of scope; when it
comes, metor-expr's `state` module (`StateKey` triples, seed guards)
is the shape a pack-ABI `state_ptr` addition should take. Live
source-uplink remains deferred: there is no file-transfer path in the
crate today, and the compiler lives ground-side where the panel
already runs it.

## Work packages

**WP1 — IR rework (revises Gate A in place).** Recorder: `Artifact`
gains `kind`, wasm artifacts drop crate/lib requirements, program
emits `Artifact{kind: Wasm}` + per-system `SystemSpec{ty, artifact}`
(D5); remove `EXPR_TYPE` carve-outs, keep program-decl validation;
goldens amended both suites, still v9. Delete the untracked expr host
drafts after mining their vtable construction.

**WP2 — metor-expr pack backend (the heart).** Prelude links stripped
`metor-fsw-ring`; pack-ABI exports as adapters over static buffers
(D2); baked `PackManifest`; guest-side run rule + rate ticks (D3);
no panicking paths; determinism pin (compile twice, byte-equal);
expr-ABI exports and all 131 existing tests untouched. Verify the
seq-fixture-style host tests can open/bind/execute a compiled Python
module via `WasmPack` directly.

**WP3 — Build-time compile.** Provision arm with `ArtifactKind`
dispatch + dl-arm kind guard (D4); build-time resolver from decoded
pack manifests; `.manifest` sidecar; bundle member-naming wasm arm;
`target.py`-line diagnostics; `--no-build` path.

**WP4 — The wired wasm arm.** `SystemBind::Wasm`/`WasmReg`, resolve
dispatch, `Mount::Wired` bind (no tail, no padding, descriptor's own
delivery list), thin `CyclicSlot`, fault policy (D8), `@rng` param
seeding (D9), edge synthesis from the expr manifest (D7), ordering
(D7). Pinned test on port order through the bridge.

**WP5 — Panel + example + differentials.** Canvas renders vehicle
Python systems read-only with source from `ProgramSpec`, position
from IR layout. `examples/adcs-fsw2/target.py` gains a real Python
system off the IMU. Integration test: eval target → provision
(compile) → init → run cycles → telemetered values vs the nox oracle.
**Bit-parity differential**: same module, same input samples, panel
host (expr ABI) vs vehicle host (pack ABI) produce bit-identical
output frames — now also proving the two export families agree. Fault
path: fuel-exhausting system degrades health, vehicle keeps cycling.
Slot smoke test: the same artifact mounted as a slot occupant loads
and steps through the existing runner. Measure artifact size
(prelude + ring vs the 307 KB core-based fixture) and record it.

Gates: WP1 alone (IR contract, small). WP2 alone (compiler-side,
self-contained, its own tests). WP3+WP4 together behind the
integration test. WP5 last. Commit at each gate.

## Decided questions (user, 2026-08-24)

**Q1 — Resample stages: rejected this phase** (build-gate diagnostic,
markers not exported). **Q2 — Runtime source replacement: deferred**
(no transport exists; slots give occupant-swap without it; compiler
stays ground-side). **Q3 — `rate=` must divide `cycle_rate`: hard
error.** **Q4 (rev 2) — pack-ABI unification over an expr-ABI host:
directed by the user**; the panel keeps the expr export family
(decision D2).

## Risks

- **The guest ring implementation is the safety-critical center.**
  Linking real `metor-fsw-ring` avoids a second implementation of
  SeqCst fencing; if it can't be carried into the prelude, the
  hand-written fallback is small but must be reviewed as
  concurrency-critical code. Either way the WP2 host-side tests
  exercise real pump traffic, not mocks.
- **Determinism is now load-bearing twice** (entry_identity reloads,
  manifest hashing). The compile-twice pin plus a
  hash-stability test across a process restart guard it.
- **The multi-field frame path gets its first real exercise** (the
  panel hard-errors on ≠1-field frames; on the vehicle every host
  frame is multi-field). The WP5 differential is the guard.
- **Baked-manifest drift**: the pack manifest is compiler-emitted
  bytes while the type it must decode to lives in
  `metor-fsw-2-core`. A round-trip test (compile → `WasmPack::
  read_manifest` decodes → descriptor equals the expr manifest's
  view) pins the contract; ABI version equality (11) is the tripwire
  for divergence.
- **Fuel-exhaustion detection is a string match** in the host
  (`is_out_of_fuel`); inherited, not worsened — noted so nobody
  "fixes" a fault-path test around it.

## Out of scope

- Sequences (Phase 4 — now with its substrate pre-paid).
- State carryover across occupant swaps (future pack-ABI addition,
  shaped like `metor_expr::state`).
- Live source uplink / file transfer to the vehicle.
- `wasm32` targets in `pack_dist` wheels (deferred in
  `wasm-occupant.md`; unchanged).
- Editing vehicle Python systems from the canvas.
