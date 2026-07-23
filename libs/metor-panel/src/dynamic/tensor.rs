//! Typed-tensor + broadcast helpers shared across dynamic-node ops.
//!
//! Wire frames in this graph are raw `[Timestamp][value bytes]` over a
//! `Disruptor`; values are described out-of-band by a
//! [`metor_db::ComponentSchema`] (`PrimType` + N-dim `dim`). Ops decode bytes
//! through the helpers in this module so each op doesn't reimplement
//! per-PrimType reads, NumPy-style broadcasting, and dtype promotion.
//!
//! Compute model: the per-element pipeline reads inputs as `f64`, applies the
//! op in `f64`, then casts back to the output dtype. This is plenty fast for
//! the visualization-rate plumbing the panel uses and lets every op share one
//! inner loop. The trade-off is precision loss for `u64`/`i64` values above
//! 2^53; revisit with monomorphized loops if real workloads need it.

use std::hash::{Hash, Hasher};

use metor_proto::types::PrimType;
use smallvec::SmallVec;

use crate::dynamic::node::BuildError;

/// Typed scalar — used both for op args (`Scale.k`, `Constant.value`) and as
/// the boundary type when reading/writing element values. Mirrors `PrimType`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TypedScalar {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    Bool(bool),
    F32(f32),
    F64(f64),
}

impl TypedScalar {
    pub fn dtype(self) -> PrimType {
        match self {
            Self::U8(_) => PrimType::U8,
            Self::U16(_) => PrimType::U16,
            Self::U32(_) => PrimType::U32,
            Self::U64(_) => PrimType::U64,
            Self::I8(_) => PrimType::I8,
            Self::I16(_) => PrimType::I16,
            Self::I32(_) => PrimType::I32,
            Self::I64(_) => PrimType::I64,
            Self::Bool(_) => PrimType::Bool,
            Self::F32(_) => PrimType::F32,
            Self::F64(_) => PrimType::F64,
        }
    }

