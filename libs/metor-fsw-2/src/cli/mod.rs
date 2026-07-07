//! The `metor-fsw` command line: a thin clap front end over the wiring surface.
//! Every command runs the same pipeline. It parses the wiring KDL, optionally
//! compiles the `cdylib` artifacts, applies any flag overrides, and either stops
//! there or resolves the wiring into a coordinator and runs it.
//!
//! Three subcommands, plus a build-then-run shortcut:
//!
//! - **`build <KDL>`** compiles the `cdylib`s the wiring references and prints
//!   where they landed.
//! - **`package <KDL> -o <DIR>`** produces a relocatable bundle directory
//!   ([`write_bundle`]).
//! - **`run <TARGET>`** loads a wiring (a source `.kdl` with `--build`, or a
//!   bundle directory) and drives the coordinator. Clock and link flags
//!   override whatever the KDL declares.
//!
//! A binary embeds the whole CLI by calling [`main`] from its own `fn main`.
//! Only `run` enters the `stellarator` runtime, at the leaf; `build` and
//! `package` are fully synchronous.
//!
//! The runner resolves against [`Registry::with_builtins`], so a wiring may use
//! the framework's static built-in systems (for example `type="Alarms"`) plus
//! any number of `dlopen`'d `cdylib` systems. It cannot host a mission's own
//! statically linked systems, because a prebuilt binary has nothing to link
//! them into; such a mission keeps its own host binary, seeded from the same
//! built-ins.

use std::ffi::OsString;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use miette::IntoDiagnostic;

use crate::wiring::{
    BuildOptions, ClockSpec, PackageOptions, Registry, SystemSpec, TCP_DOWNLINK_TYPE,
    TCP_UPLINK_TYPE, Wiring, build_artifacts, load_bundle, parse, resolve, write_bundle,
};

// ---------------------------------------------------------------------------
// clap command tree
// ---------------------------------------------------------------------------

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
    /// Build the cdylibs first; required when the target is a source `.kdl`.
    #[arg(long)]
    build: bool,
    /// Build the `--release` profile when `--build` is set.
    #[arg(long)]
    release: bool,
    /// An extra arg appended to every `cargo build` (repeatable), with `--build`.
    #[arg(long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    cargo_arg: Vec<String>,
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
    /// Enable the telemetry downlink to this TCP address, overriding the KDL.
    #[arg(long, value_name = "ADDR", group = "telem")]
    telemetry: Option<SocketAddr>,
    /// Disable telemetry even if the KDL declares it.
    #[arg(long, group = "telem")]
    no_telemetry: bool,
    /// Tap mode for `--telemetry`; only `all` is supported.
    #[arg(long, value_name = "MODE", default_value = "all")]
    telemetry_mode: String,
    /// Enable the command uplink, reading `SequenceCommand`s from its own TCP
    /// connection to this address (separate from `--telemetry`'s connection).
    #[arg(long, value_name = "ADDR")]
    uplink: Option<SocketAddr>,
    /// Run this many cycles, then stop (default: run until interrupted).
    #[arg(long, value_name = "N")]
    cycles: Option<usize>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Binary entry point. Parses argv, dispatches, renders any error to stderr,
/// and sets the exit code. A host binary's `main` is one line, `cli::main()`.
pub fn main() {
    if let Err(report) = run(std::env::args_os()) {
        // miette's Debug rendering draws the span-carrying diagnostic (the "fancy" feature).
        eprintln!("{report:?}");
        std::process::exit(1);
    }
}

/// Testable entry point. Parses `args` and dispatches, returning a
/// [`miette::Result`] so command logic is drivable without spawning a process.
/// A clap parse failure (or `--help`/`--version`) is rendered by clap and exits
/// the process with clap's own code; this `Result` reports only what the
/// command handlers below return.
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

