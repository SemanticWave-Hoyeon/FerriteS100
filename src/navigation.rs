//! Application navigation policy; portrayal units and product adapters are independent.
use winit::event::MouseScrollDelta;

pub const MIN_ZOOM: f64 = 0.005;
pub const MAX_ZOOM: f64 = 200.;

pub fn bounded_zoom(zoom: f64) -> Option<f64> {
    (zoom.is_finite() && zoom > 0.).then(|| zoom.clamp(MIN_ZOOM, MAX_ZOOM))
}

/// Add in logarithmic space: opposite steps cancel and batching is consistent
/// until the user intentionally reaches a zoom boundary. O(1), no allocation.
pub fn zoom_by_steps(current: f64, steps: f64, factor: f64) -> Option<f64> {
    let current = bounded_zoom(current)?;
    if !steps.is_finite() || !factor.is_finite() || factor <= 1. {
        return None;
    }
    if steps == 0. {
        return Some(current);
    }
    let log_zoom = current.ln() + steps * factor.ln();
    Some(if log_zoom <= MIN_ZOOM.ln() {
        MIN_ZOOM
    } else if log_zoom >= MAX_ZOOM.ln() {
        MAX_ZOOM
    } else {
        log_zoom.exp()
    })
}

/// PixelDelta is physical pixels in winit. Convert to logical distance using
/// the current window's native scale, then use 50 logical pixels per wheel step.
/// LineDelta is already in wheel steps and must not be scaled by monitor DPI.
pub fn scroll_zoom_target(current: f64, delta: MouseScrollDelta, native_scale: f64) -> Option<f64> {
    let steps = match delta {
        MouseScrollDelta::LineDelta(_, y) => y as f64,
        MouseScrollDelta::PixelDelta(position) => {
            if !native_scale.is_finite() || native_scale <= 0. {
                return None;
            }
            position.y / native_scale / 50.
        }
    };
    zoom_by_steps(current, steps, 1.15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    fn close(a: f64, b: f64) {
        assert!(
            (a - b).abs() < 1e-10 * a.abs().max(b.abs()).max(1.),
            "{a} != {b}"
        );
    }

    #[test]
    fn pixel_inputs_match_across_monitor_density_and_line_input() {
        for logical in [-400., -50., -7.25, 0., 7.25, 50., 400.] {
            let expected = 25. * 1.15_f64.powf(logical / 50.);
            for density in [1., 1.25, 1.5, 2., 3.] {
                let actual = scroll_zoom_target(
                    25.,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0., logical * density)),
                    density,
                )
                .unwrap();
                close(actual, expected);
            }
        }
        for density in [1., 2., 3.] {
            close(
                scroll_zoom_target(25., MouseScrollDelta::LineDelta(0., 1.), density).unwrap(),
                28.75,
            );
        }
    }

    #[test]
    fn batched_split_and_reverse_scroll_preserve_scale() {
        let batched = zoom_by_steps(25., -8., 1.15).unwrap();
        let split = (0..32).fold(25., |z, _| zoom_by_steps(z, -0.25, 1.15).unwrap());
        close(batched, split);
        close(zoom_by_steps(batched, 8., 1.15).unwrap(), 25.);
        // The previous 1+0.15*(-8) factor was negative and clamped to MIN_ZOOM.
        assert!(batched > 7. && batched < 9.);
    }

    #[test]
    fn bounds_are_directional_and_invalid_inputs_are_rejected() {
        assert_eq!(zoom_by_steps(25., -f64::MAX, 1.15), Some(MIN_ZOOM));
        assert_eq!(zoom_by_steps(25., f64::MAX, 1.15), Some(MAX_ZOOM));
        assert_eq!(zoom_by_steps(MIN_ZOOM, -1., 1.5), Some(MIN_ZOOM));
        assert_eq!(zoom_by_steps(MAX_ZOOM, 1., 1.5), Some(MAX_ZOOM));
        assert!(zoom_by_steps(MIN_ZOOM, 1., 1.5).unwrap() > MIN_ZOOM);
        assert!(zoom_by_steps(MAX_ZOOM, -1., 1.5).unwrap() < MAX_ZOOM);
        for invalid in [0., -1., f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(bounded_zoom(invalid).is_none());
            assert!(scroll_zoom_target(
                25.,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0., 50.)),
                invalid
            )
            .is_none());
        }
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(zoom_by_steps(25., invalid, 1.15).is_none());
            assert!(
                scroll_zoom_target(25., MouseScrollDelta::LineDelta(0., invalid as f32), 1.)
                    .is_none()
            );
        }
    }
    #[test]
    fn requested_two_hundred_times_limit_is_reachable_from_all_zoom_inputs() {
        assert_eq!(bounded_zoom(200.), Some(200.));
        assert_eq!(bounded_zoom(250.), Some(200.));
        assert_eq!(zoom_by_steps(150., 1., 1.5), Some(200.));
        assert_eq!(
            scroll_zoom_target(150., MouseScrollDelta::LineDelta(0., 5.), 1.),
            Some(200.)
        );
        for density in [1., 1.25, 1.5, 2., 3.] {
            assert_eq!(
                scroll_zoom_target(
                    150.,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0., 250. * density)),
                    density
                ),
                Some(200.)
            );
        }
        assert!(zoom_by_steps(200., -1., 1.5).unwrap() < 200.);
    }
}

