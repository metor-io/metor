use std::sync::Arc;
use std::time::Duration;

use metor_proto::types::PrimType;
use smallvec::SmallVec;

use crate::dynamic::node::{DynamicNodeExt, ValueType};
use crate::dynamic::ops;
use crate::dynamic::tensor::TypedScalar;

// Test-only convenience wrappers around the dtype/shape-aware generator
// constructors so the existing assertions stay compact and readable.
fn const_f64(
    clock: std::sync::Arc<dyn crate::dynamic::DynamicNode>,
    v: f64,
) -> Result<std::sync::Arc<dyn crate::dynamic::DynamicNode>, crate::dynamic::BuildError> {
    ops::generators::constant(clock, TypedScalar::F64(v), SmallVec::new())
}

fn waveform_f64(
    clock: std::sync::Arc<dyn crate::dynamic::DynamicNode>,
    shape: ops::generators::Waveform,
    freq: f64,
    amplitude: f64,
    phase: f64,
) -> Result<std::sync::Arc<dyn crate::dynamic::DynamicNode>, crate::dynamic::BuildError> {
    ops::generators::waveform(clock, shape, freq, amplitude, phase, PrimType::F64, SmallVec::new())
}

#[allow(dead_code)]
fn random_f64(
    clock: std::sync::Arc<dyn crate::dynamic::DynamicNode>,
    seed: u64,
) -> Result<std::sync::Arc<dyn crate::dynamic::DynamicNode>, crate::dynamic::BuildError> {
    ops::generators::random(clock, seed, PrimType::F64, SmallVec::new())
}

/// Pull `count` f64 samples (with timestamps) off a node. Bails after a
/// generous timeout so a stuck task doesn't hang the test runner.
async fn drain_f64(
    node: &std::sync::Arc<dyn crate::dynamic::DynamicNode>,
    count: usize,
) -> Vec<(metor_proto::types::Timestamp, f64)> {
    let mut reader = node.subscribe();
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let grant = reader.next().await;
        for (ts, v) in grant.samples() {
            if v.len() != 8 {
                continue;
            }
            let bytes: [u8; 8] = v.try_into().unwrap();
            out.push((ts, f64::from_le_bytes(bytes)));
            if out.len() == count {
                break;
            }
        }
    }
    out
}

#[stellarator::test]
async fn sin_chain_emits_expected_values() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    assert!(matches!(clock.value_type(), ValueType::Clock));
    let sin = waveform_f64(
        clock,
        ops::generators::Waveform::Sin,
        1.0,
        1.0,
        0.0,
    )
    .expect("sin builds");
    let scaled = ops::derive::scale(sin, TypedScalar::F64(2.0)).expect("scale builds");

    let samples = drain_f64(&scaled, 32).await;
    for (ts, v) in &samples {
        let t = (ts.0 as f64) * 1e-6;
        let expected = 2.0 * f64::sin(std::f64::consts::TAU * t);
        assert!(
            (v - expected).abs() < 1e-9,
            "v={v}, expected={expected}, ts={ts:?}"
        );
    }
}

#[stellarator::test]
async fn compose_clock_mismatch_errors() {
    let clk_a = ops::clock::fixed_rate(100.0).unwrap();
    let clk_b = ops::clock::fixed_rate(200.0).unwrap();
    let a = waveform_f64(
        clk_a,
        ops::generators::Waveform::Sin,
        1.0,
        1.0,
        0.0,
    )
    .unwrap();
    let b = waveform_f64(
        clk_b,
        ops::generators::Waveform::Sin,
        1.0,
        1.0,
        0.0,
    )
    .unwrap();
    let err = match ops::compose::add(a, b) {
        Ok(_) => panic!("must reject mismatched clocks"),
        Err(e) => e,
    };
    assert!(matches!(err, crate::dynamic::BuildError::ClockMismatch));
}

#[stellarator::test]
async fn compose_add_is_co_clocked() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let a = const_f64(clock.clone(), 3.0).unwrap();
    let b = const_f64(clock, 4.0).unwrap();
    let sum = ops::compose::add(a, b).unwrap();
    let samples = drain_f64(&sum, 16).await;
    for (_, v) in &samples {
        assert!((v - 7.0).abs() < 1e-12);
    }
}

