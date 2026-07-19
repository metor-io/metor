use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use miette::IntoDiagnostic;

use crate::wiring::{
    BuildOptions, Builder, ClockSpec, METOR_EXTENSION, PackBuildOptions, PackDevOptions,
    PackageOptions, Registry, StubgenOptions, WIRING_FILE_NAME, Wiring, build_target,
    eval_python_mission, is_python_mission, load_bundle, locate_artifacts, pack_assemble,
    pack_build, pack_dev, pack_publish, provision_artifacts, refresh_dev_packs, resolve, stubgen,
    unpack_metor, write_bundle,
};

mod ui;

/// The fully parsed command line, produced from argv by [`run`].
#[derive(Parser, Debug)]
#[command(
    name = "metor-fsw",
    version,
    about = "Build, package, and run metor-fsw missions",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Compile the cdylibs the wiring references; print where they landed.
    Build(BuildArgs),
    /// Produce a relocatable bundle directory (the cdylibs plus a manifest).
    Package(PackageArgs),
    /// Run a mission: a source `.py` (built automatically) or a bundle dir.
    Run(RunArgs),
    /// Generate the typed Python pack modules (`packs/<id>.py`) for a mission.
    Stubgen(StubgenArgs),
    /// Pack-crate packaging (`docs/design-packaging.md`).
    #[command(subcommand)]
    Pack(PackCmd),
}

#[derive(Subcommand, Debug)]
enum PackCmd {
    /// Build the host triple and lay out the pack's editable `.metor/`
    /// payload (typed module + `_libs/<triple>/`); what a pack's PEP 517
    /// backend runs on `uv sync`.
    Dev(PackDevArgs),
    /// Build the pack's wheel payloads for every configured target and
    /// assemble the fat `py3-none-any` wheel (or stage triples for a CI
    /// matrix with `--libs-out`).
    Build(PackBuildArgs),
    /// Join `--libs` stagings from matrix runners into the fat wheel,
    /// re-verifying the manifests are identical across triples.
    Assemble(PackAssembleArgs),
    /// Build (or take) the pack's wheel and hand it to `uv publish`.
    Publish(PackPublishArgs),
}

#[derive(Args, Debug)]
struct PackBuildArgs {
    /// The pack crate directory (holds `pyproject.toml` + `Cargo.toml`).
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Build this triple instead of the config's `targets` (repeatable).
    #[arg(long = "target", value_name = "TRIPLE")]
    target: Vec<String>,
    /// Stage `<triple>/` payloads here without assembling (the CI-matrix
    /// half; join with `pack assemble`).
    #[arg(long, value_name = "DIR", conflicts_with = "wheel_out")]
    libs_out: Option<PathBuf>,
    /// Write the assembled wheel here (default: `<dir>/dist`).
    #[arg(long, value_name = "DIR")]
    wheel_out: Option<PathBuf>,
    /// Override the configured builder for the cargo-family kinds.
    #[arg(long, value_name = "cargo|zigbuild")]
    builder: Option<String>,
}

#[derive(Args, Debug)]
struct PackAssembleArgs {
    /// The pack crate directory (holds `pyproject.toml` + `Cargo.toml`).
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// A staging directory of `<triple>/` payloads (repeatable).
    #[arg(long = "libs", value_name = "DIR", required = true)]
    libs: Vec<PathBuf>,
    /// Write the assembled wheel here.
    #[arg(long, value_name = "DIR", required = true)]
    wheel_out: PathBuf,
}

