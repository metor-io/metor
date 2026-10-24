// The star-field kernels: `splat` adds each star sub-step's pixel-integrated
// PSF into a fixed-point accumulator, and `expose` reads it out through shot
// noise, read noise and the ADC. Both mirror `metor_star::render`.

struct Params {
    width: u32,
    height: u32,
    n_splats: u32,
    // The PSF cut K (px).
    k: u32,
    // 1/(√2σ).
    erf_scale: f32,
    background_e: f32,
    dark_e: f32,
    read_noise_e: f32,
    full_well_e: f32,
    gain_milli: f32,
    bias_dn: f32,
    max_dn: f32,
    seed_lo: u32,
    seed_hi: u32,
    // Non-zero when the sky table serves star-free, cold pixels.
    use_table: u32,
    _pad: u32,
}

struct Splat {
    u: f32,
    v: f32,
    e: f32,
    star: u32,
}

const MAX_TAPS: u32 = 13u;
const FIXED_SCALE: f32 = 256.0;
const TABLE_LEN: u32 = 168u;
const KNUTH_MAX: f32 = 30.0;
const KNUTH_DRAWS: u32 = 128u;
const NORMAL_DRAW: u32 = 128u;
const READ_DRAW: u32 = 129u;
const SQRT_2: f32 = 1.4142135;

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> splats: array<Splat>;
@group(0) @binding(2) var<storage, read_write> acc: array<atomic<u32>>;
// [0] counts the stars that put a count on the sensor; [1 + star] marks one.
@group(0) @binding(3) var<storage, read_write> hits: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read> hot: array<f32>;
@group(0) @binding(5) var<storage, read> sky_cdf: array<f32, TABLE_LEN>;
// Two 16-bit pixels per word, the lower pixel in the low half.
@group(0) @binding(6) var<storage, read_write> image: array<u32>;

// Abramowitz & Stegun 7.1.26.
fn erf_as(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.3275911 * abs(x));
    let poly = t * (0.2548296 + t * (-0.28449674 + t * (1.4214138 + t * (-1.453152 + t * 1.0614054))));
    let y = 1.0 - poly * exp(-x * x);
    return select(y, -y, x < 0.0);
}

// The normalised tap weights of a star at `x0`; returns the first pixel.
fn weights(x0: f32, w: ptr<function, array<f32, MAX_TAPS>>) -> i32 {
    let first = i32(floor(x0)) - i32(p.k);
    let taps = 2u * p.k + 1u;
    let start = erf_as((f32(first) - x0) * p.erf_scale);
    var lo = start;
    for (var i = 0u; i < taps; i++) {
        let hi = erf_as((f32(first + i32(i) + 1) - x0) * p.erf_scale);
        (*w)[i] = hi - lo;
        lo = hi;
    }
    let total = lo - start;
    for (var i = 0u; i < taps; i++) {
        (*w)[i] = (*w)[i] / total;
    }
    return first;
}

// Adds `n` to pixel `at`, leaving it at u32::MAX once any add wraps.
fn add_saturating(at: u32, n: u32) {
    let old = atomicAdd(&acc[at], n);
    if old + n < old {
        atomicMax(&acc[at], 0xffffffffu);
    }
}

@compute @workgroup_size(64)
fn splat(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= p.n_splats {
        return;
    }
    let s = splats[id.x];
    var wx: array<f32, MAX_TAPS>;
    var wy: array<f32, MAX_TAPS>;
    let x0 = weights(s.u, &wx);
    let y0 = weights(s.v, &wy);
    let taps = 2u * p.k + 1u;
    var any = false;
    for (var b = 0u; b < taps; b++) {
        let y = y0 + i32(b);
        if y < 0 || y >= i32(p.height) {
            continue;
        }
        for (var a = 0u; a < taps; a++) {
            let x = x0 + i32(a);
            if x < 0 || x >= i32(p.width) {
                continue;
            }
            let n = u32(floor(s.e * wx[a] * wy[b] * FIXED_SCALE + 0.5));
            if n > 0u {
                add_saturating(u32(y) * p.width + u32(x), n);
                any = true;
            }
        }
    }
    if any && atomicExchange(&hits[1u + s.star], 1u) == 0u {
        atomicAdd(&hits[0], 1u);
    }
}

