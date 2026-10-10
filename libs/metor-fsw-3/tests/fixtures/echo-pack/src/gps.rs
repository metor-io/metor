//! A GPS receiver driver on a serial port: it sets the receiver to 10 Hz,
//! then publishes each fix the receiver reports.

use core::time::Duration;

use metor_fsw_3::io::test::ScriptedUart;
use metor_fsw_3::io::{Cap, DeviceSide, Read, Uart, Write};
use metor_fsw_3::{
    Input, IoInit, Log, Output, Record, Seq, SeqError, Slot, Timestamp, sequence, system,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Sets an MTK receiver's fix rate to 10 Hz.
pub const PMTK_RATE_10HZ: &[u8] = b"$PMTK220,100*2F\r\n";

/// One position fix.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(name = "gps_fix", max_len = 40)]
pub struct Gps {
    /// Degrees north.
    pub lat: f64,
    /// Degrees east.
    pub lon: f64,
    /// Meters above mean sea level.
    pub alt: f64,
    pub sats: f64,
}

/// A fix parsed from one GGA sentence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    pub alt: f64,
    pub sats: u32,
}

impl From<Fix> for Gps {
    fn from(f: Fix) -> Self {
        Self {
            lat: f.lat,
            lon: f.lon,
            alt: f.alt,
            sats: f64::from(f.sats),
        }
    }
}

/// The longest NMEA sentence, with room for a partial one behind it.
const LINE: usize = 256;

/// Splits a byte stream into NMEA sentences and parses the GGA ones.
pub struct Nmea {
    bytes: [u8; LINE],
    len: usize,
}

impl Default for Nmea {
    fn default() -> Self {
        Self {
            bytes: [0; LINE],
            len: 0,
        }
    }
}

impl Nmea {
    /// Appends received bytes. Drops what is buffered if it overflows.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > LINE {
            self.len = 0;
        }
        let n = bytes.len().min(LINE - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    /// The next valid GGA fix among the complete sentences buffered.
    pub fn next(&mut self) -> Option<Fix> {
        while let Some(end) = self.bytes[..self.len].iter().position(|b| *b == b'\n') {
            let fix = core::str::from_utf8(&self.bytes[..end])
                .ok()
                .and_then(parse_gga);
            self.bytes.copy_within(end + 1..self.len, 0);
            self.len -= end + 1;
            if fix.is_some() {
                return fix;
            }
        }
        None
    }
}

/// Parses `$..GGA,time,lat,N,lon,E,quality,sats,hdop,alt,M,...*hh`.
fn parse_gga(line: &str) -> Option<Fix> {
    let mut f = checked(line.trim_end())?.split(',');
    if !f.next()?.ends_with("GGA") {
        return None;
    }
    let _time = f.next()?;
    let lat = degrees(f.next()?, 2)? * sign(f.next()?, "S");
    let lon = degrees(f.next()?, 3)? * sign(f.next()?, "W");
    if f.next()? == "0" {
        return None;
    }
    let sats = f.next()?.parse().ok()?;
    let _hdop = f.next()?;
    let alt = f.next()?.parse().ok()?;
    Some(Fix {
        lat,
        lon,
        alt,
        sats,
    })
}

/// The sentence between `$` and `*`, if its checksum matches.
fn checked(line: &str) -> Option<&str> {
    let (body, sum) = line.strip_prefix('$')?.split_once('*')?;
    let want = u8::from_str_radix(sum, 16).ok()?;
    let got = body.bytes().fold(0, |acc, b| acc ^ b);
    (got == want).then_some(body)
}

/// `ddmm.mmmm` (or `dddmm.mmmm`) as degrees.
fn degrees(field: &str, whole: usize) -> Option<f64> {
    let deg: f64 = field.get(..whole)?.parse().ok()?;
    let min: f64 = field.get(whole..)?.parse().ok()?;
    Some(deg + min / 60.0)
}

fn sign(hemisphere: &str, negative: &str) -> f64 {
    if hemisphere == negative { -1.0 } else { 1.0 }
}

pub struct GpsState {
    rx: Slot<Read>,
    tx: Slot<Write>,
    configured: bool,
    nmea: Nmea,
}

