#[cfg(feature = "nox")]
use metor_proto::component::Component;
use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};

mod image;
mod metadata;
mod msgs;
mod path;
mod tile;
#[cfg(feature = "nox")]
mod value;

pub use image::*;
pub use metadata::*;
pub use msgs::*;
pub use path::*;
pub use tile::*;
#[cfg(feature = "nox")]
pub use value::*;

#[derive(Clone, Serialize, Deserialize, Default, Copy)]
pub struct Tick(pub u64);

#[cfg(feature = "nox")]
impl metor_proto::com_de::Decomponentize for Tick {
    type Error = core::convert::Infallible;

    fn apply_value(
        &mut self,
        component_id: metor_proto::types::ComponentId,
        value: metor_proto::types::ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        if component_id != Tick::COMPONENT_ID {
            return Ok(());
        }
        let metor_proto::types::ComponentView::U64(view) = value else {
            return Ok(());
        };
        let buf = view.buf();
        self.0 = buf[0];
        Ok(())
    }
}

impl metor_proto::component::Component for Tick {
    const NAME: &'static str = "tick";

    fn schema() -> metor_proto::schema::Schema<Vec<u64>> {
        metor_proto::schema::Schema::new(metor_proto::types::PrimType::U64, [0u64; 0])
            .expect("failed to create schema")
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SimulationTimeStep(pub f64);

impl SimulationTimeStep {
    pub fn as_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.0)
    }
}

impl metor_proto::component::Component for SimulationTimeStep {
    const NAME: &'static str = "simulation_time_step";
    const ASSET: bool = false;

    fn schema() -> metor_proto::schema::Schema<Vec<u64>> {
        metor_proto::schema::Schema::new(metor_proto::types::PrimType::F64, [0usize; 0])
            .expect("failed to create schema")
    }
}

#[cfg(feature = "nox")]
impl metor_proto::com_de::Decomponentize for SimulationTimeStep {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        component_id: metor_proto::types::ComponentId,
        value: metor_proto::types::ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        if component_id != SimulationTimeStep::COMPONENT_ID {
            return Ok(());
        }
        let metor_proto::types::ComponentView::F64(view) = value else {
            return Ok(());
        };
        let buf = view.buf();
        self.0 = buf[0];
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, Copy)]
pub struct LastUpdated(pub Timestamp);

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct IsRecording(pub bool);

#[cfg(feature = "nox")]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WorldPos {
    pub att: nox::Quaternion<f64, nox::ArrayRepr>,
    pub pos: nox::Vector3<f64, nox::ArrayRepr>,
}

#[cfg(feature = "nox")]
impl Component for WorldPos {
    const NAME: &'static str = "world_pos";
    const ASSET: bool = false;

    #[cfg(feature = "std")]
    fn schema() -> metor_proto::schema::Schema<Vec<u64>> {
        metor_proto::schema::Schema::new(metor_proto::types::PrimType::F64, [7usize])
            .expect("failed to create schema")
    }
}

#[cfg(feature = "nox")]
impl metor_proto::com_de::Decomponentize for WorldPos {
    type Error = core::convert::Infallible;

    fn apply_value(
        &mut self,
        component_id: metor_proto::types::ComponentId,
        value: metor_proto::types::ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        if component_id != WorldPos::COMPONENT_ID {
            return Ok(());
        }
        let metor_proto::types::ComponentView::F64(view) = value else {
            return Ok(());
        };
        let buf = view.buf();
        let att: [f64; 4] = buf[..4].try_into().expect("slice size wrong");
        self.att = nox::Quaternion(nox::Tensor::from_buf(att));
        let pos: [f64; 3] = buf[4..].try_into().expect("slice size wrong");
        self.pos = nox::Tensor::from_buf(pos);
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, Copy)]
pub struct CurrentTimestamp(pub Timestamp);

impl Default for CurrentTimestamp {
    fn default() -> Self {
        Self(Timestamp::EPOCH)
    }
}

impl metor_proto::component::Component for CurrentTimestamp {
    const NAME: &'static str = "current_timestamp";

    fn schema() -> metor_proto::schema::Schema<Vec<u64>> {
        metor_proto::schema::Schema::new(metor_proto::types::PrimType::I64, [1usize])
            .expect("failed to create schema")
    }
}

#[cfg(feature = "nox")]
impl metor_proto::com_de::Decomponentize for CurrentTimestamp {
    type Error = core::convert::Infallible;

    fn apply_value(
        &mut self,
        component_id: metor_proto::types::ComponentId,
        value: metor_proto::types::ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        if component_id != CurrentTimestamp::COMPONENT_ID {
            return Ok(());
        }
        let metor_proto::types::ComponentView::I64(view) = value else {
            return Ok(());
        };
        let buf = view.buf();
        self.0 = Timestamp(buf[0]);
        Ok(())
    }
}
