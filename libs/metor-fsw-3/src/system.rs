//! The [`System`] trait and the port bundles that feed it.

use metor_fsw_3_ring::{NoWake, View, Writer};
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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
}

/// A system's ports, in bind order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDef {
    pub name: Cow<'static, str>,
    pub inputs: Vec<PortDef>,
    pub outputs: Vec<PortDef>,
}

impl SystemDef {
    pub fn new<I: SystemInputs, O: SystemOutputs>(name: &'static str) -> Self {
        Self {
            name: name.into(),
            inputs: I::defs(),
            outputs: O::defs(),
        }
    }
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
    fn defs() -> Vec<PortDef>;
    /// One view list per [`defs`](SystemInputs::defs) entry, in order.
    ///
    /// # Panics
    /// Derived implementations panic on a wrong list length or unsupported frame alignment.
    fn bind(views: Vec<Vec<View<NoWake>>>) -> Self;
}

/// A struct of `Output<F>` fields. Derive with `#[derive(SystemOutputs)]`.
pub trait SystemOutputs {
    fn defs() -> Vec<PortDef>;
    /// One writer per [`defs`](SystemOutputs::defs) entry, in order.
    ///
    /// # Panics
    /// Derived implementations panic on a wrong list length or unsupported frame alignment.
    fn bind(writers: Vec<Writer<NoWake>>) -> Self;
}

impl SystemInputs for () {
    fn defs() -> Vec<PortDef> {
        Vec::new()
    }
    fn bind(_views: Vec<Vec<View<NoWake>>>) -> Self {}
}

impl SystemOutputs for () {
    fn defs() -> Vec<PortDef> {
        Vec::new()
    }
    fn bind(_writers: Vec<Writer<NoWake>>) -> Self {}
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
        fn bind(mut views: Vec<Vec<View<NoWake>>>) -> Self {
            let nav = views.pop().expect("two lists");
            let imu = views.pop().expect("two lists");
            Self {
                imu: Input::try_new(imu).expect("supported alignment"),
                nav: Input::try_new(nav).expect("supported alignment"),
            }
        }
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
        let mut bound = DerivedIn::bind(vec![
            vec![imu.view(NoWake).expect("free slot")],
            vec![nav.view(NoWake).expect("free slot")],
        ]);
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
        let mut out = DerivedOut::bind(vec![nav.writer(NoWake).expect("free writer")]);
        let mut bound = HandIn::bind(vec![
            vec![imu.view(NoWake).expect("free slot")],
            vec![nav.view(NoWake).expect("free slot")],
        ]);
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
        let _ = DerivedIn::bind(vec![Vec::new()]);
    }
}
