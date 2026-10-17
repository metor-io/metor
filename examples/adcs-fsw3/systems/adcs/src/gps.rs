//! The GPS driver: it reads a NovAtel-style receiver's `#BESTXYZA` logs from
//! a serial port and writes a fix, in ECI, for each solved one.
//!
//! It is a sequence that never ends: one `async fn` that configures the
//! receiver, then reads logs for as long as it runs.

use adcs_contracts::Gps;
use adcs_contracts::novatel::{LOG_10HZ, Lines};
use metor_fsw_3::io::{Buf, Cap, Uart};
use metor_fsw_3::{IoInit, Log, Output, Seq, SeqError, sequence};

/// The bytes one read takes, about one log.
const READ: usize = 256;

/// The driver's buffers, which its body takes when it starts.
pub struct GpsDriver {
    cmd: Option<Buf>,
    rx: Option<Buf>,
}

impl GpsDriver {
    pub fn new(io: &IoInit) -> Self {
        Self {
            cmd: Some(io.alloc_from(LOG_10HZ)),
            rx: Some(io.alloc(READ)),
        }
    }
}

#[sequence]
/// Sets the receiver to log at 10 Hz, then writes a fix for each solved log.
pub async fn gps_driver(
    #[state] s: &mut GpsDriver,
    seq: &Seq,
    uart: &Cap<Uart>,
    fix: &mut Output<Gps>,
    log: &mut Log,
) -> Result<(), SeqError> {
    let (Some(cmd), Some(mut buf)) = (s.cmd.take(), s.rx.take()) else {
        return Err(SeqError::Failed("the driver's buffers are taken".into()));
    };
    configure(seq, uart, cmd, log).await;
    let mut lines = Lines::default();
    loop {
        let (read, back) = uart.read(buf).await;
        buf = back;
        match read {
            Ok(_) => lines.push(&buf),
            Err(e) => log.fault("gps_read", e),
        }
        for line in lines.by_ref().flatten().filter(|l| l.solved) {
            let _ = fix.write(&Gps::from_best_xyz(seq.now(), &line));
        }
    }
}

/// Writes the 10 Hz command once a cycle until the receiver takes it whole.
async fn configure(seq: &Seq<'_>, uart: &Cap<Uart>, mut cmd: Buf, log: &mut Log) {
    loop {
        let (wrote, back) = uart.write(cmd).await;
        cmd = back;
        match wrote {
            Ok(n) if n == LOG_10HZ.len() => return,
            Ok(_) => {}
            Err(e) => log.fault("gps_config", e),
        }
        seq.next_cycle().await;
    }
}
