//! `metor run`: evaluate a target file, load its packs, and cycle the graph.

use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Poll;
use core::time::Duration;
use std::ffi::OsString;
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use tracing_subscriber::layer::SubscriberExt;

use super::build::{cdylib_name, read_toml, string, triple};
use super::config::{ConfigError, TargetConfig};
use super::pack_dev::{PackDevError, pack_dev};
use crate::coordinator::{BuildError, Clock, Coordinator, SystemTable};
use crate::dl::{Pack, PackError};
use crate::pack::ABI_VERSION;

/// The metor-config package in this checkout, for an in-repo run.
const IN_REPO_CONFIG_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/python/metor-config");

/// `metor run`'s arguments.
#[derive(clap::Args, Debug, Default)]
pub struct Run {
    /// The target file to evaluate; `./target.py` by default.
    pub target: Option<PathBuf>,
    /// Stop after this many cycles instead of on SIGINT.
    #[arg(long)]
    pub cycles: Option<u64>,
    /// Pace the loop at this wall rate in Hz, overriding the target's clock.
    #[arg(long, conflicts_with = "sim_dt")]
    pub wall: Option<f64>,
    /// Step a simulated clock by this many seconds per cycle.
    #[arg(long)]
    pub sim_dt: Option<f64>,
}

/// A `RunError` is why a target did not run.
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
    #[error("--sim-dt must be a positive number of seconds, got {0}")]
    SimDt(f64),
    #[error("these systems latched off: {0}")]
    Latched(String),
}

/// Refreshes every dev pack, evaluates the target, and cycles until stopped.
pub fn run(args: Run) -> Result<(), RunError> {
    install_logging();
    let target = args
        .target
        .clone()
        .unwrap_or_else(|| PathBuf::from("target.py"));
    let packs = dev_packs(target.parent().unwrap_or(Path::new(".")));
    for pack in &packs {
        pack_dev(pack)?;
    }
    let config = eval_target(&target, &packs)?;
    let coordinator = load(config, &args)?;

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

/// Installs the console layer beside the one that queues lines for `log` ports,
/// both at `$RUST_LOG`'s level or `INFO`.
fn install_logging() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|level| level.parse().ok())
        .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    let subscriber = tracing_subscriber::registry()
        .with(level)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(crate::log::layer());
    // A host that already installed a subscriber keeps it.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// The dev packs a target depends on: `[tool.uv.sources]` paths that are both a
/// cargo crate and a pack.
pub fn dev_packs(target_dir: &Path) -> Vec<PathBuf> {
    let Ok(pyproject) = read_toml(&target_dir.join("pyproject.toml")) else {
        return Vec::new();
    };
    let Some(sources) = pyproject
        .get("tool")
        .and_then(|tool| tool.get("uv"))
        .and_then(|uv| uv.get("sources"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    sources
        .values()
        .filter_map(|source| string(source, &["path"]))
        .map(|path| target_dir.join(path))
        .filter(|root| is_dev_pack(root))
        .collect()
}

fn is_dev_pack(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
        && read_toml(&root.join("pyproject.toml")).is_ok_and(|pyproject| {
            pyproject
                .get("tool")
                .and_then(|tool| tool.get("metor"))
                .and_then(|metor| metor.get("pack"))
                .is_some()
        })
}

/// Runs the target file as a script and reads the config it emitted.
pub fn eval_target(path: &Path, dev_packs: &[PathBuf]) -> Result<TargetConfig, RunError> {
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
    Ok(TargetConfig::from_slice(&bytes)?)
}

/// `$METOR_PYTHON`, else the active virtualenv's, else `python3`.
fn interpreter() -> OsString {
    if let Some(python) = std::env::var_os("METOR_PYTHON") {
        return python;
    }
    match std::env::var_os("VIRTUAL_ENV") {
        Some(venv) => PathBuf::from(venv).join("bin/python").into(),
        None => "python3".into(),
    }
}

/// `metor-config` plus every dev pack's generated module, ahead of the inherited path.
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

/// Applies the clock overrides, opens every pack, and builds the graph.
pub fn load(mut config: TargetConfig, overrides: &Run) -> Result<Coordinator, RunError> {
    if let Some(rate) = overrides.wall {
        config.coordinator.clock = Clock::Wall { rate };
    }
    if let Some(dt) = overrides.sim_dt {
        let dt = Duration::try_from_secs_f64(dt).map_err(|_| RunError::SimDt(dt))?;
        config.coordinator.clock = Clock::Simulated { dt };
    }

    let mut table = SystemTable::new();
    let opened: Vec<Pack> = config
        .packs
        .iter()
        .map(|pack| {
            let path = pack.libs.join(triple()).join(cdylib_name(&pack.lib));
            // SAFETY: the path names a pack `pack dev` built against this ABI;
            // `open` checks the version before calling anything else.
            unsafe { Pack::open(&path) }
        })
        .collect::<Result<_, _>>()?;
    for (reference, pack) in config.packs.iter().zip(&opened) {
        table.register_pack(&reference.id, pack);
    }
    Ok(config.coordinator.build(&table)?)
}

/// Set once SIGINT arrived; the run's stop future reads it each cycle.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupt(_signal: core::ffi::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Makes SIGINT flip [`INTERRUPTED`] instead of killing the process.
fn watch_sigint() {
    // SAFETY: the handler only stores to an `AtomicBool`, which is
    // async-signal-safe; no other code path installs a SIGINT handler.
    unsafe { libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t) };
}

/// Ready on SIGINT, or once `cycles` cycles have run. `Coordinator::run` polls
/// it once per cycle.
fn stop(cycles: Option<u64>) -> impl Future<Output = ()> {
    let mut polls = 0;
    poll_fn(move |_| {
        polls += 1;
        let done = INTERRUPTED.load(Ordering::Relaxed) || cycles.is_some_and(|n| polls >= n);
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
    fn dev_packs_finds_the_pack_a_target_sources() {
        let found = dev_packs(&fixture());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].canonicalize().expect("it exists"),
            fixture().canonicalize().expect("it exists")
        );
    }

    #[test]
    fn a_directory_that_is_no_pack_is_no_dev_pack() {
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
    fn a_target_that_raises_reports_its_status() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let target = dir.path().join("target.py");
        std::fs::write(&target, "raise SystemExit(3)\n").expect("writes");
        let Err(RunError::Python(status)) = eval_target(&target, &[]) else {
            panic!("a raising target is an error")
        };
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn a_negative_sim_dt_is_rejected_before_any_pack_is_opened() {
        let config = TargetConfig {
            config_version: super::super::config::CONFIG_VERSION,
            packs: Vec::new(),
            coordinator: Default::default(),
        };
        let overrides = Run {
            sim_dt: Some(-1.0),
            ..Default::default()
        };
        assert!(matches!(
            load(config, &overrides),
            Err(RunError::SimDt(dt)) if dt == -1.0
        ));
    }

    #[test]
    fn the_stop_future_is_ready_on_the_last_cycle() {
        let mut stop = Box::pin(stop(Some(2)));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert_eq!(stop.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(stop.as_mut().poll(&mut cx), Poll::Ready(()));
    }
}
