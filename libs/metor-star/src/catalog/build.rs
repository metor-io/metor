//! Builds a [`Db`] from a star list, or from the render catalog the way the
//! tracker loads it at startup. Allocates; the pair search is `O(N²)`.

use alloc::vec::Vec;

use super::cells::cell_of;
use super::db::{Db, MAX_STARS, Pair, Star};
use super::kvector;
use crate::geom::{V3, add, cross, dot, norm, normalize, scale};
use crate::starcat::StarCat;

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
    /// Cells per cube-face edge.
    pub face_n: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// More than [`MAX_STARS`] stars.
    TooManyStars,
    /// More pairs than a `u32` counts.
    TooManyPairs,
    /// `face_n` outside `1..=MAX_FACE_N`.
    FaceN,
}

/// The most cells per face edge; `6·n²` cells must fit a `u32` with room.
pub const MAX_FACE_N: u32 = 1024;

/// The onboard DB's selection from the render catalog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Select {
    pub n_stars: usize,
    /// The largest pair separation stored (deg).
    pub fov_deg: f64,
    pub face_n: u32,
    /// Stars closer than this (px) blend into one image and are merged.
    pub merge_px: f64,
    /// One pixel's angle (rad).
    pub px_rad: f64,
}

impl Default for Select {
    /// 2000 stars for the example's 20°, 5.5 µm / 16 mm tracker.
    fn default() -> Select {
        Select {
            n_stars: 2000,
            fov_deg: 20.0,
            face_n: 8,
            merge_px: 2.5,
            px_rad: 5.5e-6 / 16e-3,
        }
    }
}

/// The angle between two unit vectors, accurate at small separations.
pub fn separation(a: V3, b: V3) -> f64 {
    libm::atan2(norm(cross(a, b)), dot(a, b))
}

impl Db {
    /// The DB of `stars`, sorted by cell then Hipparcos number.
    pub fn build(stars: &[CatStar], cfg: &BuildConfig) -> Result<Db, BuildError> {
        if stars.len() > MAX_STARS {
            return Err(BuildError::TooManyStars);
        }
        let n = cfg.face_n;
        if !(1..=MAX_FACE_N).contains(&n) {
            return Err(BuildError::FaceN);
        }
        // Bin the stored f32 vector, not the f64 input, so a star near a cell
        // edge sits in the cell `cell_of` gives it.
        let mut sorted: Vec<(u32, Star)> = stars
            .iter()
            .map(|s| {
                let row = Star {
                    r: s.r.map(|x| x as f32),
                    mag: s.mag as f32,
                    hip: s.hip,
                };
                (cell_of(row.vector(), n), row)
            })
            .collect();
        sorted.sort_by_key(|(cell, s)| (*cell, s.hip));
        let n_cells = 6 * n * n;
        let mut cells = Vec::with_capacity(n_cells as usize + 1);
        cells.extend((0..=n_cells).map(|c| sorted.partition_point(|s| s.0 < c) as u32));
        let stars: Vec<Star> = sorted.into_iter().map(|(_, s)| s).collect();
        let pairs = pairs_within(&stars, cfg.fov_max_rad);
        u32::try_from(pairs.len()).map_err(|_| BuildError::TooManyPairs)?;
        let thetas: Vec<f32> = pairs.iter().map(|p| p.theta).collect();
        let (k, line) = kvector::build(&thetas);
        Ok(Db {
            stars,
            cells,
            pairs,
            k,
            line,
            face_n: n,
        })
    }

