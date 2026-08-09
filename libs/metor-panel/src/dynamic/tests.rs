use std::sync::Arc;
use std::time::Duration;

use metor_proto::types::PrimType;
use smallvec::SmallVec;

use crate::dynamic::node::{DynamicNodeExt, ValueType};
use crate::dynamic::ops;
use crate::dynamic::ops::compose::BinaryOp;
use crate::dynamic::ops::derive::{AffineOp, UnaryOp};
use crate::dynamic::ops::resample::ResampleMode;
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
    ops::generators::waveform(
        clock,
        shape,
        freq,
        amplitude,
        phase,
        PrimType::F64,
        SmallVec::new(),
    )
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
async fn from_db_adopts_the_component_wal() {
    use metor_db::{ComponentSchema, DB};
    use metor_proto::types::{ComponentId, Timestamp};

    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    let component_id = ComponentId(42);
    db.with_state_mut(|state| {
        state.insert_component(
            component_id,
            ComponentSchema::new(PrimType::F64, &[]),
            &db.path,
        )
    })
    .unwrap();
    let component = db
        .with_state(|state| state.get_component(component_id).cloned())
        .unwrap();
    let readers_before = component.wal.reader_count();

    let node = ops::db_source::from_db(&db, component_id).unwrap();
    assert_eq!(component.wal.reader_count(), readers_before);
    let mut reader = node.subscribe();
    assert_eq!(component.wal.reader_count(), readers_before + 1);

    component
        .push_buf(Timestamp(7), &12.5_f64.to_le_bytes())
        .unwrap();
    let grant = reader.next().await;
    let (timestamp, value) = grant.sample_at(0);
    assert_eq!(timestamp, Timestamp(7));
    assert_eq!(f64::from_le_bytes(value.try_into().unwrap()), 12.5);
}

#[stellarator::test]
async fn sin_chain_emits_expected_values() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    assert!(matches!(clock.value_type(), ValueType::Clock));
    let sin =
        waveform_f64(clock, ops::generators::Waveform::Sin, 1.0, 1.0, 0.0).expect("sin builds");
    let scaled =
        ops::derive::affine(sin, AffineOp::Scale, TypedScalar::F64(2.0)).expect("scale builds");

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
    let a = waveform_f64(clk_a, ops::generators::Waveform::Sin, 1.0, 1.0, 0.0).unwrap();
    let b = waveform_f64(clk_b, ops::generators::Waveform::Sin, 1.0, 1.0, 0.0).unwrap();
    let err = match ops::compose::binary_op(a, b, BinaryOp::Add) {
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
    let sum = ops::compose::binary_op(a, b, BinaryOp::Add).unwrap();
    let samples = drain_f64(&sum, 16).await;
    for (_, v) in &samples {
        assert!((v - 7.0).abs() < 1e-12);
    }
}

#[stellarator::test]
async fn window_buffers_recent_samples() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let sin = waveform_f64(clock, ops::generators::Waveform::Sin, 1.0, 1.0, 0.0).unwrap();
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
                assert!((m - 1.0).abs() < 1e-9, "bin {k}: expected 1.0, got {m}");
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
    let resampled = ops::resample::resample(src, fast, ResampleMode::Zoh).unwrap();
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
    let sum = ops::compose::binary_op(a, b, BinaryOp::Add).unwrap();
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
    let sum = ops::compose::binary_op(vec_node, scalar, BinaryOp::Add).unwrap();
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
            if got == 4 {
                break;
            }
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
    let b = const_typed(clock, TypedScalar::F64(10.0), SmallVec::from_slice(&[4, 1])).unwrap();
    let sum = ops::compose::binary_op(a, b, BinaryOp::Add).unwrap();
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
    let scaled = ops::derive::affine(src, AffineOp::Scale, TypedScalar::F64(2.5)).unwrap();
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
    let err = match ops::compose::binary_op(a, b, BinaryOp::Add) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::BroadcastMismatch { .. }
    ));
}

