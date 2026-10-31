//! `metor watch` reruns `metor run` when a source changes.
//!
//! Each run is a new process, since a loaded pack library cannot be
//! replaced. The run owns the terminal; the watcher only waits for changes
//! and restarts it.

use core::time::Duration;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Instant;

use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};

use super::run::{Run, RunError, dev_packs, find_target, interrupted, watch_sigint};
use super::ui::Ui;
use super::ui::event::Event;

/// How long changes settle before a rerun.
const SETTLE: Duration = Duration::from_millis(300);

/// How often the watcher checks for SIGINT and the run's exit.
const POLL: Duration = Duration::from_millis(100);

/// How long a run has to stop after SIGINT before it is killed.
const GRACE: Duration = Duration::from_secs(5);

/// The arguments to `metor watch`: those of `metor run`.
#[derive(clap::Args, Debug, Default)]
pub struct Watch {
    #[command(flatten)]
    pub run: Run,
}

/// Runs `metor run` with this command's arguments, again on each change,
/// until SIGINT.
pub fn watch(args: Watch, ui: &Ui) -> Result<(), RunError> {
    let target = target_file(args.run.target.as_deref())?;
    let (tx, rx) = channel();
    let mut debouncer = new_debouncer(SETTLE, tx).map_err(watch_error)?;
    for (path, mode) in watch_set(&target) {
        let _ = debouncer.watcher().watch(&path, mode);
    }
    watch_sigint();
    let argv = run_argv(std::env::args_os().skip(1));
    loop {
        let child = start(&argv)?;
        let Some(changed) = wait_for_change(child, &rx) else {
            return Err(RunError::Interrupted);
        };
        ui.emit(Event::Changed(&display(&changed)));
    }
}

/// The target file named, or the nearest one.
fn target_file(named: Option<&Path>) -> Result<PathBuf, RunError> {
    if let Some(named) = named {
        return Ok(named.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|source| RunError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    find_target(&cwd).ok_or(RunError::NoProject(cwd))
}

/// What to watch for `target`: its directory, and each dev pack's sources
/// and manifests.
pub fn watch_set(target: &Path) -> Vec<(PathBuf, RecursiveMode)> {
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut set = vec![(dir.to_path_buf(), RecursiveMode::NonRecursive)];
    for pack in dev_packs(dir) {
        set.push((pack.join("src"), RecursiveMode::Recursive));
        set.push((pack.join("Cargo.toml"), RecursiveMode::NonRecursive));
        set.push((pack.join("pyproject.toml"), RecursiveMode::NonRecursive));
    }
    set
}

/// Whether a change to `path` should rerun: a source or manifest, outside
/// build output and hidden directories.
pub fn relevant(path: &Path) -> bool {
    let ignored = path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.') || name == "target" || name == "__pycache__"
        }
        _ => false,
    });
    let source = path
        .extension()
        .is_some_and(|ext| ["py", "rs", "toml", "json"].iter().any(|kind| ext == *kind));
    !ignored && source
}

/// This process's arguments with its `watch` command turned into `run`.
pub fn run_argv(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut replaced = false;
    args.into_iter()
        .map(|arg| match arg == "watch" && !replaced {
            true => {
                replaced = true;
                "run".into()
            }
            false => arg,
        })
        .collect()
}

fn start(argv: &[OsString]) -> Result<Child, RunError> {
    let exe = std::env::current_exe().map_err(RunError::Spawn)?;
    Command::new(exe)
        .args(argv)
        .spawn()
        .map_err(RunError::Spawn)
}

/// Waits for a relevant change and stops the run for it, returning the
/// changed path. Returns `None` once SIGINT has arrived and the run has
/// stopped. A run that ends on its own waits for the next change.
fn wait_for_change(mut child: Child, rx: &Receiver<DebounceEventResult>) -> Option<PathBuf> {
    loop {
        if interrupted() {
            stop(&mut child);
            return None;
        }
        match rx.recv_timeout(POLL) {
            Ok(Ok(events)) => {
                if let Some(event) = events.into_iter().find(|event| relevant(&event.path)) {
                    stop(&mut child);
                    return Some(event.path);
                }
            }
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                stop(&mut child);
                return None;
            }
        }
        let _ = child.try_wait();
    }
}

/// Sends the run SIGINT and waits for it, killing it after [`GRACE`].
fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    // SAFETY: `kill` takes a pid and a signal number and touches no memory.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `path` relative to the working directory, when it is under it.
fn display(path: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    path.strip_prefix(&cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn watch_error(error: notify_debouncer_mini::notify::Error) -> RunError {
    RunError::Spawn(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relevant() {
        assert!(relevant(Path::new("proj/target.py")));
        assert!(relevant(Path::new("proj/pack/src/lib.rs")));
        assert!(relevant(Path::new("proj/pack/Cargo.toml")));
        assert!(!relevant(Path::new("proj/pack/target/debug/build.rs")));
        assert!(!relevant(Path::new("proj/pack/.metor/mod/__init__.py")));
        assert!(!relevant(Path::new("proj/__pycache__/t.py")));
        assert!(!relevant(Path::new("proj/notes.md")));
        assert!(!relevant(Path::new("proj/.target.py.swp")));
    }

    #[test]
    fn test_run_argv_turns_watch_into_run() {
        let argv = |args: &[&str]| run_argv(args.iter().map(OsString::from));
        assert_eq!(
            argv(&["watch", "--sim-dt", "10ms"]),
            ["run", "--sim-dt", "10ms"].map(OsString::from)
        );
        assert_eq!(
            argv(&["--color", "never", "watch", "watch.py"]),
            ["--color", "never", "run", "watch.py"].map(OsString::from)
        );
    }

    #[test]
    fn test_watch_set_covers_the_target_and_its_dev_packs() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack");
        let set = watch_set(&fixture.join("target.py"));
        assert_eq!(set[0], (fixture.clone(), RecursiveMode::NonRecursive));
        let root = fixture.canonicalize().expect("it exists");
        assert!(
            set.contains(&(root.join("src"), RecursiveMode::Recursive)),
            "{set:?}"
        );
        assert!(set.contains(&(root.join("Cargo.toml"), RecursiveMode::NonRecursive)));
    }

    #[test]
    fn test_watch_set_of_a_bare_target_is_its_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let set = watch_set(&dir.path().join("target.py"));
        assert_eq!(
            set,
            [(dir.path().to_path_buf(), RecursiveMode::NonRecursive)]
        );
    }
}
