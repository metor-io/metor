//! A simulated NovAtel-style GPS receiver on a serial port.
//!
//! It holds the device end of the line. It writes `#BESTXYZA` logs from the
//! plant's true orbit state, with Gauss-Markov position noise, and reads the
//! driver's commands. `LOG BESTXYZA ONTIME 0.1` sets the log period from 1 s
//! to 0.1 s. During an outage the logs carry no solution.
//!
//! It is a sequence that never ends: a loop reading commands joined with a
//! loop writing logs.
//!
//! Time counts from the first `BodyState`, as `nav` counts from its first
//! sensor sample, so both see the same epoch.

use adcs_contracts::novatel::{self, BestXyz, LINE_MAX};
use core::cell::Cell;

use adcs_contracts::{BodyState, GpsFault, V3, eci_to_ecef_state, epoch_at, gps_epoch, gps_time};
use futures_lite::future;
use metor_fsw_3::io::{Buf, Cap, IoInit, Uart};
use metor_fsw_3::{Input, Seq, SeqError, Timestamp, sequence};
use rand::{SeedableRng, rngs::StdRng};
use rand_distr::{Distribution, Normal};
use schemars::JsonSchema;
use serde::Deserialize;

/// The command bytes each read takes.
const COMMAND_READ: usize = 64;
/// The time constant of the position error (s).
const TAU: f64 = 100.0;

/// The params for `gps_receiver`.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct GpsReceiverParams {
    /// Seeds the receiver's noise.
    pub seed: u64,
    /// Position error standard deviation per axis (m).
    #[serde(default = "default_pos_sigma")]
    pub pos_sigma: f64,
    /// Velocity noise standard deviation per axis (m/s).
    #[serde(default = "default_vel_sigma")]
    pub vel_sigma: f64,
}

fn default_pos_sigma() -> f64 {
    5.0
}

fn default_vel_sigma() -> f64 {
    0.05
}

/// The receiver's buffers, which its body takes when it starts, and its logs.
pub struct GpsReceiver {
    rx: Option<Buf>,
    tx: Option<Buf>,
    logger: Logger,
}

impl GpsReceiver {
    pub fn new(p: GpsReceiverParams, io: &IoInit) -> Self {
        Self {
            rx: Some(io.alloc(COMMAND_READ)),
            tx: Some(io.alloc(LINE_MAX)),
            logger: Logger::new(p),
        }
    }
}

#[sequence]
/// Serves the GPS receiver's serial port from the plant's true state.
pub async fn gps_receiver(
    #[state] m: &mut GpsReceiver,
    seq: &Seq,
    uart: &Cap<Uart>,
    truth: &mut Input<BodyState>,
    faults: &mut Input<GpsFault>,
) -> Result<(), SeqError> {
    let (Some(rx), Some(tx)) = (m.rx.take(), m.tx.take()) else {
        return Err(SeqError::Failed("the receiver's buffers are taken".into()));
    };
    let period = Cell::new(None);
    let logs = logs(seq, uart, tx, &mut m.logger, truth, faults, &period);
    future::zip(commands(uart, rx, &period), logs).await;
    Ok(())
}

/// Reads the driver's commands, passing on each log period one sets.
async fn commands(uart: &Cap<Uart>, mut buf: Buf, period: &Cell<Option<f64>>) {
    let mut line = Command::default();
    loop {
        let (read, back) = uart.read(buf).await;
        buf = back;
        if read.is_ok()
            && let Some(p) = line.push(&buf)
        {
            period.set(Some(p));
        }
    }
}

/// Writes each log as it falls due, one at a time.
async fn logs(
    seq: &Seq<'_>,
    uart: &Cap<Uart>,
    mut buf: Buf,
    logger: &mut Logger,
    truth: &mut Input<BodyState>,
    faults: &mut Input<GpsFault>,
    period: &Cell<Option<f64>>,
) {
    let mut line = [0; LINE_MAX];
    loop {
        if let Some(p) = period.take() {
            logger.set_period(p);
        }
        if let Ok(Some(fault)) = faults.latest() {
            logger.outage = fault.outage != 0;
        }
        let due = match truth.latest() {
            Ok(Some(body)) => logger.update(&body, &mut line),
            _ => None,
        };
        match due {
            Some(n) => {
                buf.fill(&line[..n]);
                (_, buf) = uart.write(buf).await;
            }
            None => {
                seq.next_cycle().await;
            }
        }
    }
}

/// The command line the driver is writing.
struct Command {
    bytes: [u8; COMMAND_READ],
    len: usize,
}

impl Default for Command {
    fn default() -> Self {
        Self {
            bytes: [0; COMMAND_READ],
            len: 0,
        }
    }
}

impl Command {
    /// Takes command bytes. Returns the last log period a complete line set.
    fn push(&mut self, bytes: &[u8]) -> Option<f64> {
        let mut period = None;
        for &b in bytes {
            if self.len == self.bytes.len() {
                self.len = 0;
            }
            self.bytes[self.len] = b;
            self.len += 1;
            if b == b'\n' {
                period = parse_period(&self.bytes[..self.len]).or(period);
                self.len = 0;
            }
        }
        period
    }
}

/// The period a `LOG BESTXYZA ONTIME` line sets.
fn parse_period(line: &[u8]) -> Option<f64> {
    let line = core::str::from_utf8(line).ok()?;
    let (_, period) = line.split_once("LOG BESTXYZA ONTIME ")?;
    period.trim().parse().ok().filter(|p: &f64| *p > 0.0)
}

/// When logs fall due, and the solution and noise they carry.
struct Logger {
    period: f64,
    next_log: f64,
    first: Option<Timestamp>,
    outage: bool,
    err: V3,
    pos_sigma: f64,
    vel_sigma: f64,
    rng: StdRng,
    unit: Normal<f64>,
}

