use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use miette::IntoDiagnostic;

use crate::ir::IR_VERSION;
use crate::wiring::{
    BuildOptions, ClockSpec, Deployment, LoadError, METOR_EXTENSION, PackBuildOptions,
    PackDevOptions, PackageOptions, Registry, WIRING_FILE_NAME, Wiring, build_target,
    eval_python_deployment, load_bundle, locate_artifacts, pack_build, pack_dev,
    provision_artifacts, refresh_dev_packs, resolve, unpack_metor, validate_deployment,
    write_bundle,
};

mod launch;
mod ui;

/// The fully parsed command line, produced from argv by [`run`].
#[derive(Parser, Debug)]
#[command(
    name = "metor-fsw",
    version,
    about = "Build, package, and run metor-fsw targets",
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
    /// Run a target: a source `.py` (built automatically) or a bundle dir.
    Run(RunArgs),
    /// Pack-crate commands.
    #[command(subcommand)]
    Pack(PackCmd),
}

#[derive(Subcommand, Debug)]
enum PackCmd {
    /// Build the host triple and lay out the pack's editable `.metor/`
    /// payload (typed module + `_libs/<triple>/`); what a pack's PEP 517
    /// backend runs on `uv sync`.
    Dev(PackDevArgs),
    /// Build the pack's `py3-none-any` wheel for the host triple.
    Build(PackBuildArgs),
}

#[derive(Args, Debug)]
struct PackBuildArgs {
    /// The pack crate directory (holds `pyproject.toml` + `Cargo.toml`).
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Write the wheel here (default: `<dir>/dist`).
    #[arg(long, value_name = "DIR")]
    wheel_out: Option<PathBuf>,
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
    /// The `.py` deployment file.
    path: PathBuf,
    /// Build the `--release` profile (default: debug).
    #[arg(long)]
    release: bool,
    /// Build only this member of the deployment, by namespace. Every member
    /// is built, in envelope order, when it is omitted.
    #[arg(long = "target", value_name = "NS")]
    target: Option<String>,
    /// Provision for this target triple: prebuilt artifacts select their
    /// `<triple>/` payload, crate artifacts cross-compile.
    #[arg(long = "triple", value_name = "TRIPLE")]
    triple: Option<String>,
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
    /// The `.py` deployment source. Required unless `--check-ir`.
    path: Option<PathBuf>,
    /// Package this member of the deployment, by namespace. Required when
    /// the deployment has more than one member.
    #[arg(long = "target", value_name = "NS")]
    target: Option<String>,
    /// The bundle output: a directory (conventionally `*.bundle`), or a
    /// single-file `*.metor` archive (dispatched by the `.metor` extension).
    #[arg(short = 'o', long = "out", value_name = "OUT")]
    out: Option<PathBuf>,
    /// Package for this target triple: prebuilt artifacts select their
    /// `<triple>/` payload (no cargo), crate artifacts cross-compile, and the
    /// bundle records the triple. Defaults to the host.
    #[arg(long = "triple", value_name = "TRIPLE")]
    triple: Option<String>,
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
    /// One source `.py` deployment (built automatically), or one or more
    /// bundles (cargo-free). Defaults to `target.py` in the current directory.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Run this member of the deployment, by namespace. With several members
    /// and no `--target`, every member runs in its own process with its output
    /// prefixed by its namespace; on a bundle it must match the frozen
    /// namespace.
    #[arg(long = "target", value_name = "NS")]
    target: Option<String>,
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
    /// Use a paced wall clock, overriding the target's clock.
    #[arg(long, group = "clock")]
    wall: bool,
    /// Use a free-running simulated clock with this per-cycle step in seconds,
    /// overriding the target's clock.
    #[arg(long, value_name = "SECS", group = "clock")]
    sim_dt: Option<f64>,
    /// Override the coordinator cycle rate (Hz).
    #[arg(long, value_name = "HZ")]
    cycle_rate: Option<f64>,
    /// Run this many cycles, then stop (default: run until interrupted).
    #[arg(long, value_name = "N")]
    cycles: Option<usize>,
    /// Skip the pre-flight listing.
    #[arg(long)]
    no_preflight: bool,
    /// Serve the telemetry link on this address, overriding the target's
    /// `TcpServer` state (or declaring one, with an all-taps downlink, when
    /// the target has none). Applies to one member; with several, requires
    /// `--target`.
    #[arg(long, value_name = "ADDR")]
    serve: Option<std::net::SocketAddr>,
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
        Command::Pack(PackCmd::Dev(a)) => cmd_pack_dev(a),
        Command::Pack(PackCmd::Build(a)) => cmd_pack_build(a),
    }
}

