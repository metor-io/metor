//! Pack-shared system state: one state instance, several systems.
//!
//! Some resources are naturally owned by more than one system — a link
//! server whose connection set the downlink writes and the uplink reads, at
//! different points in the same cycle. The sharing model reuses the grant
//! systems already have: a system runs against `&mut` to a state, so several
//! systems are granted `&mut` to the *same* state, time-sliced as each takes
//! its turn on the cycle loop. The attached systems stay distinct — own
//! instance names, own ports, own health — only the state behind them is
//! one.
//!
//! [`Pack::shared_state`](crate::Pack::shared_state) declares the instance
//! and returns the [`Shared`] token entries capture; sharing is scoped to
//! the declaring pack by construction, since the token is only reachable
//! inside the `pack()` that made it. The state is constructed once, from
//! its own wiring declaration's params, and its [`SharedLifecycle`] hooks
//! run once: `start` before the first attached system's init, `shutdown`
//! after the last attached system's shutdown.
//!
//! Attachment is cyclic-only. The runtime is a single-threaded cooperative
//! executor, so a [`RefCell`] time-slices soundly between cyclic steps —
//! but an async system's state moves into its own task and lives across
//! awaits, where a held borrow would collide with the very interleaving the
//! executor exists to provide. A state that needs background tasks (a
//! server's accept loop) spawns them in `start` over an `Rc`'d interior of
//! its own, with borrows that never cross an await; the systems' `&mut S`
//! remains the sole mutable grant of the state struct itself.

use core::cell::{Cell, RefCell, RefMut};
use std::rc::Rc;

/// Once-per-instance lifecycle of a pack-shared state. Both hooks run on
/// the coordinator's loop task: `start` before the first attached system's
/// init (spawn background tasks here — a runtime is up), `shutdown` after
/// the last attached system's shutdown. Dropping the state is the cancel
/// backstop for anything `start` spawned, so hold spawned tasks as drop
/// guards.
pub trait SharedLifecycle: 'static {
    fn start(&mut self) {}
    fn shutdown(&mut self) {}
}

/// The one instance of a pack-shared state: empty until its wiring
/// declaration constructs it, then granted to attached systems one scoped
/// borrow at a time.
pub struct SharedCell<S> {
    /// The pack-declared state name (the wiring `state` type key), for
    /// diagnostics.
    name: &'static str,
    state: RefCell<Option<S>>,
    started: Cell<bool>,
    attached_live: Cell<usize>,
}

/// The clonable token [`Pack::shared_state`](crate::Pack::shared_state)
/// returns and attached entries capture. [`get`](Self::get) grants the
/// scoped `&mut` to the one instance.
pub struct Shared<S>(Rc<SharedCell<S>>);

impl<S> Clone for Shared<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S> Shared<S> {
    pub fn new(name: &'static str) -> Self {
        Self(Rc::new(SharedCell {
            name,
            state: RefCell::new(None),
            started: Cell::new(false),
            attached_live: Cell::new(0),
        }))
    }

    /// The scoped `&mut` grant. Panics if the state was never constructed —
    /// entry create rejects an undeclared state before any system runs, so
    /// reaching that panic means bypassing the wiring path — or on a
    /// re-entrant borrow, which the sequential cycle loop cannot produce.
    pub fn get(&self) -> RefMut<'_, S> {
        let inner = self.0.state.borrow_mut();
        assert!(
            inner.is_some(),
            "shared state `{}` was never constructed (missing wiring declaration)",
            self.0.name
        );
        RefMut::map(inner, |slot| slot.as_mut().expect("checked above"))
    }

    /// Construct the instance. Errors if already constructed (a duplicate
    /// wiring declaration).
    pub fn set(&self, state: S) -> Result<(), AlreadySet> {
        let mut slot = self.0.state.borrow_mut();
        if slot.is_some() {
            return Err(AlreadySet);
        }
        *slot = Some(state);
        Ok(())
    }

    pub fn erased(&self) -> Rc<dyn ErasedShared>
    where
        S: SharedLifecycle,
    {
        self.0.clone()
    }
}

/// A second construction of an already-constructed shared state.
pub struct AlreadySet;

/// The type-erased face of a [`SharedCell`], for the attachment machinery:
/// lifecycle fan-in (start once, shutdown after the last release) and the
/// construction checks entry create runs.
pub trait ErasedShared {
    fn name(&self) -> &'static str;
    fn is_constructed(&self) -> bool;
    /// Count an attached entry in (at entry create, so the wiring's
    /// unused-state check sees it); pairs with [`release`](Self::release).
    fn attach(&self);
    /// Live attachments, for the unused-state check.
    fn attached(&self) -> usize;
    /// Run `start` once, on the first attached init.
    fn ensure_started(&self);
    /// Count an attached entry out; the last release runs `shutdown`.
    fn release(&self);
}

impl<S: SharedLifecycle> ErasedShared for SharedCell<S> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn is_constructed(&self) -> bool {
        self.state.borrow().is_some()
    }

    fn attach(&self) {
        self.attached_live.set(self.attached_live.get() + 1);
    }

    fn attached(&self) -> usize {
        self.attached_live.get()
    }

    fn ensure_started(&self) {
        if !self.started.replace(true) {
            self.state
                .borrow_mut()
                .as_mut()
                .expect("constructed before first attached init")
                .start();
        }
    }

    fn release(&self) {
        let left = self.attached_live.get() - 1;
        self.attached_live.set(left);
        if left == 0 && self.started.get() {
            self.state
                .borrow_mut()
                .as_mut()
                .expect("constructed before shutdown")
                .shutdown();
        }
    }
}
