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
use std::time::{Duration, Instant};

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

/// How long a child asked to stop has before it is killed.
const GRACE: Duration = Duration::from_secs(5);

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
/// ends. Bytes that are not UTF-8 become replacement characters.
pub fn each_line(reader: impl Read, mut f: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        let text = String::from_utf8_lossy(&line);
        f(text.strip_suffix('\n').unwrap_or(&text));
        line.clear();
    }
}

/// Waits for every child to exit.
///
/// When one fails, or this process gets SIGINT, the rest are asked to stop
/// with SIGINT, and killed after [`GRACE`], so each still reports its
/// summary. Returns the namespace of every child that failed on its own; a
/// child stopped this way has not failed.
pub fn wait_all(mut children: Vec<TargetProcess>) -> Result<(), Vec<String>> {
    let mut exits: Vec<Option<ExitStatus>> = vec![None; children.len()];
    let mut asked = vec![false; children.len()];
    let mut stopping: Option<Instant> = None;
    while exits.iter().any(Option::is_none) {
        let failing = exits
            .iter()
            .zip(&asked)
            .any(|(exit, asked)| !asked && exit.is_some_and(|status| !status.success()));
        if stopping.is_none() && (failing || super::run::interrupted()) {
            stopping = Some(Instant::now());
            ask_to_stop(&children, &exits, &mut asked);
        }
        let overdue = stopping.is_some_and(|since| since.elapsed() > GRACE);
        for (child, exit) in children.iter_mut().zip(&mut exits) {
            if exit.is_none() && overdue {
                let _ = child.process.kill();
            }
            if exit.is_none() {
                *exit = child.process.try_wait().ok().flatten();
            }
        }
        if exits.iter().any(Option::is_none) {
            std::thread::sleep(POLL);
        }
    }
    let failed: Vec<String> = children
        .iter()
        .zip(exits.iter().zip(&asked))
        .filter(|(_, (exit, asked))| !**asked && !exit.is_some_and(|status| status.success()))
        .map(|(child, _)| child.namespace.clone())
        .collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(failed),
    }
}

/// Sends SIGINT to every child still running, and marks it as asked.
fn ask_to_stop(children: &[TargetProcess], exits: &[Option<ExitStatus>], asked: &mut [bool]) {
    for ((child, exit), asked) in children.iter().zip(exits).zip(asked) {
        if exit.is_none() {
            child.interrupt();
            *asked = true;
        }
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
    fn test_wait_all_stops_the_rest_and_blames_only_the_failure() {
        assert_eq!(wait_all(vec![process("ok", "true", &[])]), Ok(()));
        assert_eq!(wait_all(Vec::new()), Ok(()));
        let started = Instant::now();
        let failed = wait_all(vec![process("bad", "false", &[]), sleeper()]);
        assert_eq!(failed, Err(vec!["bad".to_string()]));
        assert!(started.elapsed() < GRACE, "sleep stops on SIGINT");
    }

    #[test]
    fn test_each_line_survives_bytes_that_are_not_utf8() {
        let mut lines = Vec::new();
        each_line(&b"one\n\xff\ntwo\n"[..], |line| {
            lines.push(line.to_string())
        });
        assert_eq!(lines, ["one", "\u{fffd}", "two"]);
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
