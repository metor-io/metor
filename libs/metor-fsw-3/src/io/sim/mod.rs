//! Simulated peripherals.
//!
//! A model system holds a `DeviceSide<K>` and calls `serve` once a cycle with
//! its model. `serve` queues each driver request in `Held`, then asks the
//! model to answer the ones that are due. A model that answers `Later` keeps
//! the op held, as a device with no data yet does.

mod held;
mod model;
mod serve;

#[cfg(any(test, kani))]
mod tests;

pub(crate) use held::Held;
pub use model::{GpioModel, Reply, UartModel, fills, serve_gpio, serve_uart};
pub use serve::Model;
