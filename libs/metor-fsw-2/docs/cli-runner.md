# The `metor-fsw` CLI runner (`cli-runner`)

> **Status: v1 IMPLEMENTED.** Landed deviations from the original draft: the artifact
> `lib=` stem change (§4.6/§8.2) was **approved and implemented**; `--no-build` was
> **dropped** from `build`/`package`/`run` as redundant (the incremental build driver is
> also the only locator — §2.2/§8.3). `meta.kdl` ships `abi_version`/`profile`/`built_at_unix`
> (schemas deferred — §8.6).
>
> **Pack update (2026-07-11, `docs/packs.md`):** artifacts are now **packs** — an
> `artifact` node has no `type=` (a cdylib exports many systems; the `system` node's
> `type=` selects the entry), `Artifact` has no `system_type` field, and the loader is
> `DlPack::open`. The CLI verbs, the bundle format, and the flow below are otherwise
> unchanged; §6's mission.kdl snapshot predates the example's merge into one
> `adcs-systems` pack.
>
> **Python/packaging update (2026-07-18, `docs/design-python-config.md`,
> `docs/design-packaging.md`):** missions are Python (`.py`), KDL was removed, and the
> `--build` flag is gone — a source `run`/`build`/`package` builds automatically, first
> refreshing any path-source dev packs (`pack dev` relayout, cargo-incremental) and then
> provisioning crate artifacts; `run --no-build` opts out and locates instead. The
> KDL invocations and the `--build` contract below are historical.

`metor-fsw-2` ships its own runner: a `metor-fsw` binary and a reusable `cli` library
module that turn a wiring KDL into a running mission. The three mission operations —
**build**, **package**, and **run** — are separate verbs, with a build→run shortcut
(`run --build`) for the common dev loop. A mission host (like the `adcs-fsw2` example)
stops hand-writing its own `main`/runner and becomes a one-line shim plus an on-disk
`mission.kdl`.

This is a **thin front-end** over the existing wiring surface — `parse` → (`build_artifacts`)
→ override → `resolve` → `Coordinator::run_for`. No new mission machinery: the CLI only
parses args, mutates a `Wiring`, and drives the coordinator. The genuinely new surface is
the clap command tree, the bundle reader/writer, and the stellarator entry point.

The implementation lands in:

- `src/cli/mod.rs` — the clap command tree, `cli::main()` / `cli::run(args)`, and the three
  command handlers. Gated on the `kdl` feature (it uses `parse`/`build_artifacts`/`resolve`).
- `src/bin/metor-fsw.rs` — the binary: `fn main() { metor_fsw_2::cli::main() }`.
- `src/wiring/` — small additions: a `cdylib_file_name(stem)` helper (moved from the example),
  a bundle writer/reader, and an artifact-locator that fills `Artifact::path` from a bundle
  directory (no cargo).

> **Scope.** The generic `metor-fsw` binary loads **dlopen'd (`cdylib`) systems only** — it
> resolves against an **empty** `Registry`, because a single prebuilt binary cannot link an
> arbitrary mission's statically-linked systems. A static-system mission still builds its own
> host (the `Coordinator::builder` path); the CLI is for the dl-open deployment model
> (dl-open.md). The `adcs-fsw2` example is all-dl, so it rides the generic binary fully.

---

## 1. Goals & non-goals

**Goals.**

1. **Three separable operations.** `build` compiles the `cdylib`s a wiring references;
   `package` produces a relocatable bundle directory; `run` drives the coordinator. Each
   works standalone. The build→run shortcut is `run --build`.
2. **A relocatable deploy artifact.** `package` emits a directory that carries the compiled
   `.so`s plus a manifest, runnable on a flight target with **no source tree and no cargo**.
3. **Live/sim and telemetry knobs as flags.** The clock (Wall/Simulated) and the telemetry
   downlink the example currently hard-codes move to clap flags that **override** what the
   KDL declares.
4. **Reuse over abstraction.** The CLI is `parse`/`build_artifacts`/`resolve`/`run_for` with
   arg parsing in front; the example's `build_live_coordinator` mutation pattern becomes the
   override step verbatim.

**Non-goals (v1).**

