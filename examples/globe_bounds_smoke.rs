//! Native culling-on/off whole-frame oracle for WGS84 bounds across the date line.
use ferrite_kernel::{
    geodesy::{direct, GeographicPosition},
    globe_camera::GlobeCamera,
};
use ferrite_render::{AreaInstruction, Color, WorldPoint};
use ferrite_wgpu::{
    globe_portrayal::{drape_area, DrapingLimits},
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
fn rectangle(lat: f64, lon: f64, dx: f64, dy: f64, color: Color) -> AreaInstruction {
    AreaInstruction::new(vec![
        WorldPoint::new(lon - dx, lat - dy),
        WorldPoint::new(lon + dx, lat - dy),
        WorldPoint::new(lon + dx, lat + dy),
        WorldPoint::new(lon - dx, lat + dy),
    ])
    .with_solid_fill(color)
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
                    .with_title("WGS84 bounds whole-frame oracle"),
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
                let dx = 0.02 / lat.to_radians().cos();
                let mut center = rectangle(lat, 179.9, dx, 0.02, Color::rgb(0., 1., 0.));
                center
                    .interiors
                    .push(rectangle(lat, 179.9, dx / 4., 0.005, Color::BLACK).exterior);
                let far = direct(focus, 90., 120000.).unwrap();
                let outside = rectangle(
                    far.latitude(),
                    far.longitude(),
                    0.005,
                    0.005,
                    Color::rgb(1., 0., 0.),
                );
                let mut histories = Vec::new();
                let mut culled = 0;
                let mut saved_vertices = 0;
                for enabled in [false, true] {
                    let limits = DrapingLimits {
                        frustum_culling: enabled,
                        ..Default::default()
                    };
                    let mut meshes = Vec::new();
                    for (index, a) in [&center, &outside].into_iter().enumerate() {
                        let (mesh, stats) = drape_area(a, &camera, limits).unwrap();
                        if enabled {
                            if stats.frustum_culled {
                                culled += 1;
                            }
                            if index == 0 {
                                assert!(!stats.frustum_culled, "central hole-bearing patch culled");
                            }
                        } else if index == 1 {
                            saved_vertices = mesh.vertices.len();
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
                assert!(saved_vertices > 0);
                assert_eq!(
                    histories[0], histories[1],
                    "culling changed pixels lat{lat} tilt{tilt}"
                );
                let green = histories[1]
                    .chunks(4)
                    .filter(|p| p[1] > 100 && p[0] < 50 && p[2] < 100)
                    .count();
                assert!(green > 20);
                let red = histories[1]
                    .chunks(4)
                    .filter(|p| p[0] > 200 && p[1] < 50)
                    .count();
                assert_eq!(red, 0);
                // Invalid topology still fails before visibility rejection.
                let invalid = AreaInstruction::new(vec![
                    WorldPoint::new(100., 30.),
                    WorldPoint::new(101., 31.),
                    WorldPoint::new(100., 31.),
                    WorldPoint::new(101., 30.),
                ])
                .with_solid_fill(Color::WHITE);
                assert!(drape_area(&invalid, &camera, Default::default()).is_err());
                rows.push(serde_json::json!({"latitude":lat,"longitude":179.9,"tilt":tilt,"culled":culled,"avoided_vertices":saved_vertices,"changed_pixels":0,"green_pixels":green,"red_pixels":red,"invalid_topology_rejected":true}));
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
