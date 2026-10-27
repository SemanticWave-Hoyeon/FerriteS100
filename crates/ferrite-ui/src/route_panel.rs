//! The S-421 route panel ([`RoutePanel`]): the registered panel that draws
//! the route data model of the active route provider
//! ([`ferrite_s421::panel`]; the native route editor in a window, the
//! external route planner plugin as a left side panel) and records the
//! provider requests (plugin UI events) the host forwards.
use crate::panels::{ProductPanel, SharedPanelState};
use crate::ui_chrome::Theme;
use ferrite_product_api::PanelDescriptor;
use ferrite_s421::panel::{RoutePanelData, NATIVE_PROVIDER, PLUGIN_PROVIDER, PROVIDERS};
use std::any::Any;
use std::sync::Arc;

/// The route panel's state: the providers' published data, the active
/// provider, inline name editing and the requests of the last frames.
#[derive(Debug, Clone, Default)]
pub struct RoutePanel {
    /// The provider whose toolbar button is active (host, every frame).
    active: Option<&'static str>,
    /// The provider the panel showed last.
    shown: Option<&'static str>,
    /// Parsed immutable native data; updated only with provider data
    /// publication.
    native: Option<Arc<RoutePanelData>>,
    /// Parsed data of the route planner plugin (same publication).
    plugin: Option<Arc<RoutePanelData>>,
    /// Route being edited (route_id, current_edit_text)
    route_editing: Option<(u32, String)>,
    /// Waypoint being edited (waypoint_id, current_edit_text)
    waypoint_editing: Option<(u32, String)>,
    /// Requests to the providers (provider id, event JSON), in order.
    events: Vec<(String, String)>,
    /// The native window was closed: toggle its provider off.
    toggle: Option<String>,
}

impl RoutePanel {
    /// Read the providers' published plugin UI data (plugin id, JSON).
    pub fn publish(&mut self, data: &[(String, String)]) {
        let parse = |provider: &str| {
            data.iter()
                .find(|(id, _)| id == provider)
                .and_then(|(_, json)| RoutePanelData::parse(json))
                .map(Arc::new)
        };
        self.native = parse(NATIVE_PROVIDER);
        self.plugin = parse(PLUGIN_PROVIDER);
    }

    /// The native editor's published data.
    pub fn native(&self) -> Option<&Arc<RoutePanelData>> {
        self.native.as_ref()
    }

    /// Set the active provider from the active plugin buttons (ids).
    pub fn set_active_providers(&mut self, mut active: impl FnMut(&str) -> bool) {
        self.active = PROVIDERS.into_iter().find(|id| active(id));
    }

