//! Native GPU oracle for common geometry pass ordering and longitude wrapping.
use ferrite_render::{
    AreaInstruction, Color, DisplayPlane, DrawingInstruction, GeoBounds, LineInstruction,
    LineStyle, RasterLayer, RenderContext, Viewport, WorldPoint,
};
use ferrite_wgpu::WgpuRenderer;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct Smoke {
    output: PathBuf,
}
fn rectangle(x: f64, y: f64, w: f64, h: f64, color: Color, priority: i32) -> AreaInstruction {
    AreaInstruction::new(vec![
        WorldPoint::new(x, y),
        WorldPoint::new(x + w, y),
        WorldPoint::new(x + w, y + h),
        WorldPoint::new(x, y + h),
    ])
    .with_solid_fill(color)
    .with_priority(priority)
}
fn pixel(image: &image::RgbImage, ctx: &RenderContext, x: f64, y: f64) -> [u8; 3] {
    let p = ctx.scaler.world_to_screen(WorldPoint::new(x, y));
    image.get_pixel(p.x.round() as u32, p.y.round() as u32).0
}
fn check(
    renderer: &mut WgpuRenderer,
    ctx: &mut RenderContext,
    path: &Path,
    probes: &[(f64, f64, [u8; 3])],
) {
    renderer.begin_frame();
    renderer.add_instructions(ctx);
    renderer.save_screenshot(path).unwrap();
    let image = image::open(path).unwrap().to_rgb8();
    for &(x, y, expected) in probes {
        assert_eq!(
            pixel(&image, ctx, x, y),
            expected,
            "{} world ({x},{y})",
            path.display()
        );
    }
    // The same buffers must also survive an actual surface render before export.
    renderer.render().unwrap();
    let after = path.with_file_name(format!(
        "{}-after-surface.png",
        path.file_stem().unwrap().to_string_lossy()
    ));
    renderer.save_screenshot(&after).unwrap();
    let later = image::open(&after).unwrap().to_rgb8();
    assert_eq!(
        image, later,
        "export before/after surface render must match"
    );
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Common chart pass regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.output).unwrap();
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.add_instruction(DrawingInstruction::Area(rectangle(
            2.,
            2.,
            6.,
            6.,
            Color::rgb(1., 0., 0.),
            8,
        )));
        ctx.add_instruction(DrawingInstruction::Area(
            rectangle(4., 2., 2., 6., Color::rgb(1., 1., 0.), 0)
                .with_display_plane(DisplayPlane::OverRadar),
        ));
        for (y, color, priority, width) in [
            (5., Color::rgb(0., 0., 1.), 2, 20.),
            (4., Color::rgb(0., 1., 0.), 9, 8.),
        ] {
            ctx.add_instruction(DrawingInstruction::Line(
                LineInstruction::new(vec![WorldPoint::new(0., y), WorldPoint::new(10., y)])
                    .with_priority(priority)
                    .with_style(LineStyle::solid(color, width)),
            ));
        }
        renderer
            .add_raster_layer(
                RasterLayer {
                    viewing_groups: Vec::new(),
                    draw_order: ferrite_render::RasterDrawOrder {
                        stage: ferrite_kernel::CompositionStage::Chart,
                        display_plane: DisplayPlane::UnderRadar,
                        priority: 4,
                    },
                    grid: None,
                    id: "priority4".into(),
                    bounds: GeoBounds::new(1., 1., 9., 9.),
                    width: 2,
                    height: 2,
                    rgba: [0, 255, 255, 255].repeat(4),
                },
                &ctx.scaler,
            )
            .unwrap();
        check(
            &mut renderer,
            &mut ctx,
            &self.output.join("priority-plane-raster.png"),
            &[
                (1.5, 5., [0, 255, 255]),
                (3., 5., [255, 0, 0]),
                (5., 5., [255, 255, 0]),
                (3., 4., [0, 255, 0]),
                (5., 4., [255, 255, 0]),
                (0.5, 5., [0, 0, 255]),
                (0.5, 8., [255, 255, 255]),
            ],
        );
        renderer.clear_raster_layers();
        let mut wrapped = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        wrapped.set_bounds(GeoBounds::new(0., 0., 1080., 10.));
        wrapped.add_instruction(DrawingInstruction::Area(rectangle(
            100.,
            1.,
            100.,
            8.,
            Color::rgb(1., 0., 0.),
            2,
        )));
        wrapped.add_instruction(DrawingInstruction::Area(rectangle(
            460.,
            1.,
            100.,
            8.,
            Color::rgb(0., 0., 1.),
            8,
        )));
        check(
            &mut renderer,
            &mut wrapped,
            &self.output.join("unwrapped.png"),
            &[
                (150., 5., [255, 0, 0]),
                (510., 5., [0, 0, 255]),
                (870., 5., [255, 255, 255]),
            ],
        );
        renderer.set_lon_wrap_pixels(360.0 * wrapped.scaler.scale_x() as f32);
        check(
            &mut renderer,
            &mut wrapped,
            &self.output.join("wrapped.png"),
            &[
                (150., 5., [0, 0, 255]),
                (510., 5., [0, 0, 255]),
                (870., 5., [0, 0, 255]),
            ],
        );
        renderer.set_lon_wrap_pixels(0.);
        let mut world = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        world.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        renderer.set_world_map(vec![vec![[0., 5.], [10., 5.]]]);
        renderer.set_world_map_chart_boxes(vec![(3., 0., 7., 10.)]);
        renderer.begin_frame();
        renderer.add_world_map_lines(&world.scaler);
        renderer.add_instructions(&mut world);
        let file = self.output.join("world-map-mask.png");
        renderer.save_screenshot(&file).unwrap();
        let image = image::open(file).unwrap().to_rgb8();
        assert_eq!(pixel(&image, &world, 5., 5.), [255, 255, 255]);
        for x in [2., 8.] {
            let p = world.scaler.world_to_screen(WorldPoint::new(x, 5.));
            assert!((-2..=2).any(|dy| image
                .get_pixel(p.x.round() as u32, (p.y.round() as i32 + dy) as u32)
                .0
                .iter()
                .any(|v| *v < 250)));
        }
        // A shifted chart pane must contain every chart layer, even after text
        // changes the scissor and a later priority draws wide lines/raster.
        let pane = Viewport::with_origin(160., 120., 480., 320.);
        let mut clipped = RenderContext::new(pane);
        clipped.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        renderer.set_world_map(vec![vec![[-100., 5.], [100., 5.]]]);
        renderer.set_world_map_chart_boxes(Vec::new());
        clipped.add_instruction(DrawingInstruction::Area(rectangle(
            -1.,
            -1.,
            12.,
            12.,
            Color::rgb(0., 1., 1.),
            0,
        )));
        clipped.add_instruction(DrawingInstruction::Text(
            ferrite_render::TextInstruction::new("EDGE".into(), WorldPoint::new(0., 8.))
                .with_font_size(36.)
                .with_color(Color::BLACK)
                .with_priority(1),
        ));
        clipped.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(-100., 2.), WorldPoint::new(100., 2.)])
                .with_style(LineStyle::solid(Color::BLACK, 30.))
                .with_priority(2),
        ));
        renderer
            .add_raster_layer(
                RasterLayer {
                    viewing_groups: Vec::new(),
                    draw_order: ferrite_render::RasterDrawOrder {
                        stage: ferrite_kernel::CompositionStage::Chart,
                        display_plane: DisplayPlane::UnderRadar,
                        priority: 3,
                    },
                    grid: None,
                    id: "pane-edge-raster".into(),
                    bounds: GeoBounds::new(-5., 6., 0.5, 15.),
                    width: 2,
                    height: 2,
                    rgba: [255, 0, 0, 255].repeat(4),
                },
                &clipped.scaler,
            )
            .unwrap();
        renderer.begin_frame();
        renderer.add_world_map_lines(&clipped.scaler);
        renderer.add_instructions(&mut clipped);
        let pane_file = self.output.join("shifted-pane.png");
        renderer.save_screenshot(&pane_file).unwrap();
        let image = image::open(&pane_file).unwrap().to_rgb8();
        let mut outside_changed = 0usize;
        for (x, y, pixel) in image.enumerate_pixels() {
            if (x < 160 || x >= 640 || y < 120 || y >= 440) && pixel.0 != [255, 255, 255] {
                outside_changed += 1;
            }
        }
        assert_eq!(
            outside_changed, 0,
            "Chart geometry leaked outside the shifted pane"
        );
        assert_eq!(pixel(&image, &clipped, 5., 5.), [0, 255, 255]);
        assert_eq!(pixel(&image, &clipped, 5., 2.), [0, 0, 0]);
        assert_eq!(pixel(&image, &clipped, 0.2, 7.), [255, 0, 0]);
        renderer.render().unwrap();
        renderer
            .save_screenshot(self.output.join("shifted-pane-after-surface.png"))
            .unwrap();
        assert_eq!(
            image,
            image::open(self.output.join("shifted-pane-after-surface.png"))
                .unwrap()
                .to_rgb8()
        );
        std::fs::write(self.output.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"exact_color_probes":13,"export_stability_across_surface_render":3,"world_map_mask_probes":3,"shifted_pane_outside_changed_pixels":0,"native_mouse_menu_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