// pcg4d (Jarzynski & Olano 2020).
fn mix(v: vec4<u32>) -> vec4<u32> {
    var m = v;
    m.x += m.y * m.w;
    m.y += m.z * m.x;
    m.z += m.x * m.y;
    m.w += m.y * m.z;
    return m;
}

fn pcg4(v: vec4<u32>) -> vec4<u32> {
    var m = mix(v * 1664525u + 1013904223u);
    m = m ^ (m >> vec4<u32>(16u));
    return mix(m);
}

// Uniform in (0, 1), keyed on (pixel, draw, seed).
fn uniform01(pixel: u32, draw: u32) -> f32 {
    let h = pcg4(vec4<u32>(pixel, draw, p.seed_lo, p.seed_hi)).x;
    return (f32(h >> 9u) + 0.5) * (1.0 / 8388608.0);
}

// Giles' single-precision erfinv.
fn erfinv(x: f32) -> f32 {
    var w = -log((1.0 - x) * (1.0 + x));
    var q: f32;
    if w < 5.0 {
        w = w - 2.5;
        q = 2.8102264e-8;
        q = 3.4327394e-7 + q * w;
        q = -3.5233877e-6 + q * w;
        q = -4.3915065e-6 + q * w;
        q = 2.1858087e-4 + q * w;
        q = -1.253725e-3 + q * w;
        q = -4.1776816e-3 + q * w;
        q = 0.24664073 + q * w;
        q = 1.5014094 + q * w;
    } else {
        w = sqrt(w) - 3.0;
        q = -2.0021426e-4;
        q = 1.0095056e-4 + q * w;
        q = 1.3493432e-3 + q * w;
        q = -3.6734284e-3 + q * w;
        q = 5.7395077e-3 + q * w;
        q = -7.6224613e-3 + q * w;
        q = 9.4388705e-3 + q * w;
        q = 1.001674 + q * w;
        q = 2.8329768 + q * w;
    }
    return q * x;
}

fn normal(pixel: u32, draw: u32) -> f32 {
    return SQRT_2 * erfinv(2.0 * uniform01(pixel, draw) - 1.0);
}

// Knuth below KNUTH_MAX, else a rounded normal floored at 0.
fn poisson(lambda: f32, pixel: u32) -> f32 {
    if lambda >= KNUTH_MAX {
        let x = lambda + sqrt(lambda) * normal(pixel, NORMAL_DRAW);
        return max(floor(x + 0.5), 0.0);
    }
    let limit = exp(-lambda);
    var q = 1.0;
    for (var k = 0u; k < KNUTH_DRAWS; k++) {
        q *= uniform01(pixel, k);
        if q <= limit {
            return f32(k);
        }
    }
    return f32(KNUTH_DRAWS);
}

// The first count whose CDF reaches `u`; TABLE_LEN past the table's end.
fn sky_sample(u: f32) -> f32 {
    var lo = 0u;
    var hi = TABLE_LEN;
    while lo < hi {
        let mid = (lo + hi) / 2u;
        if sky_cdf[mid] < u {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    return f32(lo);
}

fn pixel_dn(i: u32) -> u32 {
    let counts = atomicLoad(&acc[i]);
    let h = hot[i];
    var shot: f32;
    if p.use_table != 0u && counts == 0u && h == 1.0 {
        shot = sky_sample(uniform01(i, 0u));
    } else {
        let mean = f32(counts) / FIXED_SCALE + p.background_e + p.dark_e * h;
        shot = poisson(mean, i);
    }
    let e = min(shot, p.full_well_e) + p.read_noise_e * normal(i, READ_DRAW);
    let dn = floor(e * 1000.0 / p.gain_milli + p.bias_dn + 0.5);
    return u32(clamp(dn, 0.0, p.max_dn));
}

@compute @workgroup_size(256)
fn expose(@builtin(global_invocation_id) id: vec3<u32>) {
    let pair = id.x;
    if pair >= p.width * p.height / 2u {
        return;
    }
    image[pair] = pixel_dn(2u * pair) | (pixel_dn(2u * pair + 1u) << 16u);
}
