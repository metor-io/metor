# metor-expr Phase 2 — one canvas, and it writes

Parent design: `python-expressions.md` (revision 4). Phases 0–1 are
built (`metor-expr-phase0.md`, `metor-expr-phase1.md`, Results in both).
This phase unifies the panel's three graph surfaces into a single
editable canvas, reaches language parity with the legacy node ops, and
ends with `NodeSpec` and the node editor deleted.

Decisions ratified for this phase (user, 2026-08-23):

- **Full op parity before deletion**, including a `no_std`
  wasm-compatible FFT in the guest prelude.
- **`@` is the generalized tensor product**, with Python/nox matmul
  semantics by rank: rank-1 ⊕ rank-1 is the inner product
  (`[1,2,3] @ [1,2,3] == 14`), rank-2 is matrix multiplication, higher
  ranks broadcast leading dimensions and contract the last two
  (batched matmul). Mismatched inner dimensions are refused with a
  span diagnostic naming both shapes.
- **Native (Rust) systems keep panel-side manual positions** (the
  system graph's existing per-view overrides); `target.py` placement
  waits for Phase 3's IR surface.
- **Native cards are pure viewers** — structure, ports, live overlays;
  no parameter editing from the canvas in this phase.

## Milestones

Committed at each boundary; `cargo test -p metor-expr` and
`-p metor-panel` green throughout; differential tests against native
nox wherever nox has the operation.

**Q1 — the `@` operator.** Checker + both emit paths (open-coded and
kernel; `k_dot`/`k_matmul` exist since M0). Rank rules above, pinned by
a matrix of differential tests (nox `dot`/matmul as oracle at fixed
shapes; the Phase 0 finding stands — contractions are sequential
non-fused sums, compared accordingly). `dot(a, b)` remains as a prelude
alias or is removed in favor of `@` — pick one, document it.

**Q2 — source systems and generators.** A system with no inputs needs a
driver: `@system(rate=100.0)` declares a self-clocked source, hosted by
the FixedRate timer machinery the generators already use. Then
`waveform(kind, freq, amp)`, `constant(v)`, and `random()` become
prelude/state functions usable in bodies. `random()` is a small PRNG in
the guest prelude (xorshift/PCG class), its state in a state slot,
seeded by the host at instantiation — deterministic under test, varying
in use. Guests still never see a clock they didn't get from `now`.

**Q3 — window and FFT.** `window(x, N)` (ring buffer in a state slot,
emits rank-1 of the last N) and `fft(x)` over power-of-two static
shapes: a hand-rolled `no_std` radix-2 over f64 in the prelude crate,
refused with a diagnostic for non-power-of-two lengths. Oracle:
`rustfft` (already in the workspace tree) as a dev-dependency of the
harness only. Magnitude/complex layout must match what the legacy Fft
op published, so existing plots read the new output unchanged.

**Q4 — resample.** `resample_zoh(x, rate)` / `resample_linear(x, rate)`
are clock-changing, so they are runtime constructs, not body math: a
top-level binding whose RHS is exactly a resample call becomes a
host-implemented resample stage (the existing op machinery), and the
checker refuses resample calls anywhere else. This is a deliberate,
documented special case — the alternative (guest-side resampling)
would put timer scheduling in the sandbox.

**Q5 — the unified canvas.** One `graph_canvas`-based tile replaces the
node editor, the system graph, and the Phase 1 projection: native
systems from the `WiringManifest` (read-only cards, slots and
coordinator as today, per-view manual positions preserved), Python
systems from programs (editable). Gesture→AST edits: add system from a
palette derived from prelude + module signatures; connect edge =
rewrite the consumer's `bind`; rename = rename function + cascade
(state key migration per the design doc); drag = rewrite that system's
`@node(x=, y=)` line (the compiler learns `@node` as a
parse-and-carry presentation decorator). Cross-source edges (native
frame → Python system) render as ordinary edges. Text pane and canvas
stay two views of one source; every gesture round-trips through
reparse.

**Q6 — migration and deletion.** A converter turns a saved
`NodeEditorConfig` graph into a Python program: one declaration per
node, positions carried into `@node`, `FromDb`→bindings,
`Persist`→outputs. Runs once per preset on load, behind a
review-before-apply prompt in the pane. Then `NodeSpec`, the op
registry, per-op inspector rows, and the node editor tile are deleted;
`dynamic/ops/` keeps only the host-shaped survivors (db_source,
persist, resample, the FixedRate driver). System-graph config migrates
in place (same tile id or an alias, so existing layouts open).

**Q7 — results.** Measurements and a survivors/deleted inventory
appended here: what got removed (files, LoC), migration outcomes on the
repo's example presets, canvas edit → recompile → running latency, and
any legacy op behavior that changed shape (there should be none —
parity is the bar).

## Risks

- **Q5 is the Enso-lesson milestone** — text↔canvas sync under live
  editing. Mitigations stand: text is truth, gestures are AST edits,
  `@node` rides declarations, read-only projection already works. If
  gesture editing stalls, Q6 does NOT proceed on a half-editable
  canvas — deletion waits for parity of *editing*, not just of ops.
- **`@system(rate=)` touches the run rule** — it must compose with
  `on=` (mutually exclusive; a system is either source-clocked or
  input-driven) and with FSW semantics later (Phase 3 maps `rate=` to
  cyclic scheduling). Document in the design doc when it lands.
- **FFT correctness** is cheap to get wrong quietly; the rustfft
  differential matrix (sizes 8..1024, impulse/DC/sine/noise) is the
  gate, and the legacy Fft op's output layout is the compatibility
  contract.
- **Migration fidelity**: the converter must reproduce each legacy
  graph's published components bit-for-bit (same ids, same schemas) or
  saved dashboards break silently. Test: run both pipelines side by
  side on the example presets and diff the outputs.

## Results

### Q1 — the `@` operator

The rank rules are Python's, and batching is decided while *checking*
rather than while emitting: the leading odometer runs once in the checker
and leaves one `Batch` per matrix product, as element offsets into the
three operands. Both emit paths then see nothing but constants — the
kernel path is one `k_matmul` call per batch, the open-coded path unrolls
the lot — and the choice between them stays the existing size trade,
counted over every batch. A rank-1 operand is promoted for the duration
and the invented axis is dropped again, which is what makes `m @ v` a
vector rather than a one-column matrix.

**`dot` is removed, not aliased.** numpy's `dot` and `@` agree only up to
rank 2 and diverge above it, so a second spelling would be either a
rank-limited carve-out or a quiet lie about which contraction ran. The
old spelling compiles to a diagnostic naming the new one, which is one
keystroke in an expression field.

The differential matrix covers thirteen rank combinations, each below the
open-coding threshold and again above it, against a written-out
reference; nox is the oracle where nox has the operation, and it agrees
bit for bit — its matrix product accumulates in order, unlike its rank-1
`dot`, whose fusion `a_contraction_is_not_fused` still pins.

### Q2 — source systems and generators

`@system(rate=)` is hosted on the FixedRate node the generators already
used. It is mutually exclusive with `on=` by construction, and everything
else about a source is unchanged — so a source *with* inputs is the
cyclic FSW shape and one *without* is a generator, which is also the
Phase 3 mapping.

**One deviation from the plan's text, forced by a ratified rule.** The
plan asks for `waveform(kind, freq, amp)`. Phase 0 rule 8 refuses strings,
so a `kind` argument cannot be spelled; the four shapes are four
functions — `sine`, `cosine`, `square`, `sawtooth` — which also
autocomplete better than one name whose first argument must be memorised.
`constant(v)` is the identity, kept because the palette and the migration
both want a name for a source that does not vary.

`random()` is splitmix64 in the guest prelude over a word in a state
slot, allocated where the call is first seen so a system that never draws
carries no state. The host writes the slot at instantiation — zero is a
legal splitmix64 seed but a shared one — and because the field's declared
default is zero the guest emits no seed instruction for it, so the seed
guard needs no special case. Under test the slot is written directly,
which is what makes the draw reproducible.

### Q3 — window and FFT

Parity with the published layout is the specification, not a notion of
correctness. A window is `N` samples newest-last, preloaded with zeros;
its ring is a state slot and the result *is* that slot, so a sample
shifts in with one `memory.copy` and the whole ring reads out — no second
buffer, no index. `fft` is iterative radix-2 in the prelude with
caller-supplied scratch, because the guest has no allocator to size one
at run time; a non-power-of-two length is a diagnostic naming the length.

**The rustfft differential, sizes 8 through 1024 crossed with impulse,
DC, sine and noise: worst relative bin error 3.4e-16**, about one and a
half ULP, against a 1e-12 bound. This is the one place the crate compares
with a tolerance, and the reason is that the two are different algorithms
rather than one definition read twice.

### Q4 — resample

A top-level binding whose right-hand side is exactly a resample call is a
host-wired stage. It is not compiled at all, it publishes under its
binding name, and what reads it is an ordinary edge (`Binding::Resampled`
beside `Produced`). The call anywhere else is a diagnostic that says
where it belongs, so the special case costs one message rather than a
rule nobody can see.

Stages and systems are checked in **one pass in declaration order**,
because a stage's output type can be a system's and a system can read a
stage. Order is recovered from spans rather than stored: top-level
declarations do not overlap, so where each one starts *is* the order it
was written in, and `Manifest::declarations` is what a host builds by.

### Q5 — the unified canvas

`canvas` draws one graph from two sources. Cross-source edges needed
nothing new: a native instance name is its telemetry prefix, so a
component called `nav.attitude.omega_b` is published by the instance
`nav`, and a Python port bound to it is an edge from that card — found by
the same rule wiring matches ports with. The reverse direction does not
exist and that is a fact rather than a gap; native systems are wired
against frames the target declares.

The tile keeps the system graph's serialization key, because it *is* the
system graph with a second source added, so every saved layout naming it
opens unchanged; a layout naming the program pane opens the same tile on
the text it was saved on.

**Gesture editing did not stall.** Every gesture is a rewrite of the
program's text, and `canvas::edit` is the whole of that: each function
takes the source and hands back the source, so a gesture is not a change
to the canvas that must later be written down. A drag is one edit rather
than one per frame — the pointer moves a preview and the release does the
rewrite. Connecting rewrites one binding. Renaming works from the
compiler's spans, so renaming `rate` leaves `rate_limit` alone, and the
state key follows the system name, which is what `metor_expr::state`
already keys on. The palette is derived rather than declared: an entry is
a line of Python, split into sources (always offerable) and transforms
(offered against a selection), so what is inserted always compiles and is
always already wired.

Twenty-nine tests across the four canvas modules, and the edit tests
assert about manifests rather than about text — what an edit produces has
to *mean* the intended thing.

### Q6 — migration, and the deletion

The harness built each fixture twice — once through the node editor's own
constructors, once through the converted program — and diffed the
components each registered. **21 of 21 ops identical: same
`ComponentId`, same `ComponentSchema`.** (The repo's own presets could
not have gated this: the shipped presets are dashboards and plots and
contain no node graphs at all, so every op kind is exercised directly
instead.)

Getting the last two there needed one language addition, **ratified by
the user on 2026-08-23**: `[a, b, c]` is a fixed-size tensor
*constructor*. `Pack` was N co-clocked values with a leading length-N
axis, which is exactly what a literal writes. This amends Phase 0's rule
8 rather than reversing it — the list *type* stays gone, there is no list
value and nothing to append to, because a literal types as a `Tensor` the
moment it is read. The phase 0 plan records the amendment where the rule
is stated.

`DeltaT` is the one conversion that declares state. The synthesized class
keeps the previous timestamp as `i64` and does the subtraction there,
because that is where the legacy op did it — `(ts - prev) as f64 * 1e-6`,
not a float subtraction of two large timestamps. Its first sample is the
one place the streams differ, and the difference is the language's rather
than the converter's: legacy published *nothing* until it had two
timestamps to subtract, and a system publishes once per evaluation. A
`seen` guard makes that first sample `0.0`, so the streams line up from
the second onwards — which is every sample the legacy op ever emitted.

Three findings the harness made rather than assumptions it confirmed. A
legacy threshold published `1.0`/`0.0` and not a bool, so the faithful
translation is the conditional rather than the comparison. The legacy
composer refuses two inputs that do not share a clock, so a two-operand
fixture must derive both operands from one source, as a real graph would.
And `lang::gather` did not descend into list literals, so an anonymous
binding over `[a, b, c]` found none of its own references.

**One accepted difference, reported rather than hidden.** The legacy ops
kept a narrower element type where the language has only `f64`. Over
`f64` sources the two agree exactly; an `i32` channel through `Window`
published `i32` before and publishes `f64` now — same id, same shape.
Nothing a plot draws changes (`dynamic/tensor.rs` computed in `f64` and
cast at write time even under the old ops), but what the schema *says*
does.

With the gate passed, the node editor is gone. A layout that still names
it opens the graph tile on its graph converted to a program, showing the
text, with anything the converter could not express written at the top as
comments — a conversion is something to read before it is something to
keep. `canvas::legacy` survives as the serde surface and nothing else,
because presets saved before today still hold graphs and a format nobody
can read is a format that silently loses data.

The differential harness went with the constructors it compared against;
what it proved is pinned in its place, as every fixture's published
component written out exactly as the legacy pipeline published it.

### Q7 — measurements and inventory

**Compile latency**, `--release`, Apple silicon — unmoved by four
milestones of language growth: **151 µs** for a one-liner, **398 µs** for
a hundred lines, against the plan's 200 ms budget.

**Module size**: prelude 22,738 B; a module reaching no kernels 5,119 B;
transcendentals 10,065 B; tensor kernels 5,317 B; every kernel 16,209 B.
The prelude grew 1,293 B across Q2 and Q3 (the PRNG and the FFT), and a
module that wants neither still ships neither.

**Gesture → a program the runtime can build**, `--release`, behind the
same 200 ms debounce:

| gesture | rewrite | reparse | total |
|---|---|---|---|
| drag (`@node`) | 0.2 µs | 164 µs | **165 µs** |
| connect | 0.3 µs | 162 µs | **163 µs** |
| rename | 0.5 µs | 160 µs | **160 µs** |
| delete | 0.0 µs | 34 µs | **34 µs** |
| add from palette | 0.2 µs | 164 µs | **164 µs** |

The rewrite is free and the reparse is the whole cost, which is the
point: the text is the truth, and re-deriving everything from it costs a
sixth of a millisecond. Phase 1 measured the rest of the path —
instantiate and wire at 0.01 ms, first sample through at 2.10 ms — and
none of that changed.

**Deleted.** The node editor and everything only it needed: the tile and
its canvas (1,046), the per-op inspector rows (1,063), its tests (1,183),
the op registry (436), `NodeSpec`'s constructors and id mirroring (384),
the graph model (320), the coordinator (137), validation (104), and the
saved-format module (103); the op modules `derive` (590), `compose`
(346), and `generators` (196); the dynamic op tests (1,166) and the
`DynamicRegistry` (65) that served the editor's alive-set reconciliation.
**7,173 lines in that step alone.**

Earlier in the phase: `program/mod.rs` and `program/pane.rs` (464), the
standalone projection and its renderer (511), and the system graph's tile
(765 of its 789 lines; what remains is the part that is about wiring
rather than about drawing).

**Across Phase 2: 9,123 lines of files removed outright.** What survives
of the old world is `canvas::legacy` — the serde surface, so saved graphs
can still be read — and `dynamic/ops`, trimmed to the host-shaped
machinery the plan named: the fixed-rate clock, `db_source`, `persist`,
`resample`, and the program runtime.

**Added**: `canvas` at 4,000 lines across seven modules, of which 1,600
are tests. `metor-expr` grew by the `@` operator, source systems,
`window`/`fft`, resample stages, `@node`, and tensor literals — 93 tests
to 131.

**The panel now has one graph surface.** Where there were three pictures
of the same thing there is one, and the twenty-one op kinds behind one of
them are a language instead — where, as the complaint that started this
design put it, five nodes and eight edges are one line.