    /// Numeric cast via `f64` round-trip — `as` semantics. Bool maps to 0/1.
    pub fn cast(self, to: PrimType) -> Self {
        if self.dtype() == to {
            return self;
        }
        Self::from_f64(self.as_f64(), to)
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Self::U8(v) => v as f64,
            Self::U16(v) => v as f64,
            Self::U32(v) => v as f64,
            Self::U64(v) => v as f64,
            Self::I8(v) => v as f64,
            Self::I16(v) => v as f64,
            Self::I32(v) => v as f64,
            Self::I64(v) => v as f64,
            Self::Bool(v) => v as u8 as f64,
            Self::F32(v) => v as f64,
            Self::F64(v) => v,
        }
    }

    pub fn from_f64(v: f64, dtype: PrimType) -> Self {
        match dtype {
            PrimType::U8 => Self::U8(v as u8),
            PrimType::U16 => Self::U16(v as u16),
            PrimType::U32 => Self::U32(v as u32),
            PrimType::U64 => Self::U64(v as u64),
            PrimType::I8 => Self::I8(v as i8),
            PrimType::I16 => Self::I16(v as i16),
            PrimType::I32 => Self::I32(v as i32),
            PrimType::I64 => Self::I64(v as i64),
            PrimType::Bool => Self::Bool(v != 0.0),
            PrimType::F32 => Self::F32(v as f32),
            PrimType::F64 => Self::F64(v),
        }
    }

    pub fn write_le_bytes(self, out: &mut Vec<u8>) {
        match self {
            Self::U8(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::U16(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::U32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::U64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::I8(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::I16(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::I32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::I64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::Bool(v) => out.push(v as u8),
            Self::F32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::F64(v) => out.extend_from_slice(&v.to_le_bytes()),
        }
    }
}

impl Hash for TypedScalar {
    fn hash<H: Hasher>(&self, h: &mut H) {
        std::mem::discriminant(self).hash(h);
        match *self {
            Self::U8(v) => v.hash(h),
            Self::U16(v) => v.hash(h),
            Self::U32(v) => v.hash(h),
            Self::U64(v) => v.hash(h),
            Self::I8(v) => v.hash(h),
            Self::I16(v) => v.hash(h),
            Self::I32(v) => v.hash(h),
            Self::I64(v) => v.hash(h),
            Self::Bool(v) => v.hash(h),
            Self::F32(v) => v.to_bits().hash(h),
            Self::F64(v) => v.to_bits().hash(h),
        }
    }
}

pub fn is_float(t: PrimType) -> bool {
    matches!(t, PrimType::F32 | PrimType::F64)
}

pub fn is_signed_int(t: PrimType) -> bool {
    matches!(
        t,
        PrimType::I8 | PrimType::I16 | PrimType::I32 | PrimType::I64
    )
}

pub fn is_unsigned_int(t: PrimType) -> bool {
    matches!(
        t,
        PrimType::U8 | PrimType::U16 | PrimType::U32 | PrimType::U64
    )
}

pub fn is_int(t: PrimType) -> bool {
    is_signed_int(t) || is_unsigned_int(t) || matches!(t, PrimType::Bool)
}

fn bit_width(t: PrimType) -> u8 {
    match t {
        PrimType::Bool | PrimType::U8 | PrimType::I8 => 8,
        PrimType::U16 | PrimType::I16 => 16,
        PrimType::U32 | PrimType::I32 | PrimType::F32 => 32,
        PrimType::U64 | PrimType::I64 | PrimType::F64 => 64,
    }
}

/// NumPy-style dtype promotion narrowed to the `PrimType` set:
/// - same → that type
/// - bool + X → X
/// - any float + any → float (f64 wins; f32 + i32+ → f64; else f32)
/// - same-signedness int + int → wider
/// - mixed-signedness int + int → smallest signed type that holds both ranges
///   (u64+iN → f64 since i128 isn't in the set)
pub fn promote(a: PrimType, b: PrimType) -> PrimType {
    use PrimType::*;
    if a == b {
        return a;
    }
    if a == Bool {
        return b;
    }
    if b == Bool {
        return a;
    }
    if is_float(a) || is_float(b) {
        if a == F64 || b == F64 {
            return F64;
        }
        // F32 + intN: NumPy promotes to F64 when the int needs >=24 bits to
        // represent without loss (i.e. i32/u32/i64/u64). Match that.
        let other = if is_float(a) { b } else { a };
        return match other {
            U16 | I16 | U8 | I8 | Bool => F32,
            _ => F64,
        };
    }
    // Both ints (Bool already handled).
    let aw = bit_width(a);
    let bw = bit_width(b);
    let asg = is_signed_int(a);
    let bsg = is_signed_int(b);
    if asg == bsg {
        return if aw >= bw { a } else { b };
    }
    // Mixed signedness — need a signed type that holds the unsigned's range.
    let unsigned_w = if asg { bw } else { aw };
    let signed_w = if asg { aw } else { bw };
    if signed_w > unsigned_w {
        return if asg { a } else { b };
    }
    // Need wider signed than `unsigned_w`.
    match unsigned_w {
        8 => I16,
        16 => I32,
        32 => I64,
        64 => F64,
        _ => F64,
    }
}

pub fn promote_many<I: IntoIterator<Item = PrimType>>(types: I) -> PrimType {
    let mut iter = types.into_iter();
    let mut acc = iter.next().expect("promote_many: empty");
    for t in iter {
        acc = promote(acc, t);
    }
    acc
}

/// NumPy broadcast: align right, each pair must equal or one must be 1.
pub fn broadcast_shape(a: &[usize], b: &[usize]) -> Result<SmallVec<[usize; 4]>, BuildError> {
    let n = a.len().max(b.len());
    let mut out: SmallVec<[usize; 4]> = SmallVec::with_capacity(n);
    for i in 0..n {
        // Right-aligned: index from the back.
        let av = a.len().checked_sub(i + 1).map(|j| a[j]).unwrap_or(1);
        let bv = b.len().checked_sub(i + 1).map(|j| b[j]).unwrap_or(1);
        let v = match (av, bv) {
            (x, y) if x == y => x,
            (1, y) => y,
            (x, 1) => x,
            _ => {
                return Err(BuildError::BroadcastMismatch {
                    a: SmallVec::from_slice(a),
                    b: SmallVec::from_slice(b),
                });
            }
        };
        out.insert(0, v);
    }
    Ok(out)
}

pub fn broadcast_shape_many<I>(shapes: I) -> Result<SmallVec<[usize; 4]>, BuildError>
where
    I: IntoIterator,
    I::Item: AsRef<[usize]>,
{
    let mut iter = shapes.into_iter();
    let first = iter.next().expect("broadcast_shape_many: empty");
    let mut acc: SmallVec<[usize; 4]> = SmallVec::from_slice(first.as_ref());
    for s in iter {
        acc = broadcast_shape(&acc, s.as_ref())?;
    }
    Ok(acc)
}

/// Walks output indices in row-major order over `dst_shape`, yielding the
/// *element index* into a row-major buffer of `src_shape`. Broadcast-1 axes
/// (and missing leading axes) contribute stride 0, so they replay the same
/// source element across the output axis.
pub struct BroadcastIter {
    dst_shape: SmallVec<[usize; 4]>,
    /// Element strides in `src` per dst-axis (0 for broadcast/missing axes).
    src_strides: SmallVec<[isize; 4]>,
    /// Multidim index into `dst_shape`.
    coord: SmallVec<[usize; 4]>,
    /// Current source element index (kept in sync with `coord`).
    src_offset: isize,
    total: usize,
    yielded: usize,
}

impl BroadcastIter {
    pub fn new(src_shape: &[usize], dst_shape: &[usize]) -> Self {
        let n = dst_shape.len();
        let pad = n - src_shape.len();
        // Compute src element strides in row-major order over `src_shape`,
        // collapsing size-1 axes to stride 0 (broadcast).
        let mut src_strides_real: SmallVec<[isize; 4]> = SmallVec::from_elem(0, src_shape.len());
        let mut s: isize = 1;
        for i in (0..src_shape.len()).rev() {
            src_strides_real[i] = if src_shape[i] == 1 { 0 } else { s };
            s *= src_shape[i] as isize;
        }
        let mut src_strides: SmallVec<[isize; 4]> = SmallVec::from_elem(0, n);
        for i in 0..src_shape.len() {
            src_strides[pad + i] = src_strides_real[i];
        }
        let coord: SmallVec<[usize; 4]> = SmallVec::from_elem(0, n);
        let total: usize = if dst_shape.is_empty() {
            1
        } else {
            dst_shape.iter().product()
        };
        Self {
            dst_shape: SmallVec::from_slice(dst_shape),
            src_strides,
            coord,
            src_offset: 0,
            total,
            yielded: 0,
        }
    }
}

impl Iterator for BroadcastIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yielded >= self.total {
            return None;
        }
        let off = self.src_offset as usize;
        // Advance to the next dst index (rightmost axis fastest), keeping
        // src_offset in sync incrementally.
        let n = self.coord.len();
        for i in (0..n).rev() {
            self.coord[i] += 1;
            self.src_offset += self.src_strides[i];
            if self.coord[i] < self.dst_shape[i] {
                break;
            }
            // Carry: undo this axis's contribution and roll over.
            self.src_offset -= self.src_strides[i] * self.dst_shape[i] as isize;
            self.coord[i] = 0;
            // If this was axis 0 we're done after this yield.
        }
        self.yielded += 1;
        Some(off)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.total - self.yielded;
        (rem, Some(rem))
    }
}

/// Read element `idx` of a row-major buffer of `dtype` as `f64`. Caller is
/// responsible for ensuring `idx` is in range.
pub fn read_f64_at(buf: &[u8], dtype: PrimType, idx: usize) -> f64 {
    let sz = dtype.size();
    let off = idx * sz;
    let chunk = &buf[off..off + sz];
    match dtype {
        PrimType::U8 => chunk[0] as f64,
        PrimType::U16 => u16::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::U32 => u32::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::U64 => u64::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I8 => i8::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I16 => i16::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I32 => i32::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I64 => i64::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::Bool => (chunk[0] != 0) as u8 as f64,
        PrimType::F32 => f32::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::F64 => f64::from_le_bytes(chunk.try_into().unwrap()),
    }
}

/// Cast `value` to `dtype` and append its little-endian bytes to `out`.
pub fn write_f64_as(out: &mut Vec<u8>, dtype: PrimType, value: f64) {
    match dtype {
        PrimType::U8 => out.push(value as u8),
        PrimType::U16 => out.extend_from_slice(&(value as u16).to_le_bytes()),
        PrimType::U32 => out.extend_from_slice(&(value as u32).to_le_bytes()),
        PrimType::U64 => out.extend_from_slice(&(value as u64).to_le_bytes()),
        PrimType::I8 => out.extend_from_slice(&(value as i8).to_le_bytes()),
        PrimType::I16 => out.extend_from_slice(&(value as i16).to_le_bytes()),
        PrimType::I32 => out.extend_from_slice(&(value as i32).to_le_bytes()),
        PrimType::I64 => out.extend_from_slice(&(value as i64).to_le_bytes()),
        PrimType::Bool => out.push((value != 0.0) as u8),
        PrimType::F32 => out.extend_from_slice(&(value as f32).to_le_bytes()),
        PrimType::F64 => out.extend_from_slice(&value.to_le_bytes()),
    }
}

/// Cast `value` to `dtype` and write its little-endian bytes at element index
/// `idx` of a row-major `out` buffer. Mirror of [`read_f64_at`].
pub fn write_f64_at(out: &mut [u8], dtype: PrimType, idx: usize, value: f64) {
    let sz = dtype.size();
    let off = idx * sz;
    let dst = &mut out[off..off + sz];
    match dtype {
        PrimType::U8 => dst[0] = value as u8,
        PrimType::U16 => dst.copy_from_slice(&(value as u16).to_le_bytes()),
        PrimType::U32 => dst.copy_from_slice(&(value as u32).to_le_bytes()),
        PrimType::U64 => dst.copy_from_slice(&(value as u64).to_le_bytes()),
        PrimType::I8 => dst.copy_from_slice(&(value as i8).to_le_bytes()),
        PrimType::I16 => dst.copy_from_slice(&(value as i16).to_le_bytes()),
        PrimType::I32 => dst.copy_from_slice(&(value as i32).to_le_bytes()),
        PrimType::I64 => dst.copy_from_slice(&(value as i64).to_le_bytes()),
        PrimType::Bool => dst[0] = (value != 0.0) as u8,
        PrimType::F32 => dst.copy_from_slice(&(value as f32).to_le_bytes()),
        PrimType::F64 => dst.copy_from_slice(&value.to_le_bytes()),
    }
}

/// Element count from a shape (scalar = 1).
pub fn shape_elems(shape: &[usize]) -> usize {
    if shape.is_empty() {
        1
    } else {
        shape.iter().product()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_basic() {
        use PrimType::*;
        assert_eq!(promote(F32, F64), F64);
        assert_eq!(promote(I32, F32), F64);
        assert_eq!(promote(I16, F32), F32);
        assert_eq!(promote(Bool, I32), I32);
        assert_eq!(promote(U8, I8), I16);
        assert_eq!(promote(U32, I32), I64);
        assert_eq!(promote(I64, U64), F64);
        assert_eq!(promote(U8, U16), U16);
        assert_eq!(promote(F32, F32), F32);
    }

    #[test]
    fn broadcast_shape_basic() {
        let s = broadcast_shape(&[3], &[]).unwrap();
        assert_eq!(s.as_slice(), &[3]);
        let s = broadcast_shape(&[1, 3], &[4, 1]).unwrap();
        assert_eq!(s.as_slice(), &[4, 3]);
        let s = broadcast_shape(&[2, 1, 3], &[3]).unwrap();
        assert_eq!(s.as_slice(), &[2, 1, 3]);
        assert!(broadcast_shape(&[3], &[4]).is_err());
    }

    #[test]
    fn broadcast_iter_replicates_scalar() {
        // Scalar src → 3 dst: source idx 0, 0, 0
        let it = BroadcastIter::new(&[], &[3]);
        let xs: Vec<_> = it.collect();
        assert_eq!(xs, vec![0, 0, 0]);
    }

    #[test]
    fn broadcast_iter_outer_product_shape() {
        // src shape [1, 3] -> dst [4, 3]; should yield [0,1,2, 0,1,2, 0,1,2, 0,1,2]
        let it = BroadcastIter::new(&[1, 3], &[4, 3]);
        let xs: Vec<_> = it.collect();
        assert_eq!(xs, vec![0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2]);
        // src shape [4, 1] -> dst [4, 3]; should yield [0,0,0, 1,1,1, 2,2,2, 3,3,3]
        let it = BroadcastIter::new(&[4, 1], &[4, 3]);
        let xs: Vec<_> = it.collect();
        assert_eq!(xs, vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3]);
    }

    #[test]
    fn broadcast_iter_passthrough() {
        let it = BroadcastIter::new(&[2, 3], &[2, 3]);
        let xs: Vec<_> = it.collect();
        assert_eq!(xs, vec![0, 1, 2, 3, 4, 5]);
    }
}
