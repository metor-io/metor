//! The [`System`] trait and the port bundles that feed it.

use metor_fsw_3_ring::{NoWake, View, WakeSink, WakeSource, Writer};
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::def::{DefCx, DefError};
use crate::record::RecordSchema;

/// A `PortDef` names one port of a bundle and the record its ring carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortDef {
    pub name: Cow<'static, str>,
    /// The record's [`NAME`](crate::Record::NAME), the type a config spells.
    pub record: Cow<'static, str>,
    pub id: ComponentId,
    pub max_len: usize,
    pub alignment: usize,
    pub depth: usize,
    /// A frame's field layout, taken at the record's own name; zero for a
    /// message.
    pub layout: u64,
    /// What this port announces to the ground.
    pub schema: RecordSchema,
}

impl PortDef {
    /// Whether two ports carry the same record under any name.
    pub fn same_record(&self, other: &PortDef) -> bool {
        self.record == other.record
            && self.id == other.id
            && self.max_len == other.max_len
            && self.alignment == other.alignment
            && self.depth == other.depth
            && self.layout == other.layout
            && self.schema.same_record(&other.schema)
    }
}

/// One instance's ports, in bind order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDef {
    pub name: Cow<'static, str>,
    pub inputs: Vec<PortDef>,
    pub outputs: Vec<PortDef>,
}

impl SystemDef {
    /// The definition this instance's config asks the bundles for, whose
    /// inputs wake `W`.
    pub fn new<I: SystemInputs<W>, O: SystemOutputs, W: WakeSink>(
        name: &'static str,
        cx: &DefCx<'_>,
    ) -> Result<Self, DefError> {
        Ok(Self {
            name: name.into(),
            inputs: I::defs(cx)?,
            outputs: O::defs(cx)?,
        })
    }
}

/// One input port: the definition build settled on and one view per producer.
pub struct InputBinding<W: WakeSink = NoWake> {
    pub def: PortDef,
    pub views: Vec<View<W>>,
}

/// One output port: the definition build settled on and its writer.
pub struct OutputBinding<W: WakeSource = NoWake> {
    pub def: PortDef,
    pub writer: Writer<W>,
}

/// A `System` runs once per cycle over its state and bound ports.
pub trait System {
    type State;
    type Inputs: SystemInputs;
    type Outputs: SystemOutputs;

    /// This instance's ports, computed from its config.
    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError>;

    fn execute(
        &self,
        now: Timestamp,
        state: &mut Self::State,
        inputs: &mut Self::Inputs,
        outputs: &mut Self::Outputs,
    );

    /// Called once after `execute` panicked, before the runner is destroyed.
    fn fault(&self, _now: Timestamp, _outputs: &mut Self::Outputs, _message: &str) {}
}

/// A system's input ports, bound in [`defs`](SystemInputs::defs) order.
///
/// `W` is the wake endpoint of the ports it binds: [`NoWake`] for a cyclic
/// system, [`Notifier`](metor_fsw_3_ring::Notifier) for an async one.
pub trait SystemInputs<W: WakeSink = NoWake> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError>;

    /// One binding per [`defs`](SystemInputs::defs) entry, in order.
    ///
    /// # Panics
    /// May panic on a list that does not match `defs`.
    fn bind(inputs: Vec<InputBinding<W>>) -> Self;
}

/// A system's output ports, bound in [`defs`](SystemOutputs::defs) order.
///
/// `W` is the wake endpoint of the ports it binds, as for [`SystemInputs`].
pub trait SystemOutputs<W: WakeSource = NoWake> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError>;

    /// One binding per [`defs`](SystemOutputs::defs) entry, in order.
    ///
    /// # Panics
    /// May panic on a list that does not match `defs`.
    fn bind(outputs: Vec<OutputBinding<W>>) -> Self;
}

impl<W: WakeSink> SystemInputs<W> for () {
    fn defs(_cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Vec::new())
    }
    fn bind(_inputs: Vec<InputBinding<W>>) -> Self {}
}

impl<W: WakeSource> SystemOutputs<W> for () {
    fn defs(_cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Vec::new())
    }
    fn bind(_outputs: Vec<OutputBinding<W>>) -> Self {}
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
                layout: Nav::schema().layout(),
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
        let mut bound = DynInputs::bind(vec![binding("plant.imu", &imu), binding("nav.nav", &nav)]);
        let names: Vec<_> = bound
            .iter_mut()
            .map(|(def, _)| def.name.to_string())
            .collect();
        assert_eq!(names, vec!["plant.imu", "nav.nav"]);
    }
}
