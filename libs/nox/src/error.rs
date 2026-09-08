//! Provides error definitions.
use thiserror::Error;

/// Enumerates possible error types that can occur within the Nox tensor operations.
#[derive(Error, Debug)]
pub enum Error {
    #[error("concat dim failed with dims")]
    InvalidConcatDims,

    /// Error when Cholesky factorization encounters a non-positive pivot.
    #[error("matrix cholesky failed: {0}")]
    Cholesky(#[from] faer::linalg::cholesky::llt::factor::LltError),

    /// faer stack overflow error
    #[error("size overflow")]
    SizeOverflow,
}
