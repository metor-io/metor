# metor-expr Phase 1 — systems in the panel, text-first

Parent design: `python-expressions.md` (revision 3). Phase 0 landed the
compiler core (`metor-expr-phase0.md`, Results section): the scalar and
tensor subset compiles to zero-import wasm in 136 µs at ~5 KB, verified
against native nox. This phase adds the *system* layer of the language
and runs it live in the panel. The graph view stays **read-only**; canvas
editing is Phase 2, FSW is Phase 3.

Carried in from Phase 0's findings:

- Open-code small constant-shape elementwise ops in the emitter; the
  general kernels pay broadcast machinery per call (752 vs 14 fuel on a
  length-3 add). Kernels remain for large shapes and contractions.
- `dot` is a sequential non-fused sum by definition (wasm has no scalar
  FMA); the harness compares contractions accordingly.
- Host-side reference: nox for what nox has, libm directly for
  `exp`/`log`/`tan`/`pow`/hyperbolics.

## Scope

Language: `class X(Frame)` / `class S(State)` declarations, `@system`
in all three forms (bare, `bind=`, positional component paths, `on=`),
top-level bindings as anonymous systems, bare-expression compilation for
`=` fields. Runtime: the region ABI completed (describe, state slots),
the run rule, named-state snapshot/restore. Panel: a program pane, `=`
expression mode in binding fields, output frames as db components,
read-only projection in the node editor.

Out of scope: canvas gesture editing, `NodeSpec` deletion and preset
migration (Phase 2), FSW `ExprSystem` (Phase 3), windowed/stateful
prelude functions beyond `delta`/`lowpass` (added when a body needs
them).

## Name resolution stays out of the compiler

The compiler cannot know what `adcs.omega_b` is; only a host can. So
`metor-expr` gains a resolver boundary instead of a database dependency:

```rust
pub trait Resolver {
    /// Full dotted path -> schema, if that component exists.
    fn component(&self, path: &str) -> Option<CompSchema>;
    /// Bare-name suffix candidates, for one-liners.
    fn suffix(&self, name: &str) -> Vec<String>;
    /// Frame name -> field list, for `bind=` checking.
    fn frame(&self, name: &str) -> Option<FrameSchema>;
}

pub fn compile_module(src: &str, r: &dyn Resolver) -> Result<Program, Diagnostics>;
pub fn compile_expr(src: &str, r: &dyn Resolver) -> Result<Program, Diagnostics>;
```

The panel implements `Resolver` over the db vtables; tests implement it
over a literal table; FSW (Phase 3) implements it over the frame
registry. Resolution happens once, at compile time — the `Program`
manifest records *resolved* full paths, which is the design doc's rule
that the suffix trick never outlives authoring.

`Program` extends Phase 0's `Module`: per-system port frames (name,
fields, schemas, resolved bindings, driving input), state fields with
defaults, and the compiler version. Encoding for the wasm-embedded copy
(`expr_describe`) is postcard, matching the pack manifest's habits; the
host-side struct is authoritative in this phase and the embedded copy is
asserted equal in tests — FSW consumes it for real in Phase 3.

## The region ABI, completed

Phase 0 landed `<name>_arg_ptr(i)` / `<name>()` / `<name>_ret_ptr()`.
This phase makes it the ABI from the design doc:

```
expr_abi_version()                  -> i32
expr_describe() / expr_manifest_ptr()        // postcard Program manifest
<system>_arg_ptr(i)  -> *mut u8              // input frame i, laid out per manifest
<system>_ret_ptr()   -> *const u8            // output frame
<system>_state_ptr(i)-> *mut u8              // state field i
<system>_eval(now: i64) -> i32               // 0 ok, else fault code
```

State fields are initialized from their annotated defaults by generated
code on first call (a guard flag in the state area), so instantiation
needs no host-side init walk. Snapshot/restore is host-side byte copies
through `state_ptr`, keyed `(system, field, type)` per the design doc —
restore happens before the first `eval` of a rebuilt instance and skips
any triple that no longer matches.

## Panel integration

All construction on the `DynamicWorker` thread, as today's ops.

