//! Systems exercising the fsw-3 pack ABI.

use std::cell::RefCell;

use metor_fsw_3::ring::Notifier;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{
    DefCx, DefError, DynInputs, Frame, Input, InputBinding, Output, OutputBinding, PortDef, Record,
    Seq, SeqError, Stop, System, SystemDef, SystemInputs, SystemOutputs, SystemTable, Timestamp,
    sequence, system,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The record every system here carries. The host names it `ping` too.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}

/// The frame this pack carries, so one can cross targets.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "sample")]
#[repr(C)]
pub struct Sample {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub value: f64,
}

/// Writes one sample per cycle, counting up.
#[derive(Default)]
pub struct Sampler(i64);

/// Counts up, one sample per cycle.
#[system]
fn sampler(#[state] s: &mut Sampler, sample: &mut Output<Sample>) {
    s.0 += 1;
    let _ = sample.write(&Sample {
        timestamp: Timestamp(s.0),
        value: s.0 as f64,
    });
}

/// Pings the newest sample's value.
#[system]
fn meter(sample: &mut Input<Sample>, output: &mut Output<Ping>) {
    if let Ok(Some(sample)) = sample.latest() {
        let n = sample.value as u32;
        let _ = output.write(&Ping { n });
    }
}

/// Copies the newest ping.
#[system]
fn echo(input: &mut Input<Ping>, output: &mut Output<Ping>) {
    if let Ok(Some(ping)) = input.latest() {
        let n = ping.n;
        let _ = output.write(&Ping { n });
    }
}

/// Copies every ping.
#[system]
async fn relay(input: &mut Input<Ping, Notifier>, output: &mut Output<Ping>, stop: Stop) {
    while !stop.is_set() {
        let ping = futures_lite::future::or(async { input.next().await.ok() }, async {
            stop.wait().await;
            None
        })
        .await;
        let Some(ping) = ping else { return };
        let _ = output.write(&Ping { n: ping.n });
    }
}

/// Adds up what every configured port carries.
#[system]
fn tap(taps: &mut DynInputs, output: &mut Output<Ping>) {
    let mut n = 0;
    for (_, input) in taps.iter_mut() {
        for record in input.drain() {
            let Ok(bytes) = record else { continue };
            let Ok(ping) = Ping::decode(bytes) else {
                continue;
            };
            n += ping.n;
        }
    }
    if n > 0 {
        let _ = output.write(&Ping { n });
    }
}

/// Panics from its second cycle on.
#[derive(Default)]
pub struct Boom(u32);

/// Fails on its second cycle.
#[system]
fn boom(#[state] s: &mut Boom, output: &mut Output<Ping>) {
    s.0 += 1;
    assert!(s.0 < 2, "boom on cycle {}", s.0);
    let _ = output.write(&Ping { n: s.0 });
}

/// `gain`'s params.
#[derive(Deserialize, JsonSchema)]
pub struct GainParams {
    /// Scales every ping.
    pub gain: f64,
}

/// Scales its input by a param.
pub struct Gain(f64);

impl Gain {
    fn new(params: GainParams) -> Self {
        Self(params.gain)
    }
}

#[system]
fn fail_input(input: &mut Input<Ping>) {
    let _ = input;
    // PANIC Safety: tests guest failure isolation.
    panic!("consumer failed");
}

thread_local! {
    static RETAINED_INPUT: RefCell<Option<Input<Ping>>> = const { RefCell::new(None) };
    static RETAINED_OUTPUT: RefCell<Option<Output<Ping>>> = const { RefCell::new(None) };
}

pub struct RetainedInput;

impl SystemInputs for RetainedInput {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(vec![Input::<Ping>::def(cx, "input")])
    }

    fn bind(mut inputs: Vec<InputBinding>) -> Self {
        if let Some(binding) = inputs.pop() {
            RETAINED_INPUT.with(|slot| *slot.borrow_mut() = Some(Input::new(binding.views)));
        }
        Self
    }
}

pub struct RetainedOutput;

impl SystemOutputs for RetainedOutput {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(vec![Output::<Ping>::def(cx, "output")])
    }

    fn bind(mut outputs: Vec<OutputBinding>) -> Self {
        if let Some(binding) = outputs.pop() {
            RETAINED_OUTPUT.with(|slot| *slot.borrow_mut() = Some(Output::new(binding.writer)));
        }
        Self
    }
}

pub struct Retained;

