//! A zero-copy reader for `starcat.bin`, the render catalog: a little-endian
//! 80 B [`Header`], then the nside-16 HEALPix [`Cell`]s, the [`Star`]s (each
//! cell's a run sorted by G) and the [`HipEntry`]s, all alignment 1.

use core::ops::Range;

use zerocopy::byteorder::little_endian::{F32, F64, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::geom::{V3, cross, dot, norm, normalize};

pub const MAGIC: [u8; 8] = *b"STARCAT\0";
pub const VERSION: u32 = 1;
pub const SOURCE_GAIA_DR3: u32 = 1 << 0;
pub const SOURCE_HIPPARCOS: u32 = 1 << 1;
/// How far a stored direction may be from unit length, `||d|² − 1|`.
const UNIT_TOL: f32 = 1e-5;

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C)]
pub struct Header {
    pub magic: [u8; 8],
    pub version: U32,
    pub nside: U32,
    pub n_cells: U32,
    pub n_stars: U32,
    /// The epoch (Julian year) the positions are propagated to.
    pub epoch_jyear: F64,
    /// The faintest G (mag) the catalog holds.
    pub mag_limit: F32,
    /// `SOURCE_*` bits.
    pub sources: U32,
    pub cells_off: U64,
    pub stars_off: U64,
    pub hip_off: U64,
    pub n_hip: U32,
    pub _pad: [U32; 3],
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C)]
pub struct Cell {
    pub centre: [F32; 3],
    /// The largest angle (rad) from `centre` to any point of the cell.
    pub radius: F32,
    /// The cell's first star row.
    pub offset: U32,
    pub count: U32,
    pub _pad: [U32; 2],
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C)]
pub struct Star {
    /// The unit direction in ICRS at the catalog epoch.
    pub dir: [F32; 3],
    /// G in the low half and BP − RP in the high half, each `i16` millimag.
    pub mags: U32,
}

/// The Hipparcos number of one star row. The table is sorted by row.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C)]
pub struct HipEntry {
    pub row: U32,
    pub hip: U32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatError {
    /// The file is shorter than the header.
    TooShort,
    BadMagic,
    BadVersion(u32),
    /// `nside` is 0, or `n_cells` is not `12·nside²`.
    BadCellCount,
    /// A section offset is not where the sections before it end.
    BadOffsets,
    /// The file length is not the sum of the sections.
    BadLength {
        len: u64,
        expected: u64,
    },
    /// A cell's centre is not a finite unit vector, or its radius is not in
    /// `[0, π]`.
    BadCell {
        cell: u32,
    },
    /// A cell's offset is not the sum of the counts before it.
    CellOffset {
        cell: u32,
    },
    /// The cell counts do not sum to `n_stars`.
    CountSum {
        sum: u64,
        n_stars: u32,
    },
    /// A star's direction is not a finite unit vector.
    BadStar {
        row: u32,
    },
    /// A cell's stars are not sorted by G.
    Unsorted {
        cell: u32,
    },
    /// HIP rows are not strictly increasing star rows.
    HipRows,
    /// A HIP entry holds `u32::MAX`.
    HipValue,
}

impl Header {
    pub const LEN: usize = size_of::<Header>();

    fn check(&self) -> Result<(), CatError> {
        if self.magic != MAGIC {
            return Err(CatError::BadMagic);
        }
        if self.version.get() != VERSION {
            return Err(CatError::BadVersion(self.version.get()));
        }
        let nside = u64::from(self.nside.get());
        let n_cells = nside.checked_mul(nside).and_then(|n| n.checked_mul(12));
        if nside == 0 || n_cells != Some(u64::from(self.n_cells.get())) {
            return Err(CatError::BadCellCount);
        }
        Ok(())
    }

    /// The expected `[cells_off, stars_off, hip_off, file length]`.
    fn layout(&self) -> [u64; 4] {
        let cells = Header::LEN as u64;
        let stars = cells + size_of::<Cell>() as u64 * u64::from(self.n_cells.get());
        let hip = stars + size_of::<Star>() as u64 * u64::from(self.n_stars.get());
        let end = hip + size_of::<HipEntry>() as u64 * u64::from(self.n_hip.get());
        [cells, stars, hip, end]
    }

