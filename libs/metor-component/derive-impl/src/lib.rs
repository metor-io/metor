//! The `quote!` bodies shared by `metor-component-macros` and
//! `metor-fsw-2-macros`'s component derives (`AsVTable`, `Metadatatize`,
//! `Componentize`, `Decomponentize`).
//!
//! This crate holds no proc-macro entry points — a proc-macro crate cannot
//! export plain functions, so the two macro crates each keep their own
//! `#[proc_macro_derive]` fns and darling-based input parsing, then hand the
//! parsed [`Input`]/[`StructInput`] here to build the actual
//! [`proc_macro2::TokenStream`].

mod as_vtable;
mod componentize;
mod decomponentize;
mod field;
mod input;
mod metadatatize;

pub use as_vtable::as_vtable_impl;
pub use componentize::componentize_impl;
pub use decomponentize::decomponentize_impl;
pub use field::Field;
pub use input::{EnumInput, Input, StructInput};
pub use metadatatize::metadatatize_impl;