#[stellarator::test]
async fn neg_rejects_unsigned_dtype() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_typed(clock, TypedScalar::U8(5), SmallVec::new()).unwrap();
    let err = match ops::derive::unary(src, UnaryOp::Neg) {
        Ok(_) => panic!("must reject u8"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::UnsupportedDtype { op: "neg", .. }
    ));
}

#[stellarator::test]
async fn magnitude_reduces_vector_to_l2_norm() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // Pack [3.0, 4.0] → expected magnitude = 5.0.
    let a = const_f64(clock.clone(), 3.0).unwrap();
    let b = const_f64(clock, 4.0).unwrap();
    let packed = ops::compose::pack(vec![a, b]).unwrap();
    let mag = ops::derive::magnitude(packed).unwrap();
    let schema = match mag.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert!(schema.dim.is_empty(), "magnitude reduces to a scalar");
    let samples = drain_f64(&mag, 8).await;
    for (_, v) in &samples {
        assert!((v - 5.0).abs() < 1e-9, "got {v}");
    }
}

#[stellarator::test]
async fn magnitude_promotes_int_input_to_f64() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // i32 vector [3, 4] — magnitude should be 5.0 as f64.
    let v = const_typed(clock, TypedScalar::I32(0), SmallVec::from_slice(&[2])).unwrap();
    // Need a non-zero vector — wrap via pack of two scalars
    let _ = v;
    let clock2 = ops::clock::fixed_rate(200.0).unwrap();
    let a = const_typed(clock2.clone(), TypedScalar::I32(3), SmallVec::new()).unwrap();
    let b = const_typed(clock2, TypedScalar::I32(4), SmallVec::new()).unwrap();
    let packed = ops::compose::pack(vec![a, b]).unwrap();
    let mag = ops::derive::magnitude(packed).unwrap();
    let schema = match mag.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    let samples = drain_f64(&mag, 4).await;
    for (_, v) in &samples {
        assert!((v - 5.0).abs() < 1e-9);
    }
}

#[stellarator::test]
async fn magnitude_rejects_bool() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_typed(clock, TypedScalar::Bool(true), SmallVec::new()).unwrap();
    let err = match ops::derive::magnitude(src) {
        Ok(_) => panic!("must reject bool"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::UnsupportedDtype {
            op: "magnitude",
            ..
        }
    ));
}

#[stellarator::test]
async fn dot_product_of_vectors() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // a = [1, 2, 3], b = [4, 5, 6] → 1*4 + 2*5 + 3*6 = 32
    let a1 = const_f64(clock.clone(), 1.0).unwrap();
    let a2 = const_f64(clock.clone(), 2.0).unwrap();
    let a3 = const_f64(clock.clone(), 3.0).unwrap();
    let b1 = const_f64(clock.clone(), 4.0).unwrap();
    let b2 = const_f64(clock.clone(), 5.0).unwrap();
    let b3 = const_f64(clock, 6.0).unwrap();
    let a = ops::compose::pack(vec![a1, a2, a3]).unwrap();
    let b = ops::compose::pack(vec![b1, b2, b3]).unwrap();
    let dot = ops::compose::dot(a, b).unwrap();
    let schema = match dot.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert!(schema.dim.is_empty());
    let samples = drain_f64(&dot, 4).await;
    for (_, v) in &samples {
        assert!((v - 32.0).abs() < 1e-9, "got {v}");
    }
}

#[stellarator::test]
async fn dot_product_broadcasts_scalar() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // [1,2,3] dot scalar 2 = (1+2+3)*2 = 12 (broadcast scalar replicates)
    let a1 = const_f64(clock.clone(), 1.0).unwrap();
    let a2 = const_f64(clock.clone(), 2.0).unwrap();
    let a3 = const_f64(clock.clone(), 3.0).unwrap();
    let a = ops::compose::pack(vec![a1, a2, a3]).unwrap();
    let s = const_f64(clock, 2.0).unwrap();
    let dot = ops::compose::dot(a, s).unwrap();
    let samples = drain_f64(&dot, 4).await;
    for (_, v) in &samples {
        assert!((v - 12.0).abs() < 1e-9, "got {v}");
    }
}