    fn check_offsets(&self) -> Result<(), CatError> {
        let [cells, stars, hip, _] = self.layout();
        let found = [
            self.cells_off.get(),
            self.stars_off.get(),
            self.hip_off.get(),
        ];
        if found != [cells, stars, hip] {
            return Err(CatError::BadOffsets);
        }
        Ok(())
    }
}

impl Cell {
    pub fn centre(&self) -> V3 {
        self.centre.map(|c| f64::from(c.get()))
    }

    pub fn radius(&self) -> f64 {
        f64::from(self.radius.get())
    }

    /// The cell's star rows.
    pub fn rows(&self) -> Range<usize> {
        rows(self.offset.get(), self.count.get())
    }

    fn is_valid(&self) -> bool {
        let r = self.radius.get();
        is_unit(self.centre.map(|c| c.get())) && (0.0..=core::f32::consts::PI).contains(&r)
    }
}

/// Whether `d` is finite and within [`UNIT_TOL`] of unit length.
fn is_unit(d: [f32; 3]) -> bool {
    let n2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    d.iter().all(|c| c.is_finite()) && (n2 - 1.0).abs() < UNIT_TOL
}

fn rows(offset: u32, count: u32) -> Range<usize> {
    let start = offset as usize;
    start..start.saturating_add(count as usize)
}

impl Star {
    pub fn g_mmag(&self) -> i16 {
        self.mags.get() as u16 as i16
    }

    pub fn bp_rp_mmag(&self) -> i16 {
        (self.mags.get() >> 16) as u16 as i16
    }

    /// G (mag).
    pub fn g(&self) -> f32 {
        f32::from(self.g_mmag()) / 1000.0
    }

    /// BP − RP (mag).
    pub fn bp_rp(&self) -> f32 {
        f32::from(self.bp_rp_mmag()) / 1000.0
    }

    /// The stored direction, as the GPU reads it.
    pub fn dir(&self) -> [f32; 3] {
        self.dir.map(|c| c.get())
    }

    /// The stored direction in `f64`, renormalised.
    pub fn unit(&self) -> V3 {
        let d = self.dir().map(f64::from);
        let n = norm(d);
        d.map(|c| c / n)
    }

    fn is_valid(&self) -> bool {
        is_unit(self.dir())
    }
}

/// A parsed `starcat.bin`, borrowing the file's bytes.
#[derive(Debug, Clone, Copy)]
pub struct StarCat<'a> {
    header: &'a Header,
    cells: &'a [Cell],
    stars: &'a [Star],
    hip: &'a [HipEntry],
}

