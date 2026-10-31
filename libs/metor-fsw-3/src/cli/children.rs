//! Runs each target of a deployment as a child process.
//!
//! Each child runs `metor run <config> --target NS` with the parent's output
//! settings. Its stdout and stderr are copied line by line to the parent's,
//! and its log lines already name their target.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use super::run::Run;
use super::ui::caps::{Background, Depth};
use super::ui::{Format, Ui};

/// Set in a child's environment so it leaves the deployment's lines, such as
/// `Running`, to the parent.
pub const CHILD_ENV: &str = "METOR_CHILD";

/// Returns whether this process is a child of a multi-target run.
pub fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// How often the parent checks whether a child has exited.
const POLL: Duration = Duration::from_millis(50);

/// A running target process. Dropping it kills the process.
pub struct TargetProcess {
    pub namespace: String,
    process: Child,
    drains: Vec<JoinHandle<()>>,
}

/// Returns the command-line arguments for one child.
///
/// The child runs the target `namespace` from `envelope`, and inherits the
/// clock and cycle overrides the parent was given.
pub fn member_argv(envelope: &Path, namespace: &str, overrides: &Run) -> Vec<OsString> {
    let flags = [
        (
            "--cycles",
            overrides.cycles.map(|cycles| cycles.to_string()),
        ),
        (
            "--for",
            overrides.run_for.map(|span| span.as_secs_f64().to_string()),
        ),
        ("--wall", overrides.wall.map(|rate| rate.get().to_string())),
        (
            "--sim-dt",
            overrides.sim_dt.map(|dt| dt.as_secs_f64().to_string()),
        ),
        ("--epoch", overrides.epoch.map(|at| at.to_string())),
    ];
    let deterministic = overrides.deterministic.then(|| "--deterministic".into());
    let set = flags
        .into_iter()
        .filter_map(|(flag, value)| Some([flag.into(), value?.into()]));
    [
        "run".into(),
        envelope.into(),
        "--target".into(),
        namespace.into(),
    ]
    .into_iter()
    .chain(set.flatten())
    .chain(deterministic)
    .collect()
}

/// The flags and environment that give a child the parent's output.
///
/// The child writes to a pipe, so the parent's colour depth, glyphs, and
/// background are passed down rather than detected.
pub fn output_argv(ui: &Ui) -> (Vec<OsString>, Vec<(&'static str, OsString)>) {
    let color = if ui.caps.depth == Depth::None {
        "never"
    } else {
        "always"
    };
    let format = match ui.format {
        Format::Human => "human",
        Format::Json => "json",
    };
    let mut args: Vec<OsString> = ["--color", color, "--format", format]
        .map(OsString::from)
        .into();
    if !ui.caps.unicode {
        args.push("--ascii".into());
    }
    let colorfgbg = match ui.caps.background {
        Background::Dark => "15;0",
        Background::Light => "0;15",
    };
    let mut env = vec![(CHILD_ENV, "1".into()), ("COLORFGBG", colorfgbg.into())];
    let level = match ui.caps.depth {
        Depth::None => None,
        Depth::Ansi16 => Some("1"),
        Depth::Ansi256 => Some("2"),
        Depth::TrueColor => Some("3"),
    };
    env.extend(level.map(|level| ("FORCE_COLOR", level.into())));
    (args, env)
}

/// Starts the target `namespace` from `envelope` as a child process.
///
/// The child's stdout and stderr are copied to this process's, a line at a
/// time.
pub fn spawn(
    envelope: &Path,
    namespace: &str,
    overrides: &Run,
    ui: &Ui,
) -> std::io::Result<TargetProcess> {
    let (output, env) = output_argv(ui);
    let mut process = Command::new(std::env::current_exe()?)
        .args(member_argv(envelope, namespace, overrides))
        .args(output)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut drains = Vec::with_capacity(2);
    if let Some(stdout) = process.stdout.take() {
        drains.push(std::thread::spawn(move || relay(stdout, std::io::stdout())));
    }
    if let Some(stderr) = process.stderr.take() {
        drains.push(std::thread::spawn(move || relay(stderr, std::io::stderr())));
    }
    Ok(TargetProcess {
        namespace: namespace.to_string(),
        process,
        drains,
    })
}

/// Starts one child per namespace.
///
/// If a spawn fails, the children already started are dropped, which kills
/// them.
pub fn spawn_all(
    envelope: &Path,
    namespaces: &[&str],
    overrides: &Run,
    ui: &Ui,
) -> std::io::Result<Vec<TargetProcess>> {
    namespaces
        .iter()
        .map(|namespace| spawn(envelope, namespace, overrides, ui))
        .collect()
}

impl TargetProcess {
    /// Sends the child SIGINT. The parent forwards its own interrupt this way.
    fn interrupt(&self) {
        // SAFETY: `kill` takes a pid and a signal number and does not touch
        // memory.
        unsafe { libc::kill(self.process.id() as i32, libc::SIGINT) };
    }
}

/// Kills and reaps the child, then waits for its output to finish copying.
impl Drop for TargetProcess {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        for drain in self.drains.drain(..) {
            let _ = drain.join();
        }
    }
}

/// Copies `reader` to `sink` line by line.
///
/// Each line is written in one call, so a sink that locks, like stderr, only
/// holds the lock while writing and never while waiting on the reader, and
/// lines from several children never interleave.
pub fn relay(reader: impl Read, mut sink: impl Write) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        let _ = sink.write_all(&line).and_then(|()| sink.flush());
        line.clear();
    }
}

