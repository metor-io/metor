//! A test pack whose systems exercise the fsw-3 pack ABI.

mod chatter;
mod gps;

use std::cell::RefCell;

use metor_fsw_3::ring::Notifier;
use metor_fsw_3::zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{
    BindCx, DefCx, DefError, DynInputs, Frame, FrameBytes, Input, InputBinding, MakeError, Output,
    OutputBinding, Payload, PortDef, Ports, Record, Seq, SeqError, Stop, System, SystemDef,
    SystemTable, Timestamp, sequence, system,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The record most systems here read and write. The integration tests
/// declare the same record on the host side.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}

/// A frame type, so tests can send a frame from one target to another.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "sample")]
#[repr(C)]
pub struct Sample {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub value: f64,
}

/// The pixel bytes of an [`Image`].
pub const IMAGE_LEN: usize = 2 << 20;

/// A 2 MiB frame, so tests can move images through a guest.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "image")]
#[repr(C)]
pub struct Image {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub pixels: FrameBytes<IMAGE_LEN>,
}

/// Pings the first and last pixel bytes of the newest image, as
/// `first << 8 | last`.
#[system]
fn viewer(image: &mut Input<Image>, output: &mut Output<Ping>) {
    if let Ok(Some(image)) = image.latest() {
        let (first, last) = (image.pixels.0[0], image.pixels.0[IMAGE_LEN - 1]);
        let n = u32::from(first) << 8 | u32::from(last);
        let _ = output.write(&Ping { n });
    }
}

/// An image on the heap, rewritten each cycle.
pub struct Camera(Box<Image>);

impl Camera {
    fn new(_io: &mut metor_fsw_3::io::IoInit) -> Result<Self, MakeError> {
        let image = Image::new_box_zeroed()
            .map_err(|_| MakeError::Pack("could not allocate an image".into()))?;
        Ok(Self(image))
    }
}

/// Writes one image per cycle. Its first pixel byte is the low byte of the
/// cycle's time, and its last is that byte inverted.
#[system]
fn camera(#[state] s: &mut Camera, now: Timestamp, image: &mut Output<Image>) {
    s.0.timestamp = now;
    s.0.pixels.0[0] = now.0 as u8;
    s.0.pixels.0[IMAGE_LEN - 1] = !(now.0 as u8);
    let _ = image.write(&s.0);
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

/// Replaces each ping's count with a draw from the target seed.
#[system]
fn dice(rng: &mut metor_fsw_3::Rng, input: &mut Input<Ping>, output: &mut Output<Ping>) {
    for ping in input.drain() {
        if ping.is_ok() {
            let _ = output.write(&Ping {
                n: rng.next_u64() as u32,
            });
        }
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

/// Never returns: loops until `now` is before the epoch, which it never is.
#[system]
fn spin(now: Timestamp, input: &mut Input<Ping>, output: &mut Output<Ping>) {
    let _ = (input, output);
    while now >= Timestamp(0) {
        core::hint::spin_loop();
    }
}

/// Logs each cycle's time, and pings it.
#[system]
fn tick(now: Timestamp, output: &mut Output<Ping>) {
    metor_fsw_3::tracing::info!(now = now.0, "tick");
    let _ = output.write(&Ping { n: now.0 as u32 });
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

/// The params for `gain`.
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
    // PANIC Safety: this tests that a failing guest system is isolated.
    panic!("consumer failed");
}

thread_local! {
    static RETAINED_INPUT: RefCell<Option<Input<Ping>>> = const { RefCell::new(None) };
    static RETAINED_OUTPUT: RefCell<Option<Output<Ping>>> = const { RefCell::new(None) };
}

pub struct RetainedInput;

impl Ports<InputBinding> for RetainedInput {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(vec![Input::<Ping>::def(cx, "input")])
    }

    fn bind(mut inputs: Vec<InputBinding>, _cx: &BindCx) -> Result<Self, MakeError> {
        if let Some(binding) = inputs.pop() {
            RETAINED_INPUT.with(|slot| *slot.borrow_mut() = Some(Input::new(binding.views)));
        }
        Ok(Self)
    }
}

pub struct RetainedOutput;

impl Ports<OutputBinding> for RetainedOutput {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(vec![Output::<Ping>::def(cx, "output")])
    }

    fn bind(mut outputs: Vec<OutputBinding>, _cx: &BindCx) -> Result<Self, MakeError> {
        if let Some(binding) = outputs.pop() {
            RETAINED_OUTPUT.with(|slot| *slot.borrow_mut() = Some(Output::new(binding.writer)));
        }
        Ok(Self)
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

/// The params for `count_by`.
#[derive(Deserialize, JsonSchema)]
pub struct StrideParams {
    /// The step between pings. Zero is rejected.
    pub by: u32,
}

/// Counts in strides of `by`.
pub struct Stride(u32);

impl Stride {
    fn new(params: StrideParams) -> Result<Self, MakeError> {
        if params.by == 0 {
            return Err(metor_fsw_3::ParamError::Decode("`by` is zero".into()).into());
        }
        Ok(Self(params.by))
    }
}

/// Pings three multiples of `by` on successive cycles, then completes.
/// It has `count_up`'s ports, but its constructor can fail.
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
        // PANIC Safety: this tests that the exported C boundary contains the
        // panic.
        panic!("pack builder failed");
    }
    let mut table = SystemTable::new();
    // PANIC Safety: every system here uses compatible record schemas, so
    // registration cannot fail.
    table
        .register("sampler", sampler(), Sampler::default)
        .expect("valid records");
    table
        .register("meter", meter(), Default::default)
        .expect("valid records");
    table
        .register("viewer", viewer(), Default::default)
        .expect("valid records");
    table
        .register("camera", camera(), Camera::new)
        .expect("valid records");
    table
        .register("echo", echo(), Default::default)
        .expect("valid records");
    table
        .register("dice", dice(), Default::default)
        .expect("valid records");
    table
        .register_async("relay", relay(), Default::default)
        .expect("valid records");
    table
        .register("tap", tap(), Default::default)
        .expect("valid records");
    table
        .register("spin", spin(), Default::default)
        .expect("valid records");
    table
        .register("tick", tick(), Default::default)
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
        .register("gps", gps::gps(), gps::GpsState::new)
        .expect("valid records");
    table
        .register("uart_script", gps::uart_script(), gps::UartScript::new)
        .expect("valid records");
    table
        .register("chatter", chatter::chatter(), chatter::Chatter::new)
        .expect("valid records");
    table
        .register("reader", chatter::reader(), chatter::Reader::new)
        .expect("valid records");
    table
        .register("grower", chatter::grower(), chatter::Grower::new)
        .expect("valid records");
    table
        .register_sequence("until_fix", gps::until_fix(), gps::UntilFix::new)
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
