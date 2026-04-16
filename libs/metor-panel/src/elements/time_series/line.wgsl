struct View {
    scale: vec2<f32>,
    offset: vec2<f32>,
    viewport: vec2<f32>,
    _pad: vec2<f32>,
}

struct LineUniform {
    color: vec4<f32>,
    line_width: f32,
}

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var<uniform> line_uniform: LineUniform;

@group(2) @binding(0) var<storage, read> x_values: array<f32>;
@group(2) @binding(1) var<storage, read> y_values: array<f32>;
@group(2) @binding(2) var<storage, read> index_buffer: array<u32>;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

fn data_to_pixel(p: vec2<f32>) -> vec2<f32> {
    let clip = p * view.scale + view.offset;
    return (clip * 0.5 + 0.5) * view.viewport;
}

fn endpoint_miter(n_seg: vec2<f32>, n_neighbor: vec2<f32>, half_width: f32) -> vec2<f32> {
    let miter_limit = 4.0;
    let bisector = n_seg + n_neighbor;
    let bisector_len = length(bisector);
    if bisector_len < 1e-4 {
        return n_seg * half_width;
    }
    let m = bisector / bisector_len;
    let cos_a = dot(m, n_seg);
    let len = half_width / max(cos_a, 1.0 / miter_limit);
    if len > half_width * miter_limit {
        return n_seg * half_width;
    }
    return m * len;
}

@vertex
fn vertex(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    var positions = array<vec2<f32>, 4>(
        vec2(-1.0, 0.0),
        vec2(-1.0, 1.0),
        vec2( 1.0, 0.0),
        vec2( 1.0, 1.0),
    );
    let corner = positions[vertex_index];

    // index_buffer layout for line spans is: [sentinel, p0, p1, ..., pN-1, sentinel].
    // instance_index points at p_k, so instance_index - 1 / + 2 are always in-bounds.
    let idx_prev = index_buffer[instance_index - 1u];
    let idx_a    = index_buffer[instance_index];
    let idx_b    = index_buffer[instance_index + 1u];
    let idx_next = index_buffer[instance_index + 2u];

    let p_prev = data_to_pixel(vec2(x_values[idx_prev], y_values[idx_prev]));
    let p_a    = data_to_pixel(vec2(x_values[idx_a],    y_values[idx_a]));
    let p_b    = data_to_pixel(vec2(x_values[idx_b],    y_values[idx_b]));
    let p_next = data_to_pixel(vec2(x_values[idx_next], y_values[idx_next]));

    let half_width = max(line_uniform.line_width * 0.5, 0.5);

    let seg = p_b - p_a;
    let seg_len = max(length(seg), 1e-6);
    let t_seg = seg / seg_len;
    let n_seg = vec2(-t_seg.y, t_seg.x);

    // Boundary sentinels make p_prev == p_a / p_next == p_b; detect the
    // degenerate neighbor and fall back to the segment's own tangent.
    let d_in = p_a - p_prev;
    let d_in_len = length(d_in);
    let t_in = select(t_seg, d_in / max(d_in_len, 1e-6), d_in_len > 1e-6);
    let n_in = vec2(-t_in.y, t_in.x);

    let d_out = p_next - p_b;
    let d_out_len = length(d_out);
    let t_out = select(t_seg, d_out / max(d_out_len, 1e-6), d_out_len > 1e-6);
    let n_out = vec2(-t_out.y, t_out.x);

    let miter_a = endpoint_miter(n_seg, n_in, half_width);
    let miter_b = endpoint_miter(n_seg, n_out, half_width);

    let base = mix(p_a, p_b, corner.y);
    let miter = mix(miter_a, miter_b, corner.y);
    let pixel_pos = base + corner.x * miter;

    let ndc = (pixel_pos / view.viewport) * 2.0 - 1.0;

    var out: VertexOutput;
    out.clip_position = vec4(ndc, 0.0, 1.0);
    out.color = line_uniform.color;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
