//! `Cap<K>` as a system parameter.

use metor_fsw_3_ring::NoWake;

use super::GrantDef;
use super::cap::{Cap, Handle, Interface};
use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamPath, Views, Writers};

/// A `Cap<K>` gives its system a queue pair. It takes no port of its own.
impl<K: Interface> Param<NoWake> for Cap<K> {
    type In = ();
    type Out = Cap<K>;
    type Item<'a> = &'a Cap<K>;

    fn append_defs(at: &ParamPath<'_>, _cx: &DefCx<'_>, defs: &mut Defs) -> Result<(), DefError> {
        defs.grants.push(GrantDef::new::<K>(at.to_name()));
        Ok(())
    }

    fn bind_in(_views: &mut Views<NoWake>) {}

    fn bind_out(_at: &ParamPath<'_>, writers: &mut Writers) -> Result<Cap<K>, MakeError> {
        // PANIC Safety: `MakeCx::bind_ports` builds a queue for every def
        // with a grant, and a `Cap` adds one.
        let io = writers
            .io
            .as_ref()
            .expect("a queue for a system with a `Cap`");
        Ok(Cap::new(Handle::new(io.next_grant(), io.clone())))
    }

    fn get<'a>(_input: &'a mut (), output: &'a mut Cap<K>, _cx: &mut Cycle<'a>) -> &'a Cap<K> {
        output
    }
}
