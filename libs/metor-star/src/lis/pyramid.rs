//! The Pyramid search: for each triplet in Mortari's order, join the catalog
//! pairs of edges `ij` and `ik` on their shared star, check `jk` and the
//! handedness, and verify a unique triangle with a unique fourth star.

use alloc::vec;
use alloc::vec::Vec;

use super::verify::verify;
use super::{LisConfig, MAX_OBS, Match, NO_ROW};
use crate::catalog::Db;
use crate::catalog::build::separation;
use crate::geom::{V3, cross, dot};
use crate::wahba::{Obs, quest};

/// What one [`Pyramid::step`] ended with.
// Boxing `Match` would allocate per frame; it is returned by value.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Found(Match),
    /// Every triplet, or `max_triplets` of them, tried without a match.
    Exhausted,
    /// The budget ran out; call again to resume.
    More,
}

/// The search cursor: triplet `(i, i + dj, i + dj + dk)`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cursor {
    dj: usize,
    dk: usize,
    i: usize,
}

#[derive(Debug, Clone)]
pub struct Pyramid {
    cfg: LisConfig,
    obs: Vec<V3>,
    cursor: Option<Cursor>,
    triplets: u32,
    /// Per star: the epoch its `head` was written in.
    stamp: Vec<u32>,
    head: Vec<u32>,
    epoch: u32,
    /// The adjacency's linked lists: partner row and next entry.
    partner: Vec<u16>,
    next: Vec<u32>,
    cone: Vec<u16>,
    quest_obs: Vec<Obs>,
}

const END: u32 = u32::MAX;

impl Pyramid {
    /// Sized for a DB of `n_stars`; allocates nothing afterwards.
    pub fn new(n_stars: usize, cfg: LisConfig) -> Pyramid {
        Pyramid {
            obs: Vec::with_capacity(MAX_OBS),
            cursor: None,
            triplets: 0,
            stamp: vec![0; n_stars],
            head: vec![END; n_stars],
            epoch: 0,
            partner: Vec::with_capacity(2 * cfg.max_candidates),
            next: Vec::with_capacity(2 * cfg.max_candidates),
            cone: Vec::with_capacity(n_stars),
            quest_obs: Vec::with_capacity(MAX_OBS),
            cfg,
        }
    }

    pub fn config(&self) -> &LisConfig {
        &self.cfg
    }

    /// Starts a frame: `obs` are star-tracker-frame unit vectors, brightest
    /// first; at most [`MAX_OBS`] are kept.
    pub fn start(&mut self, obs: &[V3]) {
        self.obs.clear();
        self.obs.extend_from_slice(&obs[..obs.len().min(MAX_OBS)]);
        self.triplets = 0;
        self.cursor = (self.search_n() >= 3).then_some(Cursor { dj: 1, dk: 1, i: 0 });
    }

    /// Triplets tried in this frame so far.
    pub fn triplets(&self) -> u32 {
        self.triplets
    }

    fn search_n(&self) -> usize {
        self.obs.len().min(self.cfg.search_obs)
    }

    /// Tries up to `budget` more triplets.
    pub fn step(&mut self, db: &Db, budget: u32) -> Step {
        if self.stamp.len() < db.n_stars() {
            return Step::Exhausted;
        }
        let n = self.search_n();
        for _ in 0..budget {
            let Some(c) = self.cursor else {
                return Step::Exhausted;
            };
            if self.triplets >= self.cfg.max_triplets {
                self.cursor = None;
                return Step::Exhausted;
            }
            self.cursor = advance(c, n);
            self.triplets += 1;
            let (i, j, k) = (c.i, c.i + c.dj, c.i + c.dj + c.dk);
            if let Some(m) = self.try_triplet(db, i, j, k) {
                self.cursor = None;
                return Step::Found(m);
            }
        }
        if self.cursor.is_some() {
            Step::More
        } else {
            Step::Exhausted
        }
    }

    /// The catalog rows of the unique triangle matching `(i, j, k)`, with a
    /// unique fourth star, verified.
    fn try_triplet(&mut self, db: &Db, i: usize, j: usize, k: usize) -> Option<Match> {
        let (bi, bj, bk) = (self.obs[i], self.obs[j], self.obs[k]);
        let eps = self.cfg.eps_rad as f32;
        let edge = |a: V3, b: V3| separation(a, b) as f32;
        let (tij, tik, tjk) = (edge(bi, bj), edge(bi, bk), edge(bj, bk));
        let rij = db.pair_range(tij - eps, tij + eps);
        let rik = db.pair_range(tik - eps, tik + eps);
        if rij.len() > self.cfg.max_candidates || rik.len() > self.cfg.max_candidates {
            return None;
        }
        self.index_edge(db, rik);
        let (lo, hi) = cos_band(f64::from(tjk), self.cfg.eps_rad);
        let hand = dot(bi, cross(bj, bk)) > 0.0;
        let mut found = None;
        for p in &db.pairs()[rij] {
            for (a, b) in [(p.a, p.b), (p.b, p.a)] {
                let (ra, rb) = (vec_of(db, a), vec_of(db, b));
                let mut e = self.head_of(a);
                while e != END {
                    let c = self.partner[e as usize];
                    e = self.next[e as usize];
                    if c == b {
                        continue;
                    }
                    let rc = vec_of(db, c);
                    let cos_jk = dot(rb, rc);
                    if cos_jk < lo || cos_jk > hi || (dot(ra, cross(rb, rc)) > 0.0) != hand {
                        continue;
                    }
                    if found.is_some() {
                        return None; // Ambiguous.
                    }
                    found = Some([a, b, c]);
                }
            }
        }
        let tri = found?;
        let (r, star_r) = self.fourth(db, [i, j, k], tri)?;
        self.solve(db, [i, j, k, r], [tri[0], tri[1], tri[2], star_r])
    }

