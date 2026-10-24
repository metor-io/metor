//! Builds the onboard star DB ([`metor_star::catalog`]) from the render
//! catalog (`starcat-g8.bin`), so the tracker matches the stars the
//! simulator draws.
//!
//! Selection keeps only stars with a Hipparcos number (the simulator's truth
//! names stars by HIP) and uniformises them over the DB's cube-face cells:
//! every cell gets its brightest star before any cell gets its second, and so
//! on, until `n_stars` are taken.

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
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            n_stars: 2000,
            fov_deg: 20.0,
            face_n: 8,
        }
    }
}

/// The uniformised HIP stars of `cat`, at most `n` of them.
pub fn select(cat: &StarCat, n: usize, face_n: u32) -> Vec<CatStar> {
    // (cell, G, star) for every HIP star.
    let mut all: Vec<(u32, f32, CatStar)> = (0u32..)
        .zip(cat.all_stars())
        .filter_map(|(row, s)| {
            let hip = cat.hip(row)?;
            let r = s.unit();
            let star = CatStar {
                r,
                mag: f64::from(s.g()),
                hip,
            };
            Some((cell_of(r, face_n), s.g(), star))
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
    let stars = select(cat, opts.n_stars, opts.face_n);
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
        let stars = select(&cat, opts.n_stars, opts.face_n);
        assert_eq!(stars.len(), opts.n_stars);
        let mut per_cell = vec![0usize; 6 * (opts.face_n * opts.face_n) as usize];
        for s in &stars {
            per_cell[cell_of(s.r, opts.face_n) as usize] += 1;
        }
        let (min, max) = (per_cell.iter().min(), per_cell.iter().max());
        assert_eq!((min, max), (Some(&5), Some(&6)), "2000 over 384 cells");
        // Every row keeps the HIP number of its source star.
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

    #[test]
    fn selection_takes_the_brightest_first() {
        let cat = StarCat::parse(STARCAT).unwrap();
        let one = select(&cat, 384, 8);
        let brightest = one.iter().map(|s| s.mag).fold(f64::INFINITY, f64::min);
        assert!(brightest < -1.0, "Sirius is in: {brightest}");
        assert!(select(&cat, 0, 8).is_empty());
    }
}
