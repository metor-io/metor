//! The deployment config file.
//!
//! A deployment's Python `emit()` writes this file and `metor run` reads it.
//! One file describes every target in the deployment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::coordinator::{CoordinatorConfig, OutputSource, SystemConfig};
use crate::dl::PackRef;

use super::DeployError;

/// The config file format version this build reads.
pub const CONFIG_VERSION: u32 = 2;

/// An error reading a config file or picking a target from it.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the config is version {found}, this host reads {expected}")]
    Version { found: u32, expected: u32 },
    #[error("the config did not decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("the deployment has several targets; pick one with --target: {0}")]
    SelectTarget(String),
    #[error("no target is named `{0}`")]
    NoSuchTarget(String),
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

/// A system as a deployment writes it.
pub type DeploySystem = SystemConfig<DeploySource>;

/// A deployment whose every target can be built on its own.
pub type ResolvedDeployment = DeploymentConfig<SystemConfig>;

/// The config for one target, whose systems are `S`.
///
/// `packs` lists the packs that provide the target's system types, and
/// `coordinator` describes the systems and how they connect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig<S = SystemConfig> {
    pub packs: Vec<PackRef>,
    pub coordinator: CoordinatorConfig<S>,
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            packs: Vec::new(),
            coordinator: CoordinatorConfig::default(),
        }
    }
}

impl<S> TargetConfig<S> {
    /// Returns the target's namespace, which is the name used to select it.
    pub fn namespace(&self) -> &str {
        self.coordinator.namespace.as_deref().unwrap_or("")
    }
}

/// A deployment: every target, and the host each one runs on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeploymentConfig<S = DeploySystem> {
    pub config_version: u32,
    /// Maps a target's namespace to the host its peers dial. A target that is
    /// not listed is dialed on loopback.
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    pub targets: Vec<TargetConfig<S>>,
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
    /// Fails if an output still names a peer's port.
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

impl TargetConfig<DeploySystem> {
    fn into_runtime(self) -> Result<TargetConfig, DeployError> {
        let namespace = self.namespace().to_string();
        let runtime = |system: DeploySystem| {
            let id = system.id.clone();
            system.try_map_outputs(|port, source| match source {
                DeploySource::Runtime(source) => Ok(source),
                DeploySource::Peer(_) => Err(DeployError::Unresolved {
                    system: format!("{namespace}.{id}"),
                    port: port.to_string(),
                }),
            })
        };
        Ok(TargetConfig {
            packs: self.packs,
            coordinator: self.coordinator.try_map_systems(runtime)?,
        })
    }
}

impl<S> DeploymentConfig<S> {
    /// Returns the index of the target named `namespace`.
    ///
    /// With no namespace, this returns the only target, or an error if there
    /// are several.
    pub fn target(&self, namespace: Option<&str>) -> Result<usize, ConfigError> {
        match namespace {
            Some(name) => self
                .targets
                .iter()
                .position(|target| target.namespace() == name)
                .ok_or_else(|| ConfigError::NoSuchTarget(name.to_string())),
            None if self.targets.len() == 1 => Ok(0),
            None => Err(ConfigError::SelectTarget(self.namespaces().join(", "))),
        }
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
        let index = config.target(None).expect("one target");
        let config = config.into_runtime().expect("no peers");
        let coordinator = config.targets[index].coordinator.clone().build(&table());
        assert!(coordinator.is_ok(), "{:?}", coordinator.err());
    }

    #[test]
    fn test_select_target_by_namespace() {
        let config = envelope(vec![member("a"), member("b")]);
        assert_eq!(config.target(Some("b")).expect("named"), 1);
        assert!(
            matches!(config.target(Some("c")), Err(ConfigError::NoSuchTarget(name)) if name == "c")
        );
        let Err(ConfigError::SelectTarget(names)) = config.target(None) else {
            panic!("two targets need a name")
        };
        assert_eq!(names, "a, b");
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