#[derive(Args, Debug)]
struct PackPublishArgs {
    /// The pack crate directory (holds `pyproject.toml` + `Cargo.toml`).
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// The index to publish to: a configured index name, or a URL.
    #[arg(long, value_name = "NAME|URL")]
    index: Option<String>,
    /// Publish this wheel instead of building one.
    #[arg(long, value_name = "FILE")]
    wheel: Option<PathBuf>,
    /// Print the `uv publish` invocation instead of running it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct PackDevArgs {
    /// The pack crate directory (holds `pyproject.toml` + `Cargo.toml`).
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Build the `--release` profile.
    #[arg(long)]
    release: bool,
    /// An extra arg appended to the `cargo build` (repeatable).
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// The `.py` mission file.
    mission: PathBuf,
    /// Build the `--release` profile (default: debug).
    #[arg(long)]
    release: bool,
    /// Provision for this target triple: prebuilt artifacts select their
    /// `<triple>/` payload, crate artifacts cross-compile.
    #[arg(long, value_name = "TRIPLE")]
    target: Option<String>,
    /// An extra arg appended to every `cargo build` (repeatable).
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
    /// Skip the `<cdylib>.manifest` sidecars, for pack crates that cannot
    /// build for the host architecture.
    #[arg(long)]
    no_manifest_sidecar: bool,
}

#[derive(Args, Debug)]
struct PackageArgs {
    /// The `.py` mission source. Required unless `--check-ir`.
    mission: Option<PathBuf>,
    /// The bundle output: a directory (conventionally `*.bundle`), or a
    /// single-file `*.metor` archive (dispatched by the `.metor` extension).
    #[arg(short = 'o', long = "out", value_name = "OUT")]
    out: Option<PathBuf>,
    /// Package for this target triple: prebuilt artifacts select their
    /// `<triple>/` payload (no cargo), crate artifacts cross-compile, and the
    /// bundle records the triple. Defaults to the host.
    #[arg(long, value_name = "TRIPLE")]
    target: Option<String>,
    /// Instead of packaging, re-evaluate the given bundle's provenance source
    /// and diff the produced IR against its frozen `wiring.json`; exit non-zero
    /// on drift (the determinism gate, runnable in CI).
    #[arg(long = "check-ir", value_name = "BUNDLE")]
    check_ir: Option<PathBuf>,
    /// Build the `--release` profile (default: debug).
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable).
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
    /// Skip the `<cdylib>.manifest` sidecars, for pack crates that cannot
    /// build for the host architecture.
    #[arg(long)]
    no_manifest_sidecar: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// A source `.py` mission (built automatically) or a bundle directory
    /// (cargo-free). Defaults to `mission.py` in the current directory.
    target: Option<PathBuf>,
    /// Locate previously built cdylibs without running cargo; errors when one
    /// is missing. A no-op for bundles, which are always cargo-free.
    #[arg(long)]
    no_build: bool,
    /// Build (or with `--no-build`, locate) the `--release` profile.
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable).
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
    /// Skip the `<cdylib>.manifest` sidecars, for pack crates that cannot
    /// build for the host architecture.
    #[arg(long)]
    no_manifest_sidecar: bool,
    /// Use a paced wall clock, overriding the mission's clock.
    #[arg(long, group = "clock")]
    wall: bool,
    /// Use a free-running simulated clock with this per-cycle step in seconds,
    /// overriding the mission's clock.
    #[arg(long, value_name = "SECS", group = "clock")]
    sim_dt: Option<f64>,
    /// Override the coordinator cycle rate (Hz).
    #[arg(long, value_name = "HZ")]
    cycle_rate: Option<f64>,
    /// Run this many cycles, then stop (default: run until interrupted).
    #[arg(long, value_name = "N")]
    cycles: Option<usize>,
}

#[derive(Args, Debug)]
struct StubgenArgs {
    /// The mission directory whose `pyproject.toml` lists the artifacts
    /// (default: the current directory). Stubs land in its `packs/`.
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Write the generated `packs` package here instead of `<dir>/packs`
    /// (for build front-ends that generate stubs into a build directory).
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,
    /// Verify the checked-in stubs are byte-identical instead of writing them
    /// (the CI gate); exits non-zero if any are stale.
    #[arg(long)]
    check: bool,
    /// Require the artifacts prebuilt (locate them without running cargo).
    #[arg(long)]
    no_build: bool,
    /// Build the `--release` profile.
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable).
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
}

