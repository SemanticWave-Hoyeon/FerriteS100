//! Application chrome only. These colours and icons never replace portrayal catalogue symbols.
use egui::{Color32, Context, Response, Stroke, Ui};
#[derive(Clone, Copy)]
pub(crate) struct Theme {
    pub panel: Color32,
    pub raised: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub selection: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
}
impl Theme {
    pub fn for_profile(profile: &str) -> Self {
        let c = Color32::from_rgb;
        match profile {
            "Night" => Self {
                panel: c(19, 12, 14),
                raised: c(35, 22, 25),
                text: c(191, 145, 127),
                muted: c(158, 115, 105),
                accent: c(205, 135, 107),
                selection: c(61, 33, 30),
                success: c(159, 145, 108),
                warning: c(202, 149, 105),
                error: c(215, 121, 104),
            },
            "Dusk" => Self {
                panel: c(27, 25, 29),
                raised: c(42, 38, 42),
                text: c(224, 207, 190),
                muted: c(178, 157, 148),
                accent: c(224, 168, 111),
                selection: c(66, 48, 36),
                success: c(173, 190, 127),
                warning: c(224, 168, 111),
                error: c(224, 149, 132),
            },
            _ => Self {
                panel: c(19, 28, 38),
                raised: c(31, 43, 55),
                text: c(232, 239, 245),
                muted: c(166, 182, 196),
                accent: c(72, 203, 219),
                selection: c(25, 70, 81),
                success: c(107, 211, 151),
                warning: c(242, 190, 106),
                error: c(249, 139, 127),
            },
        }
    }
    pub fn current(ctx: &Context) -> Self {
        ctx.data(|d| d.get_temp::<Self>(egui::Id::new("application_chrome_theme")))
            .unwrap_or_else(|| Self::for_profile("Day"))
    }
    pub fn apply(self, ctx: &Context, reduced_motion: bool) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("application_chrome_theme"), self));
        let animation = if reduced_motion { 0. } else { 0.12 };
        if ctx.style().visuals.panel_fill == self.panel && ctx.style().animation_time == animation {
            return;
        }
        let mut style = (*ctx.style()).clone();
        style.animation_time = animation;
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = self.panel;
        style.visuals.window_fill = self.panel;
        style.visuals.faint_bg_color = self.raised;
        style.visuals.extreme_bg_color = self.panel;
        style.visuals.override_text_color = Some(self.text);
        style.visuals.selection.bg_fill = self.selection;
        style.visuals.selection.stroke = Stroke::new(1.0_f32, self.accent);
        style.visuals.hyperlink_color = self.accent;
        style.visuals.window_stroke = Stroke::new(1.0_f32, self.raised);
        for widget in [
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.noninteractive,
        ] {
            widget.bg_fill = self.raised;
            widget.weak_bg_fill = self.raised;
            widget.fg_stroke = Stroke::new(1.0_f32, self.text);
            widget.bg_stroke = Stroke::new(1.0_f32, self.raised);
            widget.corner_radius = egui::CornerRadius::same(5);
        }
        style.visuals.widgets.hovered.bg_fill = self.selection;
        style.visuals.widgets.hovered.weak_bg_fill = self.selection;
        style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, self.accent);
        style.visuals.widgets.hovered.bg_stroke =
            Stroke::new(1.0_f32, self.accent.gamma_multiply(0.5));
        style.visuals.widgets.active.bg_fill = self.selection;
        style.visuals.widgets.active.weak_bg_fill = self.selection;
        style.visuals.widgets.active.fg_stroke = Stroke::new(1.5_f32, self.accent);
        style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, self.accent);
        style.spacing.item_spacing = egui::vec2(8., 7.);
        style.spacing.button_padding = egui::vec2(9., 6.);
        style.spacing.interact_size = egui::vec2(34., 30.);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(18.));
        ctx.set_style(style);
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Plus,
    Minus,
    Fit,
    File,
    Folder,
    Layers,
    Inspect,
    Select,
    Clock,
    Status,
    Logs,
    Settings,
    Debug,
    Ruler,
    Route,
    Plugin,
    Mcp,
}
pub(crate) fn icon_button(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    icon_toggle(ui, icon, label, false)
}
pub(crate) fn icon_toggle(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    icon_toggle_with_badge(ui, icon, label, selected, None)
}
/// Icon toggle with an optional status dot in its top-right corner.
pub(crate) fn icon_toggle_with_badge(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    badge: Option<Color32>,
) -> Response {
    let response = ui.add(
        egui::Button::new("")
            .selected(selected)
            .min_size(egui::vec2(30., 30.)),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let stroke = ui.style().interact(&response).fg_stroke;
    let center = response.rect.center();
    let p = ui.painter();
    let pos = |x, y| center + egui::vec2(x, y);
    match icon {
        Icon::Plus | Icon::Minus => {
            p.line_segment([pos(-6., 0.), pos(6., 0.)], stroke);
            if matches!(icon, Icon::Plus) {
                p.line_segment([pos(0., -6.), pos(0., 6.)], stroke);
            }
        }
        Icon::File => {
            p.rect_stroke(
                egui::Rect::from_min_max(pos(-5., -7.), pos(5., 7.)),
                0.,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.line_segment([pos(-2., -2.), pos(3., -2.)], stroke);
            p.line_segment([pos(-2., 2.), pos(3., 2.)], stroke);
        }
        Icon::Folder => {
            p.line_segment([pos(-7., -4.), pos(-2., -4.)], stroke);
            p.line_segment([pos(-2., -4.), pos(0., -2.)], stroke);
            p.rect_stroke(
                egui::Rect::from_min_max(pos(-7., -2.), pos(7., 6.)),
                0.,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Icon::Layers => {
            for y in [-4., 0., 4.] {
                p.line_segment([pos(-7., y), pos(7., y)], stroke);
            }
        }
        Icon::Select => {
            p.line_segment([pos(-5., -7.), pos(-5., 5.)], stroke);
            p.line_segment([pos(-5., -7.), pos(6., 1.)], stroke);
            p.line_segment([pos(6., 1.), pos(0., 2.)], stroke);
            p.line_segment([pos(0., 2.), pos(3., 7.)], stroke);
        }
        Icon::Inspect => {
            p.circle_stroke(pos(-2., -2.), 5., stroke);
            p.line_segment([pos(2., 2.), pos(7., 7.)], stroke);
        }
        Icon::Clock => {
            p.circle_stroke(center, 7., stroke);
            p.line_segment([center, pos(0., -4.)], stroke);
            p.line_segment([center, pos(4., 2.)], stroke);
        }
        Icon::Status => {
            p.rect_stroke(
                egui::Rect::from_min_max(pos(-7., -6.), pos(7., 6.)),
                0.,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.line_segment([pos(-4., 1.), pos(4., 1.)], stroke);
            p.line_segment([pos(-4., -2.), pos(0., -2.)], stroke);
        }
        Icon::Logs => {
            for y in [-5., 0., 5.] {
                p.line_segment([pos(-4., y), pos(7., y)], stroke);
                p.circle_filled(pos(-7., y), 1., stroke.color);
            }
        }
        Icon::Settings => {
            p.circle_stroke(center, 4., stroke);
            for (x, y) in [(0., -7.), (7., 0.), (0., 7.), (-7., 0.)] {
                p.line_segment([pos(x * 0.65, y * 0.65), pos(x, y)], stroke);
            }
        }
        Icon::Debug => {
            p.rect_stroke(
                egui::Rect::from_min_max(pos(-4., -5.), pos(4., 5.)),
                0.,
                stroke,
                egui::StrokeKind::Inside,
            );
            for y in [-3., 0., 3.] {
                p.line_segment([pos(-7., y), pos(-4., y)], stroke);
                p.line_segment([pos(4., y), pos(7., y)], stroke);
            }
        }
        Icon::Ruler => {
            // Top and left scales, as drawn along the chart edges.
            p.line_segment([pos(-7., -6.), pos(7., -6.)], stroke);
            p.line_segment([pos(-7., -6.), pos(-7., 7.)], stroke);
            for i in 0..4 {
                let t = -3. + 3. * i as f32;
                p.line_segment([pos(t, -6.), pos(t, -3.)], stroke);
                p.line_segment([pos(-7., t), pos(-4., t)], stroke);
            }
        }
        Icon::Route => {
            p.circle_stroke(pos(-6., 5.), 2., stroke);
            p.circle_stroke(pos(6., -5.), 2., stroke);
            p.line_segment([pos(-4.5, 3.5), pos(0., 2.)], stroke);
            p.line_segment([pos(0., 2.), pos(1., -2.)], stroke);
            p.line_segment([pos(1., -2.), pos(4.5, -3.5)], stroke);
        }
        Icon::Plugin => {
            p.rect_stroke(
                egui::Rect::from_min_max(pos(-6., -4.), pos(4., 6.)),
                1.,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.circle_stroke(pos(-1., -6.), 2., stroke);
            p.circle_stroke(pos(6., 1.), 2., stroke);
        }
        Icon::Mcp => {
            // A service node linked to two clients.
            p.circle_stroke(pos(-4., 0.), 3., stroke);
            p.circle_filled(pos(5., -5.), 2., stroke.color);
            p.circle_filled(pos(5., 5.), 2., stroke.color);
            p.line_segment([pos(-1.5, -1.5), pos(3.5, -4.)], stroke);
            p.line_segment([pos(-1.5, 1.5), pos(3.5, 4.)], stroke);
        }
        Icon::Fit => {
            for (x, y, dx, dy) in [
                (-6., -6., 1., 1.),
                (6., -6., -1., 1.),
                (-6., 6., 1., -1.),
                (6., 6., -1., -1.),
            ] {
                p.line_segment([pos(x, y), pos(x + 4. * dx, y)], stroke);
                p.line_segment([pos(x, y), pos(x, y + 4. * dy)], stroke);
            }
        }
    }
    if let Some(color) = badge {
        let dot = response.rect.right_top() + egui::vec2(-6., 6.);
        p.circle_filled(dot, 3.5, ui.visuals().panel_fill);
        p.circle_filled(dot, 2.5, color);
    }
    response.on_hover_text(label)
}
/// A short accent transition never hides text or animates the chart itself.
pub(crate) fn selection_emphasis(elapsed: f64, reduced_motion: bool) -> f32 {
    if reduced_motion {
        return 1.;
    }
    let t = if elapsed.is_finite() {
        (elapsed / 0.16).clamp(0., 1.) as f32
    } else {
        1.
    };
    1. - (1. - t).powi(3)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_respects_night_and_motion_preferences() {
        let ctx = Context::default();
        Theme::for_profile("Day").apply(&ctx, false);
        let day = ctx.style().visuals.panel_fill;
        Theme::for_profile("Night").apply(&ctx, true);
        assert_ne!(ctx.style().visuals.panel_fill, day);
        assert_eq!(ctx.style().animation_time, 0.);
        assert!(Theme::for_profile("Night").accent.r() > Theme::for_profile("Night").accent.b());
        assert_eq!(selection_emphasis(0., true), 1.);
        assert_eq!(selection_emphasis(-1., false), 0.);
        assert_eq!(selection_emphasis(1., false), 1.);
    }
}
