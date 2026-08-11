//! The host half of the spike: drive the guest ladder one poll per cycle
//! through `wasmi`, under a fuel budget, and compare against running the very
//! same ladder natively.
//!
//! This measures the two things the sequencing plan flags as unproven:
//!
//! 1. **Math cost.** The ladder runs real ADCS predicates (`angular_distance`,
//!    `norm`, a `target_for`-shaped look-at). The wasm-vs-native ratio on the
//!    same code is the number that decides whether Phase 2 has to push math
//!    into host functions.
//! 2. **Port marshalling.** Each cycle copies the mailbox into guest linear
//!    memory and reads it back — the sandbox equivalent of a slot's port copy.
//!
//! It also exercises the property that motivated the substrate choice: a guest
//! that will not stop is cut off by fuel exhaustion instead of stalling the
//! cycle.
//!
//! ## Method
//!
//! Getting this wrong is easy, so the harness is deliberate about it:
//!
//! - Telemetry is **precomputed** into a `Vec` before any clock starts, so no
//!   trig lands inside a timed loop.
//! - Marshalling is isolated by timing write+read **with no poll at all**,
//!   rather than by timing a poll with no marshalling — a poll against a stale
//!   mailbox short-circuits its predicates and measures nothing.
//! - The estimate is synthesised to converge onto the guest's own HIL target,
//!   so the ladder walks every phase to `Completed`. A run that ends `Pending`
//!   never exercised the deep path and its numbers are worthless, so the
//!   harness asserts the terminal state.
//! - Each measurement runs several reps and reports the **best**, to blunt
//!   scheduler noise.

use std::time::{Duration, Instant};

use wasm_poll_guest::{Mailbox, debug_tracking_error, hil_target, new_state, step};
use wasmi::{Config, Engine, Instance, Linker, Module, Store, TypedFunc};

/// Cycles per rep — 120 Hz for a simulated minute, the ADCS example's rate.
const CYCLES: usize = 7_200;
/// The simulated cycle step (µs) at 120 Hz.
const DT_US: i64 = 8_333;
/// Reps per measurement; the best is reported.
const REPS: usize = 7;

/// The guest, instantiated with its mailbox located.
struct Guest {
    store: Store<()>,
    poll: TypedFunc<i64, i32>,
    reset: TypedFunc<(), ()>,
    memory: wasmi::Memory,
    mailbox_at: usize,
    mailbox_len: usize,
}

impl Guest {
    /// Instantiate the module. When `fuel` is set the store meters execution —
    /// and the fuel has to be granted *before* instantiation, since running the
    /// module's start section is itself metered.
    fn load(wasm: &[u8], fuel: Option<u64>) -> anyhow::Result<Self> {
        let mut config = Config::default();
        config.consume_fuel(fuel.is_some());
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm)?;
        let mut store = Store::new(&engine, ());
        if let Some(fuel) = fuel {
            store.set_fuel(fuel)?;
        }
        let linker: Linker<()> = Linker::new(&engine);
        let instance: Instance = linker.instantiate_and_start(&mut store, &module)?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| anyhow::anyhow!("guest exports no memory"))?;
        let mailbox_at = instance
            .get_typed_func::<(), u32>(&store, "mailbox_ptr")?
            .call(&mut store, ())? as usize;
        let mailbox_len = instance
            .get_typed_func::<(), u32>(&store, "mailbox_len")?
            .call(&mut store, ())? as usize;
        let poll = instance.get_typed_func::<i64, i32>(&store, "poll")?;
        let reset = instance.get_typed_func::<(), ()>(&store, "reset")?;

        Ok(Self {
            store,
            poll,
            reset,
            memory,
            mailbox_at,
            mailbox_len,
        })
    }

    fn write(&mut self, mailbox: &Mailbox) {
        let bytes = unsafe {
            core::slice::from_raw_parts(mailbox as *const Mailbox as *const u8, self.mailbox_len)
        };
        self.memory.data_mut(&mut self.store)[self.mailbox_at..self.mailbox_at + self.mailbox_len]
            .copy_from_slice(bytes);
    }

    fn read(&mut self, mailbox: &mut Mailbox) {
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(mailbox as *mut Mailbox as *mut u8, self.mailbox_len)
        };
        bytes.copy_from_slice(
            &self.memory.data(&self.store)[self.mailbox_at..self.mailbox_at + self.mailbox_len],
        );
    }
}

/// One simulated minute of telemetry, built once and reused by every rep so no
/// trigonometry lands inside a timed loop.
///
/// The estimate converges onto the guest's own velocity-vector target, so the
/// ladder settles, skips detumble (the rate is inside wheel capture), passes
/// coarse pointing and confirms — exercising every predicate.
fn telemetry() -> Vec<Mailbox> {
    (0..CYCLES)
        .map(|c| {
            let t = c as f64 * (DT_US as f64 * 1e-6);
            let angle = 1e-3 * t;
            let vel_eci = [7.6e3 * angle.cos(), 0.0, 7.6e3 * angle.sin()];
            // Decays well inside the 1e-3 rad warm-up gate and the 0.2 rad
            // pointing gate within the first second.
            let wobble = 5e-3 * (-t * 4.0).exp();
            let [w, x, y, z] = hil_target(vel_eci);
            let n = (1.0 + wobble * wobble).sqrt();
            Mailbox {
                q_hat: [w / n, (x + wobble) / n, y / n, z / n],
                omega_b: [wobble, 1e-3, wobble * 0.25],
                pos_eci: [-6.871e6 * angle.sin(), 0.0, 6.871e6 * angle.cos()],
                vel_eci,
                have_estimate: 1,
                have_gps: 1,
                ..Mailbox::default()
            }
        })
        .collect()
}