- **No static-system support in the generic binary** (empty registry; dl-only — see scope).
- ~~**No tar/zip.**~~ *(Superseded by python-config Phase 3 — §4.6: an uncompressed-tar
  `.metor` single-file form now ships alongside the directory. Still no compression.)*
- **No cross-compilation orchestration.** `--release`/`--target` pass through to the build
  driver's `extra_args`; the CLI does not manage toolchains.
- **No async systems** (inherited from dl-open.md — dl systems are cyclic-only).

---

## 2. CLI surface

```
metor-fsw <COMMAND>

  build    <SRC>                 compile the cdylibs the wiring references
  package  <SRC> -o <OUT>        freeze an IR bundle (dir or .metor); --check-ir to verify
  run      <TARGET>              load a wiring (source or bundle) and run it
```

`<SRC>` is a mission source — a `.py` (evaluated by subprocess CPython) or a `.kdl` (parsed).

### 2.1 `build`

```
metor-fsw build <KDL> [--release] [--cargo-arg <ARG>]...
```

Parse `<KDL>` → `Wiring`, run `build_artifacts`, and print each artifact's crate and located
`.so` path. No run. `--release` sets `BuildOptions::release`; repeated `--cargo-arg` append to
`BuildOptions::extra_args` (e.g. `--cargo-arg --target --cargo-arg aarch64-unknown-linux-gnu`).

```
$ metor-fsw build mission.kdl
  adcs-plant  →  target/debug/libadcs_plant.dylib
  adcs-nav    →  target/debug/libadcs_nav.dylib
  adcs-ctrl   →  target/debug/libadcs_ctrl.dylib
```

### 2.2 `package`

```
metor-fsw package <SRC> -o <OUT> [--release] [--cargo-arg <ARG>]...
metor-fsw package --check-ir <BUNDLE>
```

Freeze the IR bundle of §4 at `<OUT>`: a directory (conventionally `*.bundle`) or a single-file
`*.metor` archive, dispatched by the `.metor` extension. `<SRC>` is a `.py` or `.kdl` — both
front-ends freeze through the same path, so a Python mission packages exactly as a KDL one.
`package` always runs `build_artifacts` — but that driver is *also* the only artifact locator
(it scrapes cargo's `compiler-artifact` lines, which carry the `.so` path even when the crate
is `"fresh":true`), so on an up-to-date tree it relocates without recompiling.

`--check-ir <BUNDLE>` is the determinism gate (§4.5): it re-evaluates the bundle's provenance
source, diffs the produced IR against the frozen `wiring.json`, and exits non-zero on drift.

```
$ metor-fsw package mission.py -o dist/adcs.bundle
  packaged 2 artifacts, 6 systems → dist/adcs.bundle
$ metor-fsw package --check-ir dist/adcs.bundle
  --check-ir: dist/adcs.bundle reproduces its frozen IR
```

### 2.3 `run`

```
metor-fsw run <TARGET>
    [--build] [--release] [--cargo-arg <ARG>]...
    [--wall | --sim-dt <SECS>] [--cycle-rate <HZ>]
    [--serve <ADDR>]
    [--cycles <N>]
```

`<TARGET>` is either a **source `.py`/`.kdl` file** or a **bundle** (§4); `run` detects which
(a directory, a `*.bundle`, or a `*.metor` is a bundle). Loading differs:

- **Bundle** → `load_bundle`: check `meta.json` (abi/ir/target), deserialize `wiring.json`,
  fill each `Artifact::path` from the copied `.so`, verify manifest hashes, `resolve`. **Never
  invokes cargo, Python, or the KDL parser.** `--build` is rejected here (nothing to build).
- **Source** → the artifacts carry no located path (the source names crates, not absolute
  paths), so `run` must locate the `.so`s, and the only locator is the cargo build driver
  (incremental). Therefore **a source run requires `--build`** — it evaluates the source
  (KDL parse or Python subprocess), runs `build_artifacts` (compile-if-stale + locate), then
  resolves. Without `--build` (and not a bundle), `run` errors with guidance: `--build` to
  compile, or `package` then run the bundle. `--build` is thus the build→run shortcut (§2.4).

**Override flags** (precedence in §7): all mutate the `Wiring` after load, before `resolve`.

