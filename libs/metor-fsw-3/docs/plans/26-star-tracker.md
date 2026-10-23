# 26: Star trackers for adcs-fsw3

Status: design only. No code has been written. This complements `examples/adcs-fsw3/TODO.md` §11.1 (star-tracker images) and F13 (large records), and supersedes them where they conflict.

## 0. System overview

```
 plant ──BodyState/World──▶ star_sim (wgpu, AsyncSystem, ~10 Hz)
                               │  StarImage  (Frame: header + FrameImage<Mono16,1024,1024>)
                               │  StarTruth  (Frame: q_cam_eci, true centroids, blinded flags)
                               ▼
                           star_tracker (CPU, metor-star)  ◀── AttitudeEstimate (tracking prior)
                               │  StarVectors  (≤32 × (b_body, r_eci, σ), count)  ──▶ nav (MEKF.update_vectors)
                               │  StarSolution (q_b_eci, P_θθ, mode, n_matched, …) ──▶ telemetry / scoring only
 StarImage ──link──▶ db/gateway ──▶ metor-panel image_view (+ centroid overlay)
```

The shared interface is one fixed-size monochrome frame buffer carried as a **Frame record field**, not a msg:

```rust
#[derive(Frame)] #[repr(C)]
pub struct StarImage {
    pub timestamp: Timestamp,      // mid-exposure
    pub frame_id: u64,
    pub exposure_us: u32,
    pub gain_milli: u32,
    #[frame(image(bits = 12))]
    pub pixels: FrameImage<Mono16, 1024, 1024>,   // 2 MiB
}
```

`FrameImage` is a new `#[repr(transparent)]` zerocopy type modeled on `FrameBytes<N>` (`libs/metor-component/src/bytes.rs:11-37`).

- **Why a new type.** A plain `[[u16; W]; H]` emits one telemetry component per element, which would be about 1M components (`vtable.rs:37-47`, `metadata.rs:51-62`). `FrameImage` emits **one** component with shape `[H, W]`.
- **What the panel reads.** The component carries `image.*` metadata, which the panel keys on:
  - `image.format = mono16`
  - `image.bits = 12`
  - optional `image.black`, `image.white` and `image.stretch`
  - `lod = off`
- **Why a Frame field rather than a msg.** Readers borrow the frame zero-copy from the ring. Sibling fields such as exposure and frame id share the sample's timestamp. Msg logs have no shape and are capped at about 1 MiB today.

The default resolution follows the Terma T1 class: 1024×1024 at 12 bit stored in u16 (2 MiB), with about a 20° FOV. The ST-16-class 2592×1944 (7.6 MB) is a later stretch goal.

---

## 1. Star-tracker sensor (`libs/metor-star` + `star_tracker` system)

### 1.1 Image processing

**Background.** Use a local mean or median over a 15–25 px box, or a global sigma-clipped median. Tetra3 defaults to `local_mean` with `filtsize = 25`.

**Threshold.** A pixel is foreground when `T = μ_bg + kσ_bg`, with k ≈ 3–5. An optional 3×3 binary opening removes hot pixels.

**Segmentation.** Use two-pass union-find connected-component labelling with 8-connectivity. Reject a blob if:
- its area is outside about 3–100 px
- it is saturated
- its axis ratio is too high (a streak, cosmic ray or debris)

Keep the brightest 16–20 blobs.

**Centroid.** Background-subtracted centre of gravity:

  x_c = Σ x·(I − bg) / Σ (I − bg),  and likewise for y_c

- Typical error is 0.05–0.1 px with a defocused PSF of σ ≈ 0.7–1.2 px (Liebe 2002).
- A PSF that is too sharp gives the pixel-phase "S-curve" bias (Rufino & Accardo 2003).
- The photon-limited floor is σ_c ≈ σ_PSF / √N_ph. The full Cramér–Rao treatment is in arXiv:1910.00558.
- A Gaussian fit (Delabie et al. 2014) removes the S-curve and is deferred to P4.

**Smear.** Smear is ω·t_exp·f/p px. At 0.1°/s, 50 ms and 70″/px that is about 0.25 px, which is fine. Lost-in-space degrades above about 1°/s.

### 1.2 Camera model

**Pinhole.** With pitch p, focal length f and principal point (c_x, c_y):

  x = (u − c_x)·p,  y = (v − c_y)·p,  b_st = [x, y, f] / √(x² + y² + f²)

**Brown–Conrady distortion.**

  x_d = x(1 + k₁r² + k₂r⁴) + 2p₁xy + p₂(r² + 2x²)

Undistort with a few fixed-point iterations. The simulator uses the same parameters and the same sign convention, defined once in `metor-star::camera`.

**Mount.** b_body = A_b←st · b_st.

**Catalog vectors.** From right ascension α and declination δ:

  r = [cos δ cos α, cos δ sin α, sin δ]   (ICRF ≈ ECI J2000; the ~20 mas frame bias is ignored)

**Proper motion.** Propagate offline to the mission epoch:

  α += μ_α*·Δt / cos δ,  δ += μ_δ·Δt

**Aberration.** First order:

  b′ ≈ b + β − (b·β)b,  β = v/c