#[stellarator::test]
async fn window_buffers_recent_samples() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let sin = waveform_f64(
        clock,
        ops::generators::Waveform::Sin,
        1.0,
        1.0,
        0.0,
    )
    .unwrap();
    // Subscribe to the input *before* building the window so both readers
    // start at the same head of the disruptor.
    let mut sin_reader = sin.subscribe();
    const N: usize = 4;
    let win = ops::derive::window(sin, N).unwrap();

    // Schema must be `[N]` of f64; sample bytes are `N * 8`.
    let schema = match win.value_type() {
        ValueType::Value(s) => s,
        _ => panic!("window must produce a value"),
    };
    assert_eq!(schema.dim.as_slice(), &[N]);
    assert_eq!(schema.size(), N * 8);

    let mut win_reader = win.subscribe();
    const TOTAL: usize = 8;
    let mut sin_vals: Vec<f64> = Vec::with_capacity(TOTAL);
    let mut win_emits: Vec<[f64; N]> = Vec::with_capacity(TOTAL);
    while sin_vals.len() < TOTAL || win_emits.len() < TOTAL {
        if sin_vals.len() < TOTAL {
            let grant = sin_reader.next().await;
            for (_ts, v) in grant.samples() {
                if v.len() != 8 {
                    continue;
                }
                sin_vals.push(f64::from_le_bytes(v.try_into().unwrap()));
                if sin_vals.len() == TOTAL {
                    break;
                }
            }
        }
        if win_emits.len() < TOTAL {
            let grant = win_reader.next().await;
            for (_ts, v) in grant.samples() {
                if v.len() != N * 8 {
                    continue;
                }
                let mut row = [0.0_f64; N];
                for (k, chunk) in v.chunks_exact(8).enumerate() {
                    row[k] = f64::from_le_bytes(chunk.try_into().unwrap());
                }
                win_emits.push(row);
                if win_emits.len() == TOTAL {
                    break;
                }
            }
        }
    }

    // Index 0 = oldest, index N-1 = newest. For emit i, buf[k] is the
    // input value `(N-1-k)` ticks ago, or 0.0 if that tick predates the
    // start (the buffer is preloaded with zeros).
    #[allow(clippy::needless_range_loop)]
    for i in 0..TOTAL {
        for k in 0..N {
            let lag = N - 1 - k;
            let expected = if i >= lag { sin_vals[i - lag] } else { 0.0 };
            assert!(
                (win_emits[i][k] - expected).abs() < 1e-12,
                "emit {i}, slot {k}: got {}, expected {expected}",
                win_emits[i][k],
            );
        }
    }
}

#[stellarator::test]
async fn window_rejects_zero_size() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_f64(clock, 1.0).unwrap();
    let err = match ops::derive::window(src, 0) {
        Ok(_) => panic!("size=0 must be rejected"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::InvalidArg { op: "window", .. }
    ));
}

#[stellarator::test]
async fn fft_dc_input_concentrates_in_bin_zero() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    const N: usize = 8;
    // Pack N co-clocked constants of value 1.0 into [1, 1, ..., 1].
    let inputs: Vec<_> = (0..N)
        .map(|_| const_f64(clock.clone(), 1.0).unwrap())
        .collect();
    let packed = ops::compose::pack(inputs).unwrap();
    let fft = ops::derive::fft(packed).unwrap();

    // Output schema: N/2 + 1 magnitude bins.
    let schema = match fft.value_type() {
        ValueType::Value(s) => s,
        _ => panic!("fft must produce a value"),
    };
    let out_len = N / 2 + 1;
    assert_eq!(schema.dim.as_slice(), &[out_len]);

    let mut reader = fft.subscribe();
    let mut got = 0;
    while got < 4 {
        let grant = reader.next().await;
        for (_ts, value) in grant.samples() {
            if value.len() != out_len * 8 {
                continue;
            }
            let bins: Vec<f64> = value
                .chunks_exact(8)
                .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
                .collect();
            // All-ones input: bin 0 = N, all other bins = 0.
            assert!(
                (bins[0] - N as f64).abs() < 1e-9,
                "bin 0: got {}, expected {}",
                bins[0],
                N
            );
            for (k, m) in bins.iter().enumerate().skip(1) {
                assert!(m.abs() < 1e-9, "bin {k}: expected 0, got {m}");
            }
            got += 1;
            if got == 4 {
                break;
            }
        }
    }
}

