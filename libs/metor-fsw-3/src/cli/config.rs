//! The config file a deployment's `emit()` writes and `metor run` reads: one
//! envelope holding every target.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::coordinator::CoordinatorConfig;
use crate::dl::{DlPack, Pack, PackError};
use crate::wasm::{DEFAULT_FUEL, DEFAULT_MEMORY, DEFAULT_SETUP_FUEL, Fuel, WasmPack};

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

/// How a pack is built and loaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackKind {
    /// A shared library for this host.
    #[default]
    Dylib,
    /// A WebAssembly module, each system instance in a guest of its own.
    Wasm,
}

impl PackKind {
    fn is_dylib(&self) -> bool {
        *self == PackKind::Dylib
    }
}

/// One pack the config's types come from: its id prefix, its artifact stem,
/// the per-triple directory its generated module located, and its kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    pub id: String,
    pub lib: String,
    pub libs: PathBuf,
    #[serde(default, skip_serializing_if = "PackKind::is_dylib")]
    pub kind: PackKind,
    /// Each guest's memory ceiling in bytes, for a wasm pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<usize>,
    /// Each step's fuel grant, for a wasm pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,
}

impl PackRef {
    /// A dylib pack's reference.
    pub fn dylib(id: String, lib: String, libs: PathBuf) -> Self {
        Self {
            id,
            lib,
            libs,
            kind: PackKind::Dylib,
            memory: None,
            fuel: None,
        }
    }

    /// The pack's library for this host, or its module.
    pub fn artifact(&self) -> PathBuf {
        use super::cargo::{WASM_TRIPLE, cdylib_name, triple, wasm_name};
        match self.kind {
            PackKind::Dylib => self.libs.join(triple()).join(cdylib_name(&self.lib)),
            PackKind::Wasm => self.libs.join(WASM_TRIPLE).join(wasm_name(&self.lib)),
        }
    }

    /// Each guest's memory ceiling, the default when unset.
    pub fn memory(&self) -> usize {
        self.memory.unwrap_or(DEFAULT_MEMORY)
    }

    /// The grants a wasm pack's calls run under, the defaults when unset.
    pub fn fuel(&self) -> Fuel {
        Fuel {
            step: self.fuel.unwrap_or(DEFAULT_FUEL),
            setup: DEFAULT_SETUP_FUEL,
        }
    }

    /// Opens the artifact as its kind says.
    pub fn open(&self) -> Result<Pack, PackError> {
        let path = self.artifact();
        match self.kind {
            // SAFETY: a pack reference names a library `pack dev` built
            // against this ABI; `open` checks the version before calling
            // anything else.
            PackKind::Dylib => unsafe { DlPack::open(&path) }.map(Pack::Dl),
            PackKind::Wasm => {
                let bytes = std::fs::read(&path).map_err(PackError::Read)?;
                WasmPack::open(&bytes, self.memory(), self.fuel()).map(Pack::Wasm)
            }
        }
    }
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
    use crate::coordinator::{BuildError, SystemTable};
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

    /// The four `adcs.*` types the golden names, plus the links it adds.
    fn table() -> SystemTable {
        let mut table = SystemTable::new();
        let mut register = || -> Result<(), BuildError> {
            crate::link::register_builtins(&mut table)?;
            table.register("adcs.plant", plant(), |_: Altitude| ())?;
            table.register("adcs.nav", nav(), |_: Gain| ())?;
            table.register("adcs.mode", mode(), Default::default)?;
            table.register("adcs.ctrl", ctrl(), Default::default)?;
            Ok(())
        };
        register().expect("valid records");
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
    fn test_wasm_pack_ref_round_trips() {
        let json = serde_json::json!({
            "id": "adcs", "lib": "adcs_systems", "libs": "/l",
            "kind": "wasm", "memory": 1 << 20, "fuel": 5_000,
        });
        let pack: PackRef = serde_json::from_value(json.clone()).expect("decodes");
        assert_eq!(pack.kind, PackKind::Wasm);
        assert_eq!((pack.memory(), pack.fuel().step), (1 << 20, 5_000));
        assert_eq!(serde_json::to_value(&pack).expect("encodes"), json);
        let module = PathBuf::from("/l/wasm32-unknown-unknown/adcs_systems.wasm");
        assert_eq!(pack.artifact(), module);
    }

    #[test]
    fn test_pack_kind_defaults_to_dylib() {
        let json = serde_json::json!({ "id": "adcs", "lib": "adcs_systems", "libs": "/l" });
        let pack: PackRef = serde_json::from_value(json.clone()).expect("decodes");
        assert_eq!(pack.kind, PackKind::Dylib);
        assert_eq!((pack.memory, pack.fuel), (None, None));
        assert_eq!(pack.memory(), crate::wasm::DEFAULT_MEMORY);
        assert_eq!(pack.fuel().step, crate::wasm::DEFAULT_FUEL);
        let written = serde_json::to_value(&pack).expect("encodes");
        assert_eq!(written, json);
    }

    #[test]
    fn test_unknown_pack_kind_is_refused() {
        let json = serde_json::json!({ "id": "a", "lib": "a", "libs": "/l", "kind": "elf" });
        assert!(serde_json::from_value::<PackRef>(json).is_err());
    }

    #[test]
    fn test_missing_module_is_read_error() {
        let pack = PackRef {
            kind: PackKind::Wasm,
            ..PackRef::dylib("a".into(), "a".into(), "/no/such/dir".into())
        };
        assert!(matches!(pack.open(), Err(PackError::Read(_))));
    }
}
