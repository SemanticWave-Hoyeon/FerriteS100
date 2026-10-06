//! Actual flat renderer coverage bindings; synthetic geometry isolates each primitive.
use ferrite_kernel::{
    coverage_frame::CoverageFrame,
    coverage_selection::{CoverageFootprint, Region, SelectedCoverage, Selection},
    scale_policy::CoverageScaleRange,
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
fn frame(size: [u32; 2]) -> Arc<CoverageFrame> {
    let [w, h] = size.map(f64::from);
    let rect = |right| {
        Region::from_rings(&[[0., 0.], [right, 0.], [right, h], [0., h], [0., 0.]], &[]).unwrap()
    };
    let viewport = rect(w);
    let footprints = [(180000, viewport.clone()), (45000, rect(w / 2.))]
        .into_iter()
        .enumerate()
        .map(|(id, (min, region))| CoverageFootprint {
            dataset_id: id,
            coverage_id: id as i64,
            region,
            scales: CoverageScaleRange {
                minimum_denominator: Some(min),
                optimum_denominator: min / 2,
                maximum_denominator: min / 4,
            },
        })
        .collect::<Vec<_>>();
    let selection = Selection {
        display_band: 10,
        coverages: (0..2)
            .map(|i| SelectedCoverage {
                inventory_index: i,
                selection_band: 10,
                selected_to_fill_gap: false,
            })
            .collect(),
        uncovered: Region::from_polygons(vec![]).unwrap(),
    };
    Arc::new(
        CoverageFrame::new(
            &footprints,
            &selection,
            &viewport,
            size,
            4 * size[0] as usize * size[1] as usize,
        )
        .unwrap(),
    )
}

fn command(kind: &str, anchor: WorldPoint, mm: [f32; 2]) -> DrawingInstruction {
    match kind {
        "symbol" => DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), anchor)
                .with_scale(4.)
                .with_offset(mm[0], mm[1]),
        ),
        "text" => DrawingInstruction::Text(
            TextInstruction::new("Local mm".into(), anchor)
                .with_font_size(24.)
                .with_offset(mm[0], mm[1])
                .with_alignment(HAlign::Center, VAlign::Middle),
        ),
        _ => unreachable!(),
    }
}
struct App {
    out: PathBuf,
    pc: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("S-100 Local physical position verification")
                    .with_inner_size(PhysicalSize::new(640, 400)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let density = window.scale_factor();
        let extent = [size.width, size.height];
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        renderer.set_lon_wrap_pixels(0.);
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let coverage = frame(extent);
        let mut rows = Vec::new();
        for kind in ["symbol", "text"] {
            for zoom in [1., 4., 200.] {
                let mut reference: Option<image::RgbaImage> = None;
                for local in [false, true] {
                    let mut c =
                        RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                    let half = 0.5 / zoom;
                    c.set_bounds(GeoBounds::new(
                        0.5 - half,
                        0.5 - half,
                        0.5 + half,
                        0.5 + half,
                    ));
                    c.scaler.set_pixel_ratio(density);
                    let anchor = WorldPoint::new(0.5, 0.5);
                    let mut inst = command(kind, anchor, [9., 1.]); // Augmented mm [8,3] plus glyph [1,-2].
                    inst.set_portrayal_origin(if local {
                        PortrayalOrigin::augmented_local_point(anchor, [8., 3.]).unwrap()
                    } else {
                        PortrayalOrigin::NonPoint
                    });
                    c.add_instruction(inst);
                    c.get_sorted_instructions();
                    if local {
                        let pass = PreparedCoveragePass::prepare(
                            c.raw_instructions(),
                            coverage.clone(),
                            |_| Ok(InstructionCoverageClass::Dataset(0)),
                            |_, p| {
                                let PointOriginGeometry::AugmentedLocalPoint {
                                    reference_point,
                                    millimetres,
                                } = p
                                else {
                                    panic!()
                                };
                                let s = c.scaler.world_to_screen(*reference_point);
                                let ppm = c.scaler.pixels_per_mm();
                                Ok(Some([
                                    s.x as f64 + millimetres[0] * ppm,
                                    s.y as f64 - millimetres[1] * ppm,
                                ]))
                            },
                        )
                        .unwrap();
                        assert_eq!(
                            pass.decision(0).unwrap(),
                            ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped
                        );
                        c.set_prepared_coverage(
                            PreparedCoverage::new(
                                c.geometry_revision(),
                                c.coverage_view_revision(),
                                1,
                                vec![pass],
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    }
                    renderer.begin_frame();
                    renderer.reset_pan_offset();
                    renderer.add_instructions_with_symbols(
                        &mut c,
                        Some(&mut cache),
                        Some(profile),
                        None,
                    );
                    assert_eq!(renderer.requires_visibility_rebuild_for_navigation(), local);
                    assert_eq!(renderer.set_gpu_view_scaler(&c.scaler), !local);
                    let before=(renderer.get_pan_offset(),renderer.fast_view_scales());
                    if local {
                        assert!(!renderer.set_pan_offset(20.,-10.));
                        assert!(!renderer.set_gpu_zoom(4.,10.,20.));
                        assert_eq!(renderer.add_pan_offset(20.,-10.),None);
                    } else {
                        for scale in [0.,-1.,f32::NAN,f32::INFINITY,f32::from_bits(1)] {
                            assert!(!renderer.set_gpu_zoom(scale,10.,20.));
                        }
                        assert!(!renderer.set_gpu_zoom(f32::MAX,-f32::MAX,f32::MAX));
                        assert!(!renderer.set_pan_offset(f32::NAN,0.));
                        assert_eq!(renderer.add_pan_offset(f32::INFINITY,0.),None);
                        assert!(!renderer.set_gpu_zoom(1.,f32::INFINITY,0.));
                        assert_eq!((renderer.get_pan_offset(),renderer.fast_view_scales()),before);
                        assert!(renderer.set_pan_offset(f32::MAX,0.));
                        assert_eq!(renderer.add_pan_offset(f32::MAX,0.),None);
                        assert_eq!(renderer.get_pan_offset(),(f32::MAX,0.));
                        renderer.reset_pan_offset();
                        assert!(renderer.set_gpu_zoom(2.,10.,20.));
                        assert!(renderer.set_pan_offset(10.,-5.));
                        assert_eq!(renderer.add_pan_offset(5.,2.),Some((15.,-3.)));
                        renderer.reset_pan_offset();
                        assert!(renderer.set_gpu_view_scaler(&c.scaler));
                    }
                    assert_eq!((renderer.get_pan_offset(),renderer.fast_view_scales()),before);

                    let path = self.out.join(format!(
                        "{kind}-{zoom}-{}.png",
                        if local { "local" } else { "reference" }
                    ));
                    renderer.save_screenshot(&path).unwrap();
                    let actual = image::open(path).unwrap().to_rgba8();
                    let colored = actual.pixels().filter(|p| p.0 != [255; 4]).count();
                    assert!(
                        colored > 16,
                        "Empty physical reference {kind}/{zoom}/{local}"
                    );
                    if local {
                        let expected = reference.as_ref().unwrap();
                        let differences = actual
                            .pixels()
                            .zip(expected.pixels())
                            .filter(|(a, b)| a.0 != b.0)
                            .count();
                        assert_eq!(
                            differences, 0,
                            "Local displacement changed physical output {kind}/{zoom}"
                        );
                        rows.push(serde_json::json!({"kind":kind,"zoom":zoom,"different_pixels":differences,"colored_pixels":colored,"stale_gpu_affine_rejected":true,"low_level_affine_validation":true}));
                    } else {
                        reference = Some(actual);
                    }
                }
            }
            let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            c.set_bounds(GeoBounds::new(0., 0., 1., 1.));
            c.scaler.set_pixel_ratio(density);
            let anchor = WorldPoint::new(-2., 0.5);
            let projected = c.scaler.world_to_screen(anchor);
            let target = c.scaler.world_to_screen(WorldPoint::new(0.5, 0.5));
            let ppm = c.scaler.pixels_per_mm();
            let mm = [(target.x as f64 + 8. * ppm - projected.x as f64) / ppm, 3.];
            let mut inst = command(kind, anchor, [(mm[0] + 1.) as f32, 1.]);
            inst.set_portrayal_origin(PortrayalOrigin::augmented_local_point(anchor, mm).unwrap());
            c.add_instruction(inst);
            c.get_sorted_instructions();
            renderer.begin_frame();
            renderer.reset_pan_offset();
            renderer.add_instructions_with_symbols(&mut c, Some(&mut cache), Some(profile), None);
            let path = self.out.join(format!("{kind}-offscreen-anchor.png"));
            renderer.save_screenshot(&path).unwrap();
            let actual = image::open(path).unwrap().to_rgba8();
            let colored = actual.pixels().filter(|p| p.0 != [255; 4]).count();
            assert!(
                colored > 16,
                "Visible Local glyph discarded with offscreen feature anchor: {kind}"
            );
            rows.push(serde_json::json!({"kind":kind,"offscreen_feature_anchor":true,"colored_pixels":colored}));
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"checks":rows,"extent":extent,"density":density,"actual_pc":"S-101 ACHBRT07 and text","real_product_adapter_native_verified":false,"portrayal_crs_implemented":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut app = App {
        out: std::env::args()
            .nth(1)
            .map(PathBuf::from)
            .expect("output directory"),
        pc: std::env::args()
            .nth(2)
            .map(PathBuf::from)
            .expect("PC directory"),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
