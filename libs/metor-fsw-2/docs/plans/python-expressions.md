# Python systems over frames — the node canvas as a projection of a program

Status: **revision 4; Phases 0–1 are built** (`metor-expr-phase0.md`,
`metor-expr-phase1.md`, both with Results). Revision 1 proposed a WASM
expression *node* inside the existing graph runtime. Revision 2 inverted
it: the graph became a Python program projected onto the canvas.
Revision 3 changed the language's unit from bindings to **decorated
functions over frames**, mirroring how metor-fsw-2 systems are written.
Revision 4 widens the target: the unified canvas replaces not only the
node editor but also the **system graph view** — the panel gets one
graph surface for systems over frames, whatever their source — and
layout moves *into* the source file via a presentation decorator.

## Why

Two features grew toward the same hole from opposite sides:

- **metor-fsw-2** runs sandboxed WASM occupants in slots — fuel-metered,
  fault-contained, closed modules (`docs/plans/wasm-occupant.md`). But
  authoring one still means writing a Rust pack and cross-compiling it.
  The substrate is settled; the authoring language was explicitly deferred.
- **metor-panel** has the dynamic node system — 21 op kinds composed on a
  canvas, each a streaming task over disruptor rings. It is good for
  pipelines and bad for arithmetic: `(a*9.81 + b)/2 clamped above 500`
  is five nodes and eight edges where it should be one line.

The position this document takes: **the program is Python source, and the
canvas is a projection of it.** One artifact that is diffable, reviewable,
pasteable into a REPL, promotable to FSW whole, and equally editable as
text or as boxes and wires.

Python is the right surface. It is already the ops-facing language of this
codebase (`target.py`, the pack build backend, stubgen), and a typed
subset of it is exactly what operators write anyway.

## Prior art

### Compiling Python to WASM

The field splits into three shapes:

