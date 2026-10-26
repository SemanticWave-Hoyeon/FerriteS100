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
/// Fraction of the drift budget (and of the overscan margin) at which a
/// background build is started, so the next scene normally arrives before the
/// overscan is spent. 0.5 revealed unprepared chart on 4.5k px/s drags with
/// 10–30 ms builds; 0.25 did not up to 6k px/s (navigation motion audit).
pub(crate) const PREFETCH_FRACTION: f32 = 0.25;
/// Viewport fraction prepared beyond every side of a retained scene (S-98
/// coverage, culling): the drift budget's travel, so a pan within the budget
/// never reveals unprepared chart.
pub const OVERSCAN_FRACTION: f32 = MAX_CENTRE_DRIFT;

pub(crate) fn enabled(value: Option<&OsStr>) -> bool {
    value != Some(OsStr::new("0"))
}

/// Moving previews fill unprepared chart pixels with a low-detail placeholder
/// (`FERRITE_NAV_PLACEHOLDER=0` disables it and its coastline ring).
pub(crate) fn placeholder_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FERRITE_NAV_PLACEHOLDER").is_none_or(|v| v != "0"))
}

/// `FERRITE_NAV_PREFETCH`: a fraction in (0, 1], else the default.
pub(crate) fn prefetch_fraction(value: Option<&OsStr>) -> f32 {
    value
        .and_then(|v| v.to_str())
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|f| f.is_finite() && *f > 0. && *f <= 1.)
        .unwrap_or(PREFETCH_FRACTION)
}

/// Axis-aligned rectangle `[x0, y0, x1, y1]` in screen pixels.
pub(crate) type Rect = [f32; 4];

/// Retained pixels shown at the target `viewport` through `scale`/`translation`
/// (retained pixel * scale + translation = target pixel).
fn source_rect(scale: [f32; 2], translation: [f32; 2], viewport: Rect) -> Option<Rect> {
    if !(scale.iter().all(|s| s.is_finite() && *s > 0.)
        && translation.iter().all(|t| t.is_finite())
        && viewport.iter().all(|v| v.is_finite()))
    {
        return None;
    }
    Some([
        (viewport[0] - translation[0]) / scale[0],
        (viewport[1] - translation[1]) / scale[1],
        (viewport[2] - translation[0]) / scale[0],
        (viewport[3] - translation[1]) / scale[1],
    ])
}

/// How far the shown retained pixels reach beyond the retained viewport
/// `prepared_viewport`, as a fraction of the overscan `margin` on the worst
/// side: 0 inside, 1 at the edge of the prepared overscan, above 1 reveals
/// unprepared chart. A zero margin turns any protrusion into infinity.
pub(crate) fn revealed_margin_fraction(
    scale: [f32; 2],
    translation: [f32; 2],
    viewport: Rect,
    prepared_viewport: Rect,
    margin: [f32; 2],
) -> f32 {
    let Some(s) = source_rect(scale, translation, viewport) else {
        return f32::INFINITY;
    };
    // Sub-pixel slack absorbs f32 rounding of the composed affine.
    const SLACK: f32 = 1e-3;
    let protrusion = [
        (prepared_viewport[0] - s[0], margin[0]),
        (prepared_viewport[1] - s[1], margin[1]),
        (s[2] - prepared_viewport[2], margin[0]),
        (s[3] - prepared_viewport[3], margin[1]),
    ];
    protrusion
        .iter()
        .map(|&(p, m)| {
            if p <= SLACK {
                0.
            } else if m > 0. {
                (p - SLACK) / m
            } else {
                f32::INFINITY
            }
        })
        .fold(0., f32::max)
}

/// Parts of `viewport` (target pixels) outside the prepared rectangle
/// `prepared` (retained pixels) once mapped through the affine: at most four
/// disjoint bands (top, bottom, left, right). Empty while nothing unprepared shows.
pub(crate) fn unprepared_rects(
    scale: [f32; 2],
    translation: [f32; 2],
    viewport: Rect,
    prepared: Rect,
) -> Vec<Rect> {
    if !(scale.iter().all(|s| s.is_finite() && *s > 0.)
        && translation.iter().all(|t| t.is_finite())
        && prepared.iter().all(|v| v.is_finite())
        && viewport.iter().all(|v| v.is_finite()))
        || viewport[2] <= viewport[0]
        || viewport[3] <= viewport[1]
    {
        return Vec::new();
    }
    let p = [
        prepared[0] * scale[0] + translation[0],
        prepared[1] * scale[1] + translation[1],
        prepared[2] * scale[0] + translation[0],
        prepared[3] * scale[1] + translation[1],
    ];
    let [vx0, vy0, vx1, vy1] = viewport;
    // The prepared rectangle clipped to the viewport (possibly empty).
    let cx0 = p[0].clamp(vx0, vx1);
    let cx1 = p[2].clamp(vx0, vx1);
    let cy0 = p[1].clamp(vy0, vy1);
    let cy1 = p[3].clamp(vy0, vy1);
    if cx0 >= cx1 || cy0 >= cy1 {
        return vec![viewport];
    }
    let mut out = Vec::new();
    let mut push = |r: Rect| {
        if r[2] - r[0] > 1e-3 && r[3] - r[1] > 1e-3 {
            out.push(r);
        }
    };
    push([vx0, vy0, vx1, cy0]);
    push([vx0, cy1, vx1, vy1]);
    push([vx0, cy0, cx0, cy1]);
    push([cx1, cy0, vx1, cy1]);
    out
}

