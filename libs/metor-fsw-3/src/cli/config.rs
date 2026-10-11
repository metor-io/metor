//! The config file a target's `emit()` writes and `metor run` reads.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::coordinator::CoordinatorConfig;

/// The file format this host reads.
pub const CONFIG_VERSION: u32 = 1;

/// A `ConfigError` is why a config file is not this host's.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the config is version {found}, this host reads {expected}")]
    Version { found: u32, expected: u32 },
    #[error("the config did not decode: {0}")]
    Decode(#[from] serde_json::Error),
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub config_version: u32,
    pub packs: Vec<PackRef>,
    pub coordinator: CoordinatorConfig,
}

impl TargetConfig {
    /// Reads a config file, checking its version before the rest of the shape.
    pub fn from_slice(bytes: &[u8]) -> Result<TargetConfig, ConfigError> {
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
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::coordinator::SystemTable;
    use crate::{Input, JsonSchema, Output, Record, system};

    /// One record for every port here: the build only compares ids across an
    /// edge, and every edge in the golden joins ports of one record.
    #[derive(Record, serde::Serialize, Deserialize, Clone, Copy, Debug)]
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

    /// The four `adcs.*` types the golden names, with the golden's port names.
    fn table() -> SystemTable {
        let mut table = SystemTable::new();
        table.register("adcs.plant", |_: Altitude| Plant);
        table.register("adcs.nav", |_: Gain| Nav);
        table.register("adcs.mode", || Mode);
        table.register("adcs.ctrl", || Ctrl);
        table
    }

    const GOLDEN: &[u8] = include_bytes!("../../tests/golden/target.json");

    #[test]
    fn the_golden_target_decodes_and_builds() {
        let config = TargetConfig::from_slice(GOLDEN).expect("the golden decodes");
        assert_eq!(config.config_version, CONFIG_VERSION);
        assert_eq!(
            config.packs,
            vec![PackRef {
                id: "adcs".into(),
                lib: "adcs_systems".into(),
                libs: PathBuf::from("/abs/.metor/adcs_pack/_libs"),
            }]
        );
        let coordinator = config
            .coordinator
            .build(&table())
            .expect("the golden's graph builds");
        assert_eq!(
            coordinator.entry_names().collect::<Vec<_>>(),
            vec!["plant", "nav", "mode", "ctrl"]
        );
    }

    #[test]
    fn an_older_version_is_reported_with_both_numbers() {
        let bytes = br#"{"config_version":0,"packs":[],"coordinator":{}}"#;
        let Err(ConfigError::Version { found, expected }) = TargetConfig::from_slice(bytes) else {
            panic!("version 0 is rejected")
        };
        assert_eq!((found, expected), (0, CONFIG_VERSION));
    }

    #[test]
    fn a_config_without_packs_is_a_decode_error() {
        let bytes = br#"{"config_version":1,"coordinator":{"clock":{"Wall":{"rate":1.0}},
            "ring_depth":8,"systems":[]}}"#;
        assert!(matches!(
            TargetConfig::from_slice(bytes),
            Err(ConfigError::Decode(_))
        ));
        assert!(matches!(
            TargetConfig::from_slice(b"not json"),
            Err(ConfigError::Decode(_))
        ));
    }
}