The annual term is about 20.5″ and LEO orbital motion adds about 5″. It is a parameter, off by default in both the sim and the tracker, and is added in P4.

### 1.3 Lost-in-space identification

| Algorithm | False-star robustness | DB size | Speed | Notes |
|---|---|---|---|---|
| Liebe triangle (1992) | weak (nearest-neighbour dependent) | small | fast | baseline |
| Grid (Padgett & Kreutz-Delgado 1997) | good vs. position noise, fragile nearest neighbour | moderate | linear scan | dense catalog |
| **Pyramid + k-vector** (Mortari et al. 2004) | **strong**: 4th-star confirmation | pairs only, ≲ 1–3 MB | ms | flight heritage; LOST < 350 KiB |
| Geometric voting (Kolomenkin 2008) | very strong | pair table | O(n²) queries | simple |
| Tetra / tetra3 / cedar-solve (Brown & Stubis 2017) | strong, binomial acceptance | large | fastest; FOV-agnostic | 4-star hashed patterns |
| ML (Rijlaarsdam 2020 NN, RPNet 2019, SIGNN 2025) | good in papers | weights | inference | hard to verify; not for FSW |

**Choice: Pyramid with a k-vector pair table, behind an `Identifier` trait.** Tetra-style hashing is the natural second implementation.

**k-vector (Mortari & Neta 2000).**

1. Sort the pair values s_k = cos θ_k.
2. Fit the line z(x) = m·x + q through the two ends of the sorted table.
3. Store k(i) = #{ s ≤ z(i) }.

A query for the range [a, b] reads indices k(⌊(a − q)/m⌋) + 1 … k(⌈(b − q)/m⌉), then trims the ends. The query tolerance is ε ≈ 3√2·σ_b, plus the distortion and catalog error budgets.

**Pyramid.**
1. Pick a triplet (i, j, k) with a "smart" index ordering.
2. Find the catalog pair candidates for each edge.
3. Keep only a triangle whose edges close.
4. Confirm it with a 4th star r, giving the pyramid.
5. Cap the number of iterations.

**Verification is mandatory.**
1. Predict every catalog star inside the FOV from the candidate attitude.
2. Count the centroids within r_match of a prediction.
3. Accept only if n_match ≥ 4 and the residual RMS is below 3σ.
4. Optionally add the Tetra binomial test:
   P_fp = 1 − BinomCDF(n_match − 1; n_img, p),  p ≈ n_cat_fov·π·r_match² / A_fov

### 1.4 Tracking mode (Samaan, Mortari & Junkins 2005)

1. Propagate the MEKF prior q̂, P_θθ to mid-exposure.
2. Run a cone query in the sky index and project the catalog stars into the image.
3. Gate each centroid at

     r_g = k·√(σ_c² + (f/p)²·λ_max(P_θθ)) + |ω|·Δt·f/p  px,  k ≈ 3–5

4. Take nearest-neighbour matches and drop ambiguous ones.
5. If at least 3 stars match, solve Wahba.

Fall back to lost-in-space when verification fails or when the 1σ of P_θθ exceeds FOV/10.

### 1.5 Attitude determination (Wahba's problem)

Wahba's loss (Wahba 1965):

  L(A) = ½ Σ aᵢ ‖bᵢ − A rᵢ‖²,  aᵢ = σ_tot² / σᵢ²

  B = Σ aᵢ bᵢ rᵢᵀ,  S = B + Bᵀ,  σ = tr B,  z = Σ aᵢ bᵢ × rᵢ

**Davenport q-method.** Build K = [[S − σI, z], [zᵀ, σ]]. The optimal q is the eigenvector of K for its largest eigenvalue λ_max.

**QUEST (Shuster & Oh 1981).** λ_max is the largest root of

  λ⁴ − (a + b)λ² − cλ + (ab + cσ − d) = 0

with a = σ² − κ, b = σ² + zᵀz, c = Δ + zᵀSz, d = zᵀS²z, κ = tr adj S and Δ = det S. Solve it with Newton's method starting from λ₀ = Σ aᵢ. Then:

  α = λ² − σ² + κ,  β = λ − σ,  γ = (λ + σ)α − Δ
  x = (αI + βS + S²) z,  q = [x; γ] / √(γ² + ‖x‖²)   (scalar-last)

QUEST is singular near 180° and needs sequential rotations there.

**Alternatives.**
- ESOQ2 (Mortari 1997) reduces the problem to 3×3.
- SVD (Markley 1988): with B = UΣVᵀ, A = U·diag(1, 1, det U·det V)·Vᵀ.
- FOAM (Markley 1993).

**Choice: QUEST as the baseline.** The q-method (4×4 Jacobi) is kept as a test oracle, not as flight code.

**QUEST with sequential rotations.** QUEST's formula for q divides by a quantity that vanishes when the attitude is close to a 180° rotation from the reference frame (γ → 0 and x → 0). Shuster & Oh's method of sequential rotations avoids this:

1. If γ is small, rotate the reference vectors 180° about the x, y or z axis. That is a sign flip of two components, so it costs nothing.
2. Re-solve, picking the axis that makes |γ| largest.
3. Undo the rotation by composing the result with the matching 180° quaternion.

