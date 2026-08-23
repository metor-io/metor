//! Elementwise and contraction kernels over `nox_array::ArrayView`.
//!
//! Every kernel takes byte offsets into linear memory and writes through them.
//! Shapes ride along because the compiler knows them: it lays a descriptor into
//! the arena as a data segment and a call site is `i32.const desc; call $k_add`.
//!
//! Broadcasting follows nox's runtime rule — shapes right-aligned, an axis of
//! extent 1 stretched by giving it stride 0, missing leading axes treated as 1.

use nox_array::ArrayView;

/// Longest shape a kernel accepts, matching the rank nox's type layer covers.
pub const MAX_RANK: usize = 4;

/// One elementwise call site: three buffers and the shapes they carry.
#[repr(C)]
pub struct EwDesc {
    pub a: u32,
    pub b: u32,
    pub out: u32,
    pub rank: u32,
    pub a_shape: [u32; MAX_RANK],
    pub b_shape: [u32; MAX_RANK],
    pub out_shape: [u32; MAX_RANK],
}

fn shape_of(raw: &[u32; MAX_RANK], rank: usize, into: &mut [usize; MAX_RANK]) -> usize {
    for i in 0..rank {
        into[i] = raw[i] as usize;
    }
    rank
}

/// Strides that walk `src` while iterating `dst`, right-aligned, extent-1 axes
/// pinned to stride 0.
fn broadcast_strides(src: &[usize], dst_rank: usize) -> [usize; MAX_RANK] {
    let mut row = [0usize; MAX_RANK];
    let mut acc = 1usize;
    for i in (0..src.len()).rev() {
        row[i] = if src[i] == 1 { 0 } else { acc };
        acc *= src[i];
    }
    let mut out = [0usize; MAX_RANK];
    let lead = dst_rank - src.len();
    out[lead..dst_rank].copy_from_slice(&row[..src.len()]);
    out
}

fn elementwise(desc: &EwDesc, op: fn(f64, f64) -> f64) {
    let rank = desc.rank as usize;
    let (mut a_raw, mut b_raw, mut o_raw) = ([0usize; MAX_RANK], [0usize; MAX_RANK], [0usize; MAX_RANK]);
    let a_rank = shape_of(&desc.a_shape, rank, &mut a_raw);
    let b_rank = shape_of(&desc.b_shape, rank, &mut b_raw);
    let o_rank = shape_of(&desc.out_shape, rank, &mut o_raw);
    let a_shape = &a_raw[..a_rank];
    let b_shape = &b_raw[..b_rank];
    let out_shape = &o_raw[..o_rank];

    let count: usize = out_shape.iter().product::<usize>().max(1);
    let a = unsafe { core::slice::from_raw_parts(desc.a as *const f64, a_shape.iter().product::<usize>().max(1)) };
    let b = unsafe { core::slice::from_raw_parts(desc.b as *const f64, b_shape.iter().product::<usize>().max(1)) };
    let out = unsafe { core::slice::from_raw_parts_mut(desc.out as *mut f64, count) };

    let av = ArrayView::from_buf_shape_unchecked(a, a_shape);
    let bv = ArrayView::from_buf_shape_unchecked(b, b_shape);
    let a_stride = broadcast_strides(av.shape(), o_rank);
    let b_stride = broadcast_strides(bv.shape(), o_rank);

    let mut index = [0usize; MAX_RANK];
    for slot in out.iter_mut() {
        let mut ai = 0usize;
        let mut bi = 0usize;
        for axis in 0..o_rank {
            ai += index[axis] * a_stride[axis];
            bi += index[axis] * b_stride[axis];
        }
        *slot = op(av.buf()[ai], bv.buf()[bi]);
        for axis in (0..o_rank).rev() {
            index[axis] += 1;
            if index[axis] < out_shape[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
}

macro_rules! elementwise_kernel {
    ($($name:ident => $op:expr),* $(,)?) => {
        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn $name(desc: u32) {
                elementwise(unsafe { &*(desc as *const EwDesc) }, $op);
            }
        )*
    };
}

elementwise_kernel! {
    k_add => |a, b| a + b,
    k_sub => |a, b| a - b,
    k_mul => |a, b| a * b,
    k_div => |a, b| a / b,
    k_pow => libm::pow,
    k_atan2 => libm::atan2,
}

/// Negate `n` elements from `a` into `out`.
#[unsafe(no_mangle)]
pub extern "C" fn k_neg(a: u32, out: u32, n: u32) {
    let src = unsafe { core::slice::from_raw_parts(a as *const f64, n as usize) };
    let dst = unsafe { core::slice::from_raw_parts_mut(out as *mut f64, n as usize) };
    let view = ArrayView::from_buf_shape_unchecked(src, &[]);
    for (slot, x) in dst.iter_mut().zip(view.buf().iter()) {
        *slot = -*x;
    }
}

/// Inner product of two length-`n` vectors.
#[unsafe(no_mangle)]
pub extern "C" fn k_dot(a: u32, b: u32, n: u32) -> f64 {
    let x = unsafe { core::slice::from_raw_parts(a as *const f64, n as usize) };
    let y = unsafe { core::slice::from_raw_parts(b as *const f64, n as usize) };
    let shape = [n as usize];
    let xv = ArrayView::from_buf_shape_unchecked(x, &shape);
    let yv = ArrayView::from_buf_shape_unchecked(y, &shape);
    let mut acc = 0.0;
    for (p, q) in xv.buf().iter().zip(yv.buf().iter()) {
        acc += *p * *q;
    }
    acc
}

/// Row-major `(m, k) @ (k, n)` into `out`.
#[unsafe(no_mangle)]
pub extern "C" fn k_matmul(a: u32, b: u32, out: u32, m: u32, k: u32, n: u32) {
    let (m, k, n) = (m as usize, k as usize, n as usize);
    let x = unsafe { core::slice::from_raw_parts(a as *const f64, m * k) };
    let y = unsafe { core::slice::from_raw_parts(b as *const f64, k * n) };
    let z = unsafe { core::slice::from_raw_parts_mut(out as *mut f64, m * n) };
    for row in 0..m {
        for col in 0..n {
            let mut acc = 0.0;
            for i in 0..k {
                acc += x[row * k + i] * y[i * n + col];
            }
            z[row * n + col] = acc;
        }
    }
}

/// Sum of `n` elements — the reduction `dot` cannot express.
#[unsafe(no_mangle)]
pub extern "C" fn k_sum(a: u32, n: u32) -> f64 {
    let x = unsafe { core::slice::from_raw_parts(a as *const f64, n as usize) };
    let view = ArrayView::from_buf_shape_unchecked(x, &[]);
    let mut acc = 0.0;
    for v in view.buf() {
        acc += *v;
    }
    acc
}
