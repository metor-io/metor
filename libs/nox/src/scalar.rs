//! Provides functionality for managing scalar tensors, including operations and transformations between host and client representations.
use crate::DefaultRepr;
use crate::{ScalarDim, Tensor};

/// Type alias for a scalar tensor with a specific type `T`, an underlying representation `P`.
pub type Scalar<T, P = DefaultRepr> = Tensor<T, ScalarDim, P>;

#[cfg(test)]
mod tests {

    // #[test]
    // fn test_sqrt_log_opt() {
    //     // log operation not available for ArrayRepr
    //     let a = Scalar::from(3.141592653589793);
    //     let out = a.sqrt().log();
    //     assert_eq!(out, 0.5723649.into());
    // }
}
