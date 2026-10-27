//! Windows of the GML overlay products shown over the ENC: the S-124
//! navigational warning list (S-124 2.0.0 clause 11.1.1) and the S-129
//! under keel clearance plan panel, registered in the panel registry
//! ([`crate::panels`]) by the UI features `s124` and `s129`. The product
//! modules define the panels' data models (`ferrite_s124::panel`,
//! `ferrite_s129::panel`) and the host publishes them from the registered
//! products; this module only draws the rows it is given and records
//! requests, which the host consumes on the next frame.
use crate::panels::PanelRegistry;
#[cfg(any(feature = "s124", feature = "s129"))]
use crate::panels::{ProductPanel, SharedPanelState};
#[cfg(feature = "s124")]
use crate::ui_chrome::Theme;
#[cfg(any(feature = "s124", feature = "s129"))]
use ferrite_product_api::PanelDescriptor;
#[cfg(any(feature = "s124", feature = "s129"))]
use std::any::Any;

// The panels' data models belong to the product modules.
#[cfg(feature = "s124")]
pub use ferrite_s124::panel::{NavwarnClassification, NavwarnPartRow, NavwarnRow, NavwarnSort};
#[cfg(feature = "s129")]
pub use ferrite_s129::panel::{ControlPointRow, UkcPlanRow};

/// The overlay windows' state: the product panel registry.
pub type State = PanelRegistry;

/// The S-124 warning list panel (`S-124/warnings`).
#[cfg(feature = "s124")]
#[derive(Debug, Clone, Default)]
pub struct NavwarnPanel {
    pub open: bool,
    pub messages: Vec<NavwarnRow>,
    /// In-force bulletin missing-message indications (9.2.2).
    pub missing: Vec<String>,
    pub sort: NavwarnSort,
    pub new_count: usize,
    /// Show S-124 parts before their fixedDateRange starts.
    pub show_before_range: bool,
    /// Show messages classified as Information on the chart.
    pub show_information: bool,
    /// The OS user the classifications and indications belong to, and
    /// whether they are saved (11.1.1).
    pub user_status: String,
    /// Instant the lifecycle was evaluated at (viewing time or clock).
    pub lifecycle_time: String,
    // Requests from the window, consumed by the host.
    pub sort_changed: bool,
    pub classify: Option<(u32, NavwarnClassification)>,
    /// Select and zoom to a message (dataset) or one of its parts.
    pub select: Option<(u32, Option<String>)>,
}

/// Requests of the S-124 window, consumed by the host.
#[cfg(feature = "s124")]
#[derive(Debug, Clone, Default)]
pub struct NavwarnRequests {
    pub sort_changed: bool,
    pub classify: Option<(u32, NavwarnClassification)>,
    pub select: Option<(u32, Option<String>)>,
}

#[cfg(feature = "s124")]
impl NavwarnPanel {
    /// Mark every request consumed (host side).
    pub fn take_requests(&mut self) -> NavwarnRequests {
        NavwarnRequests {
            sort_changed: std::mem::take(&mut self.sort_changed),
            classify: self.classify.take(),
            select: self.select.take(),
        }
    }
}

