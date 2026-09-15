//! Flight-software framework: frames, typed ports, and systems.

pub mod frame;
pub mod port;
pub mod system;

pub use frame::Frame;
pub use metor_fsw_3_macros::{Frame, SystemInputs, SystemOutputs};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use port::{FrameGrant, Input, Output, capacity_for};
pub use system::{PortDef, System, SystemDef, SystemInputs, SystemOutputs};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

// Paths the component derives expand to.
pub use metor_component::path;
pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use {metor_proto, metor_proto_wkt, zerocopy};
