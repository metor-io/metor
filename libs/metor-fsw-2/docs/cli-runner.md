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
- **No tar/zip.** The bundle is a plain directory. A compress step is a future convenience
  (§4.5).
- **No cross-compilation orchestration.** `--release`/`--target` pass through to the build
  driver's `extra_args`; the CLI does not manage toolchains.
- **No async systems** (inherited from dl-open.md — dl systems are cyclic-only).

---

## 2. CLI surface

```
metor-fsw <COMMAND>

  build    <KDL>                 compile the cdylibs the wiring references
  package  <KDL> -o <DIR>        produce a relocatable bundle directory
  run      <TARGET>              load a wiring (source KDL or bundle) and run it
```

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
metor-fsw package <KDL> -o <DIR> [--release] [--cargo-arg <ARG>]...
```

Produce the relocatable bundle of §4 at `<DIR>` (created if absent; conventionally
`*.bundle`). `package` always runs `build_artifacts` — but that driver is *also* the only
artifact locator (it scrapes cargo's `compiler-artifact` lines, which carry the `.so` path
even when the crate is `"fresh":true`), so on an up-to-date tree it relocates without
recompiling. A separate `--no-build` would therefore be redundant — there is no cargo-free
locator — so it was dropped during impl (§8.3); `package` is always self-sufficient.

```
$ metor-fsw package mission.kdl -o dist/adcs.bundle
  built 3 cdylibs, copied into dist/adcs.bundle/
  wrote dist/adcs.bundle/mission.kdl  (3 artifacts, 3 systems, 3 edges)
  wrote dist/adcs.bundle/meta.kdl     (abi=3, profile=debug)
```

### 2.3 `run`

```
metor-fsw run <TARGET>
    [--build] [--release] [--cargo-arg <ARG>]...
    [--wall | --sim-dt <SECS>] [--cycle-rate <HZ>]
    [--telemetry <ADDR> [--telemetry-mode all]] [--no-telemetry]
    [--uplink <ADDR>]
    [--cycles <N>]
```

`<TARGET>` is either a **source `.kdl` file** or a **bundle directory** (§4); `run` detects
which (a directory, or a `*.bundle`, is a bundle). Loading differs:

- **Bundle** → read its `mission.kdl`, fill each `Artifact::path` from the bundle directory,
  `resolve`. **Never invokes cargo.** `--build` is rejected here (nothing to build).
- **Source KDL** → the artifacts carry no located path (the KDL names crates, not absolute
  paths), so `run` must locate the `.so`s, and the only locator is the cargo build driver
  (incremental). Therefore **a source-KDL run requires `--build`** — it runs `build_artifacts`
  (compile-if-stale + locate) then resolves. Without `--build` (and not a bundle), `run` errors
  with guidance: `--build` to compile, or `package` then run the bundle. `--build` is thus the
  build→run shortcut (§2.4).

**Override flags** (precedence in §7): all mutate the `Wiring` after load, before `resolve`.

| Flag | Effect on `Wiring` |
| --- | --- |
| `--wall` | `coordinator.clock = ClockSpec::Wall` |
| `--sim-dt <SECS>` | `coordinator.clock = ClockSpec::Simulated { dt_secs }` |
| `--cycle-rate <HZ>` | `coordinator.cycle_rate = HZ` |
| `--telemetry <ADDR>` | replaces any `TcpDownlink` spec with `SystemSpec::tcp_downlink("telemetry", addr)` (enables it) |
| `--telemetry-mode all` | sets the mode of `--telemetry` (v1: `all`; `subset` stays KDL-only) |
| `--no-telemetry` | removes every `TcpDownlink` spec (disable even if the KDL declares one) |
| `--uplink <ADDR>` | replaces any `TcpUplink` spec with `SystemSpec::tcp_uplink("uplink", addr)` — enables the command uplink (its own connection, reading panel `SequenceCommand`s, `docs/messages.md` §4.4) even if the KDL doesn't declare one |
| `--cycles <N>` | `run_for(N)`; default `usize::MAX` (run until interrupted) |

`--wall`/`--sim-dt` are mutually exclusive (clap group). `--telemetry`/`--no-telemetry` are
mutually exclusive.

**Run output.** `run` is not silent: before the runtime starts it prints a banner (the
systems, the active clock, the telemetry target, the duration), and while running it emits a
cycle-progress heartbeat (read from a shared `Coordinator::progress()` counter) every couple
of seconds, then a completion/`hard-stopped` summary. So a long mission visibly advances and
the active config is never a guess.

**Telemetry operational notes** (surfaced in the banner):
- The downlink **connects once and does not auto-reconnect** (v1; telemetry.md). So **metor-panel
  must be listening before the mission starts** — `run` does a one-shot reachability probe and
  warns (`⚠ not reachable …`) when it is not, before any cycle runs.
- Under the **simulated** clock the loop free-runs (no pacing), so a live downlink races far
  ahead of real time; the banner suggests `--wall` whenever telemetry is on with a sim clock.
  For live panel viewing use `--wall --telemetry <addr>`.
- The uplink gets the same one-shot reachability probe as telemetry, reported on its own banner
  line; with no `--uplink`/KDL `TcpUplink` system the banner says `uplink: off — pass
  \`--uplink <addr>\` to receive panel commands` (`src/cli/mod.rs`). The link flags/banner key
  on the built-in **types** (`TCP_DOWNLINK_TYPE`/`TCP_UPLINK_TYPE`), never on instance names,
  so a user-written downlink/uplink system is untouched by them; several instances get one
  banner line each.

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

