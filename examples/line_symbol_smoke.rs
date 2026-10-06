//! Native S-101/Lua -> physical line placement -> SVG/GPU evidence.
use ferrite_render::*;
use ferrite_s100_core::*;
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
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
    pc: PathBuf,
    chart: PathBuf,
}
fn source(cell: &mut S101Cell, paths: Vec<Vec<WorldPoint>>, reverse: bool) {
    let mut f = cell.features.values().next().unwrap().clone();
    f.spatial_associations.clear();
    f.masks.clear();
    f.primitive_type = SpatialPrimitiveType::Curve;
    for (n, path) in paths.into_iter().enumerate() {
        let id = RecordId::new(120, 990001 + n as u32);
        cell.curves.insert(
            id.key(),
            CurveRecord {
                id,
                segments: vec![CurveSegment {
                    segment_type: SegmentType::Line,
                    positions: path
                        .into_iter()
                        .map(|p| Coordinate::new(p.x, p.y))
                        .collect(),
                }],
                start_point: None,
                end_point: None,
                update_instruction: 1,
            },
        );
        f.spatial_associations.push(SpatialAssociation {
            spatial_id: id,
            ornt: if reverse { 2 } else { 1 },
            usag: 0,
            mask: 0,
            scale_minimum: None,
            scale_maximum: None,
            update_instruction: 1,
        });
    }
    cell.features.insert(990001, f);
}
fn convert(
    cell: &S101Cell,
    pc: &ferrite_portrayal_catalog::PortrayalCatalogue,
    ctx: &mut RenderContext,
    mode: &str,
    offset: f64,
    visible: bool,
) {
    ctx.clear_instructions();
    let p=ferrite_lua::PortrayalResult::parse("990001",&format!("Rotation:LineCRS,0;DrawingPriority:24;LinePlacement:{mode},{offset},,{visible};PointInstruction:DOT"),"").unwrap();
    ferrite_s101::convert_lua_results_for_cell(&[p], cell, pc, ctx, 0, "Day").unwrap();
    assert!(ctx.raw_instructions().iter().all(|p|matches!(p,DrawingInstruction::Point(p) if p.line_placement.as_ref().is_some_and(|l|l.visible_parts==visible))));
    let bytes = bincode::serialize(ctx.raw_instructions()).unwrap();
    let restored: Vec<DrawingInstruction> = bincode::deserialize(&bytes).unwrap();
    ctx.set_instructions_from_cache(restored);
}
fn sample(path: &std::path::Path, q: [f64; 2]) {
    let im = image::open(path).unwrap().to_rgb8();
    assert_eq!(
        im.get_pixel(q[0].round() as u32, q[1].round() as u32).0,
        [0, 255, 0],
        "actual SVG absent at {q:?}"
    );
}
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let dir = self.out.join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DOT.svg"),r##"<svg xmlns="http://www.w3.org/2000/svg" width="2mm" height="2mm" viewBox="-1 -1 2 2"><rect x="-1" y="-1" width="2" height="2" fill="#00ff00"/></svg>"##).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(1100, 800)),
            )
            .unwrap(),
        );
        let ratio = w.scale_factor();
        let mm = 96. / 25.4 * ratio;
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(dir);
        let mut cell = S101Cell::load(&self.chart).unwrap();
        let mut ctx = RenderContext::new(Viewport::with_origin(40., 50., 900., 600.));
        ctx.scaler
            .set_projection(FlatProjection::EllipsoidalMercator);
        ctx.scaler.set_pixel_ratio(ratio);
        let mut rows = Vec::new();
        for zoom in [1., 10., 200.] {
            source(
                &mut cell,
                vec![vec![
                    WorldPoint::new(179.9, 70.),
                    WorldPoint::new(-179.9, 70.),
                ]],
                false,
            );
            ctx.zoom_to_fit(GeoBounds::new(
                179.9 - 0.02 / zoom,
                70. - 0.02 / zoom,
                179.9 + 0.02 / zoom,
                70. + 0.02 / zoom,
            ));
            convert(&cell, &pc, &mut ctx, "Absolute", 10., false);
            r.begin_frame();
            r.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
            assert!(
                !r.set_gpu_view_scaler(&ctx.scaler),
                "physical line anchors must be rebuilt instead of stretched"
            );
            let hits = r.displayed_symbols();
            assert_eq!(hits.len(), 1);
            let start = ctx.scaler.world_to_screen(WorldPoint::new(179.9, 70.));
            let q = hits[0].3;
            let error = ((q.x - start.x) as f64 - 10. * mm).abs();
            assert!(error < 0.01);
            let path = self.out.join(format!("flat-{zoom}.png"));
            r.save_screenshot(&path).unwrap();
            sample(&path, [start.x as f64 + 10. * mm, start.y as f64]);
            let first = image::open(&path).unwrap().to_rgba8();
            r.begin_frame();
            r.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
            r.save_screenshot(&path).unwrap();
            assert_eq!(first, image::open(&path).unwrap().to_rgba8());
            rows.push(serde_json::json!({"family":"flat_absolute","zoom":zoom,"mm_offset_error_px":error,"actual_adapter_and_svg":true,"rebuild_pixels":0,"fast_stretch_denied":true}));
        }
        // A geographic-degree midpoint is incorrect for a Mercator line.
        source(
            &mut cell,
            vec![vec![
                WorldPoint::new(179.9, 30.),
                WorldPoint::new(179.9, 70.),
            ]],
            false,
        );
        ctx.zoom_to_fit(GeoBounds::new(179.85, 30., 179.95, 70.));
        convert(&cell, &pc, &mut ctx, "Relative", 0.5, false);
        let a = ctx.scaler.world_to_screen(WorldPoint::new(179.9, 30.));
        let b = ctx.scaler.world_to_screen(WorldPoint::new(179.9, 70.));
        let expected = [(a.x + b.x) as f64 / 2., (a.y + b.y) as f64 / 2.];
        r.begin_frame();
        r.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
        let hits = r.displayed_symbols();
        assert_eq!(hits.len(), 1);
        let q = hits[0].3;
        let error = (q.x as f64 - expected[0]).hypot(q.y as f64 - expected[1]);
        assert!(error < 0.01);
        assert!((hits[0].2.y - 50.).abs() > 1.);
        let path = self.out.join("relative-mercator.png");
        r.save_screenshot(&path).unwrap();
        sample(&path, expected);
        rows.push(serde_json::json!({"family":"flat_relative","projected_midpoint_error_px":error,"degree_midpoint_rejected":true,"actual_adapter_and_svg":true}));
        // Viewport exit and re-entry must produce two independently placed symbols.
        ctx.zoom_to_fit(GeoBounds::new(-0.02, -0.02, 0.02, 0.02));
        let points = vec![(0., 200.), (1000., 200.), (1000., 500.), (0., 500.)]
            .into_iter()
            .map(|(x, y)| ctx.scaler.screen_to_world(ScreenPoint::new(x, y)))
            .collect();
        source(&mut cell, vec![points], false);
        convert(&cell, &pc, &mut ctx, "Relative", 0.5, true);
        r.begin_frame();
        r.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
        assert_eq!(r.displayed_symbols().len(), 2);
        let path = self.out.join("visible-reentry.png");
        r.save_screenshot(&path).unwrap();
        sample(&path, [490., 200.]);
        sample(&path, [490., 500.]);
        rows.push(serde_json::json!({"family":"flat_visible_parts","symbols":2,"viewport_reentry_separated":true,"actual_adapter_and_svg":true}));
        // Disconnected source curves must not acquire a phantom joining edge.
        source(
            &mut cell,
            vec![
                vec![WorldPoint::new(-0.018, 0.01), WorldPoint::new(-0.005, 0.01)],
                vec![WorldPoint::new(0.005, -0.01), WorldPoint::new(0.018, -0.01)],
            ],
            false,
        );
        convert(&cell, &pc, &mut ctx, "Relative", 0.5, false);
        assert_eq!(ctx.instruction_count(), 2);
        r.begin_frame();
        r.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
        assert_eq!(r.displayed_symbols().len(), 2);
        rows.push(serde_json::json!({"family":"disconnected","symbols":2,"no_phantom_connection":true,"actual_adapter_and_svg":true}));
        // Runtime obtains the actual directed source tangent, including reversal.
        source(
            &mut cell,
            vec![vec![
                WorldPoint::new(-0.018, 0.),
                WorldPoint::new(0.018, 0.),
            ]],
            true,
        );
        convert(&cell, &pc, &mut ctx, "Relative", 0.5, false);
        let DrawingInstruction::Point(p) = &ctx.raw_instructions()[0] else {
            unreachable!()
        };
        let points = resolve_flat_line_symbol(p, &ctx.scaler).unwrap();
        assert_eq!(points[0].curve_tangent_bearing, Some(270.));
        rows.push(serde_json::json!({"family":"source_reversal","bearing_deg":270.,"actual_adapter_source_tangent":true}));
        // Too-short curve must not clamp the symbol to its endpoint.
        source(
            &mut cell,
            vec![vec![WorldPoint::new(0., 0.), WorldPoint::new(0.001, 0.)]],
            false,
        );
        ctx.zoom_to_fit(GeoBounds::new(-0.02, -0.02, 0.02, 0.02));
        convert(&cell, &pc, &mut ctx, "Absolute", 1000., false);
        r.begin_frame();
        r.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
        assert!(r.displayed_symbols().is_empty());
        rows.push(serde_json::json!({"family":"beyond_end","symbols":0,"not_clamped":true}));
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rows":rows,"native_pixel_ratio":ratio,"physical_hardware_input_verified":false})).unwrap()).unwrap();
        e.exit();
    }
    fn window_event(&mut self, e: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            e.exit();
        }
    }
}
fn main() {
    let mut app = App {
        out: std::env::args().nth(1).unwrap().into(),
        pc: std::env::args().nth(2).unwrap().into(),
        chart: std::env::args().nth(3).unwrap().into(),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
