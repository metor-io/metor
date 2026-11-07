//! `metor run` runs a target file.
//!
//! It evaluates the file, resolves the deployment it describes, loads the
//! target's packs, and cycles its systems until stopped. A deployment with
//! several targets runs each one as a child process.

use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Poll;
use core::time::Duration;
use std::env::JoinPathsError;
use std::ffi::OsString;
use std::future::{Future, poll_fn};
use std::io::{self as stdio, StdoutLock, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metor_proto_wkt::LogLevel;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;

use super::cargo::read_toml;
use super::console::{self, ConsoleLayer, Origin, Route, Settings, Tap};
use super::pack_dev::{PackDevError, pack_dev};
use super::relay::Relay;
use super::show::{plural, target_name};
use super::ui::Ui;
use super::ui::event::{Event, Phase, PhaseState, Summary, TargetSummary, Verb};
use super::ui::theme::Theme;
use super::units;
use crate::coordinator::{
    BuildError, BuildOptions, Clock, Coordinator, EndReason, SystemTable, WallRate,
};
use crate::deploy::{
    self, ConfigError, DeployError, DeploymentConfig, ResolvedDeployment, TargetConfig,
};
use crate::dl::store;
use crate::dl::{Pack, PackError, PackRef};

/// The metor-config package in this checkout. Runs inside the repo use it
/// when it exists.
const IN_REPO_CONFIG_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/python/metor-config");

/// The file `metor` looks for when it is not given one.
const TARGET_FILE: &str = "target.py";

/// The arguments to `metor run`.
#[derive(clap::Args, Debug, Default)]
pub struct Run {
    /// The target file to evaluate. Defaults to the nearest `target.py` in
    /// this directory or one above it.
    #[arg(add = super::completions::files())]
    pub target: Option<PathBuf>,
    /// Run the targets named NS or under it, as `flight` for
    /// `flight.cpu1`. Repeatable. Without it, every target runs.
    #[arg(long = "target", value_name = "NS", add = super::completions::names(&[super::completions::TARGET], false))]
    pub targets: Vec<String>,
    /// Leave out the targets named NS or under it. Repeatable.
    #[arg(long = "except", value_name = "NS", add = super::completions::names(&[super::completions::TARGET], false))]
    pub except: Vec<String>,
    /// Stop after this many cycles instead of on SIGINT.
    #[arg(long)]
    pub cycles: Option<u64>,
    /// Stop after this much clock time, such as `30s`, instead of on SIGINT.
    #[arg(long = "for", value_name = "TIME", conflicts_with = "cycles", value_parser = units::nonzero_duration)]
    pub run_for: Option<Duration>,
    /// Pace the loop at this wall rate, such as `100hz`, overriding the
    /// target's clock.
    #[arg(long, value_name = "RATE", conflicts_with = "sim_dt", value_parser = units::rate)]
    pub wall: Option<WallRate>,
    /// Step a simulated clock by this much per cycle, such as `10ms`. A bare
    /// number is seconds.
    #[arg(long, value_name = "TIME", value_parser = units::nonzero_duration)]
    pub sim_dt: Option<Duration>,
    /// Start the simulated clock at this UTC time, such as
    /// `2026-10-29T00:00Z`. A bare number is microseconds since the Unix
    /// epoch.
    #[arg(long, value_name = "TIME", value_parser = units::epoch)]
    pub epoch: Option<i64>,
    /// Turn every thread group on the coordinator thread, so a sim run gives
    /// the same results every time. Needs a simulated clock with an epoch.
    #[arg(long)]
    pub deterministic: bool,
}

/// An error running a target.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RunError {
    #[error("no target file at {}", .0.display())]
    #[diagnostic(
        code(M0206),
        help("check the path; without one, metor finds the nearest target.py")
    )]
    NoTargetFile(PathBuf),
    #[error("no target.py in {} or a directory above it", .0.display())]
    #[diagnostic(
        code(M0209),
        help("pass the target file, or run from a project directory")
    )]
    NoProject(PathBuf),
    #[error("these checks failed: {0}")]
    #[diagnostic(code(M0105), help("each fix is listed under its check"))]
    Doctor(String),
    #[error("nothing in the deployment is named `{name}`")]
    #[diagnostic(code(M0104), help("{}", crate::cli::diagnostic::suggest(.name, .known, crate::cli::diagnostic::SHOW_NAMES)))]
    NoSuchName { name: String, known: Vec<String> },
    #[error("`{name}` cannot name a project: {why}")]
    #[diagnostic(
        code(M0107),
        help("use lowercase letters, digits, and `-`, as in `my-sat`")
    )]
    BadName { name: String, why: &'static str },
    #[error("{} already exists", .0.display())]
    #[diagnostic(code(M0108), help("pick a new directory"))]
    Exists(PathBuf),
    #[error("running the target file: {0}")]
    #[diagnostic(code(M0203), help("set $METOR_PYTHON or activate a virtualenv"))]
    Spawn(std::io::Error),
    #[error("the target file made no deployment")]
    #[diagnostic(
        code(M0207),
        help("create a `Target` or a `Deployment` in the target file")
    )]
    NoDeployment,
    #[error("{}: {source}", path.display())]
    #[diagnostic(code(M0208))]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the target file exited with {status}")]
    #[diagnostic(code(M0201), help("the traceback above shows where"))]
    Python {
        status: ExitStatus,
        /// What the target file printed, for the caller to show.
        output: Vec<u8>,
    },
    #[error("building PYTHONPATH: {0}")]
    #[diagnostic(code(M0202))]
    PythonPath(#[from] JoinPathsError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    PackDev(#[from] PackDevError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Deploy(#[from] DeployError),
    #[error("--epoch sets a simulated clock's start; pass --sim-dt or use a simulated target")]
    #[diagnostic(code(M0101), help("pass --sim-dt, or run a simulated target"))]
    EpochWall,
    #[error("these systems latched off: {0}")]
    #[diagnostic(
        code(M0501),
        help("a latched system stops for the rest of the run; its last log lines say why")
    )]
    Latched(String),
    #[error("these targets did not exit cleanly: {0}")]
    #[diagnostic(code(M0502), help("each target's own error is above"))]
    Children(String),
    #[error("interrupted")]
    #[diagnostic(code(M0503))]
    Interrupted,
}

