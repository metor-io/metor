//! metor-fsw is a flight software framework

pub mod cli;
pub mod coordinator;
pub mod dl;
pub mod fn_system;
pub mod frame;
pub mod log;
pub mod pack;
mod panic;
pub mod port;
pub mod record;
pub mod system;

#[cfg(test)]
mod tests;

pub use coordinator::{
    BuildError, Clock, Coordinator, CoordinatorConfig, InputConfig, ParamError, Params, PortRef,
    Step, SystemConfig, SystemStatus, SystemTable,
};
pub use dl::{DlStep, Pack, PackError, PackFns};
pub use fn_system::{Ctor, Cycle, FnSystem, InSet, Names, OutSet, Param, SystemFn};
pub use frame::Frame;
pub use log::{Log, LogLayer};
pub use metor_fsw_3_macros::{Frame, Record, SystemInputs, SystemOutputs, system};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use pack::def::{PackDef, PackSystemDef};
pub use pack::{ABI_VERSION, Status};
pub use port::{Input, Latest, Output, RecvError, SendError, ring_capacity};
pub use record::{DecodeError, EncodeError, Record};
pub use system::{PortDef, System, SystemDef, SystemInputs, SystemOutputs};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

pub use metor_component::path;
pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_proto;
pub use metor_proto_wkt;
pub use postcard;
pub use postcard::experimental::max_size::MaxSize;
pub use schemars::{self, JsonSchema};
pub use serde;
pub use serde_json;
pub use tracing;
pub use zerocopy;
