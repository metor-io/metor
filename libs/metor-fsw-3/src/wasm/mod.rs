//! Runs packs built as WebAssembly modules.
//!
//! Each system instance runs in its own guest, with its own memory. A guest
//! works on copies of the host's rings, so it never touches host memory. Each
//! call into a guest has a fuel budget, so a guest cannot stall a cycle
//! forever.

mod abi;
mod guest;
mod pack;
mod step;
#[cfg(test)]
mod tests;

pub use guest::{PAGE, Trap, WasmError};
pub use pack::{Limits, WasmPack};
pub use step::WasmStep;

/// The memory ceiling for each guest when the pack does not set one.
const DEFAULT_MEMORY: usize = 16 << 20;

/// The fuel each step gets when the pack does not set a limit.
const DEFAULT_FUEL: u64 = 10_000_000;

/// The fuel for each setup call.
///
/// Setup covers instantiation, reading the descriptor, `def`, `create`, and
/// the allocator calls around them.
const DEFAULT_SETUP_FUEL: u64 = 100 * DEFAULT_FUEL;