pub async fn run() -> miette::Result<()> {
    // Route a re-executed worker child before anything else (process
    // systems; a no-op read of one env var otherwise).
    crate::proc::worker_entry();
    ui::init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Build(a) => cmd_build(a),
        Command::Package(a) => cmd_package(a),
        Command::Run(a) => cmd_run(a).await,
        Command::Stubgen(a) => cmd_stubgen(a),
        Command::Pack(PackCmd::Dev(a)) => cmd_pack_dev(a),
        Command::Pack(PackCmd::Build(a)) => cmd_pack_build(a),
        Command::Pack(PackCmd::Assemble(a)) => cmd_pack_assemble(a),
        Command::Pack(PackCmd::Publish(a)) => cmd_pack_publish(a),
    }
}

/// `pack build`: per-target payloads, then the fat wheel (or a matrix
/// staging with `--libs-out`).
fn cmd_pack_build(args: PackBuildArgs) -> miette::Result<()> {
    let builder = match args.builder.as_deref() {
        None => None,
        Some("cargo") => Some(Builder::Cargo),
        Some("zigbuild") => Some(Builder::Zigbuild),
        Some(other) => {
            return Err(miette::miette!(
                "unknown --builder `{other}` (cargo, zigbuild; command templates \
                 configure via [tool.metor.pack.builder])"
            ));
        }
    };
    let report = pack_build(
        &args.dir,
        &PackBuildOptions {
            targets: args.target,
            libs_out: args.libs_out,
            wheel_out: args.wheel_out,
            builder,
        },
    )
    .into_diagnostic()?;
    match &report.wheel {
        Some(wheel) => println!("  {} ({})", wheel.display(), report.triples.join(", ")),
        None => println!(
            "  staged {} under {}",
            report.triples.join(", "),
            report.staged.display()
        ),
    }
    Ok(())
}

/// `pack assemble`: join matrix stagings into the fat wheel.
fn cmd_pack_assemble(args: PackAssembleArgs) -> miette::Result<()> {
    let report = pack_assemble(&args.dir, &args.libs, &args.wheel_out).into_diagnostic()?;
    let wheel = report.wheel.expect("assemble always writes a wheel");
    println!("  {} ({})", wheel.display(), report.triples.join(", "));
    Ok(())
}

/// `pack publish`: hand the wheel to `uv publish`.
fn cmd_pack_publish(args: PackPublishArgs) -> miette::Result<()> {
    let wheel = pack_publish(
        &args.dir,
        args.index.as_deref(),
        args.wheel.as_deref(),
        args.dry_run,
    )
    .into_diagnostic()?;
    println!("  {}", wheel.display());
    Ok(())
}

/// `pack dev`: build the pack crate for the host and lay out its editable
/// `.metor/` payload (typed module + per-triple lib).
fn cmd_pack_dev(args: PackDevArgs) -> miette::Result<()> {
    let report = pack_dev(
        &args.dir,
        &PackDevOptions {
            release: args.release,
            cargo_args: args.cargo_arg,
        },
    )
    .into_diagnostic()?;
    println!(
        "  {} ({})\n  wrote {}",
        report.lib_path.display(),
        report.triple,
        report.module_dir.join("__init__.py").display()
    );
    Ok(())
}

/// `stubgen`: read the mission's `pyproject.toml`, (build and) describe each
/// listed artifact, and write — or, with `--check`, verify — its typed pack
/// module. Deprecated in favor of per-pack `pack dev`
/// (`docs/design-packaging.md` §10); kept working for one release.
fn cmd_stubgen(args: StubgenArgs) -> miette::Result<()> {
    eprintln!(
        "warning: mission-level `stubgen` is deprecated; give each pack crate a \
         `pyproject.toml` and use `metor-fsw pack dev` (docs/design-packaging.md §10)"
    );
    let opts = StubgenOptions {
        mission_dir: args.dir,
        out_dir: args.out_dir,
        check: args.check,
        build: !args.no_build,
        release: args.release,
        cargo_args: args.cargo_arg,
    };
    let report = stubgen(&opts).into_diagnostic()?;
    if opts.check {
        println!(
            "stubgen --check: {} module(s) up to date",
            report.modules.len()
        );
    } else {
        for path in &report.modules {
            println!("  wrote {}", path.display());
        }
    }
    Ok(())
}

