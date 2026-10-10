//! `Cap<K>` and `DeviceSide<K>` as system parameters.

use metor_fsw_3_ring::WakeSink;

use super::cap::{Cap, Handle, Interface};
use super::device::DeviceSide;
use super::{End, GrantDef};
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers, param_name};

fn grant<K: Interface>(param: &'static str, end: End) -> GrantDef {
    GrantDef {
        param: param.into(),
        interface: K::NAME.into(),
        end,
    }
}

/// A `Cap<K>` gives its system a queue pair. It takes no port of its own.
impl<K: Interface> Param for Cap<K> {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = Cap<K>;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a Cap<K>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.grants.push(grant::<K>(param_name(names), End::Host));
        defs.io = true;
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Cap<K> {
        // PANIC Safety: `fn_system::bind` builds a queue for every set whose
        // `Defs::io` is set, and a `Cap` sets it.
        let io = writers
            .io
            .as_ref()
            .expect("a queue for a system with a `Cap`");
        Cap::new(Handle::new(io.next_grant(), io.clone()))
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut Cap<K>,
        _cx: &mut Cycle<'a>,
    ) -> &'a Cap<K> {
        output
    }
}

/// A `DeviceSide<K>` takes a request input `{param}.req` and a reply output
/// `{param}.rep`, which `Defs` appends after the system's own ports.
impl<K: Interface> Param for DeviceSide<K> {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = DeviceSide<K>;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut DeviceSide<K>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.grants.push(grant::<K>(param_name(names), End::Device));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> DeviceSide<K> {
        // PANIC Safety: `fn_system::bind` builds one device end per `Device`
        // grant, in parameter order.
        let device = writers
            .devices
            .pop_front()
            .expect("a device end per `DeviceSide`");
        device.typed()
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut DeviceSide<K>,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut DeviceSide<K> {
        output
    }
}
