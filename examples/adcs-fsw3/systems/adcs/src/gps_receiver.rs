//! A simulated NovAtel-style GPS receiver on a serial port.
//!
//! It holds the device end of the line. It writes `#BESTXYZA` logs from the
//! plant's true orbit state, with Gauss-Markov position noise, and reads the
//! driver's commands. `LOG BESTXYZA ONTIME 0.1` sets the log period from 1 s
//! to 0.1 s. During an outage the logs carry no solution.
//!
//! It is a sequence that never ends: a loop reading commands joined with a
//! loop writing logs. The command loop passes each log period it reads to
//! the log loop through a ring.
//!
//! Time counts from the first `BodyState`, as `nav` counts from its first
//! sensor sample, so both see the same epoch.

use adcs_contracts::novatel::{BestXyz, LINE_MAX, LineBuf};
use adcs_contracts::{
    BodyState, GPS_POS_SIGMA, GPS_VEL_SIGMA, GpsError, GpsFault, V3, eci_to_ecef_state, epoch_at,
    gps_epoch, gps_time,
};
use futures_lite::future;
use metor_fsw_3::io::{Buf, Cap, Handle, IoInit, Uart};
use metor_fsw_3::ring::{Config, NoWake, RingBuffer, View, Writer};
use metor_fsw_3::{Input, MakeError, Seq, SeqError, Timestamp, ring_capacity, sequence};
use rand::{SeedableRng, rngs::StdRng};
use rand_distr::{Distribution, Normal};
use schemars::JsonSchema;
use serde::Deserialize;

/// The command bytes each read takes.
const COMMAND_READ: usize = 64;

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
    GPS_POS_SIGMA
}

fn default_vel_sigma() -> f64 {
    GPS_VEL_SIGMA
}

/// The receiver's buffers, which its body takes when it starts, its model,
/// and the ring that carries log periods from its command loop to its log
/// loop.
pub struct GpsReceiver {
    rx: Option<Buf>,
    tx: Option<Buf>,
    model: ReceiverModel,
    periods: RingBuffer,
}

