//! Readable, collapsible pick-report presentation. Domain values stay unchanged.
use crate::{ui_chrome::Theme, SelectedFeature};

/// A fixed number of UI preferences shared across selected objects. No per-feature
/// expansion map grows as the user browses a large exchange set.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDetailSections {
    pub definition: bool,
    pub source: bool,
    pub position: bool,
    pub attributes: bool,
    pub portrayal: bool,
    pub coverage: bool,
    pub nearby: bool,
    pub security: bool,
}
impl Default for ObjectDetailSections {
    fn default() -> Self {
        Self {
            definition: false,
            source: false,
            position: true,
            attributes: true,
            portrayal: false,
            coverage: false,
            nearby: true,
            security: false,
        }
    }
}
impl ObjectDetailSections {
    pub fn set_all(&mut self, open: bool) {
        *self = Self {
            definition: open,
            source: open,
            position: open,
            attributes: open,
            portrayal: open,
            coverage: open,
            nearby: open,
            security: open,
        };
    }
}

pub(crate) fn detail_section<R>(
    ui: &mut egui::Ui,
    title: impl Into<egui::WidgetText>,
    key: &'static str,
    open: &mut bool,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::CollapsingResponse<R> {
    let response = egui::CollapsingHeader::new(title)
        .id_salt(("object_detail_section", key))
        .open(Some(*open))
        .show(ui, body);
    if response.header_response.clicked() {
        *open = !*open;
    }
    response
}
fn value(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text).wrap().selectable(true));
}
fn row(ui: &mut egui::Ui, name: &str, text: &str) {
    ui.label(
        egui::RichText::new(name)
            .small()
            .color(Theme::current(ui.ctx()).muted),
    );
    value(ui, text);
    ui.add_space(3.);
}

