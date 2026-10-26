//! Optional application coordinate rulers, independent of catalogue portrayal.
use ferrite_render::{Scaler, ScreenPoint, WorldPoint};

#[derive(Clone, Copy)]
pub(crate) enum Axis {
    Longitude,
    Latitude,
}

pub(crate) fn ticks(scaler: &Scaler, axis: Axis, pixels_per_point: f32) -> Vec<(f32, String)> {
    let v = scaler.viewport;
    if !pixels_per_point.is_finite() || pixels_per_point <= 0. || v.width <= 0. || v.height <= 0. {
        return Vec::new();
    }
    let center =
        scaler.screen_to_world(ScreenPoint::new(v.x + v.width * 0.5, v.y + v.height * 0.5));
    let (first, last, length, spacing) = match axis {
        Axis::Longitude => (
            scaler.screen_to_world(ScreenPoint::new(v.x, v.y)).x,
            scaler
                .screen_to_world(ScreenPoint::new(v.x + v.width, v.y))
                .x,
            v.width,
            160.,
        ),
        Axis::Latitude => (
            scaler
                .screen_to_world(ScreenPoint::new(v.x, v.y + v.height))
                .y,
            scaler.screen_to_world(ScreenPoint::new(v.x, v.y)).y,
            v.height,
            65.,
        ),
    };
    let span = last - first;
    if !first.is_finite()
        || !last.is_finite()
        || !center.x.is_finite()
        || !center.y.is_finite()
        || span <= 0.
    {
        return Vec::new();
    }
    let target = span / (length / (spacing * pixels_per_point)).clamp(1., 48.) as f64;
    let magnitude = 10_f64.powf(target.log10().floor());
    let step = [1., 2., 5., 10.]
        .into_iter()
        .map(|n| n * magnitude)
        .find(|s| *s >= target)
        .unwrap_or(target);
    if !step.is_finite() || step <= 0. {
        return Vec::new();
    }
    let start = (first / step).ceil() * step;
    (0..64)
        .map(|i| start + i as f64 * step)
        .take_while(|value| *value <= last + step * 1e-9)
        .filter_map(|value| {
            if matches!(axis, Axis::Latitude) && value.abs() > 90. {
                return None;
            }
            let world = match axis {
                Axis::Longitude => WorldPoint::new(value, center.y),
                Axis::Latitude => WorldPoint::new(center.x, value),
            };
            let screen = scaler.world_to_screen(world);
            let position = match axis {
                Axis::Longitude => screen.x,
                Axis::Latitude => screen.y,
            } / pixels_per_point;
            position
                .is_finite()
                .then(|| (position, label(value, step, axis)))
        })
        .collect()
}

fn label(value: f64, step: f64, axis: Axis) -> String {
    let value = match axis {
        Axis::Longitude => (value + 180.).rem_euclid(360.) - 180.,
        Axis::Latitude => value,
    };
    let precision = ((-step.log10()).ceil() as usize).clamp(0, 14);
    let rounded_zero = value.abs() < 0.5 * 10_f64.powi(-(precision as i32));
    let hemisphere = if rounded_zero {
        ""
    } else {
        match axis {
            Axis::Longitude if value < 0. => "W",
            Axis::Longitude => "E",
            Axis::Latitude if value < 0. => "S",
            Axis::Latitude => "N",
        }
    };
    format!(
        "{:.*}°{}",
        precision,
        if rounded_zero { 0. } else { value.abs() },
        hemisphere
    )
}

