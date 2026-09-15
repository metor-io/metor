//! Flight-software framework: frames, typed ports, and systems.
//!
//! A [`Frame`] is a fixed `#[repr(C)]` struct whose bytes are one ring record.
//! A [`System`] reads [`Input`] ports and writes [`Output`] ports, keeping its
//! mutable data in `State`. A [`Coordinator`] builds a graph of systems from a
//! [`CoordinatorConfig`] and a [`SystemTable`], then steps them in list order,
//! once per cycle.
//!
//! ```
//! use metor_fsw_3::*;
//! use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//!
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! struct Imu {
//!     #[frame(timestamp)]
//!     timestamp: Timestamp,
//!     omega: f64,
//! }
//!
//! #[derive(SystemOutputs)]
//! struct GyroOut {
//!     imu: Output<Imu>,
//! }
//!
//! struct Gyro;
//!
//! impl System for Gyro {
//!     type State = i64;
//!     type Inputs = ();
//!     type Outputs = GyroOut;
//!
//!     fn def() -> SystemDef {
//!         SystemDef::new::<(), GyroOut>("gyro")
//!     }
//!
//!     fn execute(&self, tick: &mut i64, _inputs: &mut (), outputs: &mut GyroOut) {
//!         *tick += 1;
//!         let _ = outputs.imu.write(&Imu { timestamp: Timestamp(*tick), omega: 0.1 });
//!     }
//! }
//!
//! fn main() -> Result<(), BuildError> {
//!     let mut table = SystemTable::new();
//!     table.register("gyro", || (Gyro, 0));
//!
//!     let config = CoordinatorConfig {
//!         systems: vec![SystemConfig { id: "imu".into(), ty: "gyro".into(), inputs: vec![] }],
//!         ..Default::default()
//!     };
//!
//!     let mut coordinator = Coordinator::build(config, &table)?;
//!     coordinator.step(Timestamp::now());
//!     assert_eq!(coordinator.cycle(), 1);
//!     Ok(())
//! }
//! ```
//!
//! A consumer names its producers in the config: an [`InputConfig`] lists one
//! [`PortRef`] per edge, and an input with no edges is unconnected. Every
//! system also gets a coordinator-owned `status` output carrying
//! [`SystemStatus`], which a later system may read like any other ring.

pub mod coordinator;
pub mod frame;
pub mod port;
pub mod system;

pub use coordinator::{
    BuildError, Clock, Coordinator, CoordinatorConfig, InputConfig, PortRef, Step, SystemConfig,
    SystemStatus, SystemTable,
};
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