/// `build`: parse the wiring, compile and locate every artifact's `.so`, print them.
fn cmd_build(args: BuildArgs) -> miette::Result<()> {
    let text = read_file(&args.kdl)?;
    let mut wiring = parse(&text)?;
    build_artifacts(&mut wiring, &build_opts(args.release, &args.cargo_arg)).into_diagnostic()?;
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

/// `package`: compile and locate the artifacts, then write the relocatable
/// bundle. The build driver doubles as the locator, so an already up-to-date
/// tree only relocates the `.so`s without recompiling.
fn cmd_package(args: PackageArgs) -> miette::Result<()> {
    let text = read_file(&args.kdl)?;
    let mut wiring = parse(&text)?;
    build_artifacts(&mut wiring, &build_opts(args.release, &args.cargo_arg)).into_diagnostic()?;
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

/// `run`: load the wiring, apply flag overrides, resolve against
/// [`Registry::with_builtins`], and drive the coordinator on the runtime.
/// Prints a config banner ([`print_run_banner`]) up front and a live progress
/// [`heartbeat`] while the mission runs.
fn cmd_run(args: RunArgs) -> miette::Result<()> {
    let mut wiring = load_run_wiring(&args)?;
    apply_overrides(&mut wiring, &args)?;
    print_run_banner(&args.target, &wiring);

    let cycles = args.cycles.unwrap_or(usize::MAX);
    let total = args.cycles;
    let mut coord = resolve(&wiring, &Registry::with_builtins())?;
    let progress = coord.progress();

    // Enter the async runtime at the leaf; `run_for` does init, the cycle loop,
    // and shutdown. The heartbeat is a side task reading the shared progress
    // counter, since the loop holds `&mut coord` for its whole life.
    stellarator::run(move || async move {
        let _beat = stellarator::spawn(heartbeat(progress, total)).drop_guard();
        coord.run_for(cycles).await;
        let done = coord.progress().load(Relaxed);
        let stopped = coord.stopped();
        if stopped.is_empty() {
            println!("✓ mission complete — {done} cycles.");
        } else {
            let names: Vec<&str> = stopped.iter().map(|s| s.name).collect();
            println!(
                "✗ mission stopped after {done} cycles — {} system(s) hard-stopped: {}",
                stopped.len(),
                names.join(", ")
            );
        }
    });
    Ok(())
}

/// Print what `run` is about to do: the source, the systems, the active clock,
/// the link endpoints (with a reachability probe), and the duration. Runs
/// synchronously before the runtime starts.
fn print_run_banner(target: &Path, wiring: &Wiring) {
    let names: Vec<&str> = wiring.systems.iter().map(|s| s.name.as_str()).collect();
    let kind = if target.is_dir() { "bundle" } else { "mission" };
    println!("metor-fsw — running {kind} {}", target.display());
    println!(
        "  systems:   {} ({})",
        wiring.systems.len(),
        names.join(", ")
    );
    if !wiring.slots.is_empty() {
        // Each slot is a runtime-loadable position; show its initial occupant
        // (or "empty").
        let slots: Vec<String> = wiring
            .slots
            .iter()
            .map(|s| {
                let occ = s
                    .initial
                    .as_ref()
                    .map(|i| i.occupant.as_str())
                    .unwrap_or("empty");
                format!("{}[{occ}]", s.name)
            })
            .collect();
        println!("  slots:     {} ({})", wiring.slots.len(), slots.join(", "));
    }

    match wiring.coordinator.clock {
        ClockSpec::Wall => println!(
            "  clock:     wall, {:.0} Hz (paced, real time)",
            wiring.coordinator.cycle_rate
        ),
        ClockSpec::Simulated { dt_secs } => {
            println!("  clock:     simulated, dt={dt_secs}s (free-running, as fast as possible)")
        }
    }

    // The links are ordinary systems, so the banner scans for the built-in TCP
    // types and prints one line per instance (several downlinks are legal).
    let downlinks: Vec<&SystemSpec> = specs_of_type(wiring, TCP_DOWNLINK_TYPE);
    if downlinks.is_empty() {
        println!("  telemetry: off — pass `--telemetry <addr>` to stream to metor-panel");
    }
    for spec in &downlinks {
        match spec_addr(spec) {
            Some(addr) => {
                println!("  telemetry: {addr} ({}) {}", spec.name, reach_label(addr));
            }
            None => println!("  telemetry: {} (unparsable addr)", spec.name),
        }
    }
    if !downlinks.is_empty() && matches!(wiring.coordinator.clock, ClockSpec::Simulated { .. }) {
        // The sim clock free-runs, so a live downlink would race far ahead of real time.
        println!(
            "  note:      simulated clock free-runs; pass `--wall` for real-time panel viewing"
        );
    }

    let uplinks = specs_of_type(wiring, TCP_UPLINK_TYPE);
    if uplinks.is_empty() {
        println!("  uplink:    off — pass `--uplink <addr>` to receive panel commands");
    }
    for spec in &uplinks {
        match spec_addr(spec) {
            Some(addr) => println!("  uplink:    {addr} ({}) {}", spec.name, reach_label(addr)),
            None => println!("  uplink:    {} (unparsable addr)", spec.name),
        }
    }
    println!();
}

/// A one-shot, best-effort TCP reachability check, so the banner can warn when
/// nothing is listening at a link endpoint yet (the downlink connects once and
/// does not retry). The probe connection is transient and never fails the run.
fn probe_telemetry(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// The banner's reachability suffix for one probed endpoint.
fn reach_label(addr: SocketAddr) -> &'static str {
    if probe_telemetry(addr) {
        "✓ reachable"
    } else {
        "⚠ not reachable — start metor-panel BEFORE the mission (no auto-reconnect in v1)"
    }
}

/// The specs of one built-in TCP link type, in document order.
fn specs_of_type<'w>(wiring: &'w Wiring, ty: &str) -> Vec<&'w SystemSpec> {
    wiring
        .systems
        .iter()
        .filter(|s| s.ty.as_deref() == Some(ty))
        .collect()
}