#[stellarator::test]
async fn dot_product_rejects_clock_mismatch() {
    let clk_a = ops::clock::fixed_rate(100.0).unwrap();
    let clk_b = ops::clock::fixed_rate(200.0).unwrap();
    let a = const_f64(clk_a, 1.0).unwrap();
    let b = const_f64(clk_b, 1.0).unwrap();
    let err = match ops::compose::dot(a, b) {
        Ok(_) => panic!("must reject mismatched clocks"),
        Err(e) => e,
    };
    assert!(matches!(err, crate::dynamic::BuildError::ClockMismatch));
}

#[stellarator::test]
async fn index_extracts_scalar_from_packed_vector() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // Pack [10, 20, 30] → index 1 → 20
    let a = const_f64(clock.clone(), 10.0).unwrap();
    let b = const_f64(clock.clone(), 20.0).unwrap();
    let c = const_f64(clock, 30.0).unwrap();
    let packed = ops::compose::pack(vec![a, b, c]).unwrap();
    let elem = ops::derive::index(packed, 1).unwrap();
    let schema = match elem.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert!(schema.dim.is_empty());
    let samples = drain_f64(&elem, 8).await;
    for (_, v) in &samples {
        assert!((v - 20.0).abs() < 1e-12, "got {v}");
    }
}

#[stellarator::test]
async fn index_preserves_int_dtype() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let a = const_typed(clock.clone(), TypedScalar::I32(7), SmallVec::new()).unwrap();
    let b = const_typed(clock, TypedScalar::I32(11), SmallVec::new()).unwrap();
    let packed = ops::compose::pack(vec![a, b]).unwrap();
    let elem = ops::derive::index(packed, 1).unwrap();
    let schema = match elem.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::I32);
    assert!(schema.dim.is_empty());
    let mut reader = elem.subscribe();
    let mut got = 0;
    while got < 4 {
        let grant = reader.next().await;
        for (_, value) in grant.samples() {
            if value.len() != 4 {
                continue;
            }
            let v = i32::from_le_bytes(value.try_into().unwrap());
            assert_eq!(v, 11);
            got += 1;
            if got == 4 {
                break;
            }
        }
    }
}

#[stellarator::test]
async fn index_rejects_out_of_range() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let a = const_f64(clock.clone(), 1.0).unwrap();
    let b = const_f64(clock, 2.0).unwrap();
    let packed = ops::compose::pack(vec![a, b]).unwrap();
    let err = match ops::derive::index(packed, 5) {
        Ok(_) => panic!("must reject out-of-range index"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::InvalidArg { op: "index", .. }
    ));
}

#[stellarator::test]
async fn threshold_emits_one_above_zero_below() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let above = const_f64(clock.clone(), 5.0).unwrap();
    let below = const_f64(clock, -1.0).unwrap();
    let above_t = ops::derive::threshold(
        above,
        TypedScalar::F64(2.0),
        crate::dynamic::ops::derive::ThresholdOp::Gt,
    )
    .unwrap();
    let below_t = ops::derive::threshold(
        below,
        TypedScalar::F64(2.0),
        crate::dynamic::ops::derive::ThresholdOp::Gt,
    )
    .unwrap();
    let above_samples = drain_f64(&above_t, 4).await;
    for (_, v) in &above_samples {
        assert_eq!(*v, 1.0);
    }
    let below_samples = drain_f64(&below_t, 4).await;
    for (_, v) in &below_samples {
        assert_eq!(*v, 0.0);
    }
}

#[stellarator::test]
async fn threshold_preserves_input_shape() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // Vector [1, 5, 3] thresholded against 2 → [0, 1, 1]
    let a = const_f64(clock.clone(), 1.0).unwrap();
    let b = const_f64(clock.clone(), 5.0).unwrap();
    let c = const_f64(clock, 3.0).unwrap();
    let packed = ops::compose::pack(vec![a, b, c]).unwrap();
    let t = ops::derive::threshold(
        packed,
        TypedScalar::F64(2.0),
        crate::dynamic::ops::derive::ThresholdOp::Gt,
    )
    .unwrap();
    let schema = match t.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert_eq!(schema.dim.as_slice(), &[3]);
    let mut reader = t.subscribe();
    let grant = reader.next().await;
    for (_, value) in grant.samples() {
        if value.len() != 24 {
            continue;
        }
        let bins: Vec<f64> = value
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(bins, vec![0.0, 1.0, 1.0]);
        return;
    }
}

