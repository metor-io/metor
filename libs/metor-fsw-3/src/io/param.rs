//! `Cap<K>` as a system parameter.

use metor_fsw_3_ring::WakeSink;

use super::GrantDef;
use super::cap::{Cap, Handle, Interface};
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers, param_name};

fn grant<K: Interface>(param: &'static str) -> GrantDef {
    GrantDef {
        param: param.into(),
        interface: K::NAME.into(),
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
        defs.grants.push(grant::<K>(param_name(names)));
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
