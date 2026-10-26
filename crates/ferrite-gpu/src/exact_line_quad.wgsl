
// Vertex shader
struct InstanceInput {
    @location(0) start: vec2<f32>,
    @location(1) end: vec2<f32>,
    @location(2) offset0: vec2<f32>,
    @location(3) offset1: vec2<f32>,
    @location(4) offset2: vec2<f32>,
    @location(5) offset3: vec2<f32>,
    @location(6) color: vec4<f32>,
}
struct VertexInput {
    position: vec2<f32>,
    offset: vec2<f32>,
    color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

struct ViewUniforms {
    view_proj: mat4x4<f32>,
    viewport_size: vec2<f32>,
    scale: f32,
    _padding: f32,
    pan_offset: vec2<f32>,
    zoom_scale: f32,
    zoom_scale_y: f32,
    zoom_pivot: vec2<f32>,
    _padding3: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> view: ViewUniforms;

@vertex
fn vs_main(q: InstanceInput, @builtin(vertex_index) corner: u32) -> VertexOutput {
    // Select already-computed original bits. No GPU sqrt/divide/normal calculation.
    var in: VertexInput;
    in.position = q.start;
    if corner == 2u || corner == 3u { in.position = q.end; }
    in.offset = q.offset0;
    if corner == 1u { in.offset = q.offset1; }
    if corner == 2u { in.offset = q.offset2; }
    if corner == 3u { in.offset = q.offset3; }
    in.color = q.color;
    var out: VertexOutput;
    // Screen-space vertex: apply pan offset + zoom
    var pos = in.position + view.pan_offset;
    pos = (pos - view.zoom_pivot) * vec2<f32>(view.zoom_scale, view.zoom_scale_y) + view.zoom_pivot;
    out.clip_position = view.view_proj * vec4<f32>(pos + in.offset, 0.0, 1.0);
    out.color = in.color;
    return out;
}

// Fragment shader
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