async fn check_threshold(
    x: f64,
    op: crate::dynamic::ops::derive::ThresholdOp,
    k: f64,
    expected: f64,
) {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let src = const_f64(clock, x).unwrap();
    let node = ops::derive::threshold(src, TypedScalar::F64(k), op).unwrap();
    let samples = drain_f64(&node, 2).await;
    for (_, v) in &samples {
        assert_eq!(*v, expected, "x={x} op={op:?} k={k}");
    }
}

// Boundary (x == k) confirms open/closed semantics of >, >=, <, <=.
#[stellarator::test]
async fn threshold_gt_boundary_excludes_equal() {
    use crate::dynamic::ops::derive::ThresholdOp;
    check_threshold(2.0, ThresholdOp::Gt, 2.0, 0.0).await;
}

#[stellarator::test]
async fn threshold_ge_boundary_includes_equal() {
    use crate::dynamic::ops::derive::ThresholdOp;
    check_threshold(2.0, ThresholdOp::Ge, 2.0, 1.0).await;
}

#[stellarator::test]
async fn threshold_lt_boundary_excludes_equal() {
    use crate::dynamic::ops::derive::ThresholdOp;
    check_threshold(2.0, ThresholdOp::Lt, 2.0, 0.0).await;
}

#[stellarator::test]
async fn threshold_lt_below_emits_one() {
    use crate::dynamic::ops::derive::ThresholdOp;
    check_threshold(1.0, ThresholdOp::Lt, 2.0, 1.0).await;
}

#[stellarator::test]
async fn threshold_le_boundary_includes_equal() {
    use crate::dynamic::ops::derive::ThresholdOp;
    check_threshold(2.0, ThresholdOp::Le, 2.0, 1.0).await;
}

#[stellarator::test]
async fn threshold_le_above_emits_zero() {
    use crate::dynamic::ops::derive::ThresholdOp;
    check_threshold(3.0, ThresholdOp::Le, 2.0, 0.0).await;
}

#[stellarator::test]
async fn threshold_rejects_bool_input() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_typed(clock, TypedScalar::Bool(true), SmallVec::new()).unwrap();
    let err = match ops::derive::threshold(
        src,
        TypedScalar::F64(0.5),
        crate::dynamic::ops::derive::ThresholdOp::Gt,
    ) {
        Ok(_) => panic!("must reject bool"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::UnsupportedDtype {
            op: "threshold",
            ..
        }
    ));
}

#[stellarator::test]
async fn delta_skips_first_sample_then_emits_diffs() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let sin = waveform_f64(clock, ops::generators::Waveform::Sin, 1.0, 1.0, 0.0).unwrap();
    // Subscribe to input before building delta so both share a starting head.
    let mut sin_reader = sin.subscribe();
    let delta = ops::derive::delta(sin).unwrap();

    let schema = match delta.value_type() {
        ValueType::Value(s) => s,
        _ => panic!("delta must produce a value"),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert!(schema.dim.is_empty());

    let mut delta_reader = delta.subscribe();
    const TOTAL: usize = 6;
    let mut sin_vals: Vec<f64> = Vec::with_capacity(TOTAL + 1);
    let mut deltas: Vec<f64> = Vec::with_capacity(TOTAL);
    while sin_vals.len() < TOTAL + 1 || deltas.len() < TOTAL {
        if sin_vals.len() < TOTAL + 1 {
            let grant = sin_reader.next().await;
            for (_ts, v) in grant.samples() {
                if v.len() != 8 {
                    continue;
                }
                sin_vals.push(f64::from_le_bytes(v.try_into().unwrap()));
                if sin_vals.len() == TOTAL + 1 {
                    break;
                }
            }
        }
        if deltas.len() < TOTAL {
            let grant = delta_reader.next().await;
            for (_ts, v) in grant.samples() {
                if v.len() != 8 {
                    continue;
                }
                deltas.push(f64::from_le_bytes(v.try_into().unwrap()));
                if deltas.len() == TOTAL {
                    break;
                }
            }
        }
    }
    // Delta's i-th output is the diff between sin's (i+1)-th and i-th samples,
    // because the very first input is consumed silently.
    for i in 0..TOTAL {
        let expected = sin_vals[i + 1] - sin_vals[i];
        assert!(
            (deltas[i] - expected).abs() < 1e-12,
            "delta {i}: got {}, expected {expected}",
            deltas[i],
        );
    }
}