A bundle is a **plain directory** carrying everything `run` needs with no source tree and no
cargo. It is **platform-specific** — it contains compiled `.so`s — so it is built for, and run
on, one target.

### 4.1 Layout

```
dist/adcs.bundle/
├── mission.kdl          # the wiring manifest (the same KDL surface parse() reads)
├── meta.kdl             # sidecar metadata (abi version, profile, timestamp, schemas)
├── libadcs_plant.so     # the built cdylibs, one per artifact, copied in
├── libadcs_nav.so
└── libadcs_ctrl.so
```

### 4.2 `mission.kdl` — the manifest

The manifest **is the wiring KDL** (coordinator/artifact/system/connect/telemetry — the
existing `parse` surface), so a bundle stays human-readable and re-parseable with no new
format. It is the source mission's KDL, essentially **verbatim**, because the artifact
references are already relocatable:

- Each `artifact` node names its library by **stem** (`lib="adcs_plant"`), not by an absolute
  path. The bundle places the compiled file (`lib<stem>.so`) **next to** the manifest, so the
  reference resolves relative to the bundle directory — the relocatability is inherent, no
  path rewriting needed.
- `crate=` is retained for provenance but **unused at run** (the bundle never builds).

So `package` does not need a KDL *serializer*: it copies the source manifest (optionally
normalized) and the built `.so`s. (If a future front-end produces a builder-origin `Wiring`
with no source text, `package` would serialize the model back to this KDL surface — out of
scope for v1, which packages from a KDL source.)

### 4.3 `meta.kdl` — the sidecar

Metadata that is *about* the bundle rather than the wiring:

```kdl
meta {
    abi_version 4                 // FSW_ABI_VERSION the cdylibs were built against
    profile "release"            // build profile
    built_at_unix 1782000000     // epoch seconds — no date-formatting dep
}
```

`abi_version` is the **load-time guard**: `run` refuses a bundle whose `abi_version` differs
from the host `FSW_ABI_VERSION` (4 today — see `src/abi/mod.rs`'s
version-history comment) with a clear error, before opening any `.so`. **As shipped, `meta.kdl`
carries only these three fields** (`abi_version`/`profile`/`built_at_unix`,
`src/wiring/bundle.rs`'s `write_bundle`) — the per-system params-schema dump sketched in an
earlier draft of this doc was **not implemented**; `run` still opens each `.so` and reads its
live `params_schema()` at resolve, so offline (no-`.so`) schema validation stays future work
(§8.6).

### 4.4 How `run` loads a bundle vs a source KDL

```
load_wiring(args):
  if target is a bundle dir:
      meta = parse meta.kdl;  ensure meta.abi_version == FSW_ABI_VERSION
      wiring = parse(read "<dir>/mission.kdl")
      for artifact in wiring.artifacts:                      # the only framework addition `run` needs
          artifact.path = Some(dir.join(cdylib_file_name(&artifact.cdylib)))
      return wiring                                          # NO cargo
  else (source .kdl):
      require args.build (else error: pass --build, or package then run the bundle)
      wiring = parse(read target)
      build_artifacts(&mut wiring, &opts)                    # compile-if-stale + locate
      return wiring
```

`resolve` already requires each `Artifact::path` to be `Some` (`ArtifactNotBuilt` otherwise)
and then `DlPack::open`s it. The bundle path fills `path` from the directory instead of from
cargo — the **single small framework touch** the bundle needs.

### 4.5 Relation to the `Wiring` model & the framework change

`Artifact { id, crate_name, cdylib, path }` is the unit. The bundle path-fill
sets `path` from `dir + cdylib_file_name(cdylib)`. Two minimal framework additions:

1. **`cdylib_file_name(stem) -> String`** in the wiring module — the platform decoration
   (`lib{stem}.dylib` / `lib{stem}.so` / `{stem}.dll`), moved out of the example's
   `cdylib_name`. `build_artifacts`, `package`, and the bundle path-fill all use it.
2. A tiny **`locate_in_dir(&mut Wiring, dir)`** helper (or inline in `cmd_run`, since
   `Artifact` fields are `pub`) that does the path-fill above.

The packaging side adds `write_bundle(&Wiring, src_kdl, &opts, dir)` (copy `.so`s + emit
`meta.kdl` + place `mission.kdl`). These live in the `wiring` module (model-adjacent), keeping
the `cli` handlers thin orchestrators. A future tar/zip step wraps the directory and is out of
scope.

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
