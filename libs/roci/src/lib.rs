pub use metor_proto;
pub use metor_proto_wkt;
pub use roci_macros::{AsVTable, Metadatatize};
pub use vtable::AsVTable;
pub use zerocopy;

mod nox;
pub mod path;
pub mod update;
mod vtable;

#[cfg(feature = "stellar")]
pub mod tcp;

#[cfg(feature = "std")]
pub mod metadata;
#[cfg(feature = "std")]
pub use metadata::Metadatatize;