In tracking mode the prior attitude tells us which axis to choose before solving. Accept when |γ| / ‖[x; γ]‖ > about 0.1.

**Newton step for λ_max.**
- Start from λ₀ = Σaᵢ. Star-tracker residuals are tiny, so λ_max is within about 1e-10 of λ₀ and 1–2 iterations are enough.
- Evaluate the characteristic polynomial in a numerically stable form.
- Cap the iterations at 5. If they don't converge, flag the solution invalid rather than fall back to the q-method.

**Precision.** Use f64 throughout. QUEST's known accuracy loss comes from forming S² and adj S in f32 with nearly-parallel vectors. That is exactly the narrow-FOV case, so f32 is not an option.

**Covariance.** P_θθ is unchanged: it is a property of the measurements, not of the solver.

**Measurement covariance: the QUEST measurement model (Shuster 1989/1990).** Model each body vector as bᵢ = A rᵢ + vᵢ, with E[vvᵀ] = σᵢ²(I − bᵢbᵢᵀ). Then

  P_θθ = [ Σ σᵢ⁻² (I − bᵢbᵢᵀ) ]⁻¹

The per-star error is σᵢ ≈ σ_c·p/f. For example, 0.1 px × 5.5 µm / 16 mm ≈ 34 µrad ≈ 7″.

Roll, about the boresight, is weak: typically 6–10× worse than cross-boresight. Liebe's rules of thumb (constants to verify):

  ε_cb ≈ FOV·ε_c / (N_pix·√N)
  ε_roll ≈ atan(ε_c / (0.3825·N_pix)) / √N

### 1.6 Feeding the MEKF (`libs/metor-adcs/src/mekf.rs`)

**Decision: nav fuses the matched star vectors directly.** The tracker's quaternion is published, but nav does not use it.

**MEKF change.** `State::estimate_attitude<const N>` propagates and runs vector updates in one call. Split it into:
- `propagate()`
- `update_vectors(&[(b, r, σ)])`, which takes a variable number of stars per frame

`estimate_attitude` keeps its current behaviour, now built from those two.

**Each star update** uses the same model as the existing sun and magnetometer updates:
- e = b − A(q̂)r
- H = [[b̂×], 0₃]
- R = σ²I

