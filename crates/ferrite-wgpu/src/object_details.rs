//! Readable, collapsible pick-report presentation. Domain values stay unchanged.
use crate::{ui_chrome::Theme, SelectedFeature};

/// A fixed number of UI preferences shared across selected objects. No per-feature
/// expansion map grows as the user browses a large exchange set.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDetailSections {
    pub definition: bool,
    pub source: bool,
    pub catalogues: bool,
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
            catalogues: false,
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
            catalogues: open,
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
/// Keep full original values selectable/copyable, independent of display layout.
fn copy_menu(response: egui::Response, name: &str, text: &str, original: Option<&str>) {
    response.context_menu(|ui| {
        if ui.button("Copy value").clicked() {
            ui.ctx().copy_text(text.to_owned());
            ui.close_menu();
        }
        if ui.button("Copy field").clicked() {
            ui.ctx().copy_text(
                original
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{name}: {text}")),
            );
            ui.close_menu();
        }
    });
}

/// Narrow inspectors stack labels above values; wider inspectors reserve most
/// of the row for values. This never rewrites chart attributes or source text.
fn field_widths(width: f32) -> Option<(f32, f32)> {
    if width < 340. {
        return None;
    }
    let name = (width * 0.30).min(120.);
    Some((name, (width - name - 10.).max(1.)))
}
fn field(ui: &mut egui::Ui, name: &str, text: &str, original: Option<&str>) {
    let name_text = egui::RichText::new(name)
        .small()
        .strong()
        .color(Theme::current(ui.ctx()).muted);
    let width = ui.available_width();
    if name.is_empty() {
        copy_menu(
            ui.add(egui::Label::new(text).wrap().selectable(true)),
            name,
            text,
            original,
        );
    } else if let Some((name_width, value_width)) = field_widths(width) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 10.;
            ui.allocate_ui_with_layout(
                egui::vec2(name_width, 0.),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_max_width(name_width);
                    ui.add(egui::Label::new(name_text).wrap().selectable(true));
                },
            );
            ui.allocate_ui_with_layout(
                egui::vec2(value_width, 0.),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_max_width(value_width);
                    copy_menu(
                        ui.add(egui::Label::new(text).wrap().selectable(true)),
                        name,
                        text,
                        original,
                    );
                },
            );
        });
    } else {
        ui.add(egui::Label::new(name_text).wrap().selectable(true));
        copy_menu(
            ui.add(egui::Label::new(text).wrap().selectable(true)),
            name,
            text,
            original,
        );
    }
    ui.add_space(4.);
}
pub(crate) fn row(ui: &mut egui::Ui, name: &str, text: &str) {
    ui.push_id(("detail-row", name), |ui| field(ui, name, text, None));
}

/// Preserve each original report line for copying. Only the display separates
/// its first colon into label/value; colons inside the value remain unchanged.
pub(crate) fn draw_report_fields(ui: &mut egui::Ui, text: &str) {
    ui.push_id(text, |ui| {
        for (index, line) in text.lines().enumerate() {
            let (name, text) = line.split_once(':').unwrap_or(("", line));
            ui.push_id(index, |ui| field(ui, name.trim(), text.trim(), Some(line)));
        }
    });
}

fn copied_attributes(feature: &SelectedFeature) -> String {
    feature
        .attributes
        .iter()
        .map(|(name, text)| format!("{name}: {text}"))
        .collect::<Vec<_>>()
        .join("\n")
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
    if let Some((_, name)) = feature.attributes.iter().find(|(key, _)| {
        key.eq_ignore_ascii_case("name") || key.eq_ignore_ascii_case("featureName")
    }) {
        value(ui, name);
    }
    ui.small(format!("Object ID {}", feature.feature_id));
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
            ui.add(
                egui::TextEdit::singleline(query)
                    .hint_text("Find a name or value")
                    .desired_width(ui.available_width()),
            );
            ui.horizontal_wrapped(|ui| {
                if ui
                    .small_button("Copy all")
                    .on_hover_text("Copy all original attributes, including filtered values")
                    .clicked()
                {
                    ui.ctx().copy_text(copied_attributes(feature));
                }
                if !query.is_empty() && ui.small_button("Clear filter").clicked() {
                    query.clear();
                }
            });
            ui.add_space(4.);
            let needle = query.trim().to_lowercase();
            let mut matches = 0;
            for (index, (key, text)) in feature.attributes.iter().enumerate() {
                if !needle.is_empty()
                    && !key.to_lowercase().contains(&needle)
                    && !text.to_lowercase().contains(&needle)
                {
                    continue;
                }
                ui.push_id(("object_attribute", index), |ui| field(ui, key, text, None));
                ui.separator();
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
    fn responsive_fields_preserve_full_values_and_clip_to_inspector_width() {
        for width in [220., 280., 360., 520.] {
            let ctx = egui::Context::default();
            let long_value = format!(
                "https://example.test/{}: original suffix",
                "LONG_SOURCE_".repeat(32)
            );
            let out = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 2400.),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        row(ui, "Original source path and identifier", &long_value);
                    });
                },
            );
            let mut strings = Vec::new();
            for shape in &out.shapes {
                texts(&shape.shape, &mut strings);
                if let egui::epaint::Shape::Text(text) = &shape.shape {
                    let right = text.pos.x + text.galley.size().x;
                    assert!(right <= width + 1., "width={width}, right={right}");
                }
            }
            assert!(strings.iter().any(|s| s == &long_value));
        }
    }
    #[test]
    fn copy_all_retains_original_whitespace_unicode_and_value_colons() {
        let mut f = fixture();
        f.attributes = vec![
            ("name".into(), "  항로 : 原文  ".into()),
            ("link".into(), "https://example.test/a:b".into()),
        ];
        assert_eq!(
            copied_attributes(&f),
            "name:   항로 : 原文  \nlink: https://example.test/a:b"
        );
        assert_eq!(f.attributes[0].1, "  항로 : 原文  ");
    }
    #[test]
    fn report_colons_stay_in_values_and_unlabelled_lines_remain_readable() {
        let ctx = egui::Context::default();
        let out = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(240., 1200.),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    draw_report_fields(
                        ui,
                        "Source: https://example.test/a:b\nOriginal report without a colon",
                    );
                });
            },
        );
        let mut rendered = Vec::new();
        for shape in &out.shapes {
            texts(&shape.shape, &mut rendered);
        }
        assert!(rendered.iter().any(|s| s == "https://example.test/a:b"));
        assert!(rendered
            .iter()
            .any(|s| s == "Original report without a colon"));
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