    /// Take the requests recorded since the last call.
    pub fn take_events(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.events)
    }

    /// Take the provider to toggle off (its window was closed).
    pub fn take_toggle(&mut self) -> Option<String> {
        self.toggle.take()
    }

    fn draw_panel(&mut self, ctx: &egui::Context) {
        let theme = Theme::current(ctx);
        let Some(provider) = self.active else {
            return;
        };
        if self.shown != Some(provider) {
            self.route_editing = None;
            self.waypoint_editing = None;
            self.shown = Some(provider);
        }
        let route_data = if provider == NATIVE_PROVIDER {
            self.native.clone()
        } else {
            self.plugin.clone()
        };
        let native = provider == NATIVE_PROVIDER;
        let events = &mut self.events;
        let route_editing = &mut self.route_editing;
        let waypoint_editing = &mut self.waypoint_editing;
        let mut push = |event: String| events.push((provider.to_string(), event));

        let mut body = |ui: &mut egui::Ui| {
            if let Some(route_ui) = &route_data {
                {
                    let editing = route_ui.editing;
                    let title = route_ui.title.as_str();

                    if !native {
                        // Header with editing indicator
                        ui.vertical_centered(|ui| {
                            ui.heading(egui::RichText::new(title).size(18.0).strong());
                            if editing {
                                ui.label(
                                    egui::RichText::new("Click on chart to add waypoints")
                                        .size(12.0)
                                        .color(theme.success),
                                );
                            }
                        });
                        ui.separator();
                    }
                    if let Some(notice) = route_ui.notice.as_deref() {
                        ui.label(notice);
                    }
                    let display_available = route_ui.display_available;
                    if display_available {
                        // Rendering toggle at top of panel
                        let rendering_enabled = route_ui.rendering_enabled;
                        ui.horizontal(|ui| {
                            ui.label("Route Display:");
                            let btn_text = if rendering_enabled { "ON" } else { "OFF" };
                            let btn_color = if rendering_enabled {
                                theme.success
                            } else {
                                theme.error
                            };
                            if ui
                                .add_enabled(
                                    display_available,
                                    egui::Button::new(
                                        egui::RichText::new(btn_text).color(btn_color),
                                    ),
                                )
                                .clicked()
                            {
                                push(r#"{"type":"ToggleRendering"}"#.to_string());
                            }
                        });
                        ui.separator();
                    }
                    // Action buttons
                    if let Some(actions) = &route_ui.actions {
                        ui.horizontal_wrapped(|ui| {
                            for action in actions {
                                if ui
                                    .add_enabled(
                                        action.enabled,
                                        egui::Button::new(action.label.as_str()),
                                    )
                                    .clicked()
                                {
                                    let Some(event) = action.event() else {
                                        continue;
                                    };
                                    push(event.to_string());
                                }
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Route list section
                    if let Some(routes) = &route_ui.routes {
                        if !routes.is_empty() {
                            egui::CollapsingHeader::new(
                                egui::RichText::new(format!("Routes ({})", routes.len())).strong(),
                            )
                            .default_open(true)
                            .show(ui, |ui| {
                                let available_width = ui.available_width();
                                for (idx, route) in routes.iter().enumerate() {
                                    let route_id = route.id;
                                    let name = route.name.as_str();
                                    let wp_count = route.waypoint_count;
                                    let dist = route.total_distance.as_str();
                                    let is_active = route.active;

                                    let bg_color = if is_active {
                                        theme.selection
                                    } else {
                                        theme.raised
                                    };

                                    // Check if this route is being edited
                                    let route_editable = route.editable.unwrap_or(!native);
                                    let is_editing = route_editable
                                        && route_editing
                                            .as_ref()
                                            .is_some_and(|(id, _)| *id == route_id);

                                    // Track button clicks to prevent frame from consuming them
                                    let mut button_clicked = false;

                                    let frame_response = egui::Frame::new()
                                        .fill(bg_color)
                                        .corner_radius(4.0)
                                        .inner_margin(egui::Margin::symmetric(8, 4))
                                        .show(ui, |ui| {
                                            ui.set_min_width(available_width - 16.0);
                                            ui.horizontal(|ui| {
                                                // Route info (left side)
                                                ui.vertical(|ui| {
                                                    if is_editing {
                                                        // Inline text edit
                                                        if let Some((_, edit_text)) =
                                                            route_editing.as_mut()
                                                        {
                                                            let response = ui.add(
                                                                egui::TextEdit::singleline(
                                                                    edit_text,
                                                                )
                                                                .desired_width(
                                                                    available_width - 80.0,
                                                                )
                                                                .font(egui::TextStyle::Body),
                                                            );
                                                            // Auto-focus on the text field
                                                            response.request_focus();
                                                            // Confirm on Enter or focus lost
                                                            if response.lost_focus()
                                                                || ui.input(|i| {
                                                                    i.key_pressed(egui::Key::Enter)
                                                                })
                                                            {
                                                                let new_name = edit_text.clone();
                                                                let event = serde_json::json!({"type":"RenameRoute", "id":route_id, "name":new_name}).to_string();
                                                                push(event);
                                                                *route_editing = None;
                                                            }
                                                            // Cancel on Escape
                                                            if ui.input(|i| {
                                                                i.key_pressed(egui::Key::Escape)
                                                            }) {
                                                                *route_editing = None;
                                                            }
                                                        }
                                                    } else {
                                                        ui.label(
                                                            egui::RichText::new(name).strong(),
                                                        );
                                                    }
                                                    ui.label(
                                                        egui::RichText::new(format!(
                                                            "{} WP • {}",
                                                            wp_count, dist
                                                        ))
                                                        .size(11.0)
                                                        .color(theme.muted),
                                                    );
                                                });

                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        // Delete route button - use sense to capture click properly
                                                        let delete_btn = ui.add(
                                                            egui::Button::new("X")
                                                                .small()
                                                                .sense(egui::Sense::click()),
                                                        );
                                                        if delete_btn
                                                            .on_hover_text("Delete route")
                                                            .clicked()
                                                        {
                                                            button_clicked = true;
                                                            let event = format!(
                                                                r#"{{"type":"DeleteRoute","id":{}}}"#,
                                                                route_id
                                                            );
                                                            push(event);
                                                        }
                                                        // Edit route name button
                                                        if !is_editing {
                                                            let edit_btn = ui.add_enabled(
                                                                route_editable,
                                                                egui::Button::new("E")
                                                                    .small()
                                                                    .sense(egui::Sense::click()),
                                                            );
                                                            if edit_btn
                                                                .on_hover_text("Rename route")
                                                                .clicked()
                                                            {
                                                                button_clicked = true;
                                                                *route_editing =
                                                                    Some((route_id, name.to_string()));
                                                            }
                                                        }
                                                    },
                                                );
                                            });
                                        });

                                    // Make entire frame clickable for selection (except when editing or clicking buttons)
                                    if !is_editing
                                        && !button_clicked
                                        && frame_response.response.clicked()
                                        && !is_active
                                    {
                                        let event =
                                            format!(r#"{{"type":"SelectRoute","index":{}}}"#, idx);
                                        push(event);
                                    }
                                    ui.add_space(2.0);
                                }
                            });
                            ui.add_space(4.0);
                        }
                    }

                    let content_editable = route_ui.content_editable.unwrap_or(!native);
                    let turn_radius_editable = route_ui.turn_radius_editable;
                    let active_route_id = route_ui.active_route_id;

                    // Active route waypoints
                    if let Some(_active_idx) = route_ui.active_route_index {
                        ui.separator();

                        // Waypoint count and total distance for active route
                        if let Some(count) = route_ui.waypoint_count {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Waypoints:").strong());
                                ui.label(format!("{}", count));
                            });
                        }
                        if let Some(dist) = route_ui.total_distance.as_deref() {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Total:").strong());
                                ui.label(dist);
                            });
                        }
                        ui.add_space(8.0);

                        // Waypoint list with delete and edit buttons
                        if let Some(waypoints) = &route_ui.waypoints {
                            // Text, combo/editor controls, frame margins and spacing.
                            // Only visible waypoint rows allocate/layout widgets.
                            let row_height = (ui.text_style_height(&egui::TextStyle::Body) * 4.0
                                + 52.0)
                                .max(96.0);
                            egui::ScrollArea::vertical().max_height(250.0).show_rows(
                                ui,
                                row_height,
                                waypoints.len(),
                                |ui, range| {
                                    let available_width = ui.available_width();

                                    for wp in &waypoints[range] {
                                        let wp_id = wp.id;
                                        let name = wp.name.as_str();
                                        let pos = wp.position.as_str();
                                        let leg = wp.leg_distance.as_deref();

                                        // Check if this waypoint is being edited
                                        let is_wp_editing = content_editable
                                            && waypoint_editing
                                                .as_ref()
                                                .is_some_and(|(id, _)| *id == wp_id);

                                        egui::Frame::new()
                                            .fill(theme.raised)
                                            .corner_radius(4.0)
                                            .inner_margin(egui::Margin::symmetric(8, 4))
                                            .show(ui, |ui| {
                                                ui.set_min_width((available_width - 16.0).max(0.0));
                                                ui.set_min_height(row_height - 10.0);
                                                ui.horizontal(|ui| {
                                                    ui.vertical(|ui| {
                                                        if is_wp_editing {
                                                            // Inline text edit for waypoint name
                                                            if let Some((_, edit_text)) =
                                                                waypoint_editing.as_mut()
                                                            {
                                                                let response = ui.add(
                                                                    egui::TextEdit::singleline(
                                                                        edit_text,
                                                                    )
                                                                    .desired_width(
                                                                        available_width - 80.0,
                                                                    )
                                                                    .font(egui::TextStyle::Body),
                                                                );
                                                                response.request_focus();

                                                                // Confirm on Enter
                                                                if ui.input(|i| {
                                                                    i.key_pressed(egui::Key::Enter)
                                                                }) {
                                                                    let event = serde_json::json!({"type":"RenameWaypoint", "id":wp_id, "name":edit_text}).to_string();
                                                                    push(event);
                                                                    *waypoint_editing = None;
                                                                }
                                                                // Cancel on Escape
                                                                if ui.input(|i| {
                                                                    i.key_pressed(egui::Key::Escape)
                                                                }) {
                                                                    *waypoint_editing = None;
                                                                }
                                                            }
                                                        } else {
                                                            ui.add(
                                                                egui::Label::new(
                                                                    egui::RichText::new(name)
                                                                        .strong(),
                                                                )
                                                                .truncate(),
                                                            );
                                                        }
                                                        ui.add(
                                                            egui::Label::new(
                                                                egui::RichText::new(pos)
                                                                    .size(11.0)
                                                                    .color(theme.muted),
                                                            )
                                                            .truncate(),
                                                        );
                                                        if let Some(event) =
                                                            crate::route_turn_radius_ui::show(
                                                                ui,
                                                                provider,
                                                                active_route_id.unwrap_or(0),
                                                                wp_id,
                                                                wp.turn_radius_nm,
                                                                turn_radius_editable
                                                                    && active_route_id.is_some(),
                                                            )
                                                        {
                                                            push(event);
                                                        }
                                                        if let Some(d) = leg {
                                                            let bearing = |value: Option<f64>| {
                                                                value
                                                                    .map(|v| format!("{v:.1}°"))
                                                                    .unwrap_or_else(|| {
                                                                        "Unavailable".into()
                                                                    })
                                                            };
                                                            ui.add(
                                                                egui::Label::new(
                                                                    egui::RichText::new(format!(
                                                                        "Leg: {}",
                                                                        d
                                                                    ))
                                                                    .size(11.0)
                                                                    .color(theme.accent),
                                                                )
                                                                .truncate(),
                                                            )
                                                            .on_hover_text(format!(
                                                                "WGS84 · initial bearing: {} · final bearing: {}",
                                                                bearing(wp.initial_bearing_deg),
                                                                bearing(wp.final_bearing_deg)
                                                            ));
                                                            let geometry = wp.leg_geometry.as_deref();
                                                            let editable =
                                                                route_ui.leg_geometry_editable;
                                                            let mut chosen =
                                                                geometry.unwrap_or("").to_owned();
                                                            ui.add_enabled_ui(editable, |ui| {
                                                                egui::ComboBox::from_id_salt((
                                                                    "s421_leg_geometry",
                                                                    wp_id,
                                                                ))
                                                                .selected_text(
                                                                    match chosen.as_str() {
                                                                        "Loxodrome" => "Rhumb line",
                                                                        "Orthodrome" => "Geodesic",
                                                                        _ => "Choose leg geometry",
                                                                    },
                                                                )
                                                                .show_ui(ui, |ui| {
                                                                    ui.selectable_value(
                                                                        &mut chosen,
                                                                        "Loxodrome".into(),
                                                                        "Rhumb line",
                                                                    );
                                                                    ui.selectable_value(
                                                                        &mut chosen,
                                                                        "Orthodrome".into(),
                                                                        "Geodesic",
                                                                    );
                                                                });
                                                            });
                                                            if Some(chosen.as_str()) != geometry
                                                                && !chosen.is_empty()
                                                            {
                                                                let event = serde_json::json!({"type":"SetLegGeometry", "id":wp_id, "geometry":chosen}).to_string();
                                                                push(event);
                                                            }
                                                        }
                                                    });

                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            // Delete button - use sense to capture click properly
                                                            let delete_btn = ui.add_enabled(
                                                                content_editable,
                                                                egui::Button::new("X")
                                                                    .small()
                                                                    .sense(egui::Sense::click()),
                                                            );
                                                            if delete_btn
                                                                .on_hover_text("Delete waypoint")
                                                                .clicked()
                                                            {
                                                                let event = format!(
                                                                    r#"{{"type":"DeleteWaypoint","id":{}}}"#,
                                                                    wp_id
                                                                );
                                                                push(event);
                                                            }
                                                            // Edit button
                                                            if !is_wp_editing {
                                                                let edit_btn = ui.add_enabled(
                                                                    content_editable,
                                                                    egui::Button::new("E")
                                                                        .small()
                                                                        .sense(egui::Sense::click()),
                                                                );
                                                                if edit_btn
                                                                    .on_hover_text("Rename waypoint")
                                                                    .clicked()
                                                                {
                                                                    *waypoint_editing =
                                                                        Some((wp_id, name.to_string()));
                                                                }
                                                            }
                                                        },
                                                    );
                                                });
                                            });
                                        ui.add_space(2.0);
                                    }
                                },
                            );
                        }
                    } else if route_ui.routes.as_ref().is_some_and(|r| r.is_empty()) {
                        let message = if native {
                            "No routes. Import an S-421 file."
                        } else {
                            "No routes. Click 'New' to create one."
                        };
                        ui.label(egui::RichText::new(message).size(12.0).color(theme.muted));
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // S-421 Catalogue Settings
                    egui::CollapsingHeader::new(egui::RichText::new("S-421 Catalogues").strong())
                        .default_open(false)
                        .show(ui, |ui| {
                            let status_line = |ui: &mut egui::Ui,
                                               status: &Option<
                                ferrite_s421::panel::CatalogueLine,
                            >| {
                                if let Some(status) = status {
                                    let color = if status.loaded {
                                        theme.success
                                    } else {
                                        theme.error
                                    };
                                    ui.label(
                                        egui::RichText::new(status.message.as_str())
                                            .color(color)
                                            .size(11.0),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new("Not loaded")
                                            .color(theme.muted)
                                            .size(11.0),
                                    );
                                }
                            };
                            // FC Status
                            ui.horizontal(|ui| {
                                ui.label("FC:");
                                status_line(ui, &route_ui.fc_status);
                            });

                            // PC Status
                            ui.horizontal(|ui| {
                                ui.label("PC:");
                                status_line(ui, &route_ui.pc_status);
                            });

                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("Catalogue path: ./Catalogues/*/S-421/")
                                    .size(10.0)
                                    .color(theme.muted),
                            );
                        });

                    ui.add_space(4.0);

                    // Instructions based on mode
                    if editing {
                        ui.label(egui::RichText::new("Editing Mode:").size(12.0).strong());
                        ui.label(egui::RichText::new("• Left click: Add waypoint").size(11.0));
                        ui.label(egui::RichText::new("• Right click: Remove last").size(11.0));
                        ui.label(egui::RichText::new("• Click 'Finish' when done").size(11.0));
                    } else if native {
                        // The notice above describes this host's actual capabilities.
                    } else if route_ui.has_active_route_index {
                        ui.label(
                            egui::RichText::new("Click 'New' to add another route")
                                .size(12.0)
                                .color(theme.muted),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("Click 'New' to start a route")
                                .size(12.0)
                                .color(theme.muted),
                        );
                    }
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("Route Plan").size(18.0).strong());
                });
                ui.separator();
                ui.label("Loading plugin...");
            }
        };
        if native {
            let mut open = true;
            egui::Window::new("S-421 Routes")
                .id(egui::Id::new("native_s421_routes"))
                .default_pos([280.0, 70.0])
                .default_width(360.0)
                .default_height(420.0)
                .max_height((ctx.screen_rect().height() - 80.0).max(160.0))
                .open(&mut open)
                .vscroll(true)
                .show(ctx, &mut body);
            if !open {
                self.toggle = Some(provider.into());
            }
        } else {
            egui::SidePanel::left("route_panel")
                .default_width(280.0)
                .resizable(true)
                .show(ctx, body);
        }
    }
}

impl ProductPanel for RoutePanel {
    fn descriptor(&self) -> PanelDescriptor {
        ferrite_s421::panel::PANEL
    }
    /// Shown while a route provider's toolbar button is active.
    fn is_open(&self) -> bool {
        self.active.is_some()
    }
    /// Closing toggles the active provider off (its toolbar button).
    fn set_open(&mut self, open: bool) {
        if !open {
            if let Some(provider) = self.active {
                self.toggle = Some(provider.into());
            }
        }
    }
    /// The provider's toolbar button opens it (no view menu entry).
    fn in_menu(&self) -> bool {
        false
    }
    /// Drawn before the host's tool windows: the planner's side panel
    /// takes its screen space first.
    fn leading(&self) -> bool {
        true
    }
    fn draw(&mut self, ctx: &egui::Context, _shared: &mut SharedPanelState) {
        self.draw_panel(ctx);
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
