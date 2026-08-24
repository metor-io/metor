//! Q3: `window` and `fft`.
//!
//! Both exist for parity with node kinds the panel is about to lose, so both
//! are pinned against what those nodes *published* rather than against a
//! notion of correctness: a window is `N` samples newest-last, a spectrum is
//! `N / 2 + 1` magnitudes along the last axis. A saved plot reads the new
//! output unchanged or the migration was not a migration.
//!
//! **The one place this crate compares with a tolerance.** Everywhere else a
//! compiled result must equal its oracle bit for bit, because the oracle
//! computes the same operations in the same order. `rustfft` does not: it
//! plans mixed radices and vectorises, so it and a textbook radix-2 disagree
//! in the last few places by construction. The tolerance below is therefore a
//! statement about *two algorithms*, not a loosened definition — and it is
//! scaled by the transform's own magnitude, since absolute error grows with
//! the signal. The measured worst case over the whole matrix is 3.4e-16,
//! about one and a half ULP, so the 1e-12 bound has four orders of headroom
//! and would still catch a wrong twiddle, a missed stage, or a bit-reversal
//! off by one.

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use super::systems::{Table, build as build_system, imu_table, refuse};
use super::tensors::{bits, evaluate};
use super::reject;

/// What the panel's Fft node published: `|X[k]|` for `k` in `0..=n/2`.
fn oracle(signal: &[f64]) -> Vec<f64> {
    let mut buf: Vec<Complex<f64>> = signal.iter().map(|v| Complex::new(*v, 0.0)).collect();
    FftPlanner::<f64>::new()
        .plan_fft_forward(signal.len())
        .process(&mut buf);
    buf.iter().take(signal.len() / 2 + 1).map(|c| c.norm()).collect()
}

/// The four signal shapes the plan asks for, at one length.
fn signals(n: usize) -> Vec<(&'static str, Vec<f64>)> {
    let impulse = (0..n).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
    let dc = vec![1.0; n];
    let sine = (0..n)
        .map(|i| (std::f64::consts::TAU * 4.0 * i as f64 / n as f64).sin())
        .collect();
    // A fixed pseudo-random sequence: reproducible, and nothing like the
    // other three.
    let mut state = 0x243F_6A88_85A3_08D3u64;
    let noise = (0..n)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        })
        .collect();
    vec![
        ("impulse", impulse),
        ("dc", dc),
        ("sine", sine),
        ("noise", noise),
    ]
}

#[test]
fn the_spectrum_agrees_with_rustfft() {
    let mut worst = 0.0f64;
    for n in [8usize, 16, 32, 64, 128, 256, 512, 1024] {
        let source = format!(
            "def spectrum(x: Tensor[f64, {n}]) -> Tensor[f64, {}]:\n    return fft(x)\n",
            n / 2 + 1
        );
        for (name, signal) in signals(n) {
            let got = evaluate(&source, "spectrum", &[&signal], n / 2 + 1);
            let want = oracle(&signal);
            assert_eq!(got.len(), want.len(), "{name} at {n}");

            let scale = want.iter().fold(1.0f64, |acc, v| acc.max(*v));
            for (k, (a, b)) in got.iter().zip(&want).enumerate() {
                let error = (a - b).abs() / scale;
                worst = worst.max(error);
                assert!(
                    error < 1e-12,
                    "{name} at n={n}, bin {k}: {a} vs rustfft's {b}"
                );
            }
        }
    }
    // The measurement the tolerance was chosen from.
    println!("worst relative bin error against rustfft: {worst:e}");
}

/// An impulse is flat, DC is a single bin: the two cases where the answer is
/// known without an oracle at all.
#[test]
fn the_known_transforms_come_out_right() {
    let n = 16;
    let source = "def spectrum(x: Tensor[f64, 16]) -> Tensor[f64, 9]:\n    return fft(x)\n";

    let mut impulse = vec![0.0; n];
    impulse[0] = 1.0;
    let got = evaluate(source, "spectrum", &[&impulse], 9);
    assert_eq!(bits(&got), bits(&[1.0; 9]));

    let got = evaluate(source, "spectrum", &[&vec![1.0; n]], 9);
    assert_eq!(got[0], n as f64);
    assert!(got[1..].iter().all(|v| *v < 1e-12), "{got:?}");
}

/// Rows transform independently, which is what makes a spectrogram one call.
#[test]
fn a_higher_rank_transforms_along_the_last_axis() {
    let row: Vec<f64> = (0..8).map(|i| i as f64).collect();
    let mut both = row.clone();
    both.extend(row.iter().rev());
    let got = evaluate(
        "def spectrum(x: Tensor[f64, (2, 8)]) -> Tensor[f64, (2, 5)]:\n    return fft(x)\n",
        "spectrum",
        &[&both],
        10,
    );
    for (group, half) in [(0usize, &row), (1, &both[8..].to_vec())] {
        let want = oracle(half);
        for (k, (a, b)) in got[group * 5..][..5].iter().zip(&want).enumerate() {
            assert!((a - b).abs() < 1e-12, "group {group}, bin {k}: {a} vs {b}");
        }
    }
}