| Flag | Effect on `Wiring` |
| --- | --- |
| `--wall` | `coordinator.clock = ClockSpec::Wall` |
| `--sim-dt <SECS>` | `coordinator.clock = ClockSpec::Simulated { dt_secs }` |
| `--cycle-rate <HZ>` | `coordinator.cycle_rate = HZ` |
| `--serve <ADDR>` | overrides the mission's `TcpServer` state's `addr`, or — when the mission declares none — declares one (`"link"`) plus an all-taps `Downlink` (`"telemetry"`) |
| `--cycles <N>` | `run_for(N)`; default `usize::MAX` (run until interrupted) |

`--wall`/`--sim-dt` are mutually exclusive (clap group).

**Run output.** `run` is not silent: before the runtime starts it prints a banner (the
systems, the active clock, the telemetry target, the duration), and while running it emits a
cycle-progress heartbeat (read from a shared `Coordinator::progress()` counter) every couple
of seconds, then a completion/`hard-stopped` summary. So a long mission visibly advances and
the active config is never a guess.

**Telemetry operational notes:**
- The FSW **serves** its link: the mission's `TcpServer` state listens on its `addr`, and
  ground tools (metor-panel, `nc`) connect to it whenever they like — each connection gets
  the announce replay first, then the live stream, and its writes are the command ingest.
  Nothing needs to be listening before the mission starts.
- Under the **simulated** clock the loop free-runs without sleeping, so the io reactor —
  and with it the link's sockets — is starved until the run ends; for live viewing use
  `--wall`.

### 2.4 The build→run shortcut

