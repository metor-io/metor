//! mc-rt runs compiled metor-code as metor-fsw-3 systems.
//!
//! A compiled program is a [`ProgramDesc`](desc::ProgramDesc) and a table of
//! functions. [`table::register`] turns each of its types into an fsw-3
//! system type. Each step copies the newest inputs into a flat block, calls
//! one compiled function with an [`McCx`](cx::McCx), and writes the outputs
//! it marked, stamped with the cycle's time. Compiled code never touches a
//! ring.
//!
//! [`harness::Harness`] steps one instance without a coordinator, for tests.

pub mod cx;
mod cyclic;
pub mod desc;
pub mod harness;
pub mod import;
pub mod instance;
pub mod math;
pub mod ports;
pub mod program;
pub mod report;
pub mod schema;
mod sequence;
pub mod table;

pub use cx::{EntryFn, Fault, McCx, ResumeWord};
pub use desc::ProgramDesc;
pub use harness::Harness;
pub use program::{McProgram, Program, ProgramError};
pub use table::{register, table};
