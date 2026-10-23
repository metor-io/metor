# 26b: Star-tracker sensor, plan

Implements `26-star-tracker.md` §1 (the `libs/metor-star` crate, the
`star_tracker` system, nav fusion) and the matching parts of §4: P0, the
camera and catalog parts of P2, and P4. The image record and its framework
changes (plan 26a), the wgpu simulator (26c) and the panel (26d) are out of
scope. `StarImage` and `StarTruth` come from 26c's contracts PR.

Each chunk lands as one PR, gated on `cargo test`, `cargo clippy`, the Kani
proofs it touches, and `pytest` for chunks that change Python. A chunk ends
with an antagonistic review before the next one starts.

Order: A → B → C is P0 and lands first. C also needs 26a chunk A for
`FrameArray`, and J needs 26a chunk A and 26c's contracts PR. D, E and G depend only on B and can be
written in parallel. F needs E. H needs D and E. I needs G and H. J needs I
and 26a.

```
A ─▶ B ─▶ C                       (P0: fusion plumbing, ideal tracker)
     ├──▶ D ─────────┐
     ├──▶ E ─▶ F     ├─▶ H ─▶ I ─▶ J   (J also needs 26a)
     └──▶ G ─────────┴────────▲
```

## Facts the plan relies on

MEKF and nav:

- `State::estimate_attitude<const N>` propagates and updates in one call
  (`libs/metor-adcs/src/mekf.rs:109-159`). It subtracts the bias from
  `omega` and stores the result back in `State::omega` (`:123`, `:154`).
- The update is sequential within one linearisation point. Every vector uses
  `body_r = q̂⁻¹ r` from the propagated q̂ (`:133`). `δx` accumulates
  (`:144-145`) and is applied once with `integrate_body` (`:147-150`).
- `H = [[body_r×], 0₃]` and `R = σ²I` (`:132`, `:135-137`). The covariance
  update is `(I − KH)P`, not Joseph form (`:143`). A singular innovation
  covariance silently becomes `I` (`:139-141`).
- `State::new` starts at q̂ = identity and P = I (`mekf.rs:99-106`), and
  `reset_if_invalid` returns there on a non-finite state (`:182-189`).
- Nav calls `estimate_attitude` with 2 vectors (sun and mag) or 1 (mag)
  (`examples/adcs-fsw3/systems/adcs/src/nav.rs:123-135`). It filters each
  `Sensors` sample once, keyed on its timestamp (`nav.rs:64-71`, `:97`), and
  sets the step from the sample stamps (`:73-78`, `:100`).
- `test_mekf` builds body vectors as `q.inverse() * ref` and expects
  `q_hat ≈ q` (`mekf.rs:269-282`). TRIAD's test uses the same convention
  (`libs/metor-adcs/src/triad.rs:30-37`).

nox:

- A quaternion is stored `[x, y, z, w]` (`libs/nox/src/quaternion.rs:89`).
  The product is Hamilton (`:259-268`). `q * v` is `q v q⁻¹` (`:274-280`).
  `integrate_body(δα)` right-multiplies, `q + q ⊗ [δα/2, 0]`, then normalises
  (`:165-171`).
- So `AttitudeEstimate::q_hat_b_eci` ("body←ECI",
  `examples/adcs-fsw3/contracts/src/frames.rs:62-63`) satisfies
  `b = q̂⁻¹ * r_eci`. With the same four numbers, Shuster's attitude matrix
  `A(q)` is `R_Hamilton(q)ᵀ`. QUEST's `[x; γ]` should then map directly to a
  nox `Quaternion([x, y, z, γ])`. Chunk A pins this with a test; nothing else
  relies on it until then.
- nox is `no_std` without `std` (`libs/nox/src/lib.rs:2`) and depends on faer
  (`libs/nox/Cargo.toml`). Its `try_inverse` has closed forms for 2×2 and 3×3
  (`libs/nox/src/matrix.rs:37-80`).

Records:

- A nox `Tensor<T, D>` field is one telemetry component with shape `D`
  (`libs/metor-component/src/nox.rs:212-227`). A `[T; N]` field is `N`
  components (`libs/metor-component/src/vtable.rs:37-47`).
- `u32` is not a nox `Field` (`libs/nox/src/fields.rs:220-224`), so a nox
  `Vector<u32, 32>` is not possible. 26a chunk A adds `FrameArray<T, N>`, one
  `[N]` component for `u8`, `u16`, `u32`, `i32`, `f32` and `f64`
  (`26a-image-records-plan.md:201-207`). Lists in records use it. Math
  quantities (quaternions, 3-vectors, covariances, vector stacks) stay nox
  types, as the contracts use today.
- Frames are `#[repr(C)]` zerocopy structs that readers borrow from the ring
  (`libs/metor-fsw-3/src/frame.rs:8-24`). `Record::DEPTH` defaults to 1
  (`libs/metor-fsw-3/src/record.rs:59-62`).

Wiring:

- An unconnected `Input` is valid. `latest()` returns `None`
  (`libs/metor-fsw-3/src/port/mod.rs:559-563`;
  `coordinator/build.rs:1549-1553`). The generated Python stub defaults each
  input to `()` (`libs/metor-fsw-3/src/cli/module.rs:798`). So adding
  `Input<StarVectors>` to nav does not break `sim.py`, `target.py` or the
  tests.
- A system reads only the ports of systems added before it. `Target.loop`
  gives a one-cycle-delayed edge for a later producer
  (`libs/metor-fsw-3/python/metor-config/metor_config/_target.py:146-153`;
  used in `examples/adcs-fsw3/sim.py:15-16,66-67`). `star_tracker` reads nav's
  estimate and nav reads `star_tracker`, so one of those edges is a loop.
- `thread=` places only async systems (`_target.py:168`). A cyclic system
  runs inside the 120 Hz cycle (`sim.py:13`), which is 8.3 ms.
- `fsw.sim(...)` adds a model system (`_target.py:176-182`). The plant is a
  normal system in the `adcs` pack and is the only reader of truth
  (`examples/adcs-fsw3/contracts/src/lib.rs:9-13`). It draws noise from a
  seeded `rand::rngs::StdRng` (`systems/adcs/src/plant.rs:24-25,198`).
- The pack registers systems in `pack()`
  (`examples/adcs-fsw3/systems/adcs/src/lib.rs:28-54`).
- The repo toolchain is 1.98.0 with `wasm32-unknown-unknown` and
  `thumbv7em-none-eabihf` (`rust-toolchain.toml`). The pack builder targets
  `wasm32-unknown-unknown` (`libs/metor-fsw-3/src/cli/cargo.rs:268-270`).

Measured for this plan (scratchpad `star-tracker-trade/`, Apple M4 Max,
release build, single thread, synthetic 1024×1024 12-bit frame with 60 stars
of σ_PSF = 1 px, a background gradient, Poisson and read noise, and 20 hot
pixels):

