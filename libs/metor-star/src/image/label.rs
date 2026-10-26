//! 8-connected labelling over runs. One raster pass emits each row's runs of
//! lit pixels with their moments, and unions each run with the runs of the
//! row above that touch `[x0 − 1, x1 + 1]`. Roots are the smallest run id,
//! so `parent[i] ≤ i` always holds.

use alloc::vec::Vec;

use super::ImageError;
use super::background::Background;

/// A blob's background-subtracted moments about the pixel origin (pixel
/// `(i, j)` contributes at `(i, j)`), its area and its peak (DN).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Blob {
    pub w: f64,
    pub wx: f64,
    pub wy: f64,
    pub wxx: f64,
    pub wyy: f64,
    pub wxy: f64,
    pub area: u32,
    pub peak: u16,
}

impl Blob {
    fn add(&mut self, o: &Blob) {
        self.w += o.w;
        self.wx += o.wx;
        self.wy += o.wy;
        self.wxx += o.wxx;
        self.wyy += o.wyy;
        self.wxy += o.wxy;
        self.area += o.area;
        self.peak = self.peak.max(o.peak);
    }
}

#[derive(Debug, Clone, Copy)]
struct Run {
    x0: u32,
    x1: u32,
}

#[derive(Debug, Clone)]
pub struct Labeller {
    max_runs: usize,
    runs: Vec<Run>,
    moments: Vec<Blob>,
    parent: Vec<u32>,
    /// Per run, its blob's index once folded.
    slot: Vec<u32>,
    blobs: Vec<Blob>,
}

impl Labeller {
    pub fn new(max_runs: u32) -> Labeller {
        let n = max_runs as usize;
        Labeller {
            max_runs: n,
            runs: Vec::with_capacity(n),
            moments: Vec::with_capacity(n),
            parent: Vec::with_capacity(n),
            slot: Vec::with_capacity(n),
            blobs: Vec::with_capacity(n),
        }
    }

    pub fn blobs(&self) -> &[Blob] {
        &self.blobs
    }

    /// Labels the lit pixels of `px`, rows of `width`, and folds them into
    /// [`blobs`](Self::blobs).
    pub fn label(&mut self, px: &[u16], width: usize, bg: &Background) -> Result<(), ImageError> {
        self.runs.clear();
        self.moments.clear();
        self.parent.clear();
        self.blobs.clear();
        let (block, bpr) = (bg.block(), bg.blocks_per_row());
        let mut prev = 0..0;
        for (y, row) in px.chunks_exact(width.max(1)).enumerate() {
            let start = self.runs.len();
            let brow = (y / block) * bpr;
            let mut x = 0;
            while x < width {
                // Skip dark pixels a block span at a time.
                let b = brow + x / block;
                let end = ((x / block + 1) * block).min(width);
                let t = bg.threshold(b);
                match row[x..end].iter().position(|p| *p >= t) {
                    None => {
                        x = end;
                        continue;
                    }
                    Some(i) => x += i,
                }
                // A run may cross into the next block; each pixel uses its
                // own block's threshold and median.
                let x0 = x;
                let mut run = Blob::default();
                let fy = y as f64;
                while x < width {
                    let b = brow + x / block;
                    let p = row[x];
                    if p < bg.threshold(b) {
                        break;
                    }
                    let w = f64::from(p.saturating_sub(bg.median(b)));
                    let fx = x as f64;
                    run.w += w;
                    run.wx += w * fx;
                    run.wy += w * fy;
                    run.wxx += w * fx * fx;
                    run.wyy += w * fy * fy;
                    run.wxy += w * fx * fy;
                    run.area += 1;
                    run.peak = run.peak.max(p);
                    x += 1;
                }
                self.push(x0, x - 1, run)?;
                self.union_above(self.runs.len() - 1, prev.clone());
            }
            prev = start..self.runs.len();
        }
        self.fold();
        Ok(())
    }

    fn push(&mut self, x0: usize, x1: usize, run: Blob) -> Result<(), ImageError> {
        if self.runs.len() >= self.max_runs {
            return Err(ImageError::TooManyRuns);
        }
        let id = self.runs.len() as u32;
        self.runs.push(Run {
            x0: x0 as u32,
            x1: x1 as u32,
        });
        self.moments.push(run);
        self.parent.push(id);
        Ok(())
    }

    /// Unions run `id` with every run in `above` within one pixel of it.
    fn union_above(&mut self, id: usize, above: core::ops::Range<usize>) {
        let Run { x0, x1 } = self.runs[id];
        for a in above {
            let r = self.runs[a];
            if r.x1 + 1 < x0 {
                continue;
            }
            if r.x0 > x1 + 1 {
                break;
            }
            union(&mut self.parent, a, id);
        }
    }

    fn fold(&mut self) {
        self.slot.clear();
        for i in 0..self.runs.len() {
            let root = find(&mut self.parent, i);
            if root == i {
                self.slot.push(self.blobs.len() as u32);
                self.blobs.push(self.moments[i]);
            } else {
                // `root < i`, so its slot is already set.
                let s = self.slot[root] as usize;
                self.slot.push(s as u32);
                let m = self.moments[i];
                self.blobs[s].add(&m);
            }
        }
    }
}

/// The root of `i`, halving the path. Terminates when `parent[k] ≤ k`.
pub fn find(parent: &mut [u32], mut i: usize) -> usize {
    while (parent[i] as usize) < i {
        let p = parent[i] as usize;
        parent[i] = parent[p];
        i = p;
    }
    i
}

/// Joins the sets of `a` and `b` under the smaller root.
pub fn union(parent: &mut [u32], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    let (lo, hi) = (ra.min(rb), ra.max(rb));
    parent[hi] = lo as u32;
}
