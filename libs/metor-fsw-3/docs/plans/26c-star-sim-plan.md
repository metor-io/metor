# 26c: Star-field simulator and catalog tool, plan

Implements the render and catalog parts of `26-star-tracker.md` §2 and
phases P2 (catalog and CPU render), P5 (wgpu simulator) and P7 (fidelity).
The tracker, its camera model and its onboard DB are in 26b
(`26b-star-tracker-sensor-plan.md`). The image record and framework changes
are in 26a (`26a-image-records-plan.md`).

Each chunk lands as one PR, gated on `cargo test`, `cargo clippy`, and
`pytest` for chunks that change Python. GPU tests run locally on Metal and on
lavapipe through `orb`. Each chunk ends with an antagonistic review before
the next one starts.

Order: A → B → C → D → E → F → G → H → I → J → K.

- A can start now.
- B (contracts) lands right after 26a chunk A.
- C needs 26b's crate skeleton (26b chunk B).
- D needs 26b's `camera` (26b chunk D).
- E needs B and 26a's ring cap (26a chunk B).
- G to K are P7 and can be reordered.

## Decisions taken

- **Licence.**
  - The G ≤ 8 catalog (about 1.1 MB) is committed, with a `NOTICE` giving
    the CC BY-NC 3.0 IGO attribution for Gaia ("ESA/Gaia/DPAC"), Hipparcos
    and Tycho-2 ("ESA").
  - The G ≤ 12 file is not committed. It is fetched into a cache and checked
    by sha256. Where it is hosted is still open.
- **Readback.** A blocking readback of 1–5 ms per frame is accepted,
  including in deterministic builds, where it runs on the coordinator
  thread.
- **Depth.** G ≤ 12 is the ceiling. A catalog that does not fit the device's
  default binding limit is an init error, not a reason to raise limits.
- **Ephemeris in `World`.** The Moon's position and Earth's heliocentric
  velocity go into the existing `World` record, beside `sun_eci`.
- **Sensor band.** Until P7 the band is G plus a fixed colour term. Fitting
  that term to a QE curve is chunk K (P7).

## Facts the plan relies on

Code:

- **Async systems.**
  - `AsyncSystem` runs once until `stop` (`libs/metor-fsw-3/src/async_system.rs:21-36`).
  - A fn-style async system may not take `Sample`, a `Cap`, or the `Rng` param
    (`fn_system/mod.rs:91-104`, `DefError::RngOnAsync`). So `star_sim` takes
    `seed` as a param and builds its own `Rng::seeded` (`rng.rs:10`).
  - Async systems have no `now`, so `star_sim` uses `BodyState::timestamp`
    as sim time. `Input::next` parks until a record arrives (`port/mod.rs:275`).
  - `register_async_system` and `register_async` (`coordinator/table.rs:245,276`)
    place a system on the thread its adapter names. Python sets the thread
    with `add(..., thread="...")` (`metor_config/_target.py:154-174`), as
    `sim.py:72` already does for the db.
  - In a deterministic build a thread group runs on the coordinator thread
    once a cycle (`thread/os.rs:7-9`).