/// Returns a closure that turns an IO error into an `Io` error for `path`.
pub(super) fn io(path: &Path) -> impl FnOnce(std::io::Error) -> RunError + '_ {
    |source| RunError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Writes to stdout with the theme and width it can show.
pub(super) fn print(
    ui: &Ui,
    write: impl FnOnce(&mut StdoutLock<'static>, &Theme, Option<usize>) -> stdio::Result<()>,
) -> Result<(), RunError> {
    let theme = Theme::new(&ui.stdout_caps());
    let mut out = stdio::stdout().lock();
    write(&mut out, &theme, ui.stdout_width())
        .and_then(|()| out.flush())
        .map_err(io(Path::new("stdout")))
}

/// Runs `metor run`.
///
/// Rebuilds every dev pack, evaluates the target file, and cycles the target
/// until it is stopped. A child of a multi-target run catches SIGINT from
/// the start, so a stop during its build still reports.
pub fn run(args: Run, ui: &Ui) -> Result<(), RunError> {
    if super::children::is_child() {
        watch_sigint();
    }
    let route = Arc::new(Mutex::new(Route::default()));
    install_logging(ConsoleLayer {
        ui: ui.clone(),
        route: route.clone(),
    });
    let config = prepare(args.target.as_deref(), ui)?;
    let selected = config.select(&args.targets, &args.except)?;
    let [member] = selected[..] else {
        let namespaces = config.namespaces();
        let names: Vec<&str> = selected.iter().map(|&i| namespaces[i]).collect();
        ui.emit(Event::Running(&names));
        return run_children(&config, &names, &args, ui);
    };
    let latched = run_member(&config, member, &args, &route, ui)?;
    if !latched.is_empty() {
        return Err(RunError::Latched(latched.join(", ")));
    }
    if interrupted() && !super::children::is_child() {
        return Err(RunError::Interrupted);
    }
    Ok(())
}

/// Runs the `member`th target of `config` in this process until it stops,
/// then shows the summary. Returns the systems that latched off.
fn run_member(
    config: &ResolvedDeployment,
    member: usize,
    args: &Run,
    route: &Mutex<Route>,
    ui: &Ui,
) -> Result<Vec<String>, RunError> {
    let namespaces = config.namespaces();
    let target = (namespaces.len() > 1).then(|| (namespaces[member].to_string(), member));
    lock(route).target = target.clone();
    let (tx, rx) = console::channel();
    let tap = Tap {
        tx: tx.clone(),
        stats: Arc::default(),
    };
    let stats = tap.stats.clone();
    let (coordinator, systems) = load(&config.targets[member], args, Some(tap))?;
    let name = target_name(&config.targets[member]).to_string();
    ui.emit(Event::SourceWidth(source_width(config)));
    ui.emit(Event::Running(&[&name]));
    let origin = Origin {
        target,
        console: console::console_id(&systems),
        systems,
    };
    let started = Instant::now();
    let limit = cycle_limit(&config.targets[member], args)?;
    let (latched, cycles) = cycle(coordinator, limit, origin, rx, tx, route, ui);
    let target = TargetSummary {
        name,
        cycles,
        max_cycle: stats.max_cycle(),
        overruns: stats.overruns(),
    };
    end(ui, started, vec![target]);
    Ok(latched)
}

/// Shows the summary of a run that started at `started`.
fn end(ui: &Ui, started: Instant, targets: Vec<TargetSummary>) {
    ui.emit(Event::Ended(&Summary {
        took: started.elapsed(),
        interrupted: interrupted(),
        targets,
    }));
}

/// Cycles `coordinator` until it stops, with its console printing through
/// `ui`. Returns the path of every system that latched off, and the cycles
/// run.
fn cycle(
    coordinator: Coordinator,
    limit: Option<u64>,
    origin: Origin,
    rx: Receiver<console::Message>,
    tx: SyncSender<console::Message>,
    route: &Mutex<Route>,
    ui: &Ui,
) -> (Vec<String>, u64) {
    let namespace = origin.target.as_ref().map(|(name, _)| name.clone());
    let printer = console::spawn_printer(rx, origin, ui.clone());
    lock(route).tx = Some(tx);

    watch_sigint();
    let coordinator = stellarator::run(move || async move {
        let mut coordinator = coordinator;
        coordinator.run(stop(limit)).await;
        coordinator
    });

    let latched = coordinator
        .systems()
        .filter(|(_, end)| *end == Some(&EndReason::Panicked))
        .map(|(id, _)| match &namespace {
            Some(namespace) => format!("{namespace}.{id}"),
            None => id.to_string(),
        })
        .collect();
    let cycles = coordinator.cycle();
    drop(coordinator);
    lock(route).tx = None;
    let _ = printer.join();
    (latched, cycles)
}

fn lock(route: &Mutex<Route>) -> std::sync::MutexGuard<'_, Route> {
    route.lock().unwrap_or_else(|e| e.into_inner())
}