impl Logger {
    fn new(p: GpsReceiverParams) -> Self {
        Self {
            period: 1.0,
            next_log: 0.0,
            first: None,
            outage: false,
            err: V3::zeros(),
            pos_sigma: p.pos_sigma,
            vel_sigma: p.vel_sigma,
            rng: StdRng::seed_from_u64(p.seed),
            // PANIC Safety: a unit standard deviation is valid.
            unit: Normal::new(0.0, 1.0).expect("a unit normal"),
        }
    }

    /// Sets the seconds between logs, and logs next at once.
    fn set_period(&mut self, period: f64) {
        self.period = period;
        self.next_log = 0.0;
    }

    /// Encodes a log of `body` into `line` once per period of the truth's own
    /// time. Returns its length.
    fn update(&mut self, body: &BodyState, line: &mut [u8; LINE_MAX]) -> Option<usize> {
        let first = *self.first.get_or_insert(body.timestamp);
        let t = (body.timestamp - first).as_secs_f64();
        if t < self.next_log {
            return None;
        }
        // Keep the cadence while on time, and restart it from `t` when late.
        let next = self.next_log + self.period;
        self.next_log = if next > t { next } else { t + self.period };
        let log = self.log(body, t);
        novatel::encode(&log, line)
    }

    /// The log of `body` at `t` seconds since the first state.
    fn log(&mut self, body: &BodyState, t: f64) -> BestXyz {
        let (week, seconds) = gps_time(epoch_at(t));
        let mut log = BestXyz {
            week,
            seconds,
            solved: !self.outage,
            pos_ecef: [0.0; 3],
            vel_ecef: [0.0; 3],
            pos_sigma: self.pos_sigma,
            sats: if self.outage { 0 } else { 9 },
        };
        if self.outage {
            return log;
        }
        let phi = (-self.period / TAU).exp();
        self.err = phi * self.err + self.noise(self.pos_sigma * (1.0 - phi * phi).sqrt());
        let pos = body.pos_eci + self.err;
        let vel = body.vel_eci + self.noise(self.vel_sigma);
        let (pos, vel) = eci_to_ecef_state(gps_epoch(week, seconds), &pos, &vel);
        log.pos_ecef = pos.into_buf();
        log.vel_ecef = vel.into_buf();
        log
    }

    fn noise(&mut self, sigma: f64) -> V3 {
        let mut draw = || self.unit.sample(&mut self.rng) * sigma;
        V3::from_buf([draw(), draw(), draw()])
    }
}

#[cfg(test)]
mod tests {
    use adcs_contracts::Quat;
    use adcs_contracts::novatel::{LOG_10HZ, Lines};

    use super::*;

    /// The cycle period the tests run at (s).
    const STEP: f64 = 0.01;

    /// A body on a straight line through a 400 km orbit radius, at `t`.
    fn body(t: f64) -> BodyState {
        let (pos, vel) = (
            V3::from_buf([6.78e6, 0.0, 0.0]),
            V3::from_buf([0.0, 7670.0, 0.0]),
        );
        BodyState {
            timestamp: Timestamp((t * 1e6).round() as i64),
            q_b_eci: Quat::identity(),
            omega_b: V3::zeros(),
            pos_eci: pos + vel * t,
            vel_eci: vel,
        }
    }

    fn logger() -> Logger {
        Logger::new(GpsReceiverParams {
            seed: 7,
            pos_sigma: 5.0,
            vel_sigma: 0.0,
        })
    }

    /// The logs `l` writes, one cycle at a time, from `from` for `seconds`.
    fn run(l: &mut Logger, from: f64, seconds: f64) -> Vec<BestXyz> {
        let (mut lines, mut line) = (Lines::default(), [0; LINE_MAX]);
        let mut logs = Vec::new();
        for k in 0..=(seconds / STEP).round() as usize {
            if let Some(n) = l.update(&body(from + k as f64 * STEP), &mut line) {
                lines.push(&line[..n]);
                logs.extend(lines.by_ref().map(Result::unwrap));
            }
        }
        logs
    }

    #[test]
    fn logs_once_a_second_until_commanded_to_ten_hertz() {
        let mut l = logger();
        assert_eq!(run(&mut l, 0.0, 2.0).len(), 3, "at 0, 1, and 2 s");
        l.set_period(Command::default().push(LOG_10HZ).unwrap());
        assert_eq!(l.period, 0.1);
        assert!((9..=10).contains(&run(&mut l, 2.01, 0.99).len()));
    }

    #[test]
    fn a_command_split_across_reads_is_one_line() {
        let mut c = Command::default();
        assert_eq!(c.push(&LOG_10HZ[..9]), None);
        assert_eq!(c.push(&LOG_10HZ[9..]), Some(0.1));
    }

    #[test]
    fn a_bad_or_overlong_command_sets_nothing() {
        let mut c = Command::default();
        assert_eq!(c.push(b"LOG BESTXYZA ONTIME -1\r\n"), None);
        assert_eq!(c.push(b"LOG BESTXYZA ONTIME fast\r\n"), None);
        assert_eq!(c.push(&[b'x'; COMMAND_READ + 4]), None);
        assert_eq!(c.push(b"\r\n"), None);
        assert_eq!(c.push(LOG_10HZ), Some(0.1));
    }

    #[test]
    fn an_outage_logs_no_solution() {
        let mut l = logger();
        l.outage = true;
        let logs = run(&mut l, 0.0, 1.0);
        assert!(!logs.is_empty() && logs.iter().all(|l| !l.solved && l.sats == 0));
        l.outage = false;
        assert!(run(&mut l, 1.01, 1.0).iter().all(|l| l.solved));
    }
}