#[stellarator::test]
async fn fft_impulse_has_flat_magnitude() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    const N: usize = 8;
    // Pack [1, 0, 0, 0, 0, 0, 0, 0] — a unit impulse.
    let mut inputs: Vec<Arc<dyn crate::dynamic::DynamicNode>> = Vec::with_capacity(N);
    inputs.push(const_f64(clock.clone(), 1.0).unwrap());
    for _ in 1..N {
        inputs.push(const_f64(clock.clone(), 0.0).unwrap());
    }
    let packed = ops::compose::pack(inputs).unwrap();
    let fft = ops::derive::fft(packed).unwrap();

    let mut reader = fft.subscribe();
    let out_len = N / 2 + 1;
    let mut got = 0;
    while got < 4 {
        let grant = reader.next().await;
        for (_ts, value) in grant.samples() {
            if value.len() != out_len * 8 {
                continue;
            }
            let bins: Vec<f64> = value
                .chunks_exact(8)
                .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
                .collect();
            // Impulse → flat magnitude spectrum, every bin = 1.
            for (k, m) in bins.iter().enumerate() {
                assert!(
                    (m - 1.0).abs() < 1e-9,
                    "bin {k}: expected 1.0, got {m}"
                );
            }
            got += 1;
            if got == 4 {
                break;
            }
        }
    }
}

#[stellarator::test]
async fn fft_rejects_scalar_input() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let scalar = const_f64(clock, 1.0).unwrap();
    let err = match ops::derive::fft(scalar) {
        Ok(_) => panic!("scalar input must be rejected"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::InvalidArg { op: "fft", .. }
    ));
}

#[stellarator::test]
async fn pack_emits_vector_samples() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let x = const_f64(clock.clone(), 1.5).unwrap();
    let y = const_f64(clock, -2.5).unwrap();
    let packed = ops::compose::pack(vec![x, y]).unwrap();
    // Schema must be F64[2]; values come out as 16 raw bytes (two LE f64s).
    let schema = match packed.value_type() {
        crate::dynamic::ValueType::Value(s) => s,
        _ => panic!("pack must produce a value, not a clock"),
    };
    assert_eq!(schema.dim.as_slice(), &[2]);
    assert_eq!(schema.size(), 16);
    let mut reader = packed.subscribe();
    let mut got = 0;
    while got < 8 {
        let grant = reader.next().await;
        for (_ts, value) in grant.samples() {
            assert_eq!(value.len(), 16);
            let x = f64::from_le_bytes(value[..8].try_into().unwrap());
            let y = f64::from_le_bytes(value[8..].try_into().unwrap());
            assert!((x - 1.5).abs() < 1e-12);
            assert!((y + 2.5).abs() < 1e-12);
            got += 1;
            if got == 8 {
                break;
            }
        }
    }
}

#[stellarator::test]
async fn zoh_resamples_constant_input() {
    let slow = ops::clock::fixed_rate(50.0).unwrap();
    let fast = ops::clock::fixed_rate(400.0).unwrap();
    let src = const_f64(slow, 1.5).unwrap();
    let resampled = ops::resample::zoh(src, fast).unwrap();
    let samples = drain_f64(&resampled, 32).await;
    for (_, v) in &samples {
        assert!((v - 1.5).abs() < 1e-12);
    }
}

#[stellarator::test]
async fn registry_drops_unused_nodes() {
    use std::collections::HashSet;
    let mut reg = crate::dynamic::DynamicRegistry::new();
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let id = clock.id();
    reg.insert(clock);
    assert_eq!(reg.len(), 1);

    let mut alive = HashSet::new();
    alive.insert(id);
    reg.reconcile(&alive);
    assert_eq!(reg.len(), 1);

    reg.reconcile(&HashSet::new());
    assert_eq!(reg.len(), 0);
    // Give the cancelled task a moment to wind down — best-effort.
    stellarator::sleep(Duration::from_millis(20)).await;
}

#[stellarator::test]
async fn clock_of_taps_source_timestamps() {
    let src_clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_f64(src_clock, 0.0).unwrap();
    let derived = ops::clock::clock_of(src);
    assert!(matches!(derived.value_type(), ValueType::Clock));
    // Just confirm it's producing without hanging.
    let mut reader = derived.subscribe();
    let _ = reader.next().await;
}

// --- broadcast / dtype generalization tests ---

fn const_typed(
    clock: std::sync::Arc<dyn crate::dynamic::DynamicNode>,
    value: TypedScalar,
    out_shape: SmallVec<[usize; 4]>,
) -> Result<std::sync::Arc<dyn crate::dynamic::DynamicNode>, crate::dynamic::BuildError> {
    ops::generators::constant(clock, value, out_shape)
}

#[stellarator::test]
async fn add_promotes_f32_and_f64_to_f64() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let a = const_typed(clock.clone(), TypedScalar::F32(1.5), SmallVec::new()).unwrap();
    let b = const_typed(clock, TypedScalar::F64(2.25), SmallVec::new()).unwrap();
    let sum = ops::compose::add(a, b).unwrap();
    let schema = match sum.value_type() {
        ValueType::Value(s) => s,
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert!(schema.dim.is_empty());
    let samples = drain_f64(&sum, 8).await;
    for (_, v) in &samples {
        assert!((v - 3.75).abs() < 1e-6);
    }
}