/// A similarity gesture maps the world point under `from` to `to` while
/// changing scale. All coordinates are physical window pixels, like Scaler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureMotion {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub ratio: f64,
}

pub fn gesture_camera(
    base: ferrite_render::GeoBounds,
    current: &ferrite_render::Scaler,
    zoom: f64,
    motion: GestureMotion,
) -> Option<(f64, ferrite_render::GeoBounds)> {
    let (fx, fy) = motion.from;
    let (tx, ty) = motion.to;
    if ![fx, fy, tx, ty, motion.ratio].iter().all(|v| v.is_finite()) || motion.ratio <= 0. {
        return None;
    }
    let zoom = bounded_zoom(zoom * motion.ratio)?;
    let anchor = current.screen_to_world(ferrite_render::ScreenPoint::new(fx as f32, fy as f32));
    let bounds = ferrite_render::anchored_zoom_bounds_projected(
        current.projection(),
        base,
        current.viewport,
        zoom,
        anchor,
        ferrite_render::ScreenPoint::new(tx as f32, ty as f32),
        current.geo_bounds.center().y,
    )?;
    Some((zoom, bounds))
}

pub fn pinch_motion(pivot: (f64, f64), delta: f64) -> Option<GestureMotion> {
    // AppKit NSEvent.magnification is the fractional change since the previous
    // event. Do not add it to the absolute zoom or convert it to wheel ticks.
    let ratio = 1. + delta;
    (delta.is_finite() && ratio.is_finite() && ratio > 0.).then_some(GestureMotion {
        from: pivot,
        to: pivot,
        ratio,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchAction {
    Motion(GestureMotion),
    Tap((f64, f64)),
}
#[derive(Clone, Copy)]
struct Contact {
    key: (winit::event::DeviceId, u64),
    point: (f64, f64),
    origin: (f64, f64),
    max_distance: f64,
}

/// Sequence ownership is decided at the first contact and retained through
/// termination. A contact-count transition rebases the gesture, never moves
/// the camera. Three or more fingers pause navigation without losing IDs.
#[derive(Default)]
pub struct TouchNavigation {
    contacts: Vec<Contact>,
    chart_owned: bool,
    multi_touch: bool,
}
impl TouchNavigation {
    pub fn active(&self) -> bool {
        !self.contacts.is_empty()
    }
    pub fn reset(&mut self) {
        self.contacts.clear();
        self.chart_owned = false;
        self.multi_touch = false;
    }
    fn frame(&self) -> Option<((f64, f64), f64)> {
        match self.contacts.as_slice() {
            [a] => Some((a.point, 0.)),
            [a, b] => Some((
                ((a.point.0 + b.point.0) * 0.5, (a.point.1 + b.point.1) * 0.5),
                (a.point.0 - b.point.0).hypot(a.point.1 - b.point.1),
            )),
            _ => None,
        }
    }
    pub fn event(
        &mut self,
        key: (winit::event::DeviceId, u64),
        phase: winit::event::TouchPhase,
        point: (f64, f64),
        chart_allowed: bool,
        native_scale: f64,
    ) -> Option<TouchAction> {
        use winit::event::TouchPhase;
        let index = self.contacts.iter().position(|c| c.key == key);
        // Always release a known ID, including a non-finite terminal position.
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            let i = index?;
            let mut c = self.contacts.remove(i);
            if point.0.is_finite() && point.1.is_finite() {
                c.point = point;
                c.max_distance = c
                    .max_distance
                    .max((point.0 - c.origin.0).hypot(point.1 - c.origin.1));
            } else {
                c.max_distance = f64::INFINITY;
            }
            let tap = phase == TouchPhase::Ended
                && self.chart_owned
                && !self.multi_touch
                && self.contacts.is_empty()
                && native_scale.is_finite()
                && native_scale > 0.
                && c.max_distance <= 8. * native_scale;
            if self.contacts.is_empty() {
                self.reset();
            }
            return tap.then_some(TouchAction::Tap(c.point));
        }
        if !point.0.is_finite() || !point.1.is_finite() {
            return None;
        }
        if phase == TouchPhase::Started {
            if index.is_some() {
                return None;
            }
            if self.contacts.is_empty() {
                self.chart_owned = chart_allowed;
                self.multi_touch = false;
            }
            self.contacts.push(Contact {
                key,
                point,
                origin: point,
                max_distance: 0.,
            });
            self.multi_touch |= self.contacts.len() > 1;
            return None;
        }
        let i = index?;
        let before = self.frame();
        let c = &mut self.contacts[i];
        c.point = point;
        c.max_distance = c
            .max_distance
            .max((point.0 - c.origin.0).hypot(point.1 - c.origin.1));
        if !self.chart_owned {
            return None;
        }
        let (from, old_distance) = before?;
        let (to, new_distance) = self.frame()?;
        let ratio = if self.contacts.len() == 2 && old_distance > 1. && new_distance > 1. {
            new_distance / old_distance
        } else {
            1.
        };
        Some(TouchAction::Motion(GestureMotion { from, to, ratio }))
    }
}

#[cfg(test)]
mod gesture_tests {
    use super::*;
    use winit::event::{DeviceId, TouchPhase::*};
    fn key(id: u64) -> (DeviceId, u64) {
        (DeviceId::dummy(), id)
    }
    #[test]
    fn moving_centroid_and_pinch_keep_world_anchor_at_all_densities() {
        use ferrite_render::{GeoBounds, Scaler, ScreenPoint, Viewport};
        for density in [1., 1.25, 2., 3.] {
            for latitude in [-70., 0., 48.65, 80.] {
                let base = GeoBounds::new(-3., latitude - 1., -1., latitude + 1.);
                let mut s = Scaler::new(
                    base,
                    Viewport::new((1000. * density) as f32, (800. * density) as f32),
                );
                let mut z = 1.;
                for ratio in [1.2, 0.9, 2., 0.5, 200.] {
                    let from = (200. * density, 300. * density);
                    let to = (from.0 + 31. * density, from.1 - 17. * density);
                    let a = s.screen_to_world(ScreenPoint::new(from.0 as f32, from.1 as f32));
                    let (nz, b) =
                        gesture_camera(base, &s, z, GestureMotion { from, to, ratio }).unwrap();
                    s.set_bounds(b);
                    z = nz;
                    let p = s.world_to_screen(a);
                    assert!((p.x as f64 - to.0).abs() < 0.03 && (p.y as f64 - to.1).abs() < 0.03);
                }
            }
        }
    }
    #[test]
    fn contacts_rebase_across_one_two_three_and_cancel_without_tap() {
        let mut t = TouchNavigation::default();
        assert_eq!(t.event(key(1), Started, (100., 100.), true, 1.), None);
        assert_eq!(
            t.event(key(1), Moved, (110., 120.), false, 1.),
            Some(TouchAction::Motion(GestureMotion {
                from: (100., 100.),
                to: (110., 120.),
                ratio: 1.
            }))
        );
        assert_eq!(t.event(key(2), Started, (210., 120.), true, 1.), None);
        assert_eq!(
            t.event(key(2), Moved, (230., 120.), false, 1.),
            Some(TouchAction::Motion(GestureMotion {
                from: (160., 120.),
                to: (170., 120.),
                ratio: 1.2
            }))
        );
        assert_eq!(t.event(key(3), Started, (50., 50.), true, 1.), None);
        assert_eq!(t.event(key(1), Moved, (115., 120.), true, 1.), None);
        assert_eq!(t.event(key(3), Cancelled, (f64::NAN, 0.), true, 1.), None);
        assert!(matches!(
            t.event(key(2), Moved, (235., 120.), false, 1.),
            Some(TouchAction::Motion(_))
        ));
        assert_eq!(t.event(key(2), Ended, (235., 120.), false, 1.), None);
        assert_eq!(t.event(key(1), Ended, (115., 120.), false, 1.), None);
        assert!(!t.active());
    }
    #[test]
    fn ui_ownership_invalid_events_and_tap_path_distance_are_respected() {
        let mut t = TouchNavigation::default();
        t.event(key(1), Started, (10., 10.), false, 2.);
        assert_eq!(t.event(key(1), Moved, (80., 80.), true, 2.), None);
        assert_eq!(t.event(key(1), Ended, (80., 80.), true, 2.), None);
        assert_eq!(t.event(key(99), Moved, (0., 0.), true, 2.), None);
        t.event(key(1), Started, (10., 10.), true, 2.);
        assert_eq!(
            t.event(key(1), Ended, (20., 10.), false, 2.),
            Some(TouchAction::Tap((20., 10.)))
        );
        t.event(key(1), Started, (10., 10.), true, 1.);
        t.event(key(1), Moved, (100., 10.), true, 1.);
        assert_eq!(t.event(key(1), Ended, (10., 10.), true, 1.), None);
        t.event(key(1), Started, (10., 10.), true, 1.);
        t.event(key(1), Ended, (f64::NAN, 0.), true, 1.);
        assert!(!t.active());
        t.event(key(1), Started, (10., 10.), true, 1.);
        t.reset();
        assert!(!t.active());
        assert!(pinch_motion((0., 0.), f64::NAN).is_none());
        assert!(pinch_motion((0., 0.), -1.).is_none());
    }
}

/// Keeps native gesture ownership across cursor moves over UI surfaces.
#[derive(Default)]
pub struct GestureOwnership {
    active: bool,
    owned: bool,
}
impl GestureOwnership {
    pub fn reset(&mut self) {
        self.active = false;
        self.owned = false;
    }
    pub fn event(&mut self, phase: winit::event::TouchPhase, allowed: bool) -> bool {
        use winit::event::TouchPhase;
        if phase == TouchPhase::Started {
            self.active = true;
            self.owned = allowed;
        }
        let apply = self.active && self.owned && phase != TouchPhase::Cancelled;
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.reset();
        }
        apply
    }
}

/// Microsoft tablet signature plus touch bit; pen and hardware mouse remain
/// mouse inputs. Only mouse messages can be suppressed, never WM_TOUCH/POINTER.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn touch_promoted_mouse(message: u32, extra: u64) -> bool {
    (0x0200..=0x020e).contains(&message) && extra & 0xffffff00 == 0xff515700 && extra & 0x80 != 0
}
#[cfg(test)]
mod native_ownership_tests {
    use super::*;
    use winit::event::TouchPhase::*;
    #[test]
    fn native_sequence_never_changes_owner_and_cancel_clears_it() {
        let mut g = GestureOwnership::default();
        assert!(!g.event(Moved, true));
        assert!(!g.event(Started, false));
        assert!(!g.event(Moved, true));
        assert!(!g.event(Ended, true));
        assert!(g.event(Started, true));
        assert!(g.event(Moved, false));
        assert!(!g.event(Cancelled, true));
        assert!(!g.event(Moved, true));
        g.event(Started, true);
        g.reset();
        assert!(!g.event(Ended, true));
    }
    #[test]
    fn touch_promotion_filter_preserves_pen_mouse_and_nonmouse_messages() {
        for m in [0x0200, 0x0201, 0x0202, 0x020a, 0x020e] {
            assert!(touch_promoted_mouse(m, 0xff515780));
            assert!(touch_promoted_mouse(m, 0xffffffff_ff5157c7));
            assert!(!touch_promoted_mouse(m, 0xff515707));
            assert!(!touch_promoted_mouse(m, 0));
            assert!(!touch_promoted_mouse(m, 0x80));
        }
        for m in [0x0240, 0x0245, 0x0100, 0x0005] {
            assert!(!touch_promoted_mouse(m, 0xff515780));
        }
    }
}