- **`dynamic/ops/program.rs`** — one streaming task per compiled system:
  subscribe to the driving input's ring, `latest()`-read the others
  (each input holds its most recent sample bytes; a system whose
  non-driving input has never published skips the cycle, the Rust
  `else return` idiom), write args, call `eval` under a fuel budget,
  publish the return frame. Output frames register as db components
  through the `Persist` machinery (component ids `<system>.<field>`,
  `source=dynamic` metadata), so plots and alarms see them with no new
  code. A trap or fuel exhaustion parks the system in an error state
  surfaced on the pane; it does not tear down the module.
- **Rebuild** — per-system content hash (source region + resolved
  bindings): an edit recompiles the module but only replaces instances
  whose hash changed, restoring state slots across the swap. This is the
  registry's existing dedup contract at function granularity.
- **Program pane** — a new tile (registered like `NodeEditor`): a text
  editor over the module source, compile-on-debounce (200 ms), span
  diagnostics rendered inline, per-system status rows (running / error /
  fuel). Source lives in the pane's serialized state, like
  `NodeEditorConfig` does today. No projection into the node canvas yet
  beyond the read-only milestone below.
- **`=` fields** — the component picker (the machinery behind
  `inspector_rows` component pickers and plot series add) gains
  expression mode: a leading `=` switches search to expression, compiled
  via `compile_expr` against the db resolver, running as a view-owned
  system whose output feeds the view's stream through the existing
  `ComponentStreamBuilder` bridge — no db registration, content-hash
  dedup across the layout, serialized in the view's state. First
  consumers: plot series and monitor bindings; the rest follow the same
  path afterward.
- **Read-only projection** — the node editor renders a program (from a
  program pane in the same layout) as a non-editable graph: one card per
  system, sockets from the manifest, edges from frame-name matches,
  positions auto-laid-out and stored in the pane state. This proves the
  projection rule on real programs before Phase 2 makes it writable.

## Milestones

Committed at each boundary, `cargo test -p metor-expr` (and
`-p metor-panel` where touched) green.

**P1 — small-shape open-coding.** The Phase 0 finding, first, while the
emitter is small: constant-shape elementwise ops emit loops directly;
kernels only for contractions and large shapes (threshold from the M4
table, recorded in the code). The M4 fuel numbers for `sum(a+b)` at
length 3 must drop to open-coded levels.

**P2 — the language layer.** `Frame`/`State` classes, `@system` all
three forms, `on=`, top-level bindings, `compile_expr`, the `Resolver`
boundary, manifest with resolved bindings. Differential harness extends
to whole systems (host feeds frames, compares output frames against a
nox-computed reference).

**P3 — the ABI layer.** `expr_describe` embedding + assert-equal test,
state defaults + guard init, snapshot/restore helpers
(host-side, in metor-expr so both hosts share them).

**P4 — panel runtime.** `program.rs` op + rebuild-with-state + Persist
registration; program pane tile with diagnostics; integration test at
the worker level (feed synthetic rings, assert published frames).

**P5 — `=` fields.** Expression mode in the picker for plot series and
monitor; view-owned lifecycle + dedup; resolution-recorded-at-authoring
behavior pinned by a test (adding an ambiguous component later must not
break a saved layout).

**P6 — read-only projection + measurements.** The projection milestone
above, plus a results section here: end-to-end latency from keystroke to
updated plot, rebuild-with-state timing, per-sample overhead of a
three-system chain vs the equivalent three legacy nodes.

## Risks

- **The run rule meets real rings.** `latest()` on a disruptor ring from
  a task that is not its subscriber needs care (the panel's rings are
  fan-out streams, not current-value tables); P4 may need a small
  current-value cell per non-driving input, filled by a cheap subscriber
  loop. Decide in P4, record the shape chosen.
- **Editor-in-a-tile.** The program pane needs a usable multi-line code
  editor in gpui. The panel has text inputs but nothing editor-grade;
  if a real editor widget balloons, P4 falls back to an external-file
  workflow (edit in $EDITOR, pane watches and reloads) and the pane
  shows source read-only — the runtime work is unaffected.
