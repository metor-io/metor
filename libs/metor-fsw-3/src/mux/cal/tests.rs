use super::*;

/// NIST forward-table points `(mV, °C)`, one or more per inverse range.
const K_POINTS: &[(f64, f64)] = &[
    (-3.554, -100.0),
    (0.000, 0.0),
    (4.096, 100.0),
    (20.644, 500.0),
    (41.276, 1000.0),
];
const J_POINTS: &[(f64, f64)] = &[(-4.633, -100.0), (16.327, 300.0), (57.953, 1000.0)];
const T_POINTS: &[(f64, f64)] = &[(-3.379, -100.0), (9.288, 200.0)];
const E_POINTS: &[(f64, f64)] = &[(-5.237, -100.0), (37.005, 500.0)];

const KINDS: [(TcKind, &[(f64, f64)]); 4] = [
    (TcKind::K, K_POINTS),
    (TcKind::J, J_POINTS),
    (TcKind::T, T_POINTS),
    (TcKind::E, E_POINTS),
];

fn eval(cal: &Cal, x: f64) -> f64 {
    cal.eval(x, &[])
}

fn tc(kind: TcKind, mv: f64) -> f64 {
    thermocouple(kind, mv / 1000.0)
}

fn record(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn test_linear_poly() {
    let cal = Cal::poly(&[-125.0, 250.0]).expect("finite");
    assert_eq!(eval(&cal, 0.5), 0.0);
    assert_eq!(eval(&cal, 2.5), 500.0);
    assert_eq!(eval(&cal, 4.5), 1000.0);
}

#[test]
fn test_cubic_poly() {
    let cal = Cal::poly(&[1.0, -2.0, 0.5, 3.0]).expect("finite");
    assert_eq!(eval(&cal, 2.0), 1.0 - 4.0 + 2.0 + 24.0);
    assert_eq!(eval(&cal, -1.0), 1.0 + 2.0 + 0.5 - 3.0);
}

#[test]
fn test_poly_sizes() {
    let one = Cal::poly(&[7.0]).expect("finite");
    assert_eq!(eval(&one, 123.0), 7.0);
    let five = Cal::poly(&[1.0, 1.0, 1.0, 1.0, 1.0]).expect("finite");
    assert!(matches!(&five, Cal::Poly(c) if c.spilled()));
    assert_eq!(eval(&five, 2.0), 31.0);
    assert!(matches!(&one, Cal::Poly(c) if !c.spilled()));
}

#[test]
fn test_table_interpolates() {
    let cal = Cal::table(&[(0.0, 0.0), (1.0, 10.0), (3.0, 30.0)]).expect("sorted");
    assert_eq!(eval(&cal, 0.5), 5.0);
    assert_eq!(eval(&cal, 1.0), 10.0);
    assert_eq!(eval(&cal, 2.0), 20.0);
}

#[test]
fn test_table_clamps() {
    let cal = Cal::table(&[(1.0, 10.0), (2.0, 40.0)]).expect("sorted");
    assert_eq!(eval(&cal, -5.0), 10.0);
    assert_eq!(eval(&cal, 9.0), 40.0);
    assert_eq!(eval(&cal, f64::INFINITY), 40.0);
    let one = Cal::table(&[(1.0, 3.0)]).expect("one point");
    assert_eq!(
        (eval(&one, 0.0), eval(&one, 1.0), eval(&one, 2.0)),
        (3.0, 3.0, 3.0)
    );
}

#[test]
fn test_thermocouple_nist_points() {
    for (kind, points) in KINDS {
        for &(mv, celsius) in points {
            let got = tc(kind, mv);
            assert!((got - celsius).abs() < 0.06, "{kind:?} {mv} mV: {got}");
        }
    }
}

#[test]
fn test_thermocouple_k_range_ends() {
    for (mv, celsius) in [(-5.891, -200.0), (20.644, 500.0), (54.886, 1372.0)] {
        let got = tc(TcKind::K, mv);
        assert!((got - celsius).abs() < 0.1, "{mv} mV: {got}");
    }
    let below = tc(TcKind::K, 20.643);
    let above = tc(TcKind::K, 20.645);
    assert!(below < above && above - below < 0.1);
}

#[test]
fn test_thermocouple_out_of_range() {
    for (kind, top) in [
        (TcKind::K, 54.886),
        (TcKind::J, 69.553),
        (TcKind::T, 20.872),
    ] {
        assert!(tc(kind, top + 0.01).is_nan(), "{kind:?}");
    }
    assert!(tc(TcKind::E, 76.4).is_nan());
    assert!(tc(TcKind::K, -6.0).is_nan());
}

#[test]
fn test_ratiometric() {
    let cal = Cal::ratiometric(8, (0.0, 0.002), (0.0, 500.0)).expect("finite");
    let mid = record(&[0.005, 5.0]);
    assert!((cal.eval(0.005, &mid) - 250.0).abs() < 1e-9);
    let zero = record(&[0.005, 0.0]);
    assert!(!cal.eval(0.005, &zero).is_finite());
    assert!(cal.eval(0.005, &mid[..12]).is_nan(), "a short record");
}

#[test]
fn test_nan_in_nan_out() {
    let cals = [
        Cal::poly(&[1.0]).expect("finite"),
        Cal::table(&[(0.0, 1.0), (1.0, 2.0)]).expect("sorted"),
        Cal::Thermocouple { kind: TcKind::K },
        Cal::ratiometric(0, (0.0, 1.0), (0.0, 1.0)).expect("finite"),
    ];
    let excitation = record(&[5.0]);
    for cal in &cals {
        assert!(cal.eval(f64::NAN, &excitation).is_nan(), "{cal:?}");
    }
}

#[test]
fn test_refusals() {
    assert_eq!(Cal::poly(&[]), None);
    assert_eq!(Cal::poly(&[1.0, f64::NAN]), None);
    assert_eq!(Cal::poly(&[f64::INFINITY]), None);
    assert_eq!(Cal::table(&[]), None);
    assert_eq!(Cal::table(&[(1.0, 0.0), (0.0, 1.0)]), None, "unsorted");
    assert_eq!(Cal::table(&[(1.0, 0.0), (1.0, 1.0)]), None, "a repeated x");
    assert_eq!(Cal::table(&[(0.0, 0.0), (f64::NAN, 1.0)]), None);
    assert_eq!(Cal::table(&[(0.0, f64::NAN)]), None);
    assert_eq!(Cal::ratiometric(0, (1.0, 1.0), (0.0, 1.0)), None);
    assert_eq!(Cal::ratiometric(0, (0.0, 1.0), (0.0, f64::NAN)), None);
}

#[test]
fn test_read_f64() {
    let bytes = record(&[1.5, -2.0]);
    assert_eq!(read_f64(&bytes, 0), Some(1.5));
    assert_eq!(read_f64(&bytes, 8), Some(-2.0));
    assert_eq!(read_f64(&bytes, 9), None);
    assert_eq!(read_f64(&bytes, u32::MAX), None);
}
