//! The channel registry, which tells operators what channels a target has.
//!
//! [`channel_registry`] publishes every channel, and the names each one may
//! load, as one `SequenceRegistry` message.

use core::num::NonZeroU64;

use metor_proto_wkt::{SequenceChannelComponents, SequenceChannelSpec, SequenceRegistry};
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

/// One channel, the names a `Load` command on it may use, and the
/// components carrying its state.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelSpec {
    pub name: String,
    #[serde(default)]
    pub available: Vec<String>,
    /// The wrapped system's type.
    pub system: String,
    pub components: ChannelComponents,
}

/// The full component name of each component carrying the channel's state:
/// every field of its [`ChannelStatus`](super::ChannelStatus), and its
/// [`ChannelMessage`](super::ChannelMessage)'s text as `message`.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelComponents {
    pub state: String,
    pub loaded: String,
    pub artifact: String,
    pub hash: String,
    pub accepts: String,
    pub message: String,
    pub elapsed: String,
    pub abort_elapsed: String,
}

impl From<ChannelComponents> for SequenceChannelComponents {
    fn from(c: ChannelComponents) -> Self {
        Self {
            state: c.state,
            loaded: c.loaded,
            artifact: c.artifact,
            hash: c.hash,
            accepts: c.accepts,
            message: c.message,
            elapsed: c.elapsed,
            abort_elapsed: c.abort_elapsed,
        }
    }
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
                    system: spec.system,
                    components: spec.components.into(),
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
    use crate::coordinator::{MakeError, Params};
    use crate::fn_system::Ctor;

    use crate::tests::utils::pair;

    fn make(params: serde_json::Value) -> Result<Registry, MakeError> {
        Ctor::<Registry, (RegistryParams,)>::make(
            &Registry::new,
            Params(&params),
            &mut crate::io::IoInit::empty("registry"),
        )
    }

    /// A channel spec whose components live under `fsw.{name}`.
    fn spec(name: &str, available: &[&str]) -> serde_json::Value {
        let fields = [
            "state",
            "loaded",
            "artifact",
            "hash",
            "accepts",
            "elapsed",
            "abort_elapsed",
        ];
        let mut components: serde_json::Map<_, _> = fields
            .iter()
            .map(|f| {
                (
                    f.to_string(),
                    json!(format!("fsw.{name}.channel_status.{f}")),
                )
            })
            .collect();
        components.insert(
            "message".into(),
            json!(format!("fsw.{name}.channel_message.text")),
        );
        json!({ "name": name, "available": available, "system": "dl_slot", "components": components })
    }

    #[test]
    fn test_writes_on_first_cycle_then_each_period() {
        let mut state = make(json!({
            "channels": [spec("mode", &["a", "b"]), spec("arm", &[])],
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
        let mut state = make(json!({ "channels": [spec("mode", &["a"])] })).expect("valid params");
        let (_ring, mut output, mut input) = pair::<SequenceRegistry>(4);
        channel_registry()(&mut state, &mut output);
        let registry = input.drain().next().expect("a record").expect("decodes");
        assert_eq!(registry.channels.len(), 1);
        assert_eq!(registry.channels[0].name, "mode");
        assert_eq!(registry.channels[0].available, ["a"]);
        assert_eq!(registry.channels[0].system, "dl_slot");
        let components = &registry.channels[0].components;
        assert_eq!(components.state, "fsw.mode.channel_status.state");
        assert_eq!(components.message, "fsw.mode.channel_message.text");
        assert_eq!(
            components.abort_elapsed,
            "fsw.mode.channel_status.abort_elapsed"
        );
    }

    #[test]
    fn test_refuses_incomplete_or_unknown_components() {
        let mut partial = spec("mode", &[]);
        partial["components"]
            .as_object_mut()
            .expect("an object")
            .remove("message");
        assert!(make(json!({ "channels": [partial] })).is_err());
        let mut extra = spec("mode", &[]);
        extra["components"]["clock"] = json!("fsw.mode.t");
        assert!(make(json!({ "channels": [extra] })).is_err());
    }

    #[test]
    fn test_refuses_zero_period() {
        assert!(make(json!({ "channels": [], "period": 0 })).is_err());
        assert!(make(json!({ "channel": [] })).is_err());
    }
}
