//! Real ordered premultiplied geographic billboards, not official IHO fixtures.
use ferrite_kernel::{geodesy::GeographicPosition, globe_navigation::GlobePose};
use ferrite_render::*;
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
}
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let dir = self.out.join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        for (id, color, opacity) in [
            ("A", "#ff0000", 1.),
            ("B", "#00ff00", 1.),
            ("ALPHA", "#00ff00", 0.5),
            ("EMPTY", "#0000ff", 0.),
        ] {
            std::fs::write(dir.join(format!("{id}.svg")),format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm" viewBox="-5 -5 10 10"><rect x="-5" y="-5" width="10" height="10" fill="{color}" opacity="{opacity}"/></svg>"##)).unwrap();
        }
        let w = Arc::new(
            e.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(1100, 800)),
            )
            .unwrap(),
        );
        let density = w.scale_factor() as f32;
        let mm = 96. / 25.4 * density;
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(dir.clone());
        let mut ctx = RenderContext::new(Viewport::with_origin(40., 50., 900., 600.));
        ctx.set_bounds(GeoBounds::new(-0.1, -0.1, 0.1, 0.1));
        r.ui_state.globe_preview = true;
        let mut rows = Vec::new();
        for tilt in [0., 45., 70.] {
            for factor in [1., 0.1, 2.] {
                r.ui_state.globe_pose = Some(GlobePose {
                    focus: GeographicPosition::new(0., 0.).unwrap(),
                    range_m: 30000. * factor,
                    heading_deg: 0.,
                    tilt_deg: tilt,
                });
                r.ui_state.globe_tilt_deg = tilt;
                let mut baseline = None;
                for history in ["cold", "warmed", "reset"] {
                    if history == "reset" {
                        cache.clear();
                        r.clear_symbol_textures();
                    }
                    if history == "warmed" {
                        cache.get_symbol("B", profile);
                        cache.get_symbol("A", profile);
                    }
                    ctx.clear_instructions();
                    for (id, offset) in [("A", 0.), ("B", 0.), ("A", 6.)] {
                        ctx.add_instruction(DrawingInstruction::Point(
                            PointInstruction::new(id.into(), WorldPoint::new(0., 0.))
                                .with_offset(offset, 0.)
                                .with_priority(24),
                        ));
                    }
                    r.begin_frame();
                    r.prepare_globe_with_symbols(&mut ctx, None, &mut cache, Some(profile))
                        .unwrap();
                    let d = r.globe_preview_diagnostics().unwrap();
                    assert_eq!(d.symbols, 3);
                    assert_eq!(d.missing_symbol_resources, 0);
                    assert_eq!(d.rejected_geometries, 0);
                    let path = self.out.join(format!("{tilt}-{factor}-{history}.png"));
                    r.save_screenshot(&path).unwrap();
                    let img = image::open(path).unwrap().to_rgb8();
                    let sample = |dx: f32| img.get_pixel((490. + dx * mm).round() as u32, 350).0;
                    assert_eq!(sample(0.), [0, 255, 0]);
                    assert_eq!(sample(6.), [255, 0, 0]);
                    assert_eq!(sample(3.), [255, 0, 0]);
                    if let Some(before) = &baseline {
                        assert_eq!(before, &img, "texture history changed portrayal order")
                    } else {
                        baseline = Some(img);
                    }
                }
                rows.push(serde_json::json!({"tilt":tilt,"range_factor":factor,"ordered_symbols":3,"history_pixel_difference":0,"physical_size_verified":true}));
            }
        }
        // Missing, fully transparent and far-side anchors cannot authorize children.
        ctx.clear_instructions();
        r.ui_state.globe_tilt_deg = 0.;
        r.ui_state.globe_pose = Some(GlobePose {
            focus: GeographicPosition::new(0., 0.).unwrap(),
            range_m: 1e6,
            heading_deg: 0.,
            tilt_deg: 0.,
        });
        for (n, id, lon) in [(0, "MISSING", 0.), (1, "EMPTY", 0.), (2, "A", 180.)] {
            let key = format!("p{n}");
            let mut p = DrawingInstruction::Point(PointInstruction::new(
                id.into(),
                WorldPoint::new(lon, 0.),
            ));
            p.set_dependency(DrawingDependency::new(1, Some(&key), None, false));
            ctx.add_instruction(p);
            let mut child = DrawingInstruction::Area(
                AreaInstruction::new(vec![
                    WorldPoint::new(-0.1, -0.1),
                    WorldPoint::new(0.1, -0.1),
                    WorldPoint::new(0.1, 0.1),
                    WorldPoint::new(-0.1, 0.1),
                ])
                .with_solid_fill(Color::rgb(1., 0., 0.)),
            );
            child.set_dependency(DrawingDependency::new(1, None, Some(&key), false));
            ctx.add_instruction(child);
        }
        r.begin_frame();
        r.prepare_globe_with_symbols(&mut ctx, None, &mut cache, Some(profile))
            .unwrap();
        assert_eq!(r.globe_preview_diagnostics().unwrap().areas, 0);
        assert_eq!(r.globe_preview_diagnostics().unwrap().symbols, 0);
        // Premultiplied alpha over an opaque prior symbol must not acquire dark fringes.
        ctx.clear_instructions();
        for id in ["A", "ALPHA"] {
            ctx.add_instruction(DrawingInstruction::Point(
                PointInstruction::new(id.into(), WorldPoint::new(0., 0.)).with_priority(24),
            ));
        }
        r.begin_frame();
        r.prepare_globe_with_symbols(&mut ctx, None, &mut cache, Some(profile))
            .unwrap();
        r.save_screenshot(self.out.join("alpha.png")).unwrap();
        let im = image::open(self.out.join("alpha.png")).unwrap().to_rgb8();
        let q = im.get_pixel(490, 350).0;
        assert!(
            (q[0] as i32 - 127).abs() <= 2 && (q[1] as i32 - 128).abs() <= 2 && q[2] == 0,
            "incorrect premultiplied blend {q:?}"
        );
        // Independent direction oracle: a WGS84 geodesic endpoint through the
        // actual camera, rather than the screen_rotation implementation.
        std::fs::write(dir.join("DIR.svg"),r##"<svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm" viewBox="-5 -5 10 10"><rect x="-0.7" y="-4" width="1.4" height="4" fill="#ff0000"/></svg>"##).unwrap();
        let cell = ferrite_s100_core::S101Cell::load(
            std::env::args().nth(3).expect("chart for rotation fixture"),
        )
        .unwrap();
        let mut rotation_rows = Vec::new();
        for tilt in [0., 45., 70.] {
            for heading in [0., 43.] {
                let pose = GlobePose {
                    focus: GeographicPosition::new(70., 179.9).unwrap(),
                    range_m: 30000.,
                    heading_deg: heading,
                    tilt_deg: tilt,
                };
                let camera = pose.camera([900., 600.]).unwrap();
                r.ui_state.globe_pose = Some(pose);
                r.ui_state.globe_tilt_deg = tilt;
                for (name, crs) in [
                    ("PortrayalCRS", RotationCrs::Portrayal),
                    ("GeographicCRS", RotationCrs::Geographic),
                    ("LocalCRS", RotationCrs::Local),
                    ("LineCRS", RotationCrs::Line),
                ] {
                    ctx.clear_instructions();
                    let result=ferrite_lua::PortrayalResult::parse("1",&format!("Rotation:{name},45;AugmentedPoint:GeographicCRS,179.9,70;DrawingPriority:24;PointInstruction:DIR"),"").unwrap();
                    ferrite_s101::convert_lua_results_for_cell(
                        &[result],
                        &cell,
                        &pc,
                        &mut ctx,
                        0,
                        "Day",
                    )
                    .unwrap();
                    assert_eq!(ctx.instruction_count(), 1);
                    let DrawingInstruction::Point(p) = &ctx.raw_instructions()[0] else {
                        panic!("expected point")
                    };
                    assert_eq!(p.rotation_crs, crs, "S101 adapter discarded basis");
                    let mut p = p.clone();
                    if matches!(crs, RotationCrs::Local | RotationCrs::Line) {
                        p.curve_tangent_bearing = Some(90.);
                    }
                    let bytes = bincode::serialize(&p).unwrap();
                    let restored: PointInstruction = bincode::deserialize(&bytes).unwrap();
                    assert_eq!(restored.rotation_crs, crs);
                    assert_eq!(restored.curve_tangent_bearing, p.curve_tangent_bearing);
                    ctx.clear_instructions();
                    ctx.add_instruction(DrawingInstruction::Point(restored));
                    let projected = |bearing| {
                        let end =
                            ferrite_kernel::geodesy::direct(pose.focus, bearing, 0.1).unwrap();
                        let q = camera
                            .project_visible(end.to_ecef(0.).unwrap())
                            .unwrap()
                            .unwrap()
                            .screen_px;
                        let v = [q[0] - 450., q[1] - 300.];
                        let n = v[0].hypot(v[1]);
                        [v[0] / n, v[1] / n]
                    };
                    let v = match crs {
                        RotationCrs::Geographic => projected(45.),
                        RotationCrs::Portrayal => {
                            [45f64.to_radians().sin(), -45f64.to_radians().cos()]
                        }
                        _ => {
                            let t = projected(90.);
                            let (sin, cos) = 45f64.to_radians().sin_cos();
                            let n = [t[1], -t[0]];
                            [n[0] * cos - n[1] * sin, n[0] * sin + n[1] * cos]
                        }
                    };
                    let mut first = None;
                    for history in ["cold", "reset"] {
                        if history == "reset" {
                            cache.clear();
                            r.clear_symbol_textures();
                        }
                        r.begin_frame();
                        r.prepare_globe_with_symbols(&mut ctx, None, &mut cache, Some(profile))
                            .unwrap();
                        let d = r.globe_preview_diagnostics().unwrap();
                        assert_eq!(d.symbols, 1);
                        assert_eq!(d.rejected_geometries, 0);
                        let path = self
                            .out
                            .join(format!("rotation-{tilt}-{heading}-{name}-{history}.png"));
                        r.save_screenshot(&path).unwrap();
                        let im = image::open(path).unwrap().to_rgb8();
                        let x = (490. + v[0] * 2.5 * mm as f64).round() as u32;
                        let y = (350. + v[1] * 2.5 * mm as f64).round() as u32;
                        assert_eq!(
                            im.get_pixel(x, y).0,
                            [255, 0, 0],
                            "rotation oracle mismatch {name} {tilt} {heading}"
                        );
                        let bx = (490. - v[0] * 2.5 * mm as f64).round() as u32;
                        let by = (350. - v[1] * 2.5 * mm as f64).round() as u32;
                        assert_ne!(im.get_pixel(bx, by).0, [255, 0, 0], "stem reversed");
                        if let Some(before) = &first {
                            assert_eq!(before, &im);
                        } else {
                            first = Some(im);
                        }
                    }
                    rotation_rows.push(serde_json::json!({"crs":name,"tilt":tilt,"heading":heading,"adapter_and_cache_preserved":true,"geodesic_pixel_oracle":true,"cache_reset_pixel_difference":0}));
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rotation_rows":rotation_rows,"rows":rows,"missing_empty_far_side_parents_withheld":true,"premultiplied_alpha_pixel":q,"physical_hardware_input_verified":false})).unwrap()).unwrap();
        e.exit();
    }
    fn window_event(&mut self, e: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            e.exit();
        }
    }
}
fn main() {
    let mut a = App {
        out: std::env::args().nth(1).unwrap().into(),
        pc: std::env::args().nth(2).unwrap().into(),
    };
    EventLoop::new().unwrap().run_app(&mut a).unwrap();
}