    /// The onboard DB from the render catalog: its [`select`]ed stars.
    pub fn from_starcat(cat: &StarCat, opts: &Select) -> Result<Db, BuildError> {
        let cfg = BuildConfig {
            fov_max_rad: opts.fov_deg.to_radians(),
            face_n: opts.face_n,
        };
        Db::build(&select(cat, opts), &cfg)
    }
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
                let (theta, a, b) = (theta as f32, a as u16, b as u16);
                pairs.push(Pair { theta, a, b });
            }
        }
    }
    pairs.sort_by(|x, y| {
        x.theta
            .total_cmp(&y.theta)
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    pairs
}

/// HIP star `row` merged with every star within `cos_r` of it in `cells`
/// into one flux-weighted star, or `None` when a brighter HIP star is that
/// close and carries the merge. `units` are every star's unit vector.
fn merge(
    cat: &StarCat,
    units: &[V3],
    row: usize,
    cos_r: f64,
    cells: impl Iterator<Item = u32>,
) -> Option<CatStar> {
    let hip = cat.hip(row as u32)?;
    let (r0, g0) = (units[row], cat.all_stars()[row].g());
    let (mut sum, mut flux) = ([0.0; 3], 0.0);
    for cell in cells {
        let rows = cat.cells()[cell as usize].rows();
        for (other, s) in rows.zip(cat.stars(cell)) {
            let r = units[other];
            if dot(r, r0) < cos_r {
                continue;
            }
            let other_hip = cat.hip(other as u32);
            if other != row && other_hip.is_some() && (s.g(), other_hip) < (g0, Some(hip)) {
                return None;
            }
            let w = libm::pow(10.0, -0.4 * f64::from(s.g()));
            sum = add(sum, scale(r, w));
            flux += w;
        }
    }
    Some(CatStar {
        r: normalize(sum).unwrap_or(r0),
        mag: -2.5 * libm::log10(flux),
        hip,
    })
}

/// The HIP stars of `cat` (the simulator's truth names stars by HIP), each
/// merged with the stars it blends with, uniform over the DB's cells: every
/// cell gets its brightest star before any gets its second.
pub fn select(cat: &StarCat, opts: &Select) -> Vec<CatStar> {
    let radius = opts.merge_px * opts.px_rad;
    let cells = cat.cells();
    let units: Vec<V3> = cat.all_stars().iter().map(|s| s.unit()).collect();
    let centres: Vec<V3> = cells.iter().map(|c| c.centre()).collect();
    let widest = cells.iter().map(|c| c.radius()).fold(0.0, f64::max);
    let cos_reach = libm::cos(widest + radius);
    let mut near = Vec::new();
    let mut all: Vec<(u32, CatStar)> = Vec::new();
    for (cell, centre) in cells.iter().zip(&centres) {
        // The cells a star within `radius` of one in `cell` may be in.
        let cos_gap = libm::cos((cell.radius() + widest + radius).min(core::f64::consts::PI));
        near.clear();
        near.extend(
            (0u32..)
                .zip(&centres)
                .filter(|(_, o)| dot(*centre, **o) >= cos_gap),
        );
        for row in cell.rows() {
            let reach = near
                .iter()
                .filter(|(_, o)| dot(units[row], **o) >= cos_reach);
            let cells = reach.map(|(c, _)| *c);
            if let Some(star) = merge(cat, &units, row, libm::cos(radius), cells) {
                all.push((cell_of(star.r, opts.face_n), star));
            }
        }
    }
    let order = |a: &CatStar, b: &CatStar| a.mag.total_cmp(&b.mag).then(a.hip.cmp(&b.hip));
    all.sort_by(|a, b| a.0.cmp(&b.0).then(order(&a.1, &b.1)));
    // Rank within each cell, then take rank by rank.
    let mut ranked: Vec<(usize, CatStar)> = Vec::with_capacity(all.len());
    for (i, (cell, star)) in all.iter().enumerate() {
        let same = i > 0 && all[i - 1].0 == *cell;
        let rank = ranked.last().filter(|_| same).map_or(0, |r| r.0 + 1);
        ranked.push((rank, *star));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(order(&a.1, &b.1)));
    ranked.truncate(opts.n_stars);
    ranked.into_iter().map(|(_, s)| s).collect()
}
