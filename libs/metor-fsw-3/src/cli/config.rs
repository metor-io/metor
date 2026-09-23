//! The config file a deployment's `emit()` writes and `metor run` reads: one
//! envelope holding every target.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::coordinator::CoordinatorConfig;

/// The file format this host reads.
pub const CONFIG_VERSION: u32 = 2;

/// A `ConfigError` is why a config file is not this host's, or names no target.
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

/// One pack the config's types come from: its id prefix, its cdylib stem, and
/// the per-triple directory its generated module located.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    pub id: String,
    pub lib: String,
    pub libs: PathBuf,
}

/// A whole target: the packs that supply the types and the graph over them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub packs: Vec<PackRef>,
    pub coordinator: CoordinatorConfig,
}

impl TargetConfig {
    /// The name a deployment selects this target by.
    pub fn namespace(&self) -> &str {
        self.coordinator.namespace.as_deref().unwrap_or("")
    }
}

/// Every target of a deployment, and the host each is dialed at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeploymentConfig {
    pub config_version: u32,
    /// A target's namespace to the address its peers dial; absent is loopback.
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    pub targets: Vec<TargetConfig>,
}

impl DeploymentConfig {
    /// Reads a config file, checking its version before the rest of the shape.
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

    /// The index of the target `namespace` names, or of the only target.
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

    /// Every target's namespace, in file order.
    pub fn namespaces(&self) -> Vec<&str> {
        self.targets.iter().map(TargetConfig::namespace).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::coordinator::SystemTable;
    use crate::{Input, JsonSchema, Output, Record, system};

    /// One record for every port here: the build only compares ids across an
    /// edge, and every edge in the golden joins ports of one record.
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

    struct Plant;

    #[system]
    impl Plant {
        fn execute(&mut self, motor_cmd: &mut Input<Ping>, imu: &mut Output<Ping>) {
            let _ = (motor_cmd, imu);
        }
    }

    struct Nav;

    #[system]
    impl Nav {
        fn execute(&mut self, imu: &mut Input<Ping>, est: &mut Output<Ping>) {
            let _ = (imu, est);
        }
    }

    struct Mode;

    #[system]
    impl Mode {
        fn execute(&mut self, cmd: &mut Output<Ping>) {
            let _ = cmd;
        }
    }

    struct Ctrl;

    #[system]
    impl Ctrl {
        fn execute(
            &mut self,
            est: &mut Input<Ping>,
            mode: &mut Input<Ping>,
            motor_cmd: &mut Output<Ping>,
        ) {
            let _ = (est, mode, motor_cmd);
        }
    }

    /// The four `adcs.*` types the golden names, plus the links it adds.
    fn table() -> SystemTable {
        let mut table = SystemTable::new();
        crate::link::register_builtins(&mut table).expect("valid records");
        table
            .register("adcs.plant", |_: Altitude| Plant)
            .expect("valid records");
        table
            .register("adcs.nav", |_: Gain| Nav)
            .expect("valid records");
        table.register("adcs.mode", || Mode).expect("valid records");
        table.register("adcs.ctrl", || Ctrl).expect("valid records");
        table
    }

    /// The Python golden: the design doc's target, emitted as a deployment of one.
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

    fn envelope(targets: Vec<TargetConfig>) -> DeploymentConfig {
        DeploymentConfig {
            config_version: CONFIG_VERSION,
            hosts: BTreeMap::new(),
            targets,
        }
    }

    #[test]
    fn test_golden_builds() {
        let config = golden();
        assert_eq!(config.namespaces(), ["cube_sat"]);
        let target = &config.targets[config.target(None).expect("one target")];
        let coordinator = target.coordinator.clone().build(&table());
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

    #[test]
    fn test_hosts_default_empty() {
        let bytes = br#"{"config_version":2,"targets":[]}"#;
        let config = DeploymentConfig::from_slice(bytes).expect("decodes");
        assert!(config.hosts.is_empty() && config.targets.is_empty());
    }
}
