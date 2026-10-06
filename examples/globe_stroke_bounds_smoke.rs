//! Native whole-frame oracle for screen-fixed stroke bounds and dash phase.
use ferrite_kernel::{
    geodesy::{direct, GeographicPosition},
    globe_camera::GlobeCamera,
};
use ferrite_render::{CapStyle, Color, JoinStyle, LineInstruction, LineStyle, WorldPoint};
use ferrite_wgpu::{
    globe_lines::drape_line_with_spans_and_culling,
    globe_scene::{GlobeDepthMode, GlobeLayer, GlobeMesh, GlobeSceneRenderer},
    GpuState,
};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
fn capture(
    gpu: &GpuState,
    renderer: &mut GlobeSceneRenderer,
    camera: &GlobeCamera,
    earth: &GlobeMesh,
    meshes: &[GlobeMesh],
    size: [u32; 2],
) -> Vec<u8> {
    let layers: Vec<_> = std::iter::once(GlobeLayer {
        mesh: earth,
        depth_mode: GlobeDepthMode::Occluder,
    })
    .chain(meshes.iter().map(|mesh| GlobeLayer {
        mesh,
        depth_mode: GlobeDepthMode::SurfaceOverlay,
    }))
    .collect();
    renderer
        .prepare_layers(&gpu.device, &gpu.queue, camera, &layers)
        .unwrap();
    let extent = wgpu::Extent3d {
        width: size[0],
        height: size[1],
        depth_or_array_layers: 1,
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bounds oracle"),
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
        label: Some("bounds readback"),
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
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |v| tx.send(v).unwrap());
    gpu.device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    let data = buffer.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut result = Vec::new();
    for line in data.chunks(row as usize) {
        for p in line[..size[0] as usize * 4].chunks(4) {
            if bgra {
                result.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
            } else {
                result.extend_from_slice(p);
            }
        }
    }
    result
}
fn point(camera: &GlobeCamera, x: f64, y: f64) -> WorldPoint {
    let p = camera.pick([x, y]).unwrap().unwrap().geodetic.surface;
    WorldPoint::new(p.longitude(), p.latitude())
}
struct App {
    out: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(900, 600))
                    .with_title("WGS84 stroke bounds whole-frame oracle"),
            )
            .unwrap(),
        );
        let gpu = pollster::block_on(GpuState::new(w)).unwrap();
        let size = [900, 600];
        let mut renderer = GlobeSceneRenderer::new(&gpu.device, gpu.config.format);
        let earth = GlobeMesh::ellipsoid(120, 60, [0.1, 0.2, 0.3, 1.]).unwrap();
        let mut rows = Vec::new();
        for lat in [0., 70., 89.] {
            for tilt in [0., 45., 70.] {
                let focus = GeographicPosition::new(lat, 179.9).unwrap();
                let camera = GlobeCamera::orbit(
                    focus,
                    30000.,
                    0.,
                    tilt,
                    size.map(|x| x as f64),
                    45.,
                    1.,
                    1e8,
                )
                .unwrap();
                let ppm = 4.;
                let mut center = LineInstruction::new(vec![
                    point(&camera, -100., 300.),
                    point(&camera, 1000., 300.),
                ]);
                center.style = LineStyle::solid_mm(Color::rgb(0., 0., 1.), 0.8);
                center.style.dash_cycle =
                    Some(ferrite_kernel::DashCycle::new(10., vec![(2., 3.)]).unwrap());
                center.style.cap = match lat as i32 {
                    0 => CapStyle::Butt,
                    70 => CapStyle::Round,
                    _ => CapStyle::Square,
                };
                center.style.join = match tilt as i32 {
                    0 => JoinStyle::Miter,
                    45 => JoinStyle::Round,
                    _ => JoinStyle::Bevel,
                };
                let far = direct(focus, 90., 120000.).unwrap();
                let mut outside = LineInstruction::new(vec![
                    WorldPoint::new(far.longitude(), far.latitude() - 0.005),
                    WorldPoint::new(far.longitude(), far.latitude() + 0.005),
                ]);
                outside.style = LineStyle::solid_mm(Color::rgb(1., 0., 0.), 1.);
                let mut edge = LineInstruction::new(vec![
                    point(&camera, -12., 299.),
                    point(&camera, -12., 301.),
                ]);
                edge.style = LineStyle::solid(Color::rgb(0., 1., 0.), 40.);
                edge.style.cap = CapStyle::Square;
                let mut corner = LineInstruction::new(vec![
                    point(&camera, 300., 400.),
                    point(&camera, 450., 360.),
                    point(&camera, 600., 400.),
                ]);
                corner.style = LineStyle::solid(Color::rgb(1., 1., 0.), 8.);
                corner.style.cap = center.style.cap;
                corner.style.join = center.style.join;
                let p = edge.points[0];
                let q = edge.points[1];
                let qlon = ferrite_kernel::geodesy::GeographicPosition::new(q.y, q.x)
                    .unwrap()
                    .longitude_near(p.x)
                    .unwrap();
                let bounds = ferrite_kernel::surface_bounds::GeographicSurfaceBounds::new(
                    [p.y.min(q.y), p.y.max(q.y)],
                    [p.x.min(qlon), p.x.max(qlon)],
                )
                .unwrap();
                let (centre, radius) = bounds.sphere();
                assert!(
                    camera.sphere_outside_frustum(centre, radius, 0.).unwrap(),
                    "fixture must require width guard lat{lat} tilt{tilt}"
                );
                let mut histories = Vec::new();
                let mut culled = 0;
                let mut avoided_samples = 0;
                for enabled in [false, true] {
                    let mut meshes = Vec::new();
                    for (index, line) in [&center, &outside, &edge, &corner].into_iter().enumerate()
                    {
                        let (mesh, stats) =
                            drape_line_with_spans_and_culling(line, &camera, ppm, None, enabled)
                                .unwrap();
                        if enabled {
                            if stats.frustum_culled {
                                culled += 1;
                            }
                            if index != 1 {
                                assert!(!stats.frustum_culled, "visible stroke culled");
                            }
                            if index == 1 {
                                assert_eq!(stats.samples, 0);
                            }
                        } else if index == 1 {
                            avoided_samples = stats.samples;
                        }
                        meshes.push(mesh);
                    }
                    let pixels = capture(&gpu, &mut renderer, &camera, &earth, &meshes, size);
                    image::RgbaImage::from_raw(size[0], size[1], pixels.clone())
                        .unwrap()
                        .save(self.out.join(format!("lat{lat}-tilt{tilt}-{enabled}.png")))
                        .unwrap();
                    histories.push(pixels);
                }
                assert_eq!(culled, 1);
                assert!(avoided_samples > 1);
                assert_eq!(
                    histories[0], histories[1],
                    "culling changed pixels lat{lat} tilt{tilt}"
                );
                let green = histories[1]
                    .chunks(4)
                    .filter(|p| p[1] > 100 && p[0] < 50 && p[2] < 100)
                    .count();
                let blue = histories[1]
                    .chunks(4)
                    .filter(|p| p[2] > 200 && p[0] < 50 && p[1] < 50)
                    .count();
                let red = histories[1]
                    .chunks(4)
                    .filter(|p| p[0] > 200 && p[1] < 50)
                    .count();
                let yellow = histories[1]
                    .chunks(4)
                    .filter(|p| p[0] > 200 && p[1] > 200 && p[2] < 50)
                    .count();
                assert!(green > 0);
                assert!(blue > 50);
                assert!(yellow > 50);
                assert_eq!(red, 0);
                let bad = [ferrite_render::LineSpan {
                    segment: usize::MAX,
                    start: 0.,
                    end: 1.,
                }];
                for enabled in [false, true] {
                    assert!(drape_line_with_spans_and_culling(
                        &outside,
                        &camera,
                        ppm,
                        Some(&bad),
                        enabled
                    )
                    .is_err());
                }
                rows.push(serde_json::json!({"latitude":lat,"longitude":179.9,"tilt":tilt,"cap":format!("{:?}",center.style.cap),"join":format!("{:?}",center.style.join),"culled":culled,"avoided_samples":avoided_samples,"changed_pixels":0,"blue_pixels":blue,"green_pixels":green,"yellow_join_pixels":yellow,"red_pixels":red,"stroke_guard_required":true,"invalid_span_rejected":true}));
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rows":rows,"gpu":gpu.gpu_name,"whole_frame_culling_on_off":true,"physical_input_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: std::env::args().nth(1).unwrap().into(),
        })
        .unwrap();
}