There is no separate "build-and-run" verb. The shortcut is the `--build` flag on `run`:
`run --build` = `build_artifacts` then `resolve` then `run_for`. This is preferred over a
fourth verb because **`run` from a source KDL already needs the build driver to locate the
`.so`s** — `--build` is the same locate-or-compile step, named to make the cargo invocation
explicit (cargo is incremental, so a fresh tree's `--build` is a near-noop). Keeping it a flag
keeps the verb count at the three locked operations and leaves the separated path
(`build` → `package` → `run <bundle>`) intact and cargo-free at run time.

### 2.5 Concrete invocations (the adcs example)

```
# dev loop: build the cdylibs and run live against metor-panel on a wall clock
cargo run -p adcs-fsw2 -- run mission.kdl --build --wall --telemetry 127.0.0.1:2240

# headless sim (the base KDL config: simulated clock, no telemetry), bounded
cargo run -p adcs-fsw2 -- run mission.kdl --build --cycles 4000

# package a relocatable bundle, then run it on the target with no cargo/source
metor-fsw package mission.kdl -o dist/adcs.bundle
metor-fsw run dist/adcs.bundle --wall --telemetry 10.0.0.2:2240
```

---

## 3. The `cli` module shape

Two entry points, thin. The first executable statement of `run` is
`proc::worker_entry()`: a mission with `process=#true` systems re-executes the
host binary as its workers, and the guard routes such a child into the worker
loop instead of the CLI (`docs/process-systems.md` §5). For everyone else it is
one environment-variable read.

```rust
// src/cli/mod.rs   (#[cfg(feature = "kdl")])

/// Binary entry: parse argv, dispatch, render any error, set the exit code.
pub fn main() {
    if let Err(report) = run(std::env::args_os()) {
        eprintln!("{report:?}");          // miette Diagnostic rendering (spans for LoadError)
        std::process::exit(1);
    }
}

/// Testable entry: parse `args`, dispatch. Returns `Result` so tests can drive it
/// without spawning a process or touching argv.
pub fn run<I, T>(args: I) -> miette::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).map_err(...)?;   // clap derive
    match cli.command {
        Command::Build(a)   => cmd_build(a),
        Command::Package(a) => cmd_package(a),
        Command::Run(a)     => cmd_run(a),
    }
}
```

clap is the **derive** API: a `Cli { command: Command }`, `Command` enum of
`Build(BuildArgs)`/`Package(PackageArgs)`/`Run(RunArgs)`, each `*Args` a `#[derive(Parser)]`
struct mapping 1:1 to §2's flags. The handlers are plain functions; the entry points stay
trivial.

**Errors.** The CLI surfaces `miette::Result` — `LoadError` is already a `miette::Diagnostic`
(span-carrying), and `miette` is already a `kdl`-feature dep, so no `anyhow` is added.
`BuildError`/`io::Error` are wrapped into the report at the boundary.

**Entering stellarator.** Only `cmd_run` needs the async runtime, and it enters it at the
leaf, synchronously, via `stellarator::run` (the existing sync→async bridge):

```rust
fn cmd_run(args: RunArgs) -> miette::Result<()> {
    let mut wiring = load_wiring(&args)?;     // bundle or source-KDL (+build) → located Wiring
    apply_overrides(&mut wiring, &args);      // clock / telemetry / cycle-rate (the example's mutation pattern)
    let mut coord = resolve(&wiring, &Registry::new())?;   // empty registry: dl-only
    let cycles = args.cycles.unwrap_or(usize::MAX);
    stellarator::run(|| async move { coord.run_for(cycles).await });
    Ok(())
}
```

`build`/`package` are fully synchronous (cargo + file I/O); they never start a runtime.

---

## 4. Bundle format

> **Phase 3 update (python-config, `docs/python-config-phase3-plan.md`):** the bundle is now
> the **frozen, versioned IR** — `wiring.json` + a JSON `meta.json` sidecar — not verbatim KDL.
> Both front-ends (KDL and Python) freeze through the same path, so `package mission.py` works
> and the run path needs **no Python and no KDL parser**. A single-file `.metor` (uncompressed
> tar) form joins the directory form, and `package --check-ir` is the determinism gate. The
> pre-Phase-3 sections below are rewritten to match; the retired `mission.kdl` + `meta.kdl`
> layout is refused with a clear "repackage" error (`BundleError::OldLayout`).

A bundle carries everything `run` needs with no source tree and no cargo. It is
**platform-specific** — it contains compiled `.so`s — so it is built for, and run on, one
target, and `run` checks that target triple up front (§4.3). Two forms: a directory, or a
single-file `.metor` archive (§4.6).

### 4.1 Layout

```
dist/adcs.bundle/
├── meta.json                     # abi/ir versions, target triple, profile, timestamp,
│                                 #   wiring.json sha256, metor_config version
├── wiring.json                   # the frozen, versioned Wiring IR (src anchors, scopes,
│                                 #   per-artifact manifest hashes; artifact paths stripped)
├── libadcs_systems.so            # the built cdylibs, one per artifact, copied in
├── libadcs_systems.so.manifest   #   + the build driver's manifest sidecar, when present
├── libadcs_sequences.so
├── libadcs_sequences.so.manifest
└── mission.py | mission.kdl      # optional verbatim provenance copy — NEVER consumed on load
```

### 4.2 `wiring.json` — the frozen IR

The manifest is the serialized [`Wiring`](../src/ir.rs) IR (`serde_json`), the same
self-describing JSON the `WiringManifest` telemetry carries and the IR contract pins — so the
frozen file, the emitted manifest, and a CI re-evaluation are all byte-comparable. It is
written **path-stripped** ([`Wiring::path_stripped`]): artifact `path`s point into a build
tree, so they are re-derived on load, and stripping them keeps a bundle relocatable and
byte-reproducible. Source anchors and the scope table are kept — they are the panel graph
tile's deep-link data.

`package` does not need a KDL serializer: it evaluates the source (KDL parse or Python
subprocess) to a `Wiring` exactly as `build`/`run` do, then freezes that model. A Python
mission thus packages identically to a KDL one — the whole point of the IR bundle.

### 4.3 `meta.json` — the sidecar

A plain-serde [`BundleMeta`](../src/wiring/bundle.rs):

```json
{
  "abi_version": 6,
  "ir_version": 1,
  "target": "aarch64-apple-darwin",
  "profile": "debug",
  "built_at_unix": 1783899222,
  "ir_sha256": "sha256:fdc9…",
  "metor_config_version": "0.2.0"
}
```

`load_bundle` refuses a bundle before opening any `.so` on three guards: `abi_version` ≠ host
`FSW_ABI_VERSION`, `ir_version` ≠ host `IR_VERSION`, or `target` ≠ the host triple (a clean
`BundleError::TargetMismatch`, replacing today's opaque dlopen failure on an arch mismatch;
skipped only when the host triple cannot be determined). `ir_sha256` hashes the exact
`wiring.json` bytes (excluding the `built_at_unix` timestamp, which lives in `meta.json`) —
the determinism backstop `--check-ir` and CI diff. `metor_config_version` is Python-mission
provenance; `profile`/`built_at_unix` are informational.

### 4.4 How `run` loads a bundle vs a source

```
load_run_wiring(args):
  if target is a bundle (dir, *.bundle, or *.metor):
      wiring = load_bundle(target)              # NO cargo, NO Python, NO KDL parse
      # load_bundle: read meta.json → check abi/ir/target → deserialize wiring.json →
      #   fill each artifact.path from the copied .so → verify each recorded
      #   manifest_hash against its copied sidecar (tamper check before dlopen)
      return wiring
  else (source .py/.kdl):
      require args.build (else error: pass --build, or package then run the bundle)
      wiring = load_source(target)              # KDL parse or Python subprocess eval
      build_artifacts(&mut wiring, &opts)       # compile-if-stale + locate
      return wiring
```

`resolve` already requires each `Artifact::path` to be `Some` (`ArtifactNotBuilt` otherwise)
and then `DlPack::open`s it. `load_bundle` fills `path` from the bundle instead of from cargo.

### 4.5 The determinism gate — `package --check-ir <bundle>`

`package --check-ir <bundle>` re-evaluates the bundle's provenance source and diffs the
produced IR against the frozen `wiring.json`, exiting non-zero on drift — the operational
determinism enforcement (`design-python-config.md` §2). Both sides are normalized first:
artifact `path`s stripped and `src` **file names** cleared (the provenance copy sits at a
different path than the original source), keeping every anchor's line/column, so a genuine
emission change is caught while a physical-path difference is not. A KDL provenance copy is
self-contained; a Python one re-evaluates only where its `packs/` are importable (a caveat for
Phase 4 to smooth).

### 4.6 The single-file `.metor` archive

`package -o mission.metor` (extension-dispatched) writes the directory layout into one
**uncompressed tar** — tar over zip because it is streamable, order-stable for reproducible
bytes, and a plain archive keeps the load path `mmap`-friendly after unpack (`.so`s do not
compress usefully anyway). Entry order is stable — `meta.json`, `wiring.json`, then artifacts
sorted by id (each `.so` then its `.manifest`), then the provenance copy — and tar timestamps
are zeroed, so identical inputs (with a pinned `built_at_unix`) produce byte-identical
archives. A dependency-free `ustar` writer/reader lives in `src/wiring/bundle.rs`; standard
`tar` reads the output. `load_bundle` on a `.metor` unpacks it to a temp directory (dlopen
needs real files) that outlives the call, then loads identically.

### 4.7 Relation to the `Wiring` model & the framework change

`Artifact { id, crate_name, cdylib, path, manifest_hash, src }` is the unit. `load_bundle`
sets `path` from `dir + cdylib`. `package` freezes through `write_bundle(&Wiring,
&PackageOptions, out)`, extension-dispatched between the directory and `.metor` forms, both
drawing from one `bundle_members` ordering so the two forms carry identical content. The
platform decoration `cdylib_file_name(stem)` (`lib{stem}.dylib` / `.so` / `{stem}.dll`) is
shared by `build_artifacts`, `package`, and the bundle path-fill. These live in the `wiring`
module (model-adjacent), keeping the `cli` handlers thin orchestrators.

### 4.6 Platform naming: `lib=` becomes a stem

Today the KDL `artifact` node's `lib=` is the **full produced file name**
(`lib="libadcs_plant.dylib"`), and `build_artifacts` matches cargo output ending in that exact
name. The example computes it per-platform with its `cdylib_name` helper inside a `format!`.
A **static** `mission.kdl` cannot hold a `format!`, so a hardcoded `.dylib` would fail to
locate on Linux and vice-versa.

**Recommended change:** the `artifact` `lib=` carries the **library stem** (`lib="adcs_plant"`),
and `parse`/`build_artifacts` decorate it to the platform file name via `cdylib_file_name`.
This makes a static `mission.kdl` portable across dev (macOS) and target (Linux) with no
text change, and a bundle (already platform-specific) stores the concrete `.so` alongside.
This is a contained change: `Artifact::cdylib` continues to hold the produced file name, now
*computed* from the stem at parse time rather than written literally. (Fallback in §8.)

---

## 5. Feature gating & dependencies

`clap` is the one new dependency. The `cli` module and the `metor-fsw` binary use
`parse`/`build_artifacts`/`resolve`, which are `kdl`-gated, so both ride the `kdl` feature.

```toml
# Cargo.toml
[dependencies]
clap = { version = "4", features = ["derive"], optional = true }

[features]
default = ["kdl"]
kdl = ["dep:kdl", "dep:miette", "dep:postcard-dyn", "dep:serde_json", "dep:clap"]

[[bin]]
name = "metor-fsw"
path = "src/bin/metor-fsw.rs"
required-features = ["kdl"]
```

- `clap` is **optional** and pulled in by `kdl` (the cli has no meaning without the wiring
  surface). `--no-default-features` (the `abi`/`dl`-only build) drops clap and the bin.
- The bin's `required-features = ["kdl"]` keeps `cargo build --no-default-features` from trying
  to compile a binary whose module is `#[cfg]`-ed out.
- `cli` is declared `#[cfg(feature = "kdl")] pub mod cli;` in `lib.rs`.
- Error rendering reuses the already-present `miette`; **no `anyhow`** is added to the
  framework.

**The binary stays self-contained.** It depends only on the framework's own
parse/build/resolve; the mission's **system crates are built out-of-process by cargo** (the
build driver shells `cargo build -p <crate>`) and **loaded via `dlopen`**, never linked. So
the `metor-fsw` binary does **not** drag `adcs-*` or any mission crate into the framework's
dependency graph — exactly the dl-open decoupling.

