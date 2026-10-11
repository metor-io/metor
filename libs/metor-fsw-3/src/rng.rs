//! A seeded random number generator for models and systems.

/// SplitMix64. Each system draws its own stream from the target seed and its
/// instance name, so adding a system leaves the others' draws unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    /// The stream for instance `name` under target seed `seed`.
    pub fn seeded(seed: u64, name: &str) -> Self {
        Self(finalize(seed ^ finalize(fnv1a64(name))))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        finalize(self.0)
    }

    /// True with probability `p`, where `p` is a fraction of `u32::MAX`.
    pub fn chance(&mut self, p: u32) -> bool {
        let draw = (self.next_u64() >> 32) as u32;
        p == u32::MAX || draw < p
    }

    /// A draw in `0..n`, or 0 when `n` is 0.
    pub fn below(&mut self, n: u32) -> u32 {
        let draw = self.next_u64() >> 32;
        ((draw * u64::from(n)) >> 32) as u32
    }
}

/// SplitMix64's output function.
fn finalize(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fnv1a64(name: &str) -> u64 {
    let step = |hash: u64, byte: &u8| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    name.as_bytes().iter().fold(0xcbf2_9ce4_8422_2325, step)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draws(mut rng: Rng) -> Vec<u64> {
        (0..8).map(|_| rng.next_u64()).collect()
    }

    #[test]
    fn same_seed_and_name_give_the_same_stream() {
        assert_eq!(
            draws(Rng::seeded(7, "t.gps")),
            draws(Rng::seeded(7, "t.gps"))
        );
    }

    #[test]
    fn names_and_seeds_give_different_streams() {
        let a = draws(Rng::seeded(7, "t.a"));
        assert_ne!(a, draws(Rng::seeded(7, "t.b")));
        assert_ne!(a, draws(Rng::seeded(8, "t.a")));
        assert_ne!(draws(Rng::seeded(7, "")), a, "an empty name seeds too");
    }

    #[test]
    fn below_stays_below() {
        let mut rng = Rng::seeded(1, "x");
        let mut seen = [false; 8];
        for _ in 0..10_000 {
            let n = rng.below(8);
            assert!(n < 8);
            seen[n as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "every value comes up");
        assert_eq!(rng.below(0), 0);
    }

    #[test]
    fn chance_edges_are_certain() {
        let mut rng = Rng::seeded(3, "x");
        assert!((0..1000).all(|_| rng.chance(u32::MAX)));
        assert!((0..1000).all(|_| !rng.chance(0)));
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn below_is_below_n() {
        let mut rng = Rng(kani::any());
        let n: u32 = kani::any();
        kani::assume(n > 0);
        assert!(rng.below(n) < n);
    }
}
