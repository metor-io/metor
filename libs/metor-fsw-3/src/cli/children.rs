//! Every target of a deployment as a child of this binary, each running
//! `metor run <envelope> --target NS` with its output prefixed by its name.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child as Process, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use super::run::Run;

/// How often the parent looks for an exited child.
const POLL: Duration = Duration::from_millis(50);

/// One running target and the threads copying its output.
pub struct Child {
    pub namespace: String,
    process: Process,
    drains: Vec<JoinHandle<()>>,
}

/// The arguments a child runs with: the envelope, its target, and the
/// clock and cycle overrides the parent was given.
pub fn member_argv(envelope: &Path, namespace: &str, overrides: &Run) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "run".into(),
        envelope.into(),
        "--target".into(),
        namespace.into(),
    ];
    if let Some(cycles) = overrides.cycles {
        argv.extend(["--cycles".into(), cycles.to_string().into()]);
    }
    if let Some(rate) = overrides.wall {
        argv.extend(["--wall".into(), rate.to_string().into()]);
    }
    if let Some(dt) = overrides.sim_dt {
        argv.extend(["--sim-dt".into(), dt.to_string().into()]);
    }
    argv
}

/// Starts `namespace` from `envelope`, its stdout and stderr each copied to
/// this process's stderr under a `NS │ ` prefix.
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

fn drain(reader: impl Read + Send + 'static, namespace: String) -> JoinHandle<()> {
    std::thread::spawn(move || prefix(reader, &namespace, std::io::stderr().lock()))
}

/// Copies `reader` line by line onto `sink`, each line under `namespace`.
pub fn prefix(reader: impl Read, namespace: &str, mut sink: impl Write) {
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        let _ = writeln!(sink, "{namespace} │ {line}");
    }
}

/// Waits for every child. The first to exit nonzero kills the rest; the
/// result names every child that did not exit zero.
pub fn wait_all(mut children: Vec<Child>) -> Result<(), Vec<String>> {
    let mut failed = Vec::new();
    let mut running: Vec<usize> = (0..children.len()).collect();
    while !running.is_empty() {
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
    for child in children {
        for drain in child.drains {
            let _ = drain.join();
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_prefix_each_line() {
        let mut out = Vec::new();
        prefix("one\ntwo\n".as_bytes(), "gw", &mut out);
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "gw │ one\ngw │ two\n"
        );
    }
}
