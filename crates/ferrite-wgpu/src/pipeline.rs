//! Render Pipeline Definitions
//!
//! Contains shader code and pipeline creation for different rendering modes.

use crate::{state::MSAA_SAMPLE_COUNT, GpuState, LineVertex, Result, Vertex2D};

/// Basic 2D shader for solid colored geometry (screen-space vertices)
pub(crate) const BASIC_SHADER: &str = r#"
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
    zoom_scale: f32,
    zoom_scale_y: f32,
    zoom_pivot: vec2<f32>,
    _padding3: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> view: ViewUniforms;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // Screen-space vertex: apply pan offset + zoom
    var pos = in.position + view.pan_offset;
    pos = (pos - view.zoom_pivot) * vec2<f32>(view.zoom_scale, view.zoom_scale_y) + view.zoom_pivot;
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

// Stroke centers follow the map; perpendicular offsets retain their pixel size.
pub(crate) const LINE_SHADER: &str = r#"
// Vertex shader
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) offset: vec2<f32>,
    @location(2) color: vec4<f32>,
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
fn vs_main(in: VertexInput) -> VertexOutput {
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
"#;

/// Texture shader for rendering textured quads (symbols)
pub(crate) const TEXTURE_SHADER: &str = r#"
// Vertex shader for textured quads
struct TextureVertexInput {
    @location(0) position: vec2<f32>,
    @location(1) tex_coord: vec2<f32>,
    @location(2) anchor: vec2<f32>,
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
    zoom_scale: f32,
    zoom_scale_y: f32,
    zoom_pivot: vec2<f32>,
    _padding3: vec2<f32>,
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
    // Scale the geographic anchor while preserving the symbol's portrayal size.
    let anchor = (in.anchor + view.pan_offset - view.zoom_pivot) * vec2<f32>(view.zoom_scale, view.zoom_scale_y)
        + view.zoom_pivot;
    let pos = anchor + (in.position - in.anchor);
    out.clip_position = view.view_proj * vec4<f32>(pos, 0.0, 1.0);
    out.tex_coord = in.tex_coord;
    return out;
}

// Fragment shader - samples texture with premultiplied alpha
@fragment
fn fs_main(in: TextureVertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(t_diffuse, s_diffuse, in.tex_coord);
    return color;
}
"#;

/// Pattern fill shader: tiles a pattern texture across polygon geometry.
/// tex_coord stores 1/tile_size (inv_tile_size); the vertex shader computes
/// UV = (position + pan_offset) * inv_tile_size so the pattern tracks the chart.
pub(crate) const PATTERN_FILL_SHADER: &str = r#"
struct PatternVertexInput {
    @location(0) position: vec2<f32>,
    @location(1) tile_params: vec4<f32>, // (inv_tx, inv_ty, shear, 0)
}

struct PatternVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) @interpolate(flat) tile_params: vec4<f32>,
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

@group(1) @binding(0)
var t_pattern: texture_2d<f32>;
@group(1) @binding(1)
var s_pattern: sampler;

@vertex
fn vs_main(in: PatternVertexInput) -> PatternVertexOutput {
    var out: PatternVertexOutput;
    var pos = in.position + view.pan_offset;
    pos = (pos - view.zoom_pivot) * vec2<f32>(view.zoom_scale, view.zoom_scale_y) + view.zoom_pivot;
    out.clip_position = view.view_proj * vec4<f32>(pos, 0.0, 1.0);
    // Constant physical lattice parameters; fragment position determines phase.
    out.tile_params = in.tile_params;
    return out;
}

@fragment
fn fs_main(in: PatternVertexOutput) -> @location(0) vec4<f32> {
    // Fragment @builtin(position) is the framebuffer sample position. Computing
    // the lattice here avoids interpolating enormous off-screen UV coordinates,
    // and makes phase independent of triangulation, clipping and longitude copies.
    let pos = in.clip_position.xy;
    let uv = vec2<f32>((pos.x - in.tile_params.z * pos.y) * in.tile_params.x,
                      pos.y * in.tile_params.y);
    return textureSample(t_pattern, s_pattern, uv);
}
"#;

/// Vertex for textured quads
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TextureVertex {
    pub position: [f32; 2],
    pub tex_coord: [f32; 2],
    pub anchor: [f32; 2],
}