#[cfg(feature = "s124")]
impl ProductPanel for NavwarnPanel {
    fn descriptor(&self) -> PanelDescriptor {
        ferrite_s124::panel::PANEL
    }
    fn is_open(&self) -> bool {
        self.open
    }
    fn set_open(&mut self, open: bool) {
        self.open = open;
    }
    fn menu_label(&self) -> String {
        if self.new_count > 0 {
            format!("S-124 Navigational Warnings ({} new)", self.new_count)
        } else {
            "S-124 Navigational Warnings".to_owned()
        }
    }
    fn draw(&mut self, ctx: &egui::Context, shared: &mut SharedPanelState) {
        draw_navwarn(ctx, self, shared);
    }
    fn clone_box(&self) -> Box<dyn ProductPanel> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The S-129 plan panel (`S-129/plans`).
#[cfg(feature = "s129")]
#[derive(Debug, Clone, Default)]
pub struct UkcPanel {
    pub open: bool,
    pub plans: Vec<UkcPlanRow>,
    /// S-129 context parameter PlainBoundaries.
    pub plain_boundaries: bool,
    /// Apply the S-129 feature attribute scaleMinimum.
    pub apply_scale_minimum: bool,
    /// Request from the window: zoom to a plan (dataset).
    pub select_plan: Option<u32>,
}

#[cfg(feature = "s129")]
impl ProductPanel for UkcPanel {
    fn descriptor(&self) -> PanelDescriptor {
        ferrite_s129::panel::PANEL
    }
    fn is_open(&self) -> bool {
        self.open
    }
    fn set_open(&mut self, open: bool) {
        self.open = open;
    }
    fn menu_label(&self) -> String {
        "S-129 Under Keel Clearance".to_owned()
    }
    fn draw(&mut self, ctx: &egui::Context, shared: &mut SharedPanelState) {
        draw_ukc(ctx, self, shared);
    }
    fn clone_box(&self) -> Box<dyn ProductPanel> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Draw every open panel of the registry.
pub fn draw(ctx: &egui::Context, state: &mut State) {
    state.draw(ctx);
}

#[cfg(feature = "s124")]
fn draw_navwarn(ctx: &egui::Context, state: &mut NavwarnPanel, shared: &mut SharedPanelState) {
    if !state.open {
        return;
    }
    let theme = Theme::current(ctx);
    let mut open = true;
    egui::Window::new("S-124 Navigational Warnings")
        .id(egui::Id::new("s124_navwarn_window"))
        .open(&mut open)
        .default_width(460.)
        .default_height(520.)
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Sort by");
                let before = state.sort;
                egui::ComboBox::from_id_salt("s124_sort")
                    .selected_text(state.sort.label())
                    .show_ui(ui, |ui| {
                        for order in NavwarnSort::ALL {
                            ui.selectable_value(&mut state.sort, order, order.label());
                        }
                    });
                if state.sort != before {
                    state.sort_changed = true;
                }
                if state.new_count > 0 {
                    ui.label(
                        egui::RichText::new(format!("{} new", state.new_count))
                            .strong()
                            .color(theme.accent),
                    );
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .checkbox(
                        &mut state.show_before_range,
                        "Show parts before their date range",
                    )
                    .changed()
                {
                    shared.options_changed = true;
                }
                if ui
                    .checkbox(&mut state.show_information, "Show Information on chart")
                    .changed()
                {
                    shared.options_changed = true;
                }
                errata_checkbox(ui, shared);
            });
            if !state.user_status.is_empty() || !state.lifecycle_time.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.weak(&state.user_status);
                    ui.weak(&state.lifecycle_time);
                });
            }
            for missing in &state.missing {
                ui.colored_label(egui::Color32::from_rgb(220, 120, 30), missing);
            }
            ui.separator();
            if state.messages.is_empty() {
                ui.weak("No S-124 datasets loaded");
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("s124_list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for row in &state.messages {
                        ui.push_id(row.dataset, |ui| {
                            draw_message(ui, row, &mut state.classify, &mut state.select, &theme);
                        });
                    }
                });
        });
    state.open = open;
}

#[cfg(feature = "s124")]
fn draw_message(
    ui: &mut egui::Ui,
    row: &NavwarnRow,
    classify: &mut Option<(u32, NavwarnClassification)>,
    select: &mut Option<(u32, Option<String>)>,
    theme: &Theme,
) {
    let mut title = egui::RichText::new(&row.caption);
    if row.cancelled {
        title = title.strikethrough().weak();
    } else if row.new {
        title = title.strong();
    }
    ui.horizontal(|ui| {
        if row.new {
            ui.label(
                egui::RichText::new("NEW")
                    .small()
                    .strong()
                    .color(theme.accent),
            );
        }
        let response = ui
            .add(egui::Button::new(title).truncate())
            .on_hover_text(format!(
                "{}\nReceived {}\nIssued {}\n{}",
                row.status, row.received, row.issued, row.source
            ));
        if response.clicked() {
            *select = Some((row.dataset, None));
        }
    });
    ui.horizontal_wrapped(|ui| {
        let mut status = row.status.clone();
        if row.imported {
            status.push_str(" · imported, not for navigation");
        }
        if !row.portrayed && !row.cancelled {
            status.push_str(" · not on chart");
        }
        ui.small(status);
        if !row.warning_type.is_empty() {
            ui.small(format!("· {}", row.warning_type));
        }
        if !row.producer_series.is_empty() {
            ui.small(format!("· {}", row.producer_series));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.small(format!("Received {} · Issued {}", row.received, row.issued));
        if !row.general_area.is_empty() {
            ui.small(format!("· {}", row.general_area));
        }
    });
    if !row.cancelled {
        ui.horizontal(|ui| {
            for c in NavwarnClassification::ALL {
                if ui
                    .selectable_label(row.classification == c, c.label())
                    .clicked()
                    && row.classification != c
                {
                    *classify = Some((row.dataset, c));
                }
            }
        });
    }
    for part in &row.parts {
        ui.horizontal(|ui| {
            ui.add_space(12.);
            let text = if part.summary.is_empty() {
                part.id.clone()
            } else {
                part.summary.clone()
            };
            let response = ui
                .add_enabled(
                    part.has_geometry,
                    egui::Button::new(egui::RichText::new(text).small()).truncate(),
                )
                .on_hover_text(format!("{} · {}", part.id, part.timing));
            if response.clicked() {
                *select = Some((row.dataset, Some(part.id.clone())));
            }
        });
    }
    ui.separator();
}

/// The shared "Correct known PC errata" option.
#[cfg(any(feature = "s124", feature = "s129"))]
fn errata_checkbox(ui: &mut egui::Ui, shared: &mut SharedPanelState) {
    if ui
        .checkbox(&mut shared.correct_pc_errata, "Correct known PC errata")
        .on_hover_text(
            "Off (default): the IHO portrayal catalogues are applied literally (S-100 Part 9).\n\
             On: S-129 information box text with transparency 1 is drawn opaque, and S-124 \
             instructions in viewing groups the catalogue does not declare are drawn.",
        )
        .changed()
    {
        shared.options_changed = true;
    }
}

#[cfg(feature = "s129")]
fn draw_ukc(ctx: &egui::Context, state: &mut UkcPanel, shared: &mut SharedPanelState) {
    if !state.open {
        return;
    }
    let mut open = true;
    egui::Window::new("S-129 Under Keel Clearance")
        .id(egui::Id::new("s129_ukc_window"))
        .open(&mut open)
        .default_width(440.)
        .default_height(480.)
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .checkbox(&mut state.plain_boundaries, "Plain boundaries")
                    .on_hover_text("S-129 context parameter PlainBoundaries")
                    .changed()
                {
                    shared.options_changed = true;
                }
                if ui
                    .checkbox(&mut state.apply_scale_minimum, "Apply scaleMinimum")
                    .on_hover_text(
                        "Hide features below their dataset scaleMinimum (S-129 dataset loading rule)",
                    )
                    .changed()
                {
                    shared.options_changed = true;
                }
                errata_checkbox(ui, shared);
            });
            ui.separator();
            if state.plans.is_empty() {
                ui.weak("No S-129 datasets loaded");
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("s129_plans")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for plan in &state.plans {
                        ui.push_id(plan.dataset, |ui| {
                            if ui
                                .add(egui::Button::new(egui::RichText::new(&plan.name).strong()).truncate())
                                .on_hover_text("Show the plan on the chart")
                                .clicked()
                            {
                                state.select_plan = Some(plan.dataset);
                            }
                            egui::Grid::new("plan").num_columns(2).striped(true).show(ui, |ui| {
                                for (k, v) in [
                                    ("Vessel", &plan.vessel),
                                    ("Maximum draught", &plan.draught),
                                    ("Purpose", &plan.purpose),
                                    ("Generation time", &plan.generation_time),
                                    ("Validity", &plan.validity),
                                    ("Source route", &plan.route),
                                ] {
                                    if !v.is_empty() {
                                        ui.label(k);
                                        ui.label(v);
                                        ui.end_row();
                                    }
                                }
                            });
                            if plan.issues > 0 {
                                ui.small(format!("{} validation issues (see Logs)", plan.issues));
                            }
                            egui::CollapsingHeader::new(format!(
                                "Control points ({})",
                                plan.control_points.len()
                            ))
                            .id_salt("cps")
                            .default_open(true)
                            .show(ui, |ui| {
                                egui::Grid::new("cp_grid").num_columns(5).striped(true).show(ui, |ui| {
                                    for h in ["Point", "ETA", "Window", "Speed", "UKC margin"] {
                                        ui.strong(h);
                                    }
                                    ui.end_row();
                                    for cp in &plan.control_points {
                                        ui.label(&cp.name);
                                        ui.label(&cp.eta);
                                        ui.label(&cp.window);
                                        ui.label(&cp.speed);
                                        ui.label(&cp.margin);
                                        ui.end_row();
                                    }
                                });
                            });
                            ui.separator();
                        });
                    }
                });
        });
    state.open = open;
}

