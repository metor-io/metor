use metor_star::render::testkit::{STARCAT_G8, sim_camera, sim_renderer};
use metor_star::render::{Frame, Motion, Renderer};
use metor_star::starcat::StarCat;

use super::*;

fn params() -> StarTrackerParams {
    serde_json::from_str("{}").unwrap()
}

struct Rig {
    render: Renderer<'static>,
    px: Vec<u16>,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            render: sim_renderer(StarCat::parse(STARCAT_G8).unwrap()),
            px: vec![0; (SIZE * SIZE) as usize],
        }
    }

    /// Renders a still frame at `q_b_eci` and returns its truth stars' HIPs.
    fn expose(&mut self, q: Quat, id: u64) -> Vec<u32> {
        let motion = Motion {
            q_b_eci: q,
            omega_b: [0.0; 3],
            beta: [0.0; 3],
            t_exp: 0.1,
        };
        let frame = Frame::new(&sim_camera(), &motion, 16, id, id).unwrap();
        let truth = self.render.render(&frame, &mut self.px).unwrap();
        truth.brightest().iter().map(|t| t.cat_id).collect()
    }
}

fn q(i: f64) -> Quat {
    Quat([0.3 * i, -0.2, 0.5, 0.8]).normalized().unwrap()
}

#[test]
fn identifies_a_frame_and_tracks_the_next() {
    let mut rig = Rig::new();
    let mut st = StarTracker::new(params());
    let hips = rig.expose(q(1.0), 1);
    let out = st.step(Some((Timestamp(0), 1, &rig.px))).expect("resolves");
    assert_eq!(out.solution.status(), Ok(StarStatus::Valid));
    assert_eq!(out.solution.mode(), Ok(StarMode::Lost));
    let v = out.vectors.expect("vectors");
    assert!(v.count >= 6);
    // Every vector's id is a star the renderer drew.
    for id in &v.cat_id.0[..v.count as usize] {
        assert!(hips.contains(id), "HIP {id}");
    }
    let q_hat = geom::Quat(out.solution.q_b_eci.0.into_buf());
    assert!(q_hat.angle_to(q(1.0)) < 1e-3);
    // Centroids carry the same ids.
    let ids = &out.centroids.cat_id.0[..out.centroids.count as usize];
    assert!(ids.iter().filter(|i| **i != UNIDENTIFIED).count() >= v.count as usize);
    // The same frame again is not processed twice.
    assert!(st.step(Some((Timestamp(0), 1, &rig.px))).is_none());
    // The next frame, 0.1 s later and still, tracks.
    rig.expose(q(1.0), 2);
    let out = st.step(Some((Timestamp(100_000), 2, &rig.px))).unwrap();
    assert_eq!(out.solution.mode(), Ok(StarMode::Tracking));
}

/// A frame whose search needs more than one triplet: split one triplet per
/// cycle, it gives the same vectors as one call, and a new frame arriving
/// mid-search drops it.
#[test]
fn a_split_search_matches_one_call_and_a_new_frame_drops_it() {
    let mut rig = Rig::new();
    let one = StarTrackerParams {
        budget: 1,
        ..params()
    };
    let mut split_frames = 0;
    for i in 0..40 {
        rig.expose(q(f64::from(i)), 1);
        let whole = StarTracker::new(params())
            .step(Some((Timestamp(0), 1, &rig.px)))
            .unwrap();
        let mut split = StarTracker::new(one.clone());
        let mut out = split.step(Some((Timestamp(0), 1, &rig.px)));
        if out.is_some() {
            continue;
        }
        split_frames += 1;
        // A new frame now would drop this search.
        let mut dropper = StarTracker::new(one.clone());
        assert!(dropper.step(Some((Timestamp(0), 1, &rig.px))).is_none());
        dropper.step(Some((Timestamp(100_000), 2, &rig.px)));
        assert_eq!(dropper.dropped, 1);
        while out.is_none() {
            out = split.step(Some((Timestamp(0), 1, &rig.px)));
        }
        let (a, b) = (whole.vectors.unwrap(), out.unwrap().vectors.unwrap());
        assert_eq!(a.cat_id.0, b.cat_id.0);
        assert_eq!(a.b_body.into_buf(), b.b_body.into_buf());
    }
    assert!(split_frames > 0, "every frame solved on its first triplet");
}

#[test]
fn a_blank_frame_writes_a_solution_but_no_vectors() {
    let mut st = StarTracker::new(params());
    let px = vec![100u16; (SIZE * SIZE) as usize];
    let out = st.step(Some((Timestamp(0), 1, &px))).unwrap();
    assert!(out.vectors.is_none());
    assert_eq!(out.solution.status(), Ok(StarStatus::NoStars));
    assert_eq!(out.centroids.count, 0);
    // No frame and nothing pending: nothing to do.
    assert!(st.step(None).is_none());
}
