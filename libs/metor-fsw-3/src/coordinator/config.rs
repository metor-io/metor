//! The config a coordinator is built from.
//!
//! It lists the systems to run and wires each system's inputs to other
//! systems' outputs.

use core::time::Duration;
use std::collections::BTreeMap;

use metor_proto::types::Timestamp;
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::io::ResourceConfig;
use crate::system::PortDef;

/// The default [`CoordinatorConfig::ring_depth`].
pub const DEFAULT_RING_DEPTH: usize = 8;

/// The configuration for a coordinator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// The clock that paces the cycles.
    pub clock: Clock,
    /// How many records a ring holds per unit of its port's declared depth.
    pub ring_depth: usize,
    /// A prefix added to every name the coordinator announces.
    #[serde(default)]
    pub namespace: Option<String>,
    pub systems: Vec<SystemConfig>,
    /// The resources systems are granted, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceConfig>,
    /// The seed every `Rng` parameter draws from.
    #[serde(default)]
    pub seed: u64,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            clock: Clock::Wall {
                rate: WallRate(100.0),
            },
            ring_depth: DEFAULT_RING_DEPTH,
            namespace: None,
            systems: Vec::new(),
            resources: Vec::new(),
            seed: 0,
        }
    }
}

/// Configures one system instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemConfig {
    pub id: String,
    pub ty: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub inputs: Vec<InputConfig>,
    #[serde(default)]
    pub outputs: Vec<OutputConfig>,
    /// Gives `Cap<K>` parameters real resources, by parameter name. A sim
    /// resource names its holders itself.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "unique_keys"
    )]
    pub grants: BTreeMap<String, String>,
    /// The adapter the system runs behind, if any. See [`Adapter`].
    #[serde(flatten, with = "adapter_keys")]
    pub adapter: Option<Adapter>,
}

/// Deserializes a map, rejecting a key that appears twice, which serde would
/// otherwise let the last one win.
fn unique_keys<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    struct Unique;

    impl<'de> Visitor<'de> for Unique {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            f.write_str("a map from parameter to resource")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if out.contains_key(&key) {
                    return Err(A::Error::custom(format!("`{key}` is granted twice")));
                }
                out.insert(key, value);
            }
            Ok(out)
        }
    }

    d.deserialize_map(Unique)
}

/// Changes how a system is run.
///
/// [`Adapter::Thread`] runs it on a named background thread.
///
/// In a config, the adapter is written as a `thread` key next to `id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Thread(String),
}

/// Serializes an [`Adapter`] as a `thread` key.
mod adapter_keys {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Adapter;

    #[derive(Default, Deserialize)]
    struct Keys {
        #[serde(default)]
        thread: Option<String>,
    }

    #[derive(Default, Serialize)]
    struct KeysRef<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        thread: Option<&'a str>,
    }

    pub fn serialize<S: Serializer>(adapter: &Option<Adapter>, s: S) -> Result<S::Ok, S::Error> {
        let keys = match adapter {
            Some(Adapter::Thread(thread)) => KeysRef {
                thread: Some(thread),
            },
            None => KeysRef::default(),
        };
        keys.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Adapter>, D::Error> {
        match Keys::deserialize(d)? {
            Keys {
                thread: Some(thread),
            } => Ok(Some(Adapter::Thread(thread))),
            Keys { .. } => Ok(None),
        }
    }
}

impl SystemConfig {
    /// Returns a system with no params and no inputs.
    pub fn new(id: impl Into<String>, ty: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ty: ty.into(),
            params: serde_json::Value::Null,
            inputs: Vec::new(),
            outputs: Vec::new(),
            grants: BTreeMap::new(),
            adapter: None,
        }
    }
}

/// An output port declared by the config instead of the system type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub port: String,
    #[serde(flatten)]
    pub source: OutputSource,
}

impl OutputConfig {
    /// Creates an output port that carries the record called `record`.
    pub fn with_record(port: impl Into<String>, record: impl Into<String>) -> Self {
        Self {
            port: port.into(),
            source: OutputSource::Record(record.into()),
        }
    }
}

/// Where a config-defined output gets its definition from.
///
/// `Record` refers to a record registered in the system table. `Def` is a
/// definition given in full.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSource {
    Record(String),
    Def(PortDef),
}

/// An input port and the outputs that feed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputConfig {
    pub port: String,
    pub from: Vec<PortRef>,
}

/// A reference to an output port of another system.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortRef {
    pub system: String,
    pub port: String,
}

/// The clock that paces the coordinator's cycles.
///
/// `Wall` runs `rate` cycles per second of real time. `Simulated` advances time
/// by `dt` each cycle, as fast as the systems run, starting at `epoch`, or at
/// the time of the build without one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Clock {
    Wall {
        rate: WallRate,
    },
    Simulated {
        dt: Duration,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<Timestamp>,
    },
}

