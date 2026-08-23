//! The program pane: a module of Python systems, running against live
//! telemetry.
//!
//! The pane is a text editor and a status list, and the whole design is in why
//! it needs nothing else. The source is the artifact — diffable, pasteable,
//! promotable to a vehicle whole — so the pane stores text and derives
//! everything from it. Compile on a debounce, keep the systems whose source
//! did not change, rebuild the ones that did, and show what each is doing.
//!
//! ## Rebuilding is per system, not per module
//!
//! An edit recompiles the module, because a module is what the compiler takes.
//! It does not restart the module: each system is identified by the source it
//! was written in plus what its ports resolved to, so an edit to one body
//! leaves every other system's identity — and therefore its node, its task,
//! and its state — untouched. A system that *did* change is rebuilt with the
//! state its predecessor had, matched field by field by
//! [`metor_expr::state`].
//!
//! ## Diagnostics point at text
//!
//! A compiler diagnostic carries a byte range, and the editor underlines byte
//! ranges, so a type error marks the expression that caused it rather than
//! naming a line number. A program that does not compile leaves the previous
//! one running: half-typed source is the normal case, and dropping live
//! systems on every keystroke would make the pane unusable.

mod pane;

pub use pane::{ProgramPane, ProgramPaneConfig};