pub(crate) fn draw(
    ctx: &egui::Context,
    state: &crate::egui_integration::AppUiState,
    rect: egui::Rect,
) {
    if !state.coordinate_rulers {
        return;
    }
    let Some(scaler) = state.coordinate_ruler_scaler.as_ref() else {
        return;
    };
    let ppp = ctx.pixels_per_point();
    if !scaler.viewport.matches_physical_rect((
        rect.min.x * ppp,
        rect.min.y * ppp,
        rect.width() * ppp,
        rect.height() * ppp,
    )) {
        return;
    }
    let theme = crate::ui_chrome::Theme::current(ctx);
    let painter = crate::ui_overlay_layout::ruler_painter(ctx, rect);
    let font = egui::FontId::proportional(11.);
    let latitude_ticks = if state.coordinate_rulers {
        ticks(scaler, Axis::Latitude, ppp)
    } else {
        Vec::new()
    };
    let left_width = if state.coordinate_rulers {
        latitude_ticks
            .iter()
            .map(|(_, text)| {
                painter
                    .layout_no_wrap(text.clone(), font.clone(), theme.text)
                    .size()
                    .x
                    + 14.
            })
            .fold(64_f32, f32::max)
    } else {
        0.
    };
    if state.coordinate_rulers {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 24.)),
            0.,
            theme.panel,
        );
        for (x, text) in ticks(scaler, Axis::Longitude, ppp) {
            let half_width = painter
                .layout_no_wrap(text.clone(), font.clone(), theme.text)
                .size()
                .x
                * 0.5
                + 5.;
            if x < rect.min.x + left_width + half_width || x > rect.max.x - half_width {
                continue;
            }
            painter.text(
                egui::pos2(x, rect.min.y + 3.),
                egui::Align2::CENTER_TOP,
                text,
                font.clone(),
                theme.text,
            );
            painter.line_segment(
                [
                    egui::pos2(x, rect.min.y + 19.),
                    egui::pos2(x, rect.min.y + 24.),
                ],
                egui::Stroke::new(1_f32, theme.muted),
            );
        }
    }
    if state.coordinate_rulers {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(left_width, rect.height())),
            0.,
            theme.panel,
        );
        for (y, text) in latitude_ticks {
            if y < rect.min.y + 36. || y > rect.max.y - 44. {
                continue;
            }
            painter.text(
                egui::pos2(rect.min.x + left_width - 9., y),
                egui::Align2::RIGHT_CENTER,
                text,
                font.clone(),
                theme.text,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.min.x + left_width - 5., y),
                    egui::pos2(rect.min.x + left_width, y),
                ],
                egui::Stroke::new(1_f32, theme.muted),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{FlatProjection, GeoBounds, Viewport};
    #[test]
    fn ruler_ticks_follow_actual_projection_dpi_and_offset_without_wrapping_camera() {
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for ppp in [1., 2.] {
                let mut scaler = Scaler::new(
                    GeoBounds::new(179., 65., 181., 67.),
                    Viewport::with_origin(220. * ppp, 80. * ppp, 1000. * ppp, 700. * ppp),
                );
                scaler.set_projection(projection);
                for axis in [Axis::Longitude, Axis::Latitude] {
                    let result = ticks(&scaler, axis, ppp);
                    assert!(!result.is_empty() && result.len() <= 64);
                    for (position, text) in result {
                        let screen = match axis {
                            Axis::Longitude => ScreenPoint::new(position * ppp, scaler.viewport.y),
                            Axis::Latitude => ScreenPoint::new(scaler.viewport.x, position * ppp),
                        };
                        let world = scaler.screen_to_world(screen);
                        let expected = match axis {
                            Axis::Longitude => world.x,
                            Axis::Latitude => world.y,
                        };
                        let displayed: f64 = text.split('°').next().unwrap().parse().unwrap();
                        let signed = if text.ends_with('W') || text.ends_with('S') {
                            -displayed
                        } else {
                            displayed
                        };
                        let expected = match axis {
                            Axis::Longitude => (expected + 180.).rem_euclid(360.) - 180.,
                            Axis::Latitude => expected,
                        };
                        assert!((expected - signed).abs() < 0.001, "{expected} != {text}");
                    }
                }
            }
        }
    }
    #[test]
    fn fine_zoom_and_hemispheres_remain_readable() {
        assert_ne!(
            label(0., 1e-8, Axis::Longitude),
            label(1e-8, 1e-8, Axis::Longitude)
        );
        assert_eq!(label(180.25, 0.001, Axis::Longitude), "179.750°W");
        assert_eq!(label(-0.00000001, 0.1, Axis::Latitude), "0.0°");
        let scaler = Scaler::new(
            GeoBounds::new(-0.0001, 50., 0.0001, 50.0002),
            Viewport::new(1000., 700.),
        );
        assert!(ticks(&scaler, Axis::Longitude, 1.)
            .iter()
            .any(|(_, s)| s.contains("0.000")));
        assert!(ticks(&scaler, Axis::Latitude, f32::NAN).is_empty());
    }
}
