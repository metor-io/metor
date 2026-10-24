//! Builds the onboard star DB ([`metor_star::catalog`]) from the render
//! catalog (`starcat-g8.bin`), so the tracker matches the stars the
//! simulator draws.
//!
//! Selection keeps only stars with a Hipparcos number (the simulator's truth
//! names stars by HIP) and uniformises them over the DB's cube-face cells:
//! every cell gets its brightest star before any cell gets its second, and so
//! on, until `n_stars` are taken.
//!
//! Stars closer than `merge_px` blend into one image, so each HIP star is
//! first merged with every catalog star within that distance into one
//! flux-weighted star, which keeps the brightest member's HIP number; a HIP
//! star with a brighter HIP neighbour is dropped into that neighbour.

use metor_star::catalog::build::build;
use metor_star::catalog::cells::cell_of;
use metor_star::catalog::{BuildConfig, BuildError, CatStar};
use metor_star::starcat::StarCat;

/// The committed DB's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opts {
    /// Stars to keep.
    pub n_stars: usize,
    /// The largest pair separation stored (deg).
    pub fov_deg: f64,
    /// Cells per cube-face edge, for both uniformising and the sky index.
    pub face_n: u32,
    /// Stars closer than this (px) are merged.
    pub merge_px: f64,
    /// One pixel's angle (rad): the star simulator's 5.5 µm at 16 mm.
    pub px_rad: f64,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            n_stars: 2000,
            fov_deg: 20.0,
            face_n: 8,
            merge_px: 2.5,
            px_rad: 5.5e-6 / 16e-3,
        }
    }
}

/// HIP star `row` merged with every star within `radius` (rad), or `None`
/// when a brighter HIP star is that close and carries the merge.
pub fn merged(cat: &StarCat, row: u32, radius: f64) -> Option<CatStar> {
    let star = cat.all_stars().get(row as usize)?;
    let hip = cat.hip(row)?;
    let (r0, g0) = (star.unit(), star.g());
    let cos_r = radius.cos();
    let (mut sum, mut flux) = ([0.0; 3], 0.0);
    for cell in cat.cells_in_cone(r0, radius) {
        let rows = cat.cells()[cell as usize].rows();
        for (other, s) in rows.clone().zip(cat.stars(cell)) {
            let r = s.unit();
            if metor_star::geom::dot(r, r0) < cos_r {
                continue;
            }
            let other = other as u32;
            let brighter = (s.g(), cat.hip(other)) < (g0, Some(hip));
            if other != row && brighter && cat.hip(other).is_some() {
                return None;
            }
            let w = 10f64.powf(-0.4 * f64::from(s.g()));
            sum = metor_star::geom::add(sum, metor_star::geom::scale(r, w));
            flux += w;
        }
    }
    Some(CatStar {
        r: metor_star::geom::normalize(sum).unwrap_or(r0),
        mag: -2.5 * flux.log10(),
        hip,
    })
}

/// The uniformised, merged HIP stars of `cat`, at most `n` of them.
pub fn select(cat: &StarCat, opts: &Opts) -> Vec<CatStar> {
    let (n, face_n) = (opts.n_stars, opts.face_n);
    let radius = opts.merge_px * opts.px_rad;
    // (cell, G, star) for every HIP star left after merging.
    let mut all: Vec<(u32, f32, CatStar)> = (0..cat.all_stars().len() as u32)
        .filter_map(|row| {
            let star = merged(cat, row, radius)?;
            let g = star.mag as f32;
            Some((cell_of(star.r, face_n), g, star))
        })
        .collect();
    // Brightest first within each cell, ties by HIP so the order is total.
    all.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.hip.cmp(&b.2.hip))
    });
    let mut ranked: Vec<(usize, f32, CatStar)> = Vec::with_capacity(all.len());
    let mut rank = 0;
    for (i, (cell, g, star)) in all.iter().enumerate() {
        rank = if i > 0 && all[i - 1].0 == *cell {
            rank + 1
        } else {
            0
        };
        ranked.push((rank, *g, *star));
    }
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.hip.cmp(&b.2.hip))
    });
    ranked.truncate(n);
    ranked.into_iter().map(|(_, _, s)| s).collect()
}

