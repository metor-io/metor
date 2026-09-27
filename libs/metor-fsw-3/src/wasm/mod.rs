//! Packs built as WebAssembly modules, each system instance in a guest of
//! its own.

mod guest;
#[cfg(test)]
mod tests;

pub use guest::{CreateArgs, Guest, Module, Out, PAGE, TABLE_ELEMENTS, Trap, WasmError};
