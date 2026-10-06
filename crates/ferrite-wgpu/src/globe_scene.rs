//! Product-neutral 3D globe primitives with a real depth buffer. ECEF positions
//! remain f64 on the CPU; camera-relative homogeneous clips narrow to f32 only
//! after projection. Product adapters must supply draped/tessellated geometry.
use crate::gpu_globe_projection::{CameraParams, GpuProjection, SourceVertex};
use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera};
use std::sync::Arc;
#[path = "globe_coverage.rs"]
mod coverage;
const GLOBE_COLOR_SHADER: &str = r#"
struct In {@location(0) clip:vec4<f32>,@location(1) color:vec4<f32>};
struct Out {@builtin(position) position:vec4<f32>,@location(0) color:vec4<f32>};
@vertex fn vs_main(v:In)->Out {var o:Out;o.position=v.clip;o.color=v.color;return o;}
@fragment fn fs_main(v:Out)->@location(0) vec4<f32> {return v.color;}
"#;
const GLOBE_TEXTURE_SHADER: &str = r#"
struct In {@location(0) clip:vec4<f32>,@location(1) uv:vec4<f32>,@location(2) tint:vec4<f32>};
struct Out {@builtin(position) position:vec4<f32>,@location(0) uv:vec4<f32>,@location(1) tint:vec4<f32>};
@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var image_sampler:sampler;
@vertex fn vs_main(v:In)->Out {var o:Out;o.position=v.clip;o.uv=v.uv;o.tint=v.tint;return o;}
fn to_gamma(rgb:vec3<f32>)->vec3<f32>{return select(1.055*pow(rgb,vec3<f32>(1./2.4))-0.055,12.92*rgb,rgb<vec3<f32>(0.0031308));}
fn to_linear(rgb:vec3<f32>)->vec3<f32>{return select(pow((rgb+0.055)/1.055,vec3<f32>(2.4)),rgb/12.92,rgb<vec3<f32>(0.04045));}
fn glyph(v:Out)->vec4<f32>{let sampled=textureSample(image,image_sampler,v.uv.xy);if v.uv.z<0.5{return sampled;}if v.tint.a<=0.{return vec4<f32>(0.);}let rgb=to_gamma(to_linear(v.tint.rgb)/v.tint.a)*v.tint.a;return vec4<f32>(rgb,v.tint.a)*vec4<f32>(to_gamma(sampled.rgb),sampled.a);}
@fragment fn fs_main(v:Out)->@location(0) vec4<f32>{return glyph(v);}
@fragment fn fs_linear(v:Out)->@location(0) vec4<f32>{let c=glyph(v);if v.uv.z<0.5||c.a<=0.{return c;}return vec4<f32>(to_linear(c.rgb/c.a)*c.a,c.a);}
"#;

/// Nearest original-grid-cell sampling shared by color, coverage and ID.
/// xy are global source-cell coordinates, zw are exact tile integer offsets.
const GLOBE_GRID_SAMPLE: &str = r#"
fn grid_sample(coords:vec4<f32>)->vec4<f32>{
    // Derivatives precede all material branches. The same shared interpolant
    // chooses tile ownership for colour, coverage and ID wrappers.
    let span=abs(dpdx(coords.xy))+abs(dpdy(coords.xy));
    if coords.z==1048576. {
        let tile=vec2<u32>(selector_word(image,5u),selector_word(image,6u));
        let size=vec2<u32>(selector_word(image,7u),selector_word(image,8u));
        let owner=vec2<i32>(floor(coords.xy));
        if any(owner<vec2<i32>(tile)) || any(owner>=vec2<i32>(tile+size)) {discard;}
        if selector_word(image,14u)!=1u {discard;}
        let selected=continuous_grid_select(image,coords.xy-vec2<f32>(tile),span,bitcast<f32>(selector_word(image,13u)));
        if selected.rank==0xffffffffu || selected.rgba.a<=0. {discard;}
        return selected.rgba;
    }
    let cell=vec2<i32>(floor(coords.xy));
    let local=cell-vec2<i32>(coords.zw);
    let size=vec2<i32>(textureDimensions(image));
    if any(local<vec2<i32>(0)) || any(local>=size) {discard;}
    return textureLoad(image,local,0);
}
"#;
fn globe_grid_shader()->String {
    format!("{}\n{}\n{}",crate::continuous_raster_selector::SELECTOR_WGSL,GLOBE_GRID_SAMPLE,r#"
struct In {@location(0) clip:vec4<f32>,@location(1) uv:vec4<f32>,@location(2) tint:vec4<f32>};
struct Out {@builtin(position) position:vec4<f32>,@location(0) uv:vec2<f32>,@location(1) @interpolate(flat) origin:vec2<f32>};
@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var image_sampler:sampler;
@vertex fn vs_main(v:In)->Out {var o:Out;o.position=v.clip;o.uv=v.uv.xy;o.origin=v.uv.zw;return o;}
@fragment fn fs_main(v:Out)->@location(0) vec4<f32>{return grid_sample(vec4<f32>(v.uv,v.origin));}
// Preserve existing Rgba8Unorm raster color behavior on sRGB attachments.
@fragment fn fs_linear(v:Out)->@location(0) vec4<f32>{return grid_sample(vec4<f32>(v.uv,v.origin));}
"#)
}

/// One material sample function is shared with coverage and ID rasterization.
fn globe_pattern_shader() -> String {
    format!(
        "{}\n{}",
        crate::globe_pattern::PATTERN_SAMPLE_WGSL,
        r#"
struct In {@location(0) clip:vec4<f32>};
struct Out {@builtin(position) position:vec4<f32>};
@vertex fn vs_main(v:In)->Out {var o:Out;o.position=v.clip;return o;}
@fragment fn fs_main(v:Out)->@location(0) vec4<f32> {
    return s100_pattern_sample(v.position.xy);
}
"#
    )
}
fn create_globe_pattern_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    samples: u32,
    material_layout: &wgpu::BindGroupLayout,
    coverage_layout: Option<&wgpu::BindGroupLayout>,
) -> Result<wgpu::RenderPipeline, String> {
    let mut source = globe_pattern_shader();
    if coverage_layout.is_some() {
        source = crate::coverage_clip::fragment_clipped_shader(
            &source,
            1,
            &[crate::coverage_clip::FragmentEntry {
                name: "fs_main",
                input_type: "Out",
                position_field: "position",
            }],
        )
        .map_err(|e| e.to_string())?;
    }
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Globe S100 pattern SurfaceOverlay"),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let mut layouts = vec![material_layout];
    if let Some(coverage) = coverage_layout {
        layouts.push(coverage);
    }
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Globe S100 pattern"),
        bind_group_layouts: &layouts,
        push_constant_ranges: &[],
    });
    let attributes = wgpu::vertex_attr_array![0=>Float32x4];
    Ok(
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Globe S100 pattern"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 36,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attributes,
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::GreaterEqual,
                stencil: Default::default(),
                bias: wgpu::DepthBiasState {
                    constant: 2,
                    slope_scale: 0.,
                    clamp: 0.,
                },
            }),
            multisample: wgpu::MultisampleState {
                count: samples,
                ..Default::default()
            },
            multiview: None,
            cache: None,
        }),
    )
}

