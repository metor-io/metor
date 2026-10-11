//! Running the members of a deployment.
//!
//! Everything here rests on one contract: a member runs as
//! `metor-fsw run <bundle> --target <ns>` plus the clock overrides the
//! command line carried. [`member_argv`] is that argument list as data, so
//! whatever composes members — the launcher below, or a renderer that emits
//! a systemd unit — spawns the same process from the same function.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use miette::IntoDiagnostic;
use owo_colors::{OwoColorize, Style};

use super::RunArgs;
use crate::wiring::Wiring;

/// One member the launcher runs: its bundle and the namespace it must match.
pub(super) struct Member {
    pub bundle: PathBuf,
    pub namespace: String,
}

/// Pair every member with the bundle it runs from, in envelope order.
pub(super) fn plan(members: &[Wiring], bundles: &[PathBuf]) -> Vec<Member> {
    members
        .iter()
        .zip(bundles)
        .map(|(member, bundle)| Member {
            bundle: bundle.clone(),
            namespace: member
                .coordinator
                .namespace
                .clone()
                .expect("a validated deployment of several names every member"),
        })
        .collect()
}

/// The clock overrides `run` forwards to every member.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Overrides {
    wall: bool,
    sim_dt: Option<f64>,
    cycle_rate: Option<f64>,
    cycles: Option<usize>,
}

/// The clock subset of the command line; the build flags act in the parent.
pub(super) fn overrides(args: &RunArgs) -> Overrides {
    Overrides {
        wall: args.wall,
        sim_dt: args.sim_dt,
        cycle_rate: args.cycle_rate,
        cycles: args.cycles,
    }
}

/// How one member runs, anywhere: the leaf's argument list. `--peer` is not
/// here: local members find each other on loopback, and a renderer with the
/// envelope's `hosts` emits it per member itself.
pub(super) fn member_argv(bundle: &Path, namespace: &str, overrides: &Overrides) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("run"),
        bundle.as_os_str().to_os_string(),
        OsString::from("--target"),
        OsString::from(namespace),
        OsString::from("--no-preflight"),
    ];
    if overrides.wall {
        argv.push(OsString::from("--wall"));
    }
    if let Some(dt) = overrides.sim_dt {
        argv.push(OsString::from("--sim-dt"));
        argv.push(OsString::from(format!("{dt}")));
    }
    if let Some(hz) = overrides.cycle_rate {
        argv.push(OsString::from("--cycle-rate"));
        argv.push(OsString::from(format!("{hz}")));
    }
    if let Some(n) = overrides.cycles {
        argv.push(OsString::from("--cycles"));
        argv.push(OsString::from(n.to_string()));
    }
    argv
}

/// Run every member in its own process and wait; the first failure stops the
/// rest. Each member's stdout and stderr share one pipe, which a thread drains
/// line by line onto the parent's stderr under the member's prefix.
pub(super) fn launch(members: &[Member], overrides: &Overrides) -> miette::Result<()> {
    let exe = std::env::current_exe().into_diagnostic()?;
    let colour = supports_color::on(supports_color::Stream::Stderr).is_some();
    let (done, arrived) = std::sync::mpsc::channel();
    let mut children = Vec::new();

    for (index, (member, prefix)) in members.iter().zip(prefixes(members, colour)).enumerate() {
        let (reader, writer) = std::io::pipe().into_diagnostic()?;
        let mut command = Command::new(&exe);
        command
            .args(member_argv(&member.bundle, &member.namespace, overrides))
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer.try_clone().into_diagnostic()?))
            .stderr(Stdio::from(writer));
        if colour {
            // The child's stderr is a pipe, so it would otherwise drop colour.
            command.env("CLICOLOR_FORCE", "1");
        }
        // The command owns both writer ends and drops them here; a clone left
        // in the parent would mean no EOF ever arrives.
        children.push(command.spawn().into_diagnostic()?);
        let done = done.clone();
        std::thread::spawn(move || {
            let _ = pump(
                std::io::BufReader::new(reader),
                &prefix,
                &mut std::io::stderr(),
            );
            let _ = done.send(index);
        });
    }
    drop(done);

    let mut results: Vec<(&str, ExitStatus)> = Vec::new();
    let mut waited = vec![false; members.len()];
    for _ in 0..members.len() {
        let Ok(index) = arrived.recv() else {
            break;
        };
        let status = children[index].wait().into_diagnostic()?;
        waited[index] = true;
        results.push((members[index].namespace.as_str(), status));
        if !status.success() {
            // Fail fast: a half-deployment is nobody's state. The killed
            // members' pipes are left to close with the process.
            for (other, child) in children.iter_mut().enumerate() {
                if !waited[other] {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            break;
        }
    }
    verdict(&results)
}

/// The member colours, the pre-flight's dot palette in envelope order.
const PALETTE: [fn(Style) -> Style; 4] = [
    Style::bright_magenta,
    Style::bright_cyan,
    Style::bright_green,
    Style::bright_yellow,
];

/// One padded, styled prefix per member: the namespace bold in its colour,
/// then a dimmed `│`.
fn prefixes(members: &[Member], colour: bool) -> Vec<String> {
    let width = members.iter().map(|m| m.namespace.len()).max().unwrap_or(0);
    members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            // Pad before styling: ANSI escapes would defeat a format width.
            let name = format!("{:width$}", member.namespace);
            if !colour {
                return format!("{name} │ ");
            }
            let style = PALETTE[index % PALETTE.len()](Style::new()).bold();
            format!(
                "{} {} ",
                name.style(style),
                "│".style(Style::new().dimmed())
            )
        })
        .collect()
}

