//! Native residency reuse, capacity fallback, and eviction/reload audit.
use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera};
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
struct App {
    out: PathBuf,
}
impl ApplicationHandler for App {
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes()
                    .with_title("Resident geometry capacity audit")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let g = pollster::block_on(GpuState::new(w)).unwrap();
        let c = GlobeCamera::orbit(
            GeographicPosition::new(50., 0.).unwrap(),
            250000.,
            0.,
            30.,
            [g.config.width as f64, g.config.height as f64],
            45.,
            25.,
            1e10,
        )
        .unwrap();
        let make = |shift: f64| {
            (0..9)
                .map(|i| {
                    Arc::new(GlobeMesh {
                        vertices: vec![
                            GlobeVertex {
                                ecef_m: GeographicPosition::new(50. + i as f64 * 0.02, shift)
                                    .unwrap()
                                    .to_ecef(0.)
                                    .unwrap(),
                                color: [i as f32 / 9., 0.2, 0.3, 1.]
                            };
                            90000
                        ],
                        indices: vec![0, 1, 2],
                    })
                })
                .collect::<Vec<_>>()
        };
        let a = make(0.);
        let b = make(0.1);
        let mut r = GlobeSceneRenderer::new(&g.device, g.config.format);
        r.set_gpu_projection_enabled(true);
        let mut rows = Vec::new();
        let mut prior_uploads = 0;
        for (frame, owners) in [&a, &a, &b, &b, &a].into_iter().enumerate() {
            r.bind_retained_geometry(owners);
            let layers: Vec<_> = owners
                .iter()
                .map(|mesh| GlobeLayer {
                    mesh,
                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                })
                .collect();
            r.prepare_layers(&g.device, &g.queue, &c, &layers).unwrap();
            let clips = r.read_prepared_clips(&g.device, &g.queue).unwrap();
            assert_eq!(clips.len(), 810000);
            for (i, mesh) in owners.iter().enumerate() {
                let p = c
                    .clip_ecef_reverse_depth(mesh.vertices[0].ecef_m)
                    .unwrap()
                    .map(|x| x as f32 as f64);
                for index in [0, 44999, 89999] {
                    let q = clips[i * 90000 + index].map(|x| x as f64);
                    let error = ((p[0] / p[3] - q[0] / q[3]) * g.config.width as f64 / 2.)
                        .hypot((p[1] / p[3] - q[1] / q[3]) * g.config.height as f64 / 2.);
                    assert!(error <= 0.02, "frame {frame} mesh {i} error {error}");
                }
            }
            let stats = r.resource_usage();
            let p = &stats["gpu_projection"];
            assert_eq!(p["resident_area_entries"], 7);
            assert!(p["resident_area_gpu_capacity_bytes"].as_u64().unwrap() <= 32 * 1024 * 1024);
            assert!(p["resident_area_cpu_owner_bytes"].as_u64().unwrap() <= 64 * 1024 * 1024);
            let uploads = p["resident_area_uploads"].as_u64().unwrap();
            if frame == 1 || frame == 3 {
                assert_eq!(uploads, prior_uploads);
                assert_eq!(p["resident_area_frame_upload_bytes"], 0);
            } else {
                assert_eq!(uploads, prior_uploads + 7);
            }
            assert_eq!(
                p["resident_area_evictions"],
                if frame < 2 {
                    0
                } else if frame < 4 {
                    7
                } else {
                    14
                }
            );
            prior_uploads = uploads;
            rows.push(serde_json::json!({"frame":frame,"resources":stats,"capacity_fallback_vertices":180000,"positions_verified":27}));
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"gpu":g.gpu_name,"frames":rows,"passed":true}),
            )
            .unwrap(),
        )
        .unwrap();
        println!(
            "Resident geometry: reuse, pinned capacity fallback, eviction and reload verified"
        );
        e.exit();
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