impl<'a> StarCat<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CatError> {
        let (header, rest) = Header::ref_from_prefix(bytes).map_err(|_| CatError::TooShort)?;
        header.check()?;
        header.check_offsets()?;
        let cat = Self::split(header, rest).ok_or(CatError::BadLength {
            len: bytes.len() as u64,
            expected: header.layout()[3],
        })?;
        cat.check()?;
        Ok(cat)
    }

    /// Splits the bytes after the header into the sections, or `None` unless
    /// they are exactly as long as the header's counts need.
    fn split(header: &'a Header, rest: &'a [u8]) -> Option<Self> {
        let n = |x: &U32| x.get() as usize;
        let (cells, rest) = <[Cell]>::ref_from_prefix_with_elems(rest, n(&header.n_cells)).ok()?;
        let (stars, rest) = <[Star]>::ref_from_prefix_with_elems(rest, n(&header.n_stars)).ok()?;
        let hip = <[HipEntry]>::ref_from_bytes_with_elems(rest, n(&header.n_hip)).ok()?;
        Some(StarCat {
            header,
            cells,
            stars,
            hip,
        })
    }

    fn check(&self) -> Result<(), CatError> {
        if let Some(cell) = self.cells.iter().position(|c| !c.is_valid()) {
            return Err(CatError::BadCell { cell: cell as u32 });
        }
        let offsets = self.cells.iter().map(|c| (c.offset.get(), c.count.get()));
        check_offsets(offsets, self.header.n_stars.get())?;
        if let Some(row) = self.stars.iter().position(|s| !s.is_valid()) {
            return Err(CatError::BadStar { row: row as u32 });
        }
        if let Some(cell) = (0..self.cells.len()).position(|c| !self.is_sorted(c)) {
            return Err(CatError::Unsorted { cell: cell as u32 });
        }
        check_hip(self.hip, self.header.n_stars.get())
    }

    fn is_sorted(&self, cell: usize) -> bool {
        self.stars_at(cell)
            .windows(2)
            .all(|w| w[0].g_mmag() <= w[1].g_mmag())
    }

    pub fn header(&self) -> &'a Header {
        self.header
    }

    pub fn cells(&self) -> &'a [Cell] {
        self.cells
    }

    /// Every star, in row order. This is the section the GPU uploads.
    pub fn all_stars(&self) -> &'a [Star] {
        self.stars
    }

    /// The stars of `cell`, brightest first. Empty for an unknown cell.
    pub fn stars(&self, cell: u32) -> &'a [Star] {
        self.stars_at(cell as usize)
    }

    fn stars_at(&self, cell: usize) -> &'a [Star] {
        self.cells
            .get(cell)
            .and_then(|c| self.stars.get(c.rows()))
            .unwrap_or(&[])
    }

    /// The leading stars of `cell` with G below `g`.
    pub fn brighter_than(&self, cell: u32, g: f32) -> &'a [Star] {
        let stars = self.stars(cell);
        let n = stars.partition_point(|s| s.g() < g);
        stars.get(..n).unwrap_or(&[])
    }

    /// The Hipparcos number of star `row`, if it has one.
    pub fn hip(&self, row: u32) -> Option<u32> {
        let i = self.hip.binary_search_by_key(&row, |e| e.row.get()).ok()?;
        self.hip.get(i).map(|e| e.hip.get())
    }

    /// The cells that may hold a star within `half_angle` (rad) of `dir`.
    /// None for a zero or non-finite `dir`, or a negative or NaN
    /// `half_angle`.
    pub fn cells_in_cone(&self, dir: V3, half_angle: f64) -> impl Iterator<Item = u32> + 'a {
        let dir = normalize(dir).filter(|_| half_angle >= 0.0);
        let cells = self.cells;
        (0u32..).zip(cells).filter_map(move |(i, cell)| {
            let reach = half_angle + cell.radius();
            (angle(dir?, cell.centre()) <= reach).then_some(i)
        })
    }
}

/// The angle (rad) between two vectors of any non-zero length.
fn angle(a: V3, b: V3) -> f64 {
    libm::atan2(norm(cross(a, b)), dot(a, b))
}

/// Checks that each `(offset, count)` starts where the one before ends, and
/// that the counts sum to `n_stars`.
fn check_offsets(cells: impl Iterator<Item = (u32, u32)>, n_stars: u32) -> Result<(), CatError> {
    let mut next = 0u64;
    for (cell, (offset, count)) in (0u32..).zip(cells) {
        if u64::from(offset) != next {
            return Err(CatError::CellOffset { cell });
        }
        next = next.saturating_add(u64::from(count));
    }
    if next != u64::from(n_stars) {
        return Err(CatError::CountSum { sum: next, n_stars });
    }
    Ok(())
}

fn check_hip(hip: &[HipEntry], n_stars: u32) -> Result<(), CatError> {
    let increasing = hip.windows(2).all(|w| w[0].row.get() < w[1].row.get());
    let in_range = hip.last().is_none_or(|e| e.row.get() < n_stars);
    if !increasing || !in_range {
        return Err(CatError::HipRows);
    }
    if hip.iter().any(|e| e.hip.get() == u32::MAX) {
        return Err(CatError::HipValue);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod verify {
    use super::*;

    const N: usize = 4;

    /// Any offset table that passes `check_offsets` gives every cell rows
    /// inside `0..n_stars`, so `stars(cell)` never slices out of bounds.
    #[kani::proof]
    #[kani::unwind(6)]
    fn passing_offsets_stay_in_bounds() {
        let table: [(u32, u32); N] = kani::any();
        let n_stars: u32 = kani::any();
        if check_offsets(table.into_iter(), n_stars).is_ok() {
            for (offset, count) in table {
                let r = rows(offset, count);
                assert!(r.start <= r.end);
                assert!(r.end <= n_stars as usize);
            }
        }
    }
}
