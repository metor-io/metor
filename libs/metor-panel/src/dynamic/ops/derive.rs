//! Single-input value-to-value derivations. Any prim type, any shape — args
//! are dtype-typed and the output dtype follows NumPy promotion against the
//! input dtype. Compute happens in `f64` and is cast back at write time; see
//! [`crate::dynamic::tensor`] for the rationale.

use std::collections::VecDeque;
use std::hash::Hash;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use smallvec::SmallVec;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, ValueType, default_ring_bytes, hash_id,
    op_tag, require_value, write_sample,
};
use crate::dynamic::tensor::{
    self, TypedScalar, is_int, is_unsigned_int, promote, read_f64_at, write_f64_as,
};

/// Schema after promoting `input.prim_type` against `arg`'s dtype, preserving
/// shape. Used by Scale/Offset to widen ints when an `f64` arg is supplied.
fn promoted_schema(input: &ComponentSchema, arg: TypedScalar) -> ComponentSchema {
    let dtype = promote(input.prim_type, arg.dtype());
    ComponentSchema::new(dtype, &input.dim)
}

/// Generic per-element map. `f` runs in `f64`; results are cast to `out_dtype`.
fn map_each_element<F>(
    in_bytes: &[u8],
    in_dtype: PrimType,
    n: usize,
    out_dtype: PrimType,
    out: &mut Vec<u8>,
    mut f: F,
) where
    F: FnMut(f64) -> f64,
{
    out.clear();
    for i in 0..n {
        let v = read_f64_at(in_bytes, in_dtype, i);
        write_f64_as(out, out_dtype, f(v));
    }
}

fn map(
    tag: &'static [u8],
    input: Arc<dyn DynamicNode>,
    out_schema: ComponentSchema,
    extra_args: impl FnOnce(&mut std::collections::hash_map::DefaultHasher),
    f: impl Fn(f64) -> f64 + Send + Sync + 'static,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    let id = hash_id(tag, &[input.id()], extra_args);
    let parent_clock = input.parent_clock_id();
    let mut reader = input.subscribe();
    let n = tensor::shape_elems(&in_schema.dim);
    let in_value_size = in_schema.size();
    let in_dtype = in_schema.prim_type;
    let out_dtype = out_schema.prim_type;
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let mut scratch: Vec<u8> = Vec::with_capacity(out_schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_value_size {
                        continue;
                    }
                    map_each_element(value, in_dtype, n, out_dtype, &mut scratch, &f);
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// Affine transformation against a scalar `k`. Same signature for every
/// variant: one `TypedScalar` arg, dtype promoted against the input dtype.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AffineOp {
    /// `x * k`
    Scale,
    /// `x + k`
    Offset,
}

impl AffineOp {
    pub fn op_tag(self) -> &'static [u8] {
        match self {
            AffineOp::Scale => op_tag::SCALE,
            AffineOp::Offset => op_tag::OFFSET,
        }
    }

    fn apply(self, x: f64, k: f64) -> f64 {
        match self {
            AffineOp::Scale => x * k,
            AffineOp::Offset => x + k,
        }
    }
}

pub fn affine(
    input: Arc<dyn DynamicNode>,
    op: AffineOp,
    k: TypedScalar,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    let out_schema = promoted_schema(&in_schema, k);
    let kf = k.as_f64();
    map(
        op.op_tag(),
        input,
        out_schema,
        |h| k.hash(h),
        move |x| op.apply(x, kf),
    )
}

/// Single-input unary math. Schema and validation rules depend on the
/// variant — see each variant's docstring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum UnaryOp {
    /// `|x|`. Rejects `Bool`. Identity on unsigned ints. Output dtype: input.
    Abs,
    /// `-x`. Rejects `Bool` and unsigned ints. Output dtype: input.
    Neg,
    /// `ln(x)`. Rejects `Bool`. Int → F64; float preserved.
    Log,
    /// `sqrt(x)`. Rejects `Bool`. Int → F64; float preserved.
    Sqrt,
    /// `exp(x)`. Rejects `Bool`. Int → F64; float preserved.
    Exp,
    /// `floor(x)`. Rejects `Bool`. Output dtype: input (int input is identity).
    Floor,
}

