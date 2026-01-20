//! Render Pipeline Definitions
//!
//! Contains shader code and pipeline creation for different rendering modes.

use crate::{state::MSAA_SAMPLE_COUNT, GpuState, Result, Vertex2D};

/// Basic 2D shader for solid colored geometry
const BASIC_SHADER: &str = r#"
// Vertex shader
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
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
    _padding2: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> view: ViewUniforms;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // Apply pan offset before projection
    let pos = in.position + view.pan_offset;
    out.clip_position = view.view_proj * vec4<f32>(pos, 0.0, 1.0);
    out.color = in.color;
    return out;
}

// Fragment shader
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// Texture shader for rendering textured quads (symbols)
const TEXTURE_SHADER: &str = r#"
// Vertex shader for textured quads
struct TextureVertexInput {
    @location(0) position: vec2<f32>,
    @location(1) tex_coord: vec2<f32>,
}

struct TextureVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
}

struct ViewUniforms {
    view_proj: mat4x4<f32>,
    viewport_size: vec2<f32>,
    scale: f32,
    _padding: f32,
    pan_offset: vec2<f32>,
    _padding2: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> view: ViewUniforms;

@group(1) @binding(0)
var t_diffuse: texture_2d<f32>;
@group(1) @binding(1)
var s_diffuse: sampler;

@vertex
fn vs_main(in: TextureVertexInput) -> TextureVertexOutput {
    var out: TextureVertexOutput;
    // Apply pan offset before projection
    let pos = in.position + view.pan_offset;
    out.clip_position = view.view_proj * vec4<f32>(pos, 0.0, 1.0);
    out.tex_coord = in.tex_coord;
    return out;
}

// Fragment shader - samples texture with premultiplied alpha
@fragment
fn fs_main(in: TextureVertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(t_diffuse, s_diffuse, in.tex_coord);
    // resvg outputs premultiplied alpha, so we need to handle it properly
    // For alpha blending with premultiplied alpha, we use: src + dst * (1 - src_alpha)
    return color;
}
"#;

/// Vertex for textured quads
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TextureVertex {
    pub position: [f32; 2],
    pub tex_coord: [f32; 2],
}

impl TextureVertex {
    #[inline]
    pub fn new(x: f32, y: f32, u: f32, v: f32) -> Self {
        TextureVertex {
            position: [x, y],
            tex_coord: [u, v],
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TextureVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

/// Render pipelines for chart display
pub struct RenderPipelines {
    /// Pipeline for solid colored polygons (areas)
    pub area_pipeline: wgpu::RenderPipeline,
    /// Pipeline for lines
    pub line_pipeline: wgpu::RenderPipeline,
    /// Pipeline for textured quads (symbols)
    pub texture_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for view uniforms
    pub view_bind_group_layout: wgpu::BindGroupLayout,
    /// Bind group layout for textures
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    /// Sampler for texture sampling
    pub texture_sampler: wgpu::Sampler,
}

impl RenderPipelines {
    /// Create all render pipelines
    pub fn new(state: &GpuState) -> Result<Self> {
        // Create basic shader module
        let basic_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("basic_shader"),
                source: wgpu::ShaderSource::Wgsl(BASIC_SHADER.into()),
            });

        // Create texture shader module
        let texture_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("texture_shader"),
                source: wgpu::ShaderSource::Wgsl(TEXTURE_SHADER.into()),
            });

        // Create bind group layout for view uniforms
        let view_bind_group_layout =
            state
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("view_bind_group_layout"),
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    }],
                });

        // Create bind group layout for textures
        let texture_bind_group_layout =
            state
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("texture_bind_group_layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2,
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });

        // Create pipeline layout for basic rendering
        let basic_pipeline_layout =
            state
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("basic_pipeline_layout"),
                    bind_group_layouts: &[&view_bind_group_layout],
                    push_constant_ranges: &[],
                });

        // Create pipeline layout for texture rendering
        let texture_pipeline_layout =
            state
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("texture_pipeline_layout"),
                    bind_group_layouts: &[&view_bind_group_layout, &texture_bind_group_layout],
                    push_constant_ranges: &[],
                });

        // Create area pipeline (triangles with blending)
        let area_pipeline = state
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("area_pipeline"),
                layout: Some(&basic_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &basic_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[Vertex2D::desc()],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &basic_shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: state.format(),
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None, // No culling for 2D
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: MSAA_SAMPLE_COUNT,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview: None,
                cache: None,
            });

        // Create line pipeline
        let line_pipeline = state
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("line_pipeline"),
                layout: Some(&basic_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &basic_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[Vertex2D::desc()],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &basic_shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: state.format(),
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList, // Lines rendered as quads
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: MSAA_SAMPLE_COUNT,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview: None,
                cache: None,
            });

        // Create texture pipeline for symbols (premultiplied alpha blending)
        let texture_pipeline =
            state
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("texture_pipeline"),
                    layout: Some(&texture_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &texture_shader,
                        entry_point: Some("vs_main"),
                        buffers: &[TextureVertex::desc()],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &texture_shader,
                        entry_point: Some("fs_main"),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: state.format(),
                            // Premultiplied alpha blending: src + dst * (1 - src_alpha)
                            blend: Some(wgpu::BlendState {
                                color: wgpu::BlendComponent {
                                    src_factor: wgpu::BlendFactor::One,
                                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                    operation: wgpu::BlendOperation::Add,
                                },
                                alpha: wgpu::BlendComponent {
                                    src_factor: wgpu::BlendFactor::One,
                                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                    operation: wgpu::BlendOperation::Add,
                                },
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        strip_index_format: None,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: None,
                        polygon_mode: wgpu::PolygonMode::Fill,
                        unclipped_depth: false,
                        conservative: false,
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState {
                        count: MSAA_SAMPLE_COUNT,
                        mask: !0,
                        alpha_to_coverage_enabled: false,
                    },
                    multiview: None,
                    cache: None,
                });

        // Create texture sampler
        let texture_sampler = state.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("texture_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Ok(RenderPipelines {
            area_pipeline,
            line_pipeline,
            texture_pipeline,
            view_bind_group_layout,
            texture_bind_group_layout,
            texture_sampler,
        })
    }

    /// Create bind group for view uniforms
    pub fn create_view_bind_group(
        &self,
        device: &wgpu::Device,
        buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view_bind_group"),
            layout: &self.view_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        })
    }

    /// Create bind group for a texture
    pub fn create_texture_bind_group(
        &self,
        device: &wgpu::Device,
        texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture_bind_group"),
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.texture_sampler),
                },
            ],
        })
    }
}