impl GpsState {
    pub fn new(io: &IoInit) -> Self {
        Self {
            rx: Slot::Idle(io.alloc(256)),
            tx: Slot::Idle(io.alloc_from(PMTK_RATE_10HZ)),
            configured: false,
            nmea: Nmea::default(),
        }
    }
}

/// Sets the receiver to 10 Hz, then publishes each fix it reports.
#[system]
pub fn gps(#[state] s: &mut GpsState, uart: &Cap<Uart>, fix: &mut Output<Gps>, log: &mut Log) {
    match s.tx.poll() {
        Some(Ok(n)) => s.configured = n == PMTK_RATE_10HZ.len(),
        Some(Err(e)) => log.fault("gps_config", e),
        None => {}
    }
    match s.rx.poll() {
        Some(Ok(_)) => s.nmea.push(s.rx.bytes()),
        Some(Err(e)) => log.fault("gps_read", e),
        None => {}
    }
    while let Some(f) = s.nmea.next() {
        let _ = fix.write(&Gps::from(f));
    }
    if !s.configured
        && let Some(cmd) = s.tx.take_idle()
    {
        s.tx = Slot::Busy(uart.write(cmd));
    }
    if let Some(buf) = s.rx.take_idle() {
        s.rx = Slot::Busy(uart.read(buf));
    }
}

/// The params for `uart_script`.
#[derive(Deserialize, JsonSchema)]
pub struct UartScriptParams {
    /// What the device sends, one entry per read.
    pub script: Vec<String>,
}

pub struct UartScript(ScriptedUart);

impl UartScript {
    pub fn new(params: UartScriptParams) -> Self {
        let chunks = params.script.into_iter().map(String::into_bytes);
        Self(ScriptedUart::scripted(chunks))
    }
}

/// A serial device that answers reads from a script.
#[system]
pub fn uart_script(#[state] s: &mut UartScript, now: Timestamp, uart: &mut DeviceSide<Uart>) {
    uart.serve(now, &mut s.0);
}

/// The params for `until_fix`.
#[derive(Deserialize, JsonSchema)]
pub struct UntilFixParams {
    /// How long to wait for a fix, in seconds of cycle time.
    pub timeout_s: f64,
}

pub struct UntilFix {
    timeout: Duration,
}

impl UntilFix {
    pub fn new(params: UntilFixParams) -> Self {
        Self {
            timeout: Duration::from_secs_f64(params.timeout_s.max(0.0)),
        }
    }
}

/// Completes on the first fix, or fails after `timeout_s` of cycle time.
#[sequence]
pub async fn until_fix(
    #[state] s: &mut UntilFix,
    seq: &Seq,
    fix: &mut Input<Gps>,
) -> Result<(), SeqError> {
    let fixed = || fix.latest().is_ok_and(|f| f.is_some());
    seq.wait_for(s.timeout, fixed).await
}

#[cfg(test)]
mod tests {
    use metor_fsw_3::io::test::{ScriptedUart, TestIo, cap};
    use metor_fsw_3::io::{Errno, Uart};
    use metor_fsw_3::log::{LogPort, with_log_port};
    use metor_fsw_3::metor_proto_wkt::LogEvent;
    use metor_fsw_3::ring::{Config, NoWake, RingBuffer};
    use metor_fsw_3::{Input, Log, Output, Record, Timestamp, ring_capacity};

    use super::*;

    const GGA: &[u8] = b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n";