/// `--target` is sugar for the `--cargo-arg --target …` spelling: one flag
/// drives prebuilt selection, crate cross-builds, and the recorded triple.
fn merge_target(cargo_args: &[String], target: Option<&str>) -> Vec<String> {
    let mut merged = cargo_args.to_vec();
    if let Some(triple) = target {
        merged.extend(["--target".to_string(), triple.to_string()]);
    }
    merged
}

/// Load a source mission into a [`Wiring`]. Missions are Python: a `.py` file
/// is evaluated by a subprocess CPython. A `.kdl` file gets a clear
/// removed-feature error; any other extension is unrecognized.
fn load_source(path: &Path) -> miette::Result<Wiring> {
    if is_python_mission(path) {
        return eval_python_mission(path);
    }
    if path.extension().is_some_and(|e| e == "kdl") {
        return Err(miette::miette!(
            "KDL mission support was removed; missions are Python (`.py`). Port `{}` to a \
             `mission.py` (see docs/design-python-config.md)",
            path.display()
        ));
    }
    Err(miette::miette!(
        "unrecognized mission `{}`; missions are Python (`.py`)",
        path.display()
    ))
}

/// Refresh a source mission's dev packs — the path-source pack dependencies
/// its pyproject names — so the generated modules (manifest hashes, params)
/// the mission imports are current before it is evaluated. Prebuilt pack
/// artifacts are only *selected* at provisioning, so this is where their
/// sources get rebuilt; cargo's incremental build makes a clean tree a no-op.
fn refresh_source_packs(
    mission: &Path,
    release: bool,
    cargo_args: &[String],
) -> miette::Result<()> {
    let dir = match mission.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    refresh_dev_packs(
        dir,
        &PackDevOptions {
            release,
            cargo_args: cargo_args.to_vec(),
        },
    )
    .into_diagnostic()?;
    Ok(())
}

/// `build`: load the wiring, provision every artifact's `.so`, print them.
fn cmd_build(args: BuildArgs) -> miette::Result<()> {
    let cargo_args = merge_target(&args.cargo_arg, args.target.as_deref());
    refresh_source_packs(&args.mission, args.release, &cargo_args)?;
    let mut wiring = load_source(&args.mission)?;
    provision_artifacts(
        &mut wiring,
        &build_opts(args.release, &cargo_args, args.no_manifest_sidecar),
    )
    .into_diagnostic()?;
    for a in &wiring.artifacts {
        let path = a
            .path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        println!("  {:<28} →  {path}", a.crate_name);
    }
    Ok(())
}

fn cmd_package(args: PackageArgs) -> miette::Result<()> {
    if let Some(bundle) = &args.check_ir {
        return cmd_check_ir(bundle);
    }
    // The bundle freezes the evaluated `Wiring` as IR, so the packaged mission
    // runs with no Python and no config parse on target.
    let mission = args.mission.as_deref().ok_or_else(|| {
        miette::miette!("`package` needs a mission source (`.py`), or `--check-ir <bundle>`")
    })?;
    let out = args
        .out
        .as_deref()
        .ok_or_else(|| miette::miette!("`package` needs an output path (`-o <out>`)"))?;
    let cargo_args = merge_target(&args.cargo_arg, args.target.as_deref());
    refresh_source_packs(mission, args.release, &cargo_args)?;
    let mut wiring = load_source(mission)?;
    provision_artifacts(
        &mut wiring,
        &build_opts(args.release, &cargo_args, args.no_manifest_sidecar),
    )
    .into_diagnostic()?;
    let opts = PackageOptions {
        release: args.release,
        target: build_target(&cargo_args),
        provenance: Some(mission.to_path_buf()),
        // Current time; a reproducible build pins this.
        built_at_unix: None,
    };
    write_bundle(&wiring, &opts, out).into_diagnostic()?;
    println!(
        "packaged {} artifacts, {} systems → {}",
        wiring.artifacts.len(),
        wiring.systems.len(),
        out.display()
    );
    Ok(())
}

