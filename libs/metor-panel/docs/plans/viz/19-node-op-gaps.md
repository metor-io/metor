# 19 — Node-op gaps exposed by the survey

## Summary

The survey turned up five families the derivation graph cannot express today:
integral, filters (FIR/IIR/median), windowed statistics, quaternion→Euler, and
a boolean `Condition` that annunciators and gating can consume. Most of them
fall out of **one new op** — a `Reduce` over the leading axis, which composes
with the existing `Window` to give every rolling statistic and the median
filter at once. The rest are a short catalog of ops following `derive.rs` and
`compose.rs` exactly, plus one rename: `Threshold` is already the `Condition`
op, it just returns `f64` instead of `Bool`.

## Reuse vs. new

Before adding anything, what the graph already covers:

| Survey ask | Already expressible? |
|---|---|
| Moving average | `Window(N) → Reduce(Mean)` — needs `Reduce` only |
| Median filter | `Window(N) → Reduce(Median)` — needs `Reduce` only |
| Rolling min/max/RMS/percentile | `Window(N) → Reduce(stat)` — needs `Reduce` only |
| Degrees from radians | `Scale(180/π)` — nothing to add |
| Element extraction from a quaternion | `Index` (`derive.rs:360`) |
| Boolean from a comparison | `Threshold` (`derive.rs:427`), but it emits `f64` |
| Boolean from two channels | Nothing — needs `Compare` |
| FIR with arbitrary taps | Not expressible: `Constant` (`generators.rs`) only repeats one scalar, so `Window → Dot(taps)` cannot be built |

So the honest new surface is: `Reduce` + `Percentile`, `Integrate`, `Fir`,
`Smooth`, `Biquad`, `QuatToEuler`, `Band`, `UnaryOp::Not`, `Compare`, `Logic`,
`Gate` — and the `Threshold` → `Condition` broadening.

**No new dependencies.** `rustfft` is already in tree (`Cargo.toml:37`, used by
`derive::fft`), `nox` provides quaternions (`Cargo.toml:22`), and every filter
here is a handful of lines of `f64` arithmetic over the existing
`read_f64_at`/`write_f64_as` helpers (`src/dynamic/tensor.rs:347,367`). A
biquad-cookbook or DSP crate would buy nothing but a dependency.

## Design

### The one structural idea: `Reduce` over the leading axis

`Window` (`derive.rs:456`) and `Pack` (`compose.rs:303`) both *add* a leading
axis. `Reduce` removes one, which makes it their exact inverse and makes every
windowed statistic a two-node chain instead of a new op per statistic:

```
input [..shape]  →  Window(64)  →  [64, ..shape]  →  Reduce(Rms)  →  [..shape]
```

Shape rule: `[n, ..rest] → [..rest]`; a rank-1 `[n]` reduces to rank-0. Rank-0
input is rejected with `BuildError::InvalidArg`, mirroring `fft`'s guard at
`derive.rs:238`.

Dtype rule follows the conventions already in the file: order statistics
(`Min`, `Max`, `Median`, `Percentile`) return actual input values so they keep
the input dtype; accumulating statistics (`Sum`, `Mean`, `Rms`, `Std`) widen
ints to `f64` and preserve float width — the same rule as `magnitude`
(`derive.rs:313`) and `UnaryOp::out_dtype` (`derive.rs:186`). `Bool` is
rejected everywhere except `Min`/`Max`, which read naturally as all/any.

`Percentile { p }` is a *separate* spec variant rather than a `ReduceStat`
variant carrying an `f64`, so that `Reduce`'s `hash_args` stays argument-free
and editing `p` on one node cannot perturb the ids of unrelated `Reduce`
nodes. Same reason `Threshold` keeps `k` out of `ThresholdOp`.

### `Threshold` becomes `Condition`, and gains a `Bool` socket

`derive::threshold` already compares each element against `k` and emits
`1.0`/`0.0` as `f64`. It is the survey's `Condition` op wearing the wrong
output dtype. Broaden in place:

- `ThresholdOp` → `CompareOp`, gaining `Eq` and `Ne` (exact — documented as
  intended for int/bool/mode channels, not floats).
- Output dtype `F64` → `Bool`. This does **not** change any `NodeId` (the hash
  is tag + args + parents, `node.rs:349`), and `Bool` already flows everywhere
  it needs to: `promote(Bool, X) == X` (`tensor.rs:170`) keeps downstream
  arithmetic working, `TrafficLight` reads `ElementValue::Bool`
  (`views/traffic_light.rs:118`), and `CellKind::from_schema` maps
  `PrimType::Bool` to a bool cell (`views/value_strip.rs:53`).
