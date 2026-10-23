//! Star catalogs: the onboard DB (format, sky index and k-vector), its
//! builder, and a synthetic catalog for tests.

pub mod build;
pub mod cells;
pub mod format;
pub mod kvector;
pub mod synthetic;

pub use build::{BuildConfig, BuildError, CatStar};
pub use format::{Db, DbError};

#[cfg(test)]
mod tests;
