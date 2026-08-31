//! Constructors for [`DynamicNode`](super::DynamicNode)s.
//!
//! What is left here is the host-shaped machinery: the clock a source system
//! is driven by, the two bridges to the database (`from_db` / `persist`), and
//! the runtime that drives a compiled program. The arithmetic that used to
//! live beside them — twenty-one op constructors and the node kinds above
//! them — is now the language's, where one line replaces five nodes and eight
//! edges.

pub mod clock;
pub mod db_source;
pub mod persist;
pub mod program;
#[cfg(test)]
mod program_measure;
#[cfg(test)]
mod program_tests;
pub mod replay;
#[cfg(test)]
mod replay_tests;
