//! Q1: `@`, the generalized tensor product.
//!
//! The operator is Python's, so its rank rules are Python's: rank-1 against
//! rank-1 is the inner product, rank-2 is matrix multiplication, and above
//! that the leading dimensions broadcast while the last two contract. A rank-1
//! operand is promoted for the duration and the invented axis is dropped
//! again, which is why `m @ v` is a vector.
//!
//! Two oracles, for the reason Phase 0 recorded. nox's matrix product is the
//! reference where nox has one — it accumulates in order, so the comparison is
//! bit-for-bit — but nox's rank-1 `dot` reaches faer's fused multiply-add,
//! which core wasm cannot spell. The inner product is therefore checked
//! against the sequential sum the language defines it as, and
//! `tensors::a_contraction_is_not_fused` pins that divergence separately. For
//! batched shapes nox has nothing to offer at all, so the reference is written
//! out here.

use nox::{ArrayRepr, Const, ReprMonad, Tensor};

use super::tensors::{Driver, annotation_of, bits, evaluate, reaches_kernels};
use super::{build, reject};

/// Batched row-major `@`, written out: the definition the compiler is checked
/// against wherever nox has no answer.
fn reference(a: &[f64], sa: &[usize], b: &[f64], sb: &[usize]) -> Vec<f64> {
    let left = match sa {
        [n] => vec![1, *n],
        dims => dims.to_vec(),
    };
    let right = match sb {
        [n] => vec![*n, 1],
        dims => dims.to_vec(),
    };
    let (m, k) = (left[left.len() - 2], left[left.len() - 1]);
    let n = right[right.len() - 1];
    let (lead_a, lead_b) = (&left[..left.len() - 2], &right[..right.len() - 2]);

    let rank = lead_a.len().max(lead_b.len());
    let pad = |s: &[usize]| {
        let mut v = vec![1usize; rank - s.len()];
        v.extend_from_slice(s);
        v
    };
    let (pa, pb) = (pad(lead_a), pad(lead_b));
    let lead: Vec<usize> = (0..rank).map(|i| pa[i].max(pb[i])).collect();
    let strides = |s: &[usize]| {
        let mut r = vec![0usize; rank];
        let mut acc = 1;
        for i in (0..rank).rev() {
            r[i] = if s[i] == 1 { 0 } else { acc };
            acc *= s[i];
        }
        r
    };
    let (ta, tb) = (strides(&pa), strides(&pb));

    let mut index = vec![0usize; rank];
    let mut out = Vec::new();
    for _ in 0..lead.iter().product::<usize>().max(1) {
        let (ia, ib) = (0..rank).fold((0, 0), |(ia, ib), axis| {
            (ia + index[axis] * ta[axis], ib + index[axis] * tb[axis])
        });
        for row in 0..m {
            for col in 0..n {
                let mut acc = 0.0;
                for i in 0..k {
                    acc += a[ia * m * k + row * k + i] * b[ib * k * n + i * n + col];
                }
                out.push(acc);
            }
        }
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < lead[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    out
}

fn ramp(count: usize, scale: f64) -> Vec<f64> {
    (0..count).map(|i| (i as f64 + 1.0) * scale).collect()
}

/// The plan's literal example, and the reason `@` exists at all.
#[test]
fn a_vector_against_a_vector_is_the_inner_product() {
    let v = [1.0f64, 2.0, 3.0];
    let got = evaluate(
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> f64:\n    return a @ b\n",
        "f",
        &[&v, &v],
        1,
    );
    assert_eq!(got[0], 14.0);
}

/// Every rank combination the operator accepts, against the written-out
/// definition. Each shape appears once below the open-coding threshold and
/// once above it, so both emit paths answer for the same rule.
#[test]
fn every_rank_combination_contracts_the_last_two_axes() {
    let cases: &[(&[usize], &[usize], &[usize])] = &[
        (&[3], &[3], &[]),
        (&[2, 3], &[3], &[2]),
        (&[3], &[3, 2], &[2]),
        (&[2, 3], &[3, 4], &[2, 4]),
        // Batched: the leading dimensions ride along.
        (&[2, 2, 3], &[3, 4], &[2, 2, 4]),
        (&[2, 3], &[2, 3, 4], &[2, 2, 4]),
        (&[2, 2, 3], &[2, 3, 4], &[2, 2, 4]),
        (&[2, 2, 3], &[3], &[2, 2]),
        (&[3], &[2, 3, 4], &[2, 4]),
        // A leading extent of one stretches, the same way it does elementwise.
        (&[2, 1, 2, 3], &[1, 4, 3, 2], &[2, 4, 2, 2]),
        // Past `OPEN_CODE_MAX_OPS`, so the kernel answers instead.
        (&[16, 16], &[16, 16], &[16, 16]),
        (&[2, 8, 8], &[8, 8], &[2, 8, 8]),
        (&[200], &[200], &[]),
    ];

    for (sa, sb, out) in cases {
        let source = format!(
            "def f(a: {}, b: {}) -> {}:\n    return a @ b\n",
            annotation_of(sa),
            annotation_of(sb),
            annotation_of(out),
        );
        let a = ramp(sa.iter().product::<usize>(), 0.25);
        let b = ramp(sb.iter().product::<usize>(), -0.5);
        let count: usize = out.iter().product::<usize>().max(1);

        let got = evaluate(&source, "f", &[&a, &b], count);
        let want = reference(&a, sa, &b, sb);
        assert_eq!(bits(&got), bits(&want), "{sa:?} @ {sb:?} -> {out:?}");
    }
}

/// The choice between open coding and a kernel is a size trade, never a
/// semantic one: the same product both ways, and the small one reaches no
/// kernel at all.
#[test]
fn both_emit_paths_compute_the_same_product() {
    let small = build(
        "def f(a: Tensor[f64, (2, 3)], b: Tensor[f64, (3, 4)]) -> Tensor[f64, (2, 4)]:\n\
         \x20   return a @ b\n",
    );
    let large = build(
        "def f(a: Tensor[f64, (16, 16)], b: Tensor[f64, (16, 16)]) -> Tensor[f64, (16, 16)]:\n\
         \x20   return a @ b\n",
    );
    assert!(!reaches_kernels(&small), "a 2x3 @ 3x4 must be open-coded");
    assert!(reaches_kernels(&large), "a 16x16 product must call k_matmul");

    // The same numbers through both, by padding the small product out to the
    // large one's shape with zeros.
    let a = ramp(16 * 16, 0.125);
    let b = ramp(16 * 16, -0.0625);
    let got = evaluate(
        "def f(a: Tensor[f64, (16, 16)], b: Tensor[f64, (16, 16)]) -> Tensor[f64, (16, 16)]:\n\
         \x20   return a @ b\n",
        "f",
        &[&a, &b],
        256,
    );
    let want = reference(&a, &[16, 16], &b, &[16, 16]);
    assert_eq!(bits(&got), bits(&want));
}

/// nox is the oracle wherever nox has the operation, and it agrees bit for
/// bit — its matrix product accumulates in order, unlike its rank-1 `dot`.
#[test]
fn matrix_products_agree_with_nox() {
    let m = [1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0];
    let v = [1.0f64, 0.5, -2.0];
    let got = evaluate(
        "def mv(m: Tensor[f64, (2, 3)], v: Tensor[f64, 3]) -> Tensor[f64, 2]:\n\
         \x20   return m @ v\n",
        "mv",
        &[&m, &v],
        2,
    );
    let nm: Tensor<f64, (Const<2>, Const<3>), ArrayRepr> =
        [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]].into();
    let nv: Tensor<f64, Const<3>, ArrayRepr> = v.into();
    assert_eq!(
        bits(&got),
        bits(nm.dot(&nv).into_inner().view().buf()),
        "matrix times vector"
    );

    let a = [1.0f64, 2.0, 3.0, 4.0];
    let b = [5.0f64, 6.0, 7.0, 8.0];
    let got = evaluate(
        "def mm(a: Tensor[f64, (2, 2)], b: Tensor[f64, (2, 2)]) -> Tensor[f64, (2, 2)]:\n\
         \x20   return a @ b\n",
        "mm",
        &[&a, &b],
        4,
    );
    let na: Tensor<f64, (Const<2>, Const<2>), ArrayRepr> = [[1.0, 2.0], [3.0, 4.0]].into();
    let nb: Tensor<f64, (Const<2>, Const<2>), ArrayRepr> = [[5.0, 6.0], [7.0, 8.0]].into();
    assert_eq!(bits(&got), bits(na.dot(&nb).into_inner().view().buf()));
}

/// A vector on the left is promoted to a row and the row is dropped again, so
/// `v @ m` is a vector — and it is the transpose of `m_t @ v`.
#[test]
fn a_promoted_axis_leaves_with_the_promotion() {
    let v = [1.0f64, 2.0, 3.0];
    let m = [1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0];
    let got = evaluate(
        "def f(v: Tensor[f64, 3], m: Tensor[f64, (3, 2)]) -> Tensor[f64, 2]:\n\
         \x20   return v @ m\n",
        "f",
        &[&v, &m],
        2,
    );
    assert_eq!(got, vec![1.0 * 1.0 + 2.0 * 3.0 + 3.0 * 5.0, 1.0 * 2.0 + 2.0 * 4.0 + 3.0 * 6.0]);

    // And a chain keeps its shape: (2,3) @ (3,) is rank-1, so it contracts
    // again against another rank-1.
    let wasm = build(
        "def f(m: Tensor[f64, (2, 3)], v: Tensor[f64, 3], w: Tensor[f64, 2]) -> f64:\n\
         \x20   return (m @ v) @ w\n",
    );
    let mut driver = Driver::new(&wasm, "f");
    driver.write(0, &m);
    driver.write(1, &v);
    driver.write(2, &[1.0, -1.0]);
    driver.run().unwrap();
    let mv = [
        1.0 * 1.0 + 2.0 * 2.0 + 3.0 * 3.0,
        4.0 * 1.0 + 5.0 * 2.0 + 6.0 * 3.0,
    ];
    assert_eq!(driver.read(1)[0], mv[0] - mv[1]);
}

/// Every refusal names both shapes, because the operand that is wrong is not
/// knowable from one of them.
#[test]
fn refusals_name_both_shapes() {
    for (a, b, why) in [
        (&[3usize][..], &[4usize][..], "do not contract"),
        (&[2, 3][..], &[2, 3][..], "do not contract"),
        (&[2, 2, 3][..], &[3, 3, 4][..], "do not broadcast"),
        (&[2, 3, 3][..], &[4, 3, 3][..], "do not broadcast"),
    ] {
        let source = format!(
            "def f(a: {}, b: {}) -> f64:\n    return sum(a @ b)\n",
            annotation_of(a),
            annotation_of(b),
        );
        let text = format!("{}", reject(&source));
        assert!(text.contains(why), "{a:?} @ {b:?}: {text}");
        assert!(
            text.contains(&format!("{a:?}")) && text.contains(&format!("{b:?}")),
            "the diagnostic must name both shapes: {text}"
        );
    }

    // A scalar has no axes to contract, and says so with its type.
    let text = format!("{}", reject(
        "def f(a: Tensor[f64, 3], k: f64) -> f64:\n    return sum(a @ k)\n",
    ));
    assert!(text.contains("`@` contracts two tensors"), "{text}");
    assert!(text.contains("f64"), "{text}");
}

/// `dot(a, b)` was Phase 1's spelling; it is gone, and the diagnostic is the
/// migration.
#[test]
fn the_old_spelling_names_its_replacement() {
    let text = format!("{}", reject(
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> f64:\n    return dot(a, b)\n",
    ));
    assert!(text.contains("`a @ b`"), "{text}");
}
