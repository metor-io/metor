//! Run with `cargo run -p metor-fsw-2-core --example peer_contract` to export
//! a contract shared by an FSW target and its plant. No remote process is needed.

use metor_fsw_2_core::peer::{EndpointContract, FrameChannel};
// An example in the core package must expose its re-exports at crate root too:
// proc-macro-crate identifies this package as `crate`.
use metor_fsw_2_core::*;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: [f64; 3],
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = FrameChannel::of::<Imu>("imu_a", Delivery::Snapshot)?;
    a.semantics.insert("omega.units".into(), "rad/s".into());
    a.semantics
        .insert("omega.coordinate_frame".into(), "body".into());
    let mut b = a.clone();
    b.key = "imu_b".into();
    let contract = EndpointContract::new("plant_sensors", "1.0", "simulation", vec![a, b])?;
    eprintln!("{}", contract.hash()?);
    println!("{}", contract.to_json()?);
    Ok(())
}
