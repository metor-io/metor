//! `metor run` runs a target file.
//!
//! It evaluates the file, resolves the deployment it describes, loads the
//! target's packs, and cycles its systems until stopped. A deployment with
//! several targets runs each one as a child process.

use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Poll;
use core::time::Duration;
use std::ffi::OsString;
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use tracing_subscriber::layer::SubscriberExt;

use super::cargo::{lookup, read_toml, string};
use super::pack_dev::{PackDevError, pack_dev};
use crate::coordinator::{BuildError, BuildOptions, Clock, Coordinator, SystemTable};
use crate::deploy::{self, ConfigError, DeployError, DeploymentConfig, TargetConfig};
use crate::dl::store;
use crate::dl::{Pack, PackError, PackRef};
use crate::pack::ABI_VERSION;

/// The metor-config package in this checkout. Runs inside the repo use it
/// when it exists.
const IN_REPO_CONFIG_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/python/metor-config");

/// The arguments to `metor run`.
#[derive(clap::Args, Debug, Default)]
pub struct Run {
    /// The target file to evaluate. Defaults to `./target.py`.
    pub target: Option<PathBuf>,
    /// The namespace of the target to run. Without it, a deployment with
    /// several targets runs all of them.
    #[arg(long = "target", value_name = "NS")]
    pub member: Option<String>,
    /// Stop after this many cycles instead of on SIGINT.
    #[arg(long)]
    pub cycles: Option<u64>,
    /// Pace the loop at this wall rate in Hz, overriding the target's clock.
    #[arg(long, conflicts_with = "sim_dt")]
    pub wall: Option<f64>,
    /// Step a simulated clock by this many seconds per cycle.
    #[arg(long)]
    pub sim_dt: Option<f64>,
    /// Start the simulated clock at this time, in microseconds since the Unix
    /// epoch.
    #[arg(long)]
    pub epoch: Option<i64>,
    /// Turn every thread group on the coordinator thread, so a sim run gives
    /// the same results every time. Needs a simulated clock with an epoch.
    #[arg(long)]
    pub deterministic: bool,
    /// Print `<link> <addr>` for every listening link once the graph is built.
    #[arg(long)]
    pub print_ports: bool,
}

/// An error running a target.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("running the target file: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("the target file exited with {0}")]
    Python(ExitStatus),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    PackDev(#[from] PackDevError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Deploy(#[from] DeployError),
    #[error("--sim-dt must be a positive number of seconds, got {0}")]
    SimDt(f64),
    #[error("--epoch sets a simulated clock's start; pass --sim-dt or use a simulated target")]
    EpochWall,
    #[error("these systems latched off: {0}")]
    Latched(String),
    #[error("these targets did not exit cleanly: {0}")]
    Children(String),
    #[error("interrupted")]
    Interrupted,
    #[error("--print-ports names one target's links; pick one with --target")]
    PrintPortsNeedsTarget,
}

/// Runs `metor run`.
///
/// Rebuilds every dev pack, evaluates the target file, and cycles the target
/// until it is stopped.
pub fn run(args: Run) -> Result<(), RunError> {
    install_logging();
    let config = prepare(args.target.as_deref())?;
    if config.targets.len() > 1 && args.member.is_none() {
        if args.print_ports {
            return Err(RunError::PrintPortsNeedsTarget);
        }
        return run_children(&config, &args);
    }
    let member = config.target(args.member.as_deref())?;
    let coordinator = load(&config.targets[member], &args)?;
    if args.print_ports {
        print_ports();
    }

    watch_sigint();
    let cycles = args.cycles;
    let coordinator = stellarator::run(move || async move {
        let mut coordinator = coordinator;
        coordinator.run(stop(cycles)).await;
        coordinator
    });

    let latched: Vec<&str> = coordinator.latched().collect();
    if latched.is_empty() {
        return Ok(());
    }
    Err(RunError::Latched(latched.join(", ")))
}

/// Runs every target in `config` as a child process.
///
/// The resolved config is written to a temp file for the children to read.
/// Returns once every child has exited.
fn run_children(config: &DeploymentConfig, args: &Run) -> Result<(), RunError> {
    let path = std::env::temp_dir().join(format!("metor-deploy-{}.json", std::process::id()));
    std::fs::write(
        &path,
        serde_json::to_vec(config).map_err(ConfigError::Decode)?,
    )?;
    watch_sigint();
    let waited = super::children::spawn_all(&path, &config.namespaces(), args)
        .map(super::children::wait_all);
    let _ = std::fs::remove_file(&path);
    waited?.map_err(|failed| RunError::Children(failed.join(", ")))?;
    if interrupted() {
        return Err(RunError::Interrupted);
    }
    Ok(())
}

/// Prints the address of every bound link to stdout.
///
/// A caller that asked a link to bind port zero uses this to learn the real
/// port.
fn print_ports() {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for (link, addr) in crate::link::bound_ports() {
        let _ = writeln!(out, "{link} {addr}");
    }
    let _ = out.flush();
}

/// Sets up tracing to print to stderr and to forward lines to `log` ports.
///
/// The level comes from `$RUST_LOG`, and defaults to `INFO`.
fn install_logging() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|level| level.parse().ok())
        .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    let subscriber = tracing_subscriber::registry()
        .with(level)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(crate::log::LogLayer);
    // If the host already installed a subscriber, keep it.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Loads a target file and returns its resolved deployment.
