//! Native signed-plane rendering with two rules resolved from a locally authored IC XML.
//! This is not full S-98/catalogue-trust or real-feature adapter validation.
use ferrite_kernel::CompositionStage;
use ferrite_render::*;
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
use std::{num::NonZeroI32, path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
fn plane(n: i32) -> DisplayPlane {
    DisplayPlane::Interoperability(NonZeroI32::new(n).unwrap())
}
struct App {
    out: PathBuf,
    pc: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(1200, 800))
                    .with_title("Signed interoperability planes"),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let density = w.scale_factor();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let catalogue = ferrite_interoperability::Catalogue::parse(include_str!(
            "../crates/ferrite-interoperability/tests/display-plane.xml"
        ))
        .unwrap();
        let coverage_assignment = catalogue
            .resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
            .unwrap()
            .unwrap();
        // Synthetic selector input: the anchor glyph is used to test plane ordering, not wreck portrayal.
        let point_assignment = catalogue
            .resolve("S-101", "Wreck", "point", |_| {
                Ok(Some(ferrite_interoperability::Scalar::Number(
                    ferrite_interoperability::Decimal::parse("1").unwrap(),
                )))
            })
            .unwrap()
            .unwrap();
        assert!(catalogue
            .resolve("S-101", "Wreck", "surface", |_| panic!())
            .unwrap()
            .is_none());
        let mut cases = Vec::new();
        for zoom in [0.5, 1., 2.] {
            r.clear_raster_layers();
            r.set_gpu_zoom(1., 0., 0.);
            r.set_pan_offset(0., 0.);
            let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
            let rect = |x, w, color, order| {
                DrawingInstruction::Area(
                    AreaInstruction::new(vec![
                        WorldPoint::new(x, 0.),
                        WorldPoint::new(x + w, 0.),
                        WorldPoint::new(x + w, 10.),
                        WorldPoint::new(x, 10.),
                    ])
                    .with_solid_fill(color)
                    .with_priority(99)
                    .with_display_plane(plane(order)),
                )
            };
            for i in (0..500).rev() {
                c.add_instruction(rect(
                    0.,
                    10.,
                    if i == 499 { Color::GREEN } else { Color::BLUE },
                    -1000 + i,
                ));
            }
            c.add_instruction(rect(4., 2., Color::rgb(1., 1., 0.), i32::MAX));
            for (order, priority, col) in [(-10000, 99, Color::BLUE), (-100, 0, Color::RED)] {
                c.add_instruction(DrawingInstruction::Line(
                    LineInstruction::new(vec![WorldPoint::new(0., 3.), WorldPoint::new(10., 3.)])
                        .with_priority(priority)
                        .with_display_plane(plane(order))
                        .with_style(LineStyle::solid(col, 10.)),
                ));
            }
            c.add_instruction(DrawingInstruction::Point(
                PointInstruction::new("ACHBRT07".into(), WorldPoint::new(7., 7.))
                    .with_display_plane(plane(point_assignment.plane.order.get()))
                    .with_priority(point_assignment.priority)
                    .with_viewing_group(point_assignment.viewing_group),
            ));
            let mut text = TextInstruction::new("TEST".into(), WorldPoint::new(3., 7.))
                .with_font_size(24.)
                .with_color(Color::BLACK)
                .with_display_plane(plane(point_assignment.plane.order.get()))
                .with_priority(point_assignment.priority)
                .with_viewing_group(point_assignment.viewing_group);
            text.background = Some(Color::RED);
            c.add_instruction(DrawingInstruction::Text(text));
            r.begin_frame();
            r.add_instructions_with_symbols(&mut c, Some(&mut cache), Some(profile), None);
            assert_eq!(r.missing_symbol_count(), 0);
            assert_eq!(r.displayed_symbols().len(), 1);
            assert_eq!(r.displayed_symbols()[0].6, point_assignment.plane);
            assert_eq!(r.displayed_symbols()[0].4, point_assignment.priority);
            r.add_raster_layer(
                RasterLayer {
                    viewing_groups: vec![coverage_assignment.viewing_group],
                    draw_order: RasterDrawOrder {
                        stage: CompositionStage::Chart,
                        display_plane: plane(coverage_assignment.plane.order.get()),
                        priority: coverage_assignment.priority,
                    },
                    id: "IC bathymetry".into(),
                    bounds: GeoBounds::new(1., 1., 9., 9.),
                    width: 2,
                    height: 2,
                    rgba: [0, 255, 255, 255].repeat(4),
                    grid: None,
                },
                &c.scaler,
            )
            .unwrap();
            let pivot = c.scaler.world_to_screen(WorldPoint::new(5., 5.));
            r.set_gpu_zoom(zoom, pivot.x, pivot.y);
            let path = self.out.join(format!("planes-{zoom}.png"));
            r.save_screenshot(&path).unwrap();
            let im = image::open(&path).unwrap().to_rgb8();
            let project = |x, y| {
                let p = c.scaler.world_to_screen(WorldPoint::new(x, y));
                [
                    (pivot.x + (p.x - pivot.x) * zoom).round() as i32,
                    (pivot.y + (p.y - pivot.y) * zoom).round() as i32,
                ]
            };
            let mut probes = 0;
            for (x, y, expected) in [
                (3., 5., [0, 255, 255]),
                (5., 5., [255, 255, 0]),
                (3., 3., [255, 0, 0]),
                (0.5, 3., [255, 0, 0]),
                (0.5, 5., [0, 255, 0]),
            ] {
                let p = project(x, y);
                if p[0] >= 0 && p[0] < im.width() as i32 && p[1] >= 0 && p[1] < im.height() as i32 {
                    assert_eq!(
                        im.get_pixel(p[0] as u32, p[1] as u32).0,
                        expected,
                        "zoom{zoom} {x},{y}"
                    );
                    probes += 1;
                }
            }
            let count = |p: [i32; 2], radius: i32, predicate: fn([u8; 3]) -> bool| {
                let mut n = 0;
                for y in (p[1] - radius).max(0)..(p[1] + radius).min(im.height() as i32) {
                    for x in (p[0] - radius).max(0)..(p[0] + radius).min(im.width() as i32) {
                        if predicate(im.get_pixel(x as u32, y as u32).0) {
                            n += 1;
                        }
                    }
                }
                n
            };
            let glyph = project(3., 7.);
            let black = count(glyph, 70, |p| p[0] < 20 && p[1] < 20 && p[2] < 20);
            let red = count(glyph, 70, |p| p == [255, 0, 0]);
            let marked = count(project(7., 7.), 30, |p| {
                p[0] > 100 || p[1] < 100 || p[2] < 100
            });
            assert!(
                black > 20 && red > 20,
                "text/background covered: {black}/{red}"
            );
            assert!(marked > 20, "symbol covered");
            r.render().unwrap();
            let after = self.out.join(format!("planes-{zoom}-after.png"));
            r.save_screenshot(&after).unwrap();
            assert_eq!(im, image::open(after).unwrap().to_rgb8());
            cases.push(serde_json::json!({"zoom":zoom,"probes":probes,"text_black_pixels":black,"background_red_pixels":red,"symbol_marked_pixels":marked,"surface_export_equal":true}));
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"execution_os":std::env::consts::OS,"density":density,"distinct_planes":505,"cases":cases,"ic_xml_processed":true,"locally_authored_fixture":true,"resolved_rule_count":2,"full_s98_verified":false,"native_os_input_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut a = std::env::args().skip(1);
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: a.next().unwrap().into(),
            pc: a.next().unwrap().into(),
        })
        .unwrap();
}
