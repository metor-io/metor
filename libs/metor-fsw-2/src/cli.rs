use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use miette::IntoDiagnostic;

use crate::wiring::{
    BuildOptions, ClockSpec, PackageOptions, Registry, Wiring, build_artifacts,
    eval_python_mission, is_python_mission, load_bundle, parse_with_origin, resolve, write_bundle,
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
    /// The wiring KDL file.
    kdl: PathBuf,
    /// The bundle output directory (created if absent; conventionally `*.bundle`).
    #[arg(short = 'o', long = "out", value_name = "DIR")]
    out: PathBuf,
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

pub async fn run() -> miette::Result<()> {
    // Route a re-executed worker child before anything else (process
    // systems; a no-op read of one env var otherwise).
    crate::proc::worker_entry();
    let cli = Cli::parse();
    match cli.command {
        Command::Build(a) => cmd_build(a),
        Command::Package(a) => cmd_package(a),
        Command::Run(a) => cmd_run(a).await,
    }
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
    // A bundle carries its mission as verbatim KDL re-parsed on load, so a `.py`
    // mission cannot round-trip through it. The IR-carrying bundle is Phase 3;
    // until then, evaluate the mission with `build`/`run` instead.
    if is_python_mission(&args.kdl) {
        return Err(miette::miette!(
            "packaging a `.py` mission is not supported yet (the bundle manifest is \
             verbatim KDL); use `metor-fsw build`/`run` on the `.py`, or package a `.kdl`"
        ));
    }
    let text = read_file(&args.kdl)?;
    let mut wiring = parse_with_origin(&text, Some(&args.kdl.to_string_lossy()))?;
    build_artifacts(
        &mut wiring,
        &build_opts(args.release, &args.cargo_arg, args.no_manifest_sidecar),
    )
    .into_diagnostic()?;
    write_bundle(
        &wiring,
        &text,
        &PackageOptions {
            release: args.release,
        },
        &args.out,
    )
    .into_diagnostic()?;
    println!(
        "packaged {} artifacts, {} systems → {}",
        wiring.artifacts.len(),
        wiring.systems.len(),
        args.out.display()
    );
    Ok(())
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

/// A `<TARGET>` is a bundle if it is a directory (the bundle layout) or ends in `.bundle`.
fn is_bundle(path: &Path) -> bool {
    path.is_dir() || path.extension().is_some_and(|e| e == "bundle")
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
