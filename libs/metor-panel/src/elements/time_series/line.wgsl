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
    var positions = array<vec2<f32>, 4>(
        vec2(-1.0, 0.0),
        vec2(-1.0, 1.0),
        vec2( 1.0, 0.0),
        vec2( 1.0, 1.0),
    );
    let position = positions[vertex_index];

    let index_a = index_buffer[instance_index];
    let index_b = index_buffer[instance_index + 1u];

    let pos_a = vec2(x_values[index_a], y_values[index_a]);
    let pos_b = vec2(x_values[index_b], y_values[index_b]);

    let clip_a = data_to_clip(pos_a);
    let clip_b = data_to_clip(pos_b);

    let delta = clip_b - clip_a;
    let len = max(length(delta), 1e-12);
    let x_basis = delta / len;
    let y_basis = vec2(-x_basis.y, x_basis.x);

    let half_width_ndc = line_uniform.line_width / view.viewport;
    let stride = position.x * y_basis * half_width_ndc;
    let point = mix(clip_a, clip_b, position.y) + stride;

    var out: VertexOutput;
    out.clip_position = vec4(point, 0.0, 1.0);
    out.color = line_uniform.color;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