---

## 6. The example refactor (`examples/adcs-fsw2/`)

After WP9 the example is a thin shim plus a real KDL file:

**`mission.kdl`** (new, on disk — the static translation of today's `kdl_doc()`, using the
base/sim config; the live-vs-sim and telemetry knobs move to CLI flags):

```kdl
coordinator cycle_rate=120.0 sim_dt=0.008333333333333333

artifact "plant" crate="adcs-plant" lib="adcs_plant"
artifact "nav"   crate="adcs-nav"   lib="adcs_nav"
artifact "ctrl"  crate="adcs-ctrl"  lib="adcs_ctrl"

system "plant" type="Plant" artifact="plant" init_angle=0.5 init_rate=0.15 meas_sigma=0.002 seed=42
system "nav"   type="Nav"   artifact="nav"   meas_sigma=0.02
system "ctrl"  type="Ctrl"  artifact="ctrl"  q_weight=5.0 r_weight=8.0

connect "plant" -> "nav"  frame="sensors"
connect "nav"   -> "ctrl" frame="attitude_estimate"
connect "ctrl"  -> "plant" frame="torque_cmd" delayed=#true
```

The dynamic bits of `kdl_doc()` are gone: the per-platform `cdylib_name` interpolation is
replaced by **stems** (`lib="adcs_plant"`, decorated by the framework — §4.6); the live
clock + telemetry that `build_live_coordinator` injected become `--wall --telemetry <addr>`
flags. (`system … artifact="plant"` still references the *artifact id*; the property on a
`system` node was later hard-renamed from `lib=` to `artifact=` so it can't be confused with
the `artifact` node's own `lib=` stem — `examples/adcs-fsw2/mission.kdl:50-52`.)

**`src/main.rs`** (replaces the bespoke runner + heartbeat):

```rust
fn main() {
    metor_fsw_2::cli::main()
}
```

So `cargo run -p adcs-fsw2 -- run mission.kdl --build …` works, and the standalone
`metor-fsw run mission.kdl --build …` works identically.

**`src/lib.rs`** (reduced — kept only for the parity test). The test
`tests/closed_loop.rs` calls `adcs_fsw2::build_sim_coordinator()` to get the dlopen sim
coordinator and compare it against the statically-linked build. Keep exactly that function,
now reading the on-disk KDL:

```rust
pub fn build_sim_coordinator() -> anyhow::Result<Coordinator> {
    let mut wiring = parse(include_str!("../mission.kdl"))?;   // the static file, was kdl_doc()
    build_artifacts(&mut wiring, &BuildOptions::default())?;
    Ok(resolve(&wiring, &Registry::new())?)
}
```

Deleted: `kdl_doc()` (now `mission.kdl`), `build_live_coordinator()` (now `run --wall
--telemetry`), `cdylib_name()` (moved to the framework), `PANEL_ADDR` (now a `--telemetry`
arg). The test's static path and its registry-tap measurement are untouched — it keeps using
`parse`/`build_artifacts`/`resolve` directly, so WP9 does not break it.

**`Cargo.toml`**: drop `stellarator` from `[dependencies]` (the thin `main` is sync; the CLI
enters the runtime internally); keep `anyhow` (for `build_sim_coordinator`) and the
`[dev-dependencies]` (the test still links the system rlibs + `stellarator`).

---

## 7. Where clock & telemetry config lives

**KDL declares the defaults; CLI flags override; framework constants are the floor.** Exactly
the `build_live_coordinator` pattern, generalized: load → mutate `Wiring` → `resolve`.

| Knob | KDL (default) | CLI override | Precedence |
| --- | --- | --- | --- |
| clock | `coordinator sim_dt=…` / absent⇒Wall | `--sim-dt` / `--wall` | flag > KDL > `Wall` |
| cycle rate | `coordinator cycle_rate=…` | `--cycle-rate` | flag > KDL |
| telemetry | `system "…" type="TcpDownlink"` / absent⇒none | `--telemetry <addr>` / `--no-telemetry` | flag > KDL |
| telemetry mode | `instances`/`frames` subset children | `--telemetry-mode all` | flag > KDL (subset KDL-only) |

The example relies on this directly: `mission.kdl` declares the **sim** base (free-running
`Simulated` clock, no telemetry) — the headless/test config — and a live run is purely
flags: `run mission.kdl --build --wall --telemetry 127.0.0.1:2240`. The base file is what the
parity test consumes unmodified.

---

## 8. Deviations / open questions

Each fork below has a recommended default already taken in the design above; flagged here for
the gate.

1. **Source-KDL `run` requires `--build`.** ✅ **Implemented.** The only artifact locator is the
   cargo build driver (paths are not persisted between a `build` and a later `run` process), so
   running a bare source KDL must invoke cargo — `run <source.kdl>` **requires `--build`**, and
   the cargo-free path is `run <bundle>`. Keeps "touches cargo" explicit.

2. **Artifact `lib=` becomes a stem (§4.6).** ✅ **Approved & implemented.** `lib=` carries the
   library stem; `parse`/`WiringBuilder::artifact` decorate it via `cdylib_file_name` so one
   `mission.kdl` is portable macOS↔Linux. `Artifact::cdylib` still holds the produced file name,
   now computed.

3. **`package` builds by default; `--no-build` dropped.** ✅ **Resolved during impl.** The build
   driver is incremental *and* the only locator (it reads cargo's `compiler-artifact` lines even
   when `"fresh":true`), so `package`/`run --build` always run it and a `--no-build` flag would
   have no cargo-free way to find the `.so`s. `--no-build` was therefore **removed** rather than
   shipped as a no-op.

4. **Generic binary is dl-only (empty `Registry`).** ✅ **Implemented.** `cmd_run` resolves
   against `Registry::new()`; a prebuilt `metor-fsw` loads dlopen'd systems only. A static
   mission keeps its own `main`. A static-system registration hook is deferred.

5. **Bundle manifest is KDL, copied not serialized.** ✅ **Implemented.** v1 packages from a KDL
   source, so the manifest is the source KDL (relocatable as-is). A `Wiring`→KDL serializer (for
   a builder-origin mission with no source text) is deferred until a non-KDL front-end needs
   packaging.

6. **`meta.kdl` schemas are optional in v1.** ✅ **Implemented.** `run` reads each `.so`'s live
   `params_schema`, so `meta.kdl` ships `abi_version` (the load-guard), `profile`, and
   `built_at_unix` (epoch seconds — no date-formatting dep); per-system schemas are added when
   offline config validation lands.
