//! The retired `#[sequence]` attribute, kept as a passthrough.
//!
//! Sequences are ordinary async fns now: ports by value, an optional
//! `Params<P>` wrapper for typed params, registered with
//! `Pack::task("name", the_fn)` and exported through the crate's
//! `export_pack!`. The attribute once generated a `SeqSystem` wrapper and
//! per-crate `fsw_*` exports; both are gone with the pack ABI, so it now
//! emits the annotated fn unchanged. It will be removed once nothing spells
//! it.

use proc_macro::TokenStream;

pub fn sequence(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
