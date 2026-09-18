//! metor-fsw is a flight software framework

pub mod async_system;
pub mod cli;
pub mod coordinator;
pub mod dl;
pub mod fn_system;
pub mod frame;
pub mod link;
pub mod log;
pub mod pack;
mod panic;
pub mod port;
pub mod record;
pub mod system;
pub mod thread;

#[cfg(test)]
mod tests;

pub use async_system::{AsyncSystem, Stop};
pub use coordinator::{
    BuildError, Clock, Coordinator, CoordinatorConfig, InputConfig, OutputConfig, ParamError,
    Params, PortRef, Step, SystemConfig, SystemStatus, SystemTable,
};
pub use dl::{DlStep, Pack, PackError, PackFns};
pub use fn_system::{
    AsyncSystemFn, Ctor, Cycle, FnAsyncSystem, FnSystem, InSet, Names, OutSet, Param, Ports,
    SystemFn,
};
pub use frame::Frame;
pub use link::{LinkStatus, Transport};
pub use log::{Log, LogLayer};
pub use metor_fsw_3_macros::{Frame, Record, SystemInputs, SystemOutputs, system};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use pack::def::{PackDef, PackSystemDef};
pub use pack::{ABI_VERSION, Status};
pub use port::{DynInputs, DynOutputs, Input, Latest, Output, RecvError, SendError, ring_capacity};
pub use record::{Bytes, DecodeError, EncodeError, MsgCodec, Record, RecordSchema};
pub use system::{
    InputBinding, OutputBinding, PortDef, System, SystemDef, SystemInputs, SystemOutputs,
};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

pub use metor_component::path;
pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_proto;
pub use metor_proto_wkt;
pub use postcard;
pub use postcard::experimental::max_size::MaxSize;
pub use postcard_schema;
pub use postcard_schema::Schema;
pub use schemars::{self, JsonSchema};
pub use serde;
pub use serde_json;
pub use tracing;
pub use zerocopy;
