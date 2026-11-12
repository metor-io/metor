//! NIST ITS-90 inverse functions: millivolts to °C.
//!
//! Each `d` is a `d0..dn` column, transcribed exactly from the "Inverse
//! coefficients for type X" tables of the NIST ITS-90 Thermocouple Database
//! (NIST SRD 60, `allcoeff.tab`; Burns et al., NIST Monograph 175, 1993).
//! `lo` and `hi` are each column's voltage range in mV.

use super::TcKind;

/// One subrange of an inverse function.
pub(super) struct Range {
    pub lo: f64,
    pub hi: f64,
    pub d: &'static [f64],
}

/// The subranges for `kind`, in increasing voltage.
pub(super) fn ranges(kind: TcKind) -> &'static [Range] {
    match kind {
        TcKind::K => K,
        TcKind::J => J,
        TcKind::T => T,
        TcKind::E => E,
    }
}

/// Type K.
pub(super) const K: &[Range] = &[
    Range {
        lo: -5.891,
        hi: 0.0,
        d: &[
            0.0000000E+00,
            2.5173462E+01,
            -1.1662878E+00,
            -1.0833638E+00,
            -8.9773540E-01,
            -3.7342377E-01,
            -8.6632643E-02,
            -1.0450598E-02,
            -5.1920577E-04,
            0.0000000E+00,
        ],
    },
    Range {
        lo: 0.0,
        hi: 20.644,
        d: &[
            0.000000E+00,
            2.508355E+01,
            7.860106E-02,
            -2.503131E-01,
            8.315270E-02,
            -1.228034E-02,
            9.804036E-04,
            -4.413030E-05,
            1.057734E-06,
            -1.052755E-08,
        ],
    },
    Range {
        lo: 20.644,
        hi: 54.886,
        d: &[
            -1.318058E+02,
            4.830222E+01,
            -1.646031E+00,
            5.464731E-02,
            -9.650715E-04,
            8.802193E-06,
            -3.110810E-08,
            0.000000E+00,
            0.000000E+00,
            0.000000E+00,
        ],
    },
];

/// Type J.
pub(super) const J: &[Range] = &[
    Range {
        lo: -8.095,
        hi: 0.0,
        d: &[
            0.0000000E+00,
            1.9528268E+01,
            -1.2286185E+00,
            -1.0752178E+00,
            -5.9086933E-01,
            -1.7256713E-01,
            -2.8131513E-02,
            -2.3963370E-03,
            -8.3823321E-05,
        ],
    },
    Range {
        lo: 0.0,
        hi: 42.919,
        d: &[
            0.000000E+00,
            1.978425E+01,
            -2.001204E-01,
            1.036969E-02,
            -2.549687E-04,
            3.585153E-06,
            -5.344285E-08,
            5.099890E-10,
            0.000000E+00,
        ],
    },
    Range {
        lo: 42.919,
        hi: 69.553,
        d: &[
            -3.11358187E+03,
            3.00543684E+02,
            -9.94773230E+00,
            1.70276630E-01,
            -1.43033468E-03,
            4.73886084E-06,
            0.00000000E+00,
            0.00000000E+00,
            0.00000000E+00,
        ],
    },
];

/// Type T.
pub(super) const T: &[Range] = &[
    Range {
        lo: -5.603,
        hi: 0.0,
        d: &[
            0.0000000E+00,
            2.5949192E+01,
            -2.1316967E-01,
            7.9018692E-01,
            4.2527777E-01,
            1.3304473E-01,
            2.0241446E-02,
            1.2668171E-03,
        ],
    },
    Range {
        lo: 0.0,
        hi: 20.872,
        d: &[
            0.000000E+00,
            2.592800E+01,
            -7.602961E-01,
            4.637791E-02,
            -2.165394E-03,
            6.048144E-05,
            -7.293422E-07,
            0.000000E+00,
        ],
    },
];

/// Type E.
pub(super) const E: &[Range] = &[
    Range {
        lo: -8.825,
        hi: 0.0,
        d: &[
            0.0000000E+00,
            1.6977288E+01,
            -4.3514970E-01,
            -1.5859697E-01,
            -9.2502871E-02,
            -2.6084314E-02,
            -4.1360199E-03,
            -3.4034030E-04,
            -1.1564890E-05,
            0.0000000E+00,
        ],
    },
    Range {
        lo: 0.0,
        hi: 76.373,
        d: &[
            0.0000000E+00,
            1.7057035E+01,
            -2.3301759E-01,
            6.5435585E-03,
            -7.3562749E-05,
            -1.7896001E-06,
            8.4036165E-08,
            -1.3735879E-09,
            1.0629823E-11,
            -3.2447087E-14,
        ],
    },
];
