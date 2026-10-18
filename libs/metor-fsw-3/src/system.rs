//! The [`System`] trait, and the [`Ports`] trait for binding a system's ports.
//!
//! A system declares its ports as definitions first. The coordinator matches
//! those definitions against the config, creates the rings, and then binds
//! each port to its ring handles.

use metor_fsw_3_ring::{NoWake, View, WakeSink, WakeSource, Writer};
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::fn_system::Instance;
use crate::io::{GrantDef, Io, IoPorts};
use crate::record::RecordSchema;

/// Describes one port: its name and the record it carries.
///
/// The coordinator uses definitions to check that connected ports agree, and
/// to size each port's ring.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortDef {
    pub name: Cow<'static, str>,
    /// The record's [`NAME`](crate::Record::NAME).
    ///
    /// A config refers to the record type by this name.
    pub record: Cow<'static, str>,
    pub id: ComponentId,
    pub max_len: usize,
    pub alignment: usize,
    pub depth: usize,
    /// A fingerprint of a frame's field layout, or `None` for a message.
    ///
    /// See [`Record::layout`](crate::Record::layout).
    pub layout: Option<u64>,
    /// The schema this port announces to the ground.
    pub schema: RecordSchema,
}

impl PortDef {
    /// Returns whether two ports carry the same record.
    ///
    /// The ports' own names are ignored. A frame's schema depends on its port's
    /// root, so frames are matched by layout and messages by schema.
    pub fn same_record(&self, other: &PortDef) -> bool {
        self.record == other.record
            && self.id == other.id
            && self.max_len == other.max_len
            && self.alignment == other.alignment
            && self.depth == other.depth
            && self.layout == other.layout
            && match (&self.schema, &other.schema) {
                (RecordSchema::Frame { .. }, RecordSchema::Frame { .. }) => true,
                (schema, other_schema) => schema == other_schema,
            }
    }
}

/// Describes one system instance's ports.
///
/// Ports are listed in the order they are bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDef {
    pub name: Cow<'static, str>,
    pub inputs: Vec<PortDef>,
    pub outputs: Vec<PortDef>,
    /// The index in `outputs` of the `log` output, if the system has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<usize>,
    /// The `Cap` parameters, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<GrantDef>,
    /// Where the queue pair's ports are, for a system with a `Cap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<IoPorts>,
}

impl SystemDef {
    /// Builds the definition for an instance from its config.
    ///
    /// `I` and `O` compute the input and output ports from `cx`. The inputs
    /// wait on `W`.
    pub fn new<I: Ports<InputBinding<W>>, O: Ports<OutputBinding>, W: WakeSink>(
        name: &'static str,
        cx: &DefCx<'_>,
    ) -> Result<Self, DefError> {
        Ok(Self {
            name: name.into(),
            inputs: I::defs(cx)?,
            outputs: O::defs(cx)?,
            log: None,
            grants: Vec::new(),
            io: None,
        })
    }
}

/// A resolved input port, ready to bind.
///
/// It holds one view per producer.
pub struct InputBinding<W: WakeSink = NoWake> {
    pub def: PortDef,
    pub views: Vec<View<W>>,
}

/// A resolved output port, ready to bind.
pub struct OutputBinding<W: WakeSource = NoWake> {
    pub def: PortDef,
    pub writer: Writer<W>,
}

/// A unit of work that runs once per cycle.
///
/// Each cycle, [`System::execute`] reads its inputs, updates its state, and
/// writes its outputs.
pub trait System {
    type State;
    type Inputs: Ports<InputBinding>;
    type Outputs: Ports<OutputBinding>;

    /// Returns this instance's ports, computed from its config.
    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError>;

    fn execute(
        &self,
        now: Timestamp,
        state: &mut Self::State,
        inputs: &mut Self::Inputs,
        outputs: &mut Self::Outputs,
    );

    /// Called once after `execute` panics, before the system is removed from
    /// the schedule.
    fn fault(&self, _now: Timestamp, _outputs: &mut Self::Outputs, _message: &str) {}
}