#[test]
fn fft_refuses_what_radix_two_cannot_answer() {
    for (source, needle) in [
        (
            "def f(x: Tensor[f64, 6]) -> Tensor[f64, 4]:\n    return fft(x)\n",
            "power-of-two last axis, found 6",
        ),
        (
            "def f(x: Tensor[f64, 1]) -> Tensor[f64, 1]:\n    return fft(x)\n",
            "power-of-two last axis, found 1",
        ),
        (
            "def f(x: f64) -> f64:\n    return fft(x)\n",
            "`fft` needs a tensor",
        ),
    ] {
        let text = format!("{}", reject(source));
        assert!(text.contains(needle), "{source}: {text}");
    }
}

/// The ring is `N` samples with the newest last, preloaded with zeros — which
/// is exactly what the panel's Window node published, leading zeros included.
#[test]
fn a_window_holds_the_last_samples_newest_last() {
    let source = "\
@system(\"wheels.rpm\")
def last4(rpm) -> Tensor[f64, 4]:
    return window(rpm, 4)
";
    let mut run = build_system(source, &imu_table(), "last4");
    let seen: Vec<Vec<f64>> = (1..=6)
        .map(|i| {
            run.set("rpm", "rpm", &[i as f64]);
            run.eval(i);
            run.get("last4")
        })
        .collect();
    assert_eq!(seen[0], vec![0.0, 0.0, 0.0, 1.0]);
    assert_eq!(seen[2], vec![0.0, 1.0, 2.0, 3.0]);
    assert_eq!(seen[3], vec![1.0, 2.0, 3.0, 4.0]);
    assert_eq!(seen[5], vec![3.0, 4.0, 5.0, 6.0]);
}

/// A tensor sample keeps its shape, so the window is one rank deeper — the
/// legacy op's `[size, ...input.shape]`.
#[test]
fn a_window_of_a_vector_is_one_rank_deeper() {
    let source = "\
@system(\"adcs.omega_b\")
def trail(omega_b) -> Tensor[f64, (2, 3)]:
    return window(omega_b, 2)
";
    let mut run = build_system(source, &imu_table(), "trail");
    run.set("omega_b", "omega_b", &[1.0, 2.0, 3.0]);
    run.eval(1);
    assert_eq!(run.get("trail"), vec![0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
    run.set("omega_b", "omega_b", &[4.0, 5.0, 6.0]);
    run.eval(2);
    assert_eq!(run.get("trail"), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
}

/// The ring is a state field like any other, so an edit that keeps it keeps
/// the history — and two windows in one body are two rings, not one.
#[test]
fn a_window_is_ordinary_state() {
    let program = crate::compile_module(
        "@system(\"wheels.rpm\")\ndef both(rpm) -> f64:\n    return window(rpm, 4)[0] + window(rpm, 8)[0]\n",
        &imu_table(),
    )
    .unwrap();
    let state = &program.manifest.system("both").unwrap().state;
    assert_eq!(state.len(), 2);
    assert_eq!(
        state.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        vec!["@window0", "@window1"]
    );
}

#[test]
fn window_needs_a_system_and_a_literal_length() {
    let text = refuse(
        "def f(x: f64) -> Tensor[f64, 4]:\n    return window(x, 4)\n",
        &Table::new(&[]),
    );
    assert!(text.contains("inside a system"), "{text}");

    let text = refuse(
        "@system(\"wheels.rpm\")\ndef f(rpm) -> Tensor[f64, 4]:\n    return window(rpm, rpm)\n",
        &imu_table(),
    );
    assert!(text.contains("positive integer literal"), "{text}");
}

/// The two together, which is the pipeline the legacy graph spelled as two
/// nodes and an edge.
#[test]
fn a_window_feeds_a_spectrum() {
    let source = "\
@system(\"wheels.rpm\")
def spectrum(rpm) -> Tensor[f64, 5]:
    return fft(window(rpm, 8))
";
    let mut run = build_system(source, &imu_table(), "spectrum");
    let signal: Vec<f64> = (0..8)
        .map(|i| (std::f64::consts::TAU * i as f64 / 8.0).sin())
        .collect();
    for (i, v) in signal.iter().enumerate() {
        run.set("rpm", "rpm", &[*v]);
        run.eval(i as i64);
    }
    let got = run.get("spectrum");
    let want = oracle(&signal);
    for (k, (a, b)) in got.iter().zip(&want).enumerate() {
        assert!((a - b).abs() < 1e-12, "bin {k}: {a} vs {b}");
    }
}
