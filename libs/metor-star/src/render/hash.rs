//! A stateless PCG hash (Jarzynski & Olano, JCGT 9(3) 2020), the same integer
//! steps as the WGSL port, so each pixel's draws are reproducible in any
//! order.

use core::f32::consts::SQRT_2;

/// `pcg4d`: four 32-bit words in, four well-mixed words out.
pub fn pcg4(v: [u32; 4]) -> [u32; 4] {
    let mut v = v.map(|x| x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223));
    mix(&mut v);
    v = v.map(|x| x ^ (x >> 16));
    mix(&mut v);
    v
}

fn mix(v: &mut [u32; 4]) {
    v[0] = v[0].wrapping_add(v[1].wrapping_mul(v[3]));
    v[1] = v[1].wrapping_add(v[2].wrapping_mul(v[0]));
    v[2] = v[2].wrapping_add(v[0].wrapping_mul(v[1]));
    v[3] = v[3].wrapping_add(v[1].wrapping_mul(v[2]));
}

/// Uniform in (0, 1), keyed on `(pixel, draw, seed)`. 23 bits keep every
/// value exact in `f32`.
pub fn uniform(pixel: u32, draw: u32, seed: u64) -> f32 {
    let h = pcg4([pixel, draw, seed as u32, (seed >> 32) as u32])[0];
    ((h >> 9) as f32 + 0.5) * (1.0 / 8_388_608.0)
}

/// Standard normal from the one draw `draw`, by inverting its CDF:
/// `√2 · erfinv(2u − 1)`.
pub fn normal(pixel: u32, draw: u32, seed: u64) -> f32 {
    SQRT_2 * erfinv(2.0 * uniform(pixel, draw, seed) - 1.0)
}

/// Giles' single-precision `erfinv` ("Approximating the erfinv function",
/// GPU Computing Gems, 2011) for `x` in (−1, 1): one `logf`, no trig.
pub fn erfinv(x: f32) -> f32 {
    const CENTRAL: [f32; 9] = [
        2.810_226_4e-8,
        3.432_739_4e-7,
        -3.523_387_7e-6,
        -4.391_506_5e-6,
        2.185_808_7e-4,
        -1.253_725e-3,
        -4.177_681_6e-3,
        0.246_640_73,
        1.501_409_4,
    ];
    const TAIL: [f32; 9] = [
        -2.002_142_6e-4,
        1.009_505_6e-4,
        1.349_343_2e-3,
        -3.673_428_4e-3,
        5.739_507_7e-3,
        -7.622_461_3e-3,
        9.438_870_5e-3,
        1.001_674,
        2.832_976_8,
    ];
    let w = -libm::logf((1.0 - x) * (1.0 + x));
    let (coeffs, w) = if w < 5.0 {
        (&CENTRAL, w - 2.5)
    } else {
        (&TAIL, libm::sqrtf(w) - 3.0)
    };
    let p = coeffs[1..].iter().fold(coeffs[0], |p, &c| c + p * w);
    p * x
}
