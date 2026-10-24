//! The onboard star DB: a little-endian header and four sections, each
//! 8-byte aligned and read in place.
//!
//! | section | contents |
//! |---|---|
//! | stars | [`Star`], sorted by cell |
//! | cells | `u32[n_cells + 1]` offsets into stars |
//! | pairs | [`Pair`], every pair closer than `fov_max_rad`, sorted by θ |
//! | k | `u32[n_pairs + 1]`, the k-vector over θ |
//! | reserved | empty in version 1 |

use core::ops::Range;

use zerocopy::little_endian::{F32, F64, I16, U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::cells::{self, cell_of};
use super::kvector::{self, Line, Sorted};
use crate::geom::{V3, dot};

pub const MAGIC: [u8; 8] = *b"METORSTR";
pub const VERSION: u32 = 1;
/// Section alignment (B).
pub const ALIGN: usize = 8;
/// The most stars a DB may hold: pairs index them with `u16`.
pub const MAX_STARS: u32 = 65_535;

pub const STARS: usize = 0;
pub const CELLS: usize = 1;
pub const PAIRS: usize = 2;
pub const KVECTOR: usize = 3;
pub const RESERVED: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Section {
    pub offset: U32,
    pub len: U32,
}

#[derive(Debug, Clone, Copy, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Header {
    pub magic: [u8; 8],
    pub version: U32,
    pub flags: U32,
    /// Epoch the positions are propagated to (Julian years).
    pub epoch_jy: F64,
    pub mag_limit: F32,
    /// The largest pair separation stored (rad).
    pub fov_max_rad: F32,
    pub n_stars: U32,
    pub n_pairs: U32,
    pub n_cells: U32,
    /// Cells per cube-face edge.
    pub face_n: U32,
    /// The k-vector line `z(i) = kq + km·i`.
    pub km: F64,
    pub kq: F64,
    pub sections: [Section; 5],
    /// CRC-32 of the header with this field zeroed, then every section.
    pub crc32: U32,
    pub _pad: [u8; 4],
}

pub const HEADER_LEN: usize = size_of::<Header>();

/// A catalog star: its unit vector in ICRF, magnitude ×100, and Hipparcos
/// number.
#[derive(Debug, Clone, Copy, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Star {
    pub r: [F32; 3],
    pub mag_centi: I16,
    pub _pad: U16,
    pub hip: U32,
}

impl Star {
    pub fn vector(&self) -> V3 {
        self.r.map(|x| f64::from(x.get()))
    }

    pub fn mag(&self) -> f64 {
        f64::from(self.mag_centi.get()) / 100.0
    }
}

/// Two stars, by row, and the angle between them (rad).
#[derive(Debug, Clone, Copy, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Pair {
    pub theta: F32,
    pub a: U16,
    pub b: U16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbError {
    /// Shorter than its header or than a section claims.
    Truncated,
    BadMagic,
    Version(u32),
    /// A section not on an [`ALIGN`]-byte boundary.
    Misaligned,
    /// A section's length disagrees with the header's counts.
    CountMismatch,
    Crc {
        stored: u32,
        computed: u32,
    },
    TooManyStars,
    /// Cell offsets that decrease, or do not end at `n_stars`.
    Cells,
    /// A pair naming a missing star, a non-finite θ, or pairs out of θ order.
    Pairs,
    /// A k-vector that decreases or counts past `n_pairs`, or a line that
    /// is not finite and increasing.
    KVector,
}

/// A parsed DB borrowing its bytes. Every accessor is total.
#[derive(Debug, Clone, Copy)]
pub struct Db<'a> {
    pub header: &'a Header,
    stars: &'a [Star],
    cells: &'a [U32],
    pairs: &'a [Pair],
    k: &'a [U32],
    line: Line,
}

/// The CRC the header should carry for `bytes`.
pub fn crc_of(bytes: &[u8]) -> Option<u32> {
    let (header, body) = Header::read_from_prefix(bytes).ok()?;
    let mut zeroed = header;
    zeroed.crc32 = U32::new(0);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(zeroed.as_bytes());
    hasher.update(body);
    Some(hasher.finalize())
}

fn section<'a, T: FromBytes + Immutable + KnownLayout>(
    bytes: &'a [u8],
    s: &Section,
    count: usize,
) -> Result<&'a [T], DbError> {
    let (offset, len) = (s.offset.get() as usize, s.len.get() as usize);
    if !offset.is_multiple_of(ALIGN) {
        return Err(DbError::Misaligned);
    }
    let raw = offset
        .checked_add(len)
        .and_then(|end| bytes.get(offset..end))
        .ok_or(DbError::Truncated)?;
    // Checked: on a 32-bit target a forged count could wrap to `len`.
    if count.checked_mul(size_of::<T>()) != Some(len) {
        return Err(DbError::CountMismatch);
    }
    <[T]>::ref_from_bytes(raw).map_err(|_| DbError::CountMismatch)
}

fn non_decreasing(values: &[U32], max: u32) -> bool {
    values.windows(2).all(|w| w[0].get() <= w[1].get()) && values.iter().all(|v| v.get() <= max)
}

