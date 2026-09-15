//! The [`System`] trait and the port bundles that feed it.

use metor_fsw_3_ring::{NoWake, View, Writer};
use metor_proto::types::{ComponentId, Timestamp};

/// One port of a bundle: its field name, the frame it carries, and the record
/// size its ring must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortDef {
    pub name: &'static str,
    pub frame: ComponentId,
    pub max_size: usize,
}

/// A system's ports, in bind order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemDef {
    pub name: &'static str,
    pub inputs: Vec<PortDef>,
    pub outputs: Vec<PortDef>,
}

impl SystemDef {
    pub fn new<I: SystemInputs, O: SystemOutputs>(name: &'static str) -> Self {
        Self {
            name,
            inputs: I::defs(),
            outputs: O::defs(),
        }
    }
}

/// A unit of work the coordinator steps once per cycle.
///
/// `execute` takes `&self` so the definition holds no mutable data; `State` is
/// the only mutable data. Inputs are `&mut` because a read advances a cursor.
/// `now` is the cycle's timestamp, the same for every system in the cycle.
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
}

/// A struct of `Input<F>` fields. Derive with `#[derive(SystemInputs)]`.
pub trait SystemInputs {
    fn defs() -> Vec<PortDef>;
    /// One view list per [`defs`](SystemInputs::defs) entry, in order.
    fn bind(views: Vec<Vec<View<NoWake>>>) -> Self;
}

/// A struct of `Output<F>` fields. Derive with `#[derive(SystemOutputs)]`.
pub trait SystemOutputs {
    fn defs() -> Vec<PortDef>;
    /// One writer per [`defs`](SystemOutputs::defs) entry, in order.
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
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use super::*;
    use crate::port::{Input, Output, capacity_for};
    use crate::{Componentize, Frame};

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[frame(name = "imu")]
    #[repr(C)]
    struct Imu {
        #[frame(timestamp)]
        timestamp: Timestamp,
        omega: u64,
    }

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[frame(name = "nav")]
    #[repr(C)]
    struct Nav {
        #[frame(timestamp)]
        timestamp: Timestamp,
        attitude: u64,
    }

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
                imu: Input::new(imu),
                nav: Input::new(nav),
            }
        }
    }

    #[derive(crate::SystemOutputs)]
    struct DerivedOut {
        nav: Output<Nav>,
    }

    fn ring<F: Frame>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: capacity_for(F::MAX_SIZE, 4).expect("valid capacity"),
            max_readers: 2,
        })
    }

    #[test]
    fn derived_defs_match_hand_written() {
        assert_eq!(DerivedIn::defs(), HandIn::defs());
        assert_eq!(
            DerivedOut::defs(),
            vec![PortDef {
                name: "nav",
                frame: Nav::ID,
                max_size: Nav::MAX_SIZE,
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
        let mut imu_out = Output::<Imu>::new(imu.writer(NoWake).expect("free writer"));
        imu_out
            .write(&Imu {
                timestamp: Timestamp(1),
                omega: 5,
            })
            .expect("ring has room");
        assert_eq!(bound.imu.latest().expect("valid").expect("record").omega, 5);
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
                attitude: 7,
            })
            .expect("ring has room");
        assert!(bound.imu.latest().expect("valid").is_none());
        assert_eq!(
            bound.nav.latest().expect("valid").expect("record").attitude,
            7
        );
    }

    #[test]
    #[should_panic(expected = "DerivedIn::defs()")]
    fn bind_with_wrong_length_panics() {
        let _ = DerivedIn::bind(vec![Vec::new()]);
    }
}
