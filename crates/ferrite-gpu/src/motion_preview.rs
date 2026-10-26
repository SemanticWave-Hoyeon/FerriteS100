//! Interaction preview: while the camera moves, retained geometry may be shown
//! through the GPU affine although view-dependent placement or S-98 coverage
//! selection would differ at the target camera. The settle rebuild (and every
//! consumer through `ensure_navigation_scene`) restores the exact scene.
//! `FERRITE_MOTION_PREVIEW=0` restores rebuild-every-frame navigation.
use std::ffi::OsStr;

/// Largest per-axis zoom change shown from one retained scene.
pub(crate) const MAX_SCALE_DRIFT: f32 = 1.5;
/// Largest centre travel shown from one retained scene, as a viewport fraction.
pub(crate) const MAX_CENTRE_DRIFT: f32 = 0.25;
/// Fraction of the drift budget at which a background build is started, so
/// the next scene normally arrives before the budget is spent.
pub(crate) const PREFETCH_FRACTION: f32 = 0.5;

pub(crate) fn enabled(value: Option<&OsStr>) -> bool {
    value != Some(OsStr::new("0"))
}

/// `scale`/`translation` map retained screen pixels to target screen pixels.
/// Beyond the drift budget the uncovered border and stale placement grow too
/// large, so the caller rebuilds and the budget restarts from the new scene.
/// `origin`/`extent` are the chart viewport in the same screen pixels.
pub(crate) fn within_drift(
    scale: [f32; 2],
    translation: [f32; 2],
    origin: [f32; 2],
    extent: [f32; 2],
) -> bool {
    within_drift_fraction(scale, translation, origin, extent, 1.)
}

/// `within_drift` against `fraction` of the budget (log-scaled for zoom).
pub(crate) fn within_drift_fraction(
    scale: [f32; 2],
    translation: [f32; 2],
    origin: [f32; 2],
    extent: [f32; 2],
    fraction: f32,
) -> bool {
    let max_scale = MAX_SCALE_DRIFT.powf(fraction);
    (0..2).all(|axis| {
        let (s, t, o, extent) = (scale[axis], translation[axis], origin[axis], extent[axis]);
        if !(s.is_finite() && t.is_finite() && o.is_finite() && extent.is_finite())
            || s <= 0.
            || extent <= 0.
        {
            return false;
        }
        // Retained pixel now shown at the target viewport centre.
        let centre = o + extent * 0.5;
        let source = (centre - t) / s;
        (1. / max_scale..=max_scale).contains(&s)
            && (source - centre).abs() <= extent * MAX_CENTRE_DRIFT * fraction
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn within_drift_at(scale: [f32; 2], translation: [f32; 2], extent: [f32; 2]) -> bool {
        within_drift(scale, translation, [0., 0.], extent)
    }
    #[test]
    fn viewport_origin_offsets_the_centre() {
        // Chart panel starts 1000px right: zoom about its own centre (1500, 400).
        let s = 1.5;
        let t = [1500. * (1. - s), 400. * (1. - s)];
        assert!(within_drift([s, s], t, [1000., 0.], [1000., 800.]));
        assert!(!within_drift([s, s], t, [0., 0.], [1000., 800.]));
    }
    #[test]
    fn default_on_and_only_exact_zero_disables() {
        assert!(enabled(None));
        assert!(enabled(Some(OsStr::new("1"))));
        assert!(enabled(Some(OsStr::new("false"))));
        assert!(!enabled(Some(OsStr::new("0"))));
    }
    #[test]
    fn identity_and_small_motion_stay_within_budget() {
        let v = [1000., 800.];
        assert!(within_drift_at([1., 1.], [0., 0.], v));
        assert!(within_drift_at([1., 1.], [250., -200.], v));
        assert!(!within_drift_at([1., 1.], [251., 0.], v));
        assert!(!within_drift_at([1., 1.], [0., -201.], v));
    }
    #[test]
    fn zoom_about_centre_is_limited_by_scale_only() {
        let v = [1000., 800.];
        // Zoom about the viewport centre keeps the centre pixel fixed.
        let about_centre = |s: f32| [500. * (1. - s), 400. * (1. - s)];
        assert!(within_drift_at([1.5, 1.5], about_centre(1.5), v));
        assert!(within_drift_at(
            [1. / 1.5, 1. / 1.5],
            about_centre(1. / 1.5),
            v
        ));
        assert!(!within_drift_at([1.51, 1.51], about_centre(1.51), v));
        assert!(!within_drift_at([0.66, 0.66], about_centre(0.66), v));
    }
    #[test]
    fn prefetch_fraction_is_half_the_travel_and_log_half_the_zoom() {
        let v = [1000., 800.];
        let half =
            |s: [f32; 2], t: [f32; 2]| within_drift_fraction(s, t, [0., 0.], v, PREFETCH_FRACTION);
        assert!(half([1., 1.], [125., 0.]));
        assert!(!half([1., 1.], [126., 0.]));
        let about_centre = |s: f32| [500. * (1. - s), 400. * (1. - s)];
        let edge = 1.5f32.sqrt();
        assert!(half(
            [edge * 0.999, edge * 0.999],
            about_centre(edge * 0.999)
        ));
        assert!(!half(
            [edge * 1.001, edge * 1.001],
            about_centre(edge * 1.001)
        ));
    }
    #[test]
    fn invalid_inputs_decline() {
        let v = [1000., 800.];
        assert!(!within_drift_at([0., 1.], [0., 0.], v));
        assert!(!within_drift_at([1., 1.], [f32::NAN, 0.], v));
        assert!(!within_drift_at([1., 1.], [0., 0.], [0., 800.]));
    }
}
