//! Native GPU fixture for actual ellipsoidal perspective/depth, not chart portrayal.
use ferrite_kernel::{
    geodesy::{direct, GeographicPosition, WGS84_A, WGS84_B},
    globe_camera::GlobeCamera,
};
use ferrite_wgpu::{
    globe_scene::{GlobeDepthMode, GlobeLayer, GlobeMesh, GlobeSceneRenderer, GlobeVertex},
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
fn patch(center: GeographicPosition, radius: f64, height: f64, color: [f32; 4]) -> GlobeMesh {
    let mut m = GlobeMesh {
        vertices: vec![GlobeVertex {
            ecef_m: center.to_ecef(height).unwrap(),
            color,
        }],
        indices: Vec::new(),
    };
    for i in 0..=48 {
        let p = direct(center, 360. * i as f64 / 48., radius).unwrap();
        m.vertices.push(GlobeVertex {
            ecef_m: p.to_ecef(height).unwrap(),
            color,
        });
    }
    for i in 1..=48 {
        m.indices.extend_from_slice(&[0, i, i + 1]);
    }
    m
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
                    .with_title("WGS84 globe GPU depth audit")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let gpu = pollster::block_on(GpuState::new(w)).unwrap();
        let size = [gpu.config.width, gpu.config.height];
        let mut r = GlobeSceneRenderer::new(&gpu.device, gpu.config.format);
        let gpu_projection = std::env::args().any(|a| a == "--gpu-projection");
        r.set_gpu_projection_enabled(gpu_projection);
        let mut rows = Vec::new();
        for (name, lat, lon, range, tilt, heading) in [
            ("alpha-order", 0., 0., 50000., 0., 0.),
            ("invisible-first", 0., 0., 50000., 0., 0.),
            ("far-overlay", 0., 0., 50000., 0., 0.),
        ] {
            let focus = GeographicPosition::new(lat, lon).unwrap();
            let c = GlobeCamera::orbit(
                focus,
                range,
                heading,
                tilt,
                size.map(|x| x as f64),
                45.,
                range / 10000.,
                1e9,
            )
            .unwrap();
            let mut mesh = GlobeMesh::ellipsoid(360, 180, [0., 0., 1., 1.]).unwrap();
            let radius = if range < 1e6 { 10000. } else { 400000. };
            let height = if range < 1e6 { 1000. } else { 20000. };
            mesh.append(&patch(focus, radius, height, [1., 0., 0., 1.]))
                .unwrap();
            // The second ellipsoid crossing along the central camera ray is
            // behind the front patch and inside the frustum even for tilt.
            let front = focus.to_ecef(0.).unwrap();
            let ray = c.ray([size[0] as f64 / 2., size[1] as f64 / 2.]).unwrap();
            let axes = [WGS84_A, WGS84_A, WGS84_B];
            let aa: f64 = (0..3).map(|i| (ray.direction[i] / axes[i]).powi(2)).sum();
            let bb: f64 = (0..3)
                .map(|i| 2. * front[i] * ray.direction[i] / axes[i].powi(2))
                .sum();
            let far_point = std::array::from_fn(|i| front[i] - ray.direction[i] * bb / aa);
            let back = ferrite_kernel::geocentric::from_ecef(far_point)
                .unwrap()
                .surface;
            // Far-side green geometry is submitted after the front marker. It must be
            // hidden by actual GPU depth, without CPU-visible filtering of its vertices.
            mesh.append(&patch(back, radius, height, [0., 1., 0., 1.]))
                .unwrap();
            let far_clip = c.clip_ecef(back.to_ecef(height).unwrap()).unwrap();
            assert!(
                far_clip[3] > 0.
                    && far_clip[2] / far_clip[3] > 0.
                    && far_clip[2] / far_clip[3] < 1.
                    && (far_clip[0] / far_clip[3]).abs() < 1.
                    && (far_clip[1] / far_clip[3]).abs() < 1.,
                "far patch outside frustum"
            );
            let base = Arc::new(GlobeMesh::ellipsoid(360, 180, [0., 0., 1., 1.]).unwrap());
            r.bind_retained_base(base.clone()).unwrap();
            let red = patch(
                focus,
                radius,
                2000.,
                [
                    1.,
                    0.,
                    0.,
                    if name == "alpha-order" {
                        0.5
                    } else if name == "invisible-first" {
                        0.
                    } else {
                        1.
                    },
                ],
            );
            let green = patch(
                focus,
                radius,
                1000.,
                [
                    0.,
                    1.,
                    0.,
                    if name == "alpha-order" {
                        0.5
                    } else if name == "invisible-first" {
                        1.
                    } else {
                        0.
                    },
                ],
            );
            let far = patch(back, radius, height, [0., 1., 0., 1.]);
            if name == "alpha-order" {
                assert!(r.prepare(&gpu.device, &gpu.queue, &c, &red).is_err());
            }
            assert!(r
                .prepare_layers(
                    &gpu.device,
                    &gpu.queue,
                    &c,
                    &[
                        GlobeLayer {
                            mesh: &red,
                            depth_mode: GlobeDepthMode::SurfaceOverlay
                        },
                        GlobeLayer {
                            mesh: &base,
                            depth_mode: GlobeDepthMode::Occluder
                        }
                    ]
                )
                .is_err());
            let layers = [
                GlobeLayer {
                    mesh: &base,
                    depth_mode: GlobeDepthMode::Occluder,
                },
                GlobeLayer {
                    mesh: &red,
                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                },
                GlobeLayer {
                    mesh: &green,
                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                },
                GlobeLayer {
                    mesh: &far,
                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                },
            ];
            r.prepare_layers(&gpu.device, &gpu.queue, &c, &layers)
                .unwrap();

            assert_eq!(r.resource_usage()["gpu_projection_used"], gpu_projection);
            assert_eq!(r.resource_usage()["resident_base_used"], gpu_projection);
            let extent = wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            };
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("globe audit image"),
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
                label: Some("globe readback"),
                size: row as u64 * size[1] as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            assert!(r
                .render(
                    &gpu.device,
                    &mut encoder,
                    &texture.create_view(&Default::default()),
                    [size[0] - 1, size[1]],
                    wgpu::Color::BLACK
                )
                .is_err());
            r.render(
                &gpu.device,
                &mut encoder,
                &texture.create_view(&Default::default()),
                size,
                wgpu::Color::BLACK,
            )
            .unwrap();
            let frame = gpu.surface.get_current_texture().unwrap();
            r.render(
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
                .map_async(wgpu::MapMode::Read, move |v| tx.send(v).unwrap());
            gpu.device.poll(wgpu::Maintain::Wait);
            rx.recv().unwrap().unwrap();
            let data = buffer.slice(..).get_mapped_range();
            let bgra = matches!(
                gpu.config.format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
            );
            let mut pixels = Vec::new();
            for line in data.chunks(row as usize) {
                for pixel in line[..size[0] as usize * 4].chunks(4) {
                    if bgra {
                        pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                    } else {
                        pixels.extend_from_slice(pixel);
                    }
                }
            }
            drop(data);
            buffer.unmap();
            let im = image::RgbaImage::from_raw(size[0], size[1], pixels).unwrap();
            let red = im
                .pixels()
                .filter(|p| p[0] > 240 && p[1] < 10 && p[2] < 10)
                .count();
            let green = im
                .pixels()
                .filter(|p| p[1] > 240 && p[0] < 10 && p[2] < 10)
                .count();
            let earth = im
                .pixels()
                .filter(|p| p[2] > 100 && p[0] < 60 && p[1] > 60)
                .count();
            let center = im.get_pixel(size[0] / 2, size[1] / 2).0;
            let expected = if name == "alpha-order" {
                [64_u8, 128, 64]
            } else if name == "invisible-first" {
                [0, 255, 0]
            } else {
                [255, 0, 0]
            };
            assert!(
                center[..3]
                    .iter()
                    .zip(expected)
                    .all(|(a, b)| a.abs_diff(b) <= 1),
                "{name}: {center:?} expected {expected:?}"
            );
            if name != "invisible-first" {
                assert_eq!(green, 0, "far overlay leaked");
            }
            im.save(self.out.join(format!("{name}.png"))).unwrap();
            let picked = c
                .pick([size[0] as f64 / 2., size[1] as f64 / 2.])
                .unwrap()
                .unwrap();
            let error = ferrite_kernel::geodesy::inverse(focus, picked.geodetic.surface)
                .unwrap()
                .distance_m;
            assert!(error < 1e-4);
            rows.push(serde_json::json!({"gpu_projection_requested":gpu_projection,"resources":r.resource_usage(),"case":name,"center_rgba":center,"expected_rgb":expected,"resize_rejected":true,"opaque_alpha_rejected":name=="alpha-order","occluder_after_overlay_rejected":true,"red_pixels":red,"green_pixels":green,"ellipsoid_pixels":earth,"source_vertices":layers.iter().map(|l|l.mesh.vertices.len()).sum::<usize>(),"source_indices":layers.iter().map(|l|l.mesh.indices.len()).sum::<usize>(),"focus_pick_error_m":error,"far_patch_in_frustum":true,"depth_buffer":"Depth32Float","gpu":gpu.gpu_name,"native_density":density,"actual_chart_portrayal":false,"app_integration_verified":false}));
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
    let mut app = App {
        out: std::env::args().nth(1).unwrap().into(),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
