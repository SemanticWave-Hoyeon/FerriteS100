//! Hidden actual WgpuRenderer fixture; kernel regression, not IHO certification.
use ferrite_kernel::{geodesy::GeographicPosition, globe_navigation::GlobePose};
use ferrite_render::*;
use ferrite_wgpu::{globe_hatch::pattern_origin, SymbolCache, WgpuRenderer};
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
fn inside(ring: &[[f64; 2]], p: [f64; 2]) -> bool {
    let mut hit = false;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            hit = !hit;
        }
    }
    hit
}
fn distance(ring: &[[f64; 2]], p: [f64; 2]) -> f64 {
    ring.iter()
        .enumerate()
        .map(|(i, a)| {
            let b = ring[(i + 1) % ring.len()];
            let dx = b[0] - a[0];
            let dy = b[1] - a[1];
            let t = ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy);
            let t = t.clamp(0., 1.);
            (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
        })
        .fold(f64::INFINITY, f64::min)
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event: &ActiveEventLoop) {
        assert_eq!(std::env::var("FERRITE_BACKGROUND_TEST").as_deref(), Ok("1"));
        std::fs::create_dir_all(&self.out).unwrap();
        let window = Arc::new(
            event
                .create_window(
                    Window::default_attributes()
                        .with_title("Background pattern regression")
                        .with_visible(false)
                        .with_active(false)
                        .with_inner_size(PhysicalSize::new(900, 700)),
                )
                .unwrap(),
        );
        assert!(!window.is_visible().unwrap_or(true));
        assert!(!window.has_focus());
        let mut renderer = pollster::block_on(WgpuRenderer::new(window.clone())).unwrap();
        renderer.ui_state.globe_preview = true;
        let pc = self.out.join("fixture-pc");
        std::fs::create_dir_all(&pc).unwrap();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="4mm" height="4mm" viewBox="-2 -2 4 4"><rect x="0.25" y="-1" width="1.25" height="1" fill="red"/></svg>"#;
        std::fs::write(pc.join("ASYM.svg"), svg).unwrap();
        let mut cache = SymbolCache::new(&pc);
        let profile = ferrite_portrayal_catalog::ColorProfile::default();
        let ring = |r: f64| {
            vec![
                WorldPoint::new(-r, -r),
                WorldPoint::new(r, -r),
                WorldPoint::new(r, r),
                WorldPoint::new(-r, r),
            ]
        };
        let mut context = RenderContext::new(Viewport::with_origin(60., 50., 640., 480.));
        context.set_bounds(GeoBounds::new(-0.2, -0.2, 0.2, 0.2));
        let mut rows = Vec::new();
        let mut last_path = None;
        for samples in [1, 4] {
            renderer.set_globe_sample_count(samples).unwrap();
            for tilt in [0., 35.] {
                let pose = GlobePose {
                    focus: GeographicPosition::new(0., 0.).unwrap(),
                    range_m: 30000.,
                    heading_deg: 15.,
                    tilt_deg: tilt,
                };
                let camera = pose.camera([640., 480.]).unwrap();
                renderer.ui_state.globe_pose = Some(pose);
                renderer.ui_state.globe_tilt_deg = tilt;
                context.clear_instructions();
                renderer.begin_frame();
                renderer
                    .prepare_globe_with_symbols(&mut context, None, &mut cache, Some(&profile))
                    .unwrap();
                let basepath = self.out.join(format!("base-{samples}-{tilt}.png"));
                renderer.save_screenshot(&basepath).unwrap();
                let base = image::open(basepath).unwrap().to_rgb8();
                for crs in [
                    PatternCrs::Global,
                    PatternCrs::LocalGeometry,
                    PatternCrs::GlobalGeometry,
                ] {
                    for (v1, v2) in [
                        ((4., 0.), (0., 4.)),
                        ((4., 0.), (2., 4.)),
                        ((-4., 0.), (2., -4.)),
                        ((0., 4.), (4., 0.)),
                    ] {
                        let area = AreaInstruction::new(ring(0.09))
                            .with_interiors(vec![ring(0.015)])
                            .with_pattern_fill("ASYM".into(), v1, v2)
                            .with_pattern_crs(crs)
                            .with_cell_index(3)
                            .with_feature_id(42);
                        context.clear_instructions();
                        context.add_instruction(DrawingInstruction::Area(area.clone()));
                        renderer.begin_frame();
                        renderer
                            .prepare_globe_with_symbols(
                                &mut context,
                                None,
                                &mut cache,
                                Some(&profile),
                            )
                            .unwrap();
                        let diag = renderer.globe_preview_diagnostics().unwrap();
                        assert_eq!(diag.areas, 1, "{:?}", diag.reasons);
                        assert_eq!(diag.unsupported_commands, 0, "{:?}", diag.reasons);
                        assert_eq!(diag.rejected_geometries, 0, "{:?}", diag.reasons);
                        let name = format!(
                            "{samples}-{tilt}-{crs:?}-{}-{}-{}-{}",
                            v1.0, v1.1, v2.0, v2.1
                        );
                        let path = self.out.join(format!("{name}.png"));
                        renderer.save_screenshot(&path).unwrap();
                        last_path = Some(path.clone());
                        let image = image::open(path).unwrap().to_rgb8();
                        let project = |points: &[WorldPoint]| {
                            points
                                .iter()
                                .map(|p| {
                                    camera
                                        .project_visible(GeographicPosition::new(p.y, p.x).unwrap().to_ecef(0.).unwrap())
                                        .unwrap()
                                        .unwrap().screen_px
                                })
                                .collect::<Vec<_>>()
                        };
                        let exterior = project(&area.exterior);
                        let hole = project(&area.interiors[0]);
                        let origin =
                            pattern_origin(crs, &camera, area.exterior[0], WorldPoint::new(0., 0.))
                                .unwrap();
                        let ppm = context.scaler.pixels_per_mm();
                        let mut checked = 0;
                        let mut coloured = 0;
                        let mut pick_on = None;
                        let mut pick_gap = None;
                        let mut pick_hole = None;
                        for y in (0..480u32).step_by(3) {
                            for x in (0..640u32).step_by(3) {
                                let p = [f64::from(x) + 0.5, f64::from(y) + 0.5];
                                if distance(&exterior, p) < 3. || distance(&hole, p) < 3. {
                                    continue;
                                }
                                let polygon = inside(&exterior, p) && !inside(&hole, p);
                                // Independent authored site enumeration; inverse lattice is used only to
                                // bound the search, never to derive motif occupancy or its physical shape.
                                let a = [f64::from(v1.0) * ppm, -f64::from(v1.1) * ppm];
                                let b = [f64::from(v2.0) * ppm, -f64::from(v2.1) * ppm];
                                let q = [p[0] - origin[0], p[1] - origin[1]];
                                let det = a[0] * b[1] - b[0] * a[1];
                                let n = ((q[0] * b[1] - b[0] * q[1]) / det).floor() as i32;
                                let m = ((a[0] * q[1] - q[0] * a[1]) / det).floor() as i32;
                                let mut motif = false;
                                let mut boundary = false;
                                for i in n - 2..n + 3 {
                                    for j in m - 2..m + 3 {
                                        let site = [
                                            origin[0] + f64::from(i) * a[0] + f64::from(j) * b[0],
                                            origin[1] + f64::from(i) * a[1] + f64::from(j) * b[1],
                                        ];
                                        let r = [p[0] - site[0], p[1] - site[1]];
                                        let minx = 0.25 * ppm;
                                        let maxx = 1.5 * ppm;
                                        let miny = -ppm;
                                        let maxy = 0.;
                                        motif |= r[0] > minx
                                            && r[0] < maxx
                                            && r[1] > miny
                                            && r[1] < maxy;
                                        if r[0] > minx - 1.5
                                            && r[0] < maxx + 1.5
                                            && r[1] > miny - 1.5
                                            && r[1] < maxy + 1.5
                                        {
                                            boundary |= (r[0] - minx).abs() < 1.5
                                                || (r[0] - maxx).abs() < 1.5
                                                || (r[1] - miny).abs() < 1.5
                                                || r[1].abs() < 1.5;
                                        }
                                    }
                                }
                                if boundary {
                                    continue;
                                }
                                let expected = polygon && motif;
                                let pixel = image.get_pixel(x + 60, y + 50).0;
                                if expected {
                                    assert!(
                                        pixel[0] >= 250 && pixel[1] <= 3 && pixel[2] <= 3,
                                        "{name} missing {p:?} {pixel:?}"
                                    );
                                    coloured += 1;
                                    pick_on.get_or_insert(p);
                                } else {
                                    let old = base.get_pixel(x + 60, y + 50).0;
                                    assert!(
                                        (0..3).all(|i| (i16::from(pixel[i]) - i16::from(old[i]))
                                            .abs()
                                            <= 3),
                                        "{name} unwanted {p:?} {pixel:?} baseline {old:?}"
                                    );
                                    if polygon {
                                        pick_gap.get_or_insert(p);
                                    }
                                    if inside(&hole, p) {
                                        pick_hole.get_or_insert(p);
                                    }
                                }
                                checked += 1;
                            }
                        }
                        assert!(checked > 1000 && coloured > 20);
                        let hit = |renderer: &mut WgpuRenderer, p: [f64; 2]| {
                            renderer
                                .globe_feature_candidates(
                                    &context,
                                    ScreenPoint::new(p[0] as f32 + 60., p[1] as f32 + 50.),
                                    0.,
                                )
                                .unwrap()
                        };
                        assert!(hit(&mut renderer, pick_on.unwrap())
                            .iter()
                            .any(|(s, _)| *s == 0));
                        assert!(hit(&mut renderer, pick_gap.unwrap()).is_empty());
                        assert!(hit(&mut renderer, pick_hole.unwrap()).is_empty());
                        let DrawingInstruction::Area(source) = &context.raw_instructions()[0]
                        else {
                            panic!("source changed")
                        };
                        assert_eq!(source.feature_id, Some(42));
                        assert_eq!(source.cell_index, Some(3));
                        rows.push(serde_json::json!({"name":name,"samples":samples,"crs":format!("{crs:?}"),"tilt":tilt,"checked":checked,"coloured":coloured,"pixels_per_mm":ppm,"pick_motif":true,"pick_gap_empty":true,"pick_hole_empty":true,"source_feature":42,"source_cell":3}));
                    }
                }
            }
        }
        // Replacing the PC at the same path, symbol ID, calibration, lattice and
        // color profile must invalidate texture shape as well as source color.
        let before = image::open(last_path.unwrap()).unwrap().to_rgb8();
        assert_eq!(std::fs::read_to_string(pc.join("ASYM.svg")).unwrap(), svg);
        let replaced = svg
            .replace("x=\"0.25\"", "x=\"-0.75\"")
            .replace("fill=\"red\"", "fill=\"blue\"");
        std::fs::write(pc.join("ASYM.svg"), &replaced).unwrap();
        let mut replacement = SymbolCache::new(&pc);
        assert_ne!(replacement.resource_revision(), cache.resource_revision());
        renderer.begin_frame();
        renderer
            .prepare_globe_with_symbols(&mut context, None, &mut replacement, Some(&profile))
            .unwrap();
        let path = self.out.join("same-id-pc-replacement.png");
        renderer.save_screenshot(&path).unwrap();
        let after = image::open(path).unwrap().to_rgb8();
        let mut changed_to_gap = 0;
        let mut changed_to_motif = 0;
        let mut blue_probe = None;
        for y in 0..480u32 {
            for x in 0..640u32 {
                let a = before.get_pixel(x + 60, y + 50).0;
                let b = after.get_pixel(x + 60, y + 50).0;
                let old = a[0] >= 250 && a[1] <= 3 && a[2] <= 3;
                let new = b[2] >= 250 && b[1] <= 3 && b[0] <= 3;
                changed_to_gap += usize::from(old && !new);
                changed_to_motif += usize::from(new && !old);
                if new {
                    blue_probe.get_or_insert([x as f32 + 60.5, y as f32 + 50.5]);
                }
            }
        }
        assert!(
            changed_to_gap > 20 && changed_to_motif > 20,
            "same-name PC geometry stayed stale"
        );
        let probe = blue_probe.unwrap();
        assert!(renderer
            .globe_feature_candidates(&context, ScreenPoint::new(probe[0], probe[1]), 0.)
            .unwrap()
            .iter()
            .any(|(source, _)| *source == 0));
        assert!(!window.is_visible().unwrap_or(true));
        assert!(!window.has_focus());
        rows.push(serde_json::json!({"name":"same-id-pc-replacement","changed_to_gap":changed_to_gap,"changed_to_motif":changed_to_motif,"pick_source":0}));
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":rows,"native_renderer_path":true,"official_iho_fixture":false,"visible":false,"focused":false})).unwrap()).unwrap();
        event.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let out = std::env::args().nth(1).expect("output directory");
    EventLoop::new()
        .unwrap()
        .run_app(&mut App { out: out.into() })
        .unwrap();
}