/// Run `f` `REPS` times and return the fastest, as nanoseconds per cycle.
fn best(mut f: impl FnMut() -> anyhow::Result<Duration>) -> anyhow::Result<f64> {
    let mut best = Duration::MAX;
    for _ in 0..REPS {
        best = best.min(f()?);
    }
    Ok(best.as_secs_f64() * 1e9 / CYCLES as f64)
}

fn main() -> anyhow::Result<()> {
    let wasm_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/wasm32-unknown-unknown/release/wasm_poll_guest.wasm".into());
    let wasm = std::fs::read(&wasm_path)
        .map_err(|e| anyhow::anyhow!("reading {wasm_path}: {e} (run ./build.sh first)"))?;
    let feed = telemetry();
    // A feed that wanders into the look-at singularity yields NaN gates, which
    // read as "predicate false" and quietly turn the measurement into a timing
    // of the timeout path. Refuse to measure that.
    anyhow::ensure!(
        feed.iter()
            .all(|m| debug_tracking_error(m).is_some_and(|e| e.is_finite())),
        "synthetic telemetry produced a non-finite pointing gate; \
         the trajectory is too close to the point_minus_y_at singularity"
    );
    println!(
        "guest module: {} ({} bytes)\nmailbox: {} bytes/cycle each way\n",
        wasm_path,
        wasm.len(),
        core::mem::size_of::<Mailbox>()
    );

    // --- native baseline --------------------------------------------------
    let mut native_terminal = 0;
    let mut native_runs = 0u32;
    let native = best(|| {
        let mut state = new_state();
        let mut mailbox;
        let start = Instant::now();
        for (c, inputs) in feed.iter().enumerate() {
            mailbox = *inputs;
            let run_state = step(&mut mailbox, &mut state, c as i64 * DT_US);
            if run_state != 0 {
                native_terminal = run_state;
                native_runs += 1;
                state = new_state();
            }
        }
        Ok(start.elapsed())
    })?;

    // --- wasm: the full cycle, write + poll + read -------------------------
    let mut wasm_terminal = 0;
    let mut wasm_runs = 0u32;
    let full = best(|| {
        let mut guest = Guest::load(&wasm, None)?;
        let mut mailbox;
        let start = Instant::now();
        for (c, inputs) in feed.iter().enumerate() {
            mailbox = *inputs;
            guest.write(&mailbox);
            let run_state = guest.poll.call(&mut guest.store, c as i64 * DT_US)?;
            guest.read(&mut mailbox);
            if run_state != 0 {
                wasm_terminal = run_state;
                wasm_runs += 1;
                guest.reset.call(&mut guest.store, ())?;
            }
        }
        Ok(start.elapsed())
    })?;

    // --- wasm: marshalling alone, no poll ----------------------------------
    let marshal = best(|| {
        let mut guest = Guest::load(&wasm, None)?;
        let mut mailbox;
        let start = Instant::now();
        for inputs in feed.iter() {
            mailbox = *inputs;
            guest.write(&mailbox);
            guest.read(&mut mailbox);
        }
        Ok(start.elapsed())
    })?;

    let poll_cost = full - marshal;
    println!("per cycle, best of {REPS} reps over {CYCLES} cycles\n");
    println!("  native ladder             {native:>8.0} ns");
    println!("  wasm full cycle           {full:>8.0} ns");
    println!("    of which marshalling    {marshal:>8.0} ns");
    println!("    of which poll           {poll_cost:>8.0} ns");
    println!();
    println!(
        "  interpreter overhead      {:>8.1}x   (wasm poll / native)",
        poll_cost / native.max(f64::MIN_POSITIVE)
    );
    println!(
        "  marshalling share         {:>8.0}%   of a full wasm cycle",
        100.0 * marshal / full
    );
    println!(
        "  cost at 120 Hz            {:>8.3}%   of an 8.333 ms cycle",
        100.0 * full / (DT_US as f64 * 1e3)
    );

    println!(
        "\n  terminal run state: native {native_terminal}, wasm {wasm_terminal} \
         (ladder re-armed {} / {} times)",
        native_runs / REPS as u32,
        wasm_runs / REPS as u32
    );
    anyhow::ensure!(
        native_terminal == wasm_terminal,
        "native and wasm ladders disagreed ({native_terminal} vs {wasm_terminal})"
    );
    anyhow::ensure!(
        native_terminal == 1,
        "ladder did not reach Completed (run state {native_terminal}); \
         the measurement never exercised the full path"
    );

    // --- fuel: the property a native sequence cannot offer ------------------
    let mut metered = Guest::load(&wasm, Some(u64::MAX))?;
    let before = metered.store.get_fuel()?;
    metered.write(&feed[0]);
    metered.poll.call(&mut metered.store, 0)?;
    let burned = before - metered.store.get_fuel()?;

    let starved = Guest::load(&wasm, Some(burned / 2))
        .and_then(|mut g| Ok(g.poll.call(&mut g.store, 0)?))
        .is_err();

    println!("\n  fuel burned by one poll   {burned}");
    println!("  half that fuel traps      {starved}");
    anyhow::ensure!(starved, "fuel metering did not bound the poll");
    Ok(())
}
