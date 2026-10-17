//! NovAtel-style ASCII `#BESTXYZA` logs: the GPS receiver's position and
//! velocity in ECEF, as one line with a CRC-32.
//!
//! The receiver model encodes them and the flight driver decodes them, so the
//! codec is the interface both sides share. Neither side allocates.

use std::io::{Cursor, Write as _};

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
pub enum DecodeError {
    /// The line's CRC does not match its bytes.
    Crc,
    /// A field is missing or does not parse.
    Field,
}

impl BestXyz {
    /// Writes this log as one line into `out`. Returns its length, or `None`
    /// if `out` is too short.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Cursor::new(out);
        let (status, kind) = match self.solved {
            true => ("SOL_COMPUTED", "SINGLE"),
            false => ("INSUFFICIENT_OBS", "NONE"),
        };
        let [px, py, pz] = self.pos_ecef;
        let [vx, vy, vz] = self.vel_ecef;
        let (s, n) = (self.pos_sigma, self.sats);
        write!(
            w,
            "#BESTXYZA,COM1,0,0.0,FINESTEERING,{},{:.3},02000000,0000,0;\
             {status},{kind},{px:.4},{py:.4},{pz:.4},{s:.4},{s:.4},{s:.4},\
             {status},{kind},{vx:.4},{vy:.4},{vz:.4},0.0500,0.0500,0.0500,\
             \"\",0.000,0.000,0.000,{n},{n},0,0,0,00,0,00",
            self.week, self.seconds,
        )
        .ok()?;
        let len = w.position() as usize;
        let crc = crc32(&w.get_ref()[1..len]);
        write!(w, "*{crc:08x}\r\n").ok()?;
        Some(w.position() as usize)
    }

    /// Decodes one line, from `#` up to the CRC. `None` for a log other than
    /// `BESTXYZA`.
    pub fn decode(line: &[u8]) -> Option<Result<BestXyz, DecodeError>> {
        let line = core::str::from_utf8(line).ok()?;
        let (body, crc) = line.strip_prefix('#')?.rsplit_once('*')?;
        let (header, fields) = body.split_once(';')?;
        let mut header = header.split(',');
        if header.next() != Some("BESTXYZA") {
            return None;
        }
        if u32::from_str_radix(crc, 16) != Ok(crc32(body.as_bytes())) {
            return Some(Err(DecodeError::Crc));
        }
        Some(Self::parse(header, fields).ok_or(DecodeError::Field))
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
}

/// Bytes from a serial port, split into lines that end in `\n`.
pub struct LineBuf<const N: usize> {
    buf: [u8; N],
    /// Where the bytes not yet returned as a line start.
    start: usize,
    len: usize,
}

impl<const N: usize> Default for LineBuf<N> {
    fn default() -> Self {
        Self {
            buf: [0; N],
            start: 0,
            len: 0,
        }
    }
}

impl<const N: usize> LineBuf<N> {
    /// Appends received bytes. Drops what is buffered if they do not fit.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.copy_within(self.start..self.len, 0);
        self.len -= self.start;
        self.start = 0;
        if self.len + bytes.len() > N {
            self.len = 0;
        }
        let n = bytes.len().min(N);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    /// Returns the next complete line, through its `\n`.
    pub fn next_line(&mut self) -> Option<&[u8]> {
        let rest = &self.buf[self.start..self.len];
        let end = self.start + rest.iter().position(|&b| b == b'\n')? + 1;
        let line = &self.buf[self.start..end];
        self.start = end;
        Some(line)
    }
}

/// The `BESTXYZA` logs in bytes from a serial port.
#[derive(Default)]
pub struct Lines(LineBuf<LINE_MAX>);

impl Lines {
    /// Appends received bytes. Drops what is buffered if they do not fit.
    pub fn push(&mut self, bytes: &[u8]) {
        self.0.push(bytes);
    }
}

/// Each complete `BESTXYZA` log buffered, as more bytes arrive.
impl Iterator for Lines {
    type Item = Result<BestXyz, DecodeError>;

    /// The next complete `BESTXYZA` log. Skips bytes before a `#` and lines
    /// of other logs.
    fn next(&mut self) -> Option<Result<BestXyz, DecodeError>> {
        loop {
            let line = self.0.next_line()?;
            let Some(start) = line.iter().position(|&b| b == b'#') else {
                continue;
            };
            let log = line[start..]
                .strip_suffix(b"\r\n")
                .and_then(BestXyz::decode);
            if let Some(decoded) = log {
                return Some(decoded);
            }
        }
    }
}

fn triple<'a>(f: &mut impl Iterator<Item = &'a str>) -> Option<[f64; 3]> {
    Some([
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
    ])
}

#[cfg(feature = "environment")]
impl crate::Gps {
    /// The fix a solved `log` reports at `now`, rotated to ECI at the log's
    /// GPS time.
    pub fn from_best_xyz(now: metor_fsw_3::Timestamp, log: &BestXyz) -> Self {
        use crate::{V3, ecef_to_eci_state, ecef_to_geodetic, gps_epoch};
        let (pos_ecef, vel_ecef) = (V3::from_buf(log.pos_ecef), V3::from_buf(log.vel_ecef));
        let epoch = gps_epoch(log.week, log.seconds);
        let (pos_eci, vel_eci) = ecef_to_eci_state(epoch, &pos_ecef, &vel_ecef);
        let (lat, lon, alt) = ecef_to_geodetic(&pos_ecef);
        Self {
            timestamp: now,
            pos_eci,
            vel_eci,
            lla: V3::from_buf([lat.to_degrees(), lon.to_degrees(), alt]),
        }
    }
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
        let n = log.encode(&mut out).unwrap();
        out[..n].to_vec()
    }

    fn decoded(bytes: &[u8]) -> Vec<Result<BestXyz, DecodeError>> {
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
        assert_eq!(decoded(&bytes), [Err(DecodeError::Crc)]);
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
        assert_eq!(log(true).encode(&mut [0; 64]), None);
    }
}