#[path = "globe_pick.rs"]
mod picking;
pub use picking::GlobeDrawHit;
#[derive(Debug, Clone, Copy)]
pub struct GlobeVertex {
    pub ecef_m: [f64; 3],
    pub color: [f32; 4],
}
#[derive(Debug, Clone)]
pub struct GlobeMesh {
    pub vertices: Vec<GlobeVertex>,
    pub indices: Vec<u32>,
}
impl GlobeMesh {
    /// Parametric ellipsoid, not a sphere. Latitude subdivisions include both
    /// poles; duplicate seam/pole vertices permit later per-face attributes.
    pub fn ellipsoid(longitudes: u32, latitudes: u32, color: [f32; 4]) -> Result<Self, String> {
        if longitudes < 8 || latitudes < 4 {
            return Err("Insufficient globe subdivisions".into());
        }
        let count = (longitudes as u64 + 1)
            .checked_mul(latitudes as u64 + 1)
            .ok_or("Globe vertex count overflow")?;
        if count > 262144 {
            return Err("Globe vertex budget exceeded".into());
        }
        if !color
            .iter()
            .all(|x| x.is_finite() && (0. ..=1.).contains(x))
        {
            return Err("Invalid globe color".into());
        }
        let mut vertices = Vec::with_capacity(count as usize);
        let mut indices = Vec::with_capacity((longitudes as usize) * (latitudes as usize) * 6);
        for row in 0..=latitudes {
            let latitude = -90. + 180. * row as f64 / latitudes as f64;
            for column in 0..=longitudes {
                let longitude = -180. + 360. * column as f64 / longitudes as f64;
                let p = GeographicPosition::new(latitude, longitude).map_err(|e| e.to_string())?;
                vertices.push(GlobeVertex {
                    ecef_m: p.to_ecef(0.).map_err(|e| e.to_string())?,
                    color,
                });
            }
        }
        let stride = longitudes + 1;
        for row in 0..latitudes {
            for column in 0..longitudes {
                let a = row * stride + column;
                let b = a + 1;
                let c = a + stride;
                let d = c + 1;
                // Skip polar zero-area triangles; outward winding.
                if row > 0 {
                    indices.extend_from_slice(&[a, b, c]);
                }
                if row + 1 < latitudes {
                    indices.extend_from_slice(&[b, d, c]);
                }
            }
        }
        Ok(Self { vertices, indices })
    }
    pub fn validate(&self) -> Result<(), String> {self.validate_payload(false)}
    fn validate_payload(&self,source_grid:bool) -> Result<(),String> {
        if self.vertices.len() > 262144
            || self.indices.len() > 1572864
            || self.indices.len() % 3 != 0
        {
            return Err("Globe mesh budget/triangle count invalid".into());
        }
        if self
            .indices
            .iter()
            .any(|i| *i as usize >= self.vertices.len())
        {
            return Err("Globe mesh index out of bounds".into());
        }
        if self.vertices.iter().any(|v| {
            !v.ecef_m.iter().all(|x| x.is_finite())
                || !v
                    .color
                    .iter()
                    .all(|x| x.is_finite() && (0. ..=if source_grid {1048576.} else {1.}).contains(x))
        }) {
            return Err("Non-finite globe mesh/color".into());
        }
        if source_grid {
            let origin=self.vertices.first().map(|v|[v.color[2],v.color[3]]);
            if self.vertices.iter().any(|v|v.color[2].fract()!=0. || v.color[3].fract()!=0. || Some([v.color[2],v.color[3]])!=origin) {
                return Err("Source-grid origin must be exact constant integers per draw".into());
            }
        }
        Ok(())
    }
    /// Append without changing primitive order. Refuses aggregate budget growth.
    pub fn append(&mut self, other: &Self) -> Result<(), String> {
        self.validate()?;
        other.validate()?;
        if self.vertices.len() + other.vertices.len() > 262144
            || self.indices.len() + other.indices.len() > 1572864
        {
            return Err("Aggregate globe mesh budget exceeded".into());
        }
        let offset = self.vertices.len() as u32;
        self.vertices.extend_from_slice(&other.vertices);
        self.indices
            .extend(other.indices.iter().map(|x| x + offset));
        Ok(())
    }
}
/// Base occluders populate depth; chart layers preserve authored composition
/// order and test against that surface without hiding later translucent layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobeDepthMode {
    Occluder,
    SurfaceOverlay,
    /// Geographic texture draped on WGS84; depth-tested, unlike a billboard.
    SurfaceTexture,
    /// Source-cell xy / integer tile-origin zw, with nearest textureLoad.
    /// Distinct from SurfaceTexture normalized UV and billboard/font materials.
    SourceGridTexture,
    /// Screen-fixed portrayal whose geographic source has already been clipped
    /// against the exact WGS84 horizon and near/far planes. Like textured
    /// billboards, it uses painter order rather than the coarse Earth mesh's
    /// depth: ECEF chords and camera-facing extrusion are display geometry.
    /// This is not an occlusion policy for arbitrary unvalidated 3D meshes.
    ScreenOverlay,
}
fn depth_compare(mode:GlobeDepthMode,textured:bool)->wgpu::CompareFunction {
    if mode==GlobeDepthMode::ScreenOverlay || (textured && !matches!(mode,GlobeDepthMode::SurfaceTexture|GlobeDepthMode::SourceGridTexture)) {wgpu::CompareFunction::Always} else {wgpu::CompareFunction::GreaterEqual}
}
fn depth_bias(mode:GlobeDepthMode,textured:bool)->wgpu::DepthBiasState {
    if matches!(mode,GlobeDepthMode::SurfaceTexture|GlobeDepthMode::SourceGridTexture) || (mode==GlobeDepthMode::SurfaceOverlay && !textured) {wgpu::DepthBiasState{constant:2,slope_scale:0.,clamp:0.}} else {Default::default()}
}
pub struct GlobeLayer<'a> {
    pub mesh: &'a GlobeMesh,
    pub depth_mode: GlobeDepthMode,
}
/// A texture draw is a screen billboard whose geographic anchor was checked
/// against the ellipsoid before construction. It preserves portrayal order.
pub struct GlobeDraw<'a> {
    pub layer: GlobeLayer<'a>,
    pub texture: Option<&'a wgpu::BindGroup>,
    /// A draped surface material, mutually exclusive with billboard/font sampling.
    pub pattern: Option<&'a crate::globe_pattern::PatternMaterial>,
    /// Color32 font tint; None preserves premultiplied SVG sampling.
    pub font_color: Option<[u8; 4]>,
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ClipVertex {
    clip: [f32; 4],
    color: [f32; 4],
    font_color: [u8; 4],
}
pub struct GlobeSceneRenderer {
    pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    screen_pipeline: wgpu::RenderPipeline,
    texture_pipeline: Option<wgpu::RenderPipeline>,
    surface_texture_pipeline: Option<wgpu::RenderPipeline>,
    source_grid_texture_pipeline: Option<wgpu::RenderPipeline>,
    pattern_layout: wgpu::BindGroupLayout,
    pattern_pipeline: Option<wgpu::RenderPipeline>,
    ranges: Vec<(
        std::ops::Range<u32>,
        GlobeDepthMode,
        Option<wgpu::BindGroup>,
        Option<wgpu::BindGroup>,
    )>,
    pick_ranges: Vec<(
        u32,
        std::ops::Range<u32>,
        GlobeDepthMode,
        Option<wgpu::BindGroup>,
        Option<wgpu::BindGroup>,
    )>,
    pick_pipelines: Option<picking::PickPipelines>,
    pick_coverage_pipelines: Option<picking::PickPipelines>,
    coverage_pipelines: Option<coverage::CoveragePipelines>,
    coverage_binding: Option<coverage::CoverageBinding>,
    coverage_required: bool,
    coverage_batching_enabled: bool,
    prepared_epoch: u64,
    pick_texture_layout: Option<wgpu::BindGroupLayout>,
    prepared_viewport: Option<[f64; 2]>,
    depth: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
    vertices: Option<wgpu::Buffer>,
    indices: Option<wgpu::Buffer>,
    vertex_capacity: usize,
    index_capacity: usize,
    clips: Vec<ClipVertex>,
    index_count: u32,
    source_indices: Vec<u32>,
    buffer_allocations: u64,
    depth_allocations: u64,
    samples: u32,
    format: wgpu::TextureFormat,
    msaa: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
    msaa_allocations: u64,
    source_draws: usize,
    gpu_projection_requested: bool,
    gpu_projection: Option<GpuProjection>,
    retained_mesh: Option<Arc<GlobeMesh>>,
    source_vertices: Vec<SourceVertex>,
    source_references: Vec<u32>,
    retained_geometry: rustc_hash::FxHashMap<usize, Arc<GlobeMesh>>,
    small_source_frames: u32,
    prepared_vertex_count: usize,
    gpu_projection_used: bool,
    gpu_base_used: bool,
    uploaded_bytes: usize,
}
impl GlobeSceneRenderer {
    /// Use this layout when constructing immutable pattern bindings for this scene.
    pub fn pattern_layout(&self) -> &wgpu::BindGroupLayout {
        &self.pattern_layout
    }
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        Self::new_with_textures(device, format, None)
    }
    pub fn new_with_textures(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        texture_layout: Option<&wgpu::BindGroupLayout>,
    ) -> Self {
        Self::new_with_texture_samples(device, format, texture_layout, 1)
    }
    pub fn new_with_texture_samples(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        texture_layout: Option<&wgpu::BindGroupLayout>,
        samples: u32,
    ) -> Self {
        assert!(matches!(samples, 1 | 4));
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("WGS84 globe depth shader"),
            source: wgpu::ShaderSource::Wgsl(GLOBE_COLOR_SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("globe layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let attributes = wgpu::vertex_attr_array![0=>Float32x4,1=>Float32x4,2=>Unorm8x4];
        let make_pipeline = |mode: GlobeDepthMode| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("WGS84 depth globe"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 36,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attributes,
                    }],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: mode == GlobeDepthMode::Occluder,
                    depth_compare: if mode == GlobeDepthMode::ScreenOverlay {
                        wgpu::CompareFunction::Always
                    } else {
                        wgpu::CompareFunction::GreaterEqual
                    },
                    stencil: Default::default(),
                    bias: if mode != GlobeDepthMode::SurfaceOverlay {
                        Default::default()
                    } else {
                        wgpu::DepthBiasState {
                            constant: 2,
                            slope_scale: 0.,
                            clamp: 0.,
                        }
                    },
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                multiview: None,
                cache: None,
            })
        };
        let pipeline = make_pipeline(GlobeDepthMode::Occluder);
        let overlay_pipeline = make_pipeline(GlobeDepthMode::SurfaceOverlay);
        let screen_pipeline = make_pipeline(GlobeDepthMode::ScreenOverlay);
        let make_texture_pipeline = |mode:GlobeDepthMode| texture_layout.map(|textures| {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Globe premultiplied billboard"),
                source: wgpu::ShaderSource::Wgsl(if mode==GlobeDepthMode::SourceGridTexture {globe_grid_shader().into()} else {GLOBE_TEXTURE_SHADER.into()}),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Globe billboard layout"),
                bind_group_layouts: &[textures],
                push_constant_ranges: &[],
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Globe billboard"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 36,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attributes,
                    }],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(if format.is_srgb() {
                        "fs_linear"
                    } else {
                        "fs_main"
                    }),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: false,
                    depth_compare: depth_compare(mode,true),
                    stencil: Default::default(),
                    bias: depth_bias(mode,true),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                multiview: None,
                cache: None,
            })
        });
        let texture_pipeline=make_texture_pipeline(GlobeDepthMode::ScreenOverlay);
        let surface_texture_pipeline=make_texture_pipeline(GlobeDepthMode::SurfaceTexture);
        let source_grid_texture_pipeline=make_texture_pipeline(GlobeDepthMode::SourceGridTexture);
        Self {
            pipeline,
            overlay_pipeline,
            screen_pipeline,
            texture_pipeline,
            surface_texture_pipeline,
            source_grid_texture_pipeline,
            pattern_layout: crate::globe_pattern::create_pattern_layout(device),
            pattern_pipeline: None,
            ranges: Vec::new(),
            pick_ranges: Vec::new(),
            pick_pipelines: None,
            pick_coverage_pipelines: None,
            coverage_pipelines: None,
            coverage_binding: None,
            coverage_required: false,
            coverage_batching_enabled: false,
            prepared_epoch: 0,
            pick_texture_layout: texture_layout.cloned(),
            prepared_viewport: None,
            depth: None,
            vertices: None,
            indices: None,
            vertex_capacity: 0,
            index_capacity: 0,
            clips: Vec::new(),
            index_count: 0,
            source_indices: Vec::new(),
            buffer_allocations: 0,
            depth_allocations: 0,
            samples,
            format,
            msaa: None,
            msaa_allocations: 0,
            source_draws: 0,
            gpu_projection_requested: false,
            gpu_projection: None,
            retained_mesh: None,
            source_vertices: Vec::new(),
            source_references: Vec::new(),
            retained_geometry: Default::default(),
            small_source_frames: 0,
            prepared_vertex_count: 0,
            gpu_projection_used: false,
            gpu_base_used: false,
            uploaded_bytes: 0,
        }
    }
    pub fn set_gpu_projection_enabled(&mut self, enabled: bool) {
        if self.gpu_projection_requested && !enabled {
            self.gpu_projection = None;
        }
        self.gpu_projection_requested = enabled;
    }
    /// Immutable shared ownership prevents an address being reused or source
    /// vertices changing while their GPU representation remains resident.
    pub fn bind_retained_base(&mut self, mesh: Arc<GlobeMesh>) -> Result<(), String> {
        if self
            .retained_mesh
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &mesh))
        {
            return Ok(());
        }
        mesh.validate()?;
        if mesh.vertices.iter().any(|v| v.color[3] != 1.) {
            return Err("Retained globe base must be opaque".into());
        }
        self.retained_mesh = Some(mesh);
        self.gpu_projection = None;
        self.gpu_base_used = false;
        Ok(())
    }
    /// Owned immutable sources are eligible for bounded GPU residency. Draw
    /// order remains in the submitted layers; sources are matched by owned Arc.
    pub fn bind_retained_geometry(&mut self, owners: &[Arc<GlobeMesh>]) {
        self.retained_geometry.clear();
        for owner in owners {
            self.retained_geometry
                .insert(Arc::as_ptr(owner) as usize, owner.clone());
        }
    }
    /// Readback for native precision audits only; ordinary frames do not wait.
    pub fn read_prepared_clips(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<[f32; 4]>, String> {
        if self.prepared_viewport.is_none() {
            return Err("No prepared globe frame".into());
        }
        if self.prepared_vertex_count == 0 {
            return Ok(Vec::new());
        }
        let bytes = (self.prepared_vertex_count * 36) as u64;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globe projection precision audit"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("read actual globe clips"),
        });
        encoder.copy_buffer_to_buffer(
            self.vertices.as_ref().ok_or("No clip buffer")?,
            0,
            &staging,
            0,
            bytes,
        );
        queue.submit(Some(encoder.finish()));
        let (tx, rx) = std::sync::mpsc::channel();
        staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let view = staging.slice(..).get_mapped_range();
        let clips = bytemuck::cast_slice::<u8, ClipVertex>(&view)
            .iter()
            .map(|v| v.clip)
            .collect();
        drop(view);
        staging.unmap();
        Ok(clips)
    }
    /// Upload ordered source geometry for a camera. Mesh aggregate sizes are
    /// bounded; buffers and CPU clip capacity are reused for later camera frames.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        camera: &GlobeCamera,
        mesh: &GlobeMesh,
    ) -> Result<(), String> {
        self.prepare_layers(
            device,
            queue,
            camera,
            &[GlobeLayer {
                mesh,
                depth_mode: GlobeDepthMode::Occluder,
            }],
        )
    }
    pub fn prepare_layers(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        camera: &GlobeCamera,
        layers: &[GlobeLayer<'_>],
    ) -> Result<(), String> {
        let draws: Vec<_> = layers
            .iter()
            .map(|l| GlobeDraw {
                layer: GlobeLayer {
                    mesh: l.mesh,
                    depth_mode: l.depth_mode,
                },
                texture: None,
                pattern: None,
                font_color: None,
            })
            .collect();
        self.prepare_draws(device, queue, camera, &draws)
    }
    pub fn prepare_draws(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        camera: &GlobeCamera,
        draws: &[GlobeDraw<'_>],
    ) -> Result<(), String> {
        self.coverage_binding = None;
        self.prepared_viewport = None;
        self.index_count = 0;
        self.prepared_epoch = self
            .prepared_epoch
            .checked_add(1)
            .ok_or("Globe coverage epoch exhausted")?;
        if draws.iter().any(|d| d.texture.is_some()) && self.texture_pipeline.is_none() {
            return Err("Globe texture pipeline unavailable".into());
        }
        let has_patterns = draws.iter().any(|d| d.pattern.is_some());
        for draw in draws {
            if matches!(draw.layer.depth_mode,GlobeDepthMode::SurfaceTexture|GlobeDepthMode::SourceGridTexture) && (draw.texture.is_none() || draw.pattern.is_some() || draw.font_color.is_some()) {return Err("Geographic surface texture requires exclusive raster material".into());}
            if draw.pattern.is_some()
                && (draw.texture.is_some()
                    || draw.font_color.is_some()
                    || draw.layer.depth_mode != GlobeDepthMode::SurfaceOverlay)
            {
                return Err("Globe pattern requires exclusive SurfaceOverlay material".into());
            }
        }
        if has_patterns && self.pattern_pipeline.is_none() {
            self.pattern_pipeline = Some(create_globe_pattern_pipeline(
                device,
                self.format,
                self.samples,
                &self.pattern_layout,
                None,
            )?);
        }
        self.index_count = 0;
        self.ranges.clear();
        self.pick_ranges.clear();
        self.prepared_viewport = None;
        self.clips.clear();
        self.source_vertices.clear();
        self.source_references.clear();
        self.prepared_vertex_count = 0;
        self.uploaded_bytes = 0;
        let vertex_count: usize = draws.iter().map(|d| d.layer.mesh.vertices.len()).sum();
        let index_count: usize = draws.iter().map(|d| d.layer.mesh.indices.len()).sum();
        if vertex_count > 1048576 || index_count > 6291456 {
            return Err("Aggregate globe scene budget exceeded".into());
        }
        let params = CameraParams::new(camera);
        let gpu_path = self.gpu_projection_requested
            && !has_patterns
            && params.is_some()
            && GpuProjection::supported(device)
            && (vertex_count * 36).next_power_of_two()
                <= device.limits().max_storage_buffer_binding_size as usize
            && (vertex_count * 48).max(48).next_power_of_two()
                <= device.limits().max_storage_buffer_binding_size as usize
            && (vertex_count * 48).max(48).next_power_of_two() as u64
                <= device.limits().max_buffer_size
            && draws.iter().all(|d| {
                d.layer
                    .mesh
                    .vertices
                    .iter()
                    .all(|v| v.ecef_m.iter().all(|x| x.abs() <= 1e12))
            });
        if gpu_path && self.gpu_projection.is_none() {
            self.gpu_projection = Some(GpuProjection::new(device, self.retained_mesh.clone()));
        }
        if gpu_path {
            self.gpu_projection
                .as_mut()
                .unwrap()
                .begin_frame(&self.retained_geometry);
        }
        let retained = gpu_path
            && draws.first().is_some_and(|d| {
                d.layer.depth_mode == GlobeDepthMode::Occluder
                    && d.texture.is_none()
                    && d.font_color.is_none()
                    && self
                        .gpu_projection
                        .as_ref()
                        .unwrap()
                        .matches_base(d.layer.mesh)
            });
        let base_vertex_count = if retained {
            draws[0].layer.mesh.vertices.len()
        } else {
            0
        };
        let base_index_count = if retained {
            draws[0].layer.mesh.indices.len()
        } else {
            0
        };
        let rewrite_base_indices = retained && !self.gpu_base_used;
        if self.gpu_base_used != retained {
            if let Some(p) = self.gpu_projection.as_mut() {
                p.invalidate_binding();
            }
        }
        if gpu_path && !self.gpu_projection_used {
            self.clips.shrink_to_fit();
        }
        if !gpu_path && self.gpu_projection_used {
            self.source_vertices.shrink_to_fit();
            self.source_references.shrink_to_fit();
        }
        self.gpu_projection_used = gpu_path;
        self.gpu_base_used = retained;
        self.source_indices.clear();
        let indices = &mut self.source_indices;
        let mut overlays_started = false;
        let mut previous_texture: Option<&wgpu::BindGroup> = None;
        let mut previous_pattern: Option<&wgpu::BindGroup> = None;
        self.source_draws = draws.len();
        for (draw_index, draw) in draws.iter().enumerate() {
            if retained && draw_index == 0 {
                self.ranges.push((
                    0..base_index_count as u32,
                    GlobeDepthMode::Occluder,
                    None,
                    None,
                ));
                self.pick_ranges.push((
                    draw_index as u32,
                    0..base_index_count as u32,
                    GlobeDepthMode::Occluder,
                    None,
                    None,
                ));
                continue;
            }
            let layer = &draw.layer;
            if layer.depth_mode == GlobeDepthMode::Occluder && overlays_started {
                return Err("Globe occluders must precede ordered chart overlays".into());
            }
            overlays_started |= layer.depth_mode != GlobeDepthMode::Occluder;
            layer.mesh.validate_payload(layer.depth_mode==GlobeDepthMode::SourceGridTexture)?;
            if layer.depth_mode == GlobeDepthMode::Occluder
                && layer.mesh.vertices.iter().any(|v| v.color[3] != 1.)
            {
                return Err(
                    "Depth occluders must be opaque; use SurfaceOverlay for chart alpha".into(),
                );
            }
            let offset = if gpu_path {
                base_vertex_count + self.source_references.len()
            } else {
                self.clips.len()
            } as u32;
            let start = (base_index_count + indices.len()) as u32;
            if gpu_path && draw.texture.is_none() && draw.font_color.is_none() {
                if let Some(owner) = self
                    .retained_geometry
                    .get(&(layer.mesh as *const GlobeMesh as usize))
                {
                    if let Some(start) = self
                        .gpu_projection
                        .as_mut()
                        .unwrap()
                        .resident_area(device, queue, owner)
                    {
                        self.source_references.extend(
                            (0..layer.mesh.vertices.len() as u32)
                                .map(|i| 0x80000000u32 | (start + i)),
                        );
                    }
                }
            }
            let source_is_resident = gpu_path
                && self.source_references.len()
                    == offset as usize - base_vertex_count + layer.mesh.vertices.len();
            if !source_is_resident {
                for v in &layer.mesh.vertices {
                    if gpu_path {
                        if draw.font_color.is_some()
                            && camera.clip_ecef(v.ecef_m).map_err(|e| e.to_string())?[3] <= 0.
                        {
                            return Err("Globe glyph behind eye".into());
                        }
                        self.source_references
                            .push(self.source_vertices.len() as u32);
                        self.source_vertices
                            .push(SourceVertex::new(v, draw.font_color));
                        continue;
                    }
                    let homogeneous = camera
                        .clip_ecef_reverse_depth(v.ecef_m)
                        .map_err(|e| e.to_string())?;
                    // Font quads are screen-fixed with a visible surface anchor. Divide
                    // their constant depth in f64 before narrowing, just like map glyphs.
                    let clip = if draw.font_color.is_some() {
                        if homogeneous[3] <= 0. {
                            return Err("Globe glyph behind eye".into());
                        }
                        [
                            homogeneous[0] / homogeneous[3],
                            homogeneous[1] / homogeneous[3],
                            homogeneous[2] / homogeneous[3],
                            1.,
                        ]
                    } else {
                        homogeneous
                    }
                    .map(|x| x as f32);
                    if !clip.iter().all(|x| x.is_finite()) {
                        return Err("Globe GPU clip overflow".into());
                    }
                    self.clips.push(ClipVertex {
                        clip,
                        color: if draw.font_color.is_some() {
                            [v.color[0], v.color[1], 1., 1.]
                        } else {
                            v.color
                        },
                        font_color: draw.font_color.unwrap_or([255; 4]),
                    });
                }
            }
            indices.extend(layer.mesh.indices.iter().map(|i| i + offset));
            self.pick_ranges.push((
                draw_index as u32,
                start..(base_index_count + indices.len()) as u32,
                layer.depth_mode,
                draw.texture.cloned(),
                draw.pattern.map(|p| p.bind_group.clone()),
            ));
            // Joining adjacent index ranges retains primitive and blend order.
            // Both ranges use the same pipeline, depth policy and exact texture
            // object; color/font tint is already stored in each vertex.
            let same_texture = match (previous_texture, draw.texture) {
                (None, None) => true,
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                _ => false,
            };
            let pattern_binding = draw.pattern.map(|p| &p.bind_group);
            let same_pattern = match (previous_pattern, pattern_binding) {
                (None, None) => true,
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                _ => false,
            };
            if let Some((range, mode, _, _)) =
                self.ranges.last_mut().filter(|(range, mode, _, _)| {
                    range.end == start && *mode == layer.depth_mode && same_texture && same_pattern
                })
            {
                range.end = (base_index_count + indices.len()) as u32;
                debug_assert_eq!(*mode, layer.depth_mode);
            } else {
                self.ranges.push((
                    start..(base_index_count + indices.len()) as u32,
                    layer.depth_mode,
                    draw.texture.cloned(),
                    draw.pattern.map(|p| p.bind_group.clone()),
                ));
            }
            previous_texture = draw.texture;
            previous_pattern = pattern_binding;
        }
        if gpu_path && self.source_vertices.capacity() > 2 * self.source_vertices.len().max(1) {
            self.small_source_frames += 1;
        } else {
            self.small_source_frames = 0;
        }
        if self.small_source_frames >= 8 {
            let capacity = if self.source_vertices.is_empty() {
                0
            } else {
                self.source_vertices.len().next_power_of_two()
            };
            self.source_vertices.shrink_to(capacity);
            self.small_source_frames = 0;
        }
        let vertex_data = bytemuck::cast_slice(&self.clips);
        let index_data = bytemuck::cast_slice(indices);
        // Geometric growth avoids a new allocation for every slightly larger view.
        // Aggregate budgets above bound retained capacity; device limits are checked
        // before either allocation so unsupported devices fail without partial upload.
        let vertex_bytes = if gpu_path {
            vertex_count * 36
        } else {
            vertex_data.len()
        };
        let vertex_capacity = vertex_bytes
            .checked_next_power_of_two()
            .ok_or("Globe vertex capacity overflow")?;
        let index_bytes = (base_index_count + indices.len()) * 4;
        let index_capacity = index_bytes
            .checked_next_power_of_two()
            .ok_or("Globe index capacity overflow")?;
        if vertex_capacity > 32 * 1024 * 1024 || index_capacity > 32 * 1024 * 1024 {
            return Err("Globe retained GPU buffer budget exceeded".into());
        }
        if vertex_capacity as u64 > device.limits().max_buffer_size
            || index_capacity as u64 > device.limits().max_buffer_size
        {
            return Err("Globe buffers exceed device capacity".into());
        }
        if vertex_bytes > 0 {
            if vertex_bytes > self.vertex_capacity {
                self.vertices = Some(device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("camera-relative globe clips"),
                    size: vertex_capacity as u64,
                    usage: wgpu::BufferUsages::VERTEX
                        | wgpu::BufferUsages::COPY_DST
                        | wgpu::BufferUsages::COPY_SRC
                        | if GpuProjection::supported(device) {
                            wgpu::BufferUsages::STORAGE
                        } else {
                            wgpu::BufferUsages::empty()
                        },
                    mapped_at_creation: false,
                }));
                self.vertex_capacity = vertex_capacity;
                self.buffer_allocations += 1;
            }
            if gpu_path {
                self.gpu_projection.as_mut().unwrap().dispatch(
                    device,
                    queue,
                    self.vertices.as_ref().unwrap(),
                    self.buffer_allocations,
                    params.unwrap(),
                    &self.source_vertices,
                    &self.source_references,
                    base_vertex_count,
                )?;
                self.uploaded_bytes += self.gpu_projection.as_ref().unwrap().uploaded_bytes();
            } else {
                queue.write_buffer(self.vertices.as_ref().unwrap(), 0, vertex_data);
                self.uploaded_bytes += vertex_data.len();
            }
        }
        if index_bytes > 0 {
            let grew = index_bytes > self.index_capacity;
            if grew {
                self.indices = Some(device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("globe triangles"),
                    size: index_capacity as u64,
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
                self.index_capacity = index_capacity;
                self.buffer_allocations += 1;
            }
            if retained && (grew || rewrite_base_indices) {
                let base_indices = bytemuck::cast_slice(&draws[0].layer.mesh.indices);
                queue.write_buffer(self.indices.as_ref().unwrap(), 0, base_indices);
                self.uploaded_bytes += base_indices.len();
            }
            if !index_data.is_empty() {
                queue.write_buffer(
                    self.indices.as_ref().unwrap(),
                    (base_index_count * 4) as u64,
                    index_data,
                );
                self.uploaded_bytes += index_data.len();
            }
        }
        self.index_count = (base_index_count + indices.len()) as u32;
        self.prepared_vertex_count = vertex_count;
        self.prepared_viewport = Some(camera.viewport());
        Ok(())
    }
    pub fn resource_usage(&self) -> serde_json::Value {
        let draw_calls = self
            .coverage_binding
            .as_ref()
            .map_or(self.ranges.len(), |coverage| {
                if self.coverage_batching_enabled {
                    coverage.batches.len()
                } else {
                    self.pick_ranges
                        .iter()
                        .filter(|(draw, _, _, _, _)| {
                            !matches!(
                                coverage.actions[*draw as usize],
                                coverage::CoverageAction::Hidden
                            )
                        })
                        .count()
                }
            });
        let unclipped_pipelines = 3+usize::from(self.texture_pipeline.is_some())
            +usize::from(self.surface_texture_pipeline.is_some())
            +usize::from(self.source_grid_texture_pipeline.is_some())
            +usize::from(self.pattern_pipeline.is_some());
        let coverage_pipelines = self.coverage_pipelines.as_ref().map_or(0, |p| {
            3+usize::from(p.texture.is_some())+usize::from(p.surface_texture.is_some())
                +usize::from(p.source_grid_texture.is_some())+usize::from(p.pattern.is_some())
        });
        serde_json::json!({"source_draws":self.source_draws,"draw_calls":draw_calls,
            "pipeline_creations":unclipped_pipelines+coverage_pipelines,
            "pipeline_count_scope":"color pipelines; picking pipelines excluded",
            "coverage_batching_enabled":self.coverage_batching_enabled,
            "coverage_batch_capacity_bytes":self.coverage_binding.as_ref().map_or(0,|c|c.batches.capacity()*std::mem::size_of::<coverage::CoverageBatch>()),
            "coverage_required":self.coverage_required,"coverage_bound":self.coverage_binding.is_some(),
            "coverage_r8_pixel_bytes":self.coverage_pixel_bytes(),
            "coverage_mask_count":self.coverage_binding.as_ref().map_or(0, |c| c.mask_count),
            "coverage_byte_scope":"logical R8 payload; uniforms, allocator, pipelines and driver overhead excluded",
            "sample_count":self.samples,"msaa_allocations":self.msaa_allocations,
            "buffer_allocations":self.buffer_allocations,"depth_allocations":self.depth_allocations,
            "vertex_capacity_bytes":self.vertex_capacity,"index_capacity_bytes":self.index_capacity,
            "cpu_clip_capacity_bytes":self.clips.capacity()*std::mem::size_of::<ClipVertex>(),
            "cpu_index_capacity_bytes":self.source_indices.capacity()*4,
            "cpu_reference_capacity_bytes":self.source_references.capacity()*4,"uploaded_bytes":self.uploaded_bytes,
            "gpu_projection_requested":self.gpu_projection_requested,"gpu_projection_used":self.gpu_projection_used,
            "resident_base_used":self.gpu_base_used,"cpu_source_capacity_bytes":self.source_vertices.capacity()*48,
            "gpu_projection":self.gpu_projection.as_ref().map(GpuProjection::statistics)})
    }
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: [u32; 2],
        background: wgpu::Color,
    ) -> Result<(), String> {
        self.validate_coverage()?;
        if size.iter().any(|x| *x == 0)
            || size
                .iter()
                .any(|x| *x > device.limits().max_texture_dimension_2d)
        {
            return Err("Invalid globe target size".into());
        }
        let viewport = self
            .prepared_viewport
            .ok_or("Globe camera/geometry not prepared")?;
        if viewport
            .iter()
            .zip(size)
            .any(|(a, b)| (*a - b as f64).abs() > 0.01)
        {
            return Err("Globe target resized; prepare camera before rendering/picking".into());
        }
        if self.depth.as_ref().map(|d| (d.0, d.1)) != Some((size[0], size[1])) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("globe depth"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: self.samples,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            self.depth = Some((size[0], size[1], texture, view));
            self.depth_allocations += 1;
        }
        if self.samples > 1 && self.msaa.as_ref().map(|v| (v.0, v.1)) != Some((size[0], size[1])) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Globe multisample color"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: self.samples,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            self.msaa = Some((size[0], size[1], texture, view));
            self.msaa_allocations += 1;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("globe depth pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: self.msaa.as_ref().map_or(target, |v| &v.3),
                resolve_target: self.msaa.as_ref().map(|_| target),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(background),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &self.depth.as_ref().unwrap().3,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        if self.index_count > 0 {
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertices.as_ref().unwrap().slice(..));
            pass.set_index_buffer(
                self.indices.as_ref().unwrap().slice(..),
                wgpu::IndexFormat::Uint32,
            );
            if let Some(coverage) = &self.coverage_binding {
                let batched = coverage.batches.iter().map(|batch| {
                    let (_, mode, texture, pattern) = &self.ranges[batch.group];
                    (
                        &batch.range,
                        mode,
                        texture.as_ref(),
                        pattern.as_ref(),
                        &coverage.actions[batch.source],
                    )
                });
                let unbatched = self
                    .pick_ranges
                    .iter()
                    .take(if self.coverage_batching_enabled {
                        0
                    } else {
                        self.pick_ranges.len()
                    })
                    .map(|(draw, range, mode, texture, pattern)| {
                        (
                            range,
                            mode,
                            texture.as_ref(),
                            pattern.as_ref(),
                            &coverage.actions[*draw as usize],
                        )
                    });
                for (range, mode, texture, pattern, action) in batched.chain(unbatched) {
                    if matches!(action, coverage::CoverageAction::Hidden) {
                        continue;
                    }
                    if let coverage::CoverageAction::Clip(clip) = action {
                        let pipelines = self.coverage_pipelines.as_ref().unwrap();
                        if let Some(pattern) = pattern {
                            pass.set_pipeline(pipelines.pattern.as_ref().unwrap());
                            pass.set_bind_group(0, pattern, &[]);
                            pass.set_bind_group(1, &clip.bind_group, &[]);
                        } else if let Some(texture) = texture {
                            pass.set_pipeline(match mode {GlobeDepthMode::SourceGridTexture=>pipelines.source_grid_texture.as_ref().unwrap(),GlobeDepthMode::SurfaceTexture=>pipelines.surface_texture.as_ref().unwrap(),_=>pipelines.texture.as_ref().unwrap()});
                            pass.set_bind_group(0, texture, &[]);
                            pass.set_bind_group(1, &clip.bind_group, &[]);
                        } else {
                            pass.set_pipeline(match mode {
                                GlobeDepthMode::Occluder => &pipelines.base,
                                GlobeDepthMode::SurfaceOverlay | GlobeDepthMode::SurfaceTexture | GlobeDepthMode::SourceGridTexture => &pipelines.overlay,
                                GlobeDepthMode::ScreenOverlay => &pipelines.screen,
                            });
                            pass.set_bind_group(0, &clip.bind_group, &[]);
                        }
                    } else if let Some(pattern) = pattern {
                        pass.set_pipeline(self.pattern_pipeline.as_ref().unwrap());
                        pass.set_bind_group(0, pattern, &[]);
                    } else if let Some(texture) = texture {
                        pass.set_pipeline(match mode {GlobeDepthMode::SourceGridTexture=>self.source_grid_texture_pipeline.as_ref().unwrap(),GlobeDepthMode::SurfaceTexture=>self.surface_texture_pipeline.as_ref().unwrap(),_=>self.texture_pipeline.as_ref().unwrap()});
                        pass.set_bind_group(0, texture, &[]);
                    } else {
                        pass.set_pipeline(match mode {
                            GlobeDepthMode::Occluder => &self.pipeline,
                            GlobeDepthMode::SurfaceOverlay | GlobeDepthMode::SurfaceTexture | GlobeDepthMode::SourceGridTexture => &self.overlay_pipeline,
                            GlobeDepthMode::ScreenOverlay => &self.screen_pipeline,
                        });
                    }
                    pass.draw_indexed(range.clone(), 0, 0..1);
                }
            } else {
                for (range, mode, texture, pattern) in &self.ranges {
                    if let Some(pattern) = pattern {
                        pass.set_pipeline(self.pattern_pipeline.as_ref().unwrap());
                        pass.set_bind_group(0, pattern, &[]);
                    } else if let Some(texture) = texture {
                        pass.set_pipeline(match mode {GlobeDepthMode::SourceGridTexture=>self.source_grid_texture_pipeline.as_ref().unwrap(),GlobeDepthMode::SurfaceTexture=>self.surface_texture_pipeline.as_ref().unwrap(),_=>self.texture_pipeline.as_ref().unwrap()});
                        pass.set_bind_group(0, texture, &[]);
                    } else {
                        pass.set_pipeline(match mode {
                            GlobeDepthMode::Occluder => &self.pipeline,
                            GlobeDepthMode::SurfaceOverlay | GlobeDepthMode::SurfaceTexture | GlobeDepthMode::SourceGridTexture => &self.overlay_pipeline,
                            GlobeDepthMode::ScreenOverlay => &self.screen_pipeline,
                        });
                    }
                    pass.draw_indexed(range.clone(), 0, 0..1);
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::geodesy::{WGS84_A, WGS84_B};
    #[test]
    fn ellipsoid_mesh_indices_and_resource_limits() {
        let m = GlobeMesh::ellipsoid(360, 180, [0., 0.3, 0.8, 1.]).unwrap();
        m.validate().unwrap();
        assert_eq!(m.vertices.len(), 65341);
        assert_eq!(m.indices.len(), 386640);
        for v in m.vertices.iter().step_by(97) {
            let q = (v.ecef_m[0] / WGS84_A).powi(2)
                + (v.ecef_m[1] / WGS84_A).powi(2)
                + (v.ecef_m[2] / WGS84_B).powi(2);
            assert!((q - 1.).abs() < 1e-14);
        }
        assert!(GlobeMesh::ellipsoid(u32::MAX, u32::MAX, [0.; 4]).is_err());
        assert!(GlobeMesh::ellipsoid(360, 180, [f32::NAN; 4]).is_err());
        let mut bad = m.clone();
        bad.indices[0] = u32::MAX;
        assert!(bad.validate().is_err());
        let before = m.vertices.len();
        let mut dst = m.clone();
        assert!(dst.append(&bad).is_err());
        assert_eq!(dst.vertices.len(), before);
    }
}

#[cfg(test)]
mod pattern_shader_tests {
    use super::*;
    #[test]
    fn physical_pattern_color_and_coverage_shaders_validate() {
        let plain = globe_pattern_shader();
        assert!(plain.contains("s100_pattern_sample(v.position.xy)"));
        let masked = crate::coverage_clip::fragment_clipped_shader(
            &plain,
            1,
            &[crate::coverage_clip::FragmentEntry {
                name: "fs_main",
                input_type: "Out",
                position_field: "position",
            }],
        )
        .unwrap();
        for source in [&plain, &masked] {
            let module = wgpu::naga::front::wgsl::parse_str(source).unwrap();
            wgpu::naga::valid::Validator::new(
                wgpu::naga::valid::ValidationFlags::all(),
                wgpu::naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap();
        }
    }
}

#[cfg(test)]
#[path = "globe_pattern_scene_tests.rs"]
mod pattern_scene_hardware_tests;

#[cfg(test)]
#[path = "whole_motif_scene_tests.rs"]
mod whole_motif_hardware_tests;

#[cfg(test)] mod grid_shader_tests {
    use super::*;
    #[test] fn original_cell_color_and_coverage_shaders_validate() {
        let raw=globe_grid_shader();
        let clipped=crate::coverage_clip::fragment_clipped_shader(&raw,1,&[
            crate::coverage_clip::FragmentEntry{name:"fs_main",input_type:"Out",position_field:"position"},
            crate::coverage_clip::FragmentEntry{name:"fs_linear",input_type:"Out",position_field:"position"},
        ]).unwrap();
        for source in [&raw,&clipped] {
            let module=wgpu::naga::front::wgsl::parse_str(source).unwrap();
            wgpu::naga::valid::Validator::new(wgpu::naga::valid::ValidationFlags::all(),wgpu::naga::valid::Capabilities::all()).validate(&module).unwrap();
        }
    }
}

#[cfg(test)] mod source_grid_admission_tests {
    use super::*;
    #[test] fn typed_source_coordinates_do_not_weaken_legacy_color_admission() {
        let vertex=GlobeVertex{ecef_m:[1.,2.,3.],color:[8.,4.,4.,0.]};
        let mut mesh=GlobeMesh{vertices:vec![vertex;3],indices:vec![0,1,2]};
        assert!(mesh.validate().is_err());assert!(mesh.validate_payload(true).is_ok());
        for invalid in [-1.,1048577.,f32::NAN,f32::INFINITY] {mesh.vertices[0].color[0]=invalid;assert!(mesh.validate_payload(true).is_err());}
        mesh.vertices[0]=vertex;mesh.vertices[0].color[2]=4.5;assert!(mesh.validate_payload(true).is_err());
        mesh.vertices[0]=vertex;mesh.vertices[0].color[2]=5.;assert!(mesh.validate_payload(true).is_err());
        mesh.vertices[0]=vertex;mesh.indices[0]=3;assert!(mesh.validate_payload(true).is_err());
    }
}
