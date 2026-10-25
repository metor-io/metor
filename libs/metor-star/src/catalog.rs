//! The onboard star DB (sky index and k-vector), built in memory from the
//! render catalog, and a synthetic catalog for tests.

pub mod build;
pub mod cells;
pub mod db;
pub mod kvector;
pub mod synthetic;

pub use build::{BuildConfig, BuildError, CatStar, Select};
pub use db::Db;

#[cfg(test)]
mod tests;