impl System for Retained {
    type State = ();
    type Inputs = RetainedInput;
    type Outputs = RetainedOutput;

    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        SystemDef::new::<RetainedInput, RetainedOutput, _>("Retained", cx)
    }

    fn execute(&self, _: Timestamp, _: &mut (), _: &mut RetainedInput, _: &mut RetainedOutput) {}
}

fn release_retained() -> Option<u32> {
    let mut input = RETAINED_INPUT.with(|slot| slot.borrow_mut().take())?;
    let mut output = RETAINED_OUTPUT.with(|slot| slot.borrow_mut().take())?;
    let n = input.latest().ok()??.n;
    output.write(&Ping { n }).ok()?;
    Some(n)
}

#[unsafe(no_mangle)]
pub extern "C" fn echo_pack_release_retained() -> u32 {
    release_retained().unwrap_or(0)
}

/// Scales the newest ping.
#[system]
fn gain(#[state] s: &mut Gain, input: &mut Input<Ping>, output: &mut Output<Ping>) {
    if let Ok(Some(ping)) = input.latest() {
        let n = (f64::from(ping.n) * s.0) as u32;
        let _ = output.write(&Ping { n });
    }
}

/// Pings one, two, and three on successive cycles, then completes.
#[sequence]
async fn count_up(
    seq: &Seq,
    input: &mut Input<Ping>,
    output: &mut Output<Ping>,
) -> Result<(), SeqError> {
    let _ = input;
    for n in 1..=3 {
        let _ = output.write(&Ping { n });
        seq.next_cycle().await;
    }
    Ok(())
}

/// Pings zero, then waits until aborted.
#[sequence]
async fn count_down(
    seq: &Seq,
    input: &mut Input<Ping>,
    output: &mut Output<Ping>,
) -> Result<(), SeqError> {
    let _ = input;
    let _ = output.write(&Ping { n: 0 });
    seq.sleep(core::time::Duration::from_secs(3600)).await
}

/// `count_by`'s params.
#[derive(Deserialize, JsonSchema)]
pub struct StrideParams {
    /// The step between pings; zero is refused.
    pub by: u32,
}

/// Counts in strides of `by`.
pub struct Stride(u32);

impl Stride {
    fn new(params: StrideParams) -> Result<Self, metor_fsw_3::ParamError> {
        if params.by == 0 {
            return Err(metor_fsw_3::ParamError::Decode("`by` is zero".into()));
        }
        Ok(Self(params.by))
    }
}

/// Pings three strides on successive cycles, then completes; `count_up`'s
/// ports, with a constructor that can refuse.
#[sequence]
async fn count_by(
    #[state] s: &mut Stride,
    seq: &Seq,
    input: &mut Input<Ping>,
    output: &mut Output<Ping>,
) -> Result<(), SeqError> {
    let _ = input;
    for n in 1..=3 {
        let _ = output.write(&Ping { n: n * s.0 });
        seq.next_cycle().await;
    }
    Ok(())
}

/// The table this pack exports.
pub fn pack() -> SystemTable {
    if std::env::var_os("METOR_TEST_PACK_BUILDER_PANIC").is_some() {
        // PANIC Safety: exercises containment at the exported C boundary.
        panic!("pack builder failed");
    }
    let mut table = SystemTable::new();
    // PANIC Safety: these static record schemas are compatible.
    table
        .register("sampler", sampler(), Sampler::default)
        .expect("valid records");
    table
        .register("meter", meter(), Default::default)
        .expect("valid records");
    table
        .register("echo", echo(), Default::default)
        .expect("valid records");
    table
        .register_async("relay", relay(), Default::default)
        .expect("valid records");
    table
        .register("tap", tap(), Default::default)
        .expect("valid records");
    table
        .register("boom", boom(), Boom::default)
        .expect("valid records");
    table
        .register("gain", gain(), Gain::new)
        .expect("valid records");
    table
        .register("fail_input", fail_input(), Default::default)
        .expect("valid records");
    table
        .register_system("retained", |_| Ok((Retained, ())))
        .expect("valid records");
    table
        .register_sequence("count_up", count_up(), Default::default)
        .expect("valid records");
    table
        .register_sequence("count_down", count_down(), Default::default)
        .expect("valid records");
    table
        .register_sequence("count_by", count_by(), Stride::new)
        .expect("valid records");
    table
}

metor_fsw_3::export_pack!(pack);
