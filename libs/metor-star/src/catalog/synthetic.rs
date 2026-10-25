//! A deterministic synthetic catalog that stands in for a real one in tests
//! and the ideal tracker. Star `i` of the returned list has catalog id `i + 1`.

use alloc::vec::Vec;

use crate::geom::V3;

/// A xorshift64* stream, so the crate needs no `rand`.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> XorShift {
        XorShift(seed ^ 0x9e37_79b9_7f4a_7c15 | 1)
    }

    /// Uniform in [-1, 1).
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11;
        bits as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// `n` unit vectors uniform on the sphere (Marsaglia 1972), the same for the
/// same `seed`.
pub fn uniform(n: usize, seed: u64) -> Vec<V3> {
    let mut rng = XorShift::new(seed);
    let mut stars = Vec::with_capacity(n);
    while stars.len() < n {
        let (a, b) = (rng.next(), rng.next());
        let s = a * a + b * b;
        if s >= 1.0 || s == 0.0 {
            continue;
        }
        let k = 2.0 * libm::sqrt(1.0 - s);
        stars.push([a * k, b * k, 1.0 - 2.0 * s]);
    }
    stars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::norm;

    #[test]
    fn stars_are_unit_and_repeatable() {
        let stars = uniform(5000, 3);
        assert_eq!(stars.len(), 5000);
        assert!(stars.iter().all(|s| (norm(*s) - 1.0).abs() < 1e-14));
        assert_eq!(stars, uniform(5000, 3));
        assert_ne!(stars, uniform(5000, 4));
    }

    /// Each octant holds about an eighth of the stars, and the mean is near zero.
    #[test]
    fn stars_cover_the_sphere_evenly() {
        let stars = uniform(8000, 1);
        let mut octants = [0usize; 8];
        for s in &stars {
            let i =
                (s[0] > 0.0) as usize | ((s[1] > 0.0) as usize) << 1 | ((s[2] > 0.0) as usize) << 2;
            octants[i] += 1;
        }
        assert!(
            octants.iter().all(|n| (900..1100).contains(n)),
            "{octants:?}"
        );
        let mean = stars.iter().fold([0.0; 3], |m, s| crate::geom::add(m, *s));
        assert!(norm(mean) / 8000.0 < 0.02);
    }
}