/// The DB bytes for `cat` under `opts`.
pub fn build_db(cat: &StarCat, opts: &Opts) -> Result<Vec<u8>, BuildError> {
    let stars = select(cat, opts);
    let mag_limit = stars
        .iter()
        .map(|s| s.mag)
        .fold(f64::NEG_INFINITY, f64::max);
    let cfg = BuildConfig {
        fov_max_rad: opts.fov_deg.to_radians(),
        mag_limit,
        epoch_jy: cat.header().epoch_jyear.get(),
        face_n: opts.face_n,
    };
    build(&stars, &cfg)
}

#[cfg(test)]
mod tests {
    use metor_star::catalog::Db;
    use metor_star::catalog::cells::cell_of;

    use super::*;

    static STARCAT: &[u8] = include_bytes!("../../../data/starcat-g8.bin");
    static COMMITTED: &[u8] = include_bytes!("../../../data/stars-u2000.stdb");

    #[test]
    fn the_committed_db_is_what_build_writes() {
        let cat = StarCat::parse(STARCAT).unwrap();
        let db = build_db(&cat, &Opts::default()).unwrap();
        assert!(
            db == COMMITTED,
            "rebuild with `cargo run -p star-db --release -- build`"
        );
    }

    #[test]
    fn selection_is_uniform_and_named() {
        let cat = StarCat::parse(STARCAT).unwrap();
        let opts = Opts::default();
        let stars = select(&cat, &opts);
        assert_eq!(stars.len(), opts.n_stars);
        let mut per_cell = vec![0usize; 6 * (opts.face_n * opts.face_n) as usize];
        for s in &stars {
            per_cell[cell_of(s.r, opts.face_n) as usize] += 1;
        }
        let (min, max) = (per_cell.iter().min(), per_cell.iter().max());
        assert_eq!((min, max), (Some(&5), Some(&6)), "2000 over 384 cells");
        // Every row keeps the HIP number of its source star (or of a merge's
        // brightest member).
        let mut hips: Vec<u32> = stars.iter().map(|s| s.hip).collect();
        hips.sort_unstable();
        hips.dedup();
        assert_eq!(hips.len(), stars.len());
        let db = Db::parse(COMMITTED).unwrap();
        for row in 0..db.n_stars() as u16 {
            let hip = db.hip(row).unwrap();
            let src = stars.iter().find(|s| s.hip == hip).expect("a source star");
            let r = db.star(row).unwrap().vector();
            let d = (0..3).map(|i| (r[i] - src.r[i]).abs()).fold(0.0, f64::max);
            assert!(d < 1e-6, "HIP {hip} moved by {d}");
        }
    }

    /// HIP 2484 and 2487 are 0.4 px apart: one merged star keeps the
    /// brighter's number at the flux-weighted position.
    #[test]
    fn a_close_pair_merges_into_its_brighter_star() {
        let cat = StarCat::parse(STARCAT).unwrap();
        let row = |hip| {
            (0..cat.all_stars().len() as u32)
                .find(|r| cat.hip(*r) == Some(hip))
                .unwrap()
        };
        let (a, b) = (row(2484), row(2487));
        let (ga, gb) = (
            cat.all_stars()[a as usize].g(),
            cat.all_stars()[b as usize].g(),
        );
        let (bright, faint) = if ga < gb { (a, b) } else { (b, a) };
        let radius = 2.5 * Opts::default().px_rad;
        assert_eq!(merged(&cat, faint, radius), None);
        let m = merged(&cat, bright, radius).unwrap();
        assert_eq!(Some(m.hip), cat.hip(bright));
        assert!(m.mag < f64::from(ga.min(gb)));
        let (ra, rb) = (
            cat.all_stars()[a as usize].unit(),
            cat.all_stars()[b as usize].unit(),
        );
        let between = metor_star::geom::dot(m.r, ra).min(metor_star::geom::dot(m.r, rb));
        assert!(
            between > metor_star::geom::dot(ra, rb),
            "the merge lies between them"
        );
        // A lone star merges with nothing.
        let lone = merged(&cat, row(32349), radius).unwrap();
        assert!((lone.mag - f64::from(cat.all_stars()[row(32349) as usize].g())).abs() < 1e-6);
    }

    #[test]
    fn selection_takes_the_brightest_first() {
        let cat = StarCat::parse(STARCAT).unwrap();
        let opts = |n_stars| Opts {
            n_stars,
            ..Opts::default()
        };
        let one = select(&cat, &opts(384));
        let brightest = one.iter().map(|s| s.mag).fold(f64::INFINITY, f64::min);
        assert!(brightest < -1.0, "Sirius is in: {brightest}");
        assert!(select(&cat, &opts(0)).is_empty());
    }
}
