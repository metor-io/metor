//! Packs built as WebAssembly modules, each system instance in a guest of
//! its own.

mod abi;
mod guest;
mod pack;
mod step;
#[cfg(test)]
mod tests;

pub use guest::{CreateArgs, Guest, Module, Out, PAGE, TABLE_ELEMENTS, Trap, WasmError};
pub use pack::{Fuel, WasmPack};
pub use step::WasmStep;

/// Each guest's memory ceiling when a pack sets none.
pub const DEFAULT_MEMORY: usize = 16 << 20;

/// Each step's fuel grant when a pack sets none.
pub const DEFAULT_FUEL: u64 = 10_000_000;

/// The grant for loading and creating: instantiation, the descriptor, `def`,
/// and `create`.
pub const DEFAULT_SETUP_FUEL: u64 = 100 * DEFAULT_FUEL;