#[cfg(all(test, feature = "s124", feature = "s129"))]
mod tests {
    use super::*;

    #[test]
    fn windows_draw_rows_without_raising_requests() {
        let ctx = egui::Context::default();
        let mut state = State::default();
        *state.get_mut::<NavwarnPanel>().unwrap() = NavwarnPanel {
            open: true,
            messages: vec![
                NavwarnRow {
                    dataset: 1,
                    caption: "WZ 43/26 OUTER SPIT BUOY OFF STATION".into(),
                    status: "Active".into(),
                    new: true,
                    portrayed: true,
                    parts: vec![NavwarnPartRow {
                        id: "P1".into(),
                        summary: "Buoy off station".into(),
                        timing: "in force".into(),
                        has_geometry: true,
                    }],
                    ..Default::default()
                },
                NavwarnRow {
                    dataset: 2,
                    caption: "WZ 40/26".into(),
                    status: "Cancelled".into(),
                    cancelled: true,
                    ..Default::default()
                },
            ],
            missing: vec!["In-force bulletin lists warnings not held: WZ 41/26".into()],
            new_count: 1,
            ..Default::default()
        };
        *state.get_mut::<UkcPanel>().unwrap() = UkcPanel {
            open: true,
            plans: vec![UkcPlanRow {
                dataset: 3,
                name: "Torres Strait".into(),
                vessel: "9800738".into(),
                control_points: vec![ControlPointRow {
                    name: "CP01".into(),
                    eta: "2024-04-17T22:00:00Z".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            plain_boundaries: true,
            apply_scale_minimum: true,
            ..Default::default()
        };
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| draw(ctx, &mut state));
        }
        // The windows drawn (the route panel opens with its provider's button).
        assert!(state
            .iter()
            .filter(|panel| panel.in_menu())
            .all(|panel| panel.is_open()));
        let navwarn = state.get_mut::<NavwarnPanel>().unwrap().take_requests();
        assert!(navwarn.classify.is_none() && navwarn.select.is_none());
        assert!(!navwarn.sort_changed);
        assert!(state.get_mut::<UkcPanel>().unwrap().select_plan.is_none());
        assert!(!state.take_options_changed());
    }
}