impl<'a> Db<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Db<'a>, DbError> {
        let (header, _) = Header::ref_from_prefix(bytes).map_err(|_| DbError::Truncated)?;
        if header.magic != MAGIC {
            return Err(DbError::BadMagic);
        }
        if header.version.get() != VERSION {
            return Err(DbError::Version(header.version.get()));
        }
        let n_stars = header.n_stars.get();
        if n_stars > MAX_STARS {
            return Err(DbError::TooManyStars);
        }
        let (n_stars, n_pairs) = (n_stars as usize, header.n_pairs.get() as usize);
        let n_cells = header.n_cells.get() as usize;
        let s = &header.sections;
        let plus_one = |n: usize| n.checked_add(1).ok_or(DbError::CountMismatch);
        let stars = section::<Star>(bytes, &s[STARS], n_stars)?;
        let cells = section::<U32>(bytes, &s[CELLS], plus_one(n_cells)?)?;
        let pairs = section::<Pair>(bytes, &s[PAIRS], n_pairs)?;
        let k = section::<U32>(bytes, &s[KVECTOR], plus_one(n_pairs)?)?;
        section::<u8>(bytes, &s[RESERVED], 0)?;
        let stored = header.crc32.get();
        let computed = crc_of(bytes).ok_or(DbError::Truncated)?;
        if stored != computed {
            return Err(DbError::Crc { stored, computed });
        }
        let face_n = header.face_n.get();
        let cells_ok = n_cells as u64 == 6 * u64::from(face_n) * u64::from(face_n)
            && cells.first().map(|v| v.get()) == Some(0)
            && cells.last().map(|v| v.get()) == Some(n_stars as u32)
            && non_decreasing(cells, n_stars as u32);
        if !cells_ok {
            return Err(DbError::Cells);
        }
        let pair_ok = |p: &Pair| {
            (p.a.get() as usize) < n_stars
                && (p.b.get() as usize) < n_stars
                && p.theta.get().is_finite()
        };
        let sorted = pairs
            .windows(2)
            .all(|w| w[0].theta.get() <= w[1].theta.get());
        if !pairs.iter().all(pair_ok) || !sorted {
            return Err(DbError::Pairs);
        }
        let line = Line {
            m: header.km.get(),
            q: header.kq.get(),
        };
        let line_ok = line.m.is_finite() && line.m > 0.0 && line.q.is_finite();
        if !line_ok || !non_decreasing(k, n_pairs as u32) {
            return Err(DbError::KVector);
        }
        Ok(Db {
            header,
            stars,
            cells,
            pairs,
            k,
            line,
        })
    }

    pub fn n_stars(&self) -> usize {
        self.stars.len()
    }

    pub fn star(&self, row: u16) -> Option<&'a Star> {
        self.stars.get(usize::from(row))
    }

    /// The Hipparcos number of `row`.
    pub fn hip(&self, row: u16) -> Option<u32> {
        self.star(row).map(|s| s.hip.get())
    }

    pub fn pairs(&self) -> &'a [Pair] {
        self.pairs
    }

    /// The pairs with θ in `[lo, hi]`, as a range of [`pairs`](Self::pairs).
    pub fn pair_range(&self, lo: f32, hi: f32) -> Range<usize> {
        kvector::range(self, &self.line, lo, hi)
    }

    fn face_n(&self) -> u32 {
        self.header.face_n.get()
    }

    /// The stars in cell `c`.
    pub fn cell(&self, c: u32) -> Range<usize> {
        let at = |i: usize| self.cells.get(i).map_or(0, |v| v.get() as usize);
        let n = self.stars.len();
        let start = at(c as usize).min(n);
        start..at(c as usize + 1).clamp(start, n)
    }

    /// The cell a direction falls in.
    pub fn cell_of(&self, r: V3) -> u32 {
        cell_of(r, self.face_n())
    }

    /// Appends the rows of the stars within `radius` (rad) of unit `axis`.
    pub fn cone(&self, axis: V3, radius: f64, out: &mut impl Extend<u16>) {
        let n = self.face_n();
        let cos_r = libm::cos(radius);
        for c in 0..self.header.n_cells.get() {
            if !cells::may_touch(c, n, axis, radius) {
                continue;
            }
            let rows = self.cell(c);
            let hits = self.stars[rows.clone()]
                .iter()
                .zip(rows)
                .filter(|(s, _)| within(s, axis, cos_r))
                .map(|(_, row)| row as u16);
            out.extend(hits);
        }
    }
}

/// Whether `star` lies within the cone `dot ≥ cos_r` about `axis`.
pub fn within(star: &Star, axis: V3, cos_r: f64) -> bool {
    dot(star.vector(), axis) >= cos_r
}

impl Sorted for Db<'_> {
    fn len(&self) -> usize {
        self.pairs.len()
    }

    fn value(&self, i: usize) -> f32 {
        self.pairs.get(i).map_or(f32::INFINITY, |p| p.theta.get())
    }

    fn k(&self, i: usize) -> u32 {
        self.k.get(i).map_or(self.pairs.len() as u32, |v| v.get())
    }
}