/// The same body is used by the application and native UI regression fixtures.
pub fn draw_selected_object_details(
    ui: &mut egui::Ui,
    feature: &SelectedFeature,
    sections: &mut ObjectDetailSections,
    query: &mut String,
    safety_contour: f64,
) {
    let theme = Theme::current(ui.ctx());
    value(
        ui,
        egui::RichText::new(&feature.feature_type)
            .size(17.)
            .strong()
            .color(theme.accent),
    );
    ui.horizontal_wrapped(|ui| {
        ui.weak(&feature.primitive_type);
        if let Some(source) = &feature.source {
            let name = std::path::Path::new(source)
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_else(|| source.as_str().into());
            ui.weak(name).on_hover_text(source);
        }
    });
    if feature.symbol_name.as_deref() == Some("ISODGR01") {
        // Keep the danger meaning visible even with all supplementary details closed.
        value(ui, "Isolated underwater danger (IHO ISODGR01).");
    }
    ui.add_space(5.);
    detail_section(
        ui,
        "Position · WGS84",
        "position",
        &mut sections.position,
        |ui| {
            let (lon, lat) = feature.world_pos;
            row(
                ui,
                "Latitude",
                &format!(
                    "{}  ({lat:.8}°)",
                    super::egui_integration::format_dms(lat, true)
                ),
            );
            row(
                ui,
                "Longitude",
                &format!(
                    "{}  ({lon:.8}°)",
                    super::egui_integration::format_dms(lon, false)
                ),
            );
            if feature.longitude_shift != 0. {
                value(ui,format!("Display longitude copy: {:+.0}°; coordinates above retain the source longitude.",feature.longitude_shift));
            }
        },
    );
    detail_section(
        ui,
        format!("Attributes ({})", feature.attributes.len()),
        "attributes",
        &mut sections.attributes,
        |ui| {
            if feature.attributes.is_empty() {
                ui.weak("No public pick-report attributes.");
                return;
            }
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 48.).max(60.);
                ui.add(
                    egui::TextEdit::singleline(query)
                        .hint_text("Filter attributes")
                        .desired_width(width),
                );
                if !query.is_empty() && ui.small_button("Clear").clicked() {
                    query.clear();
                }
            });
            let needle = query.trim().to_lowercase();
            let mut matches = 0;
            for (key, text) in &feature.attributes {
                if !needle.is_empty()
                    && !key.to_lowercase().contains(&needle)
                    && !text.to_lowercase().contains(&needle)
                {
                    continue;
                }
                if matches > 0 {
                    ui.separator();
                }
                row(ui, key, text);
                matches += 1;
            }
            if !needle.is_empty() {
                ui.weak(format!(
                    "{matches} of {} attributes",
                    feature.attributes.len()
                ));
            }
            if matches == 0 {
                ui.weak("No matching attributes. Clear the filter to see all values.");
            }
        },
    );
    if let Some(definition) = &feature.definition {
        detail_section(
            ui,
            "Definition",
            "definition",
            &mut sections.definition,
            |ui| {
                value(ui, definition);
            },
        );
    }
    detail_section(
        ui,
        "Identity and source",
        "source",
        &mut sections.source,
        |ui| {
            let identity = if feature.cell_index.is_some() {
                format!(
                    "{} : {}",
                    feature.feature_id >> 32,
                    feature.feature_id & 0xffff_ffff
                )
            } else {
                feature.feature_id.to_string()
            };
            row(ui, "Cell record ID", &identity);
            if let Some(foid) = &feature.foid {
                row(ui, "Feature object identifier (FOID)", foid);
            }
            if let Some(source) = &feature.source {
                row(ui, "Source chart", source);
                if ui.small_button("Copy source").clicked() {
                    ui.ctx().copy_text(source.clone());
                }
            }
        },
    );
    if let Some(symbol) = &feature.symbol_name {
        detail_section(
            ui,
            "Portrayal and safety",
            "portrayal",
            &mut sections.portrayal,
            |ui| {
                row(ui, "Catalogue symbol", symbol);
                if symbol == "ISODGR01" {
                    value(ui,format!("The portrayal uses a safety contour of {safety_contour:.1} m and the object's depth / surrounding depth."));
                    value(
                        ui,
                        "Shallow Water Dangers controls the additional dangers in shallow water.",
                    );
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> SelectedFeature {
        SelectedFeature {
            feature_type: "Wreck".into(),
            feature_id: 1,
            foid: Some("FOID_SENTINEL".into()),
            cell_index: Some(0),
            primitive_type: "Point".into(),
            source: Some("/charts/SOURCE_SENTINEL.000".into()),
            attributes: vec![
                ("depth".into(), "12.34 m ATTRIBUTE_SENTINEL".into()),
                ("status".into(), "Unsurveyed STATUS_SENTINEL".into()),
            ],
            world_pos: (-1.1, 50.8),
            longitude_shift: 0.,
            definition: Some("DEFINITION_SENTINEL".into()),
            symbol_name: Some("ISODGR01".into()),
        }
    }
    fn texts(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(t) => out.push(t.galley.job.text.clone()),
            egui::epaint::Shape::Vec(v) => {
                for x in v {
                    texts(x, out)
                }
            }
            _ => {}
        }
    }
    fn draw(
        ctx: &egui::Context,
        f: &SelectedFeature,
        s: &mut ObjectDetailSections,
        q: &mut String,
        t: f64,
    ) -> Vec<String> {
        let out = ctx.run(
            egui::RawInput {
                time: Some(t),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(380., 2000.),
                )),
                ..Default::default()
            },
            |ctx| {
                Theme::for_profile("Day").apply(ctx, true);
                egui::CentralPanel::default()
                    .show(ctx, |ui| draw_selected_object_details(ui, f, s, q, 30.));
            },
        );
        let mut v = Vec::new();
        for x in out.shapes {
            texts(&x.shape, &mut v)
        }
        v
    }
    #[test]
    fn collapse_expand_and_filter_preserve_real_values_and_danger_meaning() {
        let ctx = egui::Context::default();
        let f = fixture();
        let original = f.attributes.clone();
        let mut s = ObjectDetailSections::default();
        let mut q = String::new();
        s.set_all(false);
        let v = draw(&ctx, &f, &mut s, &mut q, 0.);
        assert!(v.iter().any(|t| t == "Wreck"));
        assert!(v.iter().any(|t| t.contains("Isolated underwater danger")));
        assert!(!v.iter().any(|t| t.contains("ATTRIBUTE_SENTINEL")));
        s.set_all(true);
        let _ = draw(&ctx, &f, &mut s, &mut q, 1.);
        let v = draw(&ctx, &f, &mut s, &mut q, 2.);
        for marker in ["ATTRIBUTE_SENTINEL", "DEFINITION_SENTINEL", "FOID_SENTINEL"] {
            assert!(v.iter().any(|t| t.contains(marker)), "{marker}: {v:?}");
        }
        q = "uNsUrVeYeD".into();
        let v = draw(&ctx, &f, &mut s, &mut q, 3.);
        assert!(v.iter().any(|t| t.contains("STATUS_SENTINEL")));
        assert!(!v.iter().any(|t| t.contains("ATTRIBUTE_SENTINEL")));
        assert!(v.iter().any(|t| t == "1 of 2 attributes"));
        assert_eq!(f.attributes, original);
    }
    #[test]
    fn pointer_click_toggles_only_the_requested_section() {
        let ctx = egui::Context::default();
        let mut open = false;
        let mut center = egui::Pos2::ZERO;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(380., 300.),
                )),
                ..Default::default()
            },
            |ctx| {
                Theme::for_profile("Day").apply(ctx, true);
                egui::CentralPanel::default().show(ctx, |ui| {
                    center = detail_section(ui, "Attributes", "attributes", &mut open, |ui| {
                        ui.label("VISIBLE_BODY")
                    })
                    .header_response
                    .rect
                    .center();
                });
            },
        );
        for expected in [true, false] {
            let out = ctx.run(
                egui::RawInput {
                    events: vec![
                        egui::Event::PointerMoved(center),
                        egui::Event::PointerButton {
                            pos: center,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos: center,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        detail_section(ui, "Attributes", "attributes", &mut open, |ui| {
                            ui.label("VISIBLE_BODY")
                        });
                    });
                },
            );
            assert_eq!(open, expected);
            drop(out);
        }
    }
}