impl TextureVertex {
    #[inline]
    pub fn new(x: f32, y: f32, u: f32, v: f32, anchor: [f32; 2]) -> Self {
        TextureVertex {
            position: [x, y],
            tex_coord: [u, v],
            anchor,
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
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

/// Vertex for pattern fill with parallelogram shear support (S-100 v1/v2 lattice)
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PatternVertex {
    pub position: [f32; 2],
    /// (inv_tile_width, inv_tile_height, shear_ratio)
    /// shear_ratio = v2.x / v2.y: horizontal shift per unit of vertical position
    pub tile_params: [f32; 4], // [inv_tx, inv_ty, shear, 0.0] — padded to vec4
}

impl PatternVertex {
    #[inline]
    pub fn new(x: f32, y: f32, inv_tx: f32, inv_ty: f32, shear: f32) -> Self {
        PatternVertex {
            position: [x, y],
            tile_params: [inv_tx, inv_ty, shear, 0.0],
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<PatternVertex>() as wgpu::BufferAddress,
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
                    format: wgpu::VertexFormat::Float32x4,
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
    pub(crate) symbol_instance_pipeline: Option<wgpu::RenderPipeline>,
    /// Coverage cells use a shared source lattice and pixel-centre lookup.
    pub raster_pipeline: wgpu::RenderPipeline,
    pub(crate) continuous_raster_pipeline: Option<wgpu::RenderPipeline>,
    pub chart_text_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for view uniforms
    pub view_bind_group_layout: wgpu::BindGroupLayout,
    /// Bind group layout for textures
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    /// Sampler for texture sampling (ClampToEdge for symbols)
    pub texture_sampler: wgpu::Sampler,
    /// Pipeline for pattern-filled areas (textured triangles with repeat tiling)
    pub pattern_fill_pipeline: wgpu::RenderPipeline,
    /// Sampler for pattern fills (Repeat mode for seamless tiling)
    pub pattern_sampler: wgpu::Sampler,
    /// Bind group layout for pattern textures (same structure, separate for repeat sampler)
    pub pattern_bind_group_layout: wgpu::BindGroupLayout,
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

        let line_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("line_shader"),
                source: wgpu::ShaderSource::Wgsl(LINE_SHADER.into()),
            });

        // Create texture shader module
        let texture_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("texture_shader"),
                source: wgpu::ShaderSource::Wgsl(TEXTURE_SHADER.into()),
            });

        let raster_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("coverage_lattice_shader"),
                source: wgpu::ShaderSource::Wgsl(RASTER_SHADER.into()),
            });

        let chart_text_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("chart_text_shader"),
                source: wgpu::ShaderSource::Wgsl(CHART_TEXT_SHADER.into()),
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
                    module: &line_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[LineVertex::desc()],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &line_shader,
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

        let symbol_instance_pipeline = if std::env::var("FERRITE_SYMBOL_INSTANCING").as_deref() == Ok("1") {
            let symbol_instance_shader = state.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("symbol_instance_shader"),
                source: wgpu::ShaderSource::Wgsl(crate::symbol_instance::shader().into()),
            });
        let symbol_instance_pipeline =
            state
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("symbol_instance_pipeline"),
                    layout: Some(&texture_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &symbol_instance_shader,
                        entry_point: Some("vs_main"),
                        buffers: &[crate::symbol_instance::SymbolQuadInstance::desc()],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &symbol_instance_shader,
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

            Some(symbol_instance_pipeline)
        } else { None };

        let raster_pipeline =
            state
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("coverage_lattice_pipeline"),
                    layout: Some(&texture_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &raster_shader,
                        entry_point: Some("vs_main"),
                        buffers: &[RasterVertex::desc()],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &raster_shader,
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

        let chart_text_pipeline =
            state
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("coverage_lattice_pipeline"),
                    layout: Some(&texture_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &chart_text_shader,
                        entry_point: Some("vs_main"),
                        buffers: &[ChartTextVertex::desc()],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &chart_text_shader,
                        entry_point: Some(if state.format().is_srgb() {
                            "fs_linear"
                        } else {
                            "fs_gamma"
                        }),
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

        // Create texture sampler (ClampToEdge for symbols)
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

        // --- Pattern fill pipeline (GPU texture-repeat tiling, per S-100 standard) ---

        // Pattern sampler with Repeat mode for seamless tiling
        let pattern_sampler = state.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pattern_sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest, // Pixel-perfect (like OpenS100)
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // Pattern bind group layout (same structure, uses pattern_sampler)
        let pattern_bind_group_layout =
            state
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("pattern_bind_group_layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2,
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                            count: None,
                        },
                    ],
                });

        let pattern_pipeline_layout =
            state
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("pattern_pipeline_layout"),
                    bind_group_layouts: &[&view_bind_group_layout, &pattern_bind_group_layout],
                    push_constant_ranges: &[],
                });

        let pattern_shader = state
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("pattern_fill_shader"),
                source: wgpu::ShaderSource::Wgsl(PATTERN_FILL_SHADER.into()),
            });

        let pattern_fill_pipeline =
            state
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("pattern_fill_pipeline"),
                    layout: Some(&pattern_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &pattern_shader,
                        entry_point: Some("vs_main"),
                        buffers: &[PatternVertex::desc()],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &pattern_shader,
                        entry_point: Some("fs_main"),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: state.format(),
                            // Premultiplied alpha blending (same as symbols)
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

        Ok(RenderPipelines {
            area_pipeline,
            line_pipeline,
            texture_pipeline,
            symbol_instance_pipeline,
            raster_pipeline,
            continuous_raster_pipeline: None,
            chart_text_pipeline,
            view_bind_group_layout,
            texture_bind_group_layout,
            texture_sampler,
            pattern_fill_pipeline,
            pattern_sampler,
            pattern_bind_group_layout,
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

    /// Coverage lookup colours and NoData remain discrete at grid-cell boundaries.

    pub(crate) fn ensure_continuous_raster_pipeline(&mut self,state:&GpuState) {
        if self.continuous_raster_pipeline.is_some() {return;}
        let shader=state.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label:Some("continuous-source-selector"),source:wgpu::ShaderSource::Wgsl(crate::continuous_raster_selector::flat_shader().into()),
        });
        let layout=state.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label:Some("continuous-source-selector-layout"),bind_group_layouts:&[&self.view_bind_group_layout,&self.texture_bind_group_layout],push_constant_ranges:&[],
        });
        let pipeline =
            state
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("continuous_source_selector_pipeline"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        buffers: &[RasterVertex::desc()],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
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

        self.continuous_raster_pipeline=Some(pipeline);
    }
    pub fn create_raster_bind_group(
        &self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("coverage-nearest-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("coverage-texture-bind-group"),
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        })
    }

    /// Create bind group for a pattern fill texture (uses Repeat sampler)
    pub fn create_pattern_bind_group(
        &self,
        device: &wgpu::Device,
        texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pattern_bind_group"),
            layout: &self.pattern_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.pattern_sampler),
                },
            ],
        })
    }
}