- **Frames and rings** (from 26a's facts):
  - A `Frame` encodes as `self.as_bytes()` and does not use the scratch
    buffer. 26a chunk B drops the 2 MiB scratch allocation that every typed
    `Output` makes today.
  - A system on a thread reads and writes through mirror rings 4× the real
    ring. Today a 2 MiB frame gets a 32 MiB ring, so `star_sim`'s image
    port costs 32 MiB + 128 MiB. With 26a's proposed 8 MiB `ring_bytes` cap
    it costs 8 MiB + 32 MiB.
  - The derived `Decomponentize` builds a 2 MiB stack temporary until 26a's
    `assign_component_view` lands. `star_sim` never decomponentizes. It
    builds its `StarImage` once with `new_box_zeroed`, which needs
    zerocopy's `alloc` feature, enabled by 26a.
- **Records.**
  - `BodyState` carries `q_b_eci`, `omega_b`, `pos_eci` and `vel_eci`
    (`contracts/src/frames.rs:76-87`).
  - `World` carries `sun_eci`, `mag_eci` and `illuminated`
    (`frames.rs:161-171`).
  - The plant's epoch is `epoch_at(t_sim)`, where `t_sim` sums its own `dt`s
    (`plant.rs:337,468-471,495`, `environment.rs:90-97`). A separate system
    cannot rebuild the epoch, so the plant computes the ephemeris.
- **Ephemeris.**
  - `nox_frames::earth::sun_vec` is a low-precision analytic sun
    (`libs/nox-frames/src/earth.rs:115`).
  - nox-frames already depends on `rsofa 0.5.0`
    (`libs/nox-frames/Cargo.toml:17`). It wraps all of SOFA 2023-10-11,
    including `iauMoon98`, `iauEpv00`, `iauAb` and `iauPmsafe`
    (`rsofa-0.5.0/extern/src/{moon98,epv00,ab,pmsafe}.c`).
- **Dependencies.**
  - The panel pins `wgpu = "30"`, `bytemuck = "1"` and `pollster = "0.4"`
    (`libs/metor-panel/Cargo.toml:48-52`), and its CLAUDE.md says this must
    track Bevy.
  - The lock has wgpu 30.0.1, naga 30.0.1, naga_oil 0.23.0, encase 0.12.0,
    libm 0.2.16 and rand_pcg 0.10.2.
  - A pack is a `cdylib + rlib` crate with an `export` feature and a
    `metor-build` pyproject (`systems/adcs/Cargo.toml`,
    `systems/adcs/pyproject.toml`). The members are at `Cargo.toml:42-45`.
  - `nix/shell.nix:30` has `vulkan-loader`. The repo has no CI config.

From 26b, the interfaces this plan uses:

- `metor_star::geom`: `V3 = [f64; 3]`, `M3`, and `Quat` stored scalar-last,
  with `to_m3` returning `A(q)`.
- `metor_star::camera`:
  - `Intrinsics { f_m, pitch_m, cx, cy, k1, k2, p1, p2, width, height }`
  - `Camera { intr, q_b_st }`
  - `distort`, `undistort`, `st_to_pixel(b) -> Option<(f64, f64)>`,
    `pixel_to_st`, `body_to_st`, `half_diagonal_rad` and `aberrate(b, β)`.
  - `distort` is "the one definition the simulator also uses".
- **Pixel convention: centres are at integer + ½.** Pixel `i` covers
  `[i, i + 1)`. 26b's `image::centroid` and `camera` use this, so the
  renderer integrates over exactly that box.
- 26b's onboard DB (`metor_star::catalog`) is Hipparcos V ≤ 6.5:
  - a 20 B `Star` with an id
  - a cube-face cell grid (`cells.rs`: `cell_of`, `cell_center`)
  - a pair table and k-vector
  - magic `METORSTR`, built by the Rust `star-db` tool
- 26b's `testkit::splat` is a test-only renderer that "the sim plan's
  `render` replaces when it lands".

Verified with a prototype (`scratchpad/star-sim-trade/proto`):

- wgpu 30.0.1 runs headless with `compatible_surface: None` on Metal
  (Apple M4 Max) and on lavapipe (Mesa 25.0.6 llvmpipe, aarch64, in `orb`
  nixos, `VK_ICD_FILENAMES=<mesa>/share/vulkan/icd.d/lvp_icd.aarch64.json`).
- A frame of 50k stars took 1.2 ms on Metal and 5 ms on lavapipe once warm.
  It used a 7×7 erf PSF with `atomicAdd` on u32, a clear, a 4 MiB copy,
  `map_async` and `poll(Wait)`. The first frame took 7 ms and 22 ms.
- The image is bit-exact from run to run on one device. It differs between
  Metal and lavapipe because `exp` and `pow` differ by a few ULP. So
  CPU-vs-GPU and GPU-vs-GPU checks use tolerances, and golden hashes are only
  valid for one backend.
- `unpack2x16float` fails naga validation without
  `Capabilities::SHADER_FLOAT16_IN_FLOAT32`.
  - wgpu maps that capability to `DownlevelFlags::SHADER_F16_IN_F32`, which
    is "not supported by Vulkan on Mesa when `Features::SHADER_F16` is
    absent" (`wgpu-types-30.0.1/src/limits.rs:1223-1227`).
  - `extractBits` on an `i32` is core and validates with
    `Capabilities::empty()`.
- wgpu-profiler 0.28 builds against wgpu 30.0.1.
- wgpu's default `max_storage_buffer_binding_size` is 128 MiB
  (`wgpu-types-30.0.1/src/limits.rs:441`). G ≤ 12 (49.4 MB) fits.

Catalog numbers (Gaia archive TAP and TAPVizieR, run 2026-10-22):

| Query | Count | Size at 16 B |
|---|---|---|
| Gaia DR3 G ≤ 3 | 150 | |
| Gaia DR3 G ≤ 6 | 6,764 | |
| Gaia DR3 G ≤ 8 | 62,723 | 1.0 MB |
| Gaia DR3 G ≤ 10 | 482,106 | 7.7 MB |
| Gaia DR3 G ≤ 12 | 3,087,828 | 49.4 MB |
| Gaia DR3 G ≤ 14 | 16,844,164 | 269.5 MB (out of scope) |
| G ≤ 12 with no `pmra` (2-parameter solution) | 25,504 | |
| G ≤ 12 with no `bp_rp` | 3,838 | |
| G ≤ 12 per level-0 HEALPix cell | 102,119 – 452,202 | |
| Hipparcos I/239 rows / V ≤ 3 / V ≤ 8 | 118,218 / 177 / 41,411 | |
| `gaiadr3.hipparcos2_best_neighbour` rows | 99,525 | |
| `gaiadr3.tycho2tdsc_merge_best_neighbour` rows | 2,518,330 | |
| Tycho-2 I/259 rows | 2,539,913 | |

About 18.7k Hipparcos stars have no DR3 best neighbour. These are the
bright-end fill.

Licences:

- Gaia: "Gaia data are distributed under the CC BY-NC 3.0 IGO license"
  (cosmos.esa.int/web/gaia-users/license).
- The ESA Space Science Archives terms put all archive data under CC BY-NC
  3.0 IGO. Commercial use needs a prior request to data.licences@esa.int. The
  credit lines are "ESA, Gaia DPAC" for Gaia and "ESA" for Hipparcos
  (cosmos.esa.int/web/esdc/terms-and-conditions).
- VizieR lists I/239 (Hipparcos and Tycho) and I/259 (Tycho-2) as
  `CC-BY-NC-3.0 IGO`.
- This also answers 26b's open question 5: its committed `hip_v65.stdb`
  needs the same `NOTICE`.

## Changes to the design

1. **Magnitudes are packed as `i16` millimag, not f16.** Each star stays
   16 B: f32×3 direction, then one u32 holding G and BP−RP as two `i16`
   millimag values. f16 needs `SHADER_F16` or `SHADER_F16_IN_F32`, and
   lavapipe may have neither. Millimag is also finer than f16 at G ≈ 12.
2. **No compacted visible list and no indirect dispatch.**
   - The CPU knows each selected cell's star count from the cell table, so
     it uploads `(offset, prefix)` per cell and dispatches `ceil(total/64)`
     directly.
   - One fused kernel projects, does photometry, and splats. Integer atomics
     make the order irrelevant.
3. **Stars are sorted by (cell, G, source_id).** A magnitude limit is then
   a binary search per cell, and the CPU truth pass reads only the head of
   each cell.
4. **Truth is computed on the CPU in f64.** This covers the brightest 32
   centroids, `n_rendered` and `n_detectable`. The GPU returns only the image.
5. **The committed test catalog is G ≤ 8 from the same pipeline**, not V ≤ 8:
   62,723 stars plus the fill.
6. **Photometry sanity numbers.**
   - With V = 0 ↔ 8.8e5 ph/s/cm², a 2 cm *diameter* aperture at mag 7, with
     QE·τ = 0.5 and 100 ms, gives 219 e⁻, not 500.
   - 500 e⁻ needs a 3 cm diameter.
7. **The ephemeris comes from the plant through `World`.**

## The two catalogs are deliberately separate

`starcat` (this plan) and 26b's onboard DB share `geom` and `camera`. They
share no reader, format, or cell index. They differ in purpose and shape:

| | `starcat` (render) | onboard DB (26b) |
|---|---|---|
| Source | Gaia DR3 G ≤ 12 + Hipparcos fill | Hipparcos V ≤ 6.5 |
| Stars | 3.1 M (G ≤ 12), 63k committed | about 5k |
| Contents | direction, G, BP−RP; HIP in a CPU-only side table | direction, V, HIP id, pairs, k-vector |
| Index | HEALPix nside 16, centre and radius stored per cell | cube-face grid computed in `metor-star` |
| Built by | Python tool (Gaia TAP) | Rust `star-db` (CDS file) |
| Runs in | native sim pack, GPU-resident | wasm `adcs` pack, `include_bytes!` |

- **Different sources by design.** The design doc requires the renderer to
  use a different, deeper catalog than the tracker, so the tests do not use
  the tracker's own data as truth.
- **A shared reader would buy little.** The formats differ in everything
  but the header pattern. Merging them would put pair tables in the sim file
  or Gaia colours in the flight DB.
- **Why HEALPix rather than 26b's cube-face grid.**
  - The Rust side never computes a cell. It reads each cell's stored centre
    and radius and scans 3,072 cells, so the scheme only matters in the
    Python tool.
  - HEALPix cells are equal-area. Cube-face cells vary about 5× in area,
    which skews per-cell counts at 3 M stars.
  - Gaia's `source_id >> 51` is the nside-16 index, a free cross-check.
- **One shared test.** In C, bright stars in both committed files agree in
  position after both pipelines' proper-motion propagation. This catches an
  epoch or sign mismatch between the two tools.

## Crate trade study

| Need | Pick | Considered | Reason |
|---|---|---|---|
| HEALPix in the Python tool | `astropy-healpix` 2.0.1 (BSD-3) | `healpy` 1.20.1, `cdshealpix` 0.9.0 (Python) | healpy is GPL-2.0-only. astropy is already pulled in by astroquery. cdshealpix is a fine alternative but one more dependency. |
| HEALPix in Rust | none | `cdshealpix` 0.10.1, `healpix` 0.3.3, hand-rolled | The runtime reads stored cell centres and radii. cdshealpix has non-optional png, mapproj, rayon, chrono and flate2, and fails no_std (26b's check). Nothing is hand-rolled. |
| HEALPix cross-check | Gaia `source_id >> 51` | | A free check in the tool's tests. |
| erf (CPU and WGSL) | hand-rolled A&S 7.1.26, the same 6 lines in both; `libm::erf` as test truth | `libm`, `statrs`, `special` 0.14 | WGSL has no erf. The CPU oracle must evaluate the same polynomial. libm is already metor-star's math crate. |
| PSF cost | separable weights: 2K erf calls per star, then K² atomics | per-pixel erf | About 7× fewer erf calls at K = 13. |
| Random numbers | stateless PCG hash (Jarzynski & Olano 2020), the same ~8 lines in Rust and WGSL | `rand_pcg`, `rand_distr` | A stateful stream cannot be reproduced per pixel in parallel. `rand_distr`'s Poisson algorithm differs from the GPU's. Integer hashing is bit-exact on both. |
| Frame seed | `Rng::seeded(seed ^ frame_id, "star_sim")` | | Replays any single frame without its history. |
| Distribution tests | `statrs` 0.19 (dev-dependency) | `rand_distr` | Poisson and normal CDFs and χ² quantiles. |
| GPU struct layout | `bytemuck` (already in the panel) plus a naga reflection test | `encase` 0.13 | Four `vec4`-aligned structs. A test that compares naga's `span` and offsets with Rust's catches the same mistakes. |
| Shader composition | none: one `star_sim.wgsl`, `include_str!` | `naga_oil` 0.23, `wesl` 0.6 | About 400 lines. naga_oil is tied to Bevy's naga, and wesl is young. Revisit past about 800 lines. |
| GPU timing | `wgpu-profiler` 0.28 behind a `profile` feature | manual timestamp queries | Builds on wgpu 30 (verified). A no-op without `TIMESTAMP_QUERY`. |
| WGSL tests | `naga` 30.0.1 `wgsl-in` (dev-dependency), plus GPU kernel tests against the CPU oracle | third-party WGSL test harnesses | None is mature. naga is the validator wgpu runs. |
| No-GPU CI | lavapipe from `nixpkgs#mesa` in `orb` nixos | SwiftShader | Verified, and wgpu's own CI uses it. |
| Async glue | `pollster` 0.4 | | Only at init. Frames use `device.poll(Wait)`. |
| Moon and Earth velocity | `rsofa` 0.5 `iauMoon98` and `iauEpv00`, wrapped in nox-frames | `anise` 0.10, `satkit` 0.24, `sgp4`, a hand-rolled Meeus model | Already a dependency. Moon98 is the Meeus series. anise needs SPK kernels and pins `nalgebra =0.35`. satkit downloads JPL files. sgp4 does not apply. |
| Proper motion (Python) | `pyerfa.pmsafe` | astropy `apply_space_motion` | Vectorised, guards zero parallax, and the same algorithm as SOFA. astropy calls erfa too, so the tests use the linear formula as the oracle. |
| Queries | `astroquery.gaia`, `astroquery.vizier` | `pyvo` | Handles Gaia's async job API and retries. |
| Big image buffer | zerocopy `new_box_zeroed` (26a) | `Box::new` | Avoids a 2 MiB stack value. |

The only hand-rolled pieces are the erf polynomial and the PCG hash. Both
must be identical on the CPU and GPU, and both are under 10 lines.

## A. The `starcat` tool and the committed G ≤ 8 catalog

Files: `libs/metor-star/tools/starcat/{pyproject.toml, starcat/__init__.py,
fetch.py, astrometry.py, photometry.py, sky.py, fmt.py, build.py, ensure.py,
catalogs.toml}`, `tests/`, `libs/metor-star/data/{starcat-g8.bin, NOTICE}`.

Package:

- uv project `metor-starcat`. The base install is stdlib only, so `ensure`
  stays light.
- The `build` extra adds `astroquery`, `astropy`, `astropy-healpix`, `pyerfa`
  and `numpy`.

`fetch.py`:

- Gaia partitions are the 12 level-0 cells,
  `GAIA_HEALPIX_INDEX(0, source_id) = k`. The largest is 452k rows.
  - Columns: `source_id, ra, dec, pmra, pmdec, parallax, radial_velocity,
    ref_epoch, phot_g_mean_mag, bp_rp`
  - Filter: `phot_g_mean_mag <= :g`
- Hipparcos I/239 `hip_main` (`HIP, RAICRS, DEICRS, pmRA, pmDE, Plx, Vmag,
  B-V, V-I`), and the `(source_id, original_ext_source_id)` pairs from
  `gaiadr3.hipparcos2_best_neighbour`. They give each matched Gaia star its
  HIP number.
- Raw results are cached under `$XDG_CACHE_HOME/metor/starcat/raw/`.

`astrometry.py`:

- `propagate(..., epoch_from, epoch_to)` wraps `erfa.pmsafe`. A missing pm,
  parallax or rv is taken as 0.
- `unit(ra, dec)`.

`photometry.py`:

- `hip_to_gaia(V, V_I) -> (G, BP_RP)`, from the Gaia DR3 documentation's
  Johnson–Cousins relations (Riello et al. 2021). The coefficients are
  copied with a citation.
- `G_TO_V`: the DR3 polynomial for `V − G` in `BP − RP`. It is the default
  fixed colour term, so the default band is Johnson V. It is exported for the
  Rust default in D.
- A missing `bp_rp` defaults to 0.8. The build log counts the defaults.

`sky.py`:

- `cell(unit)`: the nside-16 nested index, via `astropy_healpix`.
- `cell_bounds() -> (centre, radius)`. The radius is the largest angle from
  the centre to the boundary, sampled with `boundaries_lonlat(step=8)`, plus
  1e-6 rad.

`build.py`:

- Merge Gaia with the unmatched Hipparcos rows.
- Give every row a HIP number: Gaia rows from the crossmatch, fill rows
  their own, and `u32::MAX` for none.
- Propagate to 2024.0. This is the same epoch as 26b's DB.
- Sort by `(cell, G, source_id)`. Hipparcos rows use `source_id = 0` and
  break ties by HIP.
- Quantise G and BP−RP to millimag.
- Write the file, print counts, defaults and the sha256.

`fmt.py` writes and reads `starcat.bin`, all little-endian:

- **Header**, 80 B: `magic = b"STARCAT\0"`, `version: u32 = 1`, `nside`,
  `n_cells`, `n_stars`, `epoch_jyear: f64`, `mag_limit: f32`, `sources: u32`
  (bit 0 Gaia DR3, bit 1 Hipparcos), `cells_off: u64`, `stars_off: u64`,
  `hip_off: u64`, `n_hip: u32`, and padding.
- **Cells**: `{ centre: [f32; 3], radius: f32, offset: u32, count: u32,
  _pad: [u32; 2] }`, 32 B each, 96 KiB in all.
- **Stars**: `{ dir: [f32; 3], mags: u32 }`, where `mags` is
  `G_mmag as i16 as u16 | (bp_rp_mmag as i16 as u16) << 16`.
- **HIP**: a sparse table of `(row: u32, hip: u32)` pairs sorted by row, `n_hip` entries (the header is 80 B and carries `n_hip`). It is CPU-only; the GPU uploads the stars section alone. About 0.94 MB at G ≤ 12 and 0.39 MB at G ≤ 8.

`ensure.py` and `catalogs.toml`:

- `catalogs.toml` lists `name`, `mag_limit`, `url`, `sha256` and `bytes`.
- `ensure(name) -> Path` downloads into the cache, checks the sha256, and
  returns the path. It never builds. A mismatch names both hashes.
- `g8` resolves to the committed file.
- `g12` resolves to the cached download. Its `url` is a placeholder until
  hosting is decided.

Committed:

- `starcat-g8.bin`, about 1.1 MB.
- A `NOTICE` with the CC BY-NC 3.0 IGO terms and the attribution for Gaia
  ("ESA/Gaia/DPAC"), Hipparcos and Tycho-2 ("ESA"), the source catalog
  references, and a statement that the data is not under the repo's
  Apache-2.0.

Tests (`pytest`; offline unless marked `network`):

- Nominal:
  - A 200-star synthetic catalog round-trips through `fmt`. It is committed
    as `libs/metor-star/tests/data/tiny.bin` for C.
  - 8-year propagation matches the linear formula to 1 mas for
    |μ| < 100 mas/yr.
  - `cell()` agrees with `source_id >> 51` away from cell edges.
  - Every star lies within its cell's radius.
- Edge:
  - Pole and RA 0/360 stars.
  - Null pm, parallax or bp_rp.
  - An empty cell.
  - G = −1.5 quantises without overflow.
- Failure:
  - `fmt.read` rejects a bad magic, a bad version, a truncated file,
    non-monotonic offsets and a wrong count sum.
  - `ensure` rejects a sha256 mismatch.
- Determinism: building `tiny` twice gives identical bytes.
- `network` (opt-in):
  - G ≤ 6 gives 6,764 Gaia stars plus the fill.
  - `hip_to_gaia` matches Gaia G with an RMS under 0.05 mag for 6 < G < 9.

## B. `StarImage` and `StarTruth` contracts

A small PR that lands right after 26a chunk A. 26b's `star_tracker` (26b
chunk J) and 26d's overlay consume these records. `star_sim` produces them.

Files: `examples/adcs-fsw3/contracts/src/star.rs` (created here, or extended
if 26b chunk C created it first), `contracts/src/lib.rs`, and
`contracts/Cargo.toml` (zerocopy `alloc`).

Both records use nox types through the contracts' `Quat` and `V3` aliases,
as `frames.rs` does.

`StarImage` (frame `star_image`):

- `#[frame(timestamp)] timestamp` (mid-exposure)
- `frame_id: u64`, `exposure_us: u32`, `gain_milli: u32`
- `pixels: FrameImage<Mono16<12>, 1024, 1024>`
- `MAX_LEN` 2,097,176
- Not `Clone`/`Copy`, since `FrameImage` is neither. It is built with
  `new_box_zeroed`.

`StarTruth` (frame `star_truth`):

- `#[frame(timestamp)] timestamp` (mid-exposure), `frame_id: u64`
- `q_cam_eci: Quat` (sensor←ECI) and `omega_cam: V3` (rad/s)
- `n_rendered: u32`, `n_detectable: u32`
- `blinded: u32`: bit 0 sun, bit 1 earth, bit 2 moon
- The true-centroid list, one shaped component per field, matching 26b's
  `StarCentroids` so the 26d overlay binds to both the same way:
  - `count: u64`
  - `u: FrameArray<f32, 32>` and `v: FrameArray<f32, 32>`, in pixels, in
    the integer + ½ convention
  - `mag: FrameArray<f32, 32>`, the band magnitude
  - `cat_id: FrameArray<u32, 32>`: the star's Hipparcos number (HIP), or
    `u32::MAX` for a star with none. 26b's `StarCentroids.cat_id` and
    onboard DB use HIP too, so scoring compares the two fields directly:
    the tracker identified the right star when they are equal.
- Entries past `count` are zero, with `cat_id = u32::MAX`.

`star.rs` also has `pub const MAX_TRUTH: usize = 32`.

Tests (`contracts` unit tests):

- Nominal:
  - `StarImage`'s vtable has one pixels field of shape `[1024, 1024]`
    with `image.bits = 12`.
  - `StarTruth`'s `u`, `v`, `mag` and `cat_id` are one `[32]` component
    each, at the same paths as `StarCentroids`' fields.
  - Both decode in place from `new_box_zeroed` bytes.
- Edge: `count = 0` and `count = 32`.
- Failure: a truncated `StarImage` byte slice fails to decode.
- The test thread uses a 512 KiB stack in debug to catch a 2 MiB stack
  temporary.

## C. `metor_star::starcat`: the catalog reader

Files: `libs/metor-star/src/starcat.rs`, `src/starcat/tests.rs`. This goes in
26b's crate skeleton (26b chunk B): `no_std` + `alloc`, `libm`, `zerocopy`,
with LE field types as 26b's DB uses.

Types and API:

- zerocopy `Header`, `Cell` and `Star`. `Star::g()` and `Star::bp_rp()`
  return f32 mag, and `Star::unit() -> V3`.
- `StarCat<'a>::parse(&'a [u8]) -> Result<Self, CatError>` borrows
  zero-copy. It checks the magic, version, `n_cells == 12·nside²`, section
  bounds and alignment, monotonic offsets and the count sum.
- `cells_in_cone(dir: V3, half_angle) -> impl Iterator<Item = u32>`
- `stars(cell)`
- `brighter_than(cell, g)`, by binary search.
- `hip(row) -> Option<u32>`, from the HIP section. Rows are global star
  indices, which `stars(cell)` slices give with `cells[cell].offset`.
- The pack loads the file into a `Vec<u32>` at init, which guarantees
  alignment.

Tests:

- Nominal:
  - Parse `tiny.bin` and `starcat-g8.bin`.
  - `cells_in_cone` loses no star against brute force at 500 random cones
    of 5–20°.
  - `brighter_than` matches a linear scan.
  - `hip` gives Vega (HIP 91262) and Sirius (HIP 32349) on their rows in
    `starcat-g8.bin`.
- Cross-catalog: once 26b's `hip_v65.stdb` is committed, every DB star
  within 2″ of a `starcat-g8` star matches it with a median separation under
  0.1″. 26b's tracker DB is read through 26b's `Db::parse`.
- Edge: cones of 0 rad, of π, and on a pole; an empty cell.
- Failure: each `CatError` from a mutated `tiny.bin`, including a HIP
  table shorter than `n_hip` or not sorted by row.
- Kani: for any offset table that passes `parse`, `stars(cell)` stays in
  bounds.

## D. `metor_star::render`: the CPU reference renderer

Files: `libs/metor-star/src/render/{mod.rs, hash.rs, psf.rs, photometry.rs,
scene.rs, splat.rs, noise.rs, truth.rs}` plus tests. Feature `sim`,
`no_std` + `alloc`.

It uses 26b's `geom` and `camera` as listed in the facts:

- The ECI→sensor rotation is `body_to_st ∘ A(q_b_eci)`.
- Projection is `st_to_pixel`, which includes `distort`.
- Pixel `i` covers `[i, i + 1)`.

The GPU port needs the raw `Intrinsics` fields and the `distort` formula,
which 26b exposes. When D lands, 26b's `testkit::splat` can call
`render` or stay as is. That is 26b's choice.

Modules:

- `hash.rs`: `pcg`, `pcg4` and `uniform(key) -> f32` in (0, 1). The key is
  `(pixel, draw, seed_lo, seed_hi)`.
- `psf.rs`:
  - `erf_as` (A&S 7.1.26).
  - `weights(x0, sigma, out)` gives pixel `i` the weight
    `½[erf((i + 1 − x0)/√2σ) − erf((i − x0)/√2σ)]`.
  - `K = ceil(4σ) ≤ 6`.
- `photometry.rs`:
  - `Photometry { flux0_ph_s_m2, aperture_m2, throughput, colour: [f32; 3] }`
  - `band_mag = G + c0 + c1·c + c2·c²`. The default `colour` is `G_TO_V`
    from A, a fixed term until K.
  - `electrons(m, t_exp)` and `background_e(mag_arcsec2, pixel_sr, t_exp)`.
- `scene.rs`:
  - `Frame { r_st_eci: M3, substeps: [M3; 32], n_sub, beta: V3, t_exp,
    seed, frame_id }`, built from the camera, `q_b_eci`, `omega_b`, the
    observer velocity and `max_sub`.
  - `Sensor { psf_sigma_px, full_well_e, read_noise_e, dark_e_s, dark_t0_c,
    temp_c, gain_e_dn, bias_dn, zodi_mag_arcsec2 }`.
  - The DN ceiling is `2^BITS − 1`, with `BITS` taken from the record's
    `Mono16<12>` type (26a), not from a param.
  - `HotPixels`: a full-frame f32 dark multiplier map built from the seed.
- `splat.rs`: rotate, aberrate via 26b's `aberrate`, project, cull, and add
  `round(e/n · wx · wy · 256)` per substep into a u32 accumulator
  (1 count = 1/256 e⁻). f32 from the sensor-frame vector on, to match WGSL.
- `noise.rs`: `expose(acc, sensor, hot, seed, out: &mut [u16])`:
  - Poisson by Knuth for λ < 30 (capped at 128 draws), else rounded normal.
  - Then read noise, the full-well clamp, gain, bias and the DN clamp.
- `truth.rs`: f64 at mid-exposure.
  - It returns the brightest 32 stars as `u`, `v`, `mag` and `cat_id`, the
    fields of B's `StarTruth`. `cat_id` is `StarCat::hip(row)` or
    `u32::MAX`.
  - It also returns `n_rendered` and `n_detectable`.
  - `metor-star` stays free of contract types. The pack copies these fields
    into the record.
- `Renderer::new(cat, camera, phot, sensor, seed)` allocates every buffer
  once. `render(&mut self, frame)` does not allocate.

Tests:

- Nominal:
  - `erf_as` is within 1.5e-7 of `libm::erf`.
  - PSF weights sum to 1 within 1e-6.
  - With no noise, σ ∈ {0.7, 1.0, 1.5} and 400 sub-pixel phases, a CoG
    centroid (26b's `image::centroid` once it exists, a local CoG before)
    matches the projected truth to < 0.02 px. σ = 0.5 shows the S-curve
    above 0.02 px.
  - Total e⁻ matches `electrons(m)` within 1e-4.
  - Photometry: V = 0 ↔ 8.8e5 ph/s/cm², D = 2 cm, mag 7, 0.5, 100 ms gives
    219 ± 1 e⁻.
  - Flat-field noise at λ ∈ {0.5, 5, 29, 31, 1000} passes a χ² test against
    `statrs` Poisson.
  - Truth centroids `pixel_to_st` back to the catalog vector within 1e-9 rad.
- Edge:
  - A star half off the edge.
  - Saturation at full well and at `2^BITS − 1`.
  - An empty sky.
  - `t_exp = 0`.
  - λ either side of 30.
- Failure: `Renderer::new` rejects σ ≤ 0, `gain ≤ 0`, and `width`/`height`
  other than the record's 1024×1024.
- Determinism:
  - The same seed and frame id give identical bytes.
  - Another frame id changes more than 99% of noisy pixels.
- Speed (an ignored bench test): G ≤ 12 at a 20° FOV takes ≤ 40 ms per
  frame.

## E. The `startracker_sim` pack on the CPU backend

Files:

- `examples/adcs-fsw3/systems/startracker-sim/{Cargo.toml, pyproject.toml,
  src/{lib.rs, params.rs, system.rs, cpu.rs}}`
- `Cargo.toml`: the member, and `wgpu`, `bytemuck`, `pollster` moved to
  `[workspace.dependencies]`
- `libs/metor-panel/Cargo.toml`
- `examples/adcs-fsw3/pyproject.toml`
- `sim.py`

Pack:

- `star_sim` is registered with `register_async`.
- Inputs: `body: BodyState`, `world: World`. Outputs: `StarImage` and
  `StarTruth` from B.
- `StarImage` is built once with `new_box_zeroed`. Pixels are written
  through `pixels_mut()`, or as bytes through zerocopy `as_mut_bytes()` for
  the GPU copy.

Params:

- `rate_hz` (10), `exposure_s` (0.1), `seed`, `catalog`, `mag_limit` (12.0)
- `camera` (26b `Intrinsics` and `q_b_st`)
- `photometry`, `sensor`, `hot_pixel_fraction`, `aberration` (false),
  `max_substeps` (16)
- `backend`: `"cpu" | "gpu" | "auto"`

Run loop:

1. `body.next().await`; an exposure starts at the first `timestamp ≥
   next_exposure`, `t`.
2. Keep taking body states until one is at or past `t + t_exp/2`. The
   frame is rendered from that state (its attitude turned back to `t` at
   its rate is the `Motion`), so the image's attitude is the true attitude
   at its stamp. Rendering from the state at `t` and extrapolating at the
   rate at `t` put each frame ατ²/8 off its stamp, which under the plant's
   rate jitter was a 1.5e-4 rad common-mode error per frame that nav
   could not model (nav NEES 5.4 against 0.9 after).
3. Select cells for the half-diagonal plus the blur sweep.
4. Render, then write both records stamped `t + t_exp/2`.
5. Advance `frame_id` and `next_exposure`.

`sim.py`: `fsw.add("star_sim", StarSim(..., catalog=starcat.ensure("g8"),
backend="cpu"), thread="startracker")`. It moves to `g12` once that file is
hosted.

Tests (`examples/adcs-fsw3/tests/star_sim.rs`):

- Nominal: 2 s give 20 frames at mid-exposure timestamps, frame ids 0–19,
  and a non-empty truth.
- Edge:
  - `rate_hz` above the cycle rate.
  - A boresight on a pole.
  - `mag_limit` below every star.
- Failure: each of these fails at construct with a named error:
  - a missing or corrupt catalog
  - an unknown backend
  - camera dimensions other than 1024×1024
- Determinism: same seed gives identical bytes; another seed differs.
- The test thread uses a 512 KiB stack in debug, as 26a's frame test does,
  to catch any 2 MiB stack temporary.

## F. The wgpu backend

Files: `startracker-sim/src/gpu/{mod.rs, device.rs, layout.rs,
star_sim.wgsl, pipelines.rs, readback.rs}`, `src/backend.rs`, and the
`justfile` (`test-gpu-lavapipe`).

Device:

- `Gpu::new` requests a headless adapter and a device with default limits.
- Every buffer is created at init: catalog, cell list, uniforms, hot-pixel
  map, accumulator, packed image, and one `MAP_READ` staging buffer.
- A catalog over `max_storage_buffer_binding_size` is `CatalogTooLarge`.
- Pipelines are built inside an error scope. `on_uncaptured_error` logs and
  marks the backend failed, and `auto` falls back to the CPU. Nothing
  panics.

`star_sim.wgsl` has three entry points:

- `clear`
- `splat`: binary search over the cell prefix sums, then the fused
  project, photometry and splat.
- `expose`: writes `lo | hi << 16`, which on little-endian is the byte
  layout of `[Mono16<12>]`.

Readback:

- Each frame's readback copies straight into the boxed `StarImage`'s pixel
  bytes.
- It blocks for 1–5 ms (accepted).
- Our code allocates nothing per frame. wgpu's internal per-encoder
  allocations are accepted for a ground-side sim pack.

`layout.rs`: `bytemuck::Pod` mirrors of the WGSL structs. The `profile`
feature adds wgpu-profiler scopes.

Tests:

- Unit (no GPU):
  - naga validates `star_sim.wgsl` with `Capabilities::empty()`.
  - Every struct's naga `span` and member offsets equal `size_of` and
    `offset_of!`.
- GPU (`tests/gpu.rs`). These skip without an adapter unless
  `METOR_REQUIRE_GPU=1`.
  - Nominal, with no noise, on `starcat-g8.bin` at 50 attitudes:
    - centroids agree with the CPU to < 0.01 px
    - total e⁻ within 1e-4
    - per-pixel |diff| ≤ 1e-3 of the peak
  - Noisy: at most 1e-3 of pixels differ, by at most 1 DN.
  - Determinism: two frames with the same seed are bit-identical.
  - Edge: an empty cell, the last cell, and saturation.
  - Failure:
    - `CatalogTooLarge`.
    - `backend = "gpu"` with no adapter fails.
    - `auto` with no adapter selects the CPU.
- `just test-gpu-lavapipe` runs the GPU tests in `orb` nixos with
  `nixpkgs#mesa`, `VK_ICD_FILENAMES` and `METOR_REQUIRE_GPU=1`.

## G. Moon and Earth velocity in `World` (P7)

Files: `libs/nox-frames/src/earth.rs`, `contracts/src/frames.rs`,
`contracts/src/environment.rs`, `systems/adcs/src/plant.rs`.

nox-frames:

- `moon_pos_gcrs(epoch) -> V3` in metres, from `iauMoon98`.
- `earth_vel_helio(epoch) -> V3` in m/s, from `iauEpv00`'s heliocentric
  velocity.
- Both use a TT/TDB two-part JD from hifitime.
- The barycentric velocity differs by about 13 m/s, which is under 0.01″ of
  aberration.

`World`:

- Gains `moon_eci: V3` and `earth_vel: V3`, beside `sun_eci`.
- The plant fills them. The telemetry layout changes.

Tests:

- Nominal: three epochs match hard-coded JPL Horizons values, to 30″ in
  direction and 1% in range for the Moon, and 1 m/s for Earth.
- Edge: `epv00` near its 1900–2100 limits.
- Failure: none. These are total functions.

## H. Aberration and motion blur (P7)

Files: `render/{scene,splat,truth}.rs`, `star_sim.wgsl`, `system.rs`.

- `β = (World::earth_vel + vel_eci)/c` when `aberration = true`. Truth uses
  the same β.
- Substeps are driven by `omega_b`.

Tests:

- Nominal:
  - With β ⊥ b at 30 km/s, a star moves 20.6″ within 0.05″.
  - The streak length is `|ω|·t_exp·f/p` within 2%.
  - The streak's CoG matches the mid-exposure projection to < 0.02 px.
  - GPU and CPU agree to F's tolerances.
- Edge:
  - Pure roll.
  - `n_sub` capped.
  - β = 0 is identical to `aberration = false`.
- Failure: `max_substeps` above 32 is rejected.

## I. Distortion and detector effects (P7)

Files: `render/{splat,noise}.rs`, `star_sim.wgsl`, `params.rs`.

- Brown–Conrady through 26b's `distort` on the CPU, ported to WGSL.
- Hot pixels with a log-uniform multiplier.
- Dark current with temperature.
- Optional blooming: charge above full well bleeds along its column. It is a
  `bleed` kernel with one thread per column, run before `expose` clamps,
  and the CPU runs the same loop.

Tests:

- Nominal:
  - Distorted truth centroids `pixel_to_st` to the catalog vector within
    1e-9 rad.
  - GPU and CPU agree to 0.01 px at the corners.
  - The hot-pixel fraction is within 4σ.
  - Dark current doubles over 6.5 °C.
- Edge: k₁ = 0 is identical to the undistorted image on the CPU.
- Failure: a folding distortion (26b's `undistort` returns `None` inside
  the image) is rejected at init.

## J. Bright bodies (P7)

Files: `render/{noise,truth}.rs`, `star_sim.wgsl`.

- `blinded` bits, each set when that body affects the FOV:
  - sun: within `sun_exclusion_deg` of the boresight (`World::sun_eci`)
  - Earth: its disk intersects the FOV (`BodyState::pos_eci`)
  - Moon: its disk intersects the FOV (`World::moon_eci`)
- Disks are drawn by a per-pixel ray–sphere test with a constant radiance
  param.
- Sun stray light is a linear gradient scaled by `1/angle²`.

Tests:

- Nominal:
  - A disk's pixel count matches its solid angle within 2%.
  - Flags change across the exclusion angle.
- Edge: a limb tangent to the edge; the sun exactly at the exclusion angle.
- Failure: a negative exclusion angle is rejected.

## K. Sensor band fit (P7)

Files: `libs/metor-star/tools/starcat/band.py`, tests, and a `band` entry in
`params.rs`.

- `band.py --qe qe.csv` fits `colour = [c0, c1, c2]` for
  `m_band − G = c0 + c1·(BP−RP) + c2·(BP−RP)²`.
  - It folds the sensor's QE curve with a stellar spectral library and the
    Gaia passbands, using Gaia DR3 XP spectra or the Pickles library (see
    the open questions).
  - It writes the coefficients and the fit RMS.
- `star_sim`'s `photometry.colour` takes the fitted values. The default
  stays the fixed `G_TO_V` term.

Tests:

- Nominal: fitting to the Johnson V passband as the "QE curve" recovers
  `G_TO_V` within 0.02 mag over 0 < BP−RP < 3.
- Edge: a flat QE curve.
- Failure: a QE file with non-monotonic wavelengths or a negative QE is
  rejected.

## Dependencies on parallel plans

- **26a.**
  - B needs `FrameImage<Mono16<12>, 1024, 1024>` with `pixels_mut()`,
    zerocopy `IntoBytes`/`FromZeros` with `alloc`, and `FrameArray<T, N>`
    (26a chunk A).
  - E needs the `ring_bytes` cap and the frame scratch removal (26a chunk B).
- **26a, for a db.** Running `sim.py` with a db also needs 26a's DB, link
  and stream fixes. Until then, E's tests read the ports directly.
- **26b.**
  - 26b consumes B: `star_tracker` reads `StarImage`, and its scoring reads
    `StarTruth`.
  - `StarTruth`'s centroid fields mirror 26b's `StarCentroids` (`u`, `v`,
    `cat_id`, `count`). 26b has `flux` where `StarTruth` has `mag`. A rename
    on either side breaks the 26d overlay preset.
  - C needs the crate skeleton and `geom` (26b chunk B).
  - D needs `camera` (26b chunk D).
  - C's cross-catalog test waits on `hip_v65.stdb` (26b chunk F).
  - 26b's `star_tracker` integration test (26b chunk J) can switch from
    `testkit` frames to `star_sim(backend="cpu")` once E lands.
  - 26b's committed DB needs the same `NOTICE`.

## Open questions

1. **Hosting the G ≤ 12 file** (about 49.5 MB): a GitHub release asset, S3,
   or something else? `catalogs.toml` holds a placeholder URL until then.
   `pmsafe` can differ by an ULP across platforms, so a local rebuild may
   not match the pinned sha256. `ensure` therefore only downloads.
2. **Spectral library for K.** Gaia DR3 XP sampled spectra (also NC) or the
   Pickles 1998 library?
3. **Ring budget.** `star_sim` on its own thread costs 8 MiB + 32 MiB
   per image port with 26a's cap. Is the 4× thread mirror (26a open
   question 3) worth reducing for this port?
