//! The functional authoring surface: a system as free fns over an abstract
//! state.
//!
//! [`system(execute_fn)`](system) builds a [`SystemDef`] whose port set is
//! the fn's parameter types. `.init(fn)` supplies construction from typed
//! params; `.state(value)` moves a prebuilt state in (the shared-handle
//! case). A [`Pack`](crate::Pack) turns defs into erased entries:
//!
//! ```ignore
//! fn nav_init(p: NavParams) -> NavState { ... }
//! fn nav_execute(nav: &mut NavState, now: Timestamp,
//!                sensors: &mut Input<Sensors>, est: &mut Output<Estimate>) { ... }
//!
//! pub fn pack() -> Pack {
//!     Pack::new().system("nav", system(nav_execute).init(nav_init))
//! }
//! ```
//!
//! The trait machinery is the standard function-parameter pattern (Bevy's
//! `SystemParamFunction`): each parameter type implements [`ExecParam`], a
//! marker generic keeps the per-arity blanket impls coherent, and one tuple
//! expansion generates declaration and bind in the same order.

mod driver;
mod param;
mod task;
mod tuples;

#[cfg(test)]
mod tests;

use core::marker::PhantomData;

use postcard_schema::Schema;
use serde::de::DeserializeOwned;

pub use param::{BindCx, CycleCx, DeclSink, ExecParam};
pub use task::{AsyncSystemFn, IntoOutcome, Params, TaskParam};
pub use tuples::{ExecParamSet, ExecuteFn};

pub(crate) use driver::{FnDriver, FutureDriver, OccupantFuture, bind_health_tail, mount_driver};
pub(crate) use task::TaskParamsSpec;

use crate::descriptor::{PortDesc, SystemDescriptor, SystemKind, split_decls};
use crate::health::{SystemHealth, SystemLog};
use crate::pack::{EntryParams, MakeError, PackEntry, Pending, decode_params};

/// Start a fn-authored system from its per-cycle execute fn. The fn takes a
/// leading `&mut State` and then only system parameters ([`ExecParam`]).
pub fn system<S, M, F>(execute: F) -> SystemDef<S, M, F, NoInit>
where
    F: ExecuteFn<S, M>,
{
    SystemDef {
        execute,
        init: NoInit,
        _m: PhantomData,
    }
}

/// A fn-authored system definition: the execute fn plus how its state is
/// constructed. Consumed by [`Pack::system`](crate::Pack::system).
pub struct SystemDef<S, M, F, I> {
    execute: F,
    init: I,
    _m: PhantomData<fn() -> (S, M)>,
}

/// No construction given: the state is `Default` and the entry takes no
/// params.
pub struct NoInit;

/// Construction from typed params via an init fn.
pub struct InitWith<G, M2> {
    init: G,
    _m: PhantomData<fn() -> M2>,
}

/// A prebuilt state moved into the entry (instantiable once).
pub struct Prebuilt<S>(Option<S>);

impl<S, M, F> SystemDef<S, M, F, NoInit>
where
    F: ExecuteFn<S, M>,
{
    /// Construct the state from typed params: `fn(P) -> S` (P becomes the
    /// entry's params type) or `fn() -> S` (paramless).
    pub fn init<M2, G>(self, init: G) -> SystemDef<S, M, F, InitWith<G, M2>>
    where
        G: InitFn<S, M2>,
    {
        SystemDef {
            execute: self.execute,
            init: InitWith {
                init,
                _m: PhantomData,
            },
            _m: PhantomData,
        }
    }

    /// Move a prebuilt state in. The shared-handle case: build the shared
    /// resource in `pack()`, hand each entry a state holding a clone. The
    /// entry can be instantiated once and never as a slot occupant.
    pub fn state(self, state: S) -> SystemDef<S, M, F, Prebuilt<S>> {
        SystemDef {
            execute: self.execute,
            init: Prebuilt(Some(state)),
            _m: PhantomData,
        }
    }
}

/// How a fn-authored system's state is constructed from decoded params.
/// Implemented for `FnMut(P) -> S` and `FnMut() -> S`.
pub trait InitFn<S, Marker>: 'static {
    /// The entry's params type (`()` for a paramless init).
    type Params: DeserializeOwned + Schema + 'static;
    fn call(&mut self, params: Self::Params) -> S;
}

