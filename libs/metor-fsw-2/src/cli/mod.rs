//! The `metor-fsw` CLI runner (cli-runner.md) — a thin clap front-end over the wiring
//! surface: `parse` → (`build_artifacts`) → override → `resolve` → `Coordinator::run_for`.
//!
//! Three separable operations, plus a build→run shortcut:
//!
//! - **`build <KDL>`** — compile the `cdylib`s a wiring references (`build_artifacts`).
//! - **`package <KDL> -o <DIR>`** — produce a relocatable bundle directory ([`write_bundle`]).
//! - **`run <TARGET>`** — load a wiring (a source `.kdl` with `--build`, or a bundle dir) and
//!   drive the coordinator. Clock/telemetry knobs are flags that override the KDL.
//!
//! The `metor-fsw` binary is `fn main() { metor_fsw_2::cli::main() }`; a mission host (the
//! `adcs-fsw2` example) delegates to the same [`main`]. Only [`run`](cmd_run) enters the
//! `stellarator` runtime, at the leaf — `build`/`package` are fully synchronous.
//!
//! The generic runner resolves against an **empty** [`Registry`](crate::wiring::Registry):
//! it loads `dlopen`'d (`cdylib`) systems only, since a single prebuilt binary cannot link an
//! arbitrary mission's statically-linked systems (a static mission keeps its own host).

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use miette::IntoDiagnostic;

use crate::wiring::{
    BuildOptions, ClockSpec, PackageOptions, Registry, TelemetryModeSpec, TelemetrySpec, Wiring,
    build_artifacts, load_bundle, parse, resolve, write_bundle,
};

// ---------------------------------------------------------------------------
// clap command tree
// ---------------------------------------------------------------------------

/// `metor-fsw` — build, package, and run metor-fsw missions.
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
    /// Produce a relocatable bundle directory (the cdylibs + a manifest).
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
}

#[derive(Args, Debug)]
struct RunArgs {
    /// A source `.kdl` file (requires `--build`) or a bundle directory (cargo-free).
    target: PathBuf,
    /// Build the cdylibs first — the build→run shortcut; required for a source `.kdl`.
    #[arg(long)]
    build: bool,
    /// Build the `--release` profile when `--build` is set.
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable), with `--build`.
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
    /// Use a paced wall clock (override the KDL's clock).
    #[arg(long, group = "clock")]
    wall: bool,
    /// Use a free-running simulated clock with this per-cycle step, in seconds
    /// (override the KDL's clock).
    #[arg(long, value_name = "SECS", group = "clock")]
    sim_dt: Option<f64>,
    /// Override the coordinator cycle rate (Hz).
    #[arg(long, value_name = "HZ")]
    cycle_rate: Option<f64>,
    /// Enable the telemetry downlink to this TCP address (override the KDL).
    #[arg(long, value_name = "ADDR", group = "telem")]
    telemetry: Option<SocketAddr>,
    /// Disable telemetry even if the KDL declares it.
    #[arg(long, group = "telem")]
    no_telemetry: bool,
    /// Tap mode for `--telemetry` (v1: `all`).
    #[arg(long, value_name = "MODE", default_value = "all")]
    telemetry_mode: String,
    /// Run this many cycles, then stop (default: run until interrupted).
    #[arg(long, value_name = "N")]
    cycles: Option<usize>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Binary entry: parse argv, dispatch, render any error to stderr, set the exit code.
/// `metor-fsw`'s `main` and a mission host's `main` are both one line: `cli::main()`.
pub fn main() {
    if let Err(report) = run(std::env::args_os()) {
        // miette's Debug rendering draws the span-carrying diagnostic (the "fancy" feature).
        eprintln!("{report:?}");
        std::process::exit(1);
    }
}

