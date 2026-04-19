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

fn data_to_clip(p: vec2<f32>) -> vec2<f32> {
    return p * view.scale + view.offset;
}

@vertex
fn vertex(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    // Rectangle corners: (left/right, top/bottom)
    var corners = array<vec2<f32>, 4>(
        vec2(0.0, 0.0),
        vec2(0.0, 1.0),
        vec2(1.0, 0.0),
        vec2(1.0, 1.0),
    );
    let corner = corners[vertex_index];

    let idx = index_buffer[instance_index];
    let data_pt = data_to_clip(vec2(x_values[idx], y_values[idx]));

    // Baseline: where y=0 maps in clip space, clamped to viewport.
    // When view crosses zero this is the zero line; otherwise the bottom/top edge.
    let baseline_y = clamp(view.offset.y, -1.0, 1.0);

    let bar_half_ndc = line_uniform.line_width / view.viewport.x;
    let x = mix(data_pt.x - bar_half_ndc, data_pt.x + bar_half_ndc, corner.x);
    let y = mix(baseline_y, data_pt.y, corner.y);

    var out: VertexOutput;
    out.clip_position = vec4(x, y, 0.0, 1.0);
    out.color = line_uniform.color;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
