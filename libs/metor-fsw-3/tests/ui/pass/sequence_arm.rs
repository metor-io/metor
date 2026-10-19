use core::time::Duration;

use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{
    FnSignature, Frame, Input, Log, Output, Seq, SeqError, SystemTable, Timestamp, sequence,
};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct MotorHealth {
    #[frame(timestamp)]
    timestamp: Timestamp,
    healthy: u64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Arm {
    #[frame(timestamp)]
    timestamp: Timestamp,
    on: u64,
}

#[derive(Default)]
struct Attempts {
    count: u32,
}

/// Powers the motors, waits for them to report healthy, then arms control.
#[sequence]
async fn arm_controls(
    #[state] attempts: &mut Attempts,
    seq: &Seq,
    log: &mut Log,
    motor_en: &mut Output<Arm>,
    motor_health: &mut Input<MotorHealth>,
    control_arm: &mut Output<Arm>,
) -> Result<(), SeqError> {
    attempts.count += 1;
    let _ = motor_en.write(&Arm {
        timestamp: seq.now(),
        on: 1,
    });
    let healthy = || {
        motor_health
            .latest()
            .ok()
            .flatten()
            .is_some_and(|h| h.healthy == 1)
    };
    seq.wait_for(Duration::from_secs(1), healthy).await?;
    log.info("motors healthy");
    seq.sleep(Duration::from_millis(500)).await?;
    let _ = control_arm.write(&Arm {
        timestamp: seq.now(),
        on: 1,
    });
    Ok(())
}

fn names<P: FnSignature>(_: &P) -> (&'static str, &'static [&'static str]) {
    (P::NAME, P::NAMES)
}

fn main() {
    assert_eq!(
        names(&arm_controls()),
        (
            "arm_controls",
            &["seq", "log", "motor_en", "motor_health", "control_arm"][..]
        )
    );
    let mut table = SystemTable::new();
    table
        .register_sequence("arm_controls", arm_controls(), Attempts::default)
        .expect("valid records");
}