/// Testable entry: parse `args`, dispatch a command. Returns a [`miette::Result`] so the
/// command logic is drivable without spawning a process. A clap parse failure (or
/// `--help`/`--version`) is rendered and the process exits with clap's own code — the
/// command handlers below are what this `Result` reports.
pub fn run<I, T>(args: I) -> miette::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).unwrap_or_else(|e| e.exit());
    match cli.command {
        Command::Build(a) => cmd_build(a),
        Command::Package(a) => cmd_package(a),
        Command::Run(a) => cmd_run(a),
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// `build` — parse the wiring, compile + locate every artifact's `.so`, print them.
fn cmd_build(args: BuildArgs) -> miette::Result<()> {
    let text = read_file(&args.kdl)?;
    let mut wiring = parse(&text)?;
    build_artifacts(&mut wiring, &build_opts(args.release, &args.cargo_arg)).into_diagnostic()?;
    for a in &wiring.artifacts {
        let path = a.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
        println!("  {:<28} →  {path}", a.crate_name);
    }
    Ok(())
}

/// `package` — compile + locate the artifacts (incremental), then write the relocatable
/// bundle. The build driver is also the locator, so this is always self-sufficient: an
/// up-to-date tree only relocates the `.so`s, it does not recompile.
fn cmd_package(args: PackageArgs) -> miette::Result<()> {
    let text = read_file(&args.kdl)?;
    let mut wiring = parse(&text)?;
    build_artifacts(&mut wiring, &build_opts(args.release, &args.cargo_arg)).into_diagnostic()?;
    write_bundle(&wiring, &text, &PackageOptions { release: args.release }, &args.out)
        .into_diagnostic()?;
    println!(
        "packaged {} artifacts, {} systems → {}",
        wiring.artifacts.len(),
        wiring.systems.len(),
        args.out.display()
    );
    Ok(())
}

/// `run` — load the wiring (bundle or source-KDL+`--build`), apply CLI overrides, resolve
/// against the empty (dl-only) registry, and drive the coordinator on the runtime.
fn cmd_run(args: RunArgs) -> miette::Result<()> {
    let mut wiring = load_run_wiring(&args)?;
    apply_overrides(&mut wiring, &args)?;
    let mut coord = resolve(&wiring, &Registry::new())?;
    let cycles = args.cycles.unwrap_or(usize::MAX);
    // Enter the async runtime at the leaf; `run_for` does init → cycle loop → shutdown.
    stellarator::run(move || async move {
        coord.run_for(cycles).await;
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve `run`'s `<TARGET>` into a located [`Wiring`]: a bundle directory loads
/// cargo-free ([`load_bundle`]); a source `.kdl` requires `--build` (the only artifact
/// locator is the cargo build driver, since `.so` paths are not persisted between a
/// `build` and a later `run`).
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
    let text = read_file(&args.target)?;
    let mut wiring = parse(&text)?;
    build_artifacts(&mut wiring, &build_opts(args.release, &args.cargo_arg)).into_diagnostic()?;
    Ok(wiring)
}

/// A `<TARGET>` is a bundle if it is a directory (the bundle layout) or ends in `.bundle`.
fn is_bundle(path: &Path) -> bool {
    path.is_dir() || path.extension().is_some_and(|e| e == "bundle")
}

/// Apply `run`'s override flags onto the loaded [`Wiring`], before `resolve` — the
/// generalized `build_live_coordinator` mutation (cli-runner.md §7). Flag beats KDL.
fn apply_overrides(wiring: &mut Wiring, args: &RunArgs) -> miette::Result<()> {
    if args.wall {
        wiring.coordinator.clock = ClockSpec::Wall;
    } else if let Some(dt_secs) = args.sim_dt {
        wiring.coordinator.clock = ClockSpec::Simulated { dt_secs };
    }
    if let Some(rate) = args.cycle_rate {
        wiring.coordinator.cycle_rate = rate;
    }
    if args.no_telemetry {
        wiring.telemetry = None;
    } else if let Some(addr) = args.telemetry {
        let mode = match args.telemetry_mode.as_str() {
            "all" => TelemetryModeSpec::All,
            other => {
                return Err(miette::miette!(
                    "unknown --telemetry-mode `{other}` (v1 supports `all`; declare a `subset` \
                     tap list in the KDL)"
                ));
            }
        };
        wiring.telemetry = Some(TelemetrySpec { addr, mode });
    }
    Ok(())
}

/// Build the [`BuildOptions`] from the shared `--release`/`--cargo-arg` flags.
fn build_opts(release: bool, cargo_arg: &[String]) -> BuildOptions {
    BuildOptions {
        release,
        extra_args: cargo_arg.to_vec(),
    }
}

/// Read a file to a string, mapping I/O errors to a clean diagnostic.
fn read_file(path: &Path) -> miette::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| miette::miette!("failed to read `{}`: {e}", path.display()))
}

#[cfg(test)]
mod tests;