impl UnaryOp {
    pub fn op_tag(self) -> &'static [u8] {
        match self {
            UnaryOp::Abs => op_tag::ABS,
            UnaryOp::Neg => op_tag::NEG,
            UnaryOp::Log => op_tag::LOG,
            UnaryOp::Sqrt => op_tag::SQRT,
            UnaryOp::Exp => op_tag::EXP,
            UnaryOp::Floor => op_tag::FLOOR,
        }
    }

    fn op_name(self) -> &'static str {
        match self {
            UnaryOp::Abs => "abs",
            UnaryOp::Neg => "neg",
            UnaryOp::Log => "log",
            UnaryOp::Sqrt => "sqrt",
            UnaryOp::Exp => "exp",
            UnaryOp::Floor => "floor",
        }
    }

    fn validate(self, dtype: PrimType) -> Result<(), BuildError> {
        if dtype == PrimType::Bool {
            return Err(BuildError::UnsupportedDtype {
                op: self.op_name(),
                dtype,
            });
        }
        if matches!(self, UnaryOp::Neg) && is_unsigned_int(dtype) {
            return Err(BuildError::UnsupportedDtype {
                op: self.op_name(),
                dtype,
            });
        }
        Ok(())
    }

    fn out_dtype(self, in_dtype: PrimType) -> PrimType {
        match self {
            UnaryOp::Abs | UnaryOp::Neg | UnaryOp::Floor => in_dtype,
            UnaryOp::Log | UnaryOp::Sqrt | UnaryOp::Exp => {
                if is_int(in_dtype) {
                    PrimType::F64
                } else {
                    in_dtype
                }
            }
        }
    }

    fn apply(self, x: f64, in_dtype: PrimType) -> f64 {
        match self {
            UnaryOp::Abs => {
                if is_unsigned_int(in_dtype) {
                    x
                } else {
                    x.abs()
                }
            }
            UnaryOp::Neg => -x,
            UnaryOp::Log => x.ln(),
            UnaryOp::Sqrt => x.sqrt(),
            UnaryOp::Exp => x.exp(),
            UnaryOp::Floor => x.floor(),
        }
    }
}

pub fn unary(input: Arc<dyn DynamicNode>, op: UnaryOp) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    let in_dtype = in_schema.prim_type;
    op.validate(in_dtype)?;
    let out_schema = ComponentSchema::new(op.out_dtype(in_dtype), &in_schema.dim);
    map(
        op.op_tag(),
        input,
        out_schema,
        |_| {},
        move |x| op.apply(x, in_dtype),
    )
}