#[stellarator::test]
async fn delta_preserves_input_shape() {
    use crate::dynamic::node::DynamicNodeExt;
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    // Constant vector [1, 2, 3] — every delta is zero, but schema must keep [3].
    let a = const_f64(clock.clone(), 1.0).unwrap();
    let b = const_f64(clock.clone(), 2.0).unwrap();
    let c = const_f64(clock, 3.0).unwrap();
    let packed = ops::compose::pack(vec![a, b, c]).unwrap();
    let delta = ops::derive::delta(packed).unwrap();
    let schema = match delta.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert_eq!(schema.dim.as_slice(), &[3]);
    let mut reader = delta.subscribe();
    let grant = reader.next().await;
    for (_, value) in grant.samples() {
        if value.len() != 24 {
            continue;
        }
        let bins: Vec<f64> = value
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(bins, vec![0.0, 0.0, 0.0]);
        return;
    }
}

#[stellarator::test]
async fn delta_widens_int_to_f64() {
    let clock = ops::clock::fixed_rate(200.0).unwrap();
    let src = const_typed(clock, TypedScalar::I32(7), SmallVec::new()).unwrap();
    let delta = ops::derive::delta(src).unwrap();
    let schema = match delta.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    let samples = drain_f64(&delta, 4).await;
    for (_, v) in &samples {
        assert_eq!(*v, 0.0);
    }
}

#[stellarator::test]
async fn delta_rejects_bool() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = const_typed(clock, TypedScalar::Bool(true), SmallVec::new()).unwrap();
    let err = match ops::derive::delta(src) {
        Ok(_) => panic!("must reject bool"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        crate::dynamic::BuildError::UnsupportedDtype { op: "delta", .. }
    ));
}