    /// The first observation outside the triangle with exactly one catalog
    /// star matching all three edges to the triangle.
    fn fourth(&self, db: &Db, obs: [usize; 3], tri: [u16; 3]) -> Option<(usize, u16)> {
        let eps = self.cfg.eps_rad;
        let n = self.search_n();
        let rt = tri.map(|row| vec_of(db, row));
        for r in (0..n).filter(|r| !obs.contains(r)) {
            let br = self.obs[r];
            let t: [f64; 3] = obs.map(|o| separation(self.obs[o], br));
            let ti = t[0] as f32;
            let range = db.pair_range(ti - eps as f32, ti + eps as f32);
            if range.len() > self.cfg.max_candidates {
                continue;
            }
            let bands = [cos_band(t[1], eps), cos_band(t[2], eps)];
            let mut hit = None;
            let mut unique = true;
            for p in &db.pairs()[range] {
                let other = match (p.a, p.b) {
                    (a, b) if a == tri[0] => b,
                    (a, b) if b == tri[0] => a,
                    _ => continue,
                };
                if tri.contains(&other) {
                    continue;
                }
                let ro = vec_of(db, other);
                let ok = (0..2).all(|m| {
                    let c = dot(rt[m + 1], ro);
                    c >= bands[m].0 && c <= bands[m].1
                });
                if ok {
                    unique &= hit.is_none();
                    hit = Some(other);
                }
            }
            if let (Some(row), true) = (hit, unique) {
                return Some((r, row));
            }
        }
        None
    }

    /// QUEST on the pyramid, verify, QUEST on every match, verify again.
    fn solve(&mut self, db: &Db, obs: [usize; 4], rows: [u16; 4]) -> Option<Match> {
        let sigma = self.cfg.sigma_rad;
        self.quest_obs.clear();
        for (o, row) in obs.iter().zip(rows) {
            self.quest_obs.push(Obs {
                b: self.obs[*o],
                r: vec_of(db, row),
                sigma,
            });
        }
        let first = quest(&self.quest_obs, None).ok()?;
        let v = verify(db, first.q, &self.obs, &self.cfg, &mut self.cone);
        if !v.accepted {
            return None;
        }
        self.quest_obs.clear();
        for (b, row) in self.obs.iter().zip(v.rows).filter(|(_, r)| *r != NO_ROW) {
            self.quest_obs.push(Obs {
                b: *b,
                r: vec_of(db, row),
                sigma,
            });
        }
        let refined = quest(&self.quest_obs, first.rotated).ok()?;
        let v = verify(db, refined.q, &self.obs, &self.cfg, &mut self.cone);
        v.accepted.then_some(Match {
            rows: v.rows,
            n_obs: self.obs.len(),
            n_match: v.n_match,
            q: refined.q,
            p_theta: refined.p_theta,
            rms_rad: v.rms_rad,
            p_false: v.p_false,
            triplets: self.triplets,
        })
    }

    /// Builds the adjacency of the pairs in `range`, both ways.
    fn index_edge(&mut self, db: &Db, range: core::ops::Range<usize>) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.fill(0);
            self.epoch = 1;
        }
        self.partner.clear();
        self.next.clear();
        for p in &db.pairs()[range] {
            for (a, b) in [(p.a, p.b), (p.b, p.a)] {
                let head = self.head_of(a);
                self.partner.push(b);
                self.next.push(head);
                let s = usize::from(a);
                self.head[s] = (self.partner.len() - 1) as u32;
                self.stamp[s] = self.epoch;
            }
        }
    }

    fn head_of(&self, row: u16) -> u32 {
        let s = usize::from(row);
        match self.stamp.get(s) {
            Some(e) if *e == self.epoch => self.head[s],
            _ => END,
        }
    }
}

fn vec_of(db: &Db, row: u16) -> V3 {
    db.star(row).map_or([0.0; 3], |s| s.unit())
}

/// `[cos(θ + ε), cos(θ − ε)]`.
fn cos_band(theta: f64, eps: f64) -> (f64, f64) {
    (libm::cos(theta + eps), libm::cos((theta - eps).max(0.0)))
}

/// The triplet after `c` among `n` observations, in Mortari's order.
fn advance(c: Cursor, n: usize) -> Option<Cursor> {
    let Cursor { dj, dk, i } = c;
    if i + dj + dk + 1 < n {
        return Some(Cursor { dj, dk, i: i + 1 });
    }
    if dj + dk + 1 < n {
        return Some(Cursor {
            dj,
            dk: dk + 1,
            i: 0,
        });
    }
    if dj + 2 < n {
        return Some(Cursor {
            dj: dj + 1,
            dk: 1,
            i: 0,
        });
    }
    None
}
