//! Provides error definitions.
use thiserror::Error;

/// Enumerates possible error types that can occur within the Nox tensor operations.
#[derive(Error, Debug)]
pub enum Error {
    #[error("concat dim failed with dims")]
    InvalidConcatDims,

    /// Error when matrix inversion failed
    #[error("matrix cholesky failed with {0} arg illegal")]
    Cholesky(#[from] faer::linalg::cholesky::llt::CholeskyError),

    /// faer stack overflow error
    #[error("size overflow")]
    SizeOverflow,
}