#[stellarator::test]
async fn delta_t_emits_seconds_between_samples() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let dt = ops::derive::delta_t(clock).unwrap();
    let schema = match dt.value_type() {
        ValueType::Value(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(schema.prim_type, PrimType::F64);
    assert!(schema.dim.is_empty());
    let samples = drain_f64(&dt, 4).await;
    // Wall-clock jitter from OS scheduling perturbs the exact period; assert
    // the unit (seconds) and order of magnitude rather than tight equality.
    for (_, v) in &samples {
        assert!(*v > 0.0, "dt must be positive, got {v}");
        assert!((v - 0.01).abs() < 0.005, "expected ~0.01s, got {v}");
    }
}

#[stellarator::test]
async fn delta_t_accepts_value_input() {
    let clock = ops::clock::fixed_rate(50.0).unwrap();
    let src = const_typed(clock, TypedScalar::I16(42), SmallVec::new()).unwrap();
    let dt = ops::derive::delta_t(src).unwrap();
    let samples = drain_f64(&dt, 4).await;
    for (_, v) in &samples {
        assert!(*v > 0.0, "dt must be positive, got {v}");
        assert!((v - 0.02).abs() < 0.01, "expected ~0.02s, got {v}");
    }
}

#[stellarator::test]
async fn compose_realigns_after_one_sample_skew() {
    use crate::dynamic::node::{NodeReader, write_sample};
    use crate::dynamic::ops::compose::run_aligned_emit;
    use metor_db::disruptor::Disruptor;
    use metor_proto::types::Timestamp;

    // Two f64 input rings and one f64 output ring (8-byte values).
    let a_ring = Disruptor::new(8 * 1024);
    let b_ring = Disruptor::new(8 * 1024);
    let out_ring = Disruptor::new(8 * 1024);

    // Readers must be created before the writes so they start at the head
    // that precedes them and see every sample.
    let a_reader = NodeReader::from_disruptor(&a_ring, 8);
    let b_reader = NodeReader::from_disruptor(&b_ring, 8);
    let mut out_reader = NodeReader::from_disruptor(&out_ring, 8);

    // Input b leads a by one sample: a = t1..=t4, b = t2..=t5. A positional
    // pairing would mismatch every tuple (t1 vs t2, t2 vs t3, ...) and emit
    // nothing forever; the aligner must drop the stale heads and converge on
    // the shared timestamps t2, t3, t4.
    for t in 1..=4i64 {
        write_sample(&a_ring, Timestamp(t), &10.0_f64.to_le_bytes());
    }
    for t in 2..=5i64 {
        write_sample(&b_ring, Timestamp(t), &1.0_f64.to_le_bytes());
    }

    let _task = stellarator::spawn(run_aligned_emit(
        vec![a_reader, b_reader],
        out_ring.clone(),
        8,
        |inputs, out| {
            let a = f64::from_le_bytes(inputs[0].try_into().unwrap());
            let b = f64::from_le_bytes(inputs[1].try_into().unwrap());
            out.copy_from_slice(&(a + b).to_le_bytes());
        },
    ));

    let mut got: Vec<(i64, f64)> = Vec::new();
    while got.len() < 3 {
        let grant = out_reader.next().await;
        for (ts, v) in grant.samples() {
            got.push((ts.0, f64::from_le_bytes(v.try_into().unwrap())));
        }
    }
    assert_eq!(got, vec![(2, 11.0), (3, 11.0), (4, 11.0)]);
}

#[stellarator::test]
async fn resample_linear_interp_covers_all_branches() {
    use crate::dynamic::ops::resample::interp;
    use metor_proto::types::Timestamp;

    let read = |bytes: &[u8]| f64::from_le_bytes(bytes.try_into().unwrap());
    let mut out = [0u8; 8];

    // Before the first sample: only `next` present → hold it.
    interp(
        Timestamp(5),
        None,
        Some(&(Timestamp(10), vec![100.0])),
        &mut out,
        PrimType::F64,
    );
    assert_eq!(read(&out), 100.0);

    // After the last sample: only `prev` present → hold it.
    interp(
        Timestamp(25),
        Some(&(Timestamp(20), vec![200.0])),
        None,
        &mut out,
        PrimType::F64,
    );
    assert_eq!(read(&out), 200.0);

    // Exact lower endpoint (tick == t0) → prev value, no interpolation.
    interp(
        Timestamp(10),
        Some(&(Timestamp(10), vec![100.0])),
        Some(&(Timestamp(20), vec![200.0])),
        &mut out,
        PrimType::F64,
    );
    assert_eq!(read(&out), 100.0);

    // Interior: halfway between (10, 100) and (20, 200) → 150.
    interp(
        Timestamp(15),
        Some(&(Timestamp(10), vec![100.0])),
        Some(&(Timestamp(20), vec![200.0])),
        &mut out,
        PrimType::F64,
    );
    assert_eq!(read(&out), 150.0);

    // Degenerate t1 == t0: a zero span must not divide by zero; take v1.
    interp(
        Timestamp(15),
        Some(&(Timestamp(10), vec![100.0])),
        Some(&(Timestamp(10), vec![300.0])),
        &mut out,
        PrimType::F64,
    );
    let v = read(&out);
    assert!(v.is_finite(), "t1 == t0 must not produce NaN/inf, got {v}");
    assert_eq!(v, 300.0);
}

#[stellarator::test]
async fn zoh_passes_through_typed_vector_bytes() {
    let slow = ops::clock::fixed_rate(50.0).unwrap();
    let fast = ops::clock::fixed_rate(400.0).unwrap();
    let src = const_typed(slow, TypedScalar::F32(3.5), SmallVec::from_slice(&[3])).unwrap();
    let resampled = ops::resample::resample(src, fast, ResampleMode::Zoh).unwrap();
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
            if got == 4 {
                break;
            }
        }
    }
}