impl GpsReceiver {
    pub fn new(p: GpsReceiverParams, io: &mut IoInit) -> Result<Self, MakeError> {
        let periods = Config {
            // PANIC Safety: two 8-byte records always form a ring.
            capacity: ring_capacity(size_of::<f64>(), 2).expect("a small ring"),
            max_readers: 1,
        };
        Ok(Self {
            rx: Some(io.alloc(COMMAND_READ)?),
            tx: Some(io.alloc(LINE_MAX)?),
            model: ReceiverModel::new(p),
            // PANIC Safety: the config above is valid.
            periods: RingBuffer::create_in_memory(periods).expect("a valid ring"),
        })
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
    let sent = m
        .periods
        .writer(NoWake)
        .map_err(|_| SeqError::Failed("the period ring's writer is taken".into()))?;
    let read = m
        .periods
        .view(NoWake)
        .map_err(|_| SeqError::Failed("the period ring has no free reader".into()))?;
    let port = connect(seq, uart).await;
    let logs = logs(seq, &port, tx, &mut m.model, truth, faults, read);
    future::zip(commands(&port, rx, sent), logs).await;
    Ok(())
}

/// Connects the device end, trying again each cycle.
async fn connect(seq: &Seq<'_>, uart: &Cap<Uart>) -> Handle<Uart> {
    loop {
        if let Ok(port) = uart.connect().await {
            return port;
        }
        seq.next_cycle().await;
    }
}

/// Reads the driver's commands, passing on each log period one sets.
async fn commands(uart: &Handle<Uart>, mut buf: Buf, mut periods: Writer<NoWake>) {
    let mut lines = LineBuf::<COMMAND_READ>::default();
    loop {
        let (read, back) = uart.read(buf).await;
        buf = back;
        if read.is_err() {
            continue;
        }
        lines.push(&buf);
        if let Some(p) = last_period(&mut lines) {
            // The log loop drains the ring each cycle, so it is full only
            // when three periods arrive in one cycle, and the last is dropped.
            let _ = periods.try_write(&p.to_le_bytes());
        }
    }
}

/// Writes each log as it falls due, one at a time.
async fn logs(
    seq: &Seq<'_>,
    uart: &Handle<Uart>,
    mut buf: Buf,
    model: &mut ReceiverModel,
    truth: &mut Input<BodyState>,
    faults: &mut Input<GpsFault>,
    mut periods: View<NoWake>,
) {
    let mut line = [0; LINE_MAX];
    loop {
        let newest = periods.drain().filter_map(Result::ok).last();
        if let Some(p) = newest.and_then(|bytes| bytes.try_into().ok()) {
            model.set_period(f64::from_le_bytes(p));
        }
        if let Ok(Some(fault)) = faults.latest() {
            model.outage = fault.outage != 0;
        }
        let due = match truth.latest() {
            Ok(Some(body)) => model.update(&body, &mut line),
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

/// Takes every complete line in `lines`. Returns the last log period one
/// sets.
fn last_period(lines: &mut LineBuf<COMMAND_READ>) -> Option<f64> {
    let mut period = None;
    while let Some(line) = lines.next_line() {
        period = parse_period(line).or(period);
    }
    period
}

/// The period a `LOG BESTXYZA ONTIME` line sets.
fn parse_period(line: &[u8]) -> Option<f64> {
    let line = core::str::from_utf8(line).ok()?;
    let (_, period) = line.split_once("LOG BESTXYZA ONTIME ")?;
    period.trim().parse().ok().filter(|p: &f64| *p > 0.0)
}

/// The simulated receiver: when its logs fall due, and the solution and
/// noise they carry.
struct ReceiverModel {
    period: f64,
    next_log: f64,
    first: Option<Timestamp>,
    outage: bool,
    pos_err: GpsError,
    pos_sigma: f64,
    vel_sigma: f64,
    rng: StdRng,
    unit: Normal<f64>,
}

impl ReceiverModel {
    fn new(p: GpsReceiverParams) -> Self {
        Self {
            period: 1.0,
            next_log: 0.0,
            first: None,
            outage: false,
            pos_err: GpsError::new(p.pos_sigma),
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
        self.next_solution(body, t).encode(line)
    }

    /// The solution for `body` at `t` seconds since the first state. This
    /// steps the position error.
    fn next_solution(&mut self, body: &BodyState, t: f64) -> BestXyz {
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
        let unit = self.noise(1.0);
        let pos = body.pos_eci + self.pos_err.step(self.period, unit);
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

    fn model() -> ReceiverModel {
        ReceiverModel::new(GpsReceiverParams {
            seed: 7,
            pos_sigma: 5.0,
            vel_sigma: 0.0,
        })
    }

    /// The logs `l` writes, one cycle at a time, from `from` for `seconds`.
    fn run(l: &mut ReceiverModel, from: f64, seconds: f64) -> Vec<BestXyz> {
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
        let mut l = model();
        assert_eq!(run(&mut l, 0.0, 2.0).len(), 3, "at 0, 1, and 2 s");
        l.set_period(push(&mut LineBuf::default(), LOG_10HZ).unwrap());
        assert_eq!(l.period, 0.1);
        assert!((9..=10).contains(&run(&mut l, 2.01, 0.99).len()));
    }

    /// Pushes `bytes` as one read, and returns the last period they set.
    fn push(lines: &mut LineBuf<COMMAND_READ>, bytes: &[u8]) -> Option<f64> {
        lines.push(bytes);
        last_period(lines)
    }

    #[test]
    fn a_command_split_across_reads_is_one_line() {
        let mut c = LineBuf::default();
        assert_eq!(push(&mut c, &LOG_10HZ[..9]), None);
        assert_eq!(push(&mut c, &LOG_10HZ[9..]), Some(0.1));
    }

    #[test]
    fn a_bad_or_overlong_command_sets_nothing() {
        let mut c = LineBuf::default();
        assert_eq!(push(&mut c, b"LOG BESTXYZA ONTIME -1\r\n"), None);
        assert_eq!(push(&mut c, b"LOG BESTXYZA ONTIME fast\r\n"), None);
        assert_eq!(push(&mut c, &[b'x'; COMMAND_READ + 4]), None);
        assert_eq!(push(&mut c, b"\r\n"), None);
        assert_eq!(push(&mut c, LOG_10HZ), Some(0.1));
    }

    #[test]
    fn an_outage_logs_no_solution() {
        let mut l = model();
        l.outage = true;
        let logs = run(&mut l, 0.0, 1.0);
        assert!(!logs.is_empty() && logs.iter().all(|l| !l.solved && l.sats == 0));
        l.outage = false;
        assert!(run(&mut l, 1.01, 1.0).iter().all(|l| l.solved));
    }
}