/// `package --check-ir <bundle>`: re-evaluate the bundle's provenance source
/// and diff the produced IR against the frozen `wiring.json`, exiting non-zero
/// on any drift — the determinism backstop, runnable in CI.
///
/// Both sides are normalized before the diff: artifact `path`s are stripped
/// (never in the frozen IR anyway) and `src` file names cleared, since the
/// provenance copy sits at a different path than the original source, which
/// would otherwise read as spurious drift. The line/column of every anchor is
/// kept, so a genuine emission change is still caught.
fn cmd_check_ir(bundle: &Path) -> miette::Result<()> {
    // Materialize a readable directory: a `.metor` unpacks to a temp dir held
    // for the duration of the check, a directory is read in place.
    let _held;
    let dir = if bundle.is_file() && bundle.extension().is_some_and(|e| e == METOR_EXTENSION) {
        let unpacked = unpack_metor(bundle).into_diagnostic()?;
        let path = unpacked.path().to_path_buf();
        _held = Some(unpacked);
        path
    } else {
        bundle.to_path_buf()
    };

    let frozen_text = read_file(&dir.join(WIRING_FILE_NAME))?;
    let frozen: Wiring = serde_json::from_str(&frozen_text).map_err(|e| {
        miette::miette!(
            "bundle `{}` has an unreadable wiring.json: {e}",
            bundle.display()
        )
    })?;

    let source = find_provenance(&dir).ok_or_else(|| {
        miette::miette!(
            "bundle `{}` carries no provenance source (mission.py); cannot --check-ir",
            bundle.display()
        )
    })?;
    let produced = load_source(&source)?;

    if normalized_ir(&produced) == normalized_ir(&frozen) {
        println!("--check-ir: {} reproduces its frozen IR", bundle.display());
        Ok(())
    } else {
        Err(miette::miette!(
            "IR drift: re-evaluating `{}` no longer reproduces the bundle's wiring.json \
             (nondeterministic emission, or the bundle is stale) — repackage it",
            source.display()
        ))
    }
}

/// The provenance source inside a bundle directory.
fn find_provenance(dir: &Path) -> Option<PathBuf> {
    let p = dir.join("mission.py");
    p.exists().then_some(p)
}

/// The normalized-for-diff JSON of a `Wiring`: artifact paths stripped and
/// every `src` file name cleared (keeping line/column), so the comparison is
/// immune to where the source physically sat.
fn normalized_ir(wiring: &Wiring) -> String {
    let mut w = wiring.path_stripped();
    let clear = |src: &mut Option<crate::wiring::SourceRef>| {
        if let Some(s) = src {
            s.file = None;
        }
    };
    for a in &mut w.artifacts {
        clear(&mut a.src);
    }
    for s in &mut w.systems {
        clear(&mut s.src);
    }
    for s in &mut w.slots {
        clear(&mut s.src);
        for occ in &mut s.allow {
            clear(&mut occ.src);
        }
    }
    for e in &mut w.edges {
        clear(&mut e.src);
    }
    for sc in &mut w.scopes {
        clear(&mut sc.src);
    }
    serde_json::to_string(&w).expect("Wiring serializes to JSON")
}

