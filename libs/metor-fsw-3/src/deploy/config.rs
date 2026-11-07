//! The deployment config file.
//!
//! A deployment's Python `emit()` writes this file and `metor run` reads it.
//! One file describes every target in the deployment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::coordinator::{CoordinatorConfig, OutputSource, SystemConfig};
use crate::dl::PackRef;
use crate::io::Backing;

use super::DeployError;

/// The config file format version this build reads.
pub const CONFIG_VERSION: u32 = 2;

/// An error reading a config file or picking a target from it.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ConfigError {
    #[error("the config is version {found}, this host reads {expected}")]
    #[diagnostic(
        code(M0204),
        help("install the metor-config that ships with this metor")
    )]
    Version { found: u32, expected: u32 },
    #[error("the config did not decode: {0}")]
    #[diagnostic(code(M0205))]
    Decode(#[from] serde_json::Error),
    #[error("no target is named `{name}` or sits under it")]
    #[diagnostic(code(M0102), help("{}", crate::cli::diagnostic::suggest(.name, .known, crate::cli::diagnostic::SHOW_TARGETS)))]
    NoSuchTarget { name: String, known: Vec<String> },
    #[error("`--except` leaves out every target `--target` selects")]
    #[diagnostic(
        code(M0109),
        help("drop an `--except`, or select more with `--target`")
    )]
    NothingSelected,
}

/// A port on a link of another target in the same deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPort {
    pub target: String,
    pub link: String,
    pub port: String,
}

/// A link on another target in the same deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerLink {
    pub target: String,
    pub link: String,
}

/// Where a deployment's config-defined output gets its definition from: a
/// runtime source, or a peer's port that `metor build` resolves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploySource {
    Peer(PeerPort),
    #[serde(untagged)]
    Runtime(OutputSource),
}

/// A resource as a deployment writes it: a runtime backing, or a dialer of
/// another link that `metor build` resolves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployBacking {
    Peer(PeerLink),
    #[serde(untagged)]
    Runtime(Backing),
}

/// A system as a deployment writes it.
pub type DeploySystem = SystemConfig<DeploySource>;

/// A deployment whose every target can be built on its own.
pub type ResolvedDeployment = DeploymentConfig<SystemConfig, Backing>;

/// The config for one target, whose systems are `S` and whose resources are
/// `R`.
///
/// `packs` lists the packs that provide the target's system types, and
/// `coordinator` describes the systems and how they connect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig<S = SystemConfig, R = Backing> {
    pub packs: Vec<PackRef>,
    pub coordinator: CoordinatorConfig<S, R>,
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            packs: Vec::new(),
            coordinator: CoordinatorConfig::default(),
        }
    }
}

impl<S, R> TargetConfig<S, R> {
    /// Returns the target's namespace, which is the name used to select it.
    pub fn namespace(&self) -> &str {
        self.coordinator.namespace.as_deref().unwrap_or("")
    }
}

/// A deployment: every target, and the host each one runs on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeploymentConfig<S = DeploySystem, R = DeployBacking> {
    pub config_version: u32,
    /// Maps a target's namespace to the host its peers dial. A target that is
    /// not listed is dialed on loopback.
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    pub targets: Vec<TargetConfig<S, R>>,
}

impl DeploymentConfig {
    /// Parses a config file.
    ///
    /// The version is checked first, so an old or new file reports a version
    /// mismatch rather than a confusing decode error.
    pub fn from_slice(bytes: &[u8]) -> Result<DeploymentConfig, ConfigError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let found = value["config_version"].as_u64().unwrap_or(0) as u32;
        if found != CONFIG_VERSION {
            return Err(ConfigError::Version {
                found,
                expected: CONFIG_VERSION,
            });
        }
        Ok(serde_json::from_value(value)?)
    }

    /// Returns the runtime form of this deployment.
    ///
    /// Fails if an output still names a peer's port, or a resource still
    /// dials a peer.
    pub fn into_runtime(self) -> Result<ResolvedDeployment, DeployError> {
        let targets = self
            .targets
            .into_iter()
            .map(TargetConfig::into_runtime)
            .collect::<Result<_, _>>()?;
        Ok(ResolvedDeployment {
            config_version: self.config_version,
            hosts: self.hosts,
            targets,
        })
    }
}

