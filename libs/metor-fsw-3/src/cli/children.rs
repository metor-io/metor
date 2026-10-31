//! Runs each target of a deployment as a child process.
//!
//! Each child runs `metor run <config> --target NS --format json`. The
//! parent reads its events and shows them as its own, so every target's
//! lines share one look and one summary.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use super::relay::Relay;
use super::run::Run;

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

/// Starts the target `namespace` from `envelope` as a child process.
///
/// The child writes `--format json`; each line of its stdout goes to
/// `relay`, and each line of its stderr is shown as it is.
pub fn spawn(
    envelope: &Path,
    namespace: &str,
    overrides: &Run,
    relay: &Relay,
) -> std::io::Result<TargetProcess> {
    let mut process = Command::new(std::env::current_exe()?)
        .args(member_argv(envelope, namespace, overrides))
        .args(["--format", "json"])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut drains = Vec::with_capacity(2);
    if let Some(stdout) = process.stdout.take() {
        let relay = relay.clone();
        drains.push(std::thread::spawn(move || {
            each_line(stdout, |line| relay.line(line))
        }));
    }
    if let Some(stderr) = process.stderr.take() {
        let relay = relay.clone();
        drains.push(std::thread::spawn(move || {
            each_line(stderr, |line| relay.output(line))
        }));
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
    relay: &Relay,
) -> std::io::Result<Vec<TargetProcess>> {
    namespaces
        .iter()
        .map(|namespace| spawn(envelope, namespace, overrides, relay))
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

/// Calls `f` with each line `reader` gives, without its newline, until it
/// ends.
pub fn each_line(reader: impl Read, mut f: impl FnMut(&str)) {
    let reader = BufReader::new(reader);
    for line in reader.lines().map_while(Result::ok) {
        f(&line);
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

    use super::*;

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
        let timed = Run {
            run_for: Some(Duration::from_millis(1500)),
            ..Run::default()
        };
        assert_eq!(
            member_argv(Path::new("d.json"), "fsw", &timed),
            ["run", "d.json", "--target", "fsw", "--for", "1.5"].map(OsString::from)
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
    fn test_each_line_keeps_text_and_styles() {
        let mut lines = Vec::new();
        each_line("one\n\x1b[1mtwo\x1b[0m\nend".as_bytes(), |line| {
            lines.push(line.to_string())
        });
        assert_eq!(lines, ["one", "\x1b[1mtwo\x1b[0m", "end"]);
        each_line(&b""[..], |_| panic!("no lines in an empty stream"));
    }
}
