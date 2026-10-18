//! `Cap<K>` as a system parameter.

use crate::port::InputWake;

use super::GrantDef;
use super::cap::{Cap, Handle, Interface};
use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers, param_name};

/// A `Cap<K>` gives its system a queue pair. It takes no port of its own.
impl<K: Interface> Param for Cap<K> {
    type In<W: InputWake> = ();
    type Out = Cap<K>;
    type Item<'a, W: InputWake> = &'a Cap<K>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.grants.push(GrantDef::new::<K>(param_name(names)));
        Ok(())
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<Cap<K>, MakeError> {
        param_name(names);
        // PANIC Safety: `MakeCx::bind_ports` builds a queue for every def
        // with a grant, and a `Cap` adds one.
        let io = writers
            .io
            .as_ref()
            .expect("a queue for a system with a `Cap`");
        Ok(Cap::new(Handle::new(io.next_grant(), io.clone())))
    }

    fn get<'a, W: InputWake>(
        _input: &'a mut (),
        output: &'a mut Cap<K>,
        _cx: &mut Cycle<'a>,
    ) -> &'a Cap<K> {
        output
    }
}
