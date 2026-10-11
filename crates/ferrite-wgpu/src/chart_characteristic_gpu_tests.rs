//! Text-positive layout/collision and ignored backend proof. Not full App/S-100 proof.
use super::*;
use ferrite_render::{TextFontProportion as P, TextFontStyle, TextFontWeight as W};

fn input(context: &egui::Context, extent: [u32; 2], density: f32) -> egui::RawInput {
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(extent[0] as f32 / density, extent[1] as f32 / density),
        )),
        max_texture_side: Some(2048),
        ..Default::default()
    };
    raw.viewports
        .entry(context.viewport_id())
        .or_default()
        .native_pixels_per_point = Some(density);
    raw
}
fn context(extent: [u32; 2], density: f32) -> egui::Context {
    let ctx = egui::Context::default();
    ctx.set_fonts(crate::chart_fonts::chart_font_definitions());
    ctx.begin_pass(input(&ctx, extent, density));
    ctx
}
fn label(source: usize, priority: i32, style: TextFontStyle, italic: bool) -> TextLabel {
    TextLabel {
        referenced_font: None,
        font_family_override: None,
        font_style: style,
        source: Some(source),
        plane: CompositionPlane::new(
            CompositionStage::Chart,
            std::num::NonZeroI32::new(1).unwrap(),
        ),
        priority,
        anchor: [16., 16.],
        rotation: 0.,
        screen_x: 16.,
        screen_y: 16.,
        text: "Map 12.3 AB".into(),
        font_size: 22.,
        color: [1.; 4],
        background: None,
        bold: false,
        italic,
        h_align: ferrite_render::HAlign::Left,
        v_align: ferrite_render::VAlign::Top,
    }
}
fn layout(
    ctx: &egui::Context,
    extent: [u32; 2],
    labels: &[TextLabel],
    capture: bool,
) -> (Vec<ChartTextShape>, Vec<bool>) {
    let fixed = FxHashSet::default();
    layout_chart_text_with_fonts(
        ChartTextLayoutEnvironment {
            physical_extent: [extent[0] as f32, extent[1] as f32],
            pan: [0.; 2],
            zoom: [1.; 2],
            pivot: [0.; 2],
            longitude_wrap: 0.,
            source_classification: None,
            device_fixed_sources: &fixed,
        },
        ctx,
        labels,
        Vec::new(),
        capture,
    )
}
fn cases() -> [(W, P, bool, bool); 8] {
    [
        (W::Light, P::Proportional, false, false),
        (W::Medium, P::Proportional, false, false),
        (W::Bold, P::Proportional, false, false),
        (W::Bold, P::MonoSpaced, false, false),
        (W::Light, P::MonoSpaced, false, true),
        (W::Medium, P::Proportional, true, false),
        (W::Medium, P::Proportional, false, true),
        (W::Bold, P::Proportional, true, true),
    ]
}
/// Independent expected family: no call to production matcher or family selector.
fn oracle_family(weight: W, proportion: P) -> egui::FontFamily {
    if proportion == P::MonoSpaced {
        egui::FontFamily::Monospace
    } else {
        match weight {
            W::Light => egui::FontFamily::Proportional,
            W::Medium => egui::FontFamily::Name("ChartMedium".into()),
            W::Bold => egui::FontFamily::Name("ChartBold".into()),
        }
    }
}
fn oracle_job(
    ctx: &egui::Context,
    weight: W,
    proportion: P,
    italic: bool,
) -> std::sync::Arc<egui::Galley> {
    ctx.layer_painter(egui::LayerId::background()).layout_job(
        egui::text::LayoutJob::single_section(
            "Map 12.3 AB".into(),
            egui::TextFormat {
                font_id: egui::FontId {
                    size: 22. / ctx.pixels_per_point(),
                    family: oracle_family(weight, proportion),
                },
                color: egui::Color32::WHITE,
                italics: italic,
                ..Default::default()
            },
        ),
    )
}
#[test]
fn text_positive_characteristics_keep_slant_and_original_priority_collision() {
    for (extent, density) in [([256, 128], 1.), ([384, 160], 2.)] {
        for (weight, proportion, serifs, italic) in cases() {
            let ctx = context(extent, density);
            let style = TextFontStyle {
                weight,
                proportion,
                serifs,
                ..Default::default()
            };
            let labels = [
                label(0, 1, style.clone(), italic),
                label(1, 2, style, italic),
            ];
            assert_eq!(layout(&ctx, extent, &labels, false).1, [false, true]);
            let shapes = layout(&ctx, extent, &labels, true).0;
            assert_eq!(shapes.len(), 1);
            assert_eq!((shapes[0].1, shapes[0].2), (2, Some(1)));
            let egui::Shape::Text(text) = &shapes[0].4.shape else {
                panic!("actual text shape required")
            };
            let oracle = oracle_job(&ctx, weight, proportion, italic);
            assert_eq!(text.galley.rect, oracle.rect);
            assert_eq!(text.galley.mesh_bounds, oracle.mesh_bounds);
            assert_eq!(
                text.galley.job.sections[0].format.font_id.family,
                oracle_family(weight, proportion)
            );
            assert_eq!(text.galley.job.sections[0].format.italics, italic);
            assert!(text.galley.rows.iter().any(|row| !row.glyphs.is_empty()));
            let _ = ctx.end_pass();
        }
    }
}
#[test]
fn medium_is_text_positive_distinct_from_light_and_explicit_override_never_shears() {
    let ctx = context([256, 128], 1.);
    let light = oracle_job(&ctx, W::Light, P::Proportional, false);
    let medium = oracle_job(&ctx, W::Medium, P::Proportional, false);
    assert_ne!(light.rect, medium.rect);
    let mut explicit = label(
        0,
        1,
        TextFontStyle {
            weight: W::Light,
            serifs: true,
            proportion: P::MonoSpaced,
            ..Default::default()
        },
        true,
    );
    // Branch-isolation fixture, not a fabricated captured PC proof. Original
    // same-ID/different-PC test separately exercises real capture and resolution.
    explicit.font_family_override = Some(egui::FontFamily::Name("ChartBold".into()));
    let shapes = layout(&ctx, [256, 128], &[explicit], true).0;
    let egui::Shape::Text(text) = &shapes[0].4.shape else {
        panic!("actual text shape required")
    };
    assert_eq!(
        text.galley.job.sections[0].format.font_id.family,
        egui::FontFamily::Name("ChartBold".into())
    );
    assert!(!text.galley.job.sections[0].format.italics);
    let _ = ctx.end_pass();
}

