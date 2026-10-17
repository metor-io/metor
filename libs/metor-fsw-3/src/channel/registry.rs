//! The channel registry, which tells operators what channels a target has.
//!
//! [`channel_registry`] publishes every channel, and the names each one may
//! load, as one `SequenceRegistry` message.

use core::num::NonZeroU64;

use metor_proto_wkt::{SequenceChannelSpec, SequenceRegistry};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::port::Output;

/// The config for [`channel_registry`].
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistryParams {
    pub channels: Vec<ChannelSpec>,
    /// The number of cycles between writes.
    #[serde(default = "default_period")]
    pub period: NonZeroU64,
}

/// One channel, and the names a `Load` command on it may use.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelSpec {
    pub name: String,
    #[serde(default)]
    pub available: Vec<String>,
}

/// The period used when the config does not set one.
fn default_period() -> NonZeroU64 {
    // PANIC Safety: evaluated at compile time.
    const { NonZeroU64::new(100).expect("100 is non-zero") }
}

/// The state of [`channel_registry`].
pub struct Registry {
    registry: SequenceRegistry,
    period: NonZeroU64,
    until: u64,
}

impl Registry {
    /// Creates the state from its config.
    pub fn new(params: RegistryParams) -> Self {
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
        Self {
            registry,
            period: params.period,
            until: 0,
        }
    }
}

/// Publishes the target's channels.
///
/// Writes the registry on the first cycle, then once every `period` cycles.
#[crate::system]
pub fn channel_registry(#[state] s: &mut Registry, registry: &mut Output<SequenceRegistry>) {
    if s.until == 0 {
        let _ = registry.write(&s.registry);
        s.until = s.period.get();
    }
    s.until -= 1;
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use super::*;
    use crate::coordinator::{ParamError, Params};
    use crate::fn_system::Ctor;

    use crate::tests::utils::pair;

    fn make(params: serde_json::Value) -> Result<Registry, ParamError> {
        Ctor::<Registry, (RegistryParams,)>::make(
            &Registry::new,
            Params(&params),
            &crate::io::IoInit::empty(),
        )
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
