//! Pack-shared state borrowed by attached cyclic systems.
//!
//! [`Pack::shared_state`](crate::Pack::shared_state) returns a [`Shared`] token.
//! The state is constructed from its wiring declaration. [`SharedLifecycle`]
//! starts it before the first attached system initializes and shuts it down
//! after the last attached system stops.
//!
//! Borrows use [`RefCell`] and must stay within a cyclic step. Async tasks may
//! own separate resources and communicate through channels; they must not
//! hold a shared-state borrow across an await.

use core::cell::{Cell, RefCell, RefMut};
use std::rc::Rc;

/// Once-per-instance lifecycle of a pack-shared state. Both hooks run on
/// the coordinator's loop task: `start` before the first attached system's
/// init (spawn background tasks here, a runtime is up), `shutdown` after
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

    /// The scoped `&mut` grant. Panics if the state was never constructed
    /// (entry create rejects an undeclared state before any system runs, so
    /// reaching that panic means bypassing the wiring path), or on a
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
