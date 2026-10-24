//! The `star_sim` async system.

use adcs_contracts::{BodyState, StarImage, StarTruth, World};
use futures_lite::future;
use metor_fsw_3::ring::Notifier;
use metor_fsw_3::{Input, Log, Output, Stop, system};

use crate::sim::StarSim;

#[system]
/// Renders a star-tracker frame from the true attitude every `1 / rate_hz`
/// of sim time, and writes the image and its truth stamped at mid-exposure.
/// Each frame is rendered from the body state at its mid-exposure, so the
/// image shows the attitude its stamp names. It reads `World` for the
/// ephemeris that aberration and bright-body blinding will use.
pub async fn star_sim(
    #[state] state: &mut StarSim,
    body: &mut Input<BodyState, Notifier>,
    world: &mut Input<World, Notifier>,
    star_image: &mut Output<StarImage>,
    star_truth: &mut Output<StarTruth>,
    log: &mut Log,
    stop: Stop,
) {
    while let Some(body) = next(body, &stop).await {
        let _ = world.latest();
        match state.sample(&body) {
            Ok(false) => continue,
            Ok(true) => {
                let _ = star_image.write(state.image());
                let _ = star_truth.write(state.truth());
            }
            Err(error) => {
                log.fault("render", format!("{error:?}"));
                return;
            }
        }
    }
}

/// The next body state, or `None` once `stop` is set.
async fn next(body: &mut Input<BodyState, Notifier>, stop: &Stop) -> Option<BodyState> {
    let read = async { body.next().await.ok().copied() };
    let stopped = async {
        stop.wait().await;
        None
    };
    future::or(read, stopped).await
}
