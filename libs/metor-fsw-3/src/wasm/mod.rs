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
