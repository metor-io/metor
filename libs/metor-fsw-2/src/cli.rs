use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use miette::IntoDiagnostic;

use crate::wiring::{
    BuildOptions, ClockSpec, METOR_EXTENSION, PackageOptions, Registry, StubgenOptions,
    WIRING_FILE_NAME, Wiring, build_artifacts, build_target, eval_python_mission, is_python_mission,
    load_bundle, metor_config_version, parse_with_origin, resolve, stubgen, unpack_metor,
    write_bundle,
};

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
    /// Load a wiring (a source `.kdl` with `--build`, or a bundle dir) and run it.
    Run(RunArgs),
    /// Generate the typed Python pack modules (`packs/<id>.py`) for a mission.
    Stubgen(StubgenArgs),
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// The wiring KDL file.
    kdl: PathBuf,
    /// Build the `--release` profile (default: debug).
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable), e.g.
    /// `--cargo-arg --target --cargo-arg aarch64-unknown-linux-gnu`.
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
    /// Skip the `<cdylib>.manifest` sidecars, for pack crates that cannot
    /// build for the host architecture.
    #[arg(long)]
    no_manifest_sidecar: bool,
}

#[derive(Args, Debug)]
struct PackageArgs {
    /// The mission source (`.py` or `.kdl`). Required unless `--check-ir`.
    kdl: Option<PathBuf>,
    /// The bundle output: a directory (conventionally `*.bundle`), or a
    /// single-file `*.metor` archive (dispatched by the `.metor` extension).
    #[arg(short = 'o', long = "out", value_name = "OUT")]
    out: Option<PathBuf>,
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
    /// A source `.kdl` file (requires `--build`) or a bundle directory (cargo-free).
    target: PathBuf,
    /// Build the cdylibs first; required when the target is a source `.kdl`.
    #[arg(long)]
    build: bool,
    /// Build the `--release` profile when `--build` is set.
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable), with `--build`.
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
    /// Skip the `<cdylib>.manifest` sidecars, for pack crates that cannot
    /// build for the host architecture.
    #[arg(long)]
    no_manifest_sidecar: bool,
    /// Use a paced wall clock, overriding the KDL's clock.
    #[arg(long, group = "clock")]
    wall: bool,
    /// Use a free-running simulated clock with this per-cycle step in seconds,
    /// overriding the KDL's clock.
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
    let cli = Cli::parse();
    match cli.command {
        Command::Build(a) => cmd_build(a),
        Command::Package(a) => cmd_package(a),
        Command::Run(a) => cmd_run(a).await,
        Command::Stubgen(a) => cmd_stubgen(a),
    }
}

/// `stubgen`: read the mission's `pyproject.toml`, (build and) describe each
/// listed artifact, and write — or, with `--check`, verify — its typed pack
/// module.
fn cmd_stubgen(args: StubgenArgs) -> miette::Result<()> {
    let opts = StubgenOptions {
        mission_dir: args.dir,
        check: args.check,
        build: !args.no_build,
        release: args.release,
        cargo_args: args.cargo_arg,
    };
    let report = stubgen(&opts).into_diagnostic()?;
    if opts.check {
        println!("stubgen --check: {} module(s) up to date", report.modules.len());
    } else {
        for path in &report.modules {
            println!("  wrote {}", path.display());
        }
    }
    Ok(())
}

/// Load a source mission into a [`Wiring`], dispatched by extension: a `.py`
/// mission is evaluated by a subprocess CPython, anything else is parsed as KDL.
fn load_source(path: &Path) -> miette::Result<Wiring> {
    if is_python_mission(path) {
        eval_python_mission(path)
    } else {
        let text = read_file(path)?;
        Ok(parse_with_origin(&text, Some(&path.to_string_lossy()))?)
    }
}