    fn ring<T: Record>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(T::MAX_LEN, 32).unwrap(),
            max_readers: 1,
        })
    }

    /// The driver, its outputs, and the model behind its uart.
    struct Rig {
        state: GpsState,
        uart: Cap<Uart>,
        io: TestIo<ScriptedUart>,
        fix: Output<Gps>,
        fixes: Input<Gps>,
        log: LogPort,
        lines: Input<LogEvent>,
        _rings: [RingBuffer; 2],
    }

    impl Rig {
        fn new(model: ScriptedUart) -> Self {
            let (uart, io) = cap::<Uart, _>(model);
            let state = GpsState::new(&io.init());
            let (fix_ring, log_ring) = (ring::<Gps>(), ring::<LogEvent>());
            Self {
                state,
                uart,
                io,
                fix: Output::new(fix_ring.writer(NoWake).unwrap()),
                fixes: Input::new(vec![fix_ring.view(NoWake).unwrap()]),
                log: LogPort::new(Output::new(log_ring.writer(NoWake).unwrap())),
                lines: Input::new(vec![log_ring.view(NoWake).unwrap()]),
                _rings: [fix_ring, log_ring],
            }
        }

        /// Runs the driver once, then lets the model answer.
        fn cycle(&mut self) {
            let Self {
                state,
                uart,
                fix,
                log,
                ..
            } = self;
            with_log_port(log, Timestamp(0), || gps()(state, uart, fix, &mut Log));
            self.io.turn();
        }

        fn fixes(&mut self) -> Vec<Gps> {
            self.fixes.drain().map(|f| f.unwrap()).collect()
        }

        fn faults(&mut self) -> Vec<String> {
            let lines = self.lines.drain().map(|l| l.unwrap());
            lines
                .flat_map(|l| l.fields.into_iter().map(|(_, v)| v.to_string()))
                .collect()
        }
    }

    #[test]
    fn uart_script_answers_a_read_per_line() {
        use metor_fsw_3::io::Op;
        use metor_fsw_3::io::test::device;

        let (mut dev, mut driver) = device::<Uart>();
        let params = UartScriptParams {
            script: vec!["one".into(), "two".into()],
        };
        let mut state = UartScript::new(params);
        let reads: Vec<_> = (0..3)
            .map(|_| driver.request(Op::Read { offset: None }, b""))
            .collect();
        uart_script()(&mut state, Timestamp(0), &mut dev);
        assert_eq!(driver.replies().len(), 2);
        assert_eq!(driver.driver_bytes(reads[0]), b"one");
        assert_eq!(driver.driver_bytes(reads[1]), b"two");
    }

    #[test]
    fn configures_once_then_publishes_a_fix() {
        let mut rig = Rig::new(ScriptedUart::scripted([GGA.to_vec()]));
        for _ in 0..3 {
            rig.cycle();
        }
        assert_eq!(rig.io.model().written(), [PMTK_RATE_10HZ.to_vec()]);
        let fixes = rig.fixes();
        assert_eq!(fixes.len(), 1);
        assert!((fixes[0].lat - 48.1173).abs() < 1e-4);
        assert!((fixes[0].lon - 11.516_667).abs() < 1e-4);
        assert_eq!((fixes[0].alt, fixes[0].sats), (545.4, 8.0));
    }

    #[test]
    fn sentence_split_across_reads_gives_one_fix() {
        let (a, b) = GGA.split_at(30);
        let mut rig = Rig::new(ScriptedUart::scripted([a.to_vec(), b.to_vec()]));
        for _ in 0..4 {
            rig.cycle();
        }
        assert_eq!(rig.fixes().len(), 1);
    }

    #[test]
    fn bad_checksum_gives_no_fix() {
        let mut bad = GGA.to_vec();
        let star = bad.iter().position(|b| *b == b'*').unwrap();
        bad[star + 1] = b'0';
        let mut rig = Rig::new(ScriptedUart::scripted([bad]));
        for _ in 0..3 {
            rig.cycle();
        }
        assert!(rig.fixes().is_empty());
    }

    #[test]
    fn short_config_write_is_retried() {
        let mut rig = Rig::new(ScriptedUart::silent().short_write(4));
        for _ in 0..4 {
            rig.cycle();
        }
        let full = PMTK_RATE_10HZ.to_vec();
        assert_eq!(rig.io.model().written(), [full[..4].to_vec(), full]);
    }

    #[test]
    fn failed_config_is_logged_and_retried() {
        let mut rig = Rig::new(ScriptedUart::silent().fail_write(Errno::EIO));
        for _ in 0..4 {
            rig.cycle();
        }
        assert_eq!(rig.faults(), ["gps_config"]);
        assert_eq!(rig.io.model().written(), [PMTK_RATE_10HZ.to_vec()]);
    }
}