/// Copy `from` to `to` a line at a time, each under `prefix`; a trailing
/// partial line gets its newline. Bytes pass through untouched, so a coloured
/// member keeps its escapes.
fn pump(mut from: impl BufRead, prefix: &str, to: &mut impl Write) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if from.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let mut out = Vec::with_capacity(prefix.len() + line.len() + 1);
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(&line);
        if out.last() != Some(&b'\n') {
            out.push(b'\n');
        }
        to.write_all(&out)?;
    }
}

/// The run's outcome from every member's exit status: the first failure names
/// its member.
fn verdict(results: &[(&str, ExitStatus)]) -> miette::Result<()> {
    for (namespace, status) in results {
        if status.success() {
            continue;
        }
        return Err(match status.code() {
            Some(code) => miette::miette!("member `{namespace}` exited with status {code}"),
            None => {
                use std::os::unix::process::ExitStatusExt;
                miette::miette!(
                    "member `{namespace}` was killed by signal {}",
                    status.signal().unwrap_or_default()
                )
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn argv(overrides: &Overrides) -> Vec<String> {
        member_argv(Path::new("/tmp/fsw.bundle"), "fsw", overrides)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    /// The renderer's pin: a deploy unit's `ExecStart` is this list, so a
    /// change here is a change to every deployed host.
    #[test]
    fn member_argv_is_the_leaf_contract() {
        assert_eq!(
            argv(&Overrides::default()),
            [
                "run",
                "/tmp/fsw.bundle",
                "--target",
                "fsw",
                "--no-preflight"
            ]
        );
        assert_eq!(
            argv(&Overrides {
                wall: true,
                cycle_rate: Some(120.0),
                cycles: Some(600),
                ..Overrides::default()
            }),
            [
                "run",
                "/tmp/fsw.bundle",
                "--target",
                "fsw",
                "--no-preflight",
                "--wall",
                "--cycle-rate",
                "120",
                "--cycles",
                "600"
            ]
        );
        let simulated = argv(&Overrides {
            sim_dt: Some(0.008_333),
            ..Overrides::default()
        });
        assert!(simulated.ends_with(&["--sim-dt".into(), "0.008333".into()]));
        assert!(!simulated.contains(&"--wall".to_string()));
    }

    /// A plan is a zip: envelope order, one bundle each.
    #[test]
    fn plan_pairs_members_with_bundles_in_order() {
        use crate::wiring::{ClockSpec, WiringBuilder};

        let member = |ns: &str| {
            let mut wiring = WiringBuilder::new()
                .coordinator(100.0, ClockSpec::Wall)
                .build();
            wiring.coordinator.namespace = Some(ns.to_string());
            wiring
        };
        let dir = Path::new("/tmp/run");
        let plan = plan(
            &[member("plant"), member("fsw")],
            &[dir.join("plant.bundle"), dir.join("fsw.bundle")],
        );
        let pairs: Vec<(&str, PathBuf)> = plan
            .iter()
            .map(|m| (m.namespace.as_str(), m.bundle.clone()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("plant", dir.join("plant.bundle")),
                ("fsw", dir.join("fsw.bundle")),
            ]
        );
    }

    #[test]
    fn overrides_from_args() {
        let args = RunArgs {
            paths: vec![PathBuf::from("target.py")],
            target: None,
            no_build: false,
            release: false,
            cargo_arg: Vec::new(),
            no_manifest_sidecar: false,
            wall: false,
            sim_dt: Some(0.01),
            cycle_rate: Some(100.0),
            cycles: Some(20),
            no_preflight: false,
            serve: None,
            peer: Vec::new(),
        };
        assert_eq!(
            overrides(&args),
            Overrides {
                wall: false,
                sim_dt: Some(0.01),
                cycle_rate: Some(100.0),
                cycles: Some(20),
            }
        );
    }

    /// Every prefix is one width, and the colours cycle in envelope order.
    #[test]
    fn prefixes_align_and_cycle() {
        let member = |ns: &str| Member {
            bundle: PathBuf::from("/tmp/x.bundle"),
            namespace: ns.to_string(),
        };
        let members = [member("plant"), member("fsw")];

        assert_eq!(prefixes(&members, false), ["plant │ ", "fsw   │ "]);

        let coloured = prefixes(&members, true);
        assert!(coloured.iter().all(|p| p.contains('\x1b')), "{coloured:?}");
        assert_ne!(
            coloured[0].split('m').next(),
            coloured[1].split('m').next(),
            "each member gets its own colour"
        );
    }

    /// The pump writes whole lines, prefixed, escapes and all.
    #[test]
    fn pump_prefixes_every_line() {
        let run = |input: &str| {
            let mut out = Vec::new();
            pump(std::io::Cursor::new(input.to_string()), "P ", &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(run("a\nb"), "P a\nP b\n");
        assert_eq!(run("\x1b[31mred\n"), "P \x1b[31mred\n");
        assert_eq!(run(""), "");
    }

    /// The verdict is the first failure, by status or by signal.
    #[cfg(unix)]
    #[test]
    fn verdict_names_the_first_failure() {
        use std::os::unix::process::ExitStatusExt;

        let status = ExitStatus::from_raw;
        assert!(verdict(&[("plant", status(0)), ("fsw", status(0))]).is_ok());
        assert_eq!(
            verdict(&[("plant", status(0)), ("fsw", status(1 << 8))])
                .unwrap_err()
                .to_string(),
            "member `fsw` exited with status 1"
        );
        assert_eq!(
            verdict(&[("plant", status(9)), ("fsw", status(1 << 8))])
                .unwrap_err()
                .to_string(),
            "member `plant` was killed by signal 9"
        );
    }
}
