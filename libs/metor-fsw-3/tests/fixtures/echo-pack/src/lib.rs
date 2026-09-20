//! Systems exercising the fsw-3 pack ABI.

use std::cell::RefCell;

use metor_fsw_3::ring::Notifier;
use metor_fsw_3::{
    DefCx, DefError, DynInputs, Input, InputBinding, Output, OutputBinding, PortDef, Record, Stop,
    System, SystemDef, SystemInputs, SystemOutputs, SystemTable, Timestamp, system,
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

/// Copies its input onto its output.
#[derive(Default)]
pub struct Echo;

#[system]
impl Echo {
    /// Copies the newest ping.
    fn execute(&mut self, input: &mut Input<Ping>, output: &mut Output<Ping>) {
        if let Ok(Some(ping)) = input.latest() {
            let n = ping.n;
            let _ = output.write(&Ping { n });
        }
    }
}

/// Copies every ping as it arrives, on a thread inside the pack.
#[derive(Default)]
pub struct Relay;

#[system]
impl Relay {
    /// Copies every ping.
    async fn run(
        &mut self,
        input: &mut Input<Ping, Notifier>,
        output: &mut Output<Ping>,
        stop: Stop,
    ) {
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
}

/// Sums the pings on every port its config gave it.
#[derive(Default)]
pub struct Tap;

#[system]
impl Tap {
    /// Adds up what every configured port carries.
    fn execute(&mut self, taps: &mut DynInputs, output: &mut Output<Ping>) {
        let mut n = 0;
        for (_, input) in taps.iter_mut() {
            for record in input.drain() {
                let Ok(bytes) = record else { continue };
                let Ok(ping) = Ping::decode(&bytes) else {
                    continue;
                };
                n += ping.n;
            }
        }
        if n > 0 {
            let _ = output.write(&Ping { n });
        }
    }
}

/// Panics from its second cycle on.
#[derive(Default)]
pub struct Boom(u32);

#[system]
impl Boom {
    /// Fails on its second cycle.
    fn execute(&mut self, output: &mut Output<Ping>) {
        self.0 += 1;
        assert!(self.0 < 2, "boom on cycle {}", self.0);
        let _ = output.write(&Ping { n: self.0 });
    }
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

#[derive(Default)]
pub struct FailInput;

#[system]
impl FailInput {
    fn execute(&mut self, input: &mut Input<Ping>) {
        let _ = input;
        // PANIC Safety: tests guest failure isolation.
        panic!("consumer failed");
    }
}

thread_local! {
    static RETAINED_INPUT: RefCell<Option<Input<Ping>>> = const { RefCell::new(None) };
    static RETAINED_OUTPUT: RefCell<Option<Output<Ping>>> = const { RefCell::new(None) };
}

pub struct RetainedInput;

impl SystemInputs for RetainedInput {
    fn defs(_cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(vec![Input::<Ping>::def("input")])
    }

    fn bind(mut inputs: Vec<InputBinding>) -> Self {
        if let Some(binding) = inputs.pop() {
            RETAINED_INPUT.with(|slot| *slot.borrow_mut() = Input::try_new(binding.views).ok());
        }
        Self
    }
}

pub struct RetainedOutput;

impl SystemOutputs for RetainedOutput {
    fn defs(_cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(vec![Output::<Ping>::def("output")])
    }

    fn bind(mut outputs: Vec<OutputBinding>) -> Self {
        if let Some(binding) = outputs.pop() {
            RETAINED_OUTPUT.with(|slot| *slot.borrow_mut() = Output::try_new(binding.writer).ok());
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
        SystemDef::new::<RetainedInput, RetainedOutput>("Retained", cx)
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

#[system]
impl Gain {
    /// Scales the newest ping.
    fn execute(&mut self, input: &mut Input<Ping>, output: &mut Output<Ping>) {
        if let Ok(Some(ping)) = input.latest() {
            let n = (f64::from(ping.n) * self.0) as u32;
            let _ = output.write(&Ping { n });
        }
    }
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
        .register("echo", Echo::default)
        .expect("valid records");
    table
        .register_async("relay", Relay::default)
        .expect("valid records");
    table.register("tap", Tap::default).expect("valid records");
    table
        .register("boom", Boom::default)
        .expect("valid records");
    table.register("gain", Gain::new).expect("valid records");
    table
        .register("fail_input", FailInput::default)
        .expect("valid records");
    table
        .register_system("retained", |_| Ok((Retained, ())))
        .expect("valid records");
    table
}

metor_fsw_3::export_pack!(pack);