/// Fraction of the viewport area covered by `rects` (disjoint).
pub(crate) fn area_fraction(rects: &[Rect], viewport: Rect) -> f32 {
    let area = (viewport[2] - viewport[0]) * (viewport[3] - viewport[1]);
    if area.is_nan() || area <= 0. {
        return 0.;
    }
    rects
        .iter()
        .map(|r| (r[2] - r[0]).max(0.) * (r[3] - r[1]).max(0.))
        .fold(0., |sum, a| sum + a)
        / area
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

/// `within_drift`, and with an overscan `margin` the shown retained pixels
/// must also stay within the prepared overscan (e.g. a zoom-out plus a pan).
pub(crate) fn within_drift_overscan(
    scale: [f32; 2],
    translation: [f32; 2],
    origin: [f32; 2],
    extent: [f32; 2],
    margin: Option<[f32; 2]>,
) -> bool {
    let viewport = [
        origin[0],
        origin[1],
        origin[0] + extent[0],
        origin[1] + extent[1],
    ];
    within_drift(scale, translation, origin, extent)
        && margin.is_none_or(|margin| {
            revealed_margin_fraction(scale, translation, viewport, viewport, margin) <= 1.
        })
}

/// Start the next background scene: past `fraction` of the drift budget, or
/// of the overscan margin on any side.
pub(crate) fn prefetch_due(
    scale: [f32; 2],
    translation: [f32; 2],
    origin: [f32; 2],
    extent: [f32; 2],
    margin: Option<[f32; 2]>,
    fraction: f32,
) -> bool {
    let viewport = [
        origin[0],
        origin[1],
        origin[0] + extent[0],
        origin[1] + extent[1],
    ];
    !within_drift_fraction(scale, translation, origin, extent, fraction)
        || margin.is_some_and(|margin| {
            revealed_margin_fraction(scale, translation, viewport, viewport, margin) > fraction
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
    fn default_prefetch_starts_at_a_quarter_of_the_budget() {
        let v = [1000., 800.];
        let m = Some([250., 200.]);
        let due = |t: [f32; 2]| prefetch_due([1., 1.], t, [0., 0.], v, m, PREFETCH_FRACTION);
        assert!(!due([62., 0.]));
        assert!(due([63., 0.]));
        let about_centre = |s: f32| [500. * (1. - s), 400. * (1. - s)];
        let edge = 1.5f32.powf(PREFETCH_FRACTION);
        let zoom =
            |s: f32| prefetch_due([s, s], about_centre(s), [0., 0.], v, m, PREFETCH_FRACTION);
        assert!(!zoom(edge * 0.999));
        assert!(zoom(edge * 1.001));
    }
    #[test]
    fn prefetch_fraction_is_half_the_travel_and_log_half_the_zoom() {
        let v = [1000., 800.];
        let half = |s: [f32; 2], t: [f32; 2]| within_drift_fraction(s, t, [0., 0.], v, 0.5);
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
    fn revealed_margin_tracks_pan_against_the_overscan() {
        let v = [0., 0., 1000., 800.];
        let m = [250., 200.];
        // Pure pan: the drift budget's travel uses exactly the overscan.
        assert_eq!(revealed_margin_fraction([1., 1.], [0., 0.], v, v, m), 0.);
        let half = revealed_margin_fraction([1., 1.], [-125., 0.], v, v, m);
        assert!((half - 0.5).abs() < 1e-4, "{half}");
        let edge = revealed_margin_fraction([1., 1.], [250., -200.], v, v, m);
        assert!((edge - 1.).abs() < 1e-4, "{edge}");
        assert!(revealed_margin_fraction([1., 1.], [251., 0.], v, v, m) > 1.);
        // Without overscan any reveal is unprepared.
        assert_eq!(
            revealed_margin_fraction([1., 1.], [10., 0.], v, v, [0., 0.]),
            f32::INFINITY
        );
        // A 1.5x zoom-out about the centre reveals exactly the overscan.
        let s = 1. / 1.5;
        let t = [500. * (1. - s), 400. * (1. - s)];
        let zoom_out = revealed_margin_fraction([s, s], t, v, v, m);
        assert!((zoom_out - 1.).abs() < 1e-3, "{zoom_out}");
        // Zooming in reveals nothing.
        let s = 1.5;
        let t = [500. * (1. - s), 400. * (1. - s)];
        assert_eq!(revealed_margin_fraction([s, s], t, v, v, m), 0.);
    }
    #[test]
    fn overscan_budget_rejects_zoom_out_plus_pan_beyond_the_ring() {
        let v = [1000., 800.];
        let m = [250., 200.];
        let s = 1. / 1.5;
        let t = [500. * (1. - s), 400. * (1. - s)];
        assert!(within_drift_overscan([s, s], t, [0., 0.], v, Some(m)));
        // The old centre budget alone allows a further 100 px pan.
        assert!(within_drift([s, s], [t[0] + 100., t[1]], [0., 0.], v));
        assert!(!within_drift_overscan(
            [s, s],
            [t[0] + 100., t[1]],
            [0., 0.],
            v,
            Some(m)
        ));
        assert!(within_drift_overscan(
            [1., 1.],
            [250., 0.],
            [0., 0.],
            v,
            Some(m)
        ));
        assert!(!within_drift_overscan(
            [1., 1.],
            [251., 0.],
            [0., 0.],
            v,
            Some(m)
        ));
        // No overscan keeps the original budget.
        assert!(within_drift_overscan(
            [s, s],
            [t[0] + 100., t[1]],
            [0., 0.],
            v,
            None
        ));
    }
    #[test]
    fn prefetch_is_due_by_revealed_margin() {
        let v = [1000., 800.];
        let m = Some([250., 200.]);
        let due = |t: [f32; 2], f: f32| prefetch_due([1., 1.], t, [0., 0.], v, m, f);
        assert!(!due([62., 0.], 0.25));
        assert!(due([63., 0.], 0.25));
        assert!(!due([124., 0.], 0.5));
        assert!(due([126., 0.], 0.5));
        assert!(!due([0., 0.], 0.1));
    }
    #[test]
    fn unprepared_rects_are_the_viewport_minus_the_mapped_overscan() {
        let v = [0., 0., 1000., 800.];
        let prepared = [-250., -200., 1250., 1000.];
        // Inside the budget nothing unprepared shows.
        assert!(unprepared_rects([1., 1.], [250., 0.], v, prepared).is_empty());
        // 100 px beyond the ring on the left.
        let r = unprepared_rects([1., 1.], [350., 0.], v, prepared);
        assert_eq!(r, vec![[0., 0., 100., 800.]]);
        assert!((area_fraction(&r, v) - 0.1).abs() < 1e-6);
        // Diagonal: a top band plus a left band, disjoint.
        let r = unprepared_rects([1., 1.], [300., 300.], v, prepared);
        assert_eq!(r, vec![[0., 0., 1000., 100.], [0., 100., 50., 800.]]);
        // A 3x zoom-out shows the ring in the middle only.
        let s = 1. / 3.;
        let t = [500. * (1. - s), 400. * (1. - s)];
        let r = unprepared_rects([s, s], t, v, prepared);
        let fraction = area_fraction(&r, v);
        assert!((fraction - (1. - 0.5 * 0.5)).abs() < 1e-4, "{fraction}");
        // Far away: the whole viewport.
        assert_eq!(unprepared_rects([1., 1.], [1e5, 0.], v, prepared), vec![v]);
    }
    #[test]
    fn prefetch_fraction_parses_only_valid_values() {
        assert_eq!(prefetch_fraction(None), PREFETCH_FRACTION);
        assert_eq!(prefetch_fraction(Some(OsStr::new("0.25"))), 0.25);
        for bad in ["0", "-1", "1.5", "nan", "x"] {
            assert_eq!(prefetch_fraction(Some(OsStr::new(bad))), PREFETCH_FRACTION);
        }
    }
    #[test]
    fn invalid_inputs_decline() {
        let v = [1000., 800.];
        assert!(!within_drift_at([0., 1.], [0., 0.], v));
        assert!(!within_drift_at([1., 1.], [f32::NAN, 0.], v));
        assert!(!within_drift_at([1., 1.], [0., 0.], [0., 800.]));
    }
}