/// The widest `target.system` in `config`, or `system` in a deployment of
/// one.
fn source_width(config: &ResolvedDeployment) -> usize {
    let several = config.targets.len() > 1;
    config
        .targets
        .iter()
        .zip(config.namespaces())
        .flat_map(|(target, namespace)| {
            let prefix = if several { namespace.len() + 1 } else { 0 };
            target
                .coordinator
                .systems
                .iter()
                .map(move |system| prefix + system.id.len())
        })
        .max()
        .unwrap_or(0)
}

/// Runs the targets `namespaces` of `config`, each as a child process.
///
/// The resolved config is written to a temp file for the children to read.
/// Returns once every child has exited.
fn run_children(
    config: &ResolvedDeployment,
    namespaces: &[&str],
    args: &Run,
    ui: &Ui,
) -> Result<(), RunError> {
    let path = std::env::temp_dir().join(format!("metor-deploy-{}.json", std::process::id()));
    let bytes = serde_json::to_vec(config).map_err(ConfigError::Decode)?;
    std::fs::write(&path, bytes).map_err(io(&path))?;
    watch_sigint();
    let started = Instant::now();
    let relay = Relay::new(ui.clone(), namespaces);
    ui.emit(Event::SourceWidth(source_width(config)));
    let waited = super::children::spawn_all(&path, namespaces, args, &relay)
        .map(super::children::wait_all)
        .map_err(RunError::Spawn);
    let _ = std::fs::remove_file(&path);
    end(ui, started, relay.rows());
    let failed = waited?;
    if interrupted() {
        return Err(RunError::Interrupted);
    }
    failed.map_err(|failed| RunError::Children(failed.join(", ")))
}