#[stellarator::test]
async fn add_broadcasts_scalar_to_vector() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let vec_node = const_typed(
        clock.clone(),
        TypedScalar::F64(2.0),
        SmallVec::from_slice(&[3]),
    )
    .unwrap();
    let scalar = const_f64(clock, 1.0).unwrap();
    let sum = ops::compose::add(vec_node, scalar).unwrap();
    let schema = match sum.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert_eq!(schema.dim.as_slice(), &[3]);
    use crate::dynamic::node::DynamicNodeExt;
    let mut reader = sum.subscribe();
    let mut got = 0;
    while got < 4 {
        let grant = reader.next().await;
        for (_, value) in grant.samples() {
            if value.len() != 3 * 8 {
                continue;
            }
            for chunk in value.chunks_exact(8) {
                let v = f64::from_le_bytes(chunk.try_into().unwrap());
                assert!((v - 3.0).abs() < 1e-12);
            }
            got += 1;
            if got == 4 { break; }
        }
    }
}

#[stellarator::test]
async fn add_broadcasts_outer_product_shape() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let a = const_typed(
        clock.clone(),
        TypedScalar::F64(1.0),
        SmallVec::from_slice(&[1, 3]),
    )
    .unwrap();
    let b = const_typed(
        clock,
        TypedScalar::F64(10.0),
        SmallVec::from_slice(&[4, 1]),
    )
    .unwrap();
    let sum = ops::compose::add(a, b).unwrap();
    let schema = match sum.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.dim.as_slice(), &[4, 3]);
    use crate::dynamic::node::DynamicNodeExt;
    let mut reader = sum.subscribe();
    let grant = reader.next().await;
    for (_, value) in grant.samples() {
        if value.len() != 12 * 8 {
            continue;
        }
        for chunk in value.chunks_exact(8) {
            let v = f64::from_le_bytes(chunk.try_into().unwrap());
            assert!((v - 11.0).abs() < 1e-12);
        }
        return;
    }
}

#[stellarator::test]
async fn scale_promotes_int_input_with_f64_arg() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let src = const_typed(clock, TypedScalar::I32(4), SmallVec::new()).unwrap();
    let scaled = ops::derive::scale(src, TypedScalar::F64(2.5)).unwrap();
    let schema = match scaled.value_type() {
        ValueType::Value(s) => s,
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    let samples = drain_f64(&scaled, 4).await;
    for (_, v) in &samples {
        assert!((v - 10.0).abs() < 1e-12);
    }
}

#[stellarator::test]
async fn add_rejects_unbroadcastable_shapes() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let a = const_typed(
        clock.clone(),
        TypedScalar::F64(1.0),
        SmallVec::from_slice(&[3]),
    )
    .unwrap();
    let b = const_typed(clock, TypedScalar::F64(1.0), SmallVec::from_slice(&[4])).unwrap();
    let err = match ops::compose::add(a, b) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    assert!(matches!(err, crate::dynamic::BuildError::BroadcastMismatch { .. }));
}

#[stellarator::test]
async fn neg_rejects_unsigned_dtype() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_typed(clock, TypedScalar::U8(5), SmallVec::new()).unwrap();
    let err = match ops::derive::neg(src) {
        Ok(_) => panic!("must reject u8"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::UnsupportedDtype { op: "neg", .. }
    ));
}

#[stellarator::test]
async fn zoh_passes_through_typed_vector_bytes() {
    let slow = ops::clock::fixed_rate(50.0).unwrap();
    let fast = ops::clock::fixed_rate(400.0).unwrap();
    let src = const_typed(
        slow,
        TypedScalar::F32(3.5),
        SmallVec::from_slice(&[3]),
    )
    .unwrap();
    let resampled = ops::resample::zoh(src, fast).unwrap();
    let schema = match resampled.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F32);
    assert_eq!(schema.dim.as_slice(), &[3]);
    use crate::dynamic::node::DynamicNodeExt;
    let mut reader = resampled.subscribe();
    let mut got = 0;
    while got < 4 {
        let grant = reader.next().await;
        for (_, value) in grant.samples() {
            if value.len() != 3 * 4 {
                continue;
            }
            for chunk in value.chunks_exact(4) {
                let v = f32::from_le_bytes(chunk.try_into().unwrap());
                assert!((v - 3.5).abs() < 1e-6);
            }
            got += 1;
            if got == 4 { break; }
        }
    }
}
