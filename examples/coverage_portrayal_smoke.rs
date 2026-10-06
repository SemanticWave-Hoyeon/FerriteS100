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
fn instruction(kind: &str) -> DrawingInstruction {
    let rect = || {
        AreaInstruction::new(vec![
            WorldPoint::new(0.05, 0.05),
            WorldPoint::new(0.95, 0.05),
            WorldPoint::new(0.95, 0.95),
            WorldPoint::new(0.05, 0.95),
        ])
    };
    match kind {
        "area" => DrawingInstruction::Area(rect().with_solid_fill(Color::RED)),
        "line" => DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0.05, 0.05),
            WorldPoint::new(0.95, 0.95),
        ])),
        "symbol" => DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0.5, 0.5)).with_scale(4.),
        ),
        "pattern" => DrawingInstruction::Area(rect().with_pattern_fill(
            "FOULAR01P".into(),
            (5., 0.),
            (2., 6.),
        )),
        "text" => DrawingInstruction::Text(
            TextInstruction::new("Coverage MASK".into(), WorldPoint::new(0.5, 0.5))
                .with_font_size(32.)
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
                    .with_title("Coverage portrayal verification")
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
        let profile = pc
            .color_profiles
            .profiles
            .get("Day")
            .or_else(|| {
                pc.color_profiles
                    .profiles
                    .values()
                    .find(|p| p.name.contains("Day"))
            })
            .unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let coverage = frame(extent);
        let mut results = Vec::new();
        for kind in ["area", "line", "symbol", "pattern", "text"] {
            let mut references = Vec::new();
            for policy in ["reference", "masked", "unclipped", "hidden"] {
                let mut context =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                context.set_bounds(GeoBounds::new(0., 0., 1., 1.));
                context.scaler.set_pixel_ratio(density);
                let mut command = instruction(kind);
                command.set_portrayal_origin(if policy == "unclipped" {
                    PortrayalOrigin::feature_point(WorldPoint::new(0.99, 0.5)).unwrap()
                } else if policy == "hidden" {
                    PortrayalOrigin::feature_point(WorldPoint::new(0.01, 0.5)).unwrap()
                } else {
                    PortrayalOrigin::NonPoint
                });
                context.add_instruction(command);
                context.get_sorted_instructions();
                if policy != "reference" {
                    let pass = PreparedCoveragePass::prepare(
                        context.raw_instructions(),
                        coverage.clone(),
                        |_| Ok(InstructionCoverageClass::Dataset(0)),
                        |_, source| {
                            Ok(Some(match source {
                                PointOriginGeometry::FeaturePoint(p) => {
                                    let p = context.scaler.world_to_screen(*p);
                                    [p.x as f64, p.y as f64]
                                }
                                _ => unreachable!(),
                            }))
                        },
                    )
                    .unwrap();
                    let binding = PreparedCoverage::new(
                        context.geometry_revision(),
                        context.coverage_view_revision(),
                        context.instruction_count(),
                        vec![pass],
                    )
                    .unwrap();
                    context.set_prepared_coverage(binding).unwrap();
                }
                renderer.begin_frame();
                renderer.reset_pan_offset();
                renderer.add_instructions_with_symbols(
                    &mut context,
                    Some(&mut cache),
                    Some(profile),
                    None,
                );
                for (index, (zoom, pan)) in
                    [(1., [0., 0.]), (1.4, [17., -9.])].into_iter().enumerate()
                {
                    let pivot = [size.width as f32 / 2., size.height as f32 / 2.];
                    renderer.set_gpu_zoom(zoom, pivot[0], pivot[1]);
                    renderer.set_pan_offset(pan[0], pan[1]);
                    let path = self.out.join(format!("{kind}-{policy}-{index}.png"));
                    renderer.save_screenshot(&path).unwrap();
                    let actual = image::open(path).unwrap().to_rgba8();
                    if policy == "reference" {
                        assert!(
                            actual.pixels().filter(|p| p.0 != [255; 4]).count() > 16,
                            "Empty reference: {kind}"
                        );
                        references.push(actual);
                        continue;
                    }
                    let mut cpu_checks = 0;
                    let mut differences = 0;
                    let mut clipped = 0;
                    let mut kept = 0;
                    for (x, y, pixel) in actual.enumerate_pixels() {
                        let point = [
                            (x as f32 + 0.5 - pivot[0]) / zoom + pivot[0] - pan[0],
                            (y as f32 + 0.5 - pivot[1]) / zoom + pivot[1] - pan[1],
                        ];
                        let visible = match policy {
                            "masked" => {
                                coverage.mask(0).unwrap().contains_pixel(
                                    point[0].floor().max(0.) as u32,
                                    point[1].floor().max(0.) as u32,
                                ) && point[0] >= 0.
                                    && point[1] >= 0.
                            }
                            "unclipped" => true,
                            "hidden" => false,
                            _ => unreachable!(),
                        };
                        if x % 19 == 0 && y % 17 == 0 {
                            assert_eq!(
                                renderer.coverage_fragment_visible(
                                    0,
                                    0,
                                    [x as f32 + 0.5, y as f32 + 0.5]
                                ),
                                visible,
                                "CPU/GPU coverage disagreement {kind}/{policy}/{index} at {x},{y}"
                            );
                            cpu_checks += 1;
                        }
                        let original = references[index].get_pixel(x, y).0;
                        let expected = if visible { original } else { [255; 4] };
                        if original != [255; 4] {
                            if visible {
                                kept += 1;
                            } else {
                                clipped += 1;
                            }
                        }
                        if pixel.0 != expected {
                            differences += 1;
                        }
                    }
                    assert_eq!(differences, 0, "{kind} / {policy} / affine {index}");
                    if policy == "masked" {
                        assert!(
                            clipped > 0 && kept > 0,
                            "Mask must remove and retain actual output: {kind}"
                        );
                    }
                    results.push(serde_json::json!({"kind":kind,"policy":policy,"affine":index,"different_pixels":differences,"cpu_fragment_checks":cpu_checks,"clipped_colored_pixels":clipped,"kept_colored_pixels":kept}));
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"checks":results,"real_data_application_binding_complete":false,"fixture":"synthetic one-pass all-five-primitives","extent":extent})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut app = App {
        pc: std::env::args()
            .nth(2)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("Catalogues/PC/S-101")),
        out: std::env::args()
            .nth(1)
            .map(PathBuf::from)
            .expect("output directory"),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