/// A wall clock's rate in cycles per second: finite and at least
/// [`WallRate::MIN`].
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct WallRate(f64);

impl WallRate {
    pub const MIN: f64 = 0.001;

    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for WallRate {
    type Error = InvalidWallRate;

    fn try_from(rate: f64) -> Result<Self, InvalidWallRate> {
        match rate.is_finite() && rate >= Self::MIN {
            true => Ok(Self(rate)),
            false => Err(InvalidWallRate(rate)),
        }
    }
}

impl From<WallRate> for f64 {
    fn from(rate: WallRate) -> f64 {
        rate.0
    }
}

/// A wall clock rate that is not finite or is below [`WallRate::MIN`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("wall clock rate {0} must be finite and at least 0.001 Hz")]
pub struct InvalidWallRate(pub f64);

/// Choices one build makes that the target's config does not.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    /// Turn every thread group on the coordinator thread, in step order.
    pub deterministic: bool,
}

impl InputConfig {
    /// Creates an input port fed by the output `output` of system `system`.
    pub fn new(
        port: impl Into<String>,
        system: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self {
            port: port.into(),
            from: vec![PortRef::new(system, output)],
        }
    }
}

impl PortRef {
    pub fn new(system: impl Into<String>, port: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            port: port.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_defaults_and_round_trip() {
        let bare: SystemConfig =
            serde_json::from_str(r#"{"id":"nav","ty":"nav"}"#).expect("params optional");
        assert_eq!(bare, SystemConfig::new("nav", "nav"));
        let full = SystemConfig {
            params: serde_json::json!({ "gain": 2.5 }),
            ..SystemConfig::new("nav", "nav")
        };
        let text = serde_json::to_string(&full).expect("serializable");
        assert_eq!(
            serde_json::from_str::<SystemConfig>(&text).expect("round trips"),
            full
        );
    }

    #[test]
    fn test_grants_default_and_round_trip() {
        let bare = SystemConfig::new("gps", "gps");
        let text = serde_json::to_string(&bare).expect("serializable");
        assert!(!text.contains("grants"));
        let granted = SystemConfig {
            grants: BTreeMap::from([("uart".into(), "gps1".into())]),
            ..bare
        };
        let text = serde_json::to_string(&granted).expect("serializable");
        assert_eq!(
            serde_json::from_str::<SystemConfig>(&text).expect("round trips"),
            granted
        );
    }

    #[test]
    fn test_grant_needs_a_resource() {
        let text = r#"{"id":"gps","ty":"gps","grants":{"uart":5}}"#;
        assert!(serde_json::from_str::<SystemConfig>(text).is_err());
    }

    #[test]
    fn test_reject_a_parameter_granted_twice() {
        let text = r#"{"id":"gps","ty":"gps","grants":{"uart":"a","uart":"b"}}"#;
        let error = serde_json::from_str::<SystemConfig>(text).expect_err("a duplicate key");
        assert!(
            error.to_string().contains("`uart` is granted twice"),
            "{error}"
        );
    }

    fn adapter_of(text: &str) -> Result<Option<Adapter>, serde_json::Error> {
        serde_json::from_str::<SystemConfig>(text).map(|system| system.adapter)
    }

    #[test]
    fn test_adapter_forms_round_trip() {
        let thread = SystemConfig {
            adapter: Some(Adapter::Thread("gps".into())),
            ..SystemConfig::new("a", "t")
        };
        for system in [SystemConfig::new("a", "t"), thread] {
            let text = serde_json::to_string(&system).expect("serializable");
            assert_eq!(
                serde_json::from_str::<SystemConfig>(&text).expect("round trips"),
                system
            );
        }
        let text = serde_json::to_string(&SystemConfig::new("a", "t")).expect("serializable");
        assert!(!text.contains("thread"), "{text}");
    }

    #[test]
    fn test_adapter_keys_parse() {
        assert_eq!(adapter_of(r#"{"id":"a","ty":"t"}"#).unwrap(), None);
        assert_eq!(
            adapter_of(r#"{"id":"a","ty":"t","thread":"gps"}"#).unwrap(),
            Some(Adapter::Thread("gps".into()))
        );
    }

    #[test]
    fn test_reject_bad_adapter() {
        assert!(adapter_of(r#"{"id":"a","ty":"t","thread":5}"#).is_err());
    }

    #[test]
    fn test_output_source_forms() {
        let record: OutputConfig =
            serde_json::from_str(r#"{"port":"ping","record":"ping"}"#).expect("record form");
        assert_eq!(record, OutputConfig::with_record("ping", "ping"));
        assert!(serde_json::from_str::<OutputConfig>(r#"{"port":"x"}"#).is_err());
    }
}