/// Pull the `addr=` property back out of a built-in link spec's params node,
/// the same node text [`resolve`] re-decodes. Banner display only, never
/// load-bearing.
fn spec_addr(spec: &SystemSpec) -> Option<SocketAddr> {
    let crate::wiring::ParamSource::Kdl(text) = &spec.params else {
        return None;
    };
    let doc: kdl::KdlDocument = text.parse().ok()?;
    doc.nodes()
        .first()?
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some("addr"))
        .and_then(|e| e.value().as_string())
        .and_then(|s| s.parse().ok())
}

/// Print the live cycle counter every couple of seconds so a long run visibly
/// advances. Cancelled when its `drop_guard` drops at the end of [`cmd_run`].
async fn heartbeat(progress: Arc<AtomicU64>, total: Option<usize>) {
    let start = Instant::now();
    loop {
        stellarator::sleep(Duration::from_secs(2)).await;
        let n = progress.load(Relaxed);
        let secs = start.elapsed().as_secs_f64();
        match total {
            Some(t) => println!("  … cycle {n}/{t}  ({secs:.0}s elapsed)"),
            None => println!("  … cycle {n}  ({secs:.0}s elapsed)"),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
    let text = read_file(&args.target)?;
    let mut wiring = parse(&text)?;
    build_artifacts(&mut wiring, &build_opts(args.release, &args.cargo_arg)).into_diagnostic()?;
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
    // The link flags add and remove instances of the built-in TCP types, keyed
    // on the type rather than the instance name, so a user's custom downlink
    // system is untouched.
    if args.no_telemetry {
        wiring
            .systems
            .retain(|s| s.ty.as_deref() != Some(TCP_DOWNLINK_TYPE));
    } else if let Some(addr) = args.telemetry {
        if args.telemetry_mode != "all" {
            return Err(miette::miette!(
                "unknown --telemetry-mode `{}` (v1 supports `all`; declare `instances`/`frames` \
                 subset children on a `system` node in the KDL)",
                args.telemetry_mode
            ));
        }
        wiring
            .systems
            .retain(|s| s.ty.as_deref() != Some(TCP_DOWNLINK_TYPE));
        wiring
            .systems
            .push(SystemSpec::tcp_downlink("telemetry", addr));
    }
    if let Some(addr) = args.uplink {
        // Patch `addr=` on the declared uplink(s) in place. Replacing the node
        // would silently drop its `msgs` config, leaving an uplink that relays
        // nothing. Only a mission with no uplink at all gets a fresh instance.
        let mut found = false;
        for spec in wiring
            .systems
            .iter_mut()
            .filter(|s| s.ty.as_deref() == Some(TCP_UPLINK_TYPE))
        {
            patch_spec_addr(spec, addr)?;
            found = true;
        }
        if !found {
            wiring.systems.push(SystemSpec::tcp_uplink("uplink", addr));
        }
    }
    Ok(())
}

/// Upsert the `addr=` property on a built-in link spec's params node,
/// preserving every other param (notably the uplink's `msgs` children); the
/// write twin of [`spec_addr`]. A param-less spec synthesizes the minimal node
/// [`SystemSpec::tcp_uplink`] would have produced.
fn patch_spec_addr(spec: &mut SystemSpec, addr: SocketAddr) -> miette::Result<()> {
    use crate::wiring::ParamSource;
    match &mut spec.params {
        ParamSource::Kdl(text) => {
            let mut doc: kdl::KdlDocument = text
                .parse()
                .map_err(|e| miette::miette!("invalid params node on `{}`: {e}", spec.name))?;
            let node = doc
                .nodes_mut()
                .first_mut()
                .ok_or_else(|| miette::miette!("empty params node on `{}`", spec.name))?;
            node.insert("addr", addr.to_string());
            *text = doc.to_string();
        }
        ParamSource::None => {
            let ty = spec.ty.as_deref().unwrap_or(TCP_UPLINK_TYPE);
            spec.params = ParamSource::Kdl(format!(
                "system \"{}\" type=\"{ty}\" addr=\"{addr}\"",
                spec.name
            ));
        }
        // Typed builder params are postcard bytes with no KDL to patch.
        ParamSource::Postcard(_) => {
            return Err(miette::miette!(
                "cannot override `addr` on `{}`: it carries typed (postcard) params",
                spec.name
            ));
        }
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
