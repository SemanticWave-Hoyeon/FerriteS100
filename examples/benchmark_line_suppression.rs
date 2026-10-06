//! Compare an independent interval oracle with indexed partial-line suppression.
use ferrite_render::{DrawingInstruction, LineInstruction, LineSuppressionCache, WorldPoint};
use std::time::Instant;
fn main() {
    let groups = 2048usize;
    let mut instructions = Vec::new();
    for i in 0..groups {
        let y = i as f64;
        for (priority, a, b) in [(2, 0., 10.), (8, 2., 8.)] {
            instructions.push(DrawingInstruction::Line(
                LineInstruction::new(vec![WorldPoint::new(a, y), WorldPoint::new(b, y)])
                    .with_priority(priority),
            ));
        }
    }
    let mut indexed = Vec::new();
    let mut brute = Vec::new();
    let mut warm = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let mut hidden = 0;
        for i in 0..groups {
            let DrawingInstruction::Line(low) = &instructions[i * 2] else {
                unreachable!()
            };
            for j in 0..groups {
                let DrawingInstruction::Line(high) = &instructions[j * 2 + 1] else {
                    unreachable!()
                };
                if high.priority.0 > low.priority.0
                    && high.points[0].y == low.points[0].y
                    && high.points[0].x < low.points[1].x
                    && high.points[1].x > low.points[0].x
                {
                    hidden += 1;
                }
            }
        }
        std::hint::black_box(hidden);
        brute.push(start.elapsed().as_secs_f64());
        assert_eq!(hidden, groups);
        let mut cache = LineSuppressionCache::default();
        let start = Instant::now();
        let plan = cache.plan(&instructions, 1000, None, None);
        indexed.push(start.elapsed().as_secs_f64());
        assert_eq!(plan.partial.len(), groups);
        for spans in plan.partial.values() {
            assert_eq!(spans.len(), 2);
            assert_eq!((spans[0].start, spans[0].end), (0., 0.2));
            assert_eq!((spans[1].start, spans[1].end), (0.8, 1.));
        }
        let start = Instant::now();
        let same = cache.plan(&instructions, 1000, None, None);
        warm.push(start.elapsed().as_secs_f64());
        assert!(std::sync::Arc::ptr_eq(&plan, &same));
    }
    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"groups":groups,"instructions":instructions.len(),"brute_interval_oracle_seconds":brute,"indexed_cold_seconds":indexed,"indexed_warm_seconds":warm,"oracle_and_visible_spans_match":true})).unwrap());
}