async fn cmd_run(args: RunArgs) -> miette::Result<()> {
    let target = match &args.target {
        Some(target) => target.clone(),
        None => detect_mission()?,
    };
    refresh_run_packs(&target, &args)?;
    let mut wiring = load_run_wiring(&target)?;
    apply_overrides(&mut wiring, &args)?;
    ui::print_preflight(&wiring, &target);
    provision_run_artifacts(&mut wiring, &target, &args)?;

    let cycles = args.cycles.unwrap_or(usize::MAX);
    let mut coord = resolve(&wiring, &Registry::with_builtins())?;

    // Enter the async runtime at the leaf; `run_for` does init, the cycle loop,
    // and shutdown. The heartbeat is a side task reading the shared progress
    // counter, since the loop holds `&mut coord` for its whole life.
    coord.run_for(cycles).await;

    // A hard-stopped system is a failed run: name each one and exit non-zero,
    // so a supervisor (or CI) sees the failure instead of a clean exit.
    let stopped = coord.stopped();
    if stopped.is_empty() {
        return Ok(());
    }
    for sys in stopped {
        eprintln!("system `{}` stopped: {:?}", sys.name, sys.reason);
    }
    Err(miette::miette!(
        "{} system(s) hard-stopped during the run",
        stopped.len()
    ))
}

/// `run` with no target uses `mission.py` in the current directory.
fn detect_mission() -> miette::Result<PathBuf> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    detect_mission_in(&cwd)
}

/// [`detect_mission`] against an explicit directory (the current directory is
/// process-global, so tests probe this).
fn detect_mission_in(dir: &Path) -> miette::Result<PathBuf> {
    let mission = dir.join("mission.py");
    if mission.exists() {
        return Ok(mission);
    }
    Err(miette::miette!(
        "no `mission.py` in `{}`; pass a mission `.py` or a bundle: `metor-fsw run <target>`",
        dir.display()
    ))
}

/// Refresh a source mission's dev packs before evaluation
/// ([`refresh_source_packs`]). Skipped for bundles, which are cargo-free by
/// contract, and under `--no-build`, which extends its "locate, never build"
/// promise to the packs.
fn refresh_run_packs(target: &Path, args: &RunArgs) -> miette::Result<()> {
    if is_bundle(target) || args.no_build {
        return Ok(());
    }
    refresh_source_packs(target, args.release, &args.cargo_arg)
}

/// Load `run`'s `<TARGET>` into a [`Wiring`]: a bundle directory loads
/// cargo-free via [`load_bundle`] with its artifact paths already recorded, a
/// source `.py` is evaluated (with its dev packs already refreshed by
/// [`refresh_run_packs`]) and its artifacts provisioned separately by
/// [`provision_run_artifacts`].
fn load_run_wiring(target: &Path) -> miette::Result<Wiring> {
    if is_bundle(target) {
        return load_bundle(target).into_diagnostic();
    }
    load_source(target)
}

/// Fill a source mission's artifact paths: the cargo build driver by default
/// (incremental, so a fresh tree is a no-op), or a cargo-free search of the
/// workspace target dir with `--no-build`. A bundle's paths are already
/// recorded, so this is a no-op for it.
fn provision_run_artifacts(
    wiring: &mut Wiring,
    target: &Path,
    args: &RunArgs,
) -> miette::Result<()> {
    if is_bundle(target) {
        return Ok(());
    }
    if args.no_build {
        let dir = match target.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        return locate_artifacts(wiring, dir, args.release).into_diagnostic();
    }
    provision_artifacts(
        wiring,
        &build_opts(args.release, &args.cargo_arg, args.no_manifest_sidecar),
    )
    .into_diagnostic()
}

/// A `<TARGET>` is a bundle if it is a directory (the bundle layout), ends in
/// `.bundle`, or is a single-file `.metor` archive.
fn is_bundle(path: &Path) -> bool {
    path.is_dir()
        || path
            .extension()
            .is_some_and(|e| e == "bundle" || e == "metor")
}

/// Apply `run`'s override flags onto the loaded [`Wiring`] before [`resolve`].
/// A flag always beats the mission's own setting.
fn apply_overrides(wiring: &mut Wiring, args: &RunArgs) -> miette::Result<()> {
    if args.wall {
        wiring.coordinator.clock = ClockSpec::Wall;
    } else if let Some(dt_secs) = args.sim_dt {
        wiring.coordinator.clock = ClockSpec::Simulated { dt_secs };
    }
    if let Some(rate) = args.cycle_rate {
        wiring.coordinator.cycle_rate = rate;
    }
    Ok(())
}

