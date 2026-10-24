//! Writes a DB from a star list. Build time only: it allocates, and its pair
//! search is `O(N²)`.

use alloc::vec::Vec;

use zerocopy::IntoBytes;
use zerocopy::little_endian::{F32, F64, I16, U16, U32};

use super::cells::cell_of;
use super::format::{
    ALIGN, CELLS, HEADER_LEN, Header, KVECTOR, MAGIC, MAX_STARS, PAIRS, Pair, RESERVED, STARS,
    Section, Star, VERSION, crc_of,
};
use super::kvector;
use crate::geom::{V3, cross, dot, norm};

/// A star going into the DB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatStar {
    /// Unit vector in ICRF at the epoch.
    pub r: V3,
    pub mag: f64,
    pub hip: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuildConfig {
    /// The largest pair separation to store (rad), the field's diagonal.
    pub fov_max_rad: f64,
    pub mag_limit: f64,
    pub epoch_jy: f64,
    /// Cells per cube-face edge.
    pub face_n: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// More than [`MAX_STARS`] stars.
    TooManyStars,
    /// More pairs than a `u32` counts, or a DB past 4 GiB.
    TooManyPairs,
    /// `face_n` outside `1..=MAX_FACE_N`.
    FaceN,
}

/// The most cells per face edge; `6·n²` cells must fit a `u32` with room.
pub const MAX_FACE_N: u32 = 1024;

/// The angle between two unit vectors, accurate at small separations.
pub fn separation(a: V3, b: V3) -> f64 {
    libm::atan2(norm(cross(a, b)), dot(a, b))
}

/// Writes the DB for `stars`, sorted by cell then Hipparcos number.
pub fn build(stars: &[CatStar], cfg: &BuildConfig) -> Result<Vec<u8>, BuildError> {
    if stars.len() > MAX_STARS as usize {
        return Err(BuildError::TooManyStars);
    }
    let n = cfg.face_n;
    if !(1..=MAX_FACE_N).contains(&n) {
        return Err(BuildError::FaceN);
    }
    // Bin the stored f32 vector, not the f64 input, so a star near a cell
    // edge sits in the cell the onboard `cell_of` gives it.
    let mut sorted: Vec<(u32, Star)> = stars
        .iter()
        .map(|s| {
            let row = to_row(s);
            (cell_of(row.vector(), n), row)
        })
        .collect();
    sorted.sort_by_key(|(cell, s)| (*cell, s.hip.get()));
    let cells = cell_offsets(&sorted, 6 * n * n);
    let rows: Vec<Star> = sorted.into_iter().map(|(_, s)| s).collect();
    let pairs = pairs_within(&rows, cfg.fov_max_rad);
    let n_pairs = u32::try_from(pairs.len()).map_err(|_| BuildError::TooManyPairs)?;
    let thetas: Vec<f32> = pairs.iter().map(|p| p.theta.get()).collect();
    let (k, line) = kvector::build(&thetas);
    let k: Vec<U32> = k.into_iter().map(U32::new).collect();
    let mut header = Header {
        magic: MAGIC,
        version: U32::new(VERSION),
        flags: U32::new(0),
        epoch_jy: F64::new(cfg.epoch_jy),
        mag_limit: F32::new(cfg.mag_limit as f32),
        fov_max_rad: F32::new(cfg.fov_max_rad as f32),
        n_stars: U32::new(rows.len() as u32),
        n_pairs: U32::new(n_pairs),
        n_cells: U32::new(6 * n * n),
        face_n: U32::new(n),
        km: F64::new(line.m),
        kq: F64::new(line.q),
        sections: [Section {
            offset: U32::new(0),
            len: U32::new(0),
        }; 5],
        crc32: U32::new(0),
        _pad: [0; 4],
    };
    let mut body = Vec::new();
    let parts: [(usize, &[u8]); 5] = [
        (STARS, rows.as_bytes()),
        (CELLS, cells.as_bytes()),
        (PAIRS, pairs.as_bytes()),
        (KVECTOR, k.as_bytes()),
        (RESERVED, &[]),
    ];
    for (i, bytes) in parts {
        while !(HEADER_LEN + body.len()).is_multiple_of(ALIGN) {
            body.push(0);
        }
        header.sections[i] = Section {
            offset: U32::new((HEADER_LEN + body.len()) as u32),
            len: U32::new(bytes.len() as u32),
        };
        body.extend_from_slice(bytes);
    }
    if u32::try_from(HEADER_LEN + body.len()).is_err() {
        return Err(BuildError::TooManyPairs);
    }
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&body);
    let crc = crc_of(&out).unwrap_or(0);
    header.crc32 = U32::new(crc);
    out[..HEADER_LEN].copy_from_slice(header.as_bytes());
    Ok(out)
}

fn to_row(s: &CatStar) -> Star {
    Star {
        r: s.r.map(|x| F32::new(x as f32)),
        mag_centi: I16::new(libm::round(s.mag * 100.0).clamp(-32768.0, 32767.0) as i16),
        _pad: U16::new(0),
        hip: U32::new(s.hip),
    }
}

/// `n_cells + 1` offsets of each cell's first star in cell-sorted `stars`.
fn cell_offsets(stars: &[(u32, Star)], n_cells: u32) -> Vec<U32> {
    let mut offsets = Vec::with_capacity(n_cells as usize + 1);
    let mut row = 0usize;
    for c in 0..=n_cells {
        while row < stars.len() && stars[row].0 < c {
            row += 1;
        }
        offsets.push(U32::new(row as u32));
    }
    offsets
}

/// Every pair of rows closer than `max` (rad), sorted by θ then rows.
fn pairs_within(rows: &[Star], max: f64) -> Vec<Pair> {
    let vectors: Vec<V3> = rows.iter().map(Star::vector).collect();
    let cos_max = libm::cos(max);
    let mut pairs = Vec::new();
    for (a, ra) in vectors.iter().enumerate() {
        for (b, rb) in vectors.iter().enumerate().skip(a + 1) {
            if dot(*ra, *rb) < cos_max - 1e-12 {
                continue;
            }
            let theta = separation(*ra, *rb);
            if theta < max {
                pairs.push(Pair {
                    theta: F32::new(theta as f32),
                    a: U16::new(a as u16),
                    b: U16::new(b as u16),
                });
            }
        }
    }
    pairs.sort_by(|x, y| {
        x.theta
            .get()
            .total_cmp(&y.theta.get())
            .then((x.a.get(), x.b.get()).cmp(&(y.a.get(), y.b.get())))
    });
    pairs
}
