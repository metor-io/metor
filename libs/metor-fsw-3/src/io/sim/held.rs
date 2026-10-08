//! The ops a model has not answered yet.

use super::super::{Cqe, IoError, Sqe, Token};

/// An op waiting for its model, and when it may be answered.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub(crate) sqe: Sqe,
    /// The serve count at which the model first sees the op.
    pub(crate) ready_at: u64,
    /// The model's answer, kept while the reply port is full.
    pub(crate) reply: Option<Cqe>,
}

/// A fixed-capacity queue of held ops, in arrival order. It never grows past
/// the capacity it was made with.
pub(crate) struct Held {
    entries: Vec<Entry>,
    cap: usize,
}

impl Held {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap),
            cap,
        }
    }

    /// Holds `sqe` until serve `ready_at`, or fails with `QueueFull`.
    pub(crate) fn push(&mut self, sqe: Sqe, ready_at: u64) -> Result<(), IoError> {
        if self.entries.len() == self.cap {
            return Err(IoError::QueueFull);
        }
        self.entries.push(Entry {
            sqe,
            ready_at,
            reply: None,
        });
        Ok(())
    }

    /// Takes the unanswered op with `token`, if held.
    pub(crate) fn remove(&mut self, token: Token) -> Option<Sqe> {
        let at = self
            .entries
            .iter()
            .position(|e| e.sqe.token == token && e.reply.is_none())?;
        Some(self.entries.remove(at).sqe)
    }

    pub(crate) fn get_mut(&mut self, at: usize) -> Option<&mut Entry> {
        self.entries.get_mut(at)
    }

    pub(crate) fn remove_at(&mut self, at: usize) {
        if at < self.entries.len() {
            self.entries.remove(at);
        }
    }

    #[cfg(any(test, kani))]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(any(test, kani))]
    pub(crate) fn cap(&self) -> usize {
        self.cap
    }
}