- `op_tag::THRESHOLD` keeps its bytes (`b"derive.threshold"`, `node.rs:329`) so
  saved graphs reconcile unchanged, and `NodeSpec::Condition` carries
  `#[serde(rename = "Threshold")]` so saved *documents* still deserialize.
- Band conditions (`Inside`/`Outside`) need a second bound, and folding a
  second arg into `Condition`'s hash would re-key every existing node. They get
  their own op, `Band { lo, hi, op }`.

`SocketKind` (`src/node_editor/registry.rs:16`) gains `Bool`, with
`compatible_with` (`:31`) accepting `(Bool, Bool)`, `(Bool, Value)` and
`(Any, _)`. That is what lets `validate_connection`
(`src/node_editor/validate.rs:23`) refuse a float channel plugged into an
`And`.

### Sample rate is not in the graph

Nodes carry a `parent_clock_id`, not a rate, so a filter specified in Hz has
no ground truth to work from. Two answers, in order of confidence:

- `Smooth { tau_s }` — single-pole exponential with `α = 1 − exp(−dt/τ)`
  computed per sample from the real timestamp delta. Rate-independent, correct
  under jitter, and the workhorse telemetry smoother. **Prefer this.**
- `Biquad { response, cutoff_hz, q }` — RBJ cookbook coefficients need `fs`.
  Measure it: keep an EWMA of `dt` (the same quantity `delta_t` already
  extracts, `derive.rs:563`) and recompute coefficients when the estimate
  drifts more than 1%. Riskiest entry in the catalog; scheduled last.

### `euler_zyx` has one home

`views/attitude.rs:316` already implements the 3-2-1 conversion with the
pitch clamp that keeps a slightly non-unit quaternion off `NaN`, and it has
tests at `attitude.rs:684-708`. A dynamic op must not import from `views`, so
move the function (retyped to `f64`) to `src/dynamic/tensor.rs` next to the
other numeric helpers, move its tests with it, and have `attitude.rs` call it
through a thin `f32` wrapper. `nox::Quaternion` has `from_euler`
(`libs/nox/src/quaternion.rs:103`) but not the inverse, so there is nothing to
reuse there without a cross-crate change.

## The catalog

`derive.rs` = single input, `compose.rs` = N co-clocked inputs. "Socket" is the
`OpDescriptor` output kind. All new ops emit into the standard
`[Timestamp][value]` framing via `write_sample` / `run_aligned_emit`.

### `src/dynamic/ops/derive.rs`

| Op | Args | In → Out | Socket | Notes |
|---|---|---|---|---|
| `Integrate` | — | any non-`Bool`, any shape → `f64` same shape | `VAL` | Trapezoidal over real `dt`: `y += (x[n]+x[n-1])/2 · dt`. First sample seeds and emits `0`. Mirror of `delta` (`derive.rs:510`); tag `derive.integrate`. |
| `Reduce` | `stat: ReduceStat` | `[n, ..rest]` → `[..rest]` | `VAL` | `Min/Max/Sum/Mean/Rms/Std/Median`. Family enum with per-variant `op_tag` (`derive.reduce_min`, …), same shape as `UnaryOp` (`derive.rs:132`). Rank-0 in rejected. |
| `Percentile` | `p: f64` | `[n, ..rest]` → `[..rest]` | `VAL` | Linear-interpolated order statistic, `p` in `0..=100`; validated at build. Dtype = input. Tag `derive.percentile`. |
| `Fir` | `taps: SmallVec<[f64; 8]>` | any non-`Bool`, any shape → `f64` same shape | `VAL` | Per-element delay line. Empty taps rejected. Hash folds each `to_bits()`. Tag `derive.fir`. |
| `Smooth` | `tau_s: f64` | any non-`Bool`, any shape → `f64` same shape | `VAL` | `α = 1 − exp(−dt/τ)` per sample; `τ > 0` validated. Tag `derive.smooth`. |
| `Biquad` | `response: BiquadResponse`, `cutoff_hz: f64`, `q: f64` | any non-`Bool`, any shape → `f64` same shape | `VAL` | `Lowpass/Highpass/Bandpass/Notch` family, per-variant tags. Per-element state; `fs` measured from timestamps. |
| `QuatToEuler` | `sequence: EulerSequence` | `[4]` (`[x,y,z,w]`) → `[3]` `f64` radians | `VAL` | Build-time check that the input totals 4 elements. `Zyx` only at first; the enum leaves room. Tag `derive.quat_to_euler`. |
| `Condition` | `k: TypedScalar`, `op: CompareOp` | any non-`Bool` → `Bool` same shape | `BOOL` | The renamed `threshold`. `Gt/Ge/Lt/Le/Eq/Ne`. Existing tag and `hash_args` unchanged. |
| `Band` | `lo: TypedScalar`, `hi: TypedScalar`, `op: BandOp` | any non-`Bool` → `Bool` same shape | `BOOL` | `Inside`/`Outside`, half-open `[lo, hi)`. `lo < hi` validated. Tag `derive.band`. |
| `UnaryOp::Not` | — | `Bool` → `Bool` same shape | `BOOL` | New variant on the existing family (`derive.rs:132`); `validate` *requires* `Bool` — the inverse of every other variant's rule. Tag `derive.not`. |

