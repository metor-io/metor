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
    @location(1) @interpolate(perspective, sample) edge_dist: f32,
    @location(2) half_width: f32,
}

fn data_to_pixel(p: vec2<f32>) -> vec2<f32> {
    let clip = p * view.scale + view.offset;
    return (clip * 0.5 + 0.5) * view.viewport;
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

    let idx_a = index_buffer[instance_index];
    let idx_b = index_buffer[instance_index + 1u];

    let p_a = data_to_pixel(vec2(x_values[idx_a], y_values[idx_a]));
    let p_b = data_to_pixel(vec2(x_values[idx_b], y_values[idx_b]));

    let half_width = max(line_uniform.line_width * 0.5, 0.5);
    let aa = 1.0;
    let expand = half_width + aa;

    let seg = p_b - p_a;
    let seg_len = max(length(seg), 1e-6);
    let t_seg = seg / seg_len;
    let n_seg = vec2(-t_seg.y, t_seg.x);

    let base = mix(p_a, p_b, corner.y);
    let pixel_pos = base + corner.x * n_seg * expand;

    let ndc = (pixel_pos / view.viewport) * 2.0 - 1.0;

    var out: VertexOutput;
    out.clip_position = vec4(ndc, 0.0, 1.0);
    out.color = line_uniform.color;
    out.edge_dist = corner.x * expand;
    out.half_width = half_width;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let coverage = clamp(in.half_width + 0.5 - abs(in.edge_dist), 0.0, 1.0);
    return vec4(in.color.rgb, in.color.a * coverage);
}