/// `pack build`: the host-triple payload and its wheel.
fn cmd_pack_build(args: PackBuildArgs) -> miette::Result<()> {
    let report = pack_build(
        &args.dir,
        &PackBuildOptions {
            wheel_out: args.wheel_out,
        },
    )
    .into_diagnostic()?;
    println!("  {} ({})", report.wheel.display(), report.triple);
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

/// `--triple` is sugar for the `--cargo-arg --target …` spelling: one flag
/// drives prebuilt selection, crate cross-builds, and the recorded triple.
fn merge_target(cargo_args: &[String], target: Option<&str>) -> Vec<String> {
    let mut merged = cargo_args.to_vec();
    if let Some(triple) = target {
        merged.extend(["--target".to_string(), triple.to_string()]);
    }
    merged
}

/// Load a source target file into a [`Deployment`]. Targets are Python: a
/// `.py` file is evaluated by a subprocess CPython; any other extension is
/// unrecognized.
fn load_source(path: &Path) -> miette::Result<Deployment> {
    if path.extension().is_some_and(|e| e == "py") {
        return eval_python_deployment(path);
    }
    Err(miette::miette!(
        "unrecognized target `{}`; targets are Python (`.py`)",
        path.display()
    ))
}

/// Pick the deployment member `--target` names. The one call into
/// [`Deployment::target`]; it renders the selection faults against the file
/// they came from, which the `LoadError` itself does not know.
fn select<'a>(
    deployment: &'a Deployment,
    path: &Path,
    namespace: Option<&str>,
) -> miette::Result<&'a Wiring> {
    let file = file_label(path);
    deployment
        .target(namespace)
        .map_err(|err| deployment_fault(&file, err))
}

/// Render an envelope fault against the input it came from: a target file's
/// name, or `bundles` for a set of them.
fn deployment_fault(label: &str, err: LoadError) -> miette::Report {
    match err {
        LoadError::TargetRequired { available } => miette::miette!(
            "deployment `{label}` has {} targets; pick one with --target ({})",
            available.len(),
            available.join(", ")
        ),
        LoadError::UnknownTarget {
            requested,
            available,
        } if available.is_empty() => miette::miette!(
            "target `{label}` has no namespace; `--target {requested}` does not apply"
        ),
        LoadError::UnknownTarget {
            requested,
            available,
        } => miette::miette!(
            "deployment `{label}` has no target `{requested}`; targets: {}",
            available.join(", ")
        ),
        other => miette::miette!("deployment `{label}`: {other}"),
    }
}