impl TargetConfig<DeploySystem, DeployBacking> {
    fn into_runtime(self) -> Result<TargetConfig, DeployError> {
        let namespace = self.namespace().to_string();
        let runtime = |system: DeploySystem| {
            let id = system.id.clone();
            let location = system.location.clone();
            system.try_map_outputs(|port, source| match source {
                DeploySource::Runtime(source) => Ok(source),
                DeploySource::Peer(_) => Err(DeployError::Unresolved {
                    system: format!("{namespace}.{id}"),
                    port: port.to_string(),
                }
                .at(location.as_ref())),
            })
        };
        let backing = |id: &str, resource| match resource {
            DeployBacking::Runtime(backing) => Ok(backing),
            DeployBacking::Peer(_) => Err(DeployError::UnresolvedDial {
                resource: format!("{namespace}.{id}"),
            }),
        };
        Ok(TargetConfig {
            packs: self.packs,
            coordinator: self.coordinator.try_map(runtime, backing)?,
        })
    }
}

/// Whether `namespace` is `pattern` or sits under it.
fn under(namespace: &str, pattern: &str) -> bool {
    namespace
        .strip_prefix(pattern)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

impl<S, R> DeploymentConfig<S, R> {
    /// Returns the targets `include` selects, less those `exclude` selects,
    /// in file order.
    ///
    /// A pattern selects each target whose namespace is the pattern or sits
    /// under it, as `flight.cpu1` sits under `flight`. With no `include`,
    /// every target is selected.
    ///
    /// # Errors
    /// Returns an error for a pattern that selects no target, or if nothing
    /// is left.
    pub fn select(
        &self,
        include: &[String],
        exclude: &[String],
    ) -> Result<Vec<usize>, ConfigError> {
        let namespaces = self.namespaces();
        if let Some(name) = include
            .iter()
            .chain(exclude)
            .find(|pattern| !namespaces.iter().any(|ns| under(ns, pattern)))
        {
            return Err(ConfigError::NoSuchTarget {
                name: name.clone(),
                known: namespaces.into_iter().map(String::from).collect(),
            });
        }
        let picked: Vec<usize> = (0..namespaces.len())
            .filter(|&i| include.is_empty() || include.iter().any(|p| under(namespaces[i], p)))
            .filter(|&i| !exclude.iter().any(|p| under(namespaces[i], p)))
            .collect();
        match picked.is_empty() {
            true => Err(ConfigError::NothingSelected),
            false => Ok(picked),
        }
    }

    /// Returns every group path above a target, as `flight` above
    /// `flight.cpu1`, in the order first seen.
    pub fn groups(&self) -> Vec<&str> {
        let mut groups: Vec<&str> = Vec::new();
        for namespace in self.namespaces() {
            let dots = namespace.match_indices('.').map(|(at, _)| &namespace[..at]);
            for group in dots {
                if !groups.contains(&group) {
                    groups.push(group);
                }
            }
        }
        groups
    }

    /// Returns every target's namespace, in file order.
    pub fn namespaces(&self) -> Vec<&str> {
        self.targets.iter().map(TargetConfig::namespace).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::coordinator::{BuildError, SystemTable};
    use crate::{Input, JsonSchema, Output, Record, system};

    /// The record type for every port in these tests. The build only checks
    /// that the ids on each edge match, so one record type is enough.
    #[derive(Record, crate::Schema, serde::Serialize, Deserialize, Clone, Copy, Debug)]
    #[record(max_len = 8)]
    struct Ping {
        n: u32,
    }

    #[derive(Deserialize, JsonSchema)]
    struct Altitude {
        #[allow(dead_code)]
        altitude: f64,
    }

    #[derive(Deserialize, JsonSchema)]
    struct Gain {
        #[allow(dead_code)]
        gain: f64,
    }

    #[system]
    fn plant(motor_cmd: &mut Input<Ping>, imu: &mut Output<Ping>) {
        let _ = (motor_cmd, imu);
    }

    #[system]
    fn nav(imu: &mut Input<Ping>, est: &mut Output<Ping>) {
        let _ = (imu, est);
    }

    #[system]
    fn mode(cmd: &mut Output<Ping>) {
        let _ = cmd;
    }

    #[system]
    fn ctrl(est: &mut Input<Ping>, mode: &mut Input<Ping>, motor_cmd: &mut Output<Ping>) {
        let _ = (est, mode, motor_cmd);
    }

    /// Registers the four `adcs.*` types the golden file uses, plus the
    /// built-in links.
    fn table() -> SystemTable {
        let mut table = SystemTable::new();
        let mut register = || -> Result<(), BuildError> {
            crate::builtins::register_builtins(&mut table)?;
            table.register("adcs.plant", plant(), |_: Altitude| ())?;
            table.register("adcs.nav", nav(), |_: Gain| ())?;
            table.register("adcs.mode", mode(), Default::default)?;
            table.register("adcs.ctrl", ctrl(), Default::default)?;
            Ok(())
        };
        register().expect("valid records");
        table
    }

    /// Loads the golden file: the design doc's target, emitted by Python as a
    /// one-target deployment.
    fn golden() -> DeploymentConfig {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/target.json");
        let bytes = std::fs::read(path).expect("the golden exists");
        DeploymentConfig::from_slice(&bytes).expect("the golden decodes")
    }

    fn member(namespace: &str) -> TargetConfig {
        TargetConfig {
            packs: Vec::new(),
            coordinator: CoordinatorConfig {
                namespace: Some(namespace.into()),
                ..Default::default()
            },
        }
    }

    fn envelope(targets: Vec<TargetConfig>) -> ResolvedDeployment {
        ResolvedDeployment {
            config_version: CONFIG_VERSION,
            hosts: BTreeMap::new(),
            targets,
        }
    }

    #[test]
    fn test_golden_builds() {
        let config = golden();
        assert_eq!(config.namespaces(), ["cube_sat"]);
        let index = config.select(&[], &[]).expect("one target")[0];
        let config = config.into_runtime().expect("no peers");
        let coordinator = config.targets[index].coordinator.clone().build(&table());
        assert!(coordinator.is_ok(), "{:?}", coordinator.err());
    }

    fn select(
        config: &ResolvedDeployment,
        include: &[&str],
        exclude: &[&str],
    ) -> Result<Vec<usize>, ConfigError> {
        let owned = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        config.select(&owned(include), &owned(exclude))
    }

    #[test]
    fn test_select_by_namespace_and_prefix() {
        let config = envelope(vec![
            member("flight.a"),
            member("flight.b"),
            member("ground"),
        ]);
        assert_eq!(select(&config, &[], &[]).ok(), Some(vec![0, 1, 2]));
        assert_eq!(select(&config, &["flight"], &[]).ok(), Some(vec![0, 1]));
        assert_eq!(select(&config, &["flight.b"], &[]).ok(), Some(vec![1]));
        assert_eq!(
            select(&config, &["ground", "flight.a"], &[]).ok(),
            Some(vec![0, 2])
        );
        assert_eq!(select(&config, &[], &["flight.a"]).ok(), Some(vec![1, 2]));
        assert_eq!(
            select(&config, &["flight"], &["flight.b"]).ok(),
            Some(vec![0])
        );
    }

    #[test]
    fn test_select_matches_whole_names() {
        let config = envelope(vec![member("flight"), member("flightx.a")]);
        assert_eq!(select(&config, &["flight"], &[]).ok(), Some(vec![0]));
        assert!(matches!(
            select(&config, &["fli"], &[]),
            Err(ConfigError::NoSuchTarget { name, .. }) if name == "fli"
        ));
    }

    #[test]
    fn test_groups_lists_each_path_above_a_target() {
        let config = envelope(vec![
            member("flight.s1.cpu"),
            member("flight.s1.gw"),
            member("gse"),
        ]);
        assert_eq!(config.groups(), ["flight", "flight.s1"]);
        assert!(envelope(vec![member("a"), member("b")]).groups().is_empty());
    }

    #[test]
    fn test_select_refuses_unknown_and_empty() {
        let config = envelope(vec![member("a"), member("b")]);
        assert!(matches!(
            select(&config, &[], &["c"]),
            Err(ConfigError::NoSuchTarget { name, .. }) if name == "c"
        ));
        assert!(matches!(
            select(&config, &["a"], &["a"]),
            Err(ConfigError::NothingSelected)
        ));
    }

    #[test]
    fn test_version_error_reports_versions() {
        for version in [0, 1, 3] {
            let bytes = format!(r#"{{"config_version":{version},"targets":[]}}"#);
            let Err(ConfigError::Version { found, expected }) =
                DeploymentConfig::from_slice(bytes.as_bytes())
            else {
                panic!("version {version} is rejected")
            };
            assert_eq!((found, expected), (version, CONFIG_VERSION));
        }
    }

    #[test]
    fn test_config_requires_packs() {
        let bytes =
            br#"{"config_version":2,"targets":[{"coordinator":{"clock":{"Wall":{"rate":1.0}},
            "ring_depth":8,"systems":[]}}]}"#;
        assert!(matches!(
            DeploymentConfig::from_slice(bytes),
            Err(ConfigError::Decode(_))
        ));
        assert!(matches!(
            DeploymentConfig::from_slice(b"not json"),
            Err(ConfigError::Decode(_))
        ));
    }
}
