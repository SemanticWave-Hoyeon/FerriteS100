//! Compare optimized suppression intersection with the earlier independent nested scan.
use ferrite_render::{
    dash_line_spans, Color, GeoBounds, LineSpan, LineStyle, Scaler, ScreenPoint, Viewport,
};
use std::{hint::black_box, time::Instant};
fn main() {
    let out = std::env::args().nth(1).unwrap();
    let s = Scaler::new(
        GeoBounds::new(0., 0., 10., 10.),
        Viewport::new(100000., 1000.),
    );
    let points: Vec<_> = (0..=5000)
        .map(|i| s.screen_to_world(ScreenPoint::new(i as f32 * 5., 500.)))
        .collect();
    let style = LineStyle::dashed(Color::BLACK, 1., vec![3., 2.]);
    let visible: Vec<_> = (0..5000)
        .map(|segment| LineSpan {
            segment,
            start: 0.2,
            end: 0.8,
        })
        .collect();
    let base = dash_line_spans(&points, &s, &style, None).unwrap();
    let naive = || {
        let mut out = Vec::new();
        for d in &base {
            for v in visible.iter().filter(|v| v.segment == d.segment) {
                let start = d.start.max(v.start);
                let end = d.end.min(v.end);
                if end > start {
                    out.push(LineSpan {
                        segment: d.segment,
                        start,
                        end,
                    });
                }
            }
        }
        out
    };
    let expected = naive();
    assert_eq!(
        expected,
        dash_line_spans(&points, &s, &style, Some(&visible)).unwrap()
    );
    let mut fast = Vec::new();
    let mut old = Vec::new();
    for _ in 0..7 {
        let t = Instant::now();
        black_box(
            dash_line_spans(black_box(&points), &s, &style, Some(black_box(&visible))).unwrap(),
        );
        fast.push(t.elapsed().as_secs_f64());
        let t = Instant::now();
        black_box(naive());
        old.push(t.elapsed().as_secs_f64());
    }
    fast.sort_by(f64::total_cmp);
    old.sort_by(f64::total_cmp);
    std::fs::write(out,serde_json::to_vec_pretty(&serde_json::json!({"segments":5000,"visible_spans":visible.len(),"dash_spans":base.len(),"runs":7,"optimized_includes_dash_generation":true,"naive_intersection_only":true,"optimized_median_seconds":fast[3],"naive_median_seconds":old[3],"median_ratio_naive_over_optimized":old[3]/fast[3],"output_equal":true})).unwrap()).unwrap();
}
