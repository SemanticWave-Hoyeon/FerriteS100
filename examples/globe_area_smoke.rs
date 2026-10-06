//! Actual official-PC drawing output draped onto WGS84. Solid areas only:
//! this is a backend audit, not complete globe chart portrayal or a main-app mode.
use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera};
use ferrite_render::{
    instruction_visible, AreaFillType, DrawingInstruction, RenderContext, Viewport,
};
use ferrite_wgpu::{
    globe_portrayal::{drape_area, DrapingLimits},
    globe_scene::{GlobeDepthMode, GlobeLayer, GlobeMesh, GlobeSceneRenderer},
    GpuState,
};
use sha2::Digest;
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct App {
    input: PathBuf,
    out: PathBuf,
    lat: f64,
    lon: f64,
}
fn compact_view(mesh: &GlobeMesh, camera: &GlobeCamera) -> GlobeMesh {
    let v = camera.viewport();
    let mut indices = Vec::new();
    for t in mesh.indices.chunks(3) {
        let clips: Vec<_> = t
            .iter()
            .map(|i| camera.clip_ecef(mesh.vertices[*i as usize].ecef_m).unwrap())
            .collect();
        if clips.iter().all(|c| c[3] > 0.) {
            let points: Vec<_> = clips
                .iter()
                .map(|c| {
                    [
                        (c[0] / c[3] + 1.) * v[0] / 2.,
                        (1. - c[1] / c[3]) * v[1] / 2.,
                    ]
                })
                .collect();
            if points.iter().all(|p| p[0] < -1.)
                || points.iter().all(|p| p[0] > v[0] + 1.)
                || points.iter().all(|p| p[1] < -1.)
                || points.iter().all(|p| p[1] > v[1] + 1.)
            {
                continue;
            }
        }
        indices.extend_from_slice(t);
    }
    let mut map = vec![u32::MAX; mesh.vertices.len()];
    let mut vertices = Vec::new();
    for i in &mut indices {
        let old = *i as usize;
        if map[old] == u32::MAX {
            map[old] = vertices.len() as u32;
            vertices.push(mesh.vertices[old]);
        }
        *i = map[old];
    }
    GlobeMesh { vertices, indices }
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Official PC solid areas on WGS84")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let gpu = pollster::block_on(GpuState::new(w)).unwrap();
        let size = [gpu.config.width, gpu.config.height];
        let data = std::fs::read(&self.input).unwrap();
        assert!(data.len() <= 64 * 1024 * 1024);
        let instructions: Vec<DrawingInstruction> = bincode::deserialize(&data).unwrap();
        let mut ctx = RenderContext::new(Viewport::new(size[0] as f32, size[1] as f32));
        ctx.settings.current_datetime = Some("2026-10-04T09:30:00+09:00".into());
        ctx.set_instructions_from_cache(instructions);
        ctx.get_sorted_instructions();
        let (dates, _, errors) = ctx.date_visibility();
        assert_eq!(errors, 0);
        let mut renderer = GlobeSceneRenderer::new(&gpu.device, gpu.config.format);
        let base =
            GlobeMesh::ellipsoid(360, 180, [40. / 255., 80. / 255., 140. / 255., 1.]).unwrap();
        let mut rows = Vec::new();
        for (name, range, tilt) in [("overhead", 35000., 0.), ("tilted", 35000., 35.)] {
            let camera = GlobeCamera::orbit(
                GeographicPosition::new(self.lat, self.lon).unwrap(),
                range,
                0.,
                tilt,
                size.map(|x| x as f64),
                45.,
                range / 10000.,
                1e9,
            )
            .unwrap();
            let mut meshes = Vec::new();
            let mut sources = Vec::new();
            let mut failures = Vec::new();
            let mut unsupported = 0;
            let start = std::time::Instant::now();
            let mut refinements = 0;
            for (index, i) in ctx.raw_instructions().iter().enumerate() {
                let DrawingInstruction::Area(a) = i else {
                    continue;
                };
                if !dates[index] || !instruction_visible(i, 100000, None, None) {
                    continue;
                }
                if !matches!(a.fill, AreaFillType::Solid(_)) {
                    unsupported += 1;
                    continue;
                }
                match drape_area(a,&camera,DrapingLimits::default()) {
     Ok((m,stats))=>{refinements+=stats.refinements;let m=compact_view(&m,&camera);if !m.indices.is_empty() {sources.push((index,a.cell_index,a.feature_id,m.vertices.len(),m.indices.len()));meshes.push(m);}},
     Err(error)=>failures.push(serde_json::json!({"index":index,"cell":a.cell_index,"feature":a.feature_id,"error":error})),
    }
            }
            assert!(failures.is_empty(), "draping failed: {failures:?}");
            assert!(meshes.len() > 10, "No actual chart areas");
            let mut layers = vec![GlobeLayer {
                mesh: &base,
                depth_mode: GlobeDepthMode::Occluder,
            }];
            layers.extend(meshes.iter().map(|mesh| GlobeLayer {
                mesh,
                depth_mode: GlobeDepthMode::SurfaceOverlay,
            }));
            renderer
                .prepare_layers(&gpu.device, &gpu.queue, &camera, &layers)
                .unwrap();
            let preparation_seconds = start.elapsed().as_secs_f64();
            let extent = wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            };
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("actual globe area image"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: gpu.config.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let row = (size[0] * 4 + 255) / 256 * 256;
            let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("globe area readback"),
                size: row as u64 * size[1] as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            renderer
                .render(
                    &gpu.device,
                    &mut encoder,
                    &texture.create_view(&Default::default()),
                    size,
                    wgpu::Color::BLACK,
                )
                .unwrap();
            let frame = gpu.surface.get_current_texture().unwrap();
            renderer
                .render(
                    &gpu.device,
                    &mut encoder,
                    &frame.texture.create_view(&Default::default()),
                    size,
                    wgpu::Color::BLACK,
                )
                .unwrap();
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(size[1]),
                    },
                },
                extent,
            );
            gpu.queue.submit([encoder.finish()]);
            frame.present();
            let (tx, rx) = std::sync::mpsc::channel();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
            gpu.device.poll(wgpu::Maintain::Wait);
            rx.recv().unwrap().unwrap();
            let data = buffer.slice(..).get_mapped_range();
            let mut pixels = Vec::new();
            let bgra = matches!(
                gpu.config.format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
            );
            for line in data.chunks(row as usize) {
                for p in line[..size[0] as usize * 4].chunks(4) {
                    if bgra {
                        pixels.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                    } else {
                        pixels.extend_from_slice(p);
                    }
                }
            }
            drop(data);
            buffer.unmap();
            let image = image::RgbaImage::from_raw(size[0], size[1], pixels).unwrap();
            image.save(self.out.join(format!("{name}.png"))).unwrap();
            let chart_pixels = image
                .pixels()
                .filter(|p| p[0] > 60 || p[1] > 100 || p[2] < 100 && p[0] > 0)
                .count();
            assert!(chart_pixels > 1000, "Chart fills not visible");
            rows.push(serde_json::json!({"case":name,"input_sha256":sha2::Digest::finalize(sha2::Sha256::new_with_prefix(std::fs::read(&self.input).unwrap())).iter().map(|b|format!("{b:02x}")).collect::<String>(),"source_instruction_count":ctx.instruction_count(),"actual_pc_solid_area_count":meshes.len(),"source_areas":sources,"unsupported_area_fills":unsupported,"failures":failures,"refinements":refinements,"preparation_seconds":preparation_seconds,"chart_pixels":chart_pixels,"gpu":gpu.gpu_name,"app_integration_verified":false,"symbols_and_text_rendered":false,"complete_chart_portrayal":false}));
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
}
fn main() {
    let mut a = std::env::args().skip(1);
    let mut app = App {
        input: a.next().unwrap().into(),
        out: a.next().unwrap().into(),
        lat: a.next().unwrap().parse().unwrap(),
        lon: a.next().unwrap().parse().unwrap(),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