**Compile all of Python.**
[py2wasm](https://wasmer.io/posts/py2wasm-a-python-to-wasm-compiler)
(Nuitka → C → wasm32-wasi) embeds the CPython runtime: multi-megabyte
modules, WASI imports, a Python 3.11 toolchain at build time. Built for
porting applications, not for kilobyte expressions.

**Ship an interpreter as the guest.**
[MicroPython's wasm port](https://github.com/rafi16jan/micropython-wasm)
(~300 KB, Emscripten imports) or RustPython (wasm32-wasi) inside the
module, with the expression as data. Full Python semantics, but the module
stops being a closed artifact (both need host imports), fuel maps to
interpreter dispatch rather than user work, and — decisive for the panel —
there is no static output type: every consumer needs each derived value's
`ComponentSchema` (dtype + shape) before anything runs, and an interpreter
can only tell you by running.

**Compile a typed subset.**
[SPy](https://github.com/spylang/spy) is the strongest statement of the
premise: modern, type-annotated Python is a statically compilable language,
and the dynamic parts people are told not to use are the only parts that
prevent it. Active through 2026, MIT, PyCon-visible. But its pipeline is
Python 3.12 → C → Emscripten/Zig — a subprocess toolchain we cannot embed
in the panel's 200 ms rebuild loop. [Codon](https://github.com/exaloop/codon)
(LLVM) still lists its wasm backend as roadmap.
[Waspy](https://lib.rs/crates/waspy) is the existence proof for the shape
we actually need: a pure-Rust crate, `rustpython-parser` → typed IR →
`wasm-encoder`, actively developed (0.14, Aug 2026), MIT. It ships far
more language than we want (strings, dicts, generators, a never-freed
heap, host file I/O) and its allocation semantics are wrong for a
per-sample hot loop, but it demonstrates that an embeddable Python-to-wasm
compiler is a tractable amount of Rust.

**Non-Python alternatives, and why not:** AssemblyScript/MoonBit produce
excellent small wasm but drag in a Node or external toolchain and a second
language beside `target.py`; Lua/Rhai/CEL/evalexpr are host-side
interpreters — two runtimes to keep in agreement, no uplinkable artifact,
no fuel story, and CEL-class languages have no loops or state; Starlark
has the right syntax but is interpreter-only, with the same no-artifact,
no-static-schema problems.

### Visual editors as frontends for text

Sorted by one question: *which representation is the source of truth?*

- **[Enso (formerly Luna)](https://medium.com/@enso_org/luna-the-future-of-computing-aaf4f76303ef)**
  is the direct precedent: dual syntax representation, where **the storage
  format is text plus (x, y) position markers**. The graph is fully
  recoverable from source; text tools (diff, git) work on programs built
  visually. Their devblogs are also the cautionary tale: the text↔visual
  sync module was their
  [long-running bug farm](https://medium.com/@enso_org/enso-dev-blog-19th-june-2020-335e528d50b).
- **Blockly / Scratch** generate text one-way; **Unreal Blueprints,
  LabVIEW, TouchDesigner** make the graph primary with code as the escape
  hatch. One-way systems all rediscover the same problem: the generated
  text is not the artifact anyone maintains.
- **JetBrains MPS** (projectional editing) proves the general form works
  but at the cost of abandoning plain text files entirely.

The Enso lesson, distilled: round-tripping is tractable iff the
projectable surface is *declaration-shaped* — a flat set of named units
whose connections are recoverable from names — with layout out-of-band.
Arbitrary top-level control flow is what breaks the isomorphism.
Revision 2 met that with one-binding-per-node; this revision meets it
with one-function-per-node, which is coarser, sturdier, and — decisively —
already how the rest of metor is shaped.

## The shape: systems over frames

### The discovery that collapses the design

metor-fsw-2 already has the two concepts this language needs, and they are
better than the ones revision 2 invented:

- A **frame** (`docs/frames.md`) is a named record of values sharing one
  timestamp. Its `FRAME_ID` is what wiring uses to match ports; its fields
  carry dotted component ids (`imu.omega`) that telemetry, storage, and
  every panel view already address.
- A **function system** (`docs/system.md`) declares its ports through its
  argument types — `Input<Imu>`, `Output<Estimate>` — and the driver runs
  it cyclically, reading `latest()` from each input.

So the Python surface should not invent a dataflow vocabulary of
`source`/`persist`/stream combinators, as revision 2 did. It should mirror
the Rust form: **a frame is a class, a system is a decorated function, the
signature is the wiring surface.**

```python
class Imu(Frame):
    omega: Tensor[f64, 3]
    accel: Tensor[f64, 3]

class RateEstimate(Frame):
    rate: f64
    rate_lp: f64
    flagged: bool

class LpState(State):
    rate_lp: f64 = 0.0

@system
def rate_watchdog(imu: Imu, wheels: Wheels, state: LpState) -> RateEstimate:
    rate = sqrt(imu.omega[0]**2 + imu.omega[1]**2 + imu.omega[2]**2)
    state.rate_lp = 0.2 * rate + 0.8 * state.rate_lp
    return RateEstimate(
        rate=rate,
        rate_lp=state.rate_lp,
        flagged=wheels.rpm > 500.0,
    )
```

Every piece corresponds one-to-one with the Rust function-system form:
frame parameters are input ports (with `latest()` semantics), the return
frame is the output port, the `State` parameter is the init-constructed
state struct, and the timestamp is implicit (each frame carries its own;
`now()` is available in the body). The output frame's fields *are*
derived telemetry — `rate_watchdog.rate_lp` is a component id the moment
the system exists, addressable by every plot, monitor, and alarm. There is
no `persist`: publishing is what returning means. There is no `source`:
binding is what the signature means.

### Two binding forms, one mechanism

The signature names frame *types*; binding attaches them to concrete
telemetry. Two forms:

**Frame binding (canonical).** By default a parameter typed `Imu` binds to
the frame named `imu` — same rule as Rust wiring's `FRAME_ID` match. To
run the same function against a different concrete frame, bind it
one-to-one:

```python
@system(bind={"imu": "adcs_imu_b"})
def rate_watchdog(imu: Imu, ...) -> RateEstimate: ...
```

The map is field-by-field — names and types must match one-to-one, or an
explicit field map is given. This is what makes functions *reusable*: one
`lowpass` or `dead_band` definition instantiated against N frames, the
binding living in the decorator, in `target.py`, or in the panel
inspector — not in the body.

**Component-path binding (sugar, for quick expressions).** The panel case
is "pick two channels, write one line." Positional component paths bind
parameters directly, and the parameters need no annotations — their types
come from the components' schemas:

```python
@system("adcs.omega_b", "wheels.rpm")
def rate(omega_b, rpm) -> f64:
    return sqrt(omega_b[0]**2 + omega_b[1]**2 + omega_b[2]**2)
```

This is not a second mechanism. Every dotted component id is already a
field of some frame, so a path binding is a *projection* of existing
frames onto an anonymous single-field view, and a scalar return is an
anonymous single-field output frame named after the function. The sugar
desugars to the canonical form; the canonical form is what FSW accepts.

### One-liners: every expression is a system

The `def` form is right for real computations and wrong for
`omega_b * 100` — nobody should write a signature to scale a channel for
a plot. The resolution is not a separate expression feature; it is a
third entry point to the same construct, resting on a happy accident of
syntax: **component paths are already valid Python.** `adcs.omega_b`
parses as attribute access, so

```python
adcs.omega_b * 100
lowpass(sqrt(adcs.omega_b[0]**2 + adcs.omega_b[1]**2), 0.2)
```

are complete programs in the expression subset. A bare expression
desugars to an anonymous path-bound system: its free component references
become the parameters, the expression becomes the return, the driving
input is the first channel referenced, everything else reads latest.
Same checker, same broadcast rules, same wasm module, same fuel — a
one-liner is a `@system` that never needed a name.

Bare (undotted) names resolve against the component tree by unique
suffix — `omega_b * 100` works when exactly one component ends in
`.omega_b`, and an ambiguous name is a diagnostic listing the candidates.
In practice the field's autocomplete (the existing component picker
machinery) makes collisions a non-event. Stateful prelude calls work in
one-liners too, since their buffers are state slots; an anonymous
expression keys its state by content hash, so editing it resets its own
filter and nothing else.

One-liners get two homes, one per audience:

- **Any binding field in the panel.** Component pickers gain an
  expression mode with the spreadsheet convention: type `=` and the
  search field becomes an expression field (`=adcs.omega_b * 100`). The
  expression compiles into a view-owned ephemeral system — serialized in
  the view's own state like any view setting, deduplicated across the
  layout by content hash, and backed by a *hidden* db component named by
  that hash (Phase 1 finding: plots and sparklines read component
  history, not live streams, so ephemerality is delivered by hiddenness
  and reference-tied lifetime rather than by non-registration). Every
  plot, monitor, table, and traffic light gains computed channels
  without any of them learning what a node is. (Houdini is the
  precedent: the same language in a parameter field as in a full
  wrangle node.)
- **Top-level bindings in the program module.** `rate_x100 =
  adcs.omega_b * 100` declares an anonymous system named by the binding
  — its output *is* the component `<module>.rate_x100`. Bindings may
  reference earlier bindings, which is just a frame edge between the
  desugared systems. On the canvas these project as compact expression
  cards: no signature chrome, just the text. (Revision 2's
  one-binding-per-node surface survives here, demoted from the language's
  foundation to its sugar tier — which is where it belonged.)

The three tiers are one language with a promotion gradient: an `=`
expression in a plot can be lifted to a named module binding (it gains a
component id and appears on the canvas), and a binding to a `@system`
with declared frames (it gains reusability and an FSW future). Each lift
is copy-paste — the text means the same thing at every tier because every
tier desugars to the same anonymous-system form.

### Frames dissolve the clock problem

Revision 2 threaded clock identity through the type system to enforce the
panel's co-clock rule at compile time. Frames make most of that machinery
unnecessary, because **a frame is already the co-timestamp unit**: fields
read from one parameter are one sample, aligned by construction.

Across parameters, the run rule is the FSW one, not a new one:

- The system fires when its **driving input** publishes — by default the
  first parameter, override with `@system(on="wheels")`.
- Every other input supplies its **latest** sample, exactly like
  `input.latest()` in a Rust system. Reading rate-mismatched inputs is
  therefore well-defined by default (it is zero-order hold), not a type
  error demanding an explicit resample node.
- In FSW the driver is the cycle itself, and every input is `latest()` —
  the same semantics with the clock supplied by the coordinator.
- A system with nothing to wait on supplies its own driver:
  `@system(rate=100.0)` declares it **source-clocked**, and the host gives
  it a timer at that rate. `rate=` and `on=` are mutually exclusive — a
  system is either source-clocked or input-driven, and saying both is a
  question with no answer. Everything else is unchanged: a source with
  inputs still holds their latest and still skips a cycle whose inputs are
  unknown, so a source *without* inputs is a generator and a source *with*
  them is exactly the FSW cyclic shape. That is also the Phase 3 mapping:
  `rate=` becomes cyclic scheduling and the coordinator's clock replaces
  the panel's timer, with no change to what the body means.

  Sources are where test signals come from now that generators are not node
  kinds: `sine`/`cosine`/`square`/`sawtooth` are pure functions of `now()`,
  `constant(v)` is `v`, and `random()` draws from a splitmix64 word kept in
  a state slot the host seeds at instantiation — so it is reproducible under
  test, varying in use, and continuous across an edit like any other state.
  The waveform kind is four names rather than one function's string
  argument because the subset has no strings.

The panel's explicit `Resample{Zoh,Linear}` ops survive for when
interpolation actually matters — but *not* as prelude functions, and this
is the language's one deliberate exception. Resampling changes which clock
a value ticks on, so it is scheduling rather than arithmetic, and a guest
that could reschedule itself would need a timer inside the sandbox — the
one thing the sandbox exists to not have. So a **top-level binding whose
right-hand side is exactly a resample call** (`slow =
resample_zoh(fast, 10.0)`) is recognised as a host-wired stage: it is not
compiled at all, it publishes under its binding name like any other
declaration, and what reads it is an ordinary edge. The call is refused
anywhere else, with a diagnostic that says where it belongs. What remains
static and checked: dtype and shape inference over bodies, reusing the panel's
broadcast rules (`dynamic/tensor.rs`), so the checker accepts
exactly what the runtime does. Windowed prelude functions with static
bounds (`window(x, 256)`, `fft`, `delta`, `lowpass`) are stateful
callables whose buffers live in the system's state — fixed-size, no
allocation — so even shape-changing pipeline stages move *into* bodies
rather than remaining separate nodes.

### Nox is the math backend

Components are tensors already: a frame's tensor field travels as bytes +
shape, which is precisely `nox_array::ArrayView`, and the workspace's
numerics — ADCS math included — are written against nox's tensor API. So
the language does not define its own numeric tower; it adopts nox's:

- **Language types are nox types.** A scalar is a rank-0 tensor;
  `Tensor[f64, 3]` is `nox::Vector<f64, 3>`; the checker's broadcast and
  promotion rules are nox's. This settles integer semantics as dtype
  semantics — ints are `i64` and wrap, as a typed tensor element does,
  rather than trapping or pretending to be Python bignums — and makes
  native nox the reference implementation the compiler is tested
  against.
- **Guest kernels come from nox itself.** nox is `no_std` (libm
  included) and `Dyn`-dim arrays participate in its op machinery, so a
  small `no_std` prelude crate wrapping nox's dynamic-shape kernels and
  libm compiles once to `wasm32-unknown-unknown` and is checked in as
  bytes. The compiler emits generated functions *into* that template
  module — scalar math as native wasm opcodes, tensor ops as calls into
  the spliced nox kernels with compile-time-constant shapes, so no guest
  allocator is needed. Panel-side native nox and guest-side wasm nox are
  the same code, which is what makes "identical results in both hosts"
  a property of the build rather than a promise.
- **The `noxpr` graph layer is out of bounds.** It is unwired from the
  crate today and slated for removal; nothing here may depend on it. Nox
  enters as a kernel library and a semantic reference, never as an IR —
  the compiler's own typed IR stays the only program representation.
- Later, nox's quaternion/spatial/integrator types are the obvious
  source for flight-flavored builtins (`quat_mul`, frame rotations),
  keeping the language's vocabulary identical to the Rust systems'.

### Projection: one system = one node

- A node is a decorated function. Its input sockets are its parameters;
  its output socket is its return frame.
- Edges are recovered from names, the same way wiring recovers ports: `A`'s
  output frame feeding `B`'s parameter *is* the fact that they name the
  same frame. Connecting an edge on the canvas is a **rebinding** — the
  gesture rewrites `B`'s `bind` map to point at `A`'s output, one-to-one.
- Path-bound parameters render as the node's channel pickers (the existing
  component-picker rows); frame classes render as typed sockets.
- Adding a node from the palette inserts a decorated function with a fresh
  name; palette entries are prelude functions and user-defined functions
  in the module. Editing a node's body is editing text in the card (or
  jumping to the text pane).
- Renaming a node renames the function; deleting it deletes the function;
  downstream frames that lose their producer surface as ordinary
  "unbound input" diagnostics on the affected nodes.

Layout goes **in the source file**, as a presentation decorator stacked
on the declaration (revision 4, reversing revision 3's sidecar):

```python
@node(x=240, y=120)
@system
def rate_watchdog(imu: Imu, ...) -> RateEstimate: ...
```

`@node` is optional — an unannotated system gets deterministic
auto-layout — and carries presentation only; the compiler ignores it
beyond parsing. Dragging a card on the canvas rewrites exactly that
decorator line. What this buys: the file is fully self-contained (share
the `.py` and the diagram travels with it), and a whole risk class dies —
there is no sidecar key to fall out of sync, and renaming a function
cannot orphan its position because the position is attached to the
declaration, not keyed by its name. What it costs: position edits appear
in diffs. That churn is confined to `@node(...)` lines and reviewers
learn to skim them, which is a better failure mode than a layout that
silently detaches. One consequence is accepted deliberately: a program
has **one canonical layout**, not one per dashboard — right for a wiring
diagram, which is documentation, not decoration. (Enso reached the same
place with a metadata footer; a stacked decorator is the same idea in
native Python syntax.)

The projection is coarser than revision 2's binding-per-node, and that is
a feature: the graph shows *architecture* (systems and frames), the text
shows *math* (bodies) — which is the actual complaint that started this
design, five nodes of arithmetic wanting to be one line. Text edits go
the other way: reparse, re-project, reconcile by function-level hash. The
invariant is structural: **the text is always the truth, the graph never
holds state the source can't express** — with `@node` in the file, that
now includes layout, so the projection is a pure function of source. v1
keeps the writer simple — canonical formatting for canvas-driven edits,
comments preserved by attachment to their function — not byte-level
trivia preservation.

### One canvas: the node editor and the system graph become one tile

The panel today has three graph surfaces: the node editor (editable,
panel-local dataflow), the **system graph** (`views/system_graph` — a
read-only node-and-wire view of the live target's `WiringManifest`,
already built on the shared `graph_canvas` primitives), and Phase 1's
read-only program projection. Revision 4's goal is one surface, because
they are already the same picture: **systems over frames, edges by frame
match** — the rule FSW wiring and Python programs share by construction.

The unified canvas renders one graph model fed by two sources:

- **Python systems** from program files — fully editable: bodies as
  text, bindings as gestures, positions as `@node` rewrites.
- **Native systems** from the live target's wiring manifest — structure
  read-only (their source of truth is Rust and `target.py`), rendered as
  the system graph renders them today, slots and coordinator included.

A frame published by a native system and consumed by a Python system (or
vice versa) is an ordinary edge, which is the point: the operator sees
*the target*, and some of its nodes happen to be openable as Python. Live
overlays (rates, staleness, health) apply uniformly since both sources
are db-visible. The layout principle extends to native systems as
"position lives at the declaration site": `@node` for Python systems;
for native systems the per-view manual overrides the system graph
already persists remain, until `target.py` grows a placement surface in
Phase 3+ and the wiring IR carries it. When Phase 3 puts Python systems
*on the vehicle*, the same canvas is where they are opened, edited, and
uplinked — the unification is what makes "prototype in the panel,
promote to flight" one view instead of a workflow across three.

### Execution

One compiled wasm module per system (or per program, with one export per
system — an implementation choice for the spike). The host — panel or
FSW — owns ports and rings; the guest is pure compute plus state:

- **Panel:** each `@system` becomes one streaming task on the
  `DynamicWorker` thread, subscribed to its driving input, reading latest
  from the rest, calling `expr_eval` per sample, publishing the output
  frame as db components (the `Persist` machinery, now automatic).
  Compile and type errors surface as node diagnostics through the
  existing `BuildError` path. View-owned `=` expressions run as the same
  tasks with one difference: their output feeds the view's stream
  directly (the existing `ComponentStreamBuilder` bridge) and skips
  component registration — ephemeral by default, shared by content hash
  when two views type the same expression.
- **FSW:** (revised again 2026-08-24 — no expr-shaped host either) the
  compiler emits modules that speak the **pack ABI**, so a Python
  program is an ordinary wasm pack artifact exposing N systems, driven
  by the same `WasmPack`/`RingBridge` machinery as any other wasm —
  slots and sequences included. The `expr_*` exports remain alongside
  for the panel's per-sample hosting; one module, two entry families.

Every instance runs under wasmi with fuel, in the panel exactly as in
FSW — a `while True:` burns its grant and surfaces as a diagnostic on the
node, never a stalled UI or a stalled vehicle. The wasm substrate's
Phase 0 already priced this regime: 7 ns port copies, thousands of fuel
units per math-heavy poll against budgets set orders of magnitude higher.

Editing must not reset the world: systems are hashed individually, so an
edit rebuilds one system's module while the rest keep running — the
content-hash dedup the node registry does today, at function granularity.
State survives edits through **named state slots**: on rebuild the host
snapshots the old instance's state fields by `(system, field, type)` and
seeds the new instance where the triple still matches.

### The region ABI

Same closed-artifact rule as pack modules (**no imports**), same
describe-then-read shape the pack ABI adopted for wasm:

```
expr_abi_version()          -> i32
expr_describe()             -> i64      // manifest length
expr_manifest_ptr()         -> *const u8
expr_arg_ptr(i)             -> *mut u8  // static buffer per input frame
expr_ret_ptr()              -> *const u8// static buffer, output frame
expr_state_ptr(i)           -> *mut u8  // named state slot
expr_eval(now: i64)         -> i32      // 0 ok, else fault code
```

The manifest lists the port and state frames — names, fields,
`ComponentSchema`s — plus the compiler version. It is the same descriptor
vocabulary `SystemDescriptor` already serializes, which is no accident:
describe *is* the signature.

### What this deletes

The end state removes more than it adds:

- the `NodeSpec` union (21 variants) and its serde surface — replaced by
  source text in the preset
- `hash_args`/`compute_node_id` spec-vs-constructor mirroring (and its
  pinned test) — replaced by hashing function sources
- the per-op inspector row wiring — bodies edit as text; bindings project
  from signatures
- `OpDescriptor::ALL` as a hand-maintained table — the palette derives
  from prelude and module function signatures
- `dynamic/ops/derive.rs`, `compose.rs`, and — beyond revision 2 — the
  `Persist` and `FromDb` *ops* as user-visible nodes: publishing is
  returning, sourcing is binding. What survives host-side is the genuinely
  host-shaped machinery those ops wrap (WAL adoption, component
  registration, generators for test signals).
- (revision 4) the `node_editor` module as a separate tile, and the
  program projection as a separate rendering: both fold into the system
  graph's `graph_canvas` machinery, which becomes *the* graph tile. The
  system graph is the base that survives, extended from read-only to
  editable-where-the-source-is-Python. Deleting the node editor requires
  language parity for the legacy op vocabulary first — generators,
  `window`/`fft`, `resample` as prelude functions — which is the gating
  work item, not the canvas itself.

Existing saved graphs migrate mechanically: topo-sort, emit each connected
region as a function (or one function per node for a literal first cut),
carry positions across. The converter is written once, run per preset,
and deleted with the old format.

## metor-fsw-2: the same function is a system, literally

This is where the frame-based surface pays off hardest: a Python `@system`
is not "promotable to" an FSW system — it already *is* one, in the same
shape the Rust macro produces. Ports from the signature, `latest()` reads,
state struct, one publish per execute. Written directly in the target file —
no module path, no wrapper system — the decorator *declares* it, and
`target.add` registers the instance exactly like a native pack entry
(revised 2026-08-24 twice: decorator-as-registration first, then explicit
`add` so the target file stays a manifest, step order and scope follow the
add call, and one function can someday bind several instances):

```python
@system("imu.omega_b")
def omega_norm(omega_b):
    return (omega_b @ omega_b) ** 0.5

target.add("omega_norm", omega_norm)
```

The Wiring IR carries the source; the build driver compiles it into an
ordinary wasm **pack artifact** at provision time (revised 2026-08-24 —
the same seam that builds path-source cdylibs), so a bad program fails
the build with a line-numbered error rather than a runtime fault, and
the flight binary never links the compiler at all. No per-triple
matrix, no toolchain on the target — the vehicle only ever loads wasm
artifacts through its one existing path. Frame classes declared in Python
generate real frame vtables (name, fields, shapes, component ids), so a
Python system's output is first-class telemetry; a signature can equally
name frames the Rust side already defines, checked one-to-one against the
existing vtable at init. Replacing a system at runtime is deferred
(2026-08-24): the `ExprSystem` parameter surface that would have carried
uplinked source is gone with the wrapper, a config change is a rebuild —
the normal FSW flow — and the panel remains the live-iteration surface.

The promotion path — prototype live against telemetry in the panel with
path-bound sugar, then tighten to declared frames and paste the same
function into `target.py` — is the payoff the whole design aims at, and
the sugar-to-canonical desugaring is what makes it a mechanical step.

## Staging

- **Phase 0 — the compiler spike.** `metor-expr` crate:
  `rustpython-parser` → typed IR → `wasm-encoder`. Scalars and fixed
  tensors, one function, no frames yet. Prove compile latency (budget:
  well under 200 ms) and module size (budget: single-digit KB), run under
  wasmi in a unit test. Decide build-vs-fork waspy with real information;
  current lean is build, with waspy as reference — our v1 is smaller than
  what a fork would have us maintain.
- **Phase 1 — systems in the panel, text-first.** `Frame`/`State` classes,
  `@system` with both binding forms, bare-expression desugaring, the run
  rule, the region ABI, named state slots, per-system rebuild. Two user
  surfaces: a program pane (type a module, it runs against live
  telemetry) and **`=` expression mode in binding fields** — the
  one-liner tier is deliberately in the first shippable phase, because it
  is the smallest end-to-end proof of the whole pipeline and the feature
  most people will touch daily. The graph view is **read-only
  projection** at this phase — prove the projection before making it
  editable.
- **Phase 2 — one canvas, and it writes.** Unify the node editor, the
  system graph, and the program projection into a single
  `graph_canvas`-based tile: native systems from the wiring manifest
  (read-only structure), Python systems from programs (editable).
  Gesture→AST edits (add function, rebind edge, rename, drag →
  `@node(x=, y=)` rewrite), palette from signatures, legacy-op parity in
  the prelude (generators, `window`/`fft`, `resample`), preset migration
  converter, then delete `NodeSpec` and the node editor. This is the
  Enso-lesson phase; it gets its own plan.
- **Phase 3 — FSW.** (Revised 2026-08-24: no `ExprSystem`, no
  `target.systems(Path)` — the decorator is the registration surface.)
  `@system` functions written directly in `target.py`, captured at
  config-eval time and threaded through the IR as source; init-gate
  compile; vtable checks against Rust-defined frames; real rings and
  first-class telemetry per system. Runtime source replacement is
  deferred with the parameter surface that would have carried it — a
  config change is a rebuild, and the panel stays the live-iteration
  surface. Plan: `metor-expr-phase3.md`.
- **Phase 4 (deferred, decision point) — sequences.** Python `async def` /
  generator syntax compiled to a poll-driven state machine speaking the
  full pack ABI, so uplinkable *sequences* can be written in Python.
  Substantial (a coroutine transform plus harness linkage); re-evaluate
  SPy then.

## Risks and open questions

- **The sync layer is the known hard part.** Enso spent years debugging
  text↔graph reconciliation. Mitigations are structural (text is truth,
  graph is derived, layout is sidecar, canvas edits are AST edits) and
  scoped (read-only projection ships a phase before editing does), and
  the function-level unit shrinks the surface — the canvas rewrites
  signatures and decorators, never bodies. Still the riskiest line item;
  plan Phase 2 as if it costs as much as Phases 0–1 combined.
- **Run-rule semantics need to be written down early.** "Fire on driving
  input, latest elsewhere" must specify startup (inputs that have never
  published — skip the cycle, as Rust systems' `else return` idiom does),
  and staleness visibility. These are semantics operators will reason
  about; they go in the language doc, not the implementation.
- **Two Pythons.** `target.py` runs under real CPython; systems compile
  under a subset whose numerics are nox's, not CPython's — ints are
  `i64` and wrap, there are no bignums. That divergence is documented
  and pinned by tests rather than papered over; within the subset,
  syntax and control flow mean exactly what Python means. Division,
  modulo, and overflow semantics are the places this needs pinned
  differential tests against native nox, not assumptions.
- **Frame identity and collisions.** Python-declared frames enter the same
  `FRAME_ID` namespace as Rust ones; a name collision with a mismatched
  field list must fail at init with a field-level diff, and the one-to-one
  binding checker is the same code either way.
- **Names are API.** Function names surface as node titles, output frame
  names, component-id prefixes, and state keys. Renames cascade or the
  feature feels haunted; a rename resets state only if the state *field*
  names change (state slots key on `(system, field, type)`, and the
  rename path must migrate the system key). Layout left this list in
  revision 4: `@node` rides the declaration, so a rename cannot orphan
  it.
- **Bare-name resolution is a stability contract.** `omega_b * 100`
  resolves by unique suffix *at compile time*; adding a second
  `*.omega_b` component later breaks a saved expression. Rule: the
  desugared binding stores the *resolved* full path (the expression text
  keeps what the user typed; the view state records the resolution), so
  saved layouts are immune to later ambiguity and the suffix rule only
  ever runs at authoring time.
- **Shape inference stays shallow.** Fixed shapes in types and
  static-bound windowed prelude functions keep inference decidable;
  dynamic shapes stay refused.
- **Float determinism.** Wasm float ops are IEEE-754 deterministic across
  hosts except NaN payloads; canonicalize NaNs at the ABI boundary and
  panel/FSW results are bit-identical.

## Sources

- SPy — https://github.com/spylang/spy
- Waspy — https://lib.rs/crates/waspy
- py2wasm — https://wasmer.io/posts/py2wasm-a-python-to-wasm-compiler
- MicroPython wasm port — https://github.com/rafi16jan/micropython-wasm
- Codon wasm status — https://github.com/exaloop/codon/issues/67
- RustPython wasm — https://github.com/RustPython/RustPython
- Enso/Luna dual representation — https://medium.com/@enso_org/luna-the-future-of-computing-aaf4f76303ef
- Enso sync-layer devblog — https://medium.com/@enso_org/enso-dev-blog-19th-june-2020-335e528d50b
- In-tree: `docs/frames.md`, `docs/system.md`, `docs/plans/wasm-occupant.md`