- **Two sources of truth during transition.** Legacy node graphs and
  programs coexist until Phase 2 migrates presets; they interact only
  through db components (a program can consume a `Persist`'d node output
  and vice versa). No shared state, no adapters.

## Results

### P1 — small-shape open-coding

The IR gained one field, `Emit`, on each tensor operation
(`Elementwise`, `TensorNeg`, `Dot`, `MatMul`, `Sum`). The checker sets it
by counting the scalar element operations the op needs, against
`OPEN_CODE_MAX_OPS = 128` in `check.rs` — the largest length M4 priced,
and chosen for emitted bytes rather than for a crossover, because M4
found none. Codegen runs the kernel's broadcast odometer at emit time
(`broadcast_pairs`) and leaves loads, stores, and instructions behind;
`k_pow` and `k_atan2` open-code to one scalar `pow`/`atan2` call per
element. The call-graph walk sees the choice, so an open-coded module
reaches no kernel at all.

**The number the milestone asked for**, `--release`, Apple silicon:

| length-3 `sum(a + b)` | ns/eval | fuel/eval |
|---|---|---|
| before (kernels) | 829 | **752** |
| after (open-coded) | 55 | **21** |
| the same arithmetic written out by hand | 47 | 14 |

The remaining 7 fuel over the hand-written form is the intermediate
`a + b` buffer, which is a store and a load per element; fusing that away
is an optimizer question, not this milestone's.

The sweep past the threshold confirms the kernels still earn their keep
where the checker leaves them:

| `dot` | natural form | written out |
|---|---|---|
| length-3 | 15 fuel | 14 fuel |
| length-8 | 35 | 34 |
| length-32 | 131 | 130 |
| length-128 | 515 | 514 |
| length-256 (kernel) | 2578 | 1026 |

Module size fell with it: a module reaching only elementwise tensor
kernels went from 6,410 to **5,198 bytes**, because it now reaches none.
Compile latency is unmoved (147 µs one-liner, 386 µs 100-line).

`negation_and_powers_run_through_kernels` is renamed
`negation_and_powers_agree_with_nox` — at length 3 it no longer does —
and `small_shapes_open_code_and_large_ones_call_kernels` pins the split
in both directions plus the agreement across it.

### P2 — the language layer

`lang.rs` is the new front half: it reads classes and decorators and
reduces all three tiers to one `SystemDecl`, so nothing downstream can
tell which tier wrote a system. `resolve.rs` is the whole of the
dependency on a host — `component` / `suffix` / `frame`, asked once.
`manifest.rs` carries what a host needs to drive a module and is the
type both `expr_describe` and the panel read.

Decisions taken while implementing, none of them departures from the
plan but all of them things the plan left open:

- **A frame's name is the snake case of its class, for output frames as
  well as ports.** Edges are recovered from names, so a system returning
  `Imu` and a port typed `Imu` have to arrive at the same string from
  the same class. `bind=` overrides the port's side only, which is
  exactly what makes it a rebinding.
- **A port reading an earlier anonymous binding gets its frame from the
  checker, not the frontend.** `biggest = scaled[0] + scaled[1]` needs
  `scaled`'s type, and `scaled`'s type is whatever its body computes —
  which is not known until it has been checked. So `PortDecl::frame` is
  optional and the checker fills it from the producer it has already
  finished. Bindings may only reference *earlier* declarations, so the
  producer is always available.
- **Every field occupies eight bytes.** A frame is one block the host
  addresses with `<system>_arg_ptr(i)`, with each field at a constant
  offset; keeping every element eight-aligned means no host reasons
  about packing and no `f64` load straddles. A `bool` uses the low four.
- **`now()` is a builtin, not an IR node.** A system's wasm signature is
  `(i64) -> i32`, so `now` is local 0 and `now()` reads it. Outside a
  system it is a diagnostic.

Three Phase 0 rejection tests changed message, all because module level
now holds more than `def`: `import math`, a bare `class A`, and `a.b`
in a body (which is now "`a` is not a frame here"). 83 tests green.

### P3 — the ABI layer