/// Waits for every child to exit.
///
/// If any child exits with a failure, the rest are killed. A SIGINT to this
/// process is forwarded to every child. On failure, returns the namespace of
/// every child that did not exit successfully.
pub fn wait_all(mut children: Vec<TargetProcess>) -> Result<(), Vec<String>> {
    let mut exits: Vec<Option<ExitStatus>> = vec![None; children.len()];
    let mut forwarded = false;
    while exits.iter().any(Option::is_none) {
        let interrupt = !forwarded && super::run::interrupted();
        forwarded |= interrupt;
        let failing = exits.iter().flatten().any(|status| !status.success());
        for (child, exit) in children.iter_mut().zip(&mut exits) {
            if exit.is_some() {
                continue;
            }
            if interrupt {
                child.interrupt();
            }
            if failing {
                let _ = child.process.kill();
            }
            *exit = child.process.try_wait().ok().flatten();
        }
        if exits.iter().any(Option::is_none) {
            std::thread::sleep(POLL);
        }
    }
    let failed: Vec<String> = children
        .iter()
        .zip(&exits)
        .filter(|(_, exit)| !exit.is_some_and(|status| status.success()))
        .map(|(child, _)| child.namespace.clone())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A reader that returns whatever the sender pushes, and blocks until
    /// then.
    struct Channel(Receiver<Vec<u8>>, Vec<u8>);

    impl Read for Channel {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1.is_empty() {
                match self.0.recv() {
                    Ok(next) => self.1 = next,
                    Err(_) => return Ok(0),
                }
            }
            let take = self.1.len().min(buf.len());
            buf[..take].copy_from_slice(&self.1[..take]);
            self.1.drain(..take);
            Ok(take)
        }
    }

    /// A sink that takes a lock on every write, like `Stderr` does.
    #[derive(Clone)]
    struct Locking(Arc<Mutex<Vec<u8>>>);

    impl Write for Locking {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("no writer panics")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn process(namespace: &str, program: &str, args: &[&str]) -> TargetProcess {
        let process = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .expect("the program runs");
        TargetProcess {
            namespace: namespace.into(),
            process,
            drains: Vec::new(),
        }
    }

    fn sleeper() -> TargetProcess {
        process("sleeper", "sleep", &["30"])
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal zero sends nothing; it only checks that the pid
        // exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[test]
    fn test_member_argv_forwards_overrides() {
        let bare = Run::default();
        assert_eq!(
            member_argv(Path::new("d.json"), "fsw", &bare),
            ["run", "d.json", "--target", "fsw"].map(OsString::from)
        );
        let overrides = Run {
            cycles: Some(3),
            wall: crate::WallRate::try_from(50.0).ok(),
            sim_dt: Some(Duration::from_millis(10)),
            epoch: Some(5),
            deterministic: true,
            ..Run::default()
        };
        assert_eq!(
            member_argv(Path::new("d.json"), "fsw", &overrides),
            [
                "run",
                "d.json",
                "--target",
                "fsw",
                "--cycles",
                "3",
                "--wall",
                "50",
                "--sim-dt",
                "0.01",
                "--epoch",
                "5",
                "--deterministic"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn test_relay_frees_the_sink_while_blocked() {
        let (lines, reader): (Sender<Vec<u8>>, _) = channel();
        let sink = Locking(Arc::new(Mutex::new(Vec::new())));
        let (out, copy) = (sink.0.clone(), sink.clone());
        let drain = std::thread::spawn(move || relay(Channel(reader, Vec::new()), copy));

        lines.send(b"one\n".to_vec()).expect("the drain reads");
        let mut held = out.lock().expect("no writer panics");
        while held.is_empty() {
            drop(held);
            std::thread::sleep(Duration::from_millis(1));
            held = out.lock().expect("no writer panics");
        }
        assert_eq!(&*held, b"one\n");
        drop(held);

        drop(lines);
        drain.join().expect("the drain ends");
    }

    #[test]
    fn test_wait_all_reports_failures_and_kills_the_rest() {
        assert_eq!(wait_all(vec![process("ok", "true", &[])]), Ok(()));
        assert_eq!(wait_all(Vec::new()), Ok(()));
        let failed = wait_all(vec![process("bad", "false", &[]), sleeper()]);
        assert_eq!(failed, Err(vec!["bad".to_string(), "sleeper".to_string()]));
    }

    #[test]
    fn test_dropped_child_is_reaped() {
        let child = sleeper();
        let pid = child.process.id();
        assert!(alive(pid));
        drop(child);
        assert!(!alive(pid), "a dropped child is killed and reaped");
    }

    #[test]
    fn test_relay_keeps_lines_and_styles() {
        let mut out = Vec::new();
        relay("one\n\x1b[1mtwo\x1b[0m\nend".as_bytes(), &mut out);
        assert_eq!(out, b"one\n\x1b[1mtwo\x1b[0m\nend");
    }

    #[test]
    fn test_output_argv_passes_the_parents_terminal() {
        use crate::cli::ui::caps::Caps;
        let caps = Caps {
            depth: Depth::TrueColor,
            background: Background::Light,
            ..Caps::plain()
        };
        let ui = Ui::with(
            Box::new(super::super::ui::human::Human::new(Vec::new(), caps)),
            caps,
        );
        let (args, env) = output_argv(&ui);
        assert_eq!(
            args,
            ["--color", "always", "--format", "human", "--ascii"].map(OsString::from)
        );
        assert!(env.contains(&("COLORFGBG", "0;15".into())));
        assert!(env.contains(&(CHILD_ENV, "1".into())));
        assert!(env.contains(&("FORCE_COLOR", "3".into())));
    }
}