/// Four vertices per raster tile; symbol vertices retain their original layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RasterVertex {
    pub position: [f32; 2],
    pub anchor: [f32; 2],
    pub origin: [f32; 2],
    pub step: [f32; 2],
    pub offset: [u32; 2],
    pub size: [u32; 2],
    pub row_bounds: [f32; 2],
    pub row_index: u32,
    pub padding: u32,
}
impl RasterVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        const ATTR: [wgpu::VertexAttribute; 8] = wgpu::vertex_attr_array![0=>Float32x2,1=>Float32x2,2=>Float32x2,3=>Float32x2,4=>Uint32x2,5=>Uint32x2,6=>Float32x2,7=>Uint32];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTR,
        }
    }
}
const RASTER_SHADER: &str = r#"
struct Input { @location(0) position:vec2<f32>, @location(1) anchor:vec2<f32>, @location(2) origin:vec2<f32>, @location(3) step:vec2<f32>, @location(4) offset:vec2<u32>, @location(5) size:vec2<u32>, @location(6) row_bounds:vec2<f32>, @location(7) row_index:u32 }
struct Output { @builtin(position) position:vec4<f32>, @location(0) @interpolate(flat) origin:vec2<f32>, @location(1) @interpolate(flat) step:vec2<f32>, @location(2) @interpolate(flat) offset:vec2<u32>, @location(3) @interpolate(flat) size:vec2<u32>, @location(4) @interpolate(flat) row_bounds:vec2<f32>, @location(5) @interpolate(flat) row_index:u32 }
struct ViewUniforms {view_proj:mat4x4<f32>,viewport_size:vec2<f32>,scale:f32,_padding:f32,pan_offset:vec2<f32>,zoom_scale:f32,zoom_scale_y:f32,zoom_pivot:vec2<f32>,_padding3:vec2<f32>}
@group(0) @binding(0) var<uniform> view:ViewUniforms;
@group(1) @binding(0) var pixels:texture_2d<f32>;
@vertex fn vs_main(v:Input)->Output {
    var out:Output;
    let anchor=(v.anchor+view.pan_offset-view.zoom_pivot)*vec2<f32>(view.zoom_scale,view.zoom_scale_y)+view.zoom_pivot;
    out.origin=(v.origin+view.pan_offset-view.zoom_pivot)*vec2<f32>(view.zoom_scale,view.zoom_scale_y)+view.zoom_pivot;
    out.step=v.step*vec2<f32>(view.zoom_scale,view.zoom_scale_y);out.offset=v.offset;out.size=v.size;
    out.row_bounds=(v.row_bounds+vec2<f32>(view.pan_offset.y-view.zoom_pivot.y))*view.zoom_scale_y+vec2<f32>(view.zoom_pivot.y);
    out.row_index=v.row_index;
    // Thin edge tiles can receive padding past the source footprint. All tiles
    // clip to the same transformed outer rectangle before MSAA rasterization.
    let far=out.origin+out.step*vec2<f32>(v.size);
    let position=clamp(anchor+(v.position-v.anchor),out.origin,far);
    out.position=view.view_proj*vec4<f32>(position,0.,1.);
    return out;
}
@fragment fn fs_main(v:Output)->@location(0) vec4<f32> {
    var global=vec2<u32>(clamp(floor((v.position.xy-v.origin)/v.step),vec2<f32>(0.),vec2<f32>(v.size)-vec2<f32>(1.)));
    if v.row_index != 0xffffffffu {
        // Exact geographic cell-row edges are projected by the CPU in f64.
        // Padded strips provide full MSAA coverage, with half-open centre ownership.
        if v.position.y < v.row_bounds.x || v.position.y >= v.row_bounds.y {discard;}
        global.y=v.row_index;
    }
    if any(global<v.offset) {discard;}
    let local=global-v.offset;
    if any(local>=textureDimensions(pixels)) {discard;}
    // Only the texture-local coordinate is signed; it is bounded by the GPU limit.
    return textureLoad(pixels,vec2<i32>(local),0);
}
"#;