/// `build`: load the wiring, compile and locate every artifact's `.so`, print them.
fn cmd_build(args: BuildArgs) -> miette::Result<()> {
    let mut wiring = load_source(&args.kdl)?;
    build_artifacts(
        &mut wiring,
        &build_opts(args.release, &args.cargo_arg, args.no_manifest_sidecar),
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
    // Both front-ends produce a `Wiring`; the bundle freezes it as IR, so a
    // `.py` mission packages through the same path a `.kdl` does — no Python
    // and no KDL parser is needed to run the result.
    let kdl = args.kdl.as_deref().ok_or_else(|| {
        miette::miette!("`package` needs a mission source (`.py`/`.kdl`), or `--check-ir <bundle>`")
    })?;
    let out = args
        .out
        .as_deref()
        .ok_or_else(|| miette::miette!("`package` needs an output path (`-o <out>`)"))?;
    let mut wiring = load_source(kdl)?;
    build_artifacts(
        &mut wiring,
        &build_opts(args.release, &args.cargo_arg, args.no_manifest_sidecar),
    )
    .into_diagnostic()?;
    let opts = PackageOptions {
        release: args.release,
        target: build_target(&args.cargo_arg),
        // The recorder version is provenance; a Python mission was evaluated by
        // this host's embedded (or $METOR_CONFIG_PY) recorder.
        metor_config_version: is_python_mission(kdl)
            .then(|| metor_config_version().to_string()),
        provenance: Some(kdl.to_path_buf()),
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
    let frozen: Wiring = serde_json::from_str(&frozen_text)
        .map_err(|e| miette::miette!("bundle `{}` has an unreadable wiring.json: {e}", bundle.display()))?;

    let source = find_provenance(&dir).ok_or_else(|| {
        miette::miette!(
            "bundle `{}` carries no provenance source (mission.py/.kdl); cannot --check-ir",
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

/// The provenance source inside a bundle directory: `mission.py` preferred,
/// then `mission.kdl`.
fn find_provenance(dir: &Path) -> Option<PathBuf> {
    ["mission.py", "mission.kdl"]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
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
    let mut wiring = load_run_wiring(&args)?;
    apply_overrides(&mut wiring, &args)?;

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

/// Resolve `run`'s `<TARGET>` into a located [`Wiring`]. A bundle directory
/// loads cargo-free via [`load_bundle`]. A source `.kdl` requires `--build`,
/// because the cargo build driver is the only artifact locator and `.so` paths
/// are not persisted between a `build` and a later `run`.
fn load_run_wiring(args: &RunArgs) -> miette::Result<Wiring> {
    if is_bundle(&args.target) {
        if args.build {
            return Err(miette::miette!(
                "`--build` is not valid for a bundle (nothing to build); run the bundle \
                 directly, or `--build` a source `.kdl`"
            ));
        }
        return load_bundle(&args.target).into_diagnostic();
    }
    if !args.build {
        return Err(miette::miette!(
            "running a source `.kdl` requires `--build` (to compile and locate the cdylibs); \
             or `metor-fsw package {} -o <dir>` and run the bundle",
            args.target.display()
        ));
    }
    let mut wiring = load_source(&args.target)?;
    build_artifacts(
        &mut wiring,
        &build_opts(args.release, &args.cargo_arg, args.no_manifest_sidecar),
    )
    .into_diagnostic()?;
    Ok(wiring)
}

/// A `<TARGET>` is a bundle if it is a directory (the bundle layout), ends in
/// `.bundle`, or is a single-file `.metor` archive.
fn is_bundle(path: &Path) -> bool {
    path.is_dir() || path.extension().is_some_and(|e| e == "bundle" || e == "metor")
}

/// Apply `run`'s override flags onto the loaded [`Wiring`] before [`resolve`].
/// A flag always beats the KDL.
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

    /// Normalized IR ignores the `src` file name (the provenance copy sits at a
    /// different path than the original source), so re-parsing the same text
    /// under a different origin is not spurious drift — but a real change is.
    #[test]
    fn normalized_ir_ignores_source_path_but_not_content() {
        let kdl = "coordinator cycle_rate=100.0\nsystem \"a\" type=\"Src\"\n";
        let frozen = parse_with_origin(kdl, Some("/build/mission.kdl")).unwrap();
        let reparsed = parse_with_origin(kdl, Some("/tmp/bundle/mission.kdl")).unwrap();
        assert_eq!(
            normalized_ir(&frozen),
            normalized_ir(&reparsed),
            "same text, different origin path — not drift"
        );

        let changed = parse_with_origin(
            "coordinator cycle_rate=200.0\nsystem \"a\" type=\"Src\"\n",
            Some("/build/mission.kdl"),
        )
        .unwrap();
        assert_ne!(
            normalized_ir(&frozen),
            normalized_ir(&changed),
            "a real emission change is drift"
        );
    }

    /// Provenance discovery prefers `mission.py`, then `mission.kdl`, and is
    /// `None` when a bundle carries neither.
    #[test]
    fn find_provenance_prefers_python() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_provenance(dir.path()).is_none(), "no provenance yet");
        std::fs::write(dir.path().join("mission.kdl"), "").unwrap();
        assert_eq!(find_provenance(dir.path()), Some(dir.path().join("mission.kdl")));
        std::fs::write(dir.path().join("mission.py"), "").unwrap();
        assert_eq!(find_provenance(dir.path()), Some(dir.path().join("mission.py")));
    }
}
