//! The k-vector (Mortari 1997): the entries of a sorted table within
//! `[lo, hi]` in constant time plus a short binary search. `k[i]` counts the
//! values below the line `z(i) = q + m·i`, which spans the table.

use alloc::vec::Vec;
use core::ops::Range;

/// A sorted table the k-vector indexes.
pub trait Sorted {
    /// The number of values, `n`.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Value `i`, for `i < n`.
    fn value(&self, i: usize) -> f32;
    /// `k[i]`, for `i ≤ n`.
    fn k(&self, i: usize) -> u32;
}

/// The line `z(i) = q + m·i`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    pub m: f64,
    pub q: f64,
}

impl Line {
    /// The line for `n` sorted values spanning `[first, last]`, with a margin
    /// so that `z(0) < first` and `z(n) > last`.
    pub fn fit(first: f32, last: f32, n: usize) -> Line {
        let (first, last) = (f64::from(first), f64::from(last));
        let margin = 1e-9 + 1e-7 * first.abs().max(last.abs());
        let span = (last - first) + 2.0 * margin;
        let m = span / n.max(1) as f64;
        Line {
            m,
            q: first - margin,
        }
    }

    pub fn z(&self, i: usize) -> f64 {
        self.q + self.m * i as f64
    }

    /// The largest `i ≤ n` with `z(i) ≤ x`, or 0 when there is none.
    fn below(&self, x: f64, n: usize) -> usize {
        let guess = libm::floor((x - self.q) / self.m);
        let mut i = if guess.is_nan() || guess <= 0.0 {
            0
        } else {
            (guess as usize).min(n)
        };
        while i > 0 && self.z(i) > x {
            i -= 1;
        }
        i
    }

    /// The smallest `i ≤ n` with `z(i) > x`, or `n` when there is none.
    fn above(&self, x: f64, n: usize) -> usize {
        let mut i = self.below(x, n);
        while i < n && self.z(i) <= x {
            i += 1;
        }
        i
    }
}

/// Builds `k` for sorted `values`, `n + 1` entries. Build time only.
pub fn build(values: &[f32]) -> (Vec<u32>, Line) {
    let n = values.len();
    let line = match (values.first(), values.last()) {
        (Some(first), Some(last)) => Line::fit(*first, *last, n),
        _ => Line { m: 1.0, q: 0.0 },
    };
    let mut k = Vec::with_capacity(n + 1);
    let mut below = 0usize;
    for i in 0..=n {
        let z = line.z(i);
        while below < n && f64::from(values[below]) < z {
            below += 1;
        }
        k.push(below as u32);
    }
    (k, line)
}

/// The indices of the values in `[lo, hi]`. Empty for `lo > hi` or a NaN
/// bound; never out of `0..=n`.
pub fn range(table: &impl Sorted, line: &Line, lo: f32, hi: f32) -> Range<usize> {
    let n = table.len();
    if table.is_empty() || lo.is_nan() || hi.is_nan() || lo > hi {
        return 0..0;
    }
    let first = line.below(f64::from(lo), n);
    let end = line.above(f64::from(hi), n);
    let window = window(|i| table.k(i), first, end, n);
    let start = lower_bound(window.clone(), |i| table.value(i) < lo);
    let end = lower_bound(start..window.end, |i| table.value(i) <= hi);
    start..end
}

/// `k[first]..k[end]`, clamped into `0..=n` and never reversed.
pub fn window(k: impl Fn(usize) -> u32, first: usize, end: usize, n: usize) -> Range<usize> {
    let at = |i: usize| (k(i.min(n)) as usize).min(n);
    let start = at(first);
    start..at(end).max(start)
}

/// The first index in `within` where `before` is false, assuming it is true
/// on a prefix. Stays within `within`.
pub fn lower_bound(within: Range<usize>, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (within.start, within.end.max(within.start));
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// The window stays in `0..=n` and is never reversed, for any `k` table
    /// of up to 8 entries and any indices.
    #[kani::proof]
    #[kani::unwind(10)]
    fn window_is_in_bounds() {
        let k: [u32; 9] = kani::any();
        let n: usize = kani::any();
        kani::assume(n <= 8);
        let (first, end): (usize, usize) = (kani::any(), kani::any());
        let w = window(|i| k[i], first, end, n);
        assert!(w.start <= w.end && w.end <= n);
    }

    /// The search stays in its range for any predicate.
    #[kani::proof]
    #[kani::unwind(10)]
    fn lower_bound_is_in_bounds() {
        let flags: [bool; 8] = kani::any();
        let (start, end): (usize, usize) = (kani::any(), kani::any());
        kani::assume(start <= 8 && end <= 8);
        let i = lower_bound(start..end, |i| flags[i]);
        assert!(i >= start && i <= end.max(start));
    }
}