`expr_abi_version()` / `expr_describe()` / `expr_manifest_ptr()`, all
`i32` — addresses are `i32` on wasm32 and a manifest is far short of
2 GB, so an `i64` length would be ceremony. The manifest rides as a
postcard data segment, and `the_embedded_manifest_equals_the_host_side_one`
asserts the two are the same object for a plain module, a stateful
system, and a bare expression.

**One extension to the plan's guard mechanism, because the plan's
version has a hole.** The plan says state seeds itself on first call
behind a guard flag, and separately that restore happens before the
first `eval` of a rebuilt instance. Those two cannot both hold: the
first `eval` would seed its defaults straight over what restore just
wrote. So the guard is addressable — it is slot `state.len()` of
`<system>_state_ptr(i)`, one past the state fields. A host that restores
a snapshot writes the guard with the same byte copy it uses for
everything else, and the seed code never runs. `state::guards` names
those slots; `a_changed_triple_resets_that_field_and_nothing_else` and
`state_survives_a_rebuild_that_keeps_the_triple` pin both directions.

Zero defaults emit no seed instructions at all, since a fresh linear
memory already holds them — so the common `= 0.0` costs only the guard
check (about 4 fuel per evaluation).

`state.rs` holds the keying and the matching rule and touches no wasm
instance: it hands back slot indices to pass to `<system>_state_ptr(i)`
and byte counts, because both hosts already own an instance and read
memory their own way. 89 tests green.

### P4 — panel runtime

`dynamic/ops/program.rs` is the whole runtime: `Compiled` (a program plus
the wasmi module it instantiates), `system` (one node per `@system`), and
`field` (one node per output field). `dynamic/resolver.rs` answers the
compiler's three questions from a snapshot of the db's component
metadata.

**The first risk, resolved without its fallback.** The plan expected
`latest()` on a disruptor from a task that is not its subscriber to need
"a small current-value cell per non-driving input, filled by a cheap
subscriber loop". It does not, and the shape it takes instead is
strictly smaller: a disruptor has no `latest()` at all, but it has
`try_next()`, and `resample.rs` already uses it to keep the newest
sample of a secondary input. So each system's own task holds one reader
per port and drains the non-driving ones with `try_next` on the way past
the await. **One task, one reader per port, no shared cells, no second
loop** — the fallback's cost (an extra task and a lock per input) buys
nothing the plain drain does not already give. A port whose cell is
still empty skips the cycle, which is the run rule's `else return`.

Decisions the plan left open:

- **A snapshot resolver, not a live one.** Compilation happens off the
  UI thread while the db keeps moving; a resolver holding the state lock
  would put the two in each other's way for as long as a parse takes.
  Snapshotting also makes resolution *reproducible* — every name in one
  compile sees the same tree, so a component appearing mid-parse cannot
  make two halves of an expression disagree.
- **Everything numeric reads as `f64`.** A component's element type is
  not the language's: `f32`, `i32`, and `u16` channels all widen on the
  way into a frame, and only `bool` stays itself. This is the panel's
  existing convention (`dynamic/tensor.rs` computes in `f64` and casts
  at write time), and it means one expression can span a float sensor
  and an integer counter without saying so.
- **The system node carries frame *bytes*; fields hang off it.** A frame
  is several fields of several types and no single `ComponentSchema`
  describes it honestly, so the system's ring is `U8[frame.bytes]` and a
  `field` node per output field re-reads it with the schema that field
  really has. Publishing then needs no new machinery at all: a field
  node is an ordinary value node, so `persist` registers it as
  `<system>.<field>` exactly as it registers anything else.
- **State is published, not fetched.** A rebuild cannot reach into a
  spawned task, so rather than ask it, the task writes its state slots
  into a cell the node owns after every evaluation — a few words, always
  current. A rebuild reads that cell and hands it to the new instance.
- **Compilation happens before the worker closure.** `WorkerHandle::run`
  blocks the UI thread until the closure returns, so the closure does
  nothing but instantiate; the wasmi module is built on the debounce
  task, behind the 200 ms window.
- **A fault parks, and keeps reading.** A system that traps or burns its
  grant stops evaluating, records why for the pane, and keeps draining
  its inputs — a reader that stops moving would make its *producer* drop
  samples, so parking silently would damage everything upstream.
