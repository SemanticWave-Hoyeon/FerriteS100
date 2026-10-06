//! CPU camera-call benchmark and old-formula screen-coordinate oracle.
//! These timings are not application FPS or GPU performance measurements.
use ferrite_render::{FlatProjection, GeoBounds, Scaler, ScreenPoint, Viewport, WorldPoint};
use std::{hint::black_box, time::Instant};
#[inline(never)]
fn previous(s: &Scaler, p: WorldPoint) -> ScreenPoint {
    let t = s.flat_transform();
    ScreenPoint::new(
        ((p.x - t.geographic_origin[0]) * t.scale[0] + t.offset[0]) as f32,
        ((t.projection.project_y(t.geographic_origin[1]) - t.projection.project_y(p.y))
            * t.scale[1]
            + t.offset[1]) as f32,
    )
}
#[inline(never)]
fn cached_project(s: &Scaler, p: WorldPoint) -> ScreenPoint {
    s.world_to_screen(p)
}
fn main() {
    let out = std::env::args().nth(1).expect("Output JSON");
    let mut rows = Vec::new();
    for latitude in [0., 48.65, 80.] {
        let mut s = Scaler::new(
            GeoBounds::new(-3., latitude - 0.5, -1., latitude + 0.5),
            Viewport::new(1000., 800.),
        );
        s.set_projection(FlatProjection::EllipsoidalMercator);
        let points: Vec<_> = (0..20000)
            .map(|i| {
                WorldPoint::new(
                    -3. + 2. * ((i * 73 % 20000) as f64) / 20000.,
                    latitude - 0.5 + ((i * 179 % 20000) as f64) / 20000.,
                )
            })
            .collect();
        let mut max_difference = 0f64;
        for p in &points {
            let a = previous(&s, *p);
            let b = s.world_to_screen(*p);
            max_difference = max_difference
                .max((a.x - b.x).abs() as f64)
                .max((a.y - b.y).abs() as f64);
        }
        assert_eq!(max_difference, 0.);
        let mut old = Vec::new();
        let mut cached = Vec::new();
        for repeat in 0..5 {
            let mut run = |old_first: bool| {
                let time = Instant::now();
                for _ in 0..10 {
                    for p in &points {
                        if old_first {
                            black_box(previous(black_box(&s), black_box(*p)));
                        } else {
                            black_box(cached_project(black_box(&s), black_box(*p)));
                        }
                    }
                }
                time.elapsed().as_secs_f64()
            };
            if repeat % 2 == 0 {
                old.push(run(true));
                cached.push(run(false));
            } else {
                cached.push(run(false));
                old.push(run(true));
            }
        }
        old.sort_by(f64::total_cmp);
        cached.sort_by(f64::total_cmp);
        rows.push(serde_json::json!({"latitude":latitude,"points":points.len(),"calls_per_sample":200000,
   "samples":5,"max_screen_difference_pixels":max_difference,"previous_median_seconds":old[2],
   "cached_median_seconds":cached[2],"camera_call_speedup":old[2]/cached[2],"application_fps_measured":false}));
    }
    std::fs::write(out, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}
