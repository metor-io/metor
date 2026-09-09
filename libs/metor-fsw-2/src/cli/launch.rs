//! Running the members of a deployment.
//!
//! Everything here rests on one contract: a member runs as
//! `metor-fsw run <bundle> --target <ns>` plus the clock overrides the
//! command line carried. [`member_argv`] is that argument list as data, so
//! whatever composes members — the launcher below, or a renderer that emits
//! a systemd unit — spawns the same process from the same function.

use std::ffi::OsString;
use std::path::Path;

use super::RunArgs;

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

/// How one member runs, anywhere: the leaf's argument list.
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

    #[test]
    fn overrides_from_args() {
        let args = RunArgs {
            path: Some(PathBuf::from("target.py")),
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
}