/// Sets up tracing to print through `console` and to forward lines to `log`
/// ports.
///
/// The level comes from `$RUST_LOG`, and defaults to `INFO`.
fn install_logging(console: ConsoleLayer) {
    let level = log_level();
    let subscriber = tracing_subscriber::registry()
        .with(level)
        .with(console)
        .with(crate::log::LogLayer);
    // If the host already installed a subscriber, keep it.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// The level from `$RUST_LOG`, or `INFO`.
fn log_level() -> LevelFilter {
    std::env::var("RUST_LOG")
        .ok()
        .and_then(|level| level.parse().ok())
        .unwrap_or(LevelFilter::INFO)
}

/// The lowest level the console prints for `filter`. `OFF` prints errors.
fn console_level(filter: LevelFilter) -> LogLevel {
    filter
        .into_level()
        .map_or(LogLevel::Error, |level| crate::log::level_of(&level))
}

/// Loads a target file and returns its resolved deployment.
///
/// Rebuilds every dev pack next to `target` first. A `.py` file is evaluated,
/// and a `.json` file is read as is. If the config still has peer references,
/// every target's packs are opened to resolve them.
pub fn prepare(target: Option<&Path>, ui: &Ui) -> Result<ResolvedDeployment, RunError> {
    let target = &target_file(target)?;
    if !target.is_file() {
        return Err(RunError::NoTargetFile(target.clone()));
    }
    let packs = dev_packs(dir_of(target));
    for pack in &packs {
        pack_dev(pack, ui)?;
    }
    let config = if target.extension().is_some_and(|ext| ext == "json") {
        DeploymentConfig::from_slice(&std::fs::read(target).map_err(io(target))?)?
    } else {
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        phased(ui, Verb::EVALUATE, &name, || {
            eval_target(target, &packs, ui)
        })?
    };
    let subject = plural(config.targets.len(), "target");
    let config = phased(ui, Verb::RESOLVE, &subject, || resolve(config, target))?;
    if target.extension().is_some_and(|ext| ext == "py") && !super::children::is_child() {
        super::completions::write_cache(target, &config);
    }
    Ok(config)
}

/// `named`, or the nearest target file to the working directory.
pub(super) fn target_file(named: Option<&Path>) -> Result<PathBuf, RunError> {
    if let Some(named) = named {
        return Ok(named.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(io(Path::new(".")))?;
    find_target(&cwd).ok_or(RunError::NoProject(cwd))
}

/// The directory of the target file `target`.
pub(super) fn dir_of(target: &Path) -> &Path {
    let dir = target.parent().filter(|dir| !dir.as_os_str().is_empty());
    dir.unwrap_or(Path::new("."))
}

/// The nearest `target.py` in `start` or a directory above it, stopping at
/// the root of a git repository.
pub fn find_target(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let target = dir.join(TARGET_FILE);
        if target.is_file() {
            return Some(target);
        }
        if dir.join(".git").exists() {
            return None;
        }
    }
    None
}

/// Runs `f` as a phase on `ui`. On failure, shows the output the failed
/// program left after the phase's line.
pub(super) fn phased<T>(
    ui: &Ui,
    verb: Verb,
    subject: &str,
    f: impl FnOnce() -> Result<T, RunError>,
) -> Result<T, RunError> {
    let phase = |state| {
        ui.emit(Event::Phase(Phase {
            verb,
            subject,
            state,
        }))
    };
    let started = Instant::now();
    phase(PhaseState::Started);
    let result = f();
    match &result {
        Ok(_) => phase(PhaseState::Done(started.elapsed())),
        Err(error) => {
            phase(PhaseState::Failed);
            if let RunError::Python { output, .. } = error {
                ui.emit(Event::Output(output));
            }
        }
    }
    result
}

/// Resolves the peer references in `config`, opening every target's packs
/// if any are left.
fn resolve(mut config: DeploymentConfig, target: &Path) -> Result<ResolvedDeployment, RunError> {
    if let Some(pending) = deploy::pending(&config)? {
        let tables: Vec<SystemTable> = config
            .targets
            .iter()
            .map(table_for)
            .collect::<Result<_, _>>()?;
        let dir = dir_of(target);
        let dir = std::path::absolute(dir).map_err(io(dir))?;
        pending.resolve(&mut config, &tables, &store::dir(&dir))?;
    }
    Ok(config.into_runtime()?)
}

/// Returns the dev packs a target depends on.
///
/// A dev pack is a `[tool.uv.sources]` path that is both a cargo crate and a
/// metor pack.
pub fn dev_packs(target_dir: &Path) -> Vec<PathBuf> {
    let Ok(project) = read_toml::<toml::Table>(&target_dir.join("pyproject.toml")) else {
        return Vec::new();
    };
    let sources = project
        .get("tool")
        .and_then(|tool| tool.get("uv")?.get("sources")?.as_table());
    let mut packs: Vec<_> = sources
        .into_iter()
        .flat_map(toml::Table::values)
        .filter_map(|source| source.get("path")?.as_str())
        .map(|path| target_dir.join(path))
        .filter(|root| is_dev_pack(root))
        .filter_map(|root| root.canonicalize().ok())
        .collect();
    packs.sort();
    packs.dedup();
    packs
}

/// Whether `root` is a cargo crate with a `[tool.metor.pack]` table.
fn is_dev_pack(root: &Path) -> bool {
    let pack = |project: toml::Table| {
        let pack = project
            .get("tool")
            .and_then(|tool| tool.get("metor")?.get("pack"));
        pack.is_some_and(toml::Value::is_table)
    };
    root.join("Cargo.toml").is_file() && read_toml(&root.join("pyproject.toml")).is_ok_and(pack)
}

/// Runs the target file as a script and reads the deployment it emitted.
///
/// What it prints is kept, so a traceback shows after the phase line and
/// never in `--format json`'s stream. On success, any of it, such as a
/// warning, is shown on `ui`.
pub fn eval_target(
    path: &Path,
    dev_packs: &[PathBuf],
    ui: &Ui,
) -> Result<DeploymentConfig, RunError> {
    let out = std::env::temp_dir().join(format!("metor-config-{}.json", std::process::id()));
    let result = Command::new(interpreter())
        .arg(path)
        .env("PYTHONPATH", python_path(dev_packs)?)
        .env("METOR_CONFIG_OUT", &out)
        .stdin(Stdio::null())
        .output()
        .map_err(RunError::Spawn)?;
    let mut output = result.stdout;
    output.extend_from_slice(&result.stderr);
    if !result.status.success() {
        return Err(RunError::Python {
            status: result.status,
            output,
        });
    }
    if !output.is_empty() {
        ui.emit(Event::Output(&output));
    }
    let bytes = std::fs::read(&out).map_err(|_| RunError::NoDeployment)?;
    let _ = std::fs::remove_file(&out);
    Ok(DeploymentConfig::from_slice(&bytes)?)
}

/// Returns the Python interpreter to use: `$METOR_PYTHON`, then the active
/// virtualenv's, then `python3`.
pub(super) fn interpreter() -> OsString {
    if let Some(python) = std::env::var_os("METOR_PYTHON") {
        return python;
    }
    match std::env::var_os("VIRTUAL_ENV") {
        Some(venv) => PathBuf::from(venv).join("bin/python").into(),
        None => "python3".into(),
    }
}

/// Builds the `PYTHONPATH` for a target file.
///
/// It lists `metor-config` and every dev pack's generated module, followed
/// by the inherited `PYTHONPATH`.
pub(super) fn python_path(dev_packs: &[PathBuf]) -> Result<OsString, JoinPathsError> {
    let mut entries: Vec<PathBuf> = match std::env::var_os("METOR_CONFIG_PY") {
        Some(config_py) => vec![config_py.into()],
        None if Path::new(IN_REPO_CONFIG_PY).is_dir() => vec![IN_REPO_CONFIG_PY.into()],
        None => Vec::new(),
    };
    entries.extend(dev_packs.iter().map(|root| root.join(".metor")));
    if let Some(inherited) = std::env::var_os("PYTHONPATH") {
        entries.extend(std::env::split_paths(&inherited));
    }
    std::env::join_paths(entries)
}

/// Builds a target's coordinator.
///
/// Applies the clock overrides from the command line, then opens the target's
/// packs. With `console`, appends the console sending to it. Returns the ids
/// of the systems the console reads.
pub fn load(
    config: &TargetConfig,
    overrides: &Run,
    console: Option<Tap>,
) -> Result<(Coordinator, Vec<String>), RunError> {
    let mut config = apply_overrides(config, overrides)?;
    let mut table = table_for(&config)?;
    let systems = match console {
        Some(Tap { tx, stats }) => {
            let settings = Settings {
                min: console_level(log_level()),
                budget: budget(&config.coordinator.clock),
                stats,
            };
            console::attach(&mut config, &mut table, tx, settings)?
        }
        None => Vec::new(),
    };
    let options = BuildOptions {
        deterministic: overrides.deterministic,
    };
    Ok((config.coordinator.build_with(&table, options)?, systems))
}

/// The cycles to run: `--cycles`, or `--for` in cycles of the target's
/// clock after the overrides, or `None` to run until SIGINT.
fn cycle_limit(config: &TargetConfig, args: &Run) -> Result<Option<u64>, RunError> {
    let Some(span) = args.run_for else {
        return Ok(args.cycles);
    };
    let cycles = match apply_overrides(config, args)?.coordinator.clock {
        Clock::Wall { rate } => units::cycles_at(span, rate.get()),
        Clock::Simulated { dt, .. } => units::cycles_in(span, dt),
    };
    Ok(Some(cycles))
}

/// The longest a cycle may take on `clock`: its period on a wall clock,
/// none on a simulated one.
fn budget(clock: &Clock) -> Option<Duration> {
    match clock {
        Clock::Wall { rate } => Duration::try_from_secs_f64(rate.get().recip()).ok(),
        Clock::Simulated { .. } => None,
    }
}

/// `config` with the clock overrides from the command line applied.
fn apply_overrides(config: &TargetConfig, overrides: &Run) -> Result<TargetConfig, RunError> {
    let mut config = config.clone();
    if let Some(rate) = overrides.wall {
        config.coordinator.clock = Clock::Wall { rate };
    }
    if let Some(dt) = overrides.sim_dt {
        let epoch = config.coordinator.clock.epoch();
        config.coordinator.clock = Clock::Simulated { dt, epoch };
    }
    if let Some(at) = overrides.epoch {
        let Clock::Simulated { epoch, .. } = &mut config.coordinator.clock else {
            return Err(RunError::EpochWall);
        };
        *epoch = Some(metor_proto::types::Timestamp(at));
    }
    Ok(config)
}

/// Builds the system table for a target: the built-ins plus every pack the
/// target lists.
fn table_for<S, R>(config: &TargetConfig<S, R>) -> Result<SystemTable, RunError> {
    let mut table = SystemTable::new();
    crate::builtins::register_builtins(&mut table)?;
    let opened: Vec<Pack> = config
        .packs
        .iter()
        .map(PackRef::open)
        .collect::<Result<_, _>>()?;
    for (reference, pack) in config.packs.iter().zip(opened) {
        table.register_pack(&reference.id, Arc::new(pack))?;
    }
    Ok(table)
}

/// Set when SIGINT arrives. The run's stop future checks it every cycle.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupt(_signal: core::ffi::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Returns true once SIGINT has arrived.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Makes SIGINT flip [`INTERRUPTED`] instead of killing the process.
pub(super) fn watch_sigint() {
    // SAFETY: the handler only stores to an `AtomicBool`, which is
    // async-signal-safe, and no other code installs a SIGINT handler.
    unsafe { libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t) };
}

/// Returns a future that completes on SIGINT, or after `cycles` cycles.
///
/// `Coordinator::run` polls it once per cycle, so each poll counts one cycle.
fn stop(cycles: Option<u64>) -> impl Future<Output = ()> {
    let mut polls = 0;
    poll_fn(move |_| {
        polls += 1;
        let done = interrupted() || cycles.is_some_and(|n| polls >= n);
        if done { Poll::Ready(()) } else { Poll::Pending }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack")
    }

    #[test]
    fn test_dev_packs_finds_source_pack() {
        let found = dev_packs(&fixture());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].canonicalize().expect("it exists"),
            fixture().canonicalize().expect("it exists")
        );
    }

    #[test]
    fn test_dev_packs_skips_non_pack() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(dev_packs(dir.path()).is_empty(), "no pyproject at all");

        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"t\"\n\n[tool.uv.sources]\nplain = { path = \".\" }\n",
        )
        .expect("writes");
        assert!(dev_packs(dir.path()).is_empty(), "no Cargo.toml, no pack");
    }

    #[test]
    fn test_target_error_reports_status() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let target = dir.path().join("target.py");
        std::fs::write(&target, "raise SystemExit(3)\n").expect("writes");
        let Err(RunError::Python { status, .. }) = eval_target(&target, &[], &Ui::silent()) else {
            panic!("a raising target is an error")
        };
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn test_epoch_overrides_the_target_and_survives_sim_dt() {
        let mut config = TargetConfig::default();
        config.coordinator.clock = Clock::Simulated {
            dt: Duration::from_millis(10),
            epoch: Some(metor_proto::types::Timestamp(1)),
        };
        let kept = Run {
            sim_dt: Some(Duration::from_millis(20)),
            ..Default::default()
        };
        let clock = apply_overrides(&config, &kept).unwrap().coordinator.clock;
        assert_eq!(clock.epoch(), Some(metor_proto::types::Timestamp(1)));
        let moved = Run {
            epoch: Some(5),
            ..Default::default()
        };
        let clock = apply_overrides(&config, &moved).unwrap().coordinator.clock;
        assert_eq!(clock.epoch(), Some(metor_proto::types::Timestamp(5)));
    }

    #[test]
    fn test_epoch_and_deterministic_need_a_simulated_clock() {
        let config = TargetConfig::default();
        let epoch = Run {
            epoch: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            load(&config, &epoch, None),
            Err(RunError::EpochWall)
        ));
        let deterministic = Run {
            deterministic: true,
            ..Default::default()
        };
        assert!(matches!(
            load(&config, &deterministic, None),
            Err(RunError::Build(BuildError::DeterministicWall))
        ));
        let simulated = Run {
            deterministic: true,
            sim_dt: Some(Duration::from_millis(10)),
            epoch: Some(0),
            ..Default::default()
        };
        assert!(load(&config, &simulated, None).is_ok());
    }

    #[test]
    fn test_console_level_follows_the_filter() {
        assert_eq!(console_level(LevelFilter::DEBUG), LogLevel::Debug);
        assert_eq!(console_level(LevelFilter::INFO), LogLevel::Info);
        assert_eq!(console_level(LevelFilter::OFF), LogLevel::Error);
    }

    #[test]
    fn test_find_target_walks_up_to_the_repo_root() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let repo = dir.path().join("repo");
        let deep = repo.join("a/b");
        std::fs::create_dir_all(&deep).expect("makes dirs");
        std::fs::create_dir(repo.join(".git")).expect("makes .git");
        assert_eq!(find_target(&deep), None, "stops at the repo root");

        std::fs::write(dir.path().join("target.py"), "").expect("writes");
        assert_eq!(find_target(&deep), None, "never looks above .git");

        std::fs::write(repo.join("target.py"), "").expect("writes");
        assert_eq!(find_target(&deep), Some(repo.join("target.py")));

        std::fs::write(deep.join("target.py"), "").expect("writes");
        assert_eq!(find_target(&deep), Some(deep.join("target.py")));
    }

    #[test]
    fn test_for_counts_cycles_of_the_clock() {
        let mut config = TargetConfig::default();
        config.coordinator.clock = Clock::Simulated {
            dt: Duration::from_millis(10),
            epoch: None,
        };
        let one_second = Run {
            run_for: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(cycle_limit(&config, &one_second).ok(), Some(Some(100)));
        let walled = Run {
            wall: WallRate::try_from(50.0).ok(),
            ..one_second
        };
        assert_eq!(cycle_limit(&config, &walled).ok(), Some(Some(50)));
        for (hz, expected) in [(30.0, 30), (120.0, 120), (3.0, 3)] {
            let walled = Run {
                wall: WallRate::try_from(hz).ok(),
                run_for: Some(Duration::from_secs(1)),
                ..Default::default()
            };
            assert_eq!(
                cycle_limit(&config, &walled).ok(),
                Some(Some(expected)),
                "{hz} Hz"
            );
        }
        let counted = Run {
            cycles: Some(7),
            ..Default::default()
        };
        assert_eq!(cycle_limit(&config, &counted).ok(), Some(Some(7)));
        assert_eq!(cycle_limit(&config, &Run::default()).ok(), Some(None));
    }

    #[test]
    fn test_stop_after_last_cycle() {
        let mut stop = Box::pin(stop(Some(2)));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert_eq!(stop.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stop.as_mut().poll(&mut cx), Poll::Ready(()));
    }
}