impl<Func, S, P> InitFn<S, fn(P) -> S> for Func
where
    Func: 'static + FnMut(P) -> S,
    P: DeserializeOwned + Schema + 'static,
{
    type Params = P;
    fn call(&mut self, params: P) -> S {
        self(params)
    }
}

impl<Func, S> InitFn<S, fn() -> S> for Func
where
    Func: 'static + FnMut() -> S,
{
    type Params = ();
    fn call(&mut self, (): ()) -> S {
        self()
    }
}

/// A definition [`Pack::system`](crate::Pack::system) can turn into an
/// erased entry.
pub trait IntoPackEntry {
    fn into_entry(self, name: &'static str) -> PackEntry;
}

/// The entry descriptor for a parameter set: the declared ports per
/// direction in parameter order, then the implicit health/log tail — the
/// same shape [`Out`](crate::Out) gives a struct system.
fn descriptor_for<P: ExecParamSet>(name: &'static str) -> SystemDescriptor {
    let mut sink = DeclSink::default();
    P::decls(&mut sink);
    let (inputs, _) = split_decls(sink.inputs);
    let (mut outputs, _) = split_decls(sink.outputs);
    outputs.push(PortDesc::of::<SystemHealth>());
    outputs.push(PortDesc::of::<SystemLog>());
    SystemDescriptor {
        name,
        kind: SystemKind::Cyclic,
        inputs,
        outputs,
        capabilities: Vec::new(),
    }
}

impl<S, M, F> IntoPackEntry for SystemDef<S, M, F, NoInit>
where
    S: Default + 'static,
    M: 'static,
    F: ExecuteFn<S, M> + Clone,
{
    fn into_entry(self, name: &'static str) -> PackEntry {
        let execute = self.execute;
        PackEntry {
            name,
            descriptor: descriptor_for::<F::Params>(name),
            params_schema: <() as Schema>::SCHEMA,
            params_default: None,
            reloadable: true,
            create: Box::new(move |params: EntryParams<'_>| {
                decode_params::<()>(params)?;
                let execute = execute.clone();
                let pending: Pending = Box::new(move |src, mount| {
                    mount_driver(src, mount, move |src| {
                        Box::new(FnDriver::bind(S::default(), execute, src))
                    })
                });
                Ok(pending)
            }),
        }
    }
}

impl<S, M, F, G, M2> IntoPackEntry for SystemDef<S, M, F, InitWith<G, M2>>
where
    S: 'static,
    M: 'static,
    M2: 'static,
    F: ExecuteFn<S, M> + Clone,
    G: InitFn<S, M2> + Clone,
{
    fn into_entry(self, name: &'static str) -> PackEntry {
        let execute = self.execute;
        let init = self.init.init;
        PackEntry {
            name,
            descriptor: descriptor_for::<F::Params>(name),
            params_schema: <G::Params as Schema>::SCHEMA,
            params_default: None,
            reloadable: true,
            create: Box::new(move |params: EntryParams<'_>| {
                let p: G::Params = decode_params(params)?;
                let state = init.clone().call(p);
                let execute = execute.clone();
                let pending: Pending = Box::new(move |src, mount| {
                    mount_driver(src, mount, move |src| {
                        Box::new(FnDriver::bind(state, execute, src))
                    })
                });
                Ok(pending)
            }),
        }
    }
}

impl<S, M, F> IntoPackEntry for SystemDef<S, M, F, Prebuilt<S>>
where
    S: 'static,
    M: 'static,
    F: ExecuteFn<S, M> + Clone,
{
    fn into_entry(self, name: &'static str) -> PackEntry {
        let execute = self.execute;
        let mut state = self.init.0;
        PackEntry {
            name,
            descriptor: descriptor_for::<F::Params>(name),
            params_schema: <() as Schema>::SCHEMA,
            params_default: None,
            reloadable: false,
            create: Box::new(move |params: EntryParams<'_>| {
                decode_params::<()>(params)?;
                let state = state.take().ok_or(MakeError::StateTaken)?;
                let execute = execute.clone();
                let pending: Pending = Box::new(move |src, mount| {
                    mount_driver(src, mount, move |src| {
                        Box::new(FnDriver::bind(state, execute, src))
                    })
                });
                Ok(pending)
            }),
        }
    }
}
