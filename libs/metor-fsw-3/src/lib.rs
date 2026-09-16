//! metor-fsw is a flight software framework

pub mod coordinator;
pub mod fn_system;
pub mod frame;
pub mod port;
pub mod record;
pub mod system;

#[cfg(test)]
mod tests;

pub use coordinator::{
    BuildError, Clock, Coordinator, CoordinatorConfig, InputConfig, ParamError, Params, PortRef,
    Step, SystemConfig, SystemStatus, SystemTable,
};
pub use fn_system::{Ctor, FnSystem, InSet, OutSet, Param, ParamSet, SystemFn};
pub use frame::Frame;
pub use metor_fsw_3_macros::{Frame, Record, SystemInputs, SystemOutputs, system};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use port::{FrameGrant, Input, Output, ring_capacity};
pub use record::{DecodeError, EncodeError, Record};
pub use system::{PortDef, System, SystemDef, SystemInputs, SystemOutputs};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

// Paths the component derives expand to.
pub use metor_component::path;
pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use postcard::experimental::max_size::MaxSize;
pub use {metor_proto, metor_proto_wkt, postcard, serde, serde_json, zerocopy};