///
/// Rebuilds every dev pack next to `target` first. A `.py` file is evaluated,
/// and a `.json` file is read as is. If the config still has peer references,
/// every target's packs are opened to resolve them.
pub fn prepare(target: Option<&Path>) -> Result<DeploymentConfig, RunError> {
    let target = target.unwrap_or(Path::new("target.py"));
    let packs = dev_packs(target.parent().unwrap_or(Path::new(".")));
    for pack in &packs {
        pack_dev(pack)?;
    }
    let mut config = if target.extension().is_some_and(|ext| ext == "json") {
        DeploymentConfig::from_slice(&std::fs::read(target)?)?
    } else {
        eval_target(target, &packs)?
    };
    if deploy::unresolved(&config) {
        let tables: Vec<SystemTable> = config
            .targets
            .iter()
            .map(table_for)
            .collect::<Result<_, _>>()?;
        let dir = target.parent().filter(|dir| !dir.as_os_str().is_empty());
        let dir = std::path::absolute(dir.unwrap_or(Path::new(".")))?;
        deploy::resolve_slots(&mut config, &tables, &store::dir(&dir))?;
        deploy::resolve(&mut config, &tables)?;
    }
    Ok(config)
}

/// Returns the dev packs a target depends on.
///
/// A dev pack is a `[tool.uv.sources]` path that is both a cargo crate and a
/// metor pack.
pub fn dev_packs(target_dir: &Path) -> Vec<PathBuf> {
    let Ok(pyproject) = read_toml(&target_dir.join("pyproject.toml")) else {
        return Vec::new();
    };
    let Some(sources) =
        lookup(&pyproject, &["tool", "uv", "sources"]).and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    let mut packs: Vec<_> = sources
        .values()
        .filter_map(|source| string(source, &["path"]))
        .map(|path| target_dir.join(path))
        .filter(|root| is_dev_pack(root))
        .filter_map(|root| root.canonicalize().ok())
        .collect();
    packs.sort();
    packs.dedup();
    packs
}

fn is_dev_pack(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
        && read_toml(&root.join("pyproject.toml"))
            .is_ok_and(|pyproject| lookup(&pyproject, &["tool", "metor", "pack"]).is_some())
}

/// Runs the target file as a script and reads the deployment it emitted.
pub fn eval_target(path: &Path, dev_packs: &[PathBuf]) -> Result<DeploymentConfig, RunError> {
    let out = std::env::temp_dir().join(format!("metor-config-{}.json", std::process::id()));
    let status = Command::new(interpreter())
        .arg(path)
        .env("PYTHONPATH", python_path(dev_packs))
        .env("METOR_CONFIG_OUT", &out)
        .env("METOR_FSW_ABI_VERSION", ABI_VERSION.to_string())
        .status()?;
    if !status.success() {
        return Err(RunError::Python(status));
    }
    let bytes = std::fs::read(&out)?;
    let _ = std::fs::remove_file(&out);
    Ok(DeploymentConfig::from_slice(&bytes)?)
}

/// Returns the Python interpreter to use: `$METOR_PYTHON`, then the active
/// virtualenv's, then `python3`.
fn interpreter() -> OsString {
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
fn python_path(dev_packs: &[PathBuf]) -> OsString {
    let mut entries: Vec<PathBuf> = match std::env::var_os("METOR_CONFIG_PY") {
        Some(config_py) => vec![config_py.into()],
        None if Path::new(IN_REPO_CONFIG_PY).is_dir() => vec![IN_REPO_CONFIG_PY.into()],
        None => Vec::new(),
    };
    entries.extend(dev_packs.iter().map(|root| root.join(".metor")));
    let mut path = std::env::join_paths(entries).unwrap_or_default();
    if let Some(inherited) = std::env::var_os("PYTHONPATH") {
        path.push(":");
        path.push(inherited);
    }
    path
}

/// Builds a target's coordinator.
///
/// Applies the clock overrides from the command line, then opens the target's
/// packs.
pub fn load(config: &TargetConfig, overrides: &Run) -> Result<Coordinator, RunError> {
    let config = apply_overrides(config, overrides)?;
    let table = table_for(&config)?;
    let options = BuildOptions {
        deterministic: overrides.deterministic,
    };
    Ok(config.coordinator.build_with(&table, options)?)
}

/// `config` with the clock overrides from the command line applied.
fn apply_overrides(config: &TargetConfig, overrides: &Run) -> Result<TargetConfig, RunError> {
    let mut config = config.clone();
    if let Some(rate) = overrides.wall {
        config.coordinator.clock = Clock::Wall { rate };
    }
    if let Some(dt) = overrides.sim_dt {
        let dt = Duration::try_from_secs_f64(dt).map_err(|_| RunError::SimDt(dt))?;
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
fn table_for(config: &TargetConfig) -> Result<SystemTable, RunError> {
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
fn watch_sigint() {
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
        let Err(RunError::Python(status)) = eval_target(&target, &[]) else {
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
            sim_dt: Some(0.02),
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
        assert!(matches!(load(&config, &epoch), Err(RunError::EpochWall)));
        let deterministic = Run {
            deterministic: true,
            ..Default::default()
        };
        assert!(matches!(
            load(&config, &deterministic),
            Err(RunError::Build(BuildError::DeterministicWall))
        ));
        let simulated = Run {
            deterministic: true,
            sim_dt: Some(0.01),
            epoch: Some(0),
            ..Default::default()
        };
        assert!(load(&config, &simulated).is_ok());
    }

    #[test]
    fn test_reject_negative_sim_dt() {
        let config = TargetConfig::default();
        let overrides = Run {
            sim_dt: Some(-1.0),
            ..Default::default()
        };
        assert!(matches!(
            load(&config, &overrides),
            Err(RunError::SimDt(dt)) if dt == -1.0
        ));
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