#[test]
#[ignore = "Root-only windowless actual chart shader; external process deadline required; not full App/S100 proof"]
fn characteristic_fonts_actual_layout_collision_chart_gpu() {
    use crate::referenced_chart_owner::gpu_tests::draw_chart_fixture;
    use sha2::{Digest, Sha256};
    pollster::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("No GPU adapter: proof not performed");
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("characteristic-font-proof"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut results = Vec::new();
        for (extent, density) in [([256, 128], 1.), ([384, 160], 2.)] {
            for (weight, proportion, serifs, italic) in cases() {
                let ctx = context(extent, density);
                let mut atlas = egui_wgpu::Renderer::new(
                    &device,
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    1,
                    false,
                );
                let style = TextFontStyle {
                    weight,
                    proportion,
                    serifs,
                    ..Default::default()
                };
                let labels = [
                    label(0, 1, style.clone(), italic),
                    label(1, 2, style, italic),
                ];
                assert_eq!(layout(&ctx, extent, &labels, false).1, [false, true]);
                let mut repeats = Vec::new();
                for repetition in 0..2 {
                    if repetition != 0 {
                        ctx.begin_pass(input(&ctx, extent, density));
                    }
                    let shapes = layout(&ctx, extent, &labels, true)
                        .0
                        .into_iter()
                        .map(|s| s.4)
                        .collect();
                    let output = ctx.end_pass();
                    let bytes: usize = output
                        .textures_delta
                        .set
                        .iter()
                        .map(|(_, d)| d.image.size()[0] * d.image.size()[1] * 4)
                        .sum();
                    assert!(
                        bytes <= 16 * 1024 * 1024,
                        "fixture atlas upload payload budget"
                    );
                    assert!(output.textures_delta.free.is_empty());
                    for (id, delta) in &output.textures_delta.set {
                        atlas.update_texture(&device, &queue, *id, delta);
                    }
                    let draw = draw_chart_fixture(
                        &device,
                        &queue,
                        (&ctx, density, extent),
                        shapes,
                        |ids| {
                            ids.iter()
                                .map(|id| {
                                    atlas.texture(id).expect("owned atlas").bind_group.clone()
                                })
                                .collect()
                        },
                    );
                    repeats.push(draw);
                }
                assert_eq!(repeats[0].bytes, repeats[1].bytes);
                assert_eq!(
                    (repeats[0].vertices, repeats[0].indices),
                    (repeats[1].vertices, repeats[1].indices)
                );
                // Independently construct expected galley from audited exact family,
                // original requested synthetic slant, same physical size and anchor.
                let oracle_ctx = context(extent, density);
                let galley = oracle_job(&oracle_ctx, weight, proportion, italic);
                let shape = egui::epaint::ClippedShape {
                    clip_rect: egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(extent[0] as f32 / density, extent[1] as f32 / density),
                    ),
                    shape: egui::Shape::galley(
                        egui::pos2(16. / density, 16. / density),
                        galley,
                        egui::Color32::WHITE,
                    ),
                };
                let output = oracle_ctx.end_pass();
                let mut oracle_atlas = egui_wgpu::Renderer::new(
                    &device,
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    1,
                    false,
                );
                for (id, delta) in &output.textures_delta.set {
                    oracle_atlas.update_texture(&device, &queue, *id, delta);
                }
                let oracle = draw_chart_fixture(
                    &device,
                    &queue,
                    (&oracle_ctx, density, extent),
                    vec![shape],
                    |ids| {
                        ids.iter()
                            .map(|id| {
                                oracle_atlas
                                    .texture(id)
                                    .expect("oracle atlas")
                                    .bind_group
                                    .clone()
                            })
                            .collect()
                    },
                );
                assert_eq!(
                    repeats[0].bytes, oracle.bytes,
                    "actual layout/priority/glyph GPU vs independently specified face"
                );
                results.push((
                    extent,
                    weight,
                    proportion,
                    serifs,
                    italic,
                    Sha256::digest(&repeats[0].bytes).to_vec(),
                ));
            }
        }
        for offset in [0, 8] {
            assert_ne!(
                results[offset].5,
                results[offset + 1].5,
                "Light and Medium actual GPU output must differ"
            );
        }
        assert!(device.pop_error_scope().await.is_none());
        println!("characteristic font backend {:?} {}: {} text-positive cases; repeat and independent oracle exact",info.backend,info.name,results.len());
    });
}
