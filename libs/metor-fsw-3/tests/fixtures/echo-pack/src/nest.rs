//! Producers at different rates, and a system that checks each lands on its
//! own element of an array or a struct of inputs.

use metor_fsw_3::{Input, Output, SystemParams, system};
use schemars::JsonSchema;
use serde::Deserialize;

use super::Ping;

/// The params for `tag`.
#[derive(Deserialize, JsonSchema)]
pub struct TagParams {
    /// The count every ping carries.
    pub tag: u32,
    /// Cycles between pings.
    pub every: u32,
}

/// Pings one count at one rate.
pub struct Tag {
    params: TagParams,
    cycle: u32,
}

impl Tag {
    pub fn new(params: TagParams) -> Self {
        Self { params, cycle: 0 }
    }
}

/// Pings its tag every `every` cycles.
#[system]
pub fn tag(#[state] s: &mut Tag, output: &mut Output<Ping>) {
    s.cycle += 1;
    if s.cycle.is_multiple_of(s.params.every.max(1)) {
        let _ = output.write(&Ping { n: s.params.tag });
    }
}

/// Two inputs of one wheel.
#[derive(SystemParams)]
pub struct Pair<'a> {
    left: &'a mut Input<Ping>,
    right: &'a mut Input<Ping>,
}

/// Cycles after which every input must have heard a ping.
const HEARD_BY: u32 = 20;

/// The cycles run, and the pings each input heard.
#[derive(Default)]
pub struct Gather {
    cycle: u32,
    heard: [u32; 7],
}

/// Checks that `tags.i` hears only tag `i`, and `pairs.i.left` and
/// `pairs.i.right` only tags `10 * (i + 1)` and one more. Fails if an input
/// hears another tag, or nothing by cycle 20.
#[system]
pub fn gather(#[state] s: &mut Gather, tags: [&mut Input<Ping>; 3], pairs: [Pair<'_>; 2]) {
    s.cycle += 1;
    let pairs = pairs.into_iter().enumerate().flat_map(|(i, pair)| {
        let tag = 10 * (i as u32 + 1);
        [(pair.left, tag), (pair.right, tag + 1)]
    });
    let tags = tags.into_iter().zip(0..);
    for ((input, want), heard) in tags.chain(pairs).zip(&mut s.heard) {
        *heard += hear(input, want);
    }
    // PANIC Safety: the fixture latches off to report a miswired input.
    assert!(
        s.cycle < HEARD_BY || s.heard.iter().all(|n| *n > 0),
        "unheard inputs: {:?}",
        s.heard
    );
}

/// Counts the pings `input` holds, panicking on one that is not `want`.
fn hear(input: &mut Input<Ping>, want: u32) -> u32 {
    let mut heard = 0;
    for ping in input.drain().flatten() {
        // PANIC Safety: the fixture latches off to report a miswired input.
        assert_eq!(ping.n, want, "an input heard another tag");
        heard += 1;
    }
    heard
}
