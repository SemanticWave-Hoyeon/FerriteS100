//! Local native fixtures for S-100 Parent execution, not an official IHO test set.
use ferrite_render::*;
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
use std::{collections::HashSet, path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
fn point(
    fid: i64,
    id: Option<&str>,
    parent: Option<&str>,
    name: &str,
    x: f64,
    y: f64,
    priority: i32,
) -> DrawingInstruction {
    let mut i = DrawingInstruction::Point(
        PointInstruction::new(name.into(), WorldPoint::new(x, y))
            .with_feature_id(fid)
            .with_priority(priority),
    );
    i.set_dependency(DrawingDependency::new(1, id, parent, false));
    i
}
fn line(
    fid: i64,
    id: Option<&str>,
    parent: Option<&str>,
    priority: i32,
    end: f64,
) -> DrawingInstruction {
    let mut i = DrawingInstruction::Line(
        LineInstruction::new(vec![WorldPoint::new(2., 5.), WorldPoint::new(end, 5.)])
            .with_feature_id(fid)
            .with_priority(priority),
    );
    i.set_dependency(DrawingDependency::new(1, id, parent, false));
    i
}
struct App {
    out: PathBuf,
    pc: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let fixtures = self.out.join("fixtures");
        std::fs::create_dir_all(&fixtures).unwrap();
        for (name, color) in [
            ("DEP_PARENT", "#ff0000"),
            ("DEP_CHILD", "#00ff00"),
            ("DEP_OTHER", "#0000ff"),
        ] {
            std::fs::write(fixtures.join(format!("{name}.svg")),format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="6mm" height="6mm" viewBox="-3 -3 6 6"><rect x="-3" y="-3" width="6" height="6" fill="{color}"/></svg>"##)).unwrap();
        }
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("S-100 Parent execution fixtures")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let density = window.scale_factor();
        let size = window.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        r.background_color = Color::WHITE;
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(fixtures);
        let mut rows = Vec::new();
        for (case, child_visible, diagnostic) in [
            ("wrapped-parent", true, false),
            ("offset-culled-parent", false, false),
            ("visible-text-parent", true, false),
            ("text-collision", false, false),
            ("whitespace-text", false, false),
            ("valid-chain", true, false),
            ("missing-root-chain", false, false),
            ("visible-forward", true, false),
            ("missing-symbol", false, false),
            ("outside-viewport", false, false),
            ("viewing-group-off", false, false),
            ("scale-hidden", false, false),
            ("date-hidden", false, false),
            ("same-id-or", true, false),
            ("missing-id", false, false),
            ("namespace-mismatch", false, false),
            ("rootless-cycle", false, false),
            ("root-deduplicated", false, false),
            ("suppressed-line", false, false),
            ("partial-line", true, false),
            ("hidden-suppressor-restores-parent", true, false),
            ("self-suppressing-conflict", false, true),
            ("empty-area", false, false),
            ("empty-text", false, false),
        ] {
            let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
            c.settings.current_datetime = Some("2026-10-04T09:30:00+09:00".into());
            let mut root = point(1, Some("a"), None, "DEP_PARENT", 3., 4., 5);
            let mut child = point(2, Some("b"), Some("a"), "DEP_CHILD", 6., 7., 1);
            let mut groups: Option<HashSet<u32>> = None;
            match case {
                "missing-symbol" | "same-id-or" | "missing-root-chain" => {
                    if let DrawingInstruction::Point(p) = &mut root {
                        p.symbol_ref = "DOES_NOT_EXIST_LOCAL_FIXTURE".into();
                    }
                }
                "wrapped-parent" => {
                    if let DrawingInstruction::Point(p) = &mut root {
                        p.position.x -= 360.;
                    }
                }
                "offset-culled-parent" => {
                    if let DrawingInstruction::Point(p) = &mut root {
                        p.local_offset = (3000., 0.);
                    }
                }
                "outside-viewport" => {
                    if let DrawingInstruction::Point(p) = &mut root {
                        p.position.x = 30.;
                    }
                }
                "viewing-group-off" => {
                    if let DrawingInstruction::Point(p) = &mut root {
                        p.viewing_group = ViewingGroup(999);
                    };
                    groups = Some(HashSet::from([21010]));
                }
                "scale-hidden" => {
                    if let DrawingInstruction::Point(p) = &mut root {
                        p.scale_range = ScaleRange {
                            scale_minimum: Some(1),
                            scale_maximum: Some(1),
                        };
                    }
                }
                "date-hidden" => {
                    let bounds = ferrite_kernel::TemporalBounds::new(
                        Some("20200101".into()),
                        Some("20200102".into()),
                    )
                    .unwrap();
                    root.set_time_intervals(&[ferrite_kernel::TemporalInterval::new(
                        Some(bounds),
                        None,
                        None,
                        ferrite_kernel::IntervalClosure::Closed,
                    )
                    .unwrap()]);
                }
                "missing-id" => child.set_dependency(DrawingDependency::new(
                    1,
                    Some("b"),
                    Some("absent"),
                    false,
                )),
                "namespace-mismatch" => {
                    child.set_dependency(DrawingDependency::new(2, Some("b"), Some("a"), false))
                }
                "rootless-cycle" => {
                    root.set_dependency(DrawingDependency::new(1, Some("a"), Some("b"), false))
                }
                "root-deduplicated" => {
                    c.add_instruction(point(5, Some("other"), None, "DEP_PARENT", 3., 4., 0))
                }
                "suppressed-line" | "partial-line" => {
                    root = line(1, Some("a"), None, 5, 8.);
                    c.add_instruction(line(
                        4,
                        None,
                        None,
                        9,
                        if case == "partial-line" { 5. } else { 8. },
                    ));
                }
                "hidden-suppressor-restores-parent" => {
                    root = point(
                        1,
                        Some("missing"),
                        None,
                        "DOES_NOT_EXIST_LOCAL_FIXTURE",
                        3.,
                        4.,
                        5,
                    );
                    c.add_instruction(line(5, Some("a"), None, 5, 8.));
                    c.add_instruction(line(4, Some("suppressor"), Some("missing"), 9, 8.));
                }
                "self-suppressing-conflict" => {
                    root = line(1, Some("a"), None, 5, 8.);
                    c.add_instruction(line(4, Some("suppressor"), Some("a"), 9, 8.));
                }
                "empty-area" => {
                    root = DrawingInstruction::Area(
                        AreaInstruction::new(Vec::new()).with_feature_id(1),
                    );
                    root.set_dependency(DrawingDependency::new(1, Some("a"), None, false));
                }
                "visible-text-parent" | "text-collision" | "whitespace-text" => {
                    let text = if case == "whitespace-text" {
                        "   "
                    } else {
                        "Parent label"
                    };
                    root = DrawingInstruction::Text(
                        TextInstruction::new(text.into(), WorldPoint::new(3., 4.))
                            .with_feature_id(1)
                            .with_priority(5),
                    );
                    root.set_dependency(DrawingDependency::new(1, Some("a"), None, false));
                    if case == "text-collision" {
                        c.add_instruction(DrawingInstruction::Text(
                            TextInstruction::new(text.into(), WorldPoint::new(3., 4.))
                                .with_feature_id(5)
                                .with_priority(10),
                        ));
                    }
                }
                "empty-text" => {
                    root = DrawingInstruction::Text(
                        TextInstruction::new(String::new(), WorldPoint::new(3., 4.))
                            .with_feature_id(1),
                    );
                    root.set_dependency(DrawingDependency::new(1, Some("a"), None, false));
                }
                _ => {}
            }
            if case == "same-id-or" {
                c.add_instruction(point(5, Some("a"), None, "DEP_PARENT", 8., 4., 5));
            }
            if case.ends_with("-chain") {
                c.add_instruction(point(3, Some("c"), Some("b"), "DEP_OTHER", 8., 8., 0));
            }
            c.add_instruction(child);
            c.add_instruction(root);
            c.add_instruction(point(6, None, None, "DEP_OTHER", 1., 1., 5));
            r.begin_frame();
            r.set_lon_wrap_pixels(if case == "wrapped-parent" {
                360. * c.scaler.scale_x() as f32
            } else {
                0.
            });
            r.add_instructions_with_symbols(
                &mut c,
                Some(&mut cache),
                Some(profile),
                groups.as_ref(),
            );
            let ids: Vec<_> = r.displayed_symbols().iter().filter_map(|s| s.1).collect();
            assert_eq!(ids.contains(&2), child_visible, "{case}: {ids:?}");
            assert!(ids.contains(&6), "independent root lost: {case}");
            if case.ends_with("-chain") {
                assert_eq!(ids.contains(&3), child_visible);
            }
            assert!(r.requires_visibility_rebuild_for_navigation());
            assert!(!r.set_gpu_view_scaler(&c.scaler));
            let status = r.dependency_render_status().clone();
            assert_eq!(
                status.nonconvergent_diagnostic, diagnostic,
                "{case}: {status:?}"
            );
            assert_eq!(status.converged, !diagnostic);
            let geom: Vec<_> = r
                .displayed_geometry()
                .iter()
                .filter_map(|&i| c.raw_instructions()[i].feature_id())
                .collect();
            if case == "hidden-suppressor-restores-parent" {
                assert!(geom.contains(&5));
                assert!(!geom.contains(&4));
                assert!(status.iterations >= 3);
            }
            if case == "suppressed-line" {
                assert!(!geom.contains(&1));
                assert!(geom.contains(&4));
            }
            if case == "partial-line" {
                assert!(geom.contains(&1) && geom.contains(&4));
            }
            if case == "self-suppressing-conflict" {
                assert!(geom.contains(&1));
                assert!(!geom.contains(&4));
            }
            let png = self.out.join(format!("{case}.png"));
            r.save_screenshot(&png).unwrap();
            let im = image::open(&png).unwrap().to_rgb8();
            let green = im.pixels().filter(|p| p.0 == [0, 255, 0]).count();
            assert_eq!(green > 0, child_visible, "{case}: green={green}");
            // At 1x density anti-aliased glyphs need not contain a fully black pixel.
            let black = im
                .pixels()
                .filter(|p| p[0] == p[1] && p[1] == p[2] && p[0] <= 64)
                .count();
            if case == "visible-text-parent" || case == "text-collision" {
                assert!(black > 0, "actual font upload missing");
            }
            if case == "whitespace-text" {
                assert_eq!(black, 0);
            }
            if case == "wrapped-parent" {
                assert!(
                    im.pixels().any(|p| p.0 == [255, 0, 0]),
                    "wrapped parent missing"
                );
            }
            rows.push(serde_json::json!({"case":case,"child_visible":child_visible,"displayed_symbol_feature_ids":ids,"displayed_geometry_feature_ids":geom,"green_pixels":green,"dark_glyph_pixels":black,"status":status.audit_value(),"native_density":density,"physical_input_verified":false,"official_iho_test_fixture":false}));
        }
        r.begin_frame();
        assert_eq!(r.dependency_render_status().iterations, 0);
        assert!(r.dependency_render_status().permitted.is_empty());
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
}
fn main() {
    let mut a = std::env::args().skip(1);
    let mut app = App {
        out: a.next().unwrap().into(),
        pc: a.next().unwrap().into(),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
