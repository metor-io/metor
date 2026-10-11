//! Containment for user callbacks and ABI exports.

use core::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) type Payload = Box<dyn Any + Send>;

pub(crate) fn catch<T>(f: impl FnOnce() -> T) -> Option<T> {
    // PANIC Safety: callers retire failed systems or return an ABI error.
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(payload) => {
            discard(payload);
            None
        }
    }
}

pub(crate) fn discard(payload: Payload) {
    // PANIC Safety: a payload's destructor is user code too.
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        // A second payload may also panic on drop; do not unwind the boundary.
        core::mem::forget(secondary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_a_payload_whose_destructor_panics() {
        struct BadDrop;
        impl Drop for BadDrop {
            fn drop(&mut self) {
                panic!("payload drop");
            }
        }
        assert!(catch(|| std::panic::panic_any(BadDrop)).is_none());
        assert_eq!(catch(|| 7), Some(7));
    }
}
