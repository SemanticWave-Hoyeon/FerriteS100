//! Ignored/windowless backend proof only. Not App, full S-100, or atomic publication proof.
use super::*;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::mpsc, time::Duration};
use wgpu::util::DeviceExt;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const FIXTURE_BUDGET: usize = 32 * 1024 * 1024;
struct Temp(PathBuf);
impl Temp {
    fn new(font: &[u8]) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferrite-font-gpu-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("Fonts")).unwrap();
        std::fs::write(path.join("portrayal_catalogue.xml"),"<portrayalCatalog><fonts><font id='same'><fileName>font.ttf</fileName><fileType>Font</fileType><fileFormat>TTF</fileFormat></font></fonts></portrayalCatalog>").unwrap();
        std::fs::write(path.join("Fonts/font.ttf"), font).unwrap();
        Self(path)
    }
    fn capture(&self) -> BoundFontReference {
        let sources = ferrite_portrayal_catalog::CatalogueSources::capture(&self.0).unwrap();
        ferrite_portrayal_catalog::BoundFontDeclarations::from_sources(sources)
            .unwrap()
            .resolve("same")
            .unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn input(context: &egui::Context, extent: [u32; 2], density: f32) -> egui::RawInput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(extent[0] as f32 / density, extent[1] as f32 / density),
        )),
        max_texture_side: Some(2048),
        ..Default::default()
    };
    input
        .viewports
        .entry(context.viewport_id())
        .or_default()
        .native_pixels_per_point = Some(density);
    input
}
fn ui_context(extent: [u32; 2], density: f32) -> egui::Context {
    let context = egui::Context::default();
    context.set_fonts(crate::chart_fonts::chart_font_definitions());
    context.begin_pass(input(&context, extent, density));
    let _ = context.end_pass();
    context
}
fn glyph_jobs(
    owner: &ReferencedChartOwner,
    font: &BoundFontReference,
) -> Vec<egui::epaint::ClippedShape> {
    let ppp = owner.ppp;
    let painter = owner.context.layer_painter(egui::LayerId::background());
    let galley = painter.layout_no_wrap(
        "Map 12.3 AB".into(),
        egui::FontId {
            size: 22. / ppp,
            family: font_family(font),
        },
        egui::Color32::WHITE,
    );
    assert!(!galley.rows.is_empty());
    assert!(galley.rect.width() > 0.);
    let shape = egui::epaint::ClippedShape {
        clip_rect: egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(owner.extent[0] as f32 / ppp, owner.extent[1] as f32 / ppp),
        ),
        shape: egui::Shape::galley(
            egui::pos2(12. / ppp, 12. / ppp),
            galley,
            egui::Color32::WHITE,
        ),
    };
    vec![shape]
}
pub(crate) struct DrawResult {
    pub(crate) bytes: Vec<u8>,
    pub(crate) vertices: usize,
    pub(crate) indices: usize,
}
fn draw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    owner: &mut ReferencedChartOwner,
    shapes: Vec<egui::epaint::ClippedShape>,
) -> DrawResult {
    let context = owner.context.clone(); // Same exclusively owned test Context, not a UI owner.
    let frame = (&context, owner.ppp, owner.extent);
    draw_chart_fixture(device, queue, frame, shapes, |ids| {
        owner.upload(device, queue).unwrap();
        ids.iter()
            .map(|id| owner.bind_group(*id).unwrap())
            .collect()
    })
}
/// Test-only shared actual CHART_TEXT_SHADER/pass factory/readback path.
pub(crate) fn draw_chart_fixture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    (context, ppp, extent): (&egui::Context, f32, [u32; 2]),
    shapes: Vec<egui::epaint::ClippedShape>,
    prepare_bindings: impl FnOnce(&[egui::TextureId]) -> Vec<wgpu::BindGroup>,
) -> DrawResult {
    let view_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("font-probe-view"),
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
    let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("font-probe-atlas"),
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
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("font-probe-layout"),
        bind_group_layouts: &[&view_layout, &texture_layout],
        push_constant_ranges: &[],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("actual-chart-text-shader"),
        source: wgpu::ShaderSource::Wgsl(crate::pipeline::CHART_TEXT_SHADER.into()),
    });
    // Shared production factory; no copied or simplified glyph shader/pass descriptor.
    let pipeline = crate::pipeline::create_chart_text_pipeline(device, &layout, &shader, FORMAT, 1);
    let uniforms = crate::ViewUniforms::new(extent[0] as f32, extent[1] as f32, 1.);
    let ub = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("font-probe-uniform"),
        contents: bytemuck::bytes_of(&uniforms),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let view_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("font-probe-view"),
        layout: &view_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: ub.as_entire_binding(),
        }],
    });
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("font-probe-target"),
        size: wgpu::Extent3d {
            width: extent[0],
            height: extent[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&Default::default());
    let row = (extent[0] * 4).div_ceil(256) * 256;
    let read_bytes = (row as usize) * (extent[1] as usize);
    let mut streams = Vec::new();
    let (mut vertex_count, mut index_count) = (0usize, 0usize);
    let mut payload = read_bytes
        .checked_add((extent[0] * extent[1] * 4) as usize)
        .unwrap();
    for job in context.tessellate(shapes, ppp) {
        let egui::epaint::Primitive::Mesh(mesh) = job.primitive else {
            panic!("Unexpected glyph callback");
        };
        assert_eq!(mesh.texture_id, egui::TextureId::Managed(0));
        if mesh.indices.is_empty() {
            continue;
        }
        let vertices = mesh
            .vertices
            .iter()
            .map(|v| crate::ChartTextVertex {
                position: [v.pos.x * ppp, v.pos.y * ppp],
                uv: [v.uv.x, v.uv.y],
                color: v.color.to_array(),
            })
            .collect::<Vec<_>>();
        vertex_count += vertices.len();
        index_count += mesh.indices.len();
        payload = payload
            .checked_add(
                std::mem::size_of_val(vertices.as_slice())
                    + std::mem::size_of_val(mesh.indices.as_slice()),
            )
            .unwrap();
        assert!(payload <= FIXTURE_BUDGET);
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("font-probe-glyphs"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("font-probe-glyph-indices"),
            contents: bytemuck::cast_slice(&mesh.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        streams.push((vb, ib, mesh.texture_id, mesh.indices.len() as u32));
    }
    assert!(vertex_count > 0 && index_count > 0);
    let ids = streams.iter().map(|s| s.2).collect::<Vec<_>>();
    let bindings = prepare_bindings(&ids);
    assert_eq!(bindings.len(), streams.len());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("font-probe-readback"),
        size: read_bytes as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("actual-chart-text-probe"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &view_bg, &[]);
        for ((vb, ib, _, count), binding) in streams.iter().zip(&bindings) {
            pass.set_bind_group(1, binding, &[]);
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..*count, 0, 0..1);
        }
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(extent[1]),
            },
        },
        wgpu::Extent3d {
            width: extent[0],
            height: extent[1],
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));
    let slice = readback.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |status| {
        let _ = tx.send(status);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("GPU map completion deadline")
        .unwrap();
    let mapped = slice.get_mapped_range();
    let mut rgba = Vec::with_capacity((extent[0] * extent[1] * 4) as usize);
    for padded in mapped.chunks_exact(row as usize) {
        rgba.extend_from_slice(&padded[..extent[0] as usize * 4]);
    }
    drop(mapped);
    readback.unmap();
    assert!(rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
    DrawResult {
        bytes: rgba,
        vertices: vertex_count,
        indices: index_count,
    }
}
#[test]
#[ignore = "Explicit Root-only windowless GPU; invoke with process deadline; no App or full standard qualification"]
fn referenced_pc_fonts_actual_chart_pipeline_gpu() {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: None,
                power_preference: wgpu::PowerPreference::default(),
                force_fallback_adapter: false,
            })
            .await
            .expect("No GPU adapter: proof NOT performed");
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("font-proof"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let regular_fixture = Temp::new(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Regular.ttf"
        ));
        let bold_fixture = Temp::new(include_bytes!(
            "../../../Catalogues/PC/S-421/Fonts/OpenSans-Bold.ttf"
        ));
        let regular = regular_fixture.capture();
        let bold = bold_fixture.capture();
        assert_eq!(regular.reference(), bold.reference());
        assert_ne!(font_family(&regular), font_family(&bold));
        let extent = [256, 128];
        let source = ui_context(extent, 1.);
        let mut results = Vec::new();
        for font in [&regular, &bold] {
            let mut owner = ReferencedChartOwner::prepare(
                &device,
                FORMAT,
                &source,
                extent,
                1.,
                [(font, "Map 12.3 AB")],
            )
            .unwrap();
            let first_jobs = glyph_jobs(&owner, font);
            owner.end_metrics().unwrap();
            let first = draw(&device, &queue, &mut owner, first_jobs);
            assert!(owner
                .matches(extent, 1., 1., [(font, "Map 12.3 AB")])
                .unwrap());
            owner.begin_metrics();
            let next_jobs = glyph_jobs(&owner, font);
            owner.end_metrics().unwrap();
            let second = draw(&device, &queue, &mut owner, next_jobs);
            assert_eq!(first.bytes, second.bytes);
            assert_eq!(first.vertices, second.vertices);
            assert_eq!(first.indices, second.indices);
            assert!(!owner
                .matches([320, 128], 1., 1., [(font, "Map 12.3 AB")])
                .unwrap());
            assert!(owner.begin_display([320, 128], 1., 1.).is_err());
            assert!(!owner
                .matches(extent, 2., 2., [(font, "Map 12.3 AB")])
                .unwrap());
            assert!(owner
                .matches(extent, 1., 1., [(font, "\u{10ffff}")])
                .is_err());
            let unknown = ReferencedChartOwner::prepare(
                &device,
                FORMAT,
                &source,
                extent,
                1.,
                [(font, "\u{10ffff}")],
            );
            assert!(unknown.is_err());
            results.push(first);
        }
        assert_ne!(
            results[0].bytes, results[1].bytes,
            "Regular/Bold same-ID owners must not alias"
        );
        for (size, density) in [([320, 128], 1.), ([512, 256], 2.)] {
            let fresh_ui = ui_context(size, density);
            let mut fresh = ReferencedChartOwner::prepare(
                &device,
                FORMAT,
                &fresh_ui,
                size,
                density,
                [(&regular, "Map 12.3 AB")],
            )
            .unwrap();
            let jobs = glyph_jobs(&fresh, &regular);
            fresh.end_metrics().unwrap();
            let a = draw(&device, &queue, &mut fresh, jobs);
            fresh.begin_display(size, density, density).unwrap();
            let jobs = glyph_jobs(&fresh, &regular);
            fresh.end_display().unwrap();
            let b = draw(&device, &queue, &mut fresh, jobs);
            assert_eq!(a.bytes, b.bytes);
            fresh_ui.begin_pass(input(&fresh_ui, size, density));
            assert!(
                fresh_ui.end_pass().textures_delta.set.is_empty(),
                "Private glyph shaping must not dirty live UI atlas"
            );
        }
        source.begin_pass(input(&source, extent, 1.));
        assert!(source.end_pass().textures_delta.set.is_empty());
        assert!(device.pop_error_scope().await.is_none());
        let rows=results.iter().map(|r| serde_json::json!({"rgba_bytes":r.bytes.len(),"sha256":format!("{:x}",Sha256::digest(&r.bytes)),"vertices":r.vertices,"indices":r.indices})).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::json!({"scope":"windowless actual CHART_TEXT_SHADER/shared production pipeline + captured original TTF/private owner atlas; no App/fullS100/atomic/pick proof","backend":format!("{:?}",info.backend),"adapter":info.name,"fixture_buffer_cap":FIXTURE_BUDGET,"rows":rows,"regular_bold_distinct":true,"repeat_exact":true,"dpi_resize_fresh_owner":true,"ui_atlas_isolated":true,"missing_glyph_rejected":true})
        );
    });
}

