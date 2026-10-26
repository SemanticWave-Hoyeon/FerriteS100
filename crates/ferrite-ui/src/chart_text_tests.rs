//! Chart text tests that need the egui text backend.
//!
//! They exercise `ferrite-gpu` internals through `EguiFonts`, the egui
//! implementation of `ChartFontContext`, so they live in this crate.
#![cfg(test)]

mod chart_fonts {
    use ferrite_gpu::chart_fonts::chart_font_definitions;
    use ferrite_gpu::chart_text_backend::epaint;

    #[test]
    fn independent_context_shaping_does_not_append_live_font_texture_deltas() {
        let live = egui::Context::default();
        live.set_fonts(chart_font_definitions());
        live.begin_pass(egui::RawInput::default());
        let _ = live.end_pass(); // Clear initial live atlas delta.
        let private = egui::Context::default();
        private.set_fonts(chart_font_definitions());
        private.begin_pass(egui::RawInput::default());
        let _ = private
            .layer_painter(egui::LayerId::background())
            .layout_no_wrap(
                "independent 12345".into(),
                epaint::FontId::proportional(16.),
                epaint::Color32::WHITE,
            );
        let output = private.end_pass();
        assert!(!output.textures_delta.set.is_empty());
        live.begin_pass(egui::RawInput::default());
        assert!(live.end_pass().textures_delta.set.is_empty());
    }
    #[test]
    fn warmed_and_fresh_font_owners_keep_layout_metrics_at_different_dpi_and_zoom() {
        fn context(density: f32, zoom: f32) -> egui::Context {
            let ctx = egui::Context::default();
            ctx.set_fonts(chart_font_definitions());
            ctx.set_zoom_factor(zoom);
            let mut input = egui::RawInput::default();
            input
                .viewports
                .entry(ctx.viewport_id())
                .or_default()
                .native_pixels_per_point = Some(density);
            ctx.begin_pass(input);
            ctx
        }
        for (density, zoom) in [(1., 1.), (2., 1.), (2., 1.25)] {
            let live = context(density, zoom);
            let private = context(density, zoom);
            // Warm only the existing owner, changing glyph atlas allocation order.
            let _ = live
                .layer_painter(egui::LayerId::background())
                .layout_no_wrap(
                    "Menus ABCDEFGHIJKLMNOPQRSTUVWXYZ 0123456789".into(),
                    epaint::FontId::proportional(13.),
                    epaint::Color32::WHITE,
                );
            for bold in [false, true] {
                for italic in [false, true] {
                    let mut job = epaint::text::LayoutJob::single_section(
                        "Depth 12.3 m / 수심 / 水深".into(),
                        epaint::text::TextFormat {
                            font_id: epaint::FontId {
                                size: 16. / live.pixels_per_point(),
                                family: if bold {
                                    epaint::FontFamily::Name("ChartBold".into())
                                } else {
                                    epaint::FontFamily::Proportional
                                },
                            },
                            color: epaint::Color32::WHITE,
                            italics: italic,
                            ..Default::default()
                        },
                    );
                    job.wrap = epaint::text::TextWrapping {
                        max_rows: 1,
                        break_anywhere: false,
                        ..Default::default()
                    };
                    let a = live
                        .layer_painter(egui::LayerId::background())
                        .layout_job(job.clone());
                    let b = private
                        .layer_painter(egui::LayerId::background())
                        .layout_job(job);
                    assert_eq!(a.rect, b.rect);
                    assert_eq!(a.mesh_bounds, b.mesh_bounds);
                    assert_eq!(a.rows.len(), b.rows.len());
                    for (x, y) in a.rows.iter().zip(&b.rows) {
                        assert_eq!(x.rect, y.rect);
                        assert_eq!(x.glyphs.len(), y.glyphs.len());
                        for (u, v) in x.glyphs.iter().zip(&y.glyphs) {
                            assert_eq!(
                                (u.chr, u.pos, u.advance_width, u.font_height),
                                (v.chr, v.pos, v.advance_width, v.font_height)
                            );
                        }
                    }
                }
            }
            let _ = live.end_pass();
            let _ = private.end_pass();
        }
    }
}
