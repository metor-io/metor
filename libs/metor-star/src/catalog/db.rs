//! The onboard star DB: stars sorted by cube-face cell, every pair closer
//! than the field's diagonal sorted by angle, and the pairs' k-vector.

use alloc::vec::Vec;
use core::ops::Range;

use super::cells::{self, cell_of};
use super::kvector::{Line, Sorted, range};
use crate::geom::{V3, dot};

/// The most stars a DB may hold: pairs index them with `u16`.
pub const MAX_STARS: usize = 65_535;

/// A catalog star: its unit vector in ICRF, magnitude and Hipparcos number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Star {
    pub r: [f32; 3],
    pub mag: f32,
    pub hip: u32,
}

impl Star {
    pub fn vector(&self) -> V3 {
        self.r.map(f64::from)
    }

    /// [`vector`](Self::vector) rescaled to unit length: the f32 rounding
    /// leaves `|r|` off by up to about 1e-7, more than a 1 px match cone.
    pub fn unit(&self) -> V3 {
        let r = self.vector();
        crate::geom::normalize(r).unwrap_or(r)
    }
}

/// Two stars, by row, and the angle between them (rad).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pair {
    pub theta: f32,
    pub a: u16,
    pub b: u16,
}

/// A built DB. Every accessor is total.
#[derive(Debug, Clone)]
pub struct Db {
    pub(super) stars: Vec<Star>,
    /// `n_cells + 1` offsets into `stars`.
    pub(super) cells: Vec<u32>,
    pub(super) pairs: Vec<Pair>,
    pub(super) k: Vec<u32>,
    pub(super) line: Line,
    /// Cells per cube-face edge.
    pub(super) face_n: u32,
}

impl Db {
    pub fn n_stars(&self) -> usize {
        self.stars.len()
    }

    pub fn n_cells(&self) -> u32 {
        6 * self.face_n * self.face_n
    }

    pub fn star(&self, row: u16) -> Option<&Star> {
        self.stars.get(usize::from(row))
    }

    /// The Hipparcos number of `row`.
    pub fn hip(&self, row: u16) -> Option<u32> {
        self.star(row).map(|s| s.hip)
    }

    pub fn pairs(&self) -> &[Pair] {
        &self.pairs
    }

    /// The pairs with θ in `[lo, hi]`, as a range of [`pairs`](Self::pairs).
    pub fn pair_range(&self, lo: f32, hi: f32) -> Range<usize> {
        range(self, &self.line, lo, hi)
    }

    /// The stars in cell `c`.
    pub fn cell(&self, c: u32) -> Range<usize> {
        let at = |i: usize| self.cells.get(i).map_or(0, |&v| v as usize);
        let n = self.stars.len();
        let start = at(c as usize).min(n);
        start..at(c as usize + 1).clamp(start, n)
    }

    /// The cell a direction falls in.
    pub fn cell_of(&self, r: V3) -> u32 {
        cell_of(r, self.face_n)
    }

    /// Appends the rows of the stars within `radius` (rad) of unit `axis`.
    pub fn cone(&self, axis: V3, radius: f64, out: &mut impl Extend<u16>) {
        let cos_r = libm::cos(radius);
        for c in 0..self.n_cells() {
            if !cells::may_touch(c, self.face_n, axis, radius) {
                continue;
            }
            let rows = self.cell(c);
            let hits = self.stars[rows.clone()]
                .iter()
                .zip(rows)
                .filter(|(s, _)| dot(s.vector(), axis) >= cos_r)
                .map(|(_, row)| row as u16);
            out.extend(hits);
        }
    }
}

impl Sorted for Db {
    fn len(&self) -> usize {
        self.pairs.len()
    }

    fn value(&self, i: usize) -> f32 {
        self.pairs.get(i).map_or(f32::INFINITY, |p| p.theta)
    }

    fn k(&self, i: usize) -> u32 {
        self.k.get(i).copied().unwrap_or(self.pairs.len() as u32)
    }
}
