//! Application overlays; these layers do not change catalogue portrayal order.

pub(crate) fn ruler_painter(ctx: &egui::Context, chart_rect: egui::Rect) -> egui::Painter {
    // Chart chrome stays below movable windows (Middle) and menu popups
    // (Foreground), even when the rulers are submitted after those widgets.
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("coordinate_rulers"),
    ))
    .with_clip_rect(chart_rect)
}

pub(crate) fn centered_window(title: &'static str, ctx: &egui::Context) -> egui::Window<'static> {
    egui::Window::new(title)
        // Initial placement only: anchoring would prevent title-bar dragging.
        .default_pos(ctx.screen_rect().center())
        .pivot(egui::Align2::CENTER_CENTER)
        .movable(true)
        .constrain_to(ctx.screen_rect())
}

pub(crate) fn display_settings_window(ctx: &egui::Context) -> egui::Window<'static> {
    centered_window("S-101 Display Settings", ctx)
        .collapsible(false)
        .resizable(false)
        .min_width(400.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000., 800.),
            )),
            events,
            ..Default::default()
        }
    }

    #[test]
    fn late_ruler_submission_stays_below_window_and_popup_and_is_clipped() {
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(100., 60.), egui::pos2(900., 700.));
        let mut layer = None;
        let out = ctx.run(input(vec![]), |ctx| {
            ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("menu"),
            ))
            .rect_filled(rect, 0., egui::Color32::BLUE);
            ctx.layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("window"),
            ))
            .rect_filled(rect, 0., egui::Color32::GREEN);
            let painter = ruler_painter(ctx, rect);
            layer = Some(painter.layer_id());
            painter.rect_filled(rect, 0., egui::Color32::RED);
        });
        assert_eq!(layer.unwrap().order, egui::Order::Background);
        let fills: Vec<_> = out
            .shapes
            .iter()
            .filter_map(|s| {
                if let egui::epaint::Shape::Rect(r) = &s.shape {
                    Some((r.fill, s.clip_rect))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            fills.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
            vec![
                egui::Color32::RED,
                egui::Color32::GREEN,
                egui::Color32::BLUE
            ]
        );
        assert_eq!(fills[0].1, rect);
    }

    #[test]
    fn settings_title_drag_moves_window_and_position_survives_reopen() {
        let ctx = egui::Context::default();
        let frame = |events: Vec<egui::Event>| {
            let mut rect = egui::Rect::NOTHING;
            let _ = ctx.run(input(events), |ctx| {
                rect = display_settings_window(ctx)
                    .show(ctx, |ui| {
                        ui.label("Display mode");
                    })
                    .unwrap()
                    .response
                    .rect;
            });
            rect
        };
        frame(vec![]);
        let before = frame(vec![]);
        let title = before.min + egui::vec2(80., 12.);
        let destination = title + egui::vec2(110., 70.);
        frame(vec![egui::Event::PointerMoved(title)]);
        frame(vec![egui::Event::PointerButton {
            pos: title,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        frame(vec![egui::Event::PointerMoved(destination)]);
        let after = frame(vec![egui::Event::PointerButton {
            pos: destination,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(
            (after.min - before.min - egui::vec2(110., 70.)).length() < 2.,
            "before={before:?}, after={after:?}"
        );
        let _ = ctx.run(input(vec![]), |_| {});
        let reopened = frame(vec![]);
        assert!((reopened.min - after.min).length() < 1.);
    }
}

#[cfg(test)]
mod stacking_tests {
    use super::*;

    #[test]
    fn clicking_each_overlapping_window_raises_it_above_the_other() {
        let ctx = egui::Context::default();
        let render = |events: Vec<egui::Event>| {
            let mut windows = Vec::new();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000., 800.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    for (title, pos) in [("Display settings", [100., 100.]), ("Logs", [280., 180.])]
                    {
                        let response = centered_window(title, ctx)
                            .pivot(egui::Align2::LEFT_TOP)
                            .default_pos(pos)
                            .fixed_size([400., 280.])
                            .collapsible(false)
                            .show(ctx, |ui| {
                                ui.label(title);
                                ui.allocate_space(egui::vec2(390., 260.));
                            })
                            .unwrap()
                            .response;
                        windows.push((response.layer_id, response.rect));
                    }
                    // HUD is emitted last, as in the real application.
                    ruler_painter(ctx, ctx.screen_rect()).rect_filled(
                        ctx.screen_rect(),
                        0.,
                        egui::Color32::RED,
                    );
                },
            );
            windows
        };
        render(vec![]);
        let windows = render(vec![]);
        let overlap_rect = windows[0].1.intersect(windows[1].1);
        assert!(overlap_rect.is_positive(), "windows={windows:?}");
        let overlap = overlap_rect.center();
        for index in [0, 1, 0] {
            let title = if index == 0 {
                windows[index].1.min + egui::vec2(80., 12.)
            } else {
                egui::pos2(windows[index].1.max.x - 60., windows[index].1.min.y + 12.)
            };
            render(vec![egui::Event::PointerMoved(title)]);
            render(vec![egui::Event::PointerButton {
                pos: title,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]);
            render(vec![egui::Event::PointerButton {
                pos: title,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            render(vec![]);
            assert_eq!(
                ctx.memory(|m| m.layer_id_at(overlap)),
                Some(windows[index].0)
            );
        }
    }
}
