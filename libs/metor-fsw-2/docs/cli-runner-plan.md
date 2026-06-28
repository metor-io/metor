# WP9 implementation plan — the `metor-fsw` CLI runner

Design: `docs/cli-runner.md` (approved). Decisions locked: binary + reusable `cli` module;
`package` → relocatable bundle dir; `lib=` becomes a **stem** the framework decorates per
platform. The CLI is a thin front-end over `parse`/`build_artifacts`/`resolve`/`run_for`.

Implemented in four sequential waves (each builds on the last; no parallel agents — the waves
share files). Build + test after each wave.

## Wave 1 — framework primitives (in `src/wiring/`)

1. **`cdylib_file_name(stem) -> String`** — new `pub fn` in the wiring module (generalizes the
   example's `cdylib_name`): `lib{stem}.dylib` / `lib{stem}.so` / `{stem}.dll`. Re-export from
   the crate root (ungated-ish; lives in the `kdl`-gated `wiring` module, re-exported under
   `#[cfg(feature="kdl")]` like the other wiring items).
2. **Stem change.** `Artifact::cdylib` keeps its meaning (the *produced file name*); only the
   front-ends change to take a stem:
   - `parse_artifact`: read `lib=` as a **stem**, set `cdylib = cdylib_file_name(stem)`.
   - `WiringBuilder::artifact`: its `cdylib` param becomes a **stem**, decorated the same way
     (keeps the two front-ends symmetric). Update the builder doctest and
     `tests/wiring_resolve.rs` (`"libadcs_plant.dylib"` → `"adcs_plant"`).
   - `build_driver`/bundle/`resolve` are unchanged — they consume `Artifact::cdylib` (the full
     name) exactly as before.
3. **`src/wiring/bundle.rs`** (new) — the relocatable bundle:
   - `write_bundle(wiring: &Wiring, mission_kdl: &str, opts: &PackageOptions, dir: &Path) -> Result<(), BundleError>`:
     create `dir`; copy each `artifact.path` (must be `Some`) into `dir` as `artifact.cdylib`;
     write `mission.kdl` (the source KDL text, verbatim); write `meta.kdl`
     (`abi_version <FSW_ABI_VERSION>`, `profile "<debug|release>"`, `built_at_unix <secs>` — epoch
     seconds, no chrono dep). Schemas deferred (design §4.3).
   - `load_bundle(dir: &Path) -> Result<Wiring, BundleError>`: read+parse `meta.kdl`, guard
     `abi_version == FSW_ABI_VERSION` (else `BundleError::AbiMismatch`); `parse(mission.kdl)`;
     fill each `artifact.path = Some(dir.join(&artifact.cdylib))`, erroring `MissingSo` if the
     file is absent. **Never** invokes cargo.
   - `BundleError` (`thiserror`): `Io`, `Parse(LoadError)`, `AbiMismatch{found,expected}`,
     `NotBuilt{artifact}` (a `path` was `None` at package), `MissingSo{path}`, `BadMeta`.
   - Export `write_bundle`/`load_bundle`/`BundleError` from `wiring` + crate root (kdl-gated).
   - `mod bundle;` in `wiring/mod.rs`.

Verify: `cargo build -p metor-fsw-2`, `cargo test -p metor-fsw-2` (wiring tests green after the
stem-string updates).

## Wave 2 — the `cli` module + binary

4. **`clap` dep** — `Cargo.toml`: `clap = { version = "4", features = ["derive"], optional = true }`;
   add `"dep:clap"` to the `kdl` feature. New `[[bin]] name = "metor-fsw"`,
   `path = "src/bin/metor-fsw.rs"`, `required-features = ["kdl"]`.
5. **`src/cli/mod.rs`** (`#[cfg(feature = "kdl")] pub mod cli;` in `lib.rs`):
   - clap derive: `Cli{command}`, `Command::{Build(BuildArgs),Package(PackageArgs),Run(RunArgs)}`.
     `BuildArgs{kdl, release, cargo_arg:Vec<String>}`; `PackageArgs{kdl, out, release, no_build,
     cargo_arg}`; `RunArgs{target, build, no_build, release, cargo_arg, wall, sim_dt:Option<f64>
     (group w/ wall), cycle_rate:Option<f64>, telemetry:Option<SocketAddr>, no_telemetry (group
     w/ telemetry), telemetry_mode, cycles:Option<usize>}`.
   - `pub fn main()`: `run(env::args_os())`, render `miette` report to stderr, `exit(1)` on err.
   - `pub fn run<I,T>(args) -> miette::Result<()>`: `Cli::try_parse_from`, dispatch.
   - `cmd_build`: `parse` file → `build_artifacts` → print `crate → path` per artifact.
   - `cmd_package`: `parse`; unless `--no-build` run `build_artifacts`; `write_bundle` → print
     summary.
   - `cmd_run`: `load_wiring(&args)` (bundle dir → `load_bundle`; else source `.kdl` requiring
     `--build` → `parse`+`build_artifacts`); `apply_overrides`; `resolve(&wiring,&Registry::new())`
     (dl-only); `stellarator::run(|| async move { coord.run_for(cycles).await })`.
   - Helpers: `is_bundle(path)` (a dir, or ends `.bundle`); `apply_overrides(&mut Wiring,&RunArgs)`
     (clock/cycle-rate/telemetry per design §7); `build_opts(release,cargo_arg)->BuildOptions`.
   - Errors: wrap `BuildError`/`BundleError`/`io::Error` into `miette::Report`; `LoadError` is
     already a `Diagnostic`.

Verify: `cargo build -p metor-fsw-2` (bin builds); `cargo build -p metor-fsw-2 --no-default-features`
(no clap, no bin — `required-features` gates it out); `cargo run -p metor-fsw -- --help`.

## Wave 3 — example refactor (`examples/adcs-fsw2/`)

6. **`mission.kdl`** (new, on disk) — the static translation of `kdl_doc()`: sim base
   (`coordinator cycle_rate=120.0 sim_dt=0.008333333333333333`), three `artifact` nodes with
   **stem** `lib=` (`lib="adcs_plant"` …), three dl `system` nodes, three `connect`s (the
   `ctrl -> plant` one `delayed=#true`). No telemetry (live knobs are flags).
7. **`src/main.rs`** → `fn main() { metor_fsw_2::cli::main() }`.
8. **`src/lib.rs`** → keep only `build_sim_coordinator()` for the parity test, now
   `parse(include_str!("../mission.kdl"))` + `build_artifacts` + `resolve`. Delete `kdl_doc`,
   `build_live_coordinator`, `cdylib_name`, `PANEL_ADDR`, `DT`. Update the module docs.
9. **`Cargo.toml`** — drop `stellarator` from `[dependencies]` (the `main` shim is sync; the CLI
   enters the runtime internally). Keep `anyhow` (for `build_sim_coordinator`'s return type) and
   `metor-fsw-2`. `[dev-dependencies]` unchanged (the test still links the system rlibs +
   `stellarator` + `adcs-contracts`).

Verify: `cargo run -p adcs-fsw2 -- run mission.kdl --build --cycles 4000` converges/exits clean;
`cargo test -p adcs-fsw2` (closed_loop parity still green).

## Wave 4 — tests & final verification

10. **CLI unit tests** (in `cli/mod.rs` `#[cfg(test)]`): `run`/`build`/`package` arg parsing;
    `apply_overrides` maps each flag onto the `Wiring`; `--wall`+`--sim-dt` and
    `--telemetry`+`--no-telemetry` mutual-exclusion rejected by clap; source-KDL `run` without
    `--build`/bundle errors with guidance.
11. **Bundle round-trip test** (`examples/adcs-fsw2/tests/` — it has the cdylibs): `parse` →
    `build_artifacts` → `write_bundle` to a tempdir → `load_bundle` → `resolve` →
    `run_for(small)`; assert it loads and steps (a thin smoke; convergence stays in
    `closed_loop.rs`). Use the scratchpad/`std::env::temp_dir` for the bundle dir.
12. **Final gate**: `cargo build -p metor-fsw-2 --no-default-features`; `cargo build -p metor-fsw-2`;
    `cargo test -p metor-fsw-2`; `cargo test -p adcs-fsw2`; `cargo run -p metor-fsw -- --help`;
    a manual `package` + `run <bundle>` of the adcs mission. Then commit (task boundary).

## Notes / invariants
- The bundle is platform-specific; built + run on the same arch, so `dir.join(cdylib)` and the
  stem re-decoration in `load_bundle`'s `parse` agree.
- The generic binary resolves against an **empty** `Registry` (dl-only) — fine for the all-dl
  adcs mission; a static mission keeps its own host.
- No `anyhow` enters the framework; the CLI surfaces `miette::Result` (reusing the kdl-gated dep).
- `built_at_unix` uses `SystemTime` (epoch secs as integer) to avoid a date-formatting dep.
