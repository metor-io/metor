//! The GPS driver: it reads a NovAtel-style receiver's `#BESTXYZA` logs from
//! a serial port and writes a fix, in ECI, for each solved one.
//!
//! It is a sequence that never ends: one `async fn` that configures the
//! receiver, then reads logs for as long as it runs.

use adcs_contracts::Gps;
use adcs_contracts::novatel::{LOG_10HZ, Lines};
use metor_fsw_3::io::{Backoff, Buf, Cap, Handle, Uart};
use metor_fsw_3::{IoInit, Log, MakeError, Output, Seq, SeqError, sequence};

/// The bytes one read takes, about one log.
const READ: usize = 256;

/// The driver's buffers, which its body takes when it starts.
pub struct GpsDriver {
    cmd: Option<Buf>,
    rx: Option<Buf>,
}

impl GpsDriver {
    pub fn new(io: &mut IoInit) -> Result<Self, MakeError> {
        Ok(Self {
            cmd: Some(io.alloc_from(LOG_10HZ)?),
            rx: Some(io.alloc(READ)?),
        })
    }
}

#[sequence]
/// Connects to the receiver, sets it to log at 10 Hz, then writes a fix for
/// each solved log. A lost port is connected and configured again.
pub async fn gps_driver(
    #[state] s: &mut GpsDriver,
    seq: &Seq,
    uart: &Cap<Uart>,
    fix: &mut Output<Gps>,
    log: &mut Log,
) -> Result<(), SeqError> {
    let (Some(mut cmd), Some(mut buf)) = (s.cmd.take(), s.rx.take()) else {
        return Err(SeqError::Failed("the driver's buffers are taken".into()));
    };
    let mut backoff = Backoff::doubling();
    loop {
        let port = match uart.connect().await {
            Ok(port) => port,
            Err(e) => {
                log.fault("gps_connect", e);
                let _ = uart.timeout(backoff.next_wait()).await;
                continue;
            }
        };
        backoff.reset();
        let configured;
        (cmd, configured) = configure(seq, &port, cmd, log).await;
        if configured {
            buf = read_until_lost(seq, &port, buf, fix, log).await;
        }
        let _ = port.close().await;
    }
}

/// Writes the 10 Hz command once a cycle until the receiver takes it whole.
/// False if the port is lost first.
async fn configure(seq: &Seq<'_>, port: &Handle<Uart>, mut cmd: Buf, log: &mut Log) -> (Buf, bool) {
    loop {
        let (wrote, back) = port.write(cmd).await;
        cmd = back;
        match wrote {
            Ok(n) if n == LOG_10HZ.len() => return (cmd, true),
            Ok(_) => {}
            Err(e) if e.is_lost() => {
                log.fault("gps_lost", e);
                return (cmd, false);
            }
            Err(e) => log.fault("gps_config", e),
        }
        seq.next_cycle().await;
    }
}

/// Reads logs from `port` until it hangs up or is lost, writing a fix for
/// each solved one.
async fn read_until_lost(
    seq: &Seq<'_>,
    port: &Handle<Uart>,
    mut buf: Buf,
    fix: &mut Output<Gps>,
    log: &mut Log,
) -> Buf {
    let mut lines = Lines::default();
    loop {
        let (read, back) = port.read(buf).await;
        buf = back;
        match read {
            Ok(0) => return buf,
            Ok(_) => lines.push(&buf),
            Err(e) if e.is_lost() => {
                log.fault("gps_lost", e);
                return buf;
            }
            Err(e) => log.fault("gps_read", e),
        }
        for line in lines.by_ref().flatten().filter(|l| l.solved) {
            let _ = fix.write(&Gps::from_best_xyz(seq.now(), &line));
        }
    }
}
