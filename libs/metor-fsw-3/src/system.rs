//! The [`System`] trait and the port bundles that feed it.

use metor_fsw_3_ring::{NoWake, View, WakeSink, WakeSource, Writer};
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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
    /// What this port announces to the ground.
    pub schema: RecordSchema,
}

/// A system's ports, in bind order.
///
/// A dynamic side declares no ports; the config lists them and build
/// completes each one before the system is bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDef {
    pub name: Cow<'static, str>,
    pub inputs: Vec<PortDef>,
    pub outputs: Vec<PortDef>,
    #[serde(default)]
    pub dynamic_inputs: bool,
    #[serde(default)]
    pub dynamic_outputs: bool,
}

impl SystemDef {
    pub fn new<I: SystemInputs, O: SystemOutputs>(name: &'static str) -> Self {
        Self {
            name: name.into(),
            inputs: I::defs(),
            outputs: O::defs(),
            dynamic_inputs: I::DYNAMIC,
            dynamic_outputs: O::DYNAMIC,
        }
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

/// A `System` is the core composable element of metor-fsw, it provides
pub trait System {
    type State;
    type Inputs: SystemInputs;
    type Outputs: SystemOutputs;

    fn def() -> SystemDef;

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

/// A struct of `Input<F>` fields. Derive with `#[derive(SystemInputs)]`.
pub trait SystemInputs {
    /// Whether the config names this bundle's ports rather than the type.
    const DYNAMIC: bool = false;

    fn defs() -> Vec<PortDef>;
    /// One binding per [`defs`](SystemInputs::defs) entry, in order, then one
    /// per port the config added to a dynamic bundle.
    ///
    /// # Panics
    /// Derived implementations panic on a wrong list length or unsupported frame alignment.
    fn bind(inputs: Vec<InputBinding>) -> Self;
}

/// A struct of `Output<F>` fields. Derive with `#[derive(SystemOutputs)]`.
pub trait SystemOutputs {
    /// Whether the config names this bundle's ports rather than the type.
    const DYNAMIC: bool = false;

    fn defs() -> Vec<PortDef>;
    /// One binding per [`defs`](SystemOutputs::defs) entry, in order, then one
    /// per port the config added to a dynamic bundle.
    ///
    /// # Panics
    /// Derived implementations panic on a wrong list length or unsupported frame alignment.
    fn bind(outputs: Vec<OutputBinding>) -> Self;
}

impl SystemInputs for () {
    fn defs() -> Vec<PortDef> {
        Vec::new()
    }
    fn bind(_inputs: Vec<InputBinding>) -> Self {}
}

impl SystemOutputs for () {
    fn defs() -> Vec<PortDef> {
        Vec::new()
    }
    fn bind(_outputs: Vec<OutputBinding>) -> Self {}
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, RingBuffer};
    use metor_proto::types::Timestamp;

    use super::*;
    use crate::port::{Input, Output, ring_capacity};
    use crate::tests::utils::{Imu, Nav};
    use crate::{Frame, Record};

    #[derive(crate::SystemInputs)]
    struct DerivedIn {
        imu: Input<Imu>,
        nav: Input<Nav>,
    }

    struct HandIn {
        imu: Input<Imu>,
        nav: Input<Nav>,
    }

    impl SystemInputs for HandIn {
        fn defs() -> Vec<PortDef> {
            vec![Input::<Imu>::def("imu"), Input::<Nav>::def("nav")]
        }
        fn bind(mut inputs: Vec<InputBinding>) -> Self {
            let nav = inputs.pop().expect("two bindings");
            let imu = inputs.pop().expect("two bindings");
            Self {
                imu: Input::try_new(imu.views).expect("supported alignment"),
                nav: Input::try_new(nav.views).expect("supported alignment"),
            }
        }
    }

    /// One binding per def, each over the rings listed beside it.
    fn bound_in(ports: Vec<(PortDef, Vec<&RingBuffer>)>) -> Vec<InputBinding> {
        ports
            .into_iter()
            .map(|(def, rings)| InputBinding {
                def,
                views: rings
                    .into_iter()
                    .map(|ring| ring.view(NoWake).expect("free slot"))
                    .collect(),
            })
            .collect()
    }

    fn bound_out(ports: Vec<(PortDef, &RingBuffer)>) -> Vec<OutputBinding> {
        ports
            .into_iter()
            .map(|(def, ring)| OutputBinding {
                def,
                writer: ring.writer(NoWake).expect("free writer"),
            })
            .collect()
    }

    #[derive(crate::SystemOutputs)]
    struct DerivedOut {
        nav: Output<Nav>,
    }

    fn ring<F: Frame>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(F::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 2,
        })
    }