/// Build the [`BuildOptions`] from the shared
/// `--release`/`--cargo-arg`/`--no-manifest-sidecar` flags.
fn build_opts(release: bool, cargo_arg: &[String], no_manifest_sidecar: bool) -> BuildOptions {
    BuildOptions {
        release,
        extra_args: cargo_arg.to_vec(),
        manifest_sidecar: !no_manifest_sidecar,
    }
}

/// Read a file to a string, mapping I/O errors to a clean diagnostic.
fn read_file(path: &Path) -> miette::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| miette::miette!("failed to read `{}`: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normalized IR clears the `src` file name (the provenance copy sits at a
    /// different path than the original source), so the same mission evaluated
    /// from two paths is not spurious drift — but a real emission change is.
    #[test]
    fn normalized_ir_ignores_source_path_but_not_content() {
        use crate::wiring::{SourceRef, WiringBuilder};

        let base = || {
            WiringBuilder::new()
                .coordinator(100.0, ClockSpec::Wall)
                .system("a")
                .ty("Src")
                .end()
                .build()
        };
        let anchor = |file: &str| {
            Some(SourceRef {
                file: Some(file.into()),
                line: 1,
                col: 1,
            })
        };

        let mut frozen = base();
        frozen.systems[0].src = anchor("/build/mission.py");
        let mut relocated = base();
        relocated.systems[0].src = anchor("/tmp/bundle/mission.py");
        assert_eq!(
            normalized_ir(&frozen),
            normalized_ir(&relocated),
            "same mission, different provenance path — not drift"
        );

        let mut changed = base();
        changed.systems[0].src = anchor("/build/mission.py");
        changed.coordinator.cycle_rate = 200.0;
        assert_ne!(
            normalized_ir(&frozen),
            normalized_ir(&changed),
            "a real emission change is drift"
        );
    }

    /// `run` with no target accepts exactly `mission.py` in the directory and
    /// errors helpfully otherwise.
    #[test]
    fn detect_mission_wants_mission_py() {
        let dir = tempfile::tempdir().unwrap();
        let err = detect_mission_in(dir.path()).expect_err("nothing to detect yet");
        assert!(err.to_string().contains("mission.py"), "{err}");
        std::fs::write(dir.path().join("mission.py"), "").unwrap();
        assert_eq!(
            detect_mission_in(dir.path()).unwrap(),
            dir.path().join("mission.py")
        );
    }

    /// The pre-eval pack refresh leaves bundles alone (cargo-free by
    /// contract), honors `--no-build`, and is a clean no-op for a mission
    /// with no path-source packs.
    #[test]
    fn refresh_run_packs_skips_and_noops() {
        let args = |no_build| RunArgs {
            target: None,
            no_build,
            release: false,
            cargo_arg: Vec::new(),
            no_manifest_sidecar: false,
            wall: false,
            sim_dt: None,
            cycle_rate: None,
            cycles: None,
        };
        let dir = tempfile::tempdir().unwrap();
        refresh_run_packs(dir.path(), &args(false)).expect("bundle dir: skipped");

        let mission = dir.path().join("mission.py");
        std::fs::write(&mission, "").unwrap();
        refresh_run_packs(&mission, &args(true)).expect("--no-build: skipped");
        refresh_run_packs(&mission, &args(false)).expect("no pyproject: nothing to refresh");
    }

    /// Provenance discovery finds `mission.py`, and is `None` when a bundle
    /// carries none.
    #[test]
    fn find_provenance_finds_python() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_provenance(dir.path()).is_none(), "no provenance yet");
        std::fs::write(dir.path().join("mission.py"), "").unwrap();
        assert_eq!(
            find_provenance(dir.path()),
            Some(dir.path().join("mission.py"))
        );
    }
}