**Why vectors and not the quaternion.** For a single frame with at least 2 stars and a good prior, the two are equivalent to first order (Shuster's QUEST measurement model). Vectors are still better in these cases:
- 1–2 matched stars: a single-frame solve fails, but each vector still constrains 2 axes.
- per-star timestamps, which a rolling shutter needs
- per-star chi-square outlier rejection
- later estimation of camera misalignment or focal length

It is also the same code path nav already uses for the sun and magnetometer.

**Bootstrap.** The MEKF starts from the identity with no attitude seed (`mekf.rs:100`, `nav.rs:45-52`). It converges from vector updates alone, because each multiplicative update re-linearises around the new q̂. No reset from the tracker's quaternion is needed. Until the filter's P_θθ is small enough to use as a tracking prior, the tracker runs lost-in-space on every frame.

**The tracker's own QUEST solve** is still needed:
- to verify lost-in-space matches against the other catalog stars before accepting them
- to fill `StarSolution` for plotting and scoring against `StarTruth`

**Latency.** The vectors are stamped at mid-exposure t_img. Either rotate b to nav time with ω_b·Δt and inflate σ by σ_g·√Δt, or run the update when t_img falls within the current cycle.

**Conventions.** Pin the nox quaternion convention (body←ECI, scalar position, and the sign of the `integrate_body` error) with a unit test.

**Optional, later.** `update_attitude(q, P_θθ)`, with residual δα = 2·vec(q̂⁻¹ ⊗ q) and H = [I₃ 0₃], would emulate a commercial tracker that outputs only a quaternion. It could also cross-check against vector fusion; the two should agree to first order.

### 1.7 Onboard catalog / DB

**Source.** Hipparcos (VizieR I/239), V ≤ 6.5–7.5, with proper motion propagated to epoch 2024.0.

**Processing.**
1. Merge double stars within 2–3 px.
2. Drop variables with amplitude above 0.5 mag.
3. Optionally keep only the brightest K stars per sky bin ("uniformise").

**Contents.**
- Star table: `[f32;3]` direction, `i16` mag×100, `u32` id.
- Cube-face or HEALPix sky index.
- Pair table for θ < FOV_diag, sorted by cos θ, plus the k-vector.

**Size.** There are about N²(1 − cos θ_max)/4 pairs. With N = 5k and θ_max = 20° that is about 380k pairs × 8 B ≈ 3 MB; uniformising cuts it to under 1 MB.

**Format.** A flat little-endian file with a header (magic, version, FOV, mag limit, epoch, CRC), read zerocopy. It is embedded with `include_bytes!` in the wasm pack or loaded from `db_path` on native.

**Built by.** `libs/metor-star/tools/star-db`.

**Sizing check.** Expected stars in the FOV are N(M)·Ω/4π. For example, 15°×15° at V ≤ 6 gives about 27. Size the magnitude limit so that P(fewer than 4 stars) < 0.1%, checked by Monte Carlo over random boresights.

### 1.8 Why these algorithms

**Criteria, in priority order:**
1. **False-ID rate.** An accepted wrong identification is the dangerous failure: it injects a confident, wrong attitude. Failing to solve only costs a frame. Images will contain hot pixels, cosmic-ray hits, planets and debris that are not in the catalog, and catalog stars that are missing (too faint, blended, variable).
2. **Determinism and bounded run time.** It runs inside a flight-software cycle, so the worst case matters more than the average.
3. **Database size.** It has to embed in the wasm `adcs` pack.
4. **Implementation risk.** Prefer something that is published in enough detail and has open implementations to test against.
5. **Speed.** This is the lowest priority. Lost-in-space only runs on acquisition, or when tracking mode fails, so even 10–50 ms is acceptable.

**How the candidates score:**
- **Liebe triangle.** Rejected. It matches a triangle built from a star's nearest neighbours. If one neighbour is a hot pixel, or a real neighbour is missing, the triangle is wrong, and nothing in the method recovers.
- **Grid (Padgett & Kreutz-Delgado).** Rejected. It handles position noise well, but it orients the pattern on the nearest neighbour, which has the same fragility. It also needs a dense, uniform catalog, and lookup is a linear scan unless we add an index.
- **Geometric voting (Kolomenkin et al.).** A credible alternative. It is the most robust to false stars, because every image pair votes and a few bad pairs are outvoted. Its cost is O(n²) range queries plus a separate verification step. It uses **the same pair table and k-vector as Pyramid**, so it is cheap to add later as a second `Identifier`.
- **Tetra / tetra3 / cedar-solve.** The strongest alternative.
  - For: it is the fastest of the group, it can work without knowing the FOV, and it has a principled binomial acceptance test. tetra3 and cedar-solve are mature open-source references.
  - Against: we gain little from those strengths. We know our FOV and calibration, and lost-in-space is rare, so speed matters little.
  - Its costs are real:
    - Its pattern database is by far the largest of these methods, which matters for the wasm pack.
    - It quantises the pattern's edge ratios into hash bins. Centroid noise near a bin edge needs multi-bin lookups, and the bin width has to be tuned.
    - It needs four real stars among the brightest few, so false stars among those brightest few cost it more patterns.
  - It is the planned second `Identifier`, if acquisition speed or calibration-free operation ever matters.
- **ML (Rijlaarsdam NN, RPNet, SIGNN).** Rejected for flight software. The results are good in papers, but there is no bound on the false-ID rate, the behaviour depends on the training set, and verifying it means trusting the weights. The verification step we would need anyway does all the work of guaranteeing correctness.
- **Pyramid + k-vector (Mortari et al.).** Chosen.
  - **False stars.** The 4th-star confirmation makes an accidental match of a triangle plus a fourth star very unlikely: Mortari et al. report false-ID rates near zero with many false stars present. Its "smart" ordering of index triplets tries combinations so that a single bad star is cycled out quickly instead of poisoning every attempt.
  - **Size and speed.** The database is only the pair table, under 1–3 MB, and less once uniformised. A k-vector range lookup is O(1) plus the number of hits.
  - **Bounds and implementation.** Capping the number of iterations bounds the worst case. It has flight heritage, and LOST (Polyakov et al. 2023) gives an open implementation to test against.
  - **Shared database.** The pair table serves both Pyramid and geometric voting.

In every case, acceptance is decided by the verification step (§1.3), not by the identifier: predict all catalog stars in the FOV and require enough matches with a small residual. That makes the false-ID guarantee mostly independent of which identifier proposed the match. It is why the `Identifier` trait is a safe place to swap in Tetra or voting later.

**Why QUEST for the attitude solve:**
- **The alternatives.** All of these solvers minimise the same Wahba loss, so their accuracy is identical. They differ in speed and robustness:
  - The q-method needs a 4×4 eigendecomposition.
  - SVD needs a 3×3 SVD.
  - QUEST replaces both with a quartic and 1–2 Newton steps, then one 3×3 matrix–vector product.
  - ESOQ2 is about as fast as QUEST but less widely implemented.
  - FOAM is comparable to QUEST.
- **Flight standard.** QUEST is the standard flight algorithm and the one most onboard references use.
- **Its two known weaknesses are both handled.** The 180° singularity is handled by sequential rotations, and the precision loss by using f64.
- **The q-method stays as the test oracle.** QUEST must agree with it to about 1e-12 on noiseless inputs, including near 180°.

### 1.9 Placement

| Piece | Location | Contents / notes |
|---|---|---|
| Algorithms | `libs/metor-star` (no_std + alloc + libm, no nox or fsw types) | modules `image`, `camera`, `catalog`, `lis` (Pyramid), `track`, `wahba`, and `render` (CPU reference renderer, feature `sim`) |
| `star_tracker` system | the `adcs` pack | one system with a Lost/Tracking state machine; processes only new `frame_id`s |
| `star_tracker` I/O | | inputs `StarImage` and `AttitudeEstimate` (tracking prior); outputs `StarVectors` (≤ 32 × `(b_body, r_eci, σ, cat_id)` + count, the nav input), `StarSolution` (q, P_θθ, mode, n_matched, residual; telemetry only) and an optional debug `StarCentroids` (≤ 32 × `(u, v, flux, cat_id)`) |
| Pipeline stages | inside `star_tracker` | pure functions, not separate systems, so variable-length centroid lists never travel through rings |
| `nav` | the `adcs` pack | gains `Input<StarVectors>` |

---

## 2. Star simulator (`star_sim`, wgpu)

### 2.1 Catalog for rendering

| | Hipparcos | Tycho-2 | Gaia DR3 |
|---|---|---|---|
| Stars | ~118k | 2,539,913 (+ suppl.) | 1.81 B |
| Complete to | V ≈ 7.3–9 | 99% @ V 11.0 | G 12–17 (bright end: ~20% of G < 3 missing, saturates < G 5–6) |
| Bands | Hp, BT/VT | BT/VT | G, BP, RP |
| Epoch | J1991.25 | ~J1991.5 / J2000 | J2016.0 |
| License | **CC BY-NC 3.0 IGO** | **CC BY-NC 3.0 IGO** | **CC BY-NC 3.0 IGO** |

The renderer must draw stars about 3–4 mag fainter than the tracker detects, because they add to the background and cause blends and false detections. It must also render from a different, deeper catalog than the tracker's DB, so the tests don't use the tracker's own data as truth (the "inverse crime").

**Default.** Gaia DR3 with G ≤ 12 (3,087,828 stars, measured by TAP query), with the bright end filled in from Hipparcos (about 18.7k of the 118,218 Hipparcos stars have no Gaia match). All three catalogs are CC BY-NC 3.0 IGO, so switching to Tycho-2 does not avoid the license. **Decision:** commit the G ≤ 8 catalog (62,723 stars, about 1.1 MB) with a NOTICE file. The G ≤ 12 file (49.4 MB) is not committed; it is fetched by sha256 into a cache. G ≤ 12 is the ceiling for now. G ≤ 14 (16.8 M stars, 270 MB) exceeds wgpu's default 128 MiB buffer limit.

**Packing.** Each star is 16 B:
- unit vector as f32×3
- mag as i16 millimag
- BP−RP as i16 millimag (f16 needs a WGSL capability that lavapipe may lack)

Proper motion is applied offline to 2024.0, so it is not stored. Flux is computed in the shader from mag, colour and the sensor band model, so changing the sensor needs no catalog rebuild.

### 2.2 Resident vs streamed

| Cut | Stars | Size @16 B | Plan |
|---|---|---|---|
| V ≤ 8 | 47k | 0.75 MB | committed test catalog |
| V ≤ 10 | 374k | 6 MB | |
| **G ≤ 12** | 3.09 M | 49.4 MB | **default, fully resident** |
| V ≤ 14 | 13 M | 211 MB | still resident |
| G ≤ 16 | ~100 M | ~1.6 GB | would need streaming |

**Decision: fully resident on the GPU, sorted by HEALPix cell.**

**Layout.** Cells at nside = 16 (3,072 cells of about 3.7°), with a `u32[3073]` offset table. Each frame:
1. The CPU selects the cells within (FOV half-diagonal + one cell + motion-blur sweep), about 30–60 cells for a 20° FOV.
2. It uploads (offset, count) pairs.
3. An indirect dispatch touches only those stars.

**Streaming later.** The same file layout supports streaming without change: mmap the file and keep an LRU of cells.

### 2.3 Shaders vs compute

The pixel-integrated Gaussian PSF for a star with N electrons at (x₀, y₀):

  E_pix = N · ¼[erf((x₁ − x₀)/√2σ) − erf((x₀′ − x₀)/√2σ)] · [same in y]

WGSL has no `erf`, so use Abramowitz–Stegun 7.1.26 (|ε| < 1.5e-7) on both the GPU and CPU. Motion blur sub-steps the projection between the start and end attitudes, using 8–32 steps scaled to the streak length.

| | Raster (instanced quads, additive blend into R32F) | **Compute scatter (fixed-point u32 atomicAdd)** | Tile gather |
|---|---|---|---|
| Determinism | float blend order; R32F blending needs `FLOAT32_BLENDABLE` | **bit-exact: integer adds are associative** | deterministic only if lists are sorted |
| 1k–50k visible stars | ≪ 1 ms | ≪ 1 ms | overkill |
| 1M+ visible | fine | a few ms, contention | best |
| Simplicity | pipeline, blend state, 256 B row padding | **storage buffers only, feeds the noise pass directly** | most code |

**Decision: compute.** Raster buys nothing without a surface or depth buffer. The tile gather is kept as an optimisation only if G ≥ 14 with a wide FOV turns out to be contention-bound.

The splat uses a scale of 1/256 e⁻ per count, which gives 16.7 M e⁻ of range per pixel, far above full well.

### 2.4 Sensor pipeline (compute passes, in electrons until the ADC)

1. **Cull and project.**
   - Apply aberration (orbital plus Earth heliocentric velocity).
   - Rotate with q_cam_b ⊗ q_b_eci at the start and end of the exposure, the end attitude being q ⊗ exp(½ω t_exp).
   - Project with the pinhole model plus Brown–Conrady.
   - Write a compacted visible list.
2. **Photometry.**

     e⁻ = F₀·10^(−0.4 m_band)·A_ap·τ·QE·t_exp

   m_band comes from G plus a colour term in BP−RP, fitted offline to the sensor's QE curve. The anchor is V = 0 ≈ 8.8e5 ph/s/cm² over the V band. As a check, 2 cm aperture, mag 7, QE·τ = 0.5 and 100 ms give about 219 e⁻.
3. **Splat** the PSF with σ = 0.6–1.5 px, fixed-point atomic add.
4. **Per-pixel noise.**
   - mean = signal + zodiacal background (V ≈ 22–23 mag/arcsec²) + dark·t·2^((T−T₀)/6.5) + a hot-pixel map
   - Poisson draw: Knuth below λ = 30, Gaussian above
   - read noise N(0, σ_r)
   - full-well clamp (blooming optional)
   - gain and bias, then quantise to 12-bit stored in u16
5. **Bright bodies.** Earth and Moon disks by per-pixel ray–sphere test; a sun exclusion cone of 30–45° with a stray-light gradient. Phase 1 only sets a `blinded` flag.

**RNG.** A stateless PCG hash (Jarzynski & Olano, JCGT 2020) keyed on (frame_seed, pixel, draw#). frame_seed comes from `metor_fsw_3::rng::Rng::seeded`.

### 2.5 Runtime

**Readback.**
- Keep the image in a storage buffer, not a texture, which avoids 256 B row padding.
- Pack u16 pixels into u32, then `copy_buffer_to_buffer` into a MAP_READ buffer, `map_async`, and `poll(Wait)`.
- A synchronous one-shot readback at 10 Hz should take about 1–5 ms (estimate, not measured). A 2-deep staging ring is only needed above 30 Hz.

**Headless.** Request an adapter with `compatible_surface: None`. This gives Metal on macOS and Vulkan on Linux.

**No GPU.** Two fallbacks:
- lavapipe, which wgpu's own CI uses. Run it via `orb` on nixos.
- A CPU reference renderer (`metor-star::render`) using the same math, at 10–40 ms per frame. It is also the test oracle.

**Execution.** An `AsyncSystem` on `thread = "startracker"`. It waits on `BodyState` and `World`, and exposes every 1/rate seconds of sim time. Each record is stamped at mid-exposure.

**Crate placement.**
- `examples/adcs-fsw3/systems/startracker-sim/` is its own pack, because wgpu must stay out of the wasm `adcs` pack.
- It uses `wgpu = "30"` (same as metor-panel; move it to `[workspace.dependencies]`), plus `bytemuck` and `pollster`.

**Outputs.**
- `StarImage`
- `StarTruth`: q_cam_eci, ω, n_rendered, n_detectable, blinded flags, and the true pixel centroids of the brightest 32 stars

**Catalog tool.** `tools/starcat/build.py` (uv, astroquery.gaia):
- partitions the ADQL queries by `source_id >> 35` (HEALPix level 12)
- propagates the stars to 2024.0
- HEALPix-sorts them and writes `starcat.bin`

Only the V ≤ 8 catalog is committed. The full catalog is fetched by sha256 into a cache.

---

## 3. Image display in metor-panel

### 3.1 Formats

| Format | Display path (to BGRA8) |
|---|---|
| Mono8 | 256-entry LUT |
| Mono16 / 12-bit | 2^bits-entry LUT (4–64 KiB), rebuilt only when the display parameters change |
| mono_f32 | affine transform, then stretch |
| RGB8 / RGBA8 | swizzle, plus an optional levels LUT |
| Bayer 8/16 | raw as mono first, then bilinear or superpixel demosaic |

**Stretch modes.** Manual black/white levels; auto percentile (0.5% / 99.95%, either locked or recomputed every frame); asinh, log and sqrt curves; gamma; histogram equalisation. **Colormaps.** Gray, inferno, viridis, and a saturation map that paints pixels at 2^bits − 1 red. Every integer format collapses to a single LUT lookup per pixel.

### 3.2 Rendering

gpui's `RenderImage` is immutable BGRA in a sprite atlas. Each frame needs a new image, and the previous one is freed with `drop_image`. The panel already does this in the spectrogram, map tiles and viewer_3d. The atlas sampler is bilinear, so zooming in on a full-resolution image would blur the pixels.

**Decision: resample to the viewport on the CPU.**
- Work on the background executor, with rayon over rows, and produce only the visible viewport at device resolution.
- Zoomed in: nearest-neighbour, so pixels stay crisp.
- Zoomed out: max-pool for mono, so single stars and hot pixels survive downsampling; box filter for RGB.

**Cost.** About 1–2 ms for the LUT and histogram on a 1024² frame, and under 1 ms to resample, which leaves comfortable headroom at 30 Hz. The cost is bounded by the pane size, not the sensor size.

**Re-render** only when the sample, the display parameters or the viewport change, with at most one render in flight. A gpui texture primitive is a later option, only if profiling shows the CPU path is the bottleneck.

### 3.3 Data path

**Reader.** Reuse `SelectedReader` (`libs/metor-panel/src/temporal/samples.rs:55-75`). It is keyed per (db, component), follows `view_time`, so live and scrubbed views share one path, and reads history through `at_or_before`. The image pane reads `sample.bytes: Arc<[u8]>` directly. Overlays use their own readers at the same `view_time`, so they stay time-aligned.

**Framework blockers that must be fixed first:**

1. **DB write-ahead log.** It allocates `schema.size() * 1024` up front (`libs/db/src/lib.rs:921,948,976`), which is about 2 GiB per 2 MiB component. Replace it with a byte budget, e.g. `clamp(size*1024, size*4, 64 MiB)`.
2. **LoD.** Level-of-detail runs on every component (`libs/db/src/lod.rs:138`). Honour `lod=off` and skip components with `image.format`.
3. **Time-series node size.** `NODE_SIZE` is 32 MiB (`libs/db/src/time_series.rs:61`). Change it to max(32 MiB, size × 64).
4. **fsw-3 link.**
   - `conn_cap` defaults to 1 MiB and is already configurable per link in Rust and Python; the size check is what's missing.
   - The batcher silently never sends a port larger than that (`link/batch.rs:29-55`).
   - The deframer rejects it as well.
   - Fix: raise `conn_cap` per link, and make an oversize port a config-time error in `coordinator/build.rs`.
5. **Streams.**
   - `FixedRate` sends every visible component at 60 Hz. Add per-subscription decimation, or exclude images unless a client requests them.
   - `GetTimeSeries` truncates the length to u32. Clamp `limit` by bytes.
6. **Ring sizing.** The global `ring_depth` (default 8) multiplies the slot count, so a 2 MiB frame gets a 32 MiB ring, and thread mirrors are 4× that. Fix: an 8 MiB cap per ring (at least 2 records) and smaller mirrors for large rings (plan 26a).

### 3.4 UI: the `image_view` pane

**Registration.** Register it in `views/dashboard/widgets.rs`, `tiles/panels.rs` and `inspector/palette.rs`. It is separate from the static PNG `image` widget.

**Ways to open one:**
- the add flow, which lists only components that carry `image.format`
- "Open as image" in the component browser and outline
- a "Star tracker" preset layout shipped by adcs-fsw3

**Config.**
- component and ROI
- display settings: stretch, levels, gamma, colormap, auto mode
- histogram and crosshair toggles
- `overlays: Vec<Overlay>`

**Interaction.**
- wheel zoom about the cursor, drag to pan, fit / 1:1
- hover readout of x, y and the **raw** DN, plus sibling fields such as exposure and frame_id
- ROI statistics
- a log-y histogram with draggable black/white handles

**Overlays.** `Overlay::Points` draws `StarCentroids` (detected) and `StarTruth` (true) in different colours, sized by magnitude. Later: a boresight/FOV graticule and a mask.

**Video.** A `FrameSource` trait lets an H.264/video-toolbox source plug into the same renderer later.

---

## 4. Phased plan

| Phase | Deliverable | Test gate |
|---|---|---|
| **P0 Fusion plumbing** | MEKF split (`propagate` / `update_vectors(&[..])`); an "ideal star tracker" that writes `StarVectors` from truth (random in-FOV catalog stars + σ noise) and a `StarSolution` from QUEST over them; nav wiring | quaternion-convention unit test; convergence test starting from identity: attitude error ~1° → < 0.05° |
| **P1 Image record + framework** | `FrameImage` + `image.*` metadata; db WAL budget, `lod=off`, node size; link `conn_cap` check; `StarImage` / `StarTruth` contracts | 1024² Mono16 at 10–30 Hz through link → db → `GetTimeSeries` (extend `libs/metor-fsw-3/tests/link.rs`) |
| **P2 Catalogs + CPU render** | `metor-star::{camera, catalog, render}`; `star-db` tool (Hipparcos DB); `starcat` tool (V ≤ 8 committed, G ≤ 12 cached) | pixel ↔ vector round trip at 1e-9 px; rendered centroid vs projected truth < 0.02 px |
| **P3 Panel image_view (min)** | Mono8/16, manual + auto levels, zoom/pan, readout, `SelectedReader`-backed scrubbing | view CPU-rendered frames in sim.py |
| **P4 Tracker chain** | background/CCL/CoG; QUEST (sequential rotations, q-method as test oracle) + P_θθ; tracking mode; Pyramid + k-vector + verification; Lost/Track state machine; `StarCentroids` | unit tests (k-vector vs brute force, QUEST vs q-method oracle incl. near 180°, NEES ≈ 3); Monte Carlo: false-ID = 0 over 10⁴ attitudes, availability target |
| **P5 wgpu simulator** | compute cull/splat/noise/pack, readback, AsyncSystem, HEALPix-resident catalog | GPU vs CPU agreement on lavapipe (centroids < 0.01 px, total e⁻ within 1e-4) |
| **P6 Closed loop + panel polish** | sim.py and target.py with star_sim → star_tracker → nav; centroid overlays, histogram, asinh; preset layout | closed-loop convergence using real images |
| **P7 Fidelity** | noise and hot pixels, motion blur, aberration (Earth velocity), distortion, Earth/Moon/Sun exclusion, Gaussian centroiding, Tetra `Identifier`, RGB/Bayer formats, stream decimation | arcsec accuracy table: cross-boresight vs roll, NEA vs bias |

**Order.** P0 and P1 are independent and can run in parallel. P3 needs only P1 plus any image source.

## 5. Decisions

- **Licensing.** Commit the G ≤ 8 catalog and the Hipparcos-derived tracker DB with a NOTICE file (CC BY-NC 3.0 IGO attribution). Hosting the G ≤ 12 file is still open.
- **Resolution.** 1024² Mono16<12> (2 MiB).
- **Storage.** Full rate is fine for now (about 216 GB/h at 30 Hz). Revisit db storage and decimation for images later.
- **Reproducibility.** A blocking 1–5 ms readback per frame in deterministic builds is accepted.
- **Ephemeris.** Moon position and Earth heliocentric velocity come from `rsofa` (already a dependency of nox-frames) and go into `World`.
- **gpui.** No patch; `RenderImage` at viewport resolution is enough.
- **Lost-in-space.** A resumable search with an iteration budget per cycle, kept in the wasm pack. Benchmark first; the resumable path is a safety net.
- **MEKF.** No reset from the tracker's quaternion. One Joseph-form update path for sun, magnetometer and star vectors.
- **Centroids.** Published as one shaped component per field, so the panel overlay reads them directly.

Per-component plans: `26a-image-records-plan.md`, `26b-star-tracker-sensor-plan.md`, `26c-star-sim-plan.md`, `26d-panel-image-view-plan.md`.

## 6. Open questions

1. **Hosting** for the G ≤ 12 catalog file.
2. **Constants to verify.** Liebe's roll and density formulas, and Tetra3 DB sizes.

## References

**Star tracking and attitude**
- Wahba 1965, SIAM Rev. 7(3):409, doi:10.1137/1007077
- Shuster & Oh 1981, JGCD 4(1):70, doi:10.2514/3.19717 (QUEST)
- Lefferts, Markley & Shuster 1982, JGCD 5(5):417, doi:10.2514/3.56190 (MEKF)
- Markley 1988, J. Astronaut. Sci. 36(3):245 (SVD)
- Markley 1993, JAS 41(2):261 (FOAM)
- Mortari 1997, AAS 97-167 (ESOQ2)
- Shuster 1989, JAS 37(1):79
- Shuster 1990, JAS 38(3):377 (QUEST measurement model) [volume/pages unverified]
- Mortari & Neta 2000, AAS 00-128 (k-vector)
- Mortari, Samaan, Bruccoleri & Junkins 2004, Navigation 51(3):171, doi:10.1002/j.2161-4296.2004.tb00349.x (Pyramid)
- Samaan, Mortari & Junkins 2005, IEEE TAES 41(4):1246, doi:10.1109/TAES.2005.1561885 (recursive mode)
- Liebe 1992, IEEE AES Mag. 7(6):34
- Liebe 2002, IEEE TAES 38(2):587, doi:10.1109/TAES.2002.1008988
- Padgett & Kreutz-Delgado 1997, IEEE TAES 33(1):202, doi:10.1109/7.570743
- Kolomenkin et al. 2008, IEEE TAES 44(2):441, doi:10.1109/TAES.2008.4560198
- Spratling & Mortari 2009, Algorithms 2(1):93, doi:10.3390/a2010093
- Rijlaarsdam et al. 2020, Sensors 20(9):2579, doi:10.3390/s20092579
- Rijlaarsdam et al. 2020, Sensors 20(13):3684 (NN)
- Xu, Jiang & Liu 2019, IEEE Access 7 (RPNet)
- Hepburn-Dickins et al. 2025, WACV (SIGNN)
- Brown & Stubis 2017, SmallSat (Tetra); tetra3 docs; cedar-solve
- Polyakov et al. 2023, SmallSat (LOST)
- Rufino & Accardo 2003, Acta Astronautica 53(2):135
- Delabie et al. 2014, JAS 61:60
- arXiv:1910.00558 (theoretical accuracy limits)
- Lang et al. 2010, AJ 139:1782 (astrometry.net)
- Markley & Crassidis 2014, *Fundamentals of Spacecraft Attitude Determination and Control*, Springer, ch. 5–6
- Wertz 1978, *Spacecraft Attitude Determination and Control*

**Catalogs, photometry, rendering**
- ESA 1997, SP-1200 (Hipparcos/Tycho)
- Høg et al. 2000, A&A 355:L27 (Tycho-2)
- Gaia DR3 (Vallenari et al. 2023, arXiv:2208.00211)
- Gaia license: cosmos.esa.int/web/gaia-users/license
- Riello et al. 2021 (G–V transforms)
- Bright-end incompleteness: arXiv:2012.06242 and arXiv:2205.12874
- hnsky.org star counts
- CHEOPS zodiacal background: arXiv:2001.05587
- INAF LSPE-STRIP TN-023
- Jarzynski & Olano 2020, JCGT 9(3) (GPU hashes)
- wgpu-types `Features` docs
