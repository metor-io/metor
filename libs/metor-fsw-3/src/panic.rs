//! Keeps panics in user code from unwinding into the runtime or across an ABI.

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

/// Returns a panic payload's message.
///
/// Returns `"panic"` if the payload is not a string.
pub(crate) fn message(payload: &(dyn Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return text;
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text;
    }
    "panic"
}

pub(crate) fn discard(payload: Payload) {
    // PANIC Safety: a payload's destructor is user code too.
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        // A second payload may also panic on drop; do not unwind the boundary.
        core::mem::forget(secondary);
    }
}

/// Returns a panic payload's message and discards the payload.
pub(crate) fn into_message(payload: Payload) -> String {
    let text = message(&*payload).to_string();
    discard(payload);
    text
}

/// Logs each panic as an `error!` with `kind = "panic"`.
///
/// A panic in a wasm guest aborts the instance, so this line is the only
/// report of it.
#[cfg(target_family = "wasm")]
pub(crate) fn log_panics() {
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(kind = "panic", "{info}");
    }));
}

/// Does nothing on native targets.
///
/// A native panic unwinds to `catch_step`, which reports it.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn log_panics() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catch_panicking_payload_drop() {
        struct BadDrop;
        impl Drop for BadDrop {
            fn drop(&mut self) {
                panic!("payload drop");
            }
        }
        assert!(catch(|| std::panic::panic_any(BadDrop)).is_none());
        assert_eq!(catch(|| 7), Some(7));
    }

    #[test]
    fn test_into_message() {
        assert_eq!(into_message(Box::new("static")), "static");
        assert_eq!(into_message(Box::new(String::from("owned"))), "owned");
        assert_eq!(into_message(Box::new(7u8)), "panic");
    }
}
