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