/// How a target file names itself in a diagnostic: its file name, falling
/// back to the whole path.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Refresh a source target's dev packs, the path-source pack dependencies
/// its pyproject names, so the generated modules (manifest hashes, params)
/// the target imports are current before it is evaluated. Prebuilt pack
/// artifacts are only *selected* at provisioning, so this is where their
/// sources get rebuilt; cargo's incremental build makes a clean tree a no-op.
fn refresh_source_packs(target: &Path, release: bool, cargo_args: &[String]) -> miette::Result<()> {
    let dir = match target.parent() {
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
    .into_diagnostic()
    .map(|_| ())
}

/// `build`: load the deployment, provision every artifact's `.so`, print
/// them. With no `--target` every member is built, in envelope order.
fn cmd_build(args: BuildArgs) -> miette::Result<()> {
    let cargo_args = merge_target(&args.cargo_arg, args.triple.as_deref());
    refresh_source_packs(&args.path, args.release, &cargo_args)?;
    let deployment = load_source(&args.path)?;
    let mut members = match &args.target {
        Some(ns) => vec![select(&deployment, &args.path, Some(ns))?.clone()],
        None => deployment.targets.clone(),
    };
    let opts = build_opts(args.release, &cargo_args, args.no_manifest_sidecar);
    for member in &mut members {
        provision_artifacts(member, &opts).into_diagnostic()?;
        ui::print_build_member(member);
    }
    Ok(())
}

fn cmd_package(args: PackageArgs) -> miette::Result<()> {
    if let Some(bundle) = &args.check_ir {
        return cmd_check_ir(bundle);
    }
    // The bundle freezes the evaluated `Wiring` as IR, so the packaged target
    // runs with no Python and no config parse on target.
    let source = args.path.as_deref().ok_or_else(|| {
        miette::miette!("`package` needs a target source (`.py`), or `--check-ir <bundle>`")
    })?;
    let out = args
        .out
        .as_deref()
        .ok_or_else(|| miette::miette!("`package` needs an output path (`-o <out>`)"))?;
    let cargo_args = merge_target(&args.cargo_arg, args.triple.as_deref());
    refresh_source_packs(source, args.release, &cargo_args)?;
    let mut wiring = select(&load_source(source)?, source, args.target.as_deref())?.clone();
    provision_artifacts(
        &mut wiring,
        &build_opts(args.release, &cargo_args, args.no_manifest_sidecar),
    )
    .into_diagnostic()?;
    let opts = PackageOptions {
        release: args.release,
        target: build_target(&cargo_args),
        provenance: Some(source.to_path_buf()),
        // Current time; a reproducible build pins this.
        built_at_unix: None,
    };
    write_bundle(&wiring, &opts, out).into_diagnostic()?;
    let member = wiring
        .coordinator
        .namespace
        .as_deref()
        .map(|ns| format!("target `{ns}`, "))
        .unwrap_or_default();
    println!(
        "packaged {member}{} artifacts, {} systems → {}",
        wiring.artifacts.len(),
        wiring.systems.len(),
        out.display()
    );
    Ok(())
}

/// `package --check-ir <bundle>`: re-evaluate the bundle's provenance source
/// and diff the produced IR against the frozen `wiring.json`, exiting non-zero
/// on any drift: the determinism backstop, runnable in CI.
///
/// Both sides are normalized before the diff: artifact `path`s are stripped
/// (never in the frozen IR anyway) and `src` file names cleared, since the
/// provenance copy sits at a different path than the original source, which
/// would otherwise read as spurious drift. The line/column of every anchor is
/// kept, so a genuine emission change is still caught.
///
/// The provenance copy is the whole deployment file, so the member to diff is
/// the one whose namespace the frozen `wiring.json` carries.
fn cmd_check_ir(bundle: &Path) -> miette::Result<()> {
    let unpacked =
        if bundle.is_file() && bundle.extension().is_some_and(|ext| ext == METOR_EXTENSION) {
            Some(unpack_metor(bundle).into_diagnostic()?)
        } else {
            None
        };
    let dir = unpacked.as_ref().map_or(bundle, |temp| temp.path());

    let frozen_text = read_file(&dir.join(WIRING_FILE_NAME))?;
    let frozen: Wiring = serde_json::from_str(&frozen_text).map_err(|e| {
        miette::miette!(
            "bundle `{}` has an unreadable wiring.json: {e}",
            bundle.display()
        )
    })?;

    let source = find_provenance(dir).ok_or_else(|| {
        miette::miette!(
            "bundle `{}` carries no provenance source (target.py); cannot --check-ir",
            bundle.display()
        )
    })?;
    let produced = select(
        &load_source(&source)?,
        &source,
        frozen.coordinator.namespace.as_deref(),
    )?
    .clone();

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
    let p = dir.join("target.py");
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
    let paths = match args.paths.as_slice() {
        [] => vec![detect_target()?],
        given => given.to_vec(),
    };
    let input = classify(&paths)?;
    // A bundle set is named as a set; one path names itself, as it always has.
    let label = match paths.as_slice() {
        [one] => one.clone(),
        _ => PathBuf::from("bundles"),
    };

    let deployment = match &input {
        Input::Source(source) => {
            refresh_run_packs(source, &args)?;
            load_source(source)?
        }
        Input::Bundles(bundles) => {
            let mut targets = Vec::new();
            for bundle in bundles {
                targets.push(load_bundle(bundle).into_diagnostic()?);
            }
            let deployment = Deployment {
                ir_version: IR_VERSION,
                targets,
                hosts: Default::default(),
            };
            validate_deployment(&deployment).map_err(|err| match err {
                LoadError::NamespaceRequired { index } => miette::miette!(
                    "bundles: member {index} (`{}`) has no namespace; every member of a \
                     deployment needs one",
                    file_label(&bundles[index])
                ),
                other => deployment_fault("bundles", other),
            })?;
            deployment
        }
    };

    let mut members = match args.target.as_deref() {
        Some(ns) => vec![select(&deployment, &label, Some(ns))?.clone()],
        None => deployment.targets.clone(),
    };
    check_serve(&members, &args)?;

    let [member] = members.as_mut_slice() else {
        return run_members(&mut members, &input, &args);
    };
    apply_overrides(member, &args)?;
    if !args.no_preflight {
        ui::print_preflight(member, &label);
    }
    if let Input::Source(source) = &input {
        provision_run_artifacts(member, source, &args)?;
    }

    let cycles = args.cycles.unwrap_or(usize::MAX);
    let mut coord = resolve(member, &Registry::with_builtins())?;

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

/// What `run`'s positional paths name. The two inputs differ in one place
/// only: a source is built and written to temporary bundles, a bundle set is
/// already the hand-off.
#[derive(Debug)]
enum Input {
    Source(PathBuf),
    Bundles(Vec<PathBuf>),
}

/// Sort `run`'s paths into one source target or a set of bundles.
fn classify(paths: &[PathBuf]) -> miette::Result<Input> {
    match paths {
        [one] if !is_bundle(one) => Ok(Input::Source(one.clone())),
        _ if paths.iter().all(|p| is_bundle(p)) => Ok(Input::Bundles(paths.to_vec())),
        _ => Err(miette::miette!(
            "`run` takes one source `.py` or bundles, not both: {}",
            paths
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `--serve` names one socket, so it names one member.
fn check_serve(members: &[Wiring], args: &RunArgs) -> miette::Result<()> {
    if args.serve.is_none() || members.len() < 2 {
        return Ok(());
    }
    Err(miette::miette!(
        "`--serve` names one socket; pick the member it applies to with --target ({})",
        namespaces(members).join(", ")
    ))
}

/// Every member's namespace, in envelope order. A validated deployment of
/// several names each of its members.
fn namespaces(members: &[Wiring]) -> Vec<&str> {
    members
        .iter()
        .map(|m| {
            m.coordinator
                .namespace
                .as_deref()
                .expect("a validated deployment of several names every member")
        })
        .collect()
}

/// Run several members, each in its own process: provision a source's members
/// into temporary bundles (a bundle set is the hand-off already), then launch.
fn run_members(members: &mut [Wiring], input: &Input, args: &RunArgs) -> miette::Result<()> {
    // The children read the bundles after `launch` spawns them, so the temp
    // dir is bound here, for the whole run.
    let temp = tempfile::tempdir().into_diagnostic()?;
    let bundles = match input {
        Input::Source(source) => {
            let opts = PackageOptions {
                release: args.release,
                target: build_target(&args.cargo_arg),
                provenance: Some(source.clone()),
                built_at_unix: None,
            };
            let names: Vec<String> = namespaces(members).iter().map(|s| s.to_string()).collect();
            let mut bundles = Vec::new();
            for (member, ns) in members.iter_mut().zip(&names) {
                provision_run_artifacts(member, source, args)?;
                let out = temp.path().join(format!("{ns}.bundle"));
                write_bundle(member, &opts, &out).into_diagnostic()?;
                bundles.push(out);
            }
            bundles
        }
        Input::Bundles(paths) => paths.clone(),
    };

    let plan = launch::plan(members, &bundles);
    if !args.no_preflight {
        // Every block before any child starts, so they do not interleave.
        for (member, entry) in members.iter().zip(&plan) {
            let mut shown = member.clone();
            apply_overrides(&mut shown, args)?;
            let path = match input {
                Input::Source(source) => source.as_path(),
                Input::Bundles(_) => entry.bundle.as_path(),
            };
            ui::print_preflight(&shown, path);
        }
    }
    launch::launch(&plan, &launch::overrides(args))
}

/// `run` with no target uses `target.py` in the current directory.
fn detect_target() -> miette::Result<PathBuf> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    detect_target_in(&cwd)
}

/// [`detect_target`] against an explicit directory (the current directory is
/// process-global, so tests probe this).
fn detect_target_in(dir: &Path) -> miette::Result<PathBuf> {
    let target = dir.join("target.py");
    if target.exists() {
        return Ok(target);
    }
    Err(miette::miette!(
        "no `target.py` in `{}`; pass a target `.py` or a bundle: `metor-fsw run <target>`",
        dir.display()
    ))
}

/// Refresh a source target's dev packs before evaluation
/// ([`refresh_source_packs`]). Skipped under `--no-build`, which extends its
/// "locate, never build" promise to the packs.
fn refresh_run_packs(path: &Path, args: &RunArgs) -> miette::Result<()> {
    if args.no_build {
        return Ok(());
    }
    refresh_source_packs(path, args.release, &args.cargo_arg)
}

/// Fill a source target's artifact paths: the cargo build driver by default
/// (incremental, so a fresh tree is a no-op), or a cargo-free search of the
/// workspace target dir with `--no-build`. A bundle's paths are recorded
/// already, so only a source reaches this.
fn provision_run_artifacts(wiring: &mut Wiring, path: &Path, args: &RunArgs) -> miette::Result<()> {
    if args.no_build {
        let dir = match path.parent() {
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

/// A `<PATH>` is a bundle if it is a directory (the bundle layout), ends in
/// `.bundle`, or is a single-file `.metor` archive.
fn is_bundle(path: &Path) -> bool {
    path.is_dir()
        || path
            .extension()
            .is_some_and(|e| e == "bundle" || e == "metor")
}

/// Apply `run`'s override flags onto the loaded [`Wiring`] before [`resolve`].
/// A flag always beats the target's own setting.
fn apply_overrides(wiring: &mut Wiring, args: &RunArgs) -> miette::Result<()> {
    if args.wall {
        wiring.coordinator.clock = ClockSpec::Wall;
    } else if let Some(dt_secs) = args.sim_dt {
        wiring.coordinator.clock = ClockSpec::Simulated { dt_secs };
    }
    if let Some(rate) = args.cycle_rate {
        wiring.coordinator.cycle_rate = rate;
    }
    if let Some(addr) = args.serve {
        use crate::ir::{StateSpec, SystemSpec, TCP_SERVER_TYPE};
        let servers: Vec<usize> = wiring
            .states
            .iter()
            .enumerate()
            .filter(|(_, s)| s.ty == TCP_SERVER_TYPE)
            .map(|(index, _)| index)
            .collect();
        if servers.len() > 1 {
            let names: Vec<&str> = servers
                .iter()
                .map(|&index| wiring.states[index].name.as_str())
                .collect();
            return Err(miette::miette!(
                "`--serve` names one socket, but this target declares {} `TcpServer` states \
                 ({}); set the addresses in the target instead",
                names.len(),
                names.join(", ")
            ));
        }
        match servers.first().map(|&index| &mut wiring.states[index]) {
            Some(state) => {
                // Override only the address; a target-set `name` (advertised
                // over mDNS) survives the CLI address override.
                match &mut state.params {
                    crate::ir::ParamSource::Value(v) => {
                        v["addr"] = serde_json::Value::String(addr.to_string());
                    }
                    other => {
                        *other = crate::ir::ParamSource::Value(
                            serde_json::json!({ "addr": addr.to_string() }),
                        );
                    }
                }
            }
            None => {
                wiring.states.push(StateSpec::tcp_server("link", addr));
                wiring.systems.push(SystemSpec::downlink("telemetry"));
            }
        }
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
    /// different path than the original source), so the same target evaluated
    /// from two paths is not spurious drift, but a real emission change is.
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
        frozen.systems[0].src = anchor("/build/target.py");
        let mut relocated = base();
        relocated.systems[0].src = anchor("/tmp/bundle/target.py");
        assert_eq!(
            normalized_ir(&frozen),
            normalized_ir(&relocated),
            "same target, different provenance path — not drift"
        );

        let mut changed = base();
        changed.systems[0].src = anchor("/build/target.py");
        changed.coordinator.cycle_rate = 200.0;
        assert_ne!(
            normalized_ir(&frozen),
            normalized_ir(&changed),
            "a real emission change is drift"
        );
    }

    /// `run` with no target accepts exactly `target.py` in the directory and
    /// errors helpfully otherwise.
    #[test]
    fn detect_target_wants_target_py() {
        let dir = tempfile::tempdir().unwrap();
        let err = detect_target_in(dir.path()).expect_err("nothing to detect yet");
        assert!(err.to_string().contains("target.py"), "{err}");
        std::fs::write(dir.path().join("target.py"), "").unwrap();
        assert_eq!(
            detect_target_in(dir.path()).unwrap(),
            dir.path().join("target.py")
        );
    }

    /// The pre-eval pack refresh honors `--no-build` and is a clean no-op for
    /// a target with no path-source packs.
    #[test]
    fn refresh_run_packs_skips_and_noops() {
        let args = |no_build| RunArgs {
            paths: Vec::new(),
            target: None,
            no_build,
            release: false,
            cargo_arg: Vec::new(),
            no_manifest_sidecar: false,
            wall: false,
            serve: None,
            sim_dt: None,
            cycle_rate: None,
            cycles: None,
            no_preflight: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.py");
        std::fs::write(&target, "").unwrap();
        refresh_run_packs(&target, &args(true)).expect("--no-build: skipped");
        refresh_run_packs(&target, &args(false)).expect("no pyproject: nothing to refresh");
    }

    /// `--serve` edits the target's one link server, declares one when there
    /// is none, and refuses to guess between several.
    #[test]
    fn serve_needs_one_tcp_server() {
        use crate::ir::{ParamSource, StateSpec, TCP_SERVER_TYPE};
        use crate::wiring::WiringBuilder;

        let args = RunArgs {
            paths: Vec::new(),
            target: None,
            no_build: false,
            release: false,
            cargo_arg: Vec::new(),
            no_manifest_sidecar: false,
            wall: false,
            serve: Some("127.0.0.1:9000".parse().unwrap()),
            sim_dt: None,
            cycle_rate: None,
            cycles: None,
            no_preflight: false,
        };
        let server = |name: &str, addr: &str| StateSpec::tcp_server(name, addr.parse().unwrap());

        let mut none = WiringBuilder::new().build();
        apply_overrides(&mut none, &args).expect("no server: one is declared");
        assert_eq!(none.states.len(), 1);
        assert_eq!(none.systems.len(), 1, "with an all-taps downlink");

        let mut one = WiringBuilder::new().build();
        one.states.push(server("link", "0.0.0.0:2240"));
        apply_overrides(&mut one, &args).expect("one server: overridden");
        let ParamSource::Value(params) = &one.states[0].params else {
            unreachable!("tcp_server writes a value tree")
        };
        assert_eq!(params["addr"], "127.0.0.1:9000");

        let mut several = WiringBuilder::new().build();
        several.states.push(server("link", "0.0.0.0:2240"));
        several.states.push(server("peer", "0.0.0.0:2242"));
        let err = apply_overrides(&mut several, &args).expect_err("ambiguous");
        assert!(err.to_string().contains("link, peer"), "{err}");
        assert!(
            several
                .states
                .iter()
                .all(|s| s.ty != TCP_SERVER_TYPE || !format!("{:?}", s.params).contains("9000")),
            "nothing is edited on the way out"
        );
    }

    /// Provenance discovery finds `target.py`, and is `None` when a bundle
    /// carries none.
    #[test]
    fn find_provenance_finds_python() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_provenance(dir.path()).is_none(), "no provenance yet");
        std::fs::write(dir.path().join("target.py"), "").unwrap();
        assert_eq!(
            find_provenance(dir.path()),
            Some(dir.path().join("target.py"))
        );
    }

    /// Two args cannot both spell `--target`; clap only notices at runtime,
    /// so the whole command tree is asserted here.
    #[test]
    fn command_tree_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    fn member(namespace: Option<&str>, cycle_rate: f64) -> Wiring {
        use crate::wiring::WiringBuilder;
        let mut wiring = WiringBuilder::new()
            .coordinator(cycle_rate, ClockSpec::Wall)
            .build();
        wiring.coordinator.namespace = namespace.map(str::to_string);
        wiring
    }

    fn deployment(targets: Vec<Wiring>) -> Deployment {
        Deployment {
            ir_version: IR_VERSION,
            targets,
            hosts: Default::default(),
        }
    }

    /// Selection over a deployment of several members: the flag is required,
    /// names a member, and an unknown namespace lists the ones there are.
    /// Every message names the file, which the `LoadError` cannot.
    #[test]
    fn select_needs_a_namespace_when_there_are_several() {
        let path = Path::new("/build/target.py");
        let deployment = deployment(vec![
            member(Some("plant"), 100.0),
            member(Some("fsw"), 200.0),
        ]);

        let err = select(&deployment, path, None).expect_err("two members, no request");
        assert_eq!(
            err.to_string(),
            "deployment `target.py` has 2 targets; pick one with --target (plant, fsw)"
        );

        let err = select(&deployment, path, Some("fws")).expect_err("typo");
        assert_eq!(
            err.to_string(),
            "deployment `target.py` has no target `fws`; targets: plant, fsw"
        );

        let picked = select(&deployment, path, Some("fsw")).expect("names a member");
        assert_eq!(picked.coordinator.cycle_rate, 200.0);
    }

    /// A deployment of one needs no flag, accepts its own namespace, and
    /// says so plainly when it has none to match against.
    #[test]
    fn select_over_a_deployment_of_one() {
        let path = Path::new("target.py");

        let bare = deployment(vec![member(None, 100.0)]);
        assert!(select(&bare, path, None).is_ok(), "the only member");
        let err = select(&bare, path, Some("sat")).expect_err("nothing to match");
        assert_eq!(
            err.to_string(),
            "target `target.py` has no namespace; `--target sat` does not apply"
        );

        let named = deployment(vec![member(Some("fsw"), 100.0)]);
        assert!(select(&named, path, None).is_ok(), "still the only member");
        assert!(
            select(&named, path, Some("fsw")).is_ok(),
            "its own namespace"
        );
        let err = select(&named, path, Some("sat")).expect_err("a different one");
        assert_eq!(
            err.to_string(),
            "deployment `target.py` has no target `sat`; targets: fsw"
        );
    }

    /// `--check-ir` selects by the frozen `wiring.json`'s namespace, so a
    /// member bundle diffs against the member it was cut from.
    #[test]
    fn check_ir_selects_the_frozen_member() {
        let path = Path::new("target.py");
        let deployment = deployment(vec![
            member(Some("plant"), 100.0),
            member(Some("fsw"), 200.0),
        ]);
        let frozen = member(Some("fsw"), 200.0);

        let produced = select(&deployment, path, frozen.coordinator.namespace.as_deref())
            .expect("the frozen namespace names a member");
        assert_eq!(normalized_ir(produced), normalized_ir(&frozen));
    }

    /// `run`'s paths are one source or a set of bundles; a mix is a mistake
    /// the CLI names before it loads anything.
    #[test]
    fn classify_paths() {
        assert!(matches!(
            classify(&[PathBuf::from("target.py")]).unwrap(),
            Input::Source(p) if p == Path::new("target.py")
        ));
        let bundles = classify(&[PathBuf::from("a.bundle"), PathBuf::from("b.metor")]).unwrap();
        assert!(matches!(&bundles, Input::Bundles(p) if p.len() == 2));
        let err = classify(&[PathBuf::from("target.py"), PathBuf::from("a.bundle")])
            .expect_err("a source and a bundle");
        assert_eq!(
            err.to_string(),
            "`run` takes one source `.py` or bundles, not both: `target.py`, `a.bundle`"
        );
    }

    /// `--serve` is one socket: it needs a member, and with several it says
    /// which ones there are.
    #[test]
    fn serve_needs_a_target_on_several_members() {
        let args = |target: Option<&str>| RunArgs {
            paths: Vec::new(),
            target: target.map(str::to_string),
            no_build: false,
            release: false,
            cargo_arg: Vec::new(),
            no_manifest_sidecar: false,
            wall: false,
            serve: Some("127.0.0.1:2240".parse().unwrap()),
            sim_dt: None,
            cycle_rate: None,
            cycles: None,
            no_preflight: false,
        };
        let both = [member(Some("plant"), 100.0), member(Some("fsw"), 100.0)];
        let err = check_serve(&both, &args(None)).expect_err("two members, one socket");
        assert_eq!(
            err.to_string(),
            "`--serve` names one socket; pick the member it applies to with --target (plant, fsw)"
        );
        assert!(
            check_serve(&both[1..], &args(Some("fsw"))).is_ok(),
            "picked"
        );
    }

    /// A bundle set validates as a deployment, and its faults name the file
    /// the member came from.
    #[test]
    fn bundle_set_validation_names_the_file() {
        let paths = [
            PathBuf::from("dist/plant.metor"),
            PathBuf::from("fsw.metor"),
        ];
        let set = deployment(vec![member(None, 100.0), member(Some("fsw"), 100.0)]);
        let err = match validate_deployment(&set).unwrap_err() {
            LoadError::NamespaceRequired { index } => miette::miette!(
                "bundles: member {index} (`{}`) has no namespace; every member of a \
                 deployment needs one",
                file_label(&paths[index])
            ),
            other => deployment_fault("bundles", other),
        };
        assert_eq!(
            err.to_string(),
            "bundles: member 0 (`plant.metor`) has no namespace; every member of a deployment \
             needs one"
        );

        let clash = deployment(vec![member(Some("fsw"), 100.0), member(Some("fsw"), 100.0)]);
        let err = deployment_fault("bundles", validate_deployment(&clash).unwrap_err());
        assert_eq!(
            err.to_string(),
            "deployment `bundles`: duplicate target namespace `fsw`"
        );
    }

    /// The launcher's hand-off: a member written into a temp dir and loaded
    /// back is the same member, namespace and all.
    #[test]
    fn temp_bundle_round_trips_a_member() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("fsw.bundle");
        let opts = PackageOptions {
            release: false,
            target: None,
            provenance: None,
            built_at_unix: None,
        };
        write_bundle(&member(Some("fsw"), 100.0), &opts, &out).unwrap();
        let loaded = load_bundle(&out).unwrap();
        assert_eq!(loaded.coordinator.namespace.as_deref(), Some("fsw"));
    }
}