`Reduce`, `Percentile`, `Band` and `Not` all route through the existing
`map`/`map_each_element` helper (`derive.rs:32,49`) where the shape is
preserved; `Reduce` and `Percentile` need their own `NodeImpl::spawn` bodies
because they change shape, modelled on `magnitude` (`derive.rs:305`).

### `src/dynamic/ops/compose.rs`

| Op | Args | In → Out | Socket | Notes |
|---|---|---|---|---|
| `Compare` | `op: CompareOp` | two co-clocked values, broadcast → `Bool` | `BOOL` | Elementwise `a <op> b`. Reuses `binary` (`compose.rs:104`) once its output dtype is overridable — see step 1. Per-variant tags `compose.cmp_gt`, … |
| `Logic` | `op: LogicOp` | N co-clocked `Bool`s, min 2, broadcast → `Bool` | `BOOL` | `And/Or/Xor`, variadic like `mean` (`compose.rs:189`), straight onto `run_aligned_emit`. This is what makes annunciator condition-sets (#6) and conditional styling (#22) expressible. |
| `Gate` | — | `(value, Bool)` co-clocked → value schema unchanged | `VAL` | Emits the value sample only while the condition holds. Needs `run_aligned_emit`'s `encode` to be able to skip — see step 1. Tag `compose.gate`. |

`Latch` (hold a condition true until dismissed) is deliberately **not** here:
latching plus first-out marking is the annunciator's semantics and belongs with
#6, where the reset gesture lives.

### Wiring checklist — every op above needs all seven

Missing any one of these fails loudly in a different subsystem, so this is the
checklist to run per op rather than per step:

1. `op_tag::X` constant — `src/dynamic/node.rs:304-336`.
2. Constructor in `ops/derive.rs` or `ops/compose.rs`.
3. `NodeSpec` variant, `NodeSpecKind` variant, and arms in `kind()`,
   `family_op_id()` (families only), `op_tag()`, `hash_args()` and `build()` —
   `src/node_editor/spec.rs:26,94,119,149,160,193,269`. `hash_args` must mirror
   the constructor's `hash_id` closure byte for byte; drift silently breaks
   reconciliation.
4. `OpDescriptor` entry in `registry::ALL` — `src/node_editor/registry.rs:94`.
   Family variants get one entry each (like the six `Unary` rows at `:185-238`).
5. `rows_for_node` arm — `src/node_editor/inspector_rows.rs:317`; argument-free
   ops join the fall-through at `:749`. Use `scalar_arg` (`:765`), `enum_arg`
   (`:822`), `typed_scalar_arg` (`:851`) and `text_arg` (`:791`); `Fir`'s tap
   list follows `shape_arg`/`parse_shape` (`:924,962`).
6. `arg_count` on the descriptor must equal the row count from step 5
   (`registry.rs:78`).
7. Tests: add the kind to the exhaustive match and array in
   `every_node_spec_kind_has_a_descriptor`
   (`src/node_editor/tests.rs:408`), a `*_id_matches` pin test for any op with
   args (pattern at `tests.rs:384-406`), and behaviour tests in
   `src/dynamic/tests.rs` following e.g. `threshold_*` (`:739-873`) and
   `delta_*` (`:875-992`).

## Implementation steps

1. **Plumbing prerequisites** (no user-visible op yet):
   - `SocketKind::Bool` + `compatible_with` arm — `src/node_editor/registry.rs:16,31`.
   - Split `compose::binary` (`compose.rs:104`) so the output dtype is a
     parameter rather than always `promote(a, b)`; `binary_op` passes the
     promotion, `Compare` passes `Bool`.
   - Change `run_aligned_emit`'s `encode` closure to return `bool`
     (`compose.rs:50-101`) so `Gate` can skip a tuple; update the three
     existing callers to return `true`.
   - Move `euler_zyx` from `views/attitude.rs:316` to
     `src/dynamic/tensor.rs` as `f64`, with its tests; leave an `f32` wrapper
     at the call site (`attitude.rs:453`).
2. **`Reduce` + `Percentile`.** The highest-value step — it retires the whole
   "windowed statistics" ask and the median filter in one go. Test `Window(4)
   → Reduce(Max)` against a known ramp, `Reduce(Median)` against an impulse
   (the point of a median filter), and shape/dtype rules per the table.
3. **`Integrate`.** Test that `Integrate` of a constant `1.0` on a 100 Hz clock
   tracks elapsed seconds, and that the first sample emits `0`.
4. **`Condition` + `Band` + `Not`.** The rename, the `Bool` output, the serde
   alias, `Band`, `UnaryOp::Not`. Pin the id stability: an existing
   `NodeSpec::Threshold` blob must deserialize to `Condition` and hash to the
   same `NodeId`. Verify end to end that a `Condition` node feeds a
   `TrafficLight` panel.
5. **`Compare` + `Logic` + `Gate`.** The composite-condition set. Test that
   `Logic(And)` rejects a non-`Bool` parent at connect time via
   `validate_connection`, and that `Gate` emits nothing while its condition is
   false.
6. **`Fir` + `Smooth`.** Test `Fir` with taps `[0.5, 0.5]` against a step, and
   `Smooth` reaching 63% of a step after one time constant.
7. **`QuatToEuler`.** Reuse the moved tests' fixtures (identity, 90° roll, 90°
   pitch) from `attitude.rs:684-708`.
