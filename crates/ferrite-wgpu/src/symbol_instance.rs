//! Exact original quad corners, transported once per symbol.
use crate::pipeline::{TextureVertex, TEXTURE_SHADER};

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SymbolQuadInstance {
    pub corners: [[f32; 2]; 4],
    pub anchor: [f32; 2],
}
impl SymbolQuadInstance {
    pub fn from_quad(quad: &[TextureVertex]) -> Self {
        assert_eq!(quad.len(), 4);
        Self { corners: [quad[0].position, quad[1].position, quad[2].position, quad[3].position], anchor: quad[0].anchor }
    }
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: 40,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 16, shader_location: 2, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 24, shader_location: 3, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 32, shader_location: 4, format: wgpu::VertexFormat::Float32x2 },
            ],
        }
    }
}
pub(crate) const QUAD_INDICES: [u32; 6] = [0, 1, 2, 0, 2, 3];

pub(crate) fn shader() -> String {
    // Keep the original float operations and fragment shader verbatim.
    let original = TEXTURE_SHADER.replacen("@vertex\nfn vs_main(in: TextureVertexInput)", "fn texture_vertex(in: TextureVertexInput)", 1);
    assert_ne!(original, TEXTURE_SHADER);
    original + r#"
struct SymbolInstanceInput {
    @location(0) corner0: vec2<f32>,
    @location(1) corner1: vec2<f32>,
    @location(2) corner2: vec2<f32>,
    @location(3) corner3: vec2<f32>,
    @location(4) anchor: vec2<f32>,
}
@vertex
fn vs_main(in: SymbolInstanceInput, @builtin(vertex_index) index: u32) -> TextureVertexOutput {
    var vertex: TextureVertexInput;
    switch index {
        case 0u: { vertex.position = in.corner0; vertex.tex_coord = vec2<f32>(0.0, 0.0); }
        case 1u: { vertex.position = in.corner1; vertex.tex_coord = vec2<f32>(1.0, 0.0); }
        case 2u: { vertex.position = in.corner2; vertex.tex_coord = vec2<f32>(1.0, 1.0); }
        default: { vertex.position = in.corner3; vertex.tex_coord = vec2<f32>(0.0, 1.0); }
    }
    vertex.anchor = in.anchor;
    return texture_vertex(vertex);
}
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_and_coverage_instance_shaders_validate() {
        let source = shader();
        let masked = crate::coverage_clip::fragment_clipped_shader(&source, 2, &[
            crate::coverage_clip::FragmentEntry { name: "fs_main", input_type: "TextureVertexOutput", position_field: "clip_position" }
        ]).unwrap();
        for shader in [&source, &masked] {
            let module = wgpu::naga::front::wgsl::parse_str(shader).unwrap();
            wgpu::naga::valid::Validator::new(
                wgpu::naga::valid::ValidationFlags::all(), wgpu::naga::valid::Capabilities::all()
            ).validate(&module).unwrap();
        }
    }

    #[test]
    fn payload_preserves_original_corner_bits_and_triangle_schedule() {
        let corners = [[-0.0, f32::MIN_POSITIVE], [123.25, -876.5], [-1.0, 0.0], [f32::MAX, -f32::MIN_POSITIVE]];
        let anchor = [-0.0, 99.5];
        let uv = [[0.,0.],[1.,0.],[1.,1.],[0.,1.]];
        let quad: Vec<_> = (0..4).map(|i| TextureVertex::new(corners[i][0], corners[i][1], uv[i][0], uv[i][1], anchor)).collect();
        let instance = SymbolQuadInstance::from_quad(&quad);
        assert_eq!(std::mem::size_of::<SymbolQuadInstance>(), 40);
        for index in QUAD_INDICES {
            let q = &quad[index as usize];
            assert_eq!(bytemuck::bytes_of(&q.position), bytemuck::bytes_of(&instance.corners[index as usize]));
            assert_eq!(bytemuck::bytes_of(&q.anchor), bytemuck::bytes_of(&instance.anchor));
        }
        assert_eq!(QUAD_INDICES, [0,1,2,0,2,3]);
    }
}