/// One-sided magnitude FFT of a real-valued vector input. Computes along the
/// last axis when input is multi-dimensional; output replaces the last axis
/// length with `N/2 + 1`. Input must total `>= 2` along that axis.
///
/// Output dtype is `f64` regardless of input dtype.
pub fn fft(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    if in_schema.dim.is_empty() {
        return Err(BuildError::InvalidArg {
            op: "fft",
            reason: "input must be at least 1-D",
        });
    }
    let last = *in_schema.dim.last().unwrap();
    if last < 2 {
        return Err(BuildError::InvalidArg {
            op: "fft",
            reason: "input last-axis length must be >= 2",
        });
    }
    let out_last = last / 2 + 1;
    let mut out_dim: SmallVec<[usize; 4]> = in_schema.dim.clone();
    *out_dim.last_mut().unwrap() = out_last;
    let out_schema = ComponentSchema::new(PrimType::F64, &out_dim);
    let id = hash_id(op_tag::FFT, &[input.id()], |_| {});
    let parent_clock = input.parent_clock_id();
    let in_value_size = in_schema.size();
    let in_dtype = in_schema.prim_type;
    let in_total: usize = in_schema.dim.iter().product();
    let groups = in_total / last;
    let mut planner = FftPlanner::<f64>::new();
    let plan = planner.plan_fft_forward(last);
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); last];
            let mut scratch: Vec<u8> = Vec::with_capacity(out_schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_value_size {
                        continue;
                    }
                    scratch.clear();
                    for g in 0..groups {
                        let base = g * last;
                        for k in 0..last {
                            let v = read_f64_at(value, in_dtype, base + k);
                            buf[k] = Complex::new(v, 0.0);
                        }
                        plan.process(&mut buf);
                        for c in buf.iter().take(out_last) {
                            scratch.extend_from_slice(&c.norm().to_le_bytes());
                        }
                    }
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// L2 norm of a vector input — `sqrt(sum(x_i²))`. Reduces over every
/// element of the input shape, regardless of rank, into a rank-0 scalar.
/// Output dtype: integer inputs widen to `f64` (NumPy convention); float
/// inputs preserve their width.
///
/// `Bool` is rejected — the operation isn't meaningful and would always
/// reduce to a count of `true` elements.
pub fn magnitude(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    if in_schema.prim_type == PrimType::Bool {
        return Err(BuildError::UnsupportedDtype {
            op: "magnitude",
            dtype: in_schema.prim_type,
        });
    }
    let out_dtype = if is_int(in_schema.prim_type) {
        PrimType::F64
    } else {
        in_schema.prim_type
    };
    let out_schema = ComponentSchema::new(out_dtype, &[]);
    let id = hash_id(op_tag::MAGNITUDE, &[input.id()], |_| {});
    let parent_clock = input.parent_clock_id();
    let in_value_size = in_schema.size();
    let in_dtype = in_schema.prim_type;
    let n_elems = tensor::shape_elems(&in_schema.dim);
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let mut scratch: Vec<u8> = Vec::with_capacity(out_schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_value_size {
                        continue;
                    }
                    let mut sum: f64 = 0.0;
                    for i in 0..n_elems {
                        let v = read_f64_at(value, in_dtype, i);
                        sum += v * v;
                    }
                    scratch.clear();
                    write_f64_as(&mut scratch, out_dtype, sum.sqrt());
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// Pull element `i` (flat row-major index) from a value, returning a
/// rank-0 scalar of the input dtype. The inverse of `Pack` in the rank-1
/// case (`Pack` of N scalars → `[N]`; `Index(packed, i)` → scalar). For
/// higher-rank inputs the index walks the row-major buffer.
///
/// Build-time validation rejects out-of-range `i`. Bool inputs round-trip
/// through bytes unchanged (no special handling needed).
pub fn index(input: Arc<dyn DynamicNode>, i: usize) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    let n = tensor::shape_elems(&in_schema.dim);
    if i >= n {
        return Err(BuildError::InvalidArg {
            op: "index",
            reason: "index out of range",
        });
    }
    let elem_size = in_schema.prim_type.size();
    let in_value_size = in_schema.size();
    let out_schema = ComponentSchema::new(in_schema.prim_type, &[]);
    let id = hash_id(op_tag::INDEX, &[input.id()], |h| i.hash(h));
    let parent_clock = input.parent_clock_id();
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let off = i * elem_size;
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_value_size {
                        continue;
                    }
                    write_sample(&output, ts, &value[off..off + elem_size]);
                }
            }
        },
    ))
}

/// Comparison operator for [`threshold`]. Variants name the relation between
/// the input element and the threshold `k` that produces a `1.0` output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ThresholdOp {
    /// `input > k`
    Gt,
    /// `input >= k`
    Ge,
    /// `input < k`
    Lt,
    /// `input <= k`
    Le,
}

impl ThresholdOp {
    fn apply(self, x: f64, k: f64) -> f64 {
        let hit = match self {
            ThresholdOp::Gt => x > k,
            ThresholdOp::Ge => x >= k,
            ThresholdOp::Lt => x < k,
            ThresholdOp::Le => x <= k,
        };
        if hit { 1.0 } else { 0.0 }
    }
}

/// Threshold each element of `input` against `k` using comparison `op`.
/// Output: same shape as input, dtype `f64`, value `1.0` where the relation
/// holds and `0.0` otherwise. `k` is a `TypedScalar` so the inspector can
/// render it in the input's dtype, but the comparison runs in `f64` to keep
/// the per-element pipeline uniform with the rest of the dynamic graph.
pub fn threshold(
    input: Arc<dyn DynamicNode>,
    k: TypedScalar,
    op: ThresholdOp,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    if in_schema.prim_type == PrimType::Bool {
        return Err(BuildError::UnsupportedDtype {
            op: "threshold",
            dtype: in_schema.prim_type,
        });
    }
    let out_schema = ComponentSchema::new(PrimType::F64, &in_schema.dim);
    let kf = k.as_f64();
    map(
        op_tag::THRESHOLD,
        input,
        out_schema,
        |h| {
            k.hash(h);
            (op as u8).hash(h);
        },
        move |x| op.apply(x, kf),
    )
}

