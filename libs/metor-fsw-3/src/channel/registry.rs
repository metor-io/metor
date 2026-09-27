//! `ChannelRegistry`: publishes the target's channels as one `SequenceRegistry`.

use metor_proto_wkt::{SequenceChannelSpec, SequenceRegistry};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::port::Output;

/// The registry a target publishes, as its config lists it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistryParams {
    pub channels: Vec<ChannelSpec>,
    /// Cycles between two writes.
    #[serde(default = "default_period")]
    pub period: u64,
}

/// One channel and the names a `Load` on it may name.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelSpec {
    pub name: String,
    #[serde(default)]
    pub available: Vec<String>,
}

/// Cycles between two writes when the params name no period.
fn default_period() -> u64 {
    100
}

/// A `Registry` holds the registry it writes and when it next writes it.
pub struct Registry {
    registry: SequenceRegistry,
    period: u64,
    until: u64,
}

impl Registry {
    /// Refuses a zero period.
    pub fn new(params: RegistryParams) -> Result<Self, ParamError> {
        if params.period == 0 {
            return Err(ParamError::Decode(
                "period must be at least one cycle".into(),
            ));
        }
        let registry = SequenceRegistry {
            channels: params
                .channels
                .into_iter()
                .map(|spec| SequenceChannelSpec {
                    name: spec.name,
                    available: spec.available,
                })
                .collect(),
        };
        Ok(Self {
            registry,
            period: params.period,
            until: 0,
        })
    }
}

/// Writes the registry on the first cycle and once a period after.
#[crate::system]
pub fn channel_registry(#[state] s: &mut Registry, registry: &mut Output<SequenceRegistry>) {
    if s.until == 0 {
        let _ = registry.write(&s.registry);
        s.until = s.period;
    }
    s.until -= 1;
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use super::*;
    use crate::coordinator::Params;
    use crate::fn_system::Ctor;

    use crate::tests::utils::pair;

    fn make(params: serde_json::Value) -> Result<Registry, ParamError> {
        Ctor::<Registry, (RegistryParams, ParamError)>::make(&Registry::new, Params(&params))
    }

    #[test]
    fn test_writes_on_first_cycle_then_each_period() {
        let mut state = make(json!({
            "channels": [{ "name": "mode", "available": ["a", "b"] }, { "name": "arm" }],
            "period": 3
        }))
        .expect("valid params");
        let (_ring, mut output, mut input) = pair::<SequenceRegistry>(4);
        let mut written = Vec::new();
        for cycle in 0..7 {
            channel_registry()(&mut state, &mut output);
            if input.drain().filter_map(Result::ok).next().is_some() {
                written.push(cycle);
            }
        }
        assert_eq!(written, [0, 3, 6]);
    }

    #[test]
    fn test_registry_decodes_to_params() {
        let mut state = make(json!({ "channels": [{ "name": "mode", "available": ["a"] }] }))
            .expect("valid params");
        let (_ring, mut output, mut input) = pair::<SequenceRegistry>(4);
        channel_registry()(&mut state, &mut output);
        let registry = input.drain().next().expect("a record").expect("decodes");
        assert_eq!(registry.channels.len(), 1);
        assert_eq!(registry.channels[0].name, "mode");
        assert_eq!(registry.channels[0].available, ["a"]);
    }

    #[test]
    fn test_refuses_zero_period() {
        assert!(make(json!({ "channels": [], "period": 0 })).is_err());
        assert!(make(json!({ "channel": [] })).is_err());
    }
}