    #[test]
    fn derived_defs_match_hand_written() {
        assert_eq!(DerivedIn::defs(), HandIn::defs());
        assert_eq!(
            DerivedOut::defs(),
            vec![PortDef {
                name: "nav".into(),
                record: Nav::NAME.into(),
                id: Nav::ID,
                max_len: size_of::<Nav>(),
                alignment: align_of::<Nav>(),
                depth: 1,
                schema: Nav::schema(),
            }]
        );
    }

    #[test]
    fn system_def_collects_both_bundles() {
        let def = SystemDef::new::<DerivedIn, DerivedOut>("nav");
        assert_eq!(def.name, "nav");
        assert_eq!(def.inputs, DerivedIn::defs());
        assert_eq!(def.outputs, DerivedOut::defs());
    }

    #[test]
    fn unit_bundles_are_empty() {
        assert!(<() as SystemInputs>::defs().is_empty());
        assert!(<() as SystemOutputs>::defs().is_empty());
        <() as SystemInputs>::bind(Vec::new());
        <() as SystemOutputs>::bind(Vec::new());
    }

    #[test]
    fn bind_follows_field_order() {
        let (imu, nav) = (ring::<Imu>(), ring::<Nav>());
        let mut bound = DerivedIn::bind(bound_in(vec![
            (Input::<Imu>::def("imu"), vec![&imu]),
            (Input::<Nav>::def("nav"), vec![&nav]),
        ]));
        let mut imu_out = Output::<Imu>::try_new(imu.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        imu_out.write(&Imu::new(1, 5.0)).expect("ring has room");
        assert_eq!(
            bound.imu.latest().expect("valid").expect("record").sample,
            5.0
        );
        assert!(bound.nav.latest().expect("valid").is_none());
    }

    #[test]
    fn hand_written_bundle_binds_like_the_derive() {
        let (imu, nav) = (ring::<Imu>(), ring::<Nav>());
        let mut out = DerivedOut::bind(bound_out(vec![(Output::<Nav>::def("nav"), &nav)]));
        let mut bound = HandIn::bind(bound_in(vec![
            (Input::<Imu>::def("imu"), vec![&imu]),
            (Input::<Nav>::def("nav"), vec![&nav]),
        ]));
        out.nav
            .write(&Nav {
                timestamp: Timestamp(2),
                estimate: 7.0,
            })
            .expect("ring has room");
        assert!(bound.imu.latest().expect("valid").is_none());
        assert_eq!(
            bound.nav.latest().expect("valid").expect("record").estimate,
            7.0
        );
    }

    #[test]
    #[should_panic(expected = "DerivedIn::defs()")]
    fn bind_with_wrong_length_panics() {
        let _ = DerivedIn::bind(bound_in(vec![(Input::<Imu>::def("imu"), Vec::new())]));
    }

    #[test]
    fn a_static_bundle_is_not_dynamic() {
        const { assert!(!DerivedIn::DYNAMIC) };
        const { assert!(!DerivedOut::DYNAMIC) };
        let def = SystemDef::new::<DerivedIn, DerivedOut>("nav");
        assert!(!def.dynamic_inputs && !def.dynamic_outputs);
    }

    #[test]
    fn a_dynamic_bundle_declares_no_ports_and_binds_what_it_is_given() {
        use crate::port::{DynInputs, DynOutputs};

        const { assert!(DynInputs::DYNAMIC) };
        const { assert!(DynOutputs::DYNAMIC) };
        let def = SystemDef::new::<DynInputs, DynOutputs>("link");
        assert!(def.inputs.is_empty() && def.outputs.is_empty());
        assert!(def.dynamic_inputs && def.dynamic_outputs);

        let (imu, nav) = (ring::<Imu>(), ring::<Nav>());
        let mut bound = DynInputs::bind(bound_in(vec![
            (Input::<Imu>::def("plant.imu"), vec![&imu]),
            (Input::<Nav>::def("nav.nav"), vec![&nav]),
        ]));
        let names: Vec<_> = bound
            .iter_mut()
            .map(|(def, _)| def.name.to_string())
            .collect();
        assert_eq!(names, vec!["plant.imu", "nav.nav"]);
    }
}