/// Sliding window over a stream. Emits `[size, ...input.shape]` on every input
/// tick, dtype preserved. The buffer is preloaded with zeros so emissions
/// start immediately; the leading zeros wash out after `size` ticks.
pub fn window(
    input: Arc<dyn DynamicNode>,
    size: usize,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if size == 0 {
        return Err(BuildError::InvalidArg {
            op: "window",
            reason: "size must be > 0",
        });
    }
    let in_schema = require_value(&input)?;
    let in_value_size = in_schema.size();
    let mut out_dim: SmallVec<[usize; 4]> = SmallVec::with_capacity(in_schema.dim.len() + 1);
    out_dim.push(size);
    out_dim.extend(in_schema.dim.iter().copied());
    let out_schema = ComponentSchema::new(in_schema.prim_type, &out_dim);
    let id = hash_id(op_tag::WINDOW, &[input.id()], |h| size.hash(h));
    let parent_clock = input.parent_clock_id();
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            // Ring of `size` slots, each `in_value_size` bytes.
            let mut buf: VecDeque<Vec<u8>> = (0..size).map(|_| vec![0u8; in_value_size]).collect();
            let mut scratch: Vec<u8> = Vec::with_capacity(out_schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_value_size {
                        continue;
                    }
                    let mut slot = buf.pop_front().expect("ring not empty");
                    slot.clear();
                    slot.extend_from_slice(value);
                    buf.push_back(slot);
                    scratch.clear();
                    for s in buf.iter() {
                        scratch.extend_from_slice(s);
                    }
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// First-difference: emits `x[n] - x[n-1]` element-wise, preserving input shape.
/// Output dtype is `f64` so signed differences are always representable. The
/// first sample is dropped (no prior to diff against); emission begins on the
/// second sample.
pub fn delta(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    if in_schema.prim_type == PrimType::Bool {
        return Err(BuildError::UnsupportedDtype {
            op: "delta",
            dtype: in_schema.prim_type,
        });
    }
    let in_dtype = in_schema.prim_type;
    let in_value_size = in_schema.size();
    let n_elems = tensor::shape_elems(&in_schema.dim);
    let out_schema = ComponentSchema::new(PrimType::F64, &in_schema.dim);
    let id = hash_id(op_tag::DELTA, &[input.id()], |_| {});
    let parent_clock = input.parent_clock_id();
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let mut prev: Vec<f64> = vec![0.0; n_elems];
            let mut curr: Vec<f64> = vec![0.0; n_elems];
            let mut scratch: Vec<u8> = Vec::with_capacity(out_schema.size());
            let mut have_prev = false;
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_value_size {
                        continue;
                    }
                    for i in 0..n_elems {
                        curr[i] = read_f64_at(value, in_dtype, i);
                    }
                    if have_prev {
                        scratch.clear();
                        for i in 0..n_elems {
                            scratch.extend_from_slice(&(curr[i] - prev[i]).to_le_bytes());
                        }
                        write_sample(&output, ts, &scratch);
                    }
                    prev.copy_from_slice(&curr);
                    have_prev = true;
                }
            }
        },
    ))
}

/// Time-difference: emits `(ts[n] - ts[n-1])` in seconds as an `f64` scalar.
/// Accepts any input — clock or value — since it only taps the timestamp
/// stream. The first sample is dropped (no prior timestamp).
pub fn delta_t(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let out_schema = ComponentSchema::new(PrimType::F64, &[]);
    let id = hash_id(op_tag::DELTA_T, &[input.id()], |_| {});
    let parent_clock = input.parent_clock_id();
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let mut prev = metor_proto::types::Timestamp(0);
            let mut have_prev = false;
            loop {
                let grant = reader.next().await;
                for (ts, _value) in grant.samples() {
                    if have_prev {
                        let dt = (ts.0 - prev.0) as f64 * 1e-6;
                        write_sample(&output, ts, &dt.to_le_bytes());
                    }
                    prev = ts;
                    have_prev = true;
                }
            }
        },
    ))
}