fn bundled_jobs(
    context: &egui::Context,
    extent: [u32; 2],
    text: &str,
) -> Vec<egui::epaint::ClippedShape> {
    let ppp = context.pixels_per_point();
    let painter = context.layer_painter(egui::LayerId::background());
    [
        egui::FontFamily::Proportional,
        egui::FontFamily::Name("ChartMedium".into()),
        egui::FontFamily::Name("ChartBold".into()),
        egui::FontFamily::Monospace,
    ]
    .into_iter()
    .enumerate()
    .map(|(row, family)| egui::epaint::ClippedShape {
        clip_rect: egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(extent[0] as f32 / ppp, extent[1] as f32 / ppp),
        ),
        shape: egui::Shape::galley(
            egui::pos2(12. / ppp, (12. + row as f32 * 26.) / ppp),
            painter.layout_no_wrap(
                text.to_owned(),
                egui::FontId {
                    size: 22. / ppp,
                    family,
                },
                egui::Color32::WHITE,
            ),
            egui::Color32::WHITE,
        ),
    })
    .collect()
}

#[test]
#[ignore = "Explicit Root-only windowless GPU; private bundled atlas prerequisite, not App atomic publication"]
fn bundled_chart_fonts_private_candidate_preserves_old_gpu_atlas() {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: None,
                power_preference: wgpu::PowerPreference::default(),
                force_fallback_adapter: false,
            })
            .await
            .expect("No GPU adapter: proof NOT performed");
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("bundled-private-font-proof"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut rows = Vec::new();
        for (extent, density) in [([256, 128], 1.), ([512, 256], 2.)] {
            let source = egui::Context::default();
            source.set_fonts(crate::chart_fonts::chart_font_definitions());
            source.begin_pass(input(&source, extent, density));
            let baseline_jobs = bundled_jobs(&source, extent, "Map 12.3 AB");
            let baseline_output = source.end_pass();
            let mut baseline_atlas = egui_wgpu::Renderer::new(&device, FORMAT, None, 1, false);
            for (id, delta) in &baseline_output.textures_delta.set {
                baseline_atlas.update_texture(&device, &queue, *id, delta);
            }
            let baseline = draw_chart_fixture(
                &device,
                &queue,
                (&source, source.pixels_per_point(), extent),
                baseline_jobs,
                |ids| {
                    ids.iter()
                        .map(|id| baseline_atlas.texture(id).unwrap().bind_group.clone())
                        .collect()
                },
            );
            let mut published = ReferencedChartOwner::prepare(
                &device,
                FORMAT,
                &source,
                extent,
                density,
                std::iter::empty(),
            )
            .unwrap();
            let jobs = bundled_jobs(&published.context, extent, "Map 12.3 AB");
            published.end_metrics().unwrap();
            let first = draw(&device, &queue, &mut published, jobs);
            assert!(
                baseline.bytes == first.bytes,
                "Private bundled glyphs differ from UI font baseline"
            );
            assert_eq!(
                (baseline.vertices, baseline.indices),
                (first.vertices, first.indices)
            );
            assert!(published
                .matches(extent, density, density, std::iter::empty())
                .unwrap());
            let mut candidate = ReferencedChartOwner::prepare(
                &device,
                FORMAT,
                &source,
                extent,
                density,
                std::iter::empty(),
            )
            .unwrap();
            let jobs = bundled_jobs(&candidate.context, extent, "xyz 9876 QWERTY");
            candidate.end_metrics().unwrap();
            let other = draw(&device, &queue, &mut candidate, jobs);
            assert!(
                first.bytes != other.bytes,
                "Candidate witness must contain different glyphs"
            );
            // Reject a separate candidate; the retained owner remains usable.
            assert!(ReferencedChartOwner::prepare(
                &device,
                FORMAT,
                &source,
                [0, extent[1]],
                density,
                std::iter::empty(),
            )
            .is_err());
            drop(candidate);
            published.begin_display(extent, density, density).unwrap();
            let jobs = bundled_jobs(&published.context, extent, "Map 12.3 AB");
            published.end_display().unwrap();
            let retained = draw(&device, &queue, &mut published, jobs);
            assert!(
                first.bytes == retained.bytes,
                "Candidate upload changed retained private atlas output"
            );
            assert_eq!(
                (first.vertices, first.indices),
                (retained.vertices, retained.indices)
            );
            source.begin_pass(input(&source, extent, density));
            assert!(
                source.end_pass().textures_delta.set.is_empty(),
                "Private shaping dirtied source UI atlas"
            );
            rows.push(serde_json::json!({
                "extent": extent, "density": density, "rgba_bytes": first.bytes.len(),
                "rgba_sha256": format!("{:x}", Sha256::digest(&first.bytes)),
                "vertices": first.vertices, "indices": first.indices,
                "builtin_families":4,"same_ui_baseline":true,"retained_after_candidate_upload_and_rejection":true,
            }));
        }
        assert!(device.pop_error_scope().await.is_none());
        assert!(device.pop_error_scope().await.is_none());
        println!(
            "{}",
            serde_json::json!({
                "scope":"Actual chart text pipeline/private built-in fonts, independent Context+GPU atlas; no App/private full-scene emission/atomic publication/FPS claim",
                "backend":format!("{:?}",info.backend),"adapter":info.name,"rows":rows,
            })
        );
    });
}
