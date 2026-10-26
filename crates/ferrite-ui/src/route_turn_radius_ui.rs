//! Explicit turn-radius entry. Source values and unfinished input remain distinct.
#[derive(Clone, Debug, Default)]
struct Draft {
    text: String,
    source: Option<u64>,
    dirty: bool,
    focused: bool,
}
fn draft_id(provider: &str, route_id: u32, wp_id: u32) -> egui::Id {
    egui::Id::new(("s421_turn_radius_draft_v1", provider, route_id, wp_id))
}
impl Draft {
    fn synchronize(&mut self, value: Option<f64>, editable: bool) {
        let bits = value.map(f64::to_bits);
        // An accepted Apply is acknowledged only by the authoritative new value.
        if self.dirty
            && bits.is_some()
            && bits != self.source
            && self.text.parse::<f64>().ok().map(f64::to_bits) == bits
        {
            self.dirty = false;
        }
        if !editable || (!self.dirty && !self.focused) {
            if bits != self.source || !editable {
                self.text = value.map(|v| v.to_string()).unwrap_or_default();
            }
            self.source = bits;
            if !editable {
                self.dirty = false;
                self.focused = false;
            }
        }
    }
}
/// Emits an explicit decimal string. The S-421 controller performs authoritative
/// lexical/range validation; this editor never rounds or supplies a default.
pub(crate) fn show(
    ui: &mut egui::Ui,
    provider: &str,
    route_id: u32,
    wp_id: u32,
    value: Option<f64>,
    editable: bool,
) -> Option<String> {
    draw(ui, provider, route_id, wp_id, value, editable).0
}
fn draw(
    ui: &mut egui::Ui,
    provider: &str,
    route_id: u32,
    wp_id: u32,
    value: Option<f64>,
    editable: bool,
) -> (Option<String>, egui::Rect, egui::Rect) {
    let id = draft_id(provider, route_id, wp_id);
    let mut draft = ui
        .ctx()
        .data(|d| d.get_temp::<Draft>(id))
        .unwrap_or_default();
    // First frame with a concrete source must also populate the text.
    if draft.source.is_none() && !draft.dirty && !draft.focused {
        draft.text = value.map(|v| v.to_string()).unwrap_or_default();
    }
    draft.synchronize(value, editable);
    let mut event = None;
    let mut input_rect = egui::Rect::NOTHING;
    let mut apply_rect = egui::Rect::NOTHING;
    ui.push_id(id, |ui| {
        ui.horizontal(|ui| {
            ui.label("Radius (NM)").on_hover_text("Explicit waypoint turn radius in nautical miles");
            ui.add_enabled_ui(editable, |ui| {
                let response = ui.add(egui::TextEdit::singleline(&mut draft.text)
                    .id(id.with("input"))
                    .hint_text("Not set")
                    .char_limit(64)
                    .desired_width(56.));
                input_rect = response.rect;
                if response.changed() { draft.dirty = true; }
                draft.focused = response.has_focus();
                let apply = ui.add_enabled(!draft.text.is_empty(), egui::Button::new("Apply").small())
                    .on_hover_text("Explicit nautical miles: 0–5, up to two decimal places. No rounding.");
                apply_rect = apply.rect;
                if apply.clicked() {
                    event = Some(serde_json::json!({"type":"SetTurnRadius", "id":wp_id, "radius_nm":draft.text}).to_string());
                }
            });
            if !editable { ui.weak("Read-only"); }
        });
    });
    ui.ctx().data_mut(|d| d.insert_temp(id, draft));
    (event, input_rect, apply_rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(
        ctx: &egui::Context,
        provider: &str,
        route: u32,
        value: Option<f64>,
        editable: bool,
        events: Vec<egui::Event>,
    ) -> (Option<String>, egui::Rect, egui::Rect) {
        let mut result = (None, egui::Rect::NOTHING, egui::Rect::NOTHING);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700., 200.),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    result = draw(ui, provider, route, 7, value, editable);
                });
            },
        );
        result
    }
    fn click(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]
    }
    #[test]
    fn absent_radius_stays_blank_and_readonly_never_emits() {
        let ctx = egui::Context::default();
        let result = frame(&ctx, "native", 1, None, true, vec![]);
        assert!(result.0.is_none());
        assert!(ctx
            .data(|d| d.get_temp::<Draft>(draft_id("native", 1, 7)))
            .unwrap()
            .text
            .is_empty());
        assert!(
            frame(&ctx, "native", 1, None, true, click(result.2.center()))
                .0
                .is_none()
        );
        let result = frame(&ctx, "imported", 1, Some(0.25), false, vec![]);
        assert!(frame(
            &ctx,
            "imported",
            1,
            Some(0.25),
            false,
            click(result.2.center())
        )
        .0
        .is_none());
        assert_eq!(
            ctx.data(|d| d.get_temp::<Draft>(draft_id("imported", 1, 7)))
                .unwrap()
                .text,
            "0.25"
        );
    }
    #[test]
    fn typed_decimal_is_emitted_verbatim_without_rounding() {
        let ctx = egui::Context::default();
        let first = frame(&ctx, "native", 1, None, true, vec![]);
        frame(&ctx, "native", 1, None, true, click(first.1.center()));
        let typed = frame(
            &ctx,
            "native",
            1,
            None,
            true,
            vec![egui::Event::Text("0.251".into())],
        );
        let event = frame(&ctx, "native", 1, None, true, click(typed.2.center()))
            .0
            .expect("Apply event");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        // The controller must reject this precision; the editor must not turn it into 0.25.
        assert_eq!(json["radius_nm"], "0.251");
        assert_eq!(json["id"], 7);
    }
    #[test]
    fn drafts_are_namespaced_and_external_updates_do_not_erase_edits() {
        let ctx = egui::Context::default();
        frame(&ctx, "a", 1, Some(0.25), true, vec![]);
        let id = draft_id("a", 1, 7);
        ctx.data_mut(|d| {
            let mut s = d.get_temp::<Draft>(id).unwrap();
            s.text = "1.23".into();
            s.dirty = true;
            d.insert_temp(id, s);
        });
        frame(&ctx, "a", 1, Some(0.5), true, vec![]);
        frame(&ctx, "a", 2, Some(2.), true, vec![]);
        frame(&ctx, "b", 1, None, true, vec![]);
        assert_eq!(ctx.data(|d| d.get_temp::<Draft>(id)).unwrap().text, "1.23");
        assert_eq!(
            ctx.data(|d| d.get_temp::<Draft>(draft_id("a", 2, 7)))
                .unwrap()
                .text,
            "2"
        );
        assert_eq!(
            ctx.data(|d| d.get_temp::<Draft>(draft_id("b", 1, 7)))
                .unwrap()
                .text,
            ""
        );
        frame(&ctx, "a", 1, Some(1.23), true, vec![]);
        assert!(!ctx.data(|d| d.get_temp::<Draft>(id)).unwrap().dirty);
    }
    #[test]
    fn text_entry_is_bounded_to_64_characters() {
        let ctx = egui::Context::default();
        let first = frame(&ctx, "native", 1, None, true, vec![]);
        frame(&ctx, "native", 1, None, true, click(first.1.center()));
        frame(
            &ctx,
            "native",
            1,
            None,
            true,
            vec![egui::Event::Text("1".repeat(100))],
        );
        assert_eq!(
            ctx.data(|d| d.get_temp::<Draft>(draft_id("native", 1, 7)))
                .unwrap()
                .text
                .len(),
            64
        );
    }
    #[test]
    fn enter_does_not_apply_focused_input_or_unrelated_widgets() {
        let ctx = egui::Context::default();
        let first = frame(&ctx, "native", 1, None, true, vec![]);
        frame(&ctx, "native", 1, None, true, click(first.1.center()));
        frame(
            &ctx,
            "native",
            1,
            None,
            true,
            vec![egui::Event::Text("0.25".into())],
        );
        let enter = || egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        };
        assert!(frame(&ctx, "native", 1, None, true, vec![enter()])
            .0
            .is_none());
        frame(&ctx, "native", 1, None, true, click(egui::pos2(600., 150.)));
        assert!(frame(&ctx, "native", 1, None, true, vec![enter()])
            .0
            .is_none());
    }
}
