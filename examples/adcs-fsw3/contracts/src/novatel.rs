//! NovAtel-style ASCII `#BESTXYZA` logs: the GPS receiver's position and
//! velocity in ECEF, as one line with a CRC-32.
//!
//! The receiver model encodes them and the flight driver decodes them, so the
//! codec is the interface both sides share. Neither side allocates.

use core::fmt::Write as _;

/// The longest line either side handles, with room to spare.
pub const LINE_MAX: usize = 512;

/// The command that sets the log period to 0.1 s.
pub const LOG_10HZ: &[u8] = b"LOG BESTXYZA ONTIME 0.1\r\n";

/// One `BESTXYZ` solution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BestXyz {
    /// GPS week.
    pub week: u32,
    /// Seconds into the GPS week, to the millisecond.
    pub seconds: f64,
    /// Whether the position and velocity are a computed solution.
    pub solved: bool,
    /// Position (m).
    pub pos_ecef: [f64; 3],
    /// Velocity (m/s).
    pub vel_ecef: [f64; 3],
    /// Position standard deviation per axis (m).
    pub pos_sigma: f64,
    /// Satellites in the solution.
    pub sats: u8,
}

/// Why a line did not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// The line's CRC does not match its bytes.
    Crc,
    /// A field is missing or does not parse.
    Field,
}

/// Writes `log` as one line into `out`. Returns its length, or `None` if
/// `out` is too short.
pub fn encode(log: &BestXyz, out: &mut [u8]) -> Option<usize> {
    let mut w = Cursor { out, len: 0 };
    let (status, kind) = match log.solved {
        true => ("SOL_COMPUTED", "SINGLE"),
        false => ("INSUFFICIENT_OBS", "NONE"),
    };
    let [px, py, pz] = log.pos_ecef;
    let [vx, vy, vz] = log.vel_ecef;
    let (s, n) = (log.pos_sigma, log.sats);
    write!(
        w,
        "#BESTXYZA,COM1,0,0.0,FINESTEERING,{},{:.3},02000000,0000,0;\
         {status},{kind},{px:.4},{py:.4},{pz:.4},{s:.4},{s:.4},{s:.4},\
         {status},{kind},{vx:.4},{vy:.4},{vz:.4},0.0500,0.0500,0.0500,\
         \"\",0.000,0.000,0.000,{n},{n},0,0,0,00,0,00",
        log.week, log.seconds,
    )
    .ok()?;
    let crc = crc32(&w.out[1..w.len]);
    write!(w, "*{crc:08x}\r\n").ok()?;
    Some(w.len)
}

/// Bytes from a serial port, split into logs.
pub struct Lines {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Default for Lines {
    fn default() -> Self {
        Self {
            buf: [0; LINE_MAX],
            len: 0,
        }
    }
}

impl Lines {
    /// Appends received bytes. Drops what is buffered if they do not fit.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > LINE_MAX {
            self.len = 0;
        }
        let n = bytes.len().min(LINE_MAX);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    fn consume(&mut self, n: usize) {
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }
}

/// Each complete `BESTXYZA` log buffered, as more bytes arrive.
impl Iterator for Lines {
    type Item = Result<BestXyz, Bad>;

    /// The next complete `BESTXYZA` log. Skips bytes before a `#` and lines
    /// of other logs.
    fn next(&mut self) -> Option<Result<BestXyz, Bad>> {
        loop {
            let start = self.buf[..self.len].iter().position(|&b| b == b'#');
            let Some(start) = start else {
                self.len = 0;
                return None;
            };
            self.consume(start);
            let end = find(&self.buf[..self.len], b"\r\n")?;
            let decoded = decode(&self.buf[..end]);
            self.consume(end + 2);
            if let Some(decoded) = decoded {
                return Some(decoded);
            }
        }
    }
}

/// Decodes one line, from `#` up to the CRC. `None` for a log other than
/// `BESTXYZA`.
fn decode(line: &[u8]) -> Option<Result<BestXyz, Bad>> {
    let line = core::str::from_utf8(line).ok()?;
    let (body, crc) = line.strip_prefix('#')?.rsplit_once('*')?;
    let (header, fields) = body.split_once(';')?;
    let mut header = header.split(',');
    if header.next() != Some("BESTXYZA") {
        return None;
    }
    if u32::from_str_radix(crc, 16) != Ok(crc32(body.as_bytes())) {
        return Some(Err(Bad::Crc));
    }
    Some(parse(header, fields).ok_or(Bad::Field))
}

fn parse<'a>(mut header: impl Iterator<Item = &'a str>, fields: &str) -> Option<BestXyz> {
    let week = header.nth(4)?.parse().ok()?;
    let seconds = header.next()?.parse().ok()?;
    let mut f = fields.split(',');
    let pos_solved = f.next()? == "SOL_COMPUTED";
    let _kind = f.next()?;
    let pos_ecef = triple(&mut f)?;
    let pos_sigma = f.next()?.parse().ok()?;
    let _sigmas = (f.next()?, f.next()?);
    let vel_solved = f.next()? == "SOL_COMPUTED";
    let _kind = f.next()?;
    let vel_ecef = triple(&mut f)?;
    let sats = f.nth(7)?.parse().ok()?;
    Some(BestXyz {
        week,
        seconds,
        solved: pos_solved && vel_solved,
        pos_ecef,
        vel_ecef,
        pos_sigma,
        sats,
    })
}

fn triple<'a>(f: &mut impl Iterator<Item = &'a str>) -> Option<[f64; 3]> {
    Some([
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
    ])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// NovAtel's CRC-32: polynomial `0xEDB88320`, reflected, with a zero
/// initial value and no final inversion.
pub fn crc32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| match c & 1 {
            1 => (c >> 1) ^ 0xEDB8_8320,
            _ => c >> 1,
        })
    })
}

/// Formats into a byte slice.
struct Cursor<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        let dst = self.out.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(solved: bool) -> BestXyz {
        BestXyz {
            week: 2295,
            seconds: 86_412.375,
            solved,
            pos_ecef: [-1634531.5683, -3664618.0326, 4942496.327],
            vel_ecef: [7012.1234, -1203.5, 0.25],
            pos_sigma: 5.0,
            sats: 9,
        }
    }

    fn line(log: &BestXyz) -> Vec<u8> {
        let mut out = [0; LINE_MAX];
        let n = encode(log, &mut out).unwrap();
        out[..n].to_vec()
    }

    fn decoded(bytes: &[u8]) -> Vec<Result<BestXyz, Bad>> {
        let mut lines = Lines::default();
        lines.push(bytes);
        lines.collect()
    }

    #[test]
    fn a_log_round_trips() {
        assert_eq!(decoded(&line(&log(true))), [Ok(log(true))]);
        assert_eq!(decoded(&line(&log(false))), [Ok(log(false))]);
    }

    #[test]
    fn a_bad_crc_is_refused() {
        let mut bytes = line(&log(true));
        bytes[30] ^= 1;
        assert_eq!(decoded(&bytes), [Err(Bad::Crc)]);
    }

    #[test]
    fn garbage_other_logs_and_split_reads_are_handled() {
        let mut bytes = b"\x00\xffjunk#BESTPOSA,COM1;x*00000000\r\n".to_vec();
        bytes.extend(line(&log(true)));
        let (a, b) = bytes.split_at(60);
        let mut lines = Lines::default();
        lines.push(a);
        assert_eq!(lines.next(), None, "half a line");
        lines.push(b);
        assert_eq!(lines.next(), Some(Ok(log(true))));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn a_short_buffer_does_not_encode() {
        assert_eq!(encode(&log(true), &mut [0; 64]), None);
    }
}
