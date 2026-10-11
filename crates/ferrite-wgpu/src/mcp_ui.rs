//! Native S-100 service controls; independent of the external plugin system.
use crate::ui_chrome::Theme;

#[derive(Debug, Clone, Default)]
pub struct State {
    pub open: bool,
    pub enabled: bool,
    pub public_tunnel: bool,
    pub restart: bool,
    pub refresh: bool,
    pub status: String,
    pub url: String,
    pub approval_code: String,
    pub dataset_count: usize,
    /// The host reports the service could not start or publish.
    pub error: bool,
    /// Query indexes for the loaded datasets are still being built.
    pub preparing: bool,
}

/// One classification shared by the window and the toolbar badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Stopped,
    Starting,
    Preparing,
    Running,
    Error,
}

impl State {
    pub fn health(&self) -> Health {
        if self.error {
            Health::Error
        } else if !self.enabled {
            Health::Stopped
        } else if self.url.is_empty() {
            Health::Starting
        } else if self.preparing {
            Health::Preparing
        } else {
            Health::Running
        }
    }
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting",
            Self::Preparing => "Preparing Indexes",
            Self::Running => "Running",
            Self::Error => "Error",
        }
    }
    /// Toolbar badge colour; a stopped service shows no badge.
    pub(crate) fn badge(self, theme: &Theme) -> Option<egui::Color32> {
        match self {
            Self::Stopped => None,
            Self::Starting | Self::Preparing => Some(theme.warning),
            Self::Running => Some(theme.success),
            Self::Error => Some(theme.error),
        }
    }
}

pub fn draw(ctx: &egui::Context, state: &mut State) {
    if !state.open {
        return;
    }
    let theme = Theme::current(ctx);
    let health = state.health();
    let mut open = state.open;
    crate::ui_overlay_layout::centered_window("S-100 MCP Service", ctx)
        .id(egui::Id::new("s100-mcp-service-window"))
        .open(&mut open)
        .default_width(440.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(10., 10.), egui::Sense::hover());
                ui.painter().circle_filled(
                    rect.center(),
                    4.,
                    health.badge(&theme).unwrap_or(theme.muted),
                );
                ui.strong(health.label());
                if health == Health::Running {
                    ui.weak(format!("· {} datasets published", state.dataset_count));
                }
            });
            if !state.status.is_empty() && state.status != health.label() {
                ui.weak(&state.status);
            }
            ui.add_space(4.);
            ui.label("Read-only access for AI clients to the datasets loaded in this window.");

            ui.separator();
            ui.strong("Service");
            if ui
                .checkbox(&mut state.enabled, "Enable MCP Service")
                .changed()
            {
                state.restart = true;
            }
            if ui
                .checkbox(&mut state.public_tunnel, "Allow Public Access (ngrok)")
                .changed()
            {
                state.restart = true;
            }
            ui.weak(
                "Local access is the default. Public access requires ngrok and its own \
                 account configuration.",
            );

            if state.enabled && !state.url.is_empty() {
                ui.separator();
                ui.strong("Connection");
                // Values are read-only, selectable and never truncated.
                for (label, value) in [
                    ("Endpoint", state.url.clone()),
                    ("Approval Code", state.approval_code.clone()),
                ] {
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(110., 24.),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| ui.label(label),
                        );
                        ui.add(
                            egui::Label::new(egui::RichText::new(&value).monospace())
                                .selectable(true),
                        );
                        if ui
                            .button("Copy")
                            .on_hover_text(format!("Copy {label}"))
                            .clicked()
                        {
                            ui.ctx().copy_text(value.clone());
                        }
                    });
                }
                ui.weak(
                    "Enter the approval code only on the authorization page of a client you \
                     chose to connect. Stopping the service revokes its tokens.",
                );
                if ui
                    .add_enabled(
                        health != Health::Preparing,
                        egui::Button::new("Refresh Datasets"),
                    )
                    .on_hover_text("Republish the datasets currently loaded")
                    .clicked()
                {
                    state.refresh = true;
                }
            }

            ui.separator();
            egui::CollapsingHeader::new("Supported Queries")
                .id_salt("s100-mcp-queries")
                .show(ui, |ui| {
                    ui.label("All loaded products: datasets, product and FC/PC metadata.");
                    ui.label(
                        "S-101: catalogue search, attributes, feature lookup and approximate \
                         spatial queries.",
                    );
                    ui.label(
                        "Other products expose metadata only. Queries never modify charts, \
                         routes or portrayal.",
                    );
                });
        });
    state.open = open;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn health_follows_error_enable_endpoint_and_index_state_in_that_order() {
        let mut s = State::default();
        assert_eq!(s.health(), Health::Stopped);
        s.enabled = true;
        assert_eq!(s.health(), Health::Starting);
        s.url = "http://127.0.0.1:1/mcp".into();
        s.preparing = true;
        assert_eq!(s.health(), Health::Preparing);
        s.preparing = false;
        assert_eq!(s.health(), Health::Running);
        s.error = true;
        assert_eq!(s.health(), Health::Error);
        s.enabled = false;
        assert_eq!(s.health(), Health::Error);
    }
    #[test]
    fn stopped_service_has_no_badge_and_others_do() {
        let theme = Theme::for_profile("Day");
        assert!(Health::Stopped.badge(&theme).is_none());
        for h in [
            Health::Starting,
            Health::Preparing,
            Health::Running,
            Health::Error,
        ] {
            assert!(h.badge(&theme).is_some());
        }
        assert_ne!(Health::Running.badge(&theme), Health::Error.badge(&theme));
    }
}