| Extractor | ms/frame | allocations/frame after warm-up | matched / 60 | RMS error |
|---|---|---|---|---|
| hand-rolled: block-median bg, run-length CCL with union-find over runs, CoG; all buffers preallocated | 0.95 | 0 (the 1 counted is the harness's clone) | 57 | 0.088 px |
| imageproc 0.27 `connected_components` (u8 mask) + CoG | 1.67 | 9 (24 MiB) | 55 | 0.107 px |
| kornia-imgproc 0.2 `connected_components` into a preallocated label image + CoG | 0.83 | 36 (30 KiB) | 55 | 0.107 px |
| tetra3 0.14 `CentroidExtractor` (u16 → f32 copy, CCL path) | 1.71 | 1364 (994 KiB) | 50 | 0.120 px |
| tetra3 0.14 `extract_centroids_fast` | 0.31 | 460 (530 KiB) | 58 | 0.124 px |

Build checks with the repo toolchain:

| Crate | `wasm32-unknown-unknown` | `no_std` (thumbv7em) | Normal deps |
|---|---|---|---|
| imageproc 0.27, default features off | builds | no (std) | 48 |
| kornia-imgproc 0.2 | **fails** (getrandom 0.4 needs `wasm_js`) | no | 45 |
| tetra3 0.14 (with or without `image`) | builds | no (std) | 41–47 |
| cdshealpix 0.10 | builds | **fails** | 63–76 |
| kiddo 6 | builds | **fails** | 23–36 |
| nalgebra 0.35 (`libm`, `alloc`) | — | builds | 13 |
| rkyv 0.8 (`alloc`) | — | builds | 19 |
| zerocopy 0.8 | — | builds | 9 |
| numeris 0.6 | — | builds | 4 |

## Crate trade study

### Image processing (background, threshold, CCL, centroid)

Constraints: builds for `wasm32-unknown-unknown`; `no_std + alloc` for
`metor-star` (§1.9); no allocation after init; reads a borrowed `&[u16]`
without a conversion copy; bounded worst case.

| Candidate | wasm32 | no_std | Alloc after init | u16 in | Perf (1024²) | License / maintenance / weight | Verdict |
|---|---|---|---|---|---|---|---|
| **image** 0.25 | yes | no | `ImageBuffer` owns a `Vec`; no ops of its own | `Luma<u16>` | n/a | MIT/Apache, very active, light without codecs | Not needed: we never decode or own a file image. |
| **imageproc** 0.27 | yes | no | every op returns a new image (`connected_components` allocates a 4 MiB `u32` label image per call) | CCL is generic; `threshold`, morphology and `adaptive_threshold` are `GrayImage` (u8) only | 1.67 ms | MIT, active, 48 deps | Reject for flight: per-frame allocation, u8-only threshold. |
| **kornia-imgproc** 0.2 | **no** (getrandom) | no | takes `&mut dst`, but CCL keeps per-call `Vec`s (36 allocations/frame); `rayon` is a hard dependency | threshold is generic; CCL is u8 only | 0.83 ms | Apache-2.0, active, 45 deps, optional CUDA/OpenCV | Reject: does not build for the pack target. The run-based union-find design is worth copying. |
| **ndarray** + **ndarray-ndimage** 0.6 | likely | no | every op returns a new `Array`; `label` is 3-D only (`ArrayRef3<bool>`) | yes | not run | MIT/Apache, 1 maintainer | Reject: 3-D only, allocating. |
| **ndarray-stats** 0.7 | likely | no | quantiles mutate an owned array | yes | not run | active | Not needed: `select_nth_unstable` on a reused scratch slice gives the block median. |
| **opencv** 0.101 (bindings) | no | no | `Mat` allocations; C++ runtime | yes | not run | Apache-2.0, needs system OpenCV | Reject: native C++ dependency, no wasm. |
| **photon-rs** 0.3 | yes | no | RGBA `PhotonImage` only | no | not run | Apache-2.0, slow cadence | Reject: RGBA filters, no CCL. |
| **seiza-imgproc** 0.3 | likely | no | returns `Vec`s | u8 mostly | not run | Apache-2.0, new | Reject: u8, allocating. |
| **tetra3** 0.14 (`centroid_extraction`) | yes | no | `CentroidExtractor` reuses image buffers but still allocates per blob (460–1364 allocations/frame) | `f32` only (a 4 MiB copy) | 0.31 ms (fast path) / 1.71 ms | MIT (derived from Apache-2.0 tetra3 and cedar-solve), alpha, active | Reject for flight. **Use as a dev-dependency oracle** for centroids and for lost-in-space solutions. |
| ruststartracker 0.2 | no | no | OpenCV + pyo3 are hard dependencies | u8 only | not run | MIT | Reject. |
| **Hand-rolled** in `metor-star::image` | yes | yes | none: `Extractor::new` sizes every buffer | native `&[u16]` | 0.95 ms (unoptimised prototype) | ours | **Pick.** |

Why hand-rolled is justified here: the whole pipeline is about 300 lines.
Background is a block median and MAD (`select_nth_unstable` on a reused
scratch slice). The threshold compares each pixel with its block's level.
Labelling is one raster pass that emits runs and unions overlapping runs of
adjacent rows (8-connectivity), so the union-find is over runs, not pixels.
Moments accumulate per run and fold into roots. No crate meets the wasm,
no_std, u16 and no-allocation constraints together, and the prototype is
already within 3× of the fastest crate path while being more accurate. The
prototype is in the scratchpad (`star-tracker-trade/bench/src/main.rs`,
module `hand`); chunk G starts from it.

### Linear algebra for QUEST and the q-method

| Candidate | Verdict |
|---|---|
| **nox** | Already in the pack, but its types are heavy generics over `Repr`, and §1.9 keeps `metor-star` free of nox. Use nox only at the system boundary (`StarVectors`, nav). |
| **nalgebra** 0.35 | no_std with `libm`, 13 deps, MIT/Apache. Has `SymmetricEigen` for a 4×4. **Pick as a dev-dependency** for the q-method oracle, since an oracle should share no code with what it checks. |
| **faer** 0.24 | Already in the tree through nox. Overkill for fixed 3×3/4×4 and needs workspace memory for decompositions. Not used directly. |
| numeris 0.6 | no_std, 4 deps, MIT, used by tetra3. Young; adds a second small-matrix API. Not used. |
| **Hand-rolled** `metor-star::geom` (`[f64; 3]`, `[[f64; 3]; 3]`, a scalar-last quaternion) | **Pick for flight code.** QUEST needs only 3×3 products, trace, adjugate, determinant and a matrix–vector product. Plain arrays keep the crate dependency-free, keep Kani harnesses tractable, and make f64 precision explicit. About 120 lines. |

### Pair table, k-vector and sky index

| Need | Candidate | Verdict |
|---|---|---|
| k-vector over pair angles | no crate exists | **Hand-roll** (about 60 lines plus tests against a brute-force scan). |
| Onboard cone query | **cdshealpix** 0.10 | std only (fails no_std); `cone_coverage` allocates a BMOC per query. Reject onboard. |
| | **kiddo** 6 (k-d tree) | std only; built for k-NN, not cone ranges over 5k stars. Reject. |
| | **Hand-rolled cube-face grid** | **Pick.** A vector maps to (face, i, j) with one division. The DB stores stars sorted by cell and a `u32` offset table. A cone query tests the 6·n² cell centres (n = 8: 384 dot products) and scans hits. Bounded and simple, with no face-edge cases. |
| Brute force | scan all ~5k stars | The test oracle for the cone query. |

The DB stores pair angles θ as f32 radians, not cos θ. At small separations,
f32 cos θ resolves only about 6e-8 / sin θ rad, which is 12″ at 0.06°. That
is comparable to σ_b (≈ 7″). An f32 θ has relative precision of 6e-8.

### Offline catalog parsing (`star-db` tool)

| Candidate | Verdict |
|---|---|
| **csv** 1.4 (already in `Cargo.lock`) | **Pick.** Hipparcos `I/239/hip_main.dat` is `|`-delimited fixed-width text: `ReaderBuilder::delimiter(b'|')`, `has_headers(false)`, with fields trimmed. |
| polars | Reject: hundreds of deps for a 118k-row file. |
| star-catalog 0.2, starstuff-types 0.1 | Their own formats and epochs; we need to control merging, variability cuts and proper motion ourselves. Reject. |
| tetra3 catalog loaders | Gaia+Hipparcos merged postcard format, not ours. Use only through the oracle tests. |
| **cdshealpix** | Not needed: the onboard index is a cube-face grid computed by `metor-star` itself, so the tool and the tracker share one function. |
| **flate2** (in lock) + **sha2** (in lock) | Read the cached `hip_main.dat.gz` and check it against a pinned sha256. |
| **clap** 4 (in lock) | Tool CLI. |

### Binary DB format

| Candidate | Verdict |
|---|---|
| **zerocopy** 0.8 | **Pick.** Already used for every frame; no_std; 9 deps. The DB is a header plus flat `#[repr(C)]` arrays; `Ref::from_prefix` / `FromBytes::ref_from_bytes` borrow each section from an `include_bytes!` or a loaded `Vec<u8>` with no copy. Use `zerocopy::byteorder::little_endian` field types so the file is LE on any host. |
| rkyv 0.8 | no_std, but its archived-type machinery and validation (`bytecheck`) are more than flat arrays need, and the format is tied to rkyv versions. Reject. |
| postcard | Not zero-copy for arrays (tetra3 uses it). Reject. |
| **crc32fast** 1.5 (in lock, `default-features = false`) | Header CRC over the sections. |

### Summary of picks

| Step | Pick | Reason |
|---|---|---|
| Background / threshold / CCL / CoG | hand-rolled `metor-star::image` | only option that is wasm + no_std + u16 + zero-allocation; measured 0.95 ms |
| Centroid and LIS cross-check | `tetra3` (dev-dependency) | independent open implementation to test against |
| QUEST, covariance, camera math | hand-rolled `metor-star::geom` on `[f64; N]` | tiny, dependency-free, Kani-friendly |
| q-method oracle | `nalgebra` `SymmetricEigen` (dev-dependency) | independent eigen-solver |
| k-vector, pair table | hand-rolled | no crate; small |
| Sky index | hand-rolled cube-face grid | cdshealpix and kiddo are std-only; brute force is the oracle |
| Catalog parse | `csv`, `flate2`, `sha2`, `clap` (tool only) | already in the lock, light |
| DB format | `zerocopy` + `crc32fast` | already used, no_std, zero-copy |
| Math in no_std | `libm` | already in the tree |

## A. MEKF split

Files: `libs/metor-adcs/src/mekf.rs`, `libs/metor-adcs/src/tests.rs` (or a
new `mekf/tests.rs` if long).

- `pub struct VectorObs { pub body: V3, pub reference: V3, pub sigma: f64 }`.
  `V3 = Vector<f64, 3, ArrayRepr>`.
- `State::propagate(&mut self)`: lines `:123-125` as they are. It subtracts
  the bias, propagates q̂ and P, and stores the corrected rate in `omega`.
- `State::update_vectors(&mut self, obs: &[VectorObs], gate: f64) -> Update`.
  `Update { used: u32, gated: u32 }`. This is the loop of `:126-150` over a
  slice, still with one linearisation point, plus:
  - an innovation gate: skip an observation when `eᵀS⁻¹e > gate` and count it
    in `gated`. `f64::INFINITY` turns it off.
  - Joseph-form covariance update `(I − KH)P(I − KH)ᵀ + KRKᵀ`, then
    symmetrise. Star σ ≈ 3e-5 rad gives R ≈ 1e-9 against a prior of P = I,
    where `(I − KH)P` loses symmetry.
  - an empty slice is a no-op that returns `Update::default()`.
- `update_vectors` is the one update path. Sun, magnetometer and star
  vectors all go through it, so the Joseph form applies to all of them.
- `estimate_attitude<const N>` becomes `propagate` then `update_vectors` over
  the zipped arrays with `gate = ∞`. Its signature is unchanged. Its results
  are no longer bit-identical to today's.

Tests:

- Nominal: `test_mekf` passes with its existing tolerances (1e-3).
  `estimate_attitude` matches the pre-split implementation (kept as a
  test-only copy) on the `test_mekf` sequence to 1e-9 on q̂ and 1e-9 relative
  on P.
- Existing tests: a test that compares nav or MEKF output against recorded
  values switches to a tolerance. `convergence.rs` compares runs of the same
  build against each other (linked, cdylib, wasm), so its byte comparisons
  stay exact. Any golden recorded before the split is regenerated or compared
  with a tolerance, and the PR lists which.
- Convention (`test_quaternion_convention`): for a random q, `q.inverse() * r`
  equals `R_Hamiltonᵀ(q)·r` written out by hand, and `integrate_body(δα)`
  equals `q ⊗ exp(δα/2)` to first order. This pins body←ECI, scalar-last
  storage and the error sign.
- Edge: 32 star vectors with σ = 3e-5 from P = I, then 10⁴ updates: P stays
  symmetric (‖P − Pᵀ‖ < 1e-15·‖P‖) and positive definite (a Cholesky via nox
  succeeds). One vector constrains only 2 axes: the variance about it stays
  near its prior.
- Failure: a gross outlier (a vector 5° off) with `gate = 13.8` (χ²₂ at
  p = 0.001) is counted in `gated` and leaves q̂ unchanged. A NaN body vector
  is gated, not fused.

## B. `metor-star` skeleton, `geom` and `wahba`

Files: `libs/metor-star/{Cargo.toml, src/lib.rs, src/geom.rs,
src/wahba.rs, src/wahba/tests.rs}`, workspace `Cargo.toml` (member).

Crate: `#![no_std]`, `extern crate alloc`, deps `libm` and `zerocopy`
(derive). Dev-dependencies: `nalgebra` and `rand`/`rand_distr`. Features:
`std` (off) for the builder in E, and `testkit` (off) for G.

`geom`:

- `type V3 = [f64; 3]`, `type M3 = [[f64; 3]; 3]`, `struct Quat([f64; 4])`
  stored scalar-last, `[x, y, z, w]`, the same order as nox.
- `dot`, `cross`, `norm`, `normalize -> Option<V3>` (None below 1e-300),
  `mat_vec`, `mat_mul`, `transpose`, `outer`, `trace`, `det`, `adj`,
  `inv_sym -> Option<M3>`.
- `Quat::{mul, conj, rotate, from_m3, to_m3}`, with `to_m3` returning
  Shuster's `A(q)`, so `b = A(q) r`. A doc test states the nox mapping:
  `nox::Quaternion([x, y, z, w]) == q_b_eci`.

`wahba`:

- `struct Obs { b: V3, r: V3, sigma: f64 }`.
- `struct Profile { b: M3, s: M3, z: V3, sigma: f64, weight_sum: f64 }`, built
  once from `&[Obs]` with `aᵢ = σ_tot²/σᵢ²`, normalised so that Σaᵢ = 1.
- `fn lambda_max(p: &Profile) -> Option<f64>`: Newton on the characteristic
  quartic from λ₀ = 1 (Σaᵢ), at most 5 steps, converged at
  |Δλ| < 1e-14. Returns None if not converged. The polynomial is evaluated in
  the nested form `((λ² − (a + b))λ − c)λ + (ab + cσ − d)`.
- `fn quest(obs: &[Obs], hint: Option<Axis>) -> Result<Solution, WahbaError>`:
  1. Try the unrotated profile, or the hinted axis first.
  2. If `|γ| / ‖[x; γ]‖ < 0.1`, re-solve with the reference vectors rotated
     180° about x, y and z (sign flips of two components) and keep the
     largest |γ|.
  3. Undo the rotation by composing with the matching 180° quaternion.
  - Errors: `TooFew` (< 2 non-parallel vectors), `Degenerate` (all vectors
    within 1e-9 rad of each other), `NoConvergence`.
- `Solution { q: Quat, p_theta: M3, loss: f64, rotated: Option<Axis> }`.
  `p_theta = inv_sym(Σ σᵢ⁻²(I − bᵢbᵢᵀ))`, or `Degenerate` if that fails.
- `fn wahba_loss(obs, q) -> f64`, shared by tests and verification.

Tests (`wahba/tests.rs`):

- Oracle: `q_method(obs) -> Quat` in the test module only. It builds K
  (4×4) and takes the eigenvector of `nalgebra::SymmetricEigen` with the
  largest eigenvalue.
- Nominal: 10⁴ random attitudes, 3–20 stars in a 20° cone, noiseless.
  QUEST agrees with the truth and with the oracle to 1e-12 rad (angle of
  `q ⊗ q_oracle⁻¹`, sign-insensitive).
- Near 180°: rotations of 180°, 180° − 1e-6 and 179.9° about x, y, z and 100
  random axes. Same 1e-12 bound, and `rotated` is set when expected.
- Noisy: σ = 34 µrad. QUEST's loss equals the oracle's loss to 1e-12
  relative. The NEES of the attitude error under `p_theta` over 10⁴ trials is
  3 ± 0.1.
- Off-nominal: 2 stars (the minimum), weights spanning 1e6, a star repeated
  twice.
- Failure: 0 or 1 vectors give `TooFew`; two parallel vectors give
  `Degenerate`; NaN input gives an error, not a NaN quaternion.

Kani: `normalize` returns None or a vector of norm within 1e-12 of 1 for any
finite input (bounded to small exponent ranges).

## C. Contracts, the ideal tracker, nav wiring (closes P0)

Files: `examples/adcs-fsw3/contracts/src/{star.rs, params.rs, lib.rs}`,
`systems/adcs/src/{star_ideal.rs, nav.rs, lib.rs}`, `systems/adcs/Cargo.toml`
(add `metor-star`), `sim.py`, `tests/stars.rs`, `libs/metor-star/src/catalog/synthetic.rs`.

Contracts (`star.rs`):

- `pub const MAX_STARS: usize = 32`.
- `StarVectors` (frame `star_vectors`): `timestamp` (mid-exposure),
  `frame_id: u64`, `count: u64`, `b_body: Matrix<f64, 32, 3>`,
  `r_eci: Matrix<f64, 32, 3>`, `sigma: Vector<f64, 32>`,
  `cat_id: FrameArray<u32, 32>`: the Hipparcos number (HIP), never a DB row
  index, with `u32::MAX` for unidentified. Rows at or past `count` are
  zero. Each field is
  one component. If `Matrix<f64, 32, 3, ArrayRepr>` fails a derive, fall back
  to `FrameBytes` and record why.
- `fn obs(&self) -> impl Iterator<Item = (V3, V3, f64)>` over the first
  `count.min(32)` rows.
- `StarSolution` (frame `star_solution`): `timestamp`, `frame_id`,
  `q_b_eci: Quat`, `p_theta: Matrix3<f64>`, `residual_rms: f64` (rad),
  `p_false: f64`, then `mode: u8` (`Ideal`, `Lost`, `Tracking`),
  `status: u8` (`Valid`, `NoStars`, `NoMatch`, `VerifyFailed`,
  `NoConvergence`, `Degenerate`), `n_centroids: u16`, `n_matched: u16`, and
  padding. Codes use `TryFrom<u8>` like `Mode` (`frames.rs:241-254`).
- `StarParams { rate_hz, fov_deg, sigma_rad, n_stars_max, seed,
  q_b_st: [f64; 4] }`. `sigma_rad` is a constant per-star σ from config. and `NavParams` gains
  `#[serde(default = "...")] star_gate: f64` (13.8).

`metor-star::catalog::synthetic`: `fn uniform(n: usize, seed: u64) ->
Vec<V3>`, a deterministic uniform sphere (Marsaglia) with a small inline
xorshift so the crate needs no `rand`. It stands in for the DB until E and F
land. Synthetic stars get ids `1..=n` in the same `u32` id space, so the
scoring helper in J works on both.

`star_ideal` (a normal system in the `adcs` pack, placed after `plant`,
reading truth like the plant does):

- Inputs `body: Input<BodyState>`. Outputs `star_vectors`, `star_solution`.
- State: the synthetic catalog (5000 stars, built in `new`), `StdRng`, and
  the last frame time. All buffers are allocated in `new`.
- At `rate_hz`: compute the boresight `q_b_eci ⊗ q_b_st · ẑ`, select the
  catalog stars inside the FOV cone, keep at most `n_stars_max` (random
  order), and write `b = q⁻¹ r` perturbed by an isotropic tangent-plane
  error of σ, renormalised. Run `wahba::quest` and write `StarSolution` with
  mode `Ideal`.
- Pure helpers (`select_in_fov`, `perturb`, `to_obs`) carry the logic, so the
  system body is just I/O.

nav:

- Gains `stars: &mut Input<StarVectors>` and `last_star: Option<u64>`
  (frame id).
- `fn star_obs(sv, now_ω, Δt) -> impl Iterator<Item = VectorObs>`. Each b is
  rotated to nav time by `exp(−[ω×]Δt)` (Rodrigues), where
  Δt = t_nav − t_img ≥ 0, and σ² is inflated by `σ_g²·Δt`. A frame with
  Δt < 0 or Δt > 1 s is dropped with one warning.
- The flow becomes `propagate`, then sun/mag `update_vectors`, then star
  `update_vectors(.., star_gate)`, all through A's one update path.
- Nav fuses star vectors only. It never reads `StarSolution` and has no
  reset or seed from the tracker's quaternion. It starts from q̂ = identity,
  P = I, as today.
- Log once per transition: "star vectors live" and "star vectors stale".

`sim.py`: add `star = fsw.add("star", StarIdeal(body=plant.body, ...))`
before nav and pass `stars=star.star_vectors`. `target.py` is unchanged
(nav's `stars` defaults to unconnected).

Tests:

- `nav.rs` unit tests: `star_obs` rotates a vector by ω·Δt exactly
  (1e-15); Δt = 0 is the identity; negative Δt is dropped.
- `star_ideal.rs` unit tests: every emitted star is inside the FOV; σ = 0
  reproduces `q⁻¹ r` exactly; `StarSolution.q` equals truth to 1e-12 at
  σ = 0.
- `tests/stars.rs` (integration, the P0 gate). Run the `sim.py` loop with
  `StarIdeal` at 10 Hz and σ = 34 µrad:
  - attitude error, sampled after 1 s at ~1° (from the CSS/mag start), falls
    below 0.05° within 30 s and stays there;
  - convergence from identity is the gate. From q̂ = identity and P = I, with
    the true attitude 30°, 90° and 170° away and sun and mag off, the error
    falls below 0.05° on star vectors alone. Run these cases first with the
    filter as A leaves it. Today every frame relinearises about the q̂ the
    previous frame left, but the vectors within one frame share one
    linearisation point (`mekf.rs:133`, `:144-150`). If 90° or 170° fails,
    the fix is to iterate the frame's vector update about the updated q̂,
    only while `√λ_max(P_θθ)` is above a threshold (for example 1°), with a
    fixed iteration cap. Inflating R while P is large is the fallback. There
    is no reset from the tracker;
  - with `stars` unconnected, `convergence.rs` passes. Its runs compare
    against each other, not against pre-split bytes.
- Failure: a frame of all-zero rows with `count = 32` is gated, not fused;
  `count > 32` is clamped.

## D. Camera model

Files: `libs/metor-star/src/camera.rs`, `camera/tests.rs`.

- `struct Intrinsics { f_m, pitch_m, cx, cy, k1, k2, p1, p2, width, height }`
  and `struct Camera { intr: Intrinsics, q_b_st: Quat }`.
- `fn distort(x, y) -> (f64, f64)` is Brown–Conrady as in §1.2, the one
  definition the simulator also uses.
- `fn undistort(xd, yd) -> Option<(f64, f64)>`: fixed-point iteration, at
  most 20 steps, converged at 1e-12 m. None if not converged.
- `pixel_to_st(u, v) -> Option<V3>`, `st_to_pixel(b) -> Option<(f64, f64)>`
  (None for `z ≤ 0` or off-sensor beyond a margin), and
  `st_to_body(b) / body_to_st(b)` through `q_b_st`.
- `fn pixel_sigma_to_rad(σ_px) -> f64 = σ_px·pitch/f`.
- `fn half_diagonal_rad() -> f64` for cone queries.
- Aberration (`fn aberrate(b, β)`) is present but off; P7 turns it on.

Tests:

- Nominal: pixel → vector → pixel round trip on a 33×33 grid including the
  corners, with and without distortion, to 1e-9 px (the P2 gate).
- Edge: zero distortion is exact pinhole; the principal point maps to ẑ.
- Failure: a vector behind the camera is None; distortion strong enough to
  fold the field makes `undistort` return None, not a wrong point.

## E. Onboard DB: format, k-vector, cone query

Files: `libs/metor-star/src/catalog/{mod.rs, format.rs, kvector.rs, cells.rs,
build.rs, tests.rs}`.

Format (all little-endian, every section 8-byte aligned):

- `Header { magic: [u8; 8] = b"METORSTR", version: u32, flags: u32,
  epoch_jy: f64, mag_limit: f32, fov_max_rad: f32, n_stars: u32, n_pairs: u32,
  n_cells: u32, face_n: u32, km: f64, kq: f64,
  sections: [Section; 5], crc32: u32, _pad }`, where
  `Section { offset: u32, len: u32 }`.
- Stars, sorted by cell: `Star { r: [f32; 3], mag_centi: i16, _pad: u16,
  hip: u32 }`, 20 B. `hip` is the Hipparcos number. Row indices (`u16`) are
  internal to the DB (pairs, matching); everything that leaves `metor-star`
  carries `hip`, read through `Db::hip(row) -> u32`.
- Cells: `u32[n_cells + 1]` offsets.
- Pairs, sorted by θ: `Pair { theta: f32, a: u16, b: u16 }`, 8 B.
- K-vector: `u32[n_pairs]`.
- `Db<'a>::parse(bytes: &'a [u8]) -> Result<Db<'a>, DbError>` checks the
  magic, version, section bounds and alignment, the CRC, that the counts
  agree, that the cell offsets are monotonic and end at `n_stars`, and that
  `n_stars ≤ 65535`. After parsing, every accessor is total.

k-vector (`kvector.rs`):

- `fn build(thetas: &[f32]) -> (Vec<u32>, f64, f64)` (feature `std` or
  `alloc`; build time only).
- `fn range(&self, lo: f32, hi: f32) -> Range<usize>`: the candidate range
  from k, then trimmed with `partition_point` on the sorted θ. Clamped, never
  panics, empty for `lo > hi` or NaN.

Cells (`cells.rs`):

- `fn cell_of(r: V3, n: u32) -> u32` (face by largest |component|, then
  `i, j` from the tangent-plane ratios). `fn cell_center(c, n) -> V3`. Used
  by both the builder and the tracker.
- `fn cone(&self, axis: V3, radius: f64, out: &mut impl Extend<u16>)`: cells
  whose centre is within `radius + cell_radius(n)`, then stars with
  `dot ≥ cos radius`. Writes into a caller-owned buffer.

Builder (`build.rs`, `alloc`): `fn build(stars: &[CatStar], cfg) ->
Vec<u8>`. It sorts by cell, generates every pair with θ < `fov_max`, sorts,
builds the k-vector and writes the sections and CRC. Used by the tool (F) and
by tests with the synthetic catalog.

Tests:

- k-vector: random tables (10–10⁵ pairs, including duplicate θ and a single
  value). `range` equals a brute-force scan for 10⁴ random intervals,
  including intervals past both ends.
- Cone: equals brute force for 10⁴ random axes and radii, including axes on
  cube edges and corners.
- Format: build → parse round trip; a flipped byte fails the CRC; a
  truncated buffer, a wrong version, non-monotonic offsets and a misaligned
  slice each give their own `DbError`.

Kani:

- `kvector::range` stays within `0..=n_pairs` for any `lo`, `hi` (including
  NaN and ±∞) over a symbolic table of ≤ 8 entries.
- `cell_of` stays within `0..6n²` for any finite unit vector.

## F. `star-db` tool and the committed DB

Files: `libs/metor-star/tools/star-db/{Cargo.toml, src/main.rs,
src/hipparcos.rs, src/clean.rs}`, `libs/metor-star/data/{hip_v65.stdb,
NOTICE}` (committed), workspace members.

The built DB is committed the same way 26c commits `starcat-g8.bin`
(`26c-star-sim-plan.md` chunk A). It sits next to it in
`libs/metor-star/data/`. The `NOTICE` there gains an entry for
`hip_v65.stdb`: derived from Hipparcos (VizieR I/239), CC BY-NC 3.0 IGO, with
the credit line "ESA" and a note that the file is not covered by the repo's
Apache-2.0 licence. If 26c has not landed, F creates the `NOTICE`.

- `star-db fetch` downloads `I/239/hip_main.dat.gz` from CDS into a cache
  directory and checks a pinned sha256. Tests never fetch.
- `star-db build --mag 6.5 --fov 20 --epoch 2024.0 --merge-px 2.5
  --pitch-rad ...` (clap):
  1. Parse with `csv` (`|`): HIP, Vmag, RAdeg, DEdeg, pmRA, pmDE, variability
     fields.
  2. Drop rows without astrometry.
  3. Propagate proper motion from J1991.25 to the epoch (§1.2).
  4. Merge pairs closer than `merge_px` into one flux-weighted star.
  5. Drop variables with amplitude > 0.5 mag.
  6. Optionally uniformise to K per cell.
  7. Write with `catalog::build`.
- `star-db check DB`: print the counts and size, and run the sizing Monte
  Carlo, P(< 4 stars in FOV) over 10⁵ random boresights, against the 0.1%
  target.
- `clean.rs` holds pure functions (`propagate_pm`, `merge_close`,
  `drop_variable`, `uniformise`), each unit tested.

Tests:

- `hipparcos.rs` parses a committed 20-line excerpt; Sirius (HIP 32349) and
  Vega (HIP 91262) land within 1″ of their SIMBAD J2000 positions after
  propagation.
- Edge: blank pm fields are treated as zero pm and flagged; a merged pair
  keeps the brighter star's HIP number. Every row of the built DB has the HIP
  number of its source row (checked against the parsed input).
- Failure: a malformed line is an error that names its line number; a sha
  mismatch refuses to build.
- `check` on the committed DB meets the sizing target. A test in
  `metor-star` parses the committed DB with `include_bytes!`.
- A test fails if `data/` holds a file that the `NOTICE` does not name.

## G. Image processing

Files: `libs/metor-star/src/image/{mod.rs, background.rs, label.rs,
centroid.rs, tests.rs}`, `libs/metor-star/src/testkit.rs` (feature
`testkit`).

- `ImageConfig { block: u16 (32), step: u8 (2), k_sigma: f32 (5),
  min_area: u16 (3), max_area: u16 (100), saturation: u16, max_axis_ratio:
  f32 (3), max_runs: u32 (16384), keep: u8 (20) }`.
- `Extractor::new(width, height, cfg)` allocates: block medians and
  thresholds, the scratch slice, runs, parents, blobs and the output
  `[Centroid; 32]`. `run(&mut self, px: &[u16]) -> Result<&[Centroid],
  ImageError>`. A length mismatch is an error. `metor-star` does not depend
  on `metor-component`, so it takes plain `&[u16]`; the system casts 26a's
  `&[Mono16<12>]` (see "What this plan uses from 26a").
- `background`: per block, the median and MAD of a strided subsample; the
  global σ is 1.4826 × the median of the block MADs (floored at 1 DN). The
  threshold is per block.
- `label`: one raster pass emitting `Run { row, x0, x1, moments }`, with
  union-find over run ids and unions of overlaps in `[x0 − 1, x1 + 1]` on the
  previous row (8-connected). Hitting `max_runs` stops with
  `ImageError::TooManyRuns`. The frame is rejected (a flooded image is the
  Sun or Earth), never partially processed.
- `centroid`: fold runs into roots; per blob Σw, Σwx, Σwy, Σwx², Σwy², Σwxy,
  area, peak. Reject on area, saturation, or axis ratio from the second
  moments. Keep the brightest `keep` with `select_nth_unstable_by`. Pixel
  centres are at integer + 0.5 (§1.2's u, v convention, shared with
  `camera`).
- An optional 3×3 opening for hot pixels is left out: `min_area = 3` removes
  single hot pixels in the prototype. Revisit in P7 with the hot-pixel map.
- `testkit::splat(img, stars, psf_sigma, bg, read_noise, seed)`: the
  pixel-integrated Gaussian of §2.3 (A&S erf), Poisson and read noise, and
  hot pixels. Test-only; the sim plan's `render` replaces it when it lands.

Tests:

- Nominal: on `testkit` frames, every star above 8σ is found; the CoG error
  RMS is < 0.1 px at σ_PSF = 1 px; no allocations after `new` (a
  counting-allocator test under `std`).
- Edge: a star on the image border; two touching stars become one blob
  (rejected by axis ratio); a saturated star is rejected; a 1-px hot pixel is
  rejected; a uniform frame gives no blobs; a strong gradient is still flat
  after background.
- Failure: a frame of all 4095 gives `TooManyRuns` or zero blobs, never a
  panic; a wrong-length slice is an error.
- Oracle: on 100 `testkit` frames, centroids agree with tetra3
  `extract_centroids_fast` to 0.2 px for every star both find
  (dev-dependency).
- Perf: a `#[ignore]` test reports ms/frame for 1024² and asserts < 3 ms
  native release. Chunk J measures wasm.

Kani:

- `find` terminates and returns a root for any parent array where
  `parent[i] ≤ i` (≤ 8 runs).
- Union keeps `parent[i] ≤ i`.
- `label` never pushes past `max_runs` (symbolic 4×4 mask).

## H. Lost-in-space: Pyramid, verification, Monte Carlo

Files: `libs/metor-star/src/lis/{mod.rs, pyramid.rs, verify.rs,
binomial.rs, tests.rs}`, `libs/metor-star/tests/monte_carlo.rs`.

- `trait Identifier { fn start(&mut self, obs: &[V3]); fn step(&mut self,
  db: &Db, budget: u32) -> Step; }` with `Step::{Found(Match), Exhausted,
  More}`. `budget` counts triplets. The search state (the triplet cursor and
  the stamp epoch) lives in the identifier, so `step` resumes where it
  stopped. A hard cap on total triplets per frame (`max_triplets`) ends the
  search with `Exhausted`. Resuming is a safety net: if the benchmark below
  shows typical runs fit in one cycle, the system passes a budget that
  normally finishes in one call, and only an atypical frame spills into the
  next cycle.
- **Benchmark first.** The first commit of H is
  `libs/metor-star/benches/lis.rs`, written before the identifier is tuned.
  It covers the committed DB and a synthetic DB with typical (5–20 stars) and
  worst-case inputs:
  - no false stars;
  - 2 and 5 false stars among the brightest;
  - only false stars, which runs to `max_triplets`;
  - p_miss = 0.2.

  It reports, per case, the median, p99 and maximum time per frame, triplets
  per frame, and ns per triplet. It runs native release and, in J, under the
  pack's wasm runtime. The results go in the PR description and set
  `max_triplets` and the default budget. Nothing has been measured yet; the
  design's 10–50 ms is a literature figure.
- `Pyramid` (all buffers allocated in `new(max_obs, max_candidates)`):
  1. Precompute the image pair angles for the brightest `n ≤ 16`.
  2. Iterate triplets in Mortari's "smart" order (`dj, dk, i`).
  3. For each edge, look up `kvector.range(θ ± ε)` with
     ε = 3√2·σ_b + ε_cal.
  4. Intersect with an epoch-stamped `u32` array of `n_stars` (no clearing
     per triplet).
  5. Keep triangles whose handedness (sign of `bᵢ·(bⱼ×bₖ)`) matches.
  6. Require a unique fourth star r that matches all three.
  7. More than `max_candidates` pairs on an edge skips the triplet and counts
     it.
- `verify(db, cam, q, centroids, cfg) -> Verdict`: cone query at the
  boresight (half-diagonal), project every catalog star and match each
  centroid to its nearest prediction within `r_match` px. A centroid with two
  predictions within `r_match` is ambiguous and unmatched. Accept if:
  - `n_match ≥ 4`,
  - the residual RMS < 3σ_c,
  - `p_false < 1e-9` from `binomial::sf`, with log-gamma via `libm::lgamma`.

  The pipeline is identify → QUEST on the pyramid → verify → QUEST on all
  matches → verify again.
- `Match { rows: [u16; 32], n, q, p_theta }`, where `rows` are DB rows. The
  tracker maps them to HIP with `Db::hip` when it fills its outputs, and
  the Monte Carlo harness scores on HIP.

Tests (`lis/tests.rs`):

- Nominal: on a synthetic-catalog DB, noiseless projections of 4–20 stars
  identify correctly for 1000 random attitudes.
- Edge: exactly 4 stars; one image star is the mirror of a triangle
  (handedness rejects it); the brightest star is false.
- Failure: all false stars give `Exhausted` within the triplet cap, and
  `verify` rejects a correct-looking triangle with no fourth-star support.

Monte Carlo (`tests/monte_carlo.rs`): a vector-level harness, run on the
committed DB and on the synthetic DB.

- Per trial: a random attitude; project every catalog star with mag ≤ limit
  + 0.5 (to model faint-end incompleteness) through the camera; add centroid
  noise σ_c, magnitude noise of 0.2 mag (so brightness order is imperfect),
  drop each star with p_miss, and add n_false uniformly placed false stars
  with random brightness.
- Score per trial: `Correct` (attitude within 3σ and every id right),
  `FalseId` (accepted and wrong), or `NoSolve`. Also record the triplet count
  and wall time.
- Grid: σ_c ∈ {0.05, 0.1, 0.3} px × n_false ∈ {0, 2, 5} × p_miss ∈ {0, 0.1,
  0.2}.
- Gates (`#[ignore]`, 10⁴ trials per cell, run in CI nightly): `FalseId = 0`
  in every cell. Availability ≥ 99% at (0.1 px, 0, 0) and ≥ 95% at
  (0.1 px, 2, 0.1). **Both availability numbers are placeholders** until
  there is a mission requirement; the constants carry a `// placeholder`
  marker. The false-ID gate is not a placeholder. The default run is 500
  trials per cell.
- Cross-check: for 200 trials, tetra3 solves the same centroid set; where
  both solve, the attitudes agree within 3σ (dev-dependency; uses tetra3's
  Hipparcos path, not our DB).
- The worst-case triplet count over all trials is printed. Together with the
  ns-per-triplet from the benchmark, it sets the per-cycle budget in J.

## I. Tracking mode and the tracker state machine

Files: `libs/metor-star/src/{track.rs, tracker.rs, tracker/tests.rs}`.

- `fn predict(prior_q, prior_p_theta, ω, Δt) -> (Quat, M3)`: propagate to
  mid-exposure.
- `fn track(db, cam, prior, centroids, cfg) -> Result<Match, TrackError>`:
  1. Cone query.
  2. Project.
  3. Gate radius `r_g = k·√(σ_c² + (f/p)²·λ_max(P_θθ)) + |ω|·Δt·f/p`, with
     `λ_max` from the closed-form eigenvalues of a symmetric 3×3.
  4. Nearest neighbour, dropping a centroid whose second-nearest prediction
     is inside `2r_g`.
  5. QUEST if ≥ 3 matches, then `verify`.
- `Tracker::new(db, cam, ImageConfig, LisConfig, TrackConfig)` owns the
  `Extractor`, `Pyramid` and every buffer.
- `Tracker::process(&mut self, px, prior: Option<Prior>, budget) ->
  Outcome`:
  - `Prior` is used when `√λ_max(P_θθ) < FOV/10`.
  - `Lost`: run the extractor, then `Pyramid::step` within the budget;
    `More` keeps the search for the next call on the same frame.
  - `Tracking`: run `track`; on failure, fall back to `Lost` on the same
    frame.
  - The output is `StarVectors`-shaped data in `metor-star` types: vectors in
    body frame, ids, σ per star, and the solution.
- Per-star σ = `pixel_sigma_to_rad(σ_c)`, with σ_c a constant from config.
  A flux-based σ_c (§1.1's photon-limited model) is P7 fidelity work.

Tests:

- Nominal: a 100-frame `testkit` sequence at 0.1°/s: the first frame is
  `Lost` → `Tracking`, and every later frame is `Tracking` with
  `n_matched ≥ 6`.
- Edge: a prior 3σ off still tracks; a prior with P_θθ above FOV/10 uses LIS;
  ω·Δt smear widens the gate.
- Failure: a wrong prior (5° off, small P) gives `VerifyFailed` and falls
  back to LIS; a blank frame gives `NoStars`; a frame interrupted by budget
  resumes and solves on the next call.
- NEES of `p_theta` over 1000 tracked frames is 3 ± 0.3.

## J. `star_tracker` system

Files: `examples/adcs-fsw3/contracts/src/star.rs` (`StarCentroids`,
`StarTrackerParams`), `systems/adcs/src/star_tracker.rs`,
`systems/adcs/src/lib.rs`, `sim.py`, `tests/stars.rs`.

- Inputs: `image: Input<StarImage>` (26a) and
  `estimate: Input<AttitudeEstimate>` through a `fsw.loop(AttitudeEstimate)`,
  since nav runs after the tracker.
- Outputs: `star_vectors`, `star_solution`, `star_centroids`.
- `StarCentroids` (frame `star_centroids`) is one shaped component per field,
  so the panel overlay (26d chunk F) binds to it directly: `timestamp`,
  `frame_id: u64`, `count: u64`, `u: FrameArray<f32, 32>`,
  `v: FrameArray<f32, 32>`, `flux: FrameArray<f32, 32>`,
  `cat_id: FrameArray<u32, 32>`: the HIP number, matching
  `StarTruth.cat_id`, with `u32::MAX` for unidentified. `FrameArray`
  comes from 26a chunk A; a plain `[f32; 32]` field would be 32 components
  (`vtable.rs:37-47`). `u` and `v` are in 26a's pixel convention. **This is
  the interface to 26d**; renaming a field is a breaking change for the
  overlay preset.
- State: `Tracker`, built in `new` from `StarTrackerParams` and the DB
  (`include_bytes!("../../../../libs/metor-star/data/hip_v65.stdb")`, or
  `db_path` on native). It also holds the last processed `frame_id`.
- Each cycle:
  1. If a new `frame_id` arrived, start it.
  2. Call `process` with `budget` triplets.
  3. Write outputs once the frame resolves.

  A new frame arriving mid-search drops the old search (counted).
- The system runs in the wasm `adcs` pack as a cyclic system. Run H's
  benchmark and the extractor under the pack's wasm runtime. Set `budget` so
  one cycle stays under 2 ms. If typical frames fit, the resumable path is
  only a safety net. If the extractor alone exceeds 2 ms, split `label` by
  row band (the raster pass is resumable).
- `StarImage` and `StarTruth` come from 26c's contracts PR, which lands right
  after 26a chunk A. J consumes them and does not define them.
- `sim.py`: the star_sim or a CPU-render source from the sim plan feeds
  `image`. Until it exists, the system test drives the system directly with
  `testkit` frames.

Tests:

- Unit: frame-id dedupe; the mid-search frame drop; the budget split gives
  the same `StarVectors` as one large budget.
- Integration (`tests/stars.rs`): the coordinator with a test-only system
  writing `testkit` `StarImage`s and the matching `StarTruth` (HIP ids)
  from `BodyState` at 10 Hz. `star_tracker` →
  nav reaches the P0 gate (< 0.05°). `StarSolution` against truth: < 10″
  cross-boresight and < 60″ roll RMS at σ_c = 0.1 px.
- ID scoring: `fn score_ids(centroids: &StarCentroids, truth: &StarTruth,
  r_px: f32) -> IdScore { correct, wrong, unidentified, missed }` in
  `adcs-contracts` (or the test crate). It pairs each centroid with the truth
  centroid within `r_px` and compares `cat_id`s directly, since both carry
  the HIP number (`u32::MAX` for none). Unit tests: equal ids score
  `correct`; a different HIP scores `wrong`; `u32::MAX` on the tracker side
  scores `unidentified`; a truth star with no centroid scores `missed`; a
  false star (truth `u32::MAX`) matched to a HIP scores `wrong`. The
  integration run asserts `wrong == 0` over every frame.
- Failure: a blank image stream keeps nav on sun/mag with no star updates; a
  frame with 5 false stars and no catalog stars never emits `StarVectors`.

## What this plan uses from 26a

26a (`26a-image-records-plan.md`) provides, and this plan relies on:

- `FrameImage<P: Pixel, const W, const H>([[P; W]; H])`,
  `#[repr(transparent)]`, with `pixels() -> &[P]` and `rows()` (26a chunk A).
  The pixel type carries the bits: `Mono16<const BITS: u8 = 16>(u16)`,
  `#[repr(transparent)]`. So the field is
  `pixels: FrameImage<Mono16<12>, 1024, 1024>`, with no
  `#[frame(image(bits = 12))]` attribute. `star_tracker` sets G's
  `ImageConfig::saturation` to `(1 << Mono16::<12>::BITS) − 1`, since
  `metor-star` does not see the pixel type.
- `star_tracker` gets `&[u16]` with no copy:
  `<[u16]>::ref_from_bytes(image.pixels.pixels().as_bytes())`. `Mono16` is
  `IntoBytes` and has the alignment of `u16`, so the cast cannot fail on a
  frame borrowed from the ring. A failed cast is still handled as an error,
  not unwrapped.
- `FrameImage` is neither `Clone` nor `Copy` and has no `Default`. Producers
  build `StarImage` with `FromZeros::new_box_zeroed` (26a enables zerocopy's
  `alloc` feature). Every 2 MiB value in this plan is boxed at init and never
  moved by value. That covers J's test image writer and the `testkit` frames
  passed through records. `StarImage` cannot sit in a `Sample`, which
  requires `Copy`, so `star_tracker` reads it only through `Input::latest()`
  and its borrow.
- Stack size: the derived `Decomponentize` built a 2 MiB field by value and
  overflowed a 3 MiB debug stack in 26a's prototype. 26a's chunk A replaces
  it with an in-place `assign_component_view`. J depends on that change
  having landed: the wasm guest stack is far smaller than 3 MiB.
- Ring sizing (26a chunk B): `ring_bytes` caps a ring at 8 MiB by default,
  which is 3 records of a 2 MiB frame. A frame output has no scratch buffer.
  A wasm guest holds a copy of twice the real ring, so the `star_image` input
  costs the pack's guest about 16 MiB. J's PR records the guest's peak
  memory.
- `FrameArray<T, N>` (26a chunk A) for the id and centroid lists in
  `StarVectors` (C) and `StarCentroids` (J).
- `StarImage` and `StarTruth` are not defined by 26a. 26c owns them, in a
  small contracts PR right after 26a chunk A, because the simulator produces
  them.

Still open with 26a and 26c: the pixel-centre convention. G and D assume
pixel centres at integer + 0.5. The 26c renderer and the 26d overlay must
use the same convention. D's tests pin it.

## Open questions

Decided since the first draft:
- LIS is a resumable search with a per-cycle triplet budget inside the wasm
  pack. It is a safety net if typical runs fit in a cycle, which H's
  benchmark decides.
- Nav fuses vectors only, with no reset from the tracker's quaternion.
  Convergence from identity is the gate (C).
- One Joseph-form update path serves sun, mag and star vectors (A).
- tetra3 is a dev-dependency only.
- The availability targets stay as marked placeholders (H).
- The Hipparcos DB is committed with a CC BY-NC 3.0 IGO `NOTICE` (F).
- Per-star σ is a constant from config; flux-based σ is P7.
- `StarCentroids` is one shaped component per field (J; the interface to
  26d).

- `StarImage` and `StarTruth` come from 26c's contracts PR after 26a chunk A.
- Large-angle convergence: test C's cases as is. If needed, iterate the
  update within the frame while P_θθ is large; inflating R is the fallback.
- Lists in records use 26a's `FrameArray`; ids are `u32` with `u32::MAX` for
  unidentified.

No open questions remain for the user. The benchmark in H and the
convergence cases in C may reopen the budget and filter choices.