// Chart glyph positions have already received map transforms. Only the projection
// applies here; the font size and local offset remain physical screen sizes.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ChartTextVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}
impl ChartTextVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        const ATTR: [wgpu::VertexAttribute; 3] =
            wgpu::vertex_attr_array![0=>Float32x2,1=>Float32x2,2=>Unorm8x4];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTR,
        }
    }
}
// The egui atlas uses sRGB premultiplied colours. Preserve egui's gamma-space
// multiplication, then convert for an sRGB target when required.
pub(crate) const CHART_TEXT_SHADER: &str = r#"
struct View {projection:mat4x4<f32>,size:vec2<f32>,scale:f32,padding:f32,pan:vec2<f32>,zoom:f32,padding2:f32,pivot:vec2<f32>,padding3:vec2<f32>}
@group(0) @binding(0) var<uniform> view:View;
@group(1) @binding(0) var atlas:texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler:sampler;
struct Out {@builtin(position) position:vec4<f32>,@location(0) uv:vec2<f32>,@location(1) color:vec4<f32>}
@vertex fn vs_main(@location(0) position:vec2<f32>,@location(1) uv:vec2<f32>,@location(2) color:vec4<f32>)->Out {
 var out:Out;out.position=view.projection*vec4<f32>(position,0.,1.);out.uv=uv;out.color=color;return out;
}
fn to_gamma(rgb:vec3<f32>)->vec3<f32>{return select(1.055*pow(rgb,vec3<f32>(1./2.4))-0.055,12.92*rgb,rgb<vec3<f32>(0.0031308));}
fn to_linear(rgb:vec3<f32>)->vec3<f32>{return select(pow((rgb+0.055)/1.055,vec3<f32>(2.4)),rgb/12.92,rgb<vec3<f32>(0.04045));}
// Color32 stores gamma-encoded, linearly premultiplied RGB. Recover straight
// gamma colour before premultiplying for an unorm chart target.
fn glyph(v:Out)->vec4<f32>{
 let sampled=textureSample(atlas,atlas_sampler,v.uv);
 if v.color.a<=0. {return vec4<f32>(0.);}
 let rgb=to_gamma(to_linear(v.color.rgb)/v.color.a)*v.color.a;
 return vec4<f32>(rgb,v.color.a)*vec4<f32>(to_gamma(sampled.rgb),sampled.a);
}
@fragment fn fs_gamma(v:Out)->@location(0) vec4<f32>{return glyph(v);}
@fragment fn fs_linear(v:Out)->@location(0) vec4<f32>{let c=glyph(v);if c.a<=0. {return vec4<f32>(0.);};return vec4<f32>(to_linear(c.rgb/c.a)*c.a,c.a);}
"#;
