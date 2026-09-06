// Tonemap pass: stretch an R32Float intensity grid over the plot area and
// map each cell through a 256-entry colormap LUT.
//
// The field is read with `textureLoad` rather than a sampler: R32Float is
// non-filterable, and the hard per-cell edges are what makes a spectrogram
// legible when one column is one sample.

struct Tonemap {
    lo: f32,
    hi: f32,
    gain: f32,
    scale_mode: u32,
    row_lo: f32,
    row_hi: f32,
    cols: u32,
    rows: u32,
}

@group(0) @binding(0) var<uniform> tonemap: Tonemap;
@group(1) @binding(0) var field: texture_2d<f32>;
@group(1) @binding(1) var lut: texture_2d<f32>;
@group(1) @binding(2) var lut_sampler: sampler;

// Matches `EMPTY_INTENSITY` on the CPU side. Compared with a slack threshold
// because fast-math reassociation makes an exact equality test unreliable.
const EMPTY_THRESHOLD: f32 = -1.0e38;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    // Normalized plot position: x runs left to right, y runs bottom to top so
    // it shares the data axis's direction.
    @location(0) uv: vec2<f32>,
}

@vertex
fn vertex(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Fullscreen triangle: three vertices whose [0,2]² footprint covers the
    // whole target, with no vertex buffer.
    let p = vec2<f32>(
        f32((vertex_index << 1u) & 2u),
        f32(vertex_index & 2u),
    );
    var out: VertexOutput;
    out.clip_position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
    out.uv = p;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let col = i32(floor(in.uv.x * f32(tonemap.cols)));
    if col < 0 || col >= i32(tonemap.cols) {
        discard;
    }
    // Rows are addressed in data units so a Y zoom re-slices the same grid
    // instead of forcing a rebuild.
    let bin = tonemap.row_lo + in.uv.y * (tonemap.row_hi - tonemap.row_lo);
    let row = i32(floor(bin));
    if row < 0 || row >= i32(tonemap.rows) {
        discard;
    }

    let v = textureLoad(field, vec2<i32>(col, row), 0).r;
    if v <= EMPTY_THRESHOLD {
        discard;
    }

    var t = (v - tonemap.lo) / max(tonemap.hi - tonemap.lo, 1e-12);
    t = max(t, 0.0);
    if tonemap.scale_mode == 1u {
        t = sqrt(t);
    } else if tonemap.scale_mode == 2u {
        t = log(1.0 + t * 9.0) / log(10.0);
    }
    t = clamp(t * tonemap.gain, 0.0, 1.0);

    // `textureSampleLevel` rather than `textureSample`: the discards above put
    // this in non-uniform control flow, where implicit derivatives are invalid.
    return textureSampleLevel(lut, lut_sampler, vec2(t, 0.5), 0.0);
}
