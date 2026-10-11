//! Runs each target of a deployment as a child process.
//!
//! Each child runs `metor run <config> --target NS`. Its output is copied to
//! this process's stderr, with every line prefixed by the target's namespace.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child as Process, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use super::run::Run;

/// How often the parent checks whether a child has exited.
const POLL: Duration = Duration::from_millis(50);

/// A running target process. Dropping it kills the process.
pub struct Child {
    pub namespace: String,
    process: Process,
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
        ("--wall", overrides.wall.map(|rate| rate.to_string())),
        ("--sim-dt", overrides.sim_dt.map(|dt| dt.to_string())),
    ];
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
    .collect()
}

/// Starts the target `namespace` from `envelope` as a child process.
///
/// The child's stdout and stderr are both copied to this process's stderr,
/// with each line prefixed by `NS │ `.
pub fn spawn(envelope: &Path, namespace: &str, overrides: &Run) -> std::io::Result<Child> {
    let mut process = Command::new(std::env::current_exe()?)
        .args(member_argv(envelope, namespace, overrides))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut drains = Vec::with_capacity(2);
    if let Some(stdout) = process.stdout.take() {
        drains.push(drain(stdout, namespace.to_string()));
    }
    if let Some(stderr) = process.stderr.take() {
        drains.push(drain(stderr, namespace.to_string()));
    }
    Ok(Child {
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
) -> std::io::Result<Vec<Child>> {
    namespaces
        .iter()
        .map(|namespace| spawn(envelope, namespace, overrides))
        .collect()
}

impl Child {
    /// Sends the child SIGINT. The parent forwards its own interrupt this way.
    fn interrupt(&self) {
        // SAFETY: `kill` takes a pid and a signal number and does not touch
        // memory.
        unsafe { libc::kill(self.process.id() as i32, libc::SIGINT) };
    }
}

/// Kills and reaps the child, then waits for its output to finish copying.
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        for drain in self.drains.drain(..) {
            let _ = drain.join();
        }
    }
}

fn drain(reader: impl Read + Send + 'static, namespace: String) -> JoinHandle<()> {
    std::thread::spawn(move || prefix(reader, &namespace, std::io::stderr()))
}

/// Copies `reader` to `sink` line by line, prefixing each line with
/// `namespace`.
///
/// Each line is written in one call, so a sink that locks, like stderr, only
/// holds the lock while writing and never while waiting on the reader.
pub fn prefix(reader: impl Read, namespace: &str, mut sink: impl Write) {
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        let _ = writeln!(sink, "{namespace} │ {line}");
    }
}

/// Waits for every child to exit.
///
/// If any child exits with a failure, the rest are killed. A SIGINT to this
/// process is forwarded to every child. On failure, returns the namespace of
/// every child that did not exit successfully.
pub fn wait_all(mut children: Vec<Child>) -> Result<(), Vec<String>> {
    let mut failed = Vec::new();
    let mut running: Vec<usize> = (0..children.len()).collect();
    let mut forwarded = false;
    while !running.is_empty() {
        if !forwarded && super::run::interrupted() {
            forwarded = true;
            for &at in &running {
                children[at].interrupt();
            }
        }
        let mut exited = Vec::new();
        for &at in &running {
            if let Some(status) = children[at].process.try_wait().ok().flatten() {
                exited.push((at, status));
            }
        }
        for (at, status) in exited {
            running.retain(|&r| r != at);
            if !status.success() {
                failed.push(children[at].namespace.clone());
                for &other in &running {
                    let _ = children[other].process.kill();
                }
            }
        }
        if !running.is_empty() {
            std::thread::sleep(POLL);
        }
    }
    for child in &mut children {
        let status = child.process.wait();
        if status.as_ref().is_ok_and(ExitStatus::success) || failed.contains(&child.namespace) {
            continue;
        }
        failed.push(child.namespace.clone());
    }
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

    fn sleeper() -> Child {
        let process = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep runs");
        Child {
            namespace: "sleeper".into(),
            process,
            drains: Vec::new(),
        }
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
            wall: Some(50.0),
            sim_dt: Some(0.01),
            ..Run::default()
        };
        assert_eq!(
            member_argv(Path::new("d.json"), "fsw", &overrides),
            [
                "run", "d.json", "--target", "fsw", "--cycles", "3", "--wall", "50", "--sim-dt",
                "0.01"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn test_prefix_frees_the_sink_while_blocked() {
        let (lines, reader): (Sender<Vec<u8>>, _) = channel();
        let sink = Locking(Arc::new(Mutex::new(Vec::new())));
        let (out, copy) = (sink.0.clone(), sink.clone());
        let drain = std::thread::spawn(move || prefix(Channel(reader, Vec::new()), "gw", copy));

        lines.send(b"one\n".to_vec()).expect("the drain reads");
        let mut held = out.lock().expect("no writer panics");
        while held.is_empty() {
            drop(held);
            std::thread::sleep(Duration::from_millis(1));
            held = out.lock().expect("no writer panics");
        }
        assert_eq!(&*held, b"gw \xe2\x94\x82 one\n");
        drop(held);

        drop(lines);
        drain.join().expect("the drain ends");
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
    fn test_prefix_each_line() {
        let mut out = Vec::new();
        prefix("one\ntwo\n".as_bytes(), "gw", &mut out);
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "gw │ one\ngw │ two\n"
        );
    }
}
