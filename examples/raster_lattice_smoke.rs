//! Independent GPU comparison of full/tiled regular-grid cell ownership.
use anyhow::Result;
use ferrite_kernel::{CoverageSample, CoverageSource, CoverageTile, GridGeometry, GridWindow};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{Color, RenderContext, Viewport, WorldPoint};
use ferrite_s102::{BathymetryPortrayal, DepthSettings};
use ferrite_wgpu::WgpuRenderer;
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct Mock {
    grid: GridGeometry,
}
impl CoverageSource for Mock {
    fn geometry(&self) -> &GridGeometry {
        &self.grid
    }
    fn read_window(&self, w: GridWindow) -> Result<CoverageTile> {
        w.validate(&self.grid)?;
        let mut samples = Vec::new();
        for y in w.row..w.row + w.height {
            for x in w.column..w.column + w.width {
                samples.push(CoverageSample {
                    value: if (x + y) % 3 == 0 {
                        None
                    } else {
                        Some(if (x + y) % 2 == 0 { 7. } else { 60. })
                    },
                    uncertainty: None,
                });
            }
        }
        Ok(CoverageTile { window: w, samples })
    }
}
struct Smoke {
    out: PathBuf,
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Raster lattice oracle")
                    .with_inner_size(PhysicalSize::new(1200, 800)),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let pc = PortrayalCatalogue::load(
            "Standards/S102-PC-3.0.0/S-102-Portrayal-Catalogue-3.0.0.20241216/PortrayalCatalog",
        )
        .unwrap();
        let p = BathymetryPortrayal::from_catalogue(&pc, "Day", DepthSettings::default()).unwrap();
        std::fs::create_dir_all(&self.out).unwrap();
        let mut checks = Vec::new();
        for sx in [-1., 1.] {
            for sy in [-1., 1.] {
                let source = Mock {
                    grid: GridGeometry {
                        width: 11,
                        height: 9,
                        origin_x: -2.,
                        origin_y: 48.,
                        spacing_x: sx * 0.00005,
                        spacing_y: sy * 0.00005,
                        horizontal_crs: 4326,
                    },
                };
                let mut ctx =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                let whole = GridWindow {
                    column: 0,
                    row: 0,
                    width: 11,
                    height: 9,
                };
                ctx.set_bounds(
                    p.raster_window(&source, whole, "bounds".into())
                        .unwrap()
                        .bounds,
                );
                for zoom in [0.5f32, 1., 2.] {
                    r.reset_pan_offset();
                    r.begin_frame();
                    r.add_instructions(&mut ctx);
                    r.clear_raster_layers();
                    r.add_raster_layer(
                        p.raster_window(&source, whole, "full".into()).unwrap(),
                        &ctx.scaler,
                    )
                    .unwrap();
                    r.set_pan_offset(17.25, -11.5);
                    r.set_gpu_zoom(zoom, size.width as f32 / 2., size.height as f32 / 2.);
                    let base = self.out.join(format!("{sx}-{sy}-{zoom}-full.png"));
                    r.save_screenshot(&base).unwrap();
                    let expected = image::open(base).unwrap().to_rgb8();
                    r.raster_batch(&ctx.scaler, true, |upload| -> Result<()> {
                        for window in source.grid.windows(3, 4)? {
                            upload(p.raster_window(&source, window, "tile".into())?)?;
                        }
                        Ok(())
                    })
                    .unwrap();
                    let file = self.out.join(format!("{sx}-{sy}-{zoom}-tile.png"));
                    r.save_screenshot(&file).unwrap();
                    let actual = image::open(file).unwrap().to_rgb8();
                    let mut node_probes = 0;
                    for row in 0..source.grid.height {
                        for column in 0..source.grid.width {
                            let node = ctx.scaler.world_to_screen(WorldPoint::new(
                                source.grid.origin_x + column as f64 * source.grid.spacing_x,
                                source.grid.origin_y + row as f64 * source.grid.spacing_y,
                            ));
                            let x = ((node.x + 17.25 - size.width as f32 / 2.) * zoom
                                + size.width as f32 / 2.)
                                .floor() as i32;
                            let y = ((node.y - 11.5 - size.height as f32 / 2.) * zoom
                                + size.height as f32 / 2.)
                                .floor() as i32;
                            if x < 0 || y < 0 || x >= size.width as i32 || y >= size.height as i32 {
                                continue;
                            }
                            let value = if (column + row) % 3 == 0 {
                                None
                            } else {
                                Some(if (column + row) % 2 == 0 { 7. } else { 60. })
                            };
                            let rgba = p
                                .rgba(CoverageSample {
                                    value,
                                    uncertainty: None,
                                })
                                .unwrap();
                            assert!(
                                rgba[3] == 0 || rgba[3] == 255,
                                "Fixture requires opaque or missing cells"
                            );
                            let rgb = if rgba[3] == 0 {
                                [255, 255, 255]
                            } else {
                                [rgba[0], rgba[1], rgba[2]]
                            };
                            assert_eq!(actual.get_pixel(x as u32,y as u32).0,rgb,"Independent source node {column},{row}, axes {sx},{sy}, zoom {zoom}");
                            node_probes += 1;
                        }
                    }
                    let changed = actual
                        .pixels()
                        .zip(expected.pixels())
                        .filter(|(a, b)| a != b)
                        .count();
                    assert_eq!(
                        changed, 0,
                        "Axis {sx},{sy} zoom {zoom}: tile ownership differs"
                    );
                    r.render().unwrap();
                    let after = self.out.join(format!("{sx}-{sy}-{zoom}-surface.png"));
                    r.save_screenshot(&after).unwrap();
                    assert!(
                        actual == image::open(after).unwrap().to_rgb8(),
                        "Surface/export divergence"
                    );
                    checks.push(serde_json::json!({"axis_signs":[sx,sy],"zoom":zoom,"pan":[17.25,-11.5],"changed_pixels":changed,"tiles":12,"independent_source_node_probes":node_probes}));
                }
            }
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"cases":checks,"native_mouse_verified":false}),
            )
            .unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke {
            out: std::env::args().nth(1).unwrap().into(),
        })
        .unwrap();
}
