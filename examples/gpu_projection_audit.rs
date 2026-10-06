//! Native readback of actual GPU arithmetic, including metre-scale views at poles.
use ferrite_kernel::{
    geodesy::{direct, GeographicPosition},
    globe_camera::GlobeCamera,
};
use ferrite_wgpu::{
    globe_scene::{
        GlobeDepthMode, GlobeDraw, GlobeLayer, GlobeMesh, GlobeSceneRenderer, GlobeVertex,
    },
    GpuState,
};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event_loop::{ActiveEventLoop, EventLoop},
    window::Window,
};
struct App {
    out: PathBuf,
}
impl ApplicationHandler for App {
    fn window_event(
        &mut self,
        _: &ActiveEventLoop,
        _: winit::window::WindowId,
        _: winit::event::WindowEvent,
    ) {
    }
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes()
                    .with_title("GPU projection precision audit")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let g = pollster::block_on(GpuState::new(w)).unwrap();
        let size = [g.config.width as f64, g.config.height as f64];
        let base = Arc::new(GlobeMesh::ellipsoid(48, 24, [0.1, 0.2, 0.3, 1.]).unwrap());
        let mut r = GlobeSceneRenderer::new(&g.device, g.config.format);
        r.bind_retained_base(base.clone()).unwrap();
        r.set_gpu_projection_enabled(true);
        let mut rows = Vec::new();
        let mut global_max = 0f64;
        let mut global_depth = 0f64;
        let mut visible = 0usize;
        for lat in [-89.9999, 0., 50., 89.9999] {
            for lon in [-179.99, 0., 179.99] {
                for range in [1., 1000., 250000., 1e7] {
                    for tilt in [0., 70.] {
                        let focus = GeographicPosition::new(lat, lon).unwrap();
                        let c = GlobeCamera::orbit(
                            focus,
                            range,
                            87.,
                            tilt,
                            size,
                            45.,
                            range / 10000.,
                            1e10,
                        )
                        .unwrap();
                        let mut mesh = GlobeMesh {
                            vertices: Vec::new(),
                            indices: Vec::new(),
                        };
                        for scale in [0., 0.01, 0.1, 0.3, 1., 2.] {
                            for bearing in 0..24 {
                                let p = direct(focus, bearing as f64 * 15., range * scale).unwrap();
                                mesh.vertices.push(GlobeVertex {
                                    ecef_m: p.to_ecef(0.).unwrap(),
                                    color: [0.5, 0.2, 0.1, 1.],
                                });
                            }
                        }
                        let mesh = Arc::new(mesh);
                        r.bind_retained_geometry(&[mesh.clone()]);
                        let anchor = focus.to_ecef(0.).unwrap();
                        let fonts = GlobeMesh {
                            vertices: [[-7., -11.], [7., -11.], [7., 11.], [-7., 11.]]
                                .into_iter()
                                .map(|offset| GlobeVertex {
                                    ecef_m: c.offset_pixels(anchor, offset).unwrap(),
                                    color: [0., 0., 1., 1.],
                                })
                                .collect(),
                            indices: vec![0, 1, 2, 0, 2, 3],
                        };
                        let draws = [
                            GlobeDraw {
                                layer: GlobeLayer {
                                    mesh: &base,
                                    depth_mode: GlobeDepthMode::Occluder,
                                },
                                texture: None,
                                pattern: None,
                                font_color: None,
                            },
                            GlobeDraw {
                                layer: GlobeLayer {
                                    mesh: &mesh,
                                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                                },
                                texture: None,
                                pattern: None,
                                font_color: None,
                            },
                            GlobeDraw {
                                layer: GlobeLayer {
                                    mesh: &fonts,
                                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                                },
                                texture: None,
                                pattern: None,
                                font_color: Some([128, 64, 32, 255]),
                            },
                        ];
                        r.prepare_draws(&g.device, &g.queue, &c, &draws).unwrap();
                        assert_eq!(r.resource_usage()["gpu_projection_used"], true);
                        assert_eq!(r.resource_usage()["resident_base_used"], true);
                        let uploads = r.resource_usage()["gpu_projection"]["resident_area_uploads"]
                            .as_u64()
                            .unwrap();
                        r.prepare_draws(&g.device, &g.queue, &c, &draws).unwrap();
                        assert_eq!(
                            r.resource_usage()["gpu_projection"]["resident_area_uploads"],
                            uploads
                        );
                        assert_eq!(
                            r.resource_usage()["gpu_projection"]
                                ["resident_area_frame_upload_bytes"],
                            0
                        );
                        let clips = r.read_prepared_clips(&g.device, &g.queue).unwrap();
                        let mut max = 0f64;
                        let mut depth_error = 0f64;
                        let mut count = 0usize;
                        let mut i = 0;
                        for d in &draws {
                            for v in &d.layer.mesh.vertices {
                                let p = c.clip_ecef_reverse_depth(v.ecef_m).unwrap();
                                let mut expected = p;
                                if d.font_color.is_some() {
                                    expected = [p[0] / p[3], p[1] / p[3], p[2] / p[3], 1.];
                                }
                                let actual = clips[i].map(|x| x as f64);
                                assert!(actual.iter().all(|x| x.is_finite()));
                                let ref32 = expected.map(|x| x as f32 as f64);
                                if p[3] > 0.
                                    && p[2] >= 0.
                                    && p[2] <= p[3]
                                    && (p[0] / p[3]).abs() <= 1.25
                                    && (p[1] / p[3]).abs() <= 1.25
                                {
                                    let dx = (actual[0] / actual[3] - ref32[0] / ref32[3])
                                        * size[0]
                                        / 2.;
                                    let dy = (actual[1] / actual[3] - ref32[1] / ref32[3])
                                        * size[1]
                                        / 2.;
                                    if dx.hypot(dy) > 0.02 && dx.hypot(dy) > max {
                                        eprintln!("bad pose {lat}/{lon}/{range}/{tilt} vertex {i} ecef {:?} ref {:?} actual {:?}",v.ecef_m,ref32,actual);
                                    }
                                    max = max.max(dx.hypot(dy));
                                    depth_error = depth_error
                                        .max((actual[2] / actual[3] - ref32[2] / ref32[3]).abs());
                                    count += 1;
                                }
                                i += 1;
                            }
                        }
                        assert!(
                            max <= 0.02,
                            "GPU projection error {max}px at {lat}/{lon}/{range}/{tilt}"
                        );
                        assert!(depth_error <= 3e-7, "GPU depth error {depth_error}");
                        global_max = global_max.max(max);
                        global_depth = global_depth.max(depth_error);
                        visible += count;
                        rows.push(serde_json::json!({"lat":lat,"lon":lon,"range_m":range,"tilt":tilt,"visible_vertices":count,"max_pixel_error_against_cpu_f32":max,"max_depth_error":depth_error}));
                    }
                }
            }
        }
        assert!(visible > 0);
        std::fs::write(self.out.join("precision.json"),serde_json::to_vec_pretty(&serde_json::json!({"gpu":g.gpu_name,"poses":rows,"visible_vertices":visible,"max_pixel_error":global_max,"max_depth_error":global_depth,"resources":r.resource_usage()})).unwrap()).unwrap();
        println!(
            "GPU precision verified: {} poses, {visible} visible vertices, maximum {global_max}px",
            rows.len()
        );
        e.exit();
    }
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: PathBuf::from(std::env::args().nth(1).expect("output directory")),
        })
        .unwrap();
}