/// A system's input or output ports, bound from one `B` per port.
///
/// `B` is an [`InputBinding`] for inputs and an [`OutputBinding`] for outputs.
/// Its wake parameter is what the ports wait on or notify: [`NoWake`] for a
/// cyclic system, and [`Notifier`](metor_fsw_3_ring::Notifier) for an async
/// system's inputs.
pub trait Ports<B> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError>;

    /// Binds the ports under `cx`.
    ///
    /// `bindings` has one entry per [`defs`](Ports::defs) entry, in the same
    /// order, less the queue pair's ports, which `cx` holds as its `Io`.
    ///
    /// # Errors
    /// Fails if a parameter cannot bind under `cx`, such as an `Rng` with no
    /// seed.
    ///
    /// # Panics
    /// May panic if `bindings` does not match `defs`.
    fn bind(bindings: Vec<B>, cx: &BindCx) -> Result<Self, ParamError>
    where
        Self: Sized;
}

impl<B> Ports<B> for () {
    fn defs(_cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Vec::new())
    }

    fn bind(_bindings: Vec<B>, _cx: &BindCx) -> Result<Self, ParamError> {
        Ok(())
    }
}

/// What a system's ports bind under: the instance they belong to, and its
/// queue if it has a `Cap`.
#[derive(Clone, Default)]
pub struct BindCx {
    pub(crate) instance: Instance,
    pub(crate) io: Option<Io>,
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::RingBuffer;

    use super::*;
    use crate::Record;
    use crate::def::DefCx;
    use crate::port::{DynInputs, DynOutputs, Input, Output};
    use crate::tests::utils::{Imu, ImuPair, Nav, ring};

    #[test]
    fn test_port_definition() {
        assert_eq!(
            Output::<Nav>::def(&DefCx::empty(), "nav"),
            PortDef {
                name: "nav".into(),
                record: Nav::NAME.into(),
                id: Nav::ID,
                max_len: size_of::<Nav>(),
                alignment: align_of::<Nav>(),
                depth: 1,
                layout: Nav::layout(),
                schema: Nav::schema(),
            }
        );
    }

    #[test]
    fn test_layout_tells_frames_of_one_name_apart() {
        let cx = DefCx::empty();
        let (imu, pair) = (
            Output::<Imu>::def(&cx, "imu"),
            Output::<ImuPair>::def(&cx, "imu"),
        );
        assert_eq!((imu.record.as_ref(), imu.max_len), ("imu", pair.max_len));
        assert_ne!(imu.layout, pair.layout);
        assert!(!imu.same_record(&pair));
    }

    #[test]
    fn test_layout_survives_the_port_root() {
        let rooted = DefCx {
            id: "plant",
            namespace: Some("cube_sat"),
            ..DefCx::empty()
        };
        let (here, there) = (
            Output::<Imu>::def(&DefCx::empty(), "imu"),
            Output::<Imu>::def(&rooted, "imu"),
        );
        assert_ne!(here.schema, there.schema);
        assert_eq!(here.layout, there.layout);
        assert!(here.same_record(&there));
    }

    #[test]
    fn test_dynamic_bundle_configuration() {
        let def = SystemDef::new::<DynInputs, DynOutputs, _>("link", &DefCx::empty())
            .expect("a static definition");
        assert!(def.inputs.is_empty() && def.outputs.is_empty());

        let producer = Output::<Imu>::def(&DefCx::empty(), "imu");
        let edges = [("plant.imu", &producer)];
        let cx = DefCx {
            inputs: &edges,
            ..DefCx::empty()
        };
        let def = SystemDef::new::<DynInputs, DynOutputs, _>("link", &cx).expect("one config port");
        // The dynamic input carries the producer's def under the config's name.
        assert_eq!(
            def.inputs,
            vec![PortDef {
                name: "plant.imu".into(),
                ..producer.clone()
            }]
        );

        let (imu, nav) = (ring::<Imu>(4, 2), ring::<Nav>(4, 2));
        let binding = |name, ring: &RingBuffer| InputBinding {
            def: Input::<Imu>::def(&DefCx::empty(), name),
            views: vec![ring.view(NoWake).expect("free slot")],
        };
        let mut bound = DynInputs::new(vec![binding("plant.imu", &imu), binding("nav.nav", &nav)]);
        let names: Vec<_> = bound
            .iter_mut()
            .map(|(def, _)| def.name.to_string())
            .collect();
        assert_eq!(names, vec!["plant.imu", "nav.nav"]);
    }
}
