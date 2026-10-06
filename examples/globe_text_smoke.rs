//! Actual ordered WGPU globe glyphs against the existing map glyph renderer.
use ferrite_kernel::{
    geodesy::{direct, GeographicPosition},
    globe_navigation::GlobePose,
};
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
        let w = Arc::new(
            e.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(1100, 800)),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let dir = self.out.join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("WHITE.svg"),r##"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="100mm" viewBox="-50 -50 100 100"><rect x="-50" y="-50" width="100" height="100" fill="#ffffff"/></svg>"##).unwrap();
        let mut cache = SymbolCache::new(dir);
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut rows = Vec::new();
        for lat in [0., 70.] {
            for tilt in [0., 45., 70.] {
                for range in [30000., 1500.] {
                    let focus = GeographicPosition::new(lat, 179.9).unwrap();
                    let pose = GlobePose {
                        focus,
                        range_m: range,
                        heading_deg: 23.,
                        tilt_deg: tilt,
                    };
                    let camera = pose.camera([900., 600.]).unwrap();
                    let mut c = RenderContext::new(Viewport::with_origin(40., 50., 900., 600.));
                    c.scaler.set_projection(FlatProjection::EllipsoidalMercator);
                    c.set_bounds(GeoBounds::new(178.9, lat - 1., 180.9, lat + 1.));
                    // An authored screen-fixed white test backdrop isolates glyphs
                    // from unrelated Earth/draped-area depth approximation seams.
                    c.add_instruction(DrawingInstruction::Point(
                        PointInstruction::new("WHITE".into(), WorldPoint::new(179.9, lat))
                            .with_scale(10.)
                            .with_priority(1),
                    ));
                    let t = TextInstruction::new("Heading".into(), WorldPoint::new(179.9, lat))
                        .with_font_size(24.)
                        .with_color(Color::rgb(0., 0., 1.))
                        .with_rotation(45.)
                        .with_rotation_crs(RotationCrs::Geographic)
                        .with_alignment(HAlign::Center, VAlign::Middle)
                        .with_offset(2., 3.)
                        .with_priority(20);
                    c.add_instruction(DrawingInstruction::Text(t.clone()));
                    let mut low = t.clone();
                    low.color = Color::rgb(1., 0., 0.);
                    low.priority.0 = 3;
                    c.add_instruction(DrawingInstruction::Text(low));
                    let alpha = TextInstruction::new("Tint".into(), WorldPoint::new(179.9, lat))
                        .with_font_size(18.)
                        .with_color(Color::rgba(0., 0., 1., 0.5))
                        .with_alignment(HAlign::Center, VAlign::Middle)
                        .with_offset(20., -18.)
                        .with_priority(20);
                    c.add_instruction(DrawingInstruction::Text(alpha.clone()));
                    let mut bg = TextInstruction::new("BG".into(), WorldPoint::new(179.9, lat))
                        .with_font_size(24.)
                        .with_color(Color::rgba(0., 0., 0., 0.))
                        .with_alignment(HAlign::Center, VAlign::Middle)
                        .with_offset(-30., -20.)
                        .with_priority(20);
                    bg.background = Some(Color::rgba(0., 1., 0., 0.5));
                    c.add_instruction(DrawingInstruction::Text(bg.clone()));
                    let mut far = t.clone();
                    far.position.x -= 180.;
                    far.color = Color::rgb(1., 0., 0.);
                    c.add_instruction(DrawingInstruction::Text(far));
                    let mut empty = DrawingInstruction::Text(TextInstruction::new(
                        "".into(),
                        WorldPoint::new(179.9, lat),
                    ));
                    empty.set_dependency(DrawingDependency::new(5, Some("empty"), None, false));
                    c.add_instruction(empty);
                    let mut child =
                        DrawingInstruction::Text(t.clone().with_color(Color::rgb(1., 0., 0.)));
                    child.set_dependency(DrawingDependency::new(
                        5,
                        Some("child"),
                        Some("empty"),
                        false,
                    ));
                    c.add_instruction(child);
                    r.ui_state.globe_preview = true;
                    r.ui_state.globe_pose = Some(pose);
                    r.ui_state.globe_tilt_deg = tilt;
                    let index = rows.len();
                    let mut first = None;
                    let mut diagnostics = None;
                    for history in ["cold", "warm"] {
                        r.begin_frame();
                        r.prepare_globe_with_symbols(&mut c, None, &mut cache, Some(profile))
                            .unwrap();
                        let d = r.globe_preview_diagnostics().unwrap();
                        assert_eq!(d.texts, 3, "dependency/collision/horizon execution");
                        assert_eq!(d.unsupported_commands, 0);
                        assert_eq!(d.rejected_geometries, 0, "{:?}", d.reasons);
                        diagnostics = Some(d.as_json());
                        let path = self.out.join(format!("case{index}-{history}.png"));
                        r.save_screenshot(&path).unwrap();
                        let image = image::open(path).unwrap().to_rgba8();
                        if let Some(old) = &first {
                            assert_eq!(&image, old);
                        } else {
                            first = Some(image);
                        }
                    }
                    let actual = first.unwrap();
                    assert_eq!(actual.get_pixel(0, 0).0, [255, 255, 255, 255]);
                    let red = actual
                        .pixels()
                        .filter(|p| p[0] > 200 && p[1] < 100 && p[2] < 100)
                        .count();
                    assert_eq!(red, 0, "hidden/dependent text became visible");
                    // Independent central geodesic direction, rather than the portrayal derivative.
                    let a = direct(focus, 225., 0.5).unwrap().to_ecef(0.).unwrap();
                    let b = direct(focus, 45., 0.5).unwrap().to_ecef(0.).unwrap();
                    let a = camera.project_visible(a).unwrap().unwrap();
                    let b = camera.project_visible(b).unwrap().unwrap();
                    let expected = (b.screen_px[0] - a.screen_px[0])
                        .atan2(a.screen_px[1] - b.screen_px[1])
                        .to_degrees()
                        .rem_euclid(360.) as f32;
                    let anchor = camera
                        .project_visible(focus.to_ecef(0.).unwrap())
                        .unwrap()
                        .unwrap()
                        .screen_px;
                    let mut reference = RenderContext::new(c.scaler.viewport);
                    reference.set_bounds(c.scaler.geo_bounds);
                    reference
                        .scaler
                        .set_projection(FlatProjection::EllipsoidalMercator);
                    let world = reference.scaler.screen_to_world(ScreenPoint::new(
                        anchor[0] as f32 + 40.,
                        anchor[1] as f32 + 50.,
                    ));
                    for mut text in [t.clone(), alpha.clone(), bg.clone()] {
                        text.position = world;
                        text.rotation_crs = RotationCrs::Portrayal;
                        if text.rotation != 0. {
                            text.rotation = expected;
                        }
                        reference.add_instruction(DrawingInstruction::Text(text));
                    }
                    r.ui_state.globe_preview = false;
                    r.clear_globe_preview();
                    r.begin_frame();
                    r.add_instructions_with_symbols(&mut reference, None, None, None);
                    let path = self.out.join(format!("case{index}-reference.png"));
                    r.save_screenshot(&path).unwrap();
                    let refimage = image::open(path).unwrap().to_rgba8();
                    let glyph = |p: [u8; 4]| p[2] >= 254 && p[0] < 250 && p[1] < 250;
                    let centre = [
                        (anchor[0] + 40. - 30. * 96. / 25.4 * density).round() as u32,
                        (anchor[1] + 50. + 20. * 96. / 25.4 * density).round() as u32,
                    ];
                    let actual_bg = actual.get_pixel(centre[0], centre[1]).0;
                    let reference_bg = refimage.get_pixel(centre[0], centre[1]).0;
                    assert!(
                        (0..3).all(|k| actual_bg[k].abs_diff([128, 255, 128][k]) <= 2
                            && actual_bg[k].abs_diff(reference_bg[k]) <= 2),
                        "background physical anchor/alpha {:?} {:?}",
                        actual_bg,
                        reference_bg
                    );
                    let mut changed = 0;
                    let mut maximum = 0;
                    let mut nonedge = 0;
                    let mut centroid = [[0f64; 3]; 2];
                    let mut bbox = [[u32::MAX, u32::MAX, 0, 0]; 2];
                    for y in 180..620 {
                        for x in 220..800 {
                            let a = actual.get_pixel(x, y).0;
                            let b = refimage.get_pixel(x, y).0;
                            for (k, p) in [a, b].into_iter().enumerate() {
                                if glyph(p) {
                                    let weight = (255 - p[0]) as f64;
                                    centroid[k][0] += x as f64 * weight;
                                    centroid[k][1] += y as f64 * weight;
                                    centroid[k][2] += weight;
                                    bbox[k][0] = bbox[k][0].min(x);
                                    bbox[k][1] = bbox[k][1].min(y);
                                    bbox[k][2] = bbox[k][2].max(x);
                                    bbox[k][3] = bbox[k][3].max(y);
                                }
                            }
                            if !(glyph(a) || glyph(b)) {
                                continue;
                            }
                            let delta = (0..4).map(|i| a[i].abs_diff(b[i])).max().unwrap();
                            if delta > 0 {
                                changed += 1;
                                maximum = maximum.max(delta);
                            }
                            if delta > 2 {
                                let edge = |image: &image::RgbaImage| {
                                    let mut min = 255;
                                    let mut max = 0;
                                    for yy in y - 1..=y + 1 {
                                        for xx in x - 1..=x + 1 {
                                            let p = image.get_pixel(xx, yy).0;
                                            min = min.min(p[0]);
                                            max = max.max(p[0]);
                                        }
                                    }
                                    max - min > 5
                                };
                                if !(edge(&actual) && edge(&refimage)) {
                                    nonedge += 1;
                                }
                            }
                        }
                    }
                    assert_eq!(nonedge, 0, "non-edge glyph colour discrepancy");
                    assert_eq!(bbox[0], bbox[1], "glyph placement/physical size mismatch");
                    let centroid_drift = ((centroid[0][0] / centroid[0][2]
                        - centroid[1][0] / centroid[1][2])
                        .powi(2)
                        + (centroid[0][1] / centroid[0][2] - centroid[1][1] / centroid[1][2])
                            .powi(2))
                    .sqrt();
                    assert!(
                        centroid_drift < 0.05,
                        "glyph ink centroid changed {centroid_drift}"
                    );
                    let blue = actual
                        .pixels()
                        .filter(|p| p[2] > 200 && p[0] < 190 && p[1] < 190)
                        .count();
                    assert!(blue > 300);
                    rows.push(serde_json::json!({"case":index,"latitude":lat,"tilt":tilt,"range_m":range,"density":density,"warm_cold_changed_pixels":0,"reference_roi":[220,180,580,440],"reference_changed_pixels":changed,"reference_max_channel_difference":maximum,"nonedge_colour_discrepancies":nonedge,"glyph_bbox":bbox[0],"reference_bbox":bbox[1],"ink_centroid_drift_px":centroid_drift,"physical_vertex_roundtrip_limit_px":1e-5,"blue_pixels":blue,"forbidden_red_pixels":red,"diagnostics":diagnostics,"physical_input_verified":false,"background_centre":centre,"background_actual_rgba":actual_bg,"background_reference_rgba":reference_bg}));
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rows":rows,"independent_geodesic_basis":true,"map_glyph_renderer_reference":true,"complete_line_text_placement":false})).unwrap()).unwrap();
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
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