8. **`Biquad`.** Last, and separable — the only entry whose correctness depends
   on inferring a sample rate. Test that a lowpass at `fs/10` passes DC
   unattenuated and attenuates a Nyquist-adjacent waveform.

Steps 2–8 are independent of each other once step 1 lands, so they can be
reordered by need.

## Open questions

- **Is `Window → Reduce` fast enough?** `Window(N)` emits `N ×` the input bytes
  every tick, and `Reduce` walks all of it — `O(N)` per sample, `O(N log N)`
  for `Median`/`Percentile`. At `N = 64` and panel rates that is nothing; at
  `N = 4096` it is not. A fused `Rolling { stat, size }` with an incremental
  accumulator (monotonic deque for min/max, running sums for mean/RMS) would
  fix it, at the cost of duplicating what the composition already expresses.
  **Decide from a profile, not up front** — ship the composition first.
- **`Reduce` axis selection.** Reducing the leading axis is the inverse of
  `Window`/`Pack` and covers every case the survey named. A `Reduce` over the
  *last* axis (to collapse an FFT's frequency bins) or over all axes (already
  `magnitude` for L2) may be wanted later. Adding an `axis` arg afterwards
  would re-key existing nodes, so if we want it, we want it now — but nothing
  in the survey asks for it.
- **`Eq`/`Ne` on floats.** Exact comparison is the right default for mode and
  status channels and a trap on floats. Should `Condition` grow a tolerance
  arg, or should the inspector warn when `Eq` is selected on a float parent?
  The warn is cheaper and `parent_dtype` (`inspector_rows.rs:983`) already
  resolves the dtype.
- **A `Histogram` op.** #4 was cut and
  `docs/plans/viz/11-value-density-heatmap.md` buckets in the view, so nothing
  needs one today. If a *persisted* distribution is ever wanted (bin counts
  written to the DB rather than recomputed per frame), it is
  `Histogram { bins, min, max }` in `derive.rs`, `[n, ..] → [bins]` — noted so
  it is not reinvented.
- **Descriptor sprawl.** `ReduceStat` alone adds seven palette entries on top
  of `Unary`'s six and `Binary`'s four. `ALL` (`registry.rs:94`) is a flat list
  the palette renders by `category`; at ~45 entries the "Derive" category may
  need sub-grouping. Cosmetic, but worth watching as this lands.
- **Where `Gate`'s output clock lives.** `Gate` keeps its parent clock id, so a
  gated stream is still nominally co-clocked with its siblings even though it
  emits sparsely. Composing it with a non-gated sibling then relies on
  `run_aligned_emit`'s skew realignment (`compose.rs:69-92`) rather than true
  tick alignment. Confirm that behaves sanely, or require a resample after a
  gate.
